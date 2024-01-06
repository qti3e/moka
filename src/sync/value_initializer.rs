use parking_lot::RwLock;
use std::{
    any::{Any, TypeId},
    fmt,
    hash::{BuildHasher, Hash},
    sync::Arc,
};
use triomphe::Arc as TrioArc;

use crate::{ops::compute, Entry};

use super::{ComputeNone, OptionallyNone};

const WAITER_MAP_NUM_SEGMENTS: usize = 64;

pub(crate) trait GetOrInsert<K, V> {
    /// Gets an entry for the given key _with_ recording the access to the cache
    /// policies.
    fn get_entry(&self, key: &Arc<K>, hash: u64) -> Option<Entry<K, V>>
    where
        V: 'static;

    /// Inserts a value for the given key.
    fn insert(&self, key: Arc<K>, hash: u64, value: V);

    /// Removes a value for the given key. Returns the removed value.
    fn remove(&self, key: &Arc<K>, hash: u64) -> Option<V>;
}

type ErrorObject = Arc<dyn Any + Send + Sync + 'static>;

// type WaiterValue<V> = Option<Result<V, ErrorObject>>;
enum WaiterValue<V> {
    Computing,
    Ready(Result<V, ErrorObject>),
    ReadyNone,
    // https://github.com/moka-rs/moka/issues/43
    InitClosurePanicked,
}

impl<V> fmt::Debug for WaiterValue<V> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            WaiterValue::Computing => write!(f, "Computing"),
            WaiterValue::Ready(_) => write!(f, "Ready"),
            WaiterValue::ReadyNone => write!(f, "ReadyNone"),
            WaiterValue::InitClosurePanicked => write!(f, "InitFuturePanicked"),
        }
    }
}

type Waiter<V> = TrioArc<RwLock<WaiterValue<V>>>;

pub(crate) enum InitResult<V, E> {
    Initialized(V),
    ReadExisting(V),
    InitErr(Arc<E>),
}

pub(crate) enum ComputeResult<V, E> {
    Inserted(V),
    Updated(V),
    Removed(V),
    Nop(Option<V>),
    EvalErr(E),
}

pub(crate) struct ValueInitializer<K, V, S> {
    // TypeId is the type ID of the concrete error type of generic type E in the
    // try_get_with method. We use the type ID as a part of the key to ensure that
    // we can always downcast the trait object ErrorObject (in Waiter<V>) into
    // its concrete type.
    waiters: crate::cht::SegmentedHashMap<(Arc<K>, TypeId), Waiter<V>, S>,
}

impl<K, V, S> ValueInitializer<K, V, S>
where
    K: Eq + Hash,
    V: Clone,
    S: BuildHasher,
{
    pub(crate) fn with_hasher(hasher: S) -> Self {
        Self {
            waiters: crate::cht::SegmentedHashMap::with_num_segments_and_hasher(
                WAITER_MAP_NUM_SEGMENTS,
                hasher,
            ),
        }
    }

    /// # Panics
    /// Panics if the `init` closure has been panicked.
    pub(crate) fn try_init_or_read<O, E>(
        &self,
        key: &Arc<K>,
        type_id: TypeId,
        // Closure to get an existing value from cache.
        mut get: impl FnMut() -> Option<V>,
        // Closure to initialize a new value.
        init: impl FnOnce() -> O,
        // Closure to insert a new value into cache.
        mut insert: impl FnMut(V),
        // Function to convert a value O, returned from the init future, into
        // Result<V, E>.
        post_init: fn(O) -> Result<V, E>,
    ) -> InitResult<V, E>
    where
        E: Send + Sync + 'static,
    {
        use std::panic::{catch_unwind, resume_unwind, AssertUnwindSafe};
        use InitResult::{InitErr, ReadExisting};

        const MAX_RETRIES: usize = 200;
        let mut retries = 0;

        let (w_key, w_hash) = self.waiter_key_hash(key, type_id);

        let waiter = TrioArc::new(RwLock::new(WaiterValue::Computing));
        let mut lock = waiter.write();

        loop {
            let Some(existing_waiter) = self.try_insert_waiter(w_key.clone(), w_hash, &waiter)
            else {
                // Inserted.
                break;
            };

            // Somebody else's waiter already exists, so wait for its result to become available.
            let waiter_result = existing_waiter.read();
            match &*waiter_result {
                WaiterValue::Ready(Ok(value)) => return ReadExisting(value.clone()),
                WaiterValue::Ready(Err(e)) => return InitErr(Arc::clone(e).downcast().unwrap()),
                // Somebody else's init closure has been panicked.
                WaiterValue::InitClosurePanicked => {
                    retries += 1;
                    assert!(
                        retries < MAX_RETRIES,
                        "Too many retries. Tried to read the return value from the `init` \
                        closure but failed {retries} times. Maybe the `init` kept panicking?"
                    );

                    // Retry from the beginning.
                    continue;
                }
                // Unexpected state.
                s @ (WaiterValue::Computing | WaiterValue::ReadyNone) => panic!(
                    "Got unexpected state `{s:?}` after resolving `init` future. \
                    This might be a bug in Moka"
                ),
            }
        }

        // Our waiter was inserted.

        // Check if the value has already been inserted by other thread.
        if let Some(value) = get() {
            // Yes. Set the waiter value, remove our waiter, and return
            // the existing value.
            *lock = WaiterValue::Ready(Ok(value.clone()));
            self.remove_waiter(w_key, w_hash);
            return InitResult::ReadExisting(value);
        }

        // The value still does note exist. Let's evaluate the init
        // closure. Catching panic is safe here as we do not try to
        // evaluate the closure again.
        match catch_unwind(AssertUnwindSafe(init)) {
            // Evaluated.
            Ok(value) => {
                let init_res = match post_init(value) {
                    Ok(value) => {
                        insert(value.clone());
                        *lock = WaiterValue::Ready(Ok(value.clone()));
                        InitResult::Initialized(value)
                    }
                    Err(e) => {
                        let err: ErrorObject = Arc::new(e);

                        *lock = WaiterValue::Ready(Err(Arc::clone(&err)));
                        InitResult::InitErr(err.downcast().unwrap())
                    }
                };
                self.remove_waiter(w_key, w_hash);
                init_res
            }
            // Panicked.
            Err(payload) => {
                *lock = WaiterValue::InitClosurePanicked;
                // Remove the waiter so that others can retry.
                self.remove_waiter(w_key, w_hash);
                resume_unwind(payload);
            }
        }
        // The write lock will be unlocked here.
    }

    /// # Panics
    /// Panics if the `init` closure has been panicked.
    pub(crate) fn try_compute<'a, C, F, O, E>(
        &'a self,
        c_key: &Arc<K>,
        c_hash: u64,
        cache: &C,
        f: F,
        post_init: fn(O) -> Result<compute::Op<V>, E>,
        allow_nop: bool,
    ) -> ComputeResult<V, E>
    where
        V: 'static,
        C: GetOrInsert<K, V> + Send + 'a,
        F: FnOnce(Option<Entry<K, V>>) -> O,
        E: Send + Sync + 'static,
    {
        use std::panic::{catch_unwind, resume_unwind, AssertUnwindSafe};
        use ComputeResult::{EvalErr, Inserted, Nop, Removed, Updated};

        let type_id = TypeId::of::<ComputeNone>();
        let (w_key, w_hash) = self.waiter_key_hash(c_key, type_id);
        let waiter = TrioArc::new(RwLock::new(WaiterValue::Computing));
        // NOTE: We have to acquire a write lock before `try_insert_waiter`,
        // so that any concurrent attempt will get our lock and wait on it.
        let mut lock = waiter.write();

        loop {
            let Some(existing_waiter) = self.try_insert_waiter(w_key.clone(), w_hash, &waiter)
            else {
                // Inserted.
                break;
            };

            // Somebody else's waiter already exists, so wait for it to finish
            // (wait for it to release the write lock).
            let waiter_result = existing_waiter.read();
            match &*waiter_result {
                // Unexpected state.
                WaiterValue::Computing => panic!(
                    "Got unexpected state `Computing` after resolving `init` future. \
                    This might be a bug in Moka"
                ),
                _ => {
                    // Try to insert our waiter again.
                    continue;
                }
            }
        }

        // Our waiter was inserted.

        // Get the current value.
        let maybe_entry = cache.get_entry(c_key, c_hash);
        let maybe_value = if allow_nop {
            maybe_entry.as_ref().map(|ent| ent.value().clone())
        } else {
            None
        };
        let entry_existed = maybe_entry.is_some();

        // Evaluate the `f` closure. Catching panic is safe here as we will not
        // evaluate the closure again.
        match catch_unwind(AssertUnwindSafe(|| f(maybe_entry))) {
            // Evaluated.
            Ok(op) => {
                let init_res = match post_init(op) {
                    Ok(op) => match op {
                        compute::Op::Nop => Nop(maybe_value),
                        compute::Op::Put(value) => {
                            cache.insert(Arc::clone(c_key), c_hash, value.clone());
                            if entry_existed {
                                Updated(value)
                            } else {
                                Inserted(value)
                            }
                        }
                        compute::Op::Remove => {
                            let maybe_prev_v = cache.remove(c_key, c_hash);
                            if let Some(prev_v) = maybe_prev_v {
                                Removed(prev_v)
                            } else {
                                Nop(None)
                            }
                        }
                    },
                    Err(e) => EvalErr(e),
                };
                *lock = WaiterValue::ReadyNone;
                self.remove_waiter(w_key, w_hash);
                init_res
            }
            // Panicked.
            Err(payload) => {
                *lock = WaiterValue::InitClosurePanicked;
                // Remove the waiter so that others can retry.
                self.remove_waiter(w_key, w_hash);
                resume_unwind(payload);
            }
        }
        // The lock will be unlocked here.
    }

    /// The `post_init` function for the `get_with` method of cache.
    pub(crate) fn post_init_for_get_with(value: V) -> Result<V, ()> {
        Ok(value)
    }

    /// The `post_init` function for the `optionally_get_with` method of cache.
    pub(crate) fn post_init_for_optionally_get_with(
        value: Option<V>,
    ) -> Result<V, Arc<OptionallyNone>> {
        // `value` can be either `Some` or `None`. For `None` case, without change
        // the existing API too much, we will need to convert `None` to Arc<E> here.
        // `Infallible` could not be instantiated. So it might be good to use an
        // empty struct to indicate the error type.
        value.ok_or(Arc::new(OptionallyNone))
    }

    /// The `post_init` function for `try_get_with` method of cache.
    pub(crate) fn post_init_for_try_get_with<E>(result: Result<V, E>) -> Result<V, E> {
        result
    }

    /// The `post_init` function for the `and_upsert_with` method of cache.
    pub(crate) fn post_init_for_upsert_with(value: V) -> Result<compute::Op<V>, ()> {
        Ok(compute::Op::Put(value))
    }

    /// The `post_init` function for the `and_compute_with` method of cache.
    pub(crate) fn post_init_for_compute_with(op: compute::Op<V>) -> Result<compute::Op<V>, ()> {
        Ok(op)
    }

    /// The `post_init` function for the `and_try_compute_with` method of cache.
    pub(crate) fn post_init_for_try_compute_with<E>(
        op: Result<compute::Op<V>, E>,
    ) -> Result<compute::Op<V>, E>
    where
        E: Send + Sync + 'static,
    {
        op
    }

    /// Returns the `type_id` for `get_with` method of cache.
    pub(crate) fn type_id_for_get_with() -> TypeId {
        // NOTE: We use a regular function here instead of a const fn because TypeId
        // is not stable as a const fn. (as of our MSRV)
        TypeId::of::<()>()
    }

    /// Returns the `type_id` for `optionally_get_with` method of cache.
    pub(crate) fn type_id_for_optionally_get_with() -> TypeId {
        TypeId::of::<OptionallyNone>()
    }

    /// Returns the `type_id` for `try_get_with` method of cache.
    pub(crate) fn type_id_for_try_get_with<E: 'static>() -> TypeId {
        TypeId::of::<E>()
    }

    #[inline]
    fn remove_waiter(&self, w_key: (Arc<K>, TypeId), w_hash: u64) {
        self.waiters.remove(w_hash, |k| k == &w_key);
    }

    #[inline]
    fn try_insert_waiter(
        &self,
        w_key: (Arc<K>, TypeId),
        w_hash: u64,
        waiter: &Waiter<V>,
    ) -> Option<Waiter<V>> {
        let waiter = TrioArc::clone(waiter);
        self.waiters.insert_if_not_present(w_key, w_hash, waiter)
    }

    #[inline]
    fn waiter_key_hash(&self, c_key: &Arc<K>, type_id: TypeId) -> ((Arc<K>, TypeId), u64) {
        let w_key = (Arc::clone(c_key), type_id);
        let w_hash = self.waiters.hash(&w_key);
        (w_key, w_hash)
    }
}

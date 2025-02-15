#![doc = include_str!("../README.md")]
//! In memory Key/Value store with LRU expire and concurrent access
//!
//!
//! Description
//! ===========
//!
//! Items are stored in N sharded/bucketized HashMaps to improve concurrency.  Every Item is
//! always behind a RwLock.  Quering an item will return a guard associated to this lock.
//! Items that are not locked are kept in a list to implement a least-recent-used expire
//! policy.  Locked items are removed from that lru list and put into the lru-list when they
//! become unlocked.  Locked Items will not block the hosting HashMap.
//!
//!
//! Implementation Discussion
//! =========================
//!
//! The HashMap storing the Items in Boxed entries.  Entries protect the actual item by a
//! RwLock.  The API allows access to items only over these locks, returning wraped guards
//! thereof. Since cConcurrent access to the Entries will not block the Hashmap, some
//! 'unsafe' code is required which is hidden behind an safe API.
//!
//! New Items are constructed in an atomic way by passing a closure producing the item to the
//! respective lookup function.  While an Item is constructed it has a write lock which
//! ensures that on concurrent construction/queries only one contructor wins and any other
//! will acquire the newly constructed item.
//!
//!
//! Proof that no lifetime guarantees are violated
//! ----------------------------------------------
//!
//! Is actually simple, the returned guard has a rust lifetime bound to the CacheDB
//! object.  Thus no access can outlive the hosting collection.
//!
//!
//! Proof that no data races exist
//! ------------------------------
//!
//! In most parts the Mutex and RwLock ensures that no data races can happen, this is
//! validated by rust.
//!
//! The unsafe part of the implementation detaches a LockGuard from its hosting collection to
//! free the mutex on the HashMap.  This could lead to potential UB when the HashMap drops a
//! value that is still in use/locked.  However this can never be happen because there is no
//! way to drop Entries in a uncontrolled way.  The guard lifetimes are tied to the hosting
//! hashmap the can not outlive it.  Dropping items from the hash map is normally only done
//! from the LRU list which will never contain locked (and thus in-use) Entries. The
//! 'remove(key)' member function checks explicitly that an Entry is not in use or delays the
//! removal until all locks on the Item are released.
//!
//! While the HashMap may reallocate the tables and thus move the Boxes containing the Entries
//! around, this is not a problem since the lock guards contain references to Entries
//! directly, not to the outer Box.
//!
//!
//! Proof that locking is deadlock free
//! -----------------------------------
//!
//! Locks acquired in the same order can never deadlock.  Deadlocks happen only when 2 or more
//! threads wait on a resource while already holding resource another theread is trying to
//! obtain.
//!
//! On lookup the hashmap will be locked. When the element is found the LRU list is locked and
//! the element may be removed from it (when it was not in use). Once done with the LRU list
//! its lock is released.
//!
//! It is worth to mention that code using the cachedb can still deadlock when it acquires
//! locks in ill order. The simplest advise is to have only one single exclusive lock at all
//! time per thread. When is impractical one need to carefully consider locking order or
//! employ other tactics to counter deadlocks.
//!
//!
//! LRU List and expire configuration
//! =================================
//!
//! Items that are not in use are pushed onto the tail of an least-recently-used
//! list. Whenever a CacheDb decides to expire Items these are taken from the head of the
//! lru-list and dropped.
//!
//!
//! TESTS
//! =====
//!
//! The 'test::multithreaded_stress' test can be controlled by environment variables
//!
//!  * 'STRESS_THREADS' sets the number of threads to spawn.  Defaults to 10.
//!  * 'STRESS_WAIT' threads randomly wait up to this much milliseconds to fake some work.  Defaults to 5.
//!  * 'STRESS_ITERATIONS' how many iterations each thread shall do.  Defaults to 100.
//!  * 'STRESS_RANGE' how many unique keys the test uses.  Defaults to 1000.
//!
//! The default values are rather small to make the test suite complete in short time. For dedicated
//! stress testing at least STRESS_ITERATIONS and STRESS_THREADS has to be incresed significantly.
//! Try 'STRESS_ITERATIONS=10000 STRESS_RANGE=10000 STRESS_THREADS=10000' for some harder test.
#![allow(clippy::type_complexity)]
use std::sync::atomic::{AtomicU32, Ordering};
use std::collections::HashSet;
use std::pin::Pin;

#[allow(unused_imports)]
use log::{debug, error, info, trace, warn};
use intrusive_collections::UnsafeRef;
use parking_lot::{MutexGuard, RwLockWriteGuard};

mod entry;
use crate::entry::Entry;
pub use crate::entry::{EntryReadGuard, EntryWriteGuard, KeyTraits};

mod bucket;
use crate::bucket::Bucket;
pub use crate::bucket::Bucketize;

mod locking_method;
pub use crate::locking_method::*;

/// CacheDb implements the concurrent (bucketed) Key/Value store.  Keys must implement
/// 'Bucketize' which has more lax requirments than a full hash implmementation.  'N' is the
/// number of buckets to use. This is const because less dereferencing and management
/// overhead.  Buckets by themself are not very expensive thus it is recommended to use a
/// generous large enough number here.  Think about expected number of concurrenct accesses
/// times four.
#[derive(Debug)]
pub struct CacheDb<K, V, const N: usize>
where
    K: KeyTraits,
{
    buckets:      [Bucket<K, V>; N],
    lru_disabled: AtomicU32,
}

impl<K, V, const N: usize> CacheDb<K, V, N>
where
    K: KeyTraits,
{
    /// Create a new CacheDb
    pub fn new() -> CacheDb<K, V, N> {
        CacheDb {
            buckets:      [(); N].map(|()| Bucket::new()),
            lru_disabled: AtomicU32::new(0),
        }
    }

    /// queries an entry and detaches it from the LRU
    fn query_entry(&self, key: &K) -> Result<(&Bucket<K, V>, *const Entry<K, V>), Error> {
        let bucket = &self.buckets[key.bucket::<N>()];
        let map_lock = bucket.lock_map();

        if let Some(entry) = map_lock.get(key) {
            bucket.use_entry(entry);
            Ok((bucket, &**entry))
        } else {
            Err(Error::NoEntry)
        }
    }

    /// Query the Entry associated with key for reading
    /// the 'method' defines how entries are locked and can be one of:
    ///   * Blocking: normal blocking lock, returns when the lock is acquired
    ///   * TryLock: tries to lock the entry, returns 'Error::LockUnavailable'
    ///     when the lock can't be obtained instantly.
    ///   * Duration: tries to lock the entry with a timeout, returns 'Error::LockUnavailable'
    ///     when the lock can't be obtained within this time.
    ///   * Instant: tries to lock the entry until some point in time, returns 'Error::LockUnavailable'
    ///     when the lock can't be obtained in time.
    ///   All of the can be wraped in 'Recursive()' to allow a thread to relock any lock it already helds.
    pub fn get<'a, M>(&'a self, method: M, key: &K) -> Result<EntryReadGuard<K, V, N>, Error>
    where
        M: 'a + LockingMethod<'a, V>,
    {
        let (bucket, entry_ptr) = self.query_entry(key)?;
        Ok(EntryReadGuard {
            bucket,
            entry: unsafe { &*entry_ptr },
            guard: unsafe { LockingMethod::read(&method, &(*entry_ptr).value)? },
        })
    }

    /// Query the Entry associated with key for writing
    pub fn get_mut<'a, M>(&'a self, method: M, key: &K) -> Result<EntryWriteGuard<K, V, N>, Error>
    where
        M: 'a + LockingMethod<'a, V>,
    {
        let (bucket, entry_ptr) = self.query_entry(key)?;
        Ok(EntryWriteGuard {
            bucket,
            entry: unsafe { &*entry_ptr },
            guard: unsafe { LockingMethod::write(&method, &(*entry_ptr).value)? },
        })
    }

    // queries an entry and detaches it from the LRU or creates a new one
    fn query_or_insert_entry(
        &self,
        key: &K,
    ) -> std::result::Result<
        (&Bucket<K, V>, *const Entry<K, V>),
        (
            &Bucket<K, V>,
            *const Entry<K, V>,
            MutexGuard<HashSet<Pin<Box<entry::Entry<K, V>>>>>,
        ),
    > {
        let bucket = &self.buckets[key.bucket::<N>()];
        let mut map_lock = bucket.lock_map();

        if let Some(entry) = map_lock.get(key) {
            bucket.use_entry(entry);
            Ok((bucket, &**entry))
        } else {
            let entry = Box::pin(Entry::new(key.clone()));
            let entry_ptr: *const Entry<K, V> = &*entry;
            map_lock.insert(entry);
            Err((bucket, entry_ptr, map_lock))
        }
    }

    /// Tries to insert an entry with the given constructor.  Returns Ok(true) when the
    /// constructor was called, Ok(false) when and item is already present under the given key
    /// or some Err() in case the constructor failed.
    pub fn insert<F>(&self, key: &K, ctor: F) -> DynResult<bool>
    where
        F: FnOnce(&K) -> DynResult<V>,
    {
        match self.query_or_insert_entry(key) {
            Ok(_) => Ok(false),
            Err((bucket, entry_ptr, mut map_lock)) => {
                if self.lru_disabled.load(Ordering::Relaxed) == 0 {
                    bucket.maybe_evict(&mut map_lock);
                }

                // need write lock for the ctor, before releasing the map to avoid a race.
                let mut wguard = unsafe { LockingMethod::write(&Blocking, &(*entry_ptr).value)? };

                // release the map_lock, we dont need it anymore
                drop(map_lock);

                // but we have wguard here which allows us to constuct the inner guts
                *wguard = Some(ctor(key)?);

                Ok(true)
            }
        }
    }

    // TODO: The ctor function may become double nested Fn() -> Result(Fn() -> Result(Value)) The
    //       outer can acquire resouces while the cachedb is (temporary) unlocked and returns the
    //       real ctor then.
    /// Query an Entry for reading or construct it (atomically)
    pub fn get_or_insert<'a, M, F>(
        &'a self,
        method: M,
        key: &K,
        ctor: F,
    ) -> DynResult<EntryReadGuard<K, V, N>>
    where
        F: FnOnce(&K) -> DynResult<V>,
        M: 'a + LockingMethod<'a, V>,
    {
        match self.query_or_insert_entry(key) {
            Ok((bucket, entry_ptr)) => Ok(EntryReadGuard {
                bucket,
                entry: unsafe { &*entry_ptr },
                guard: unsafe { LockingMethod::read(&method, &(*entry_ptr).value)? },
            }),
            Err((bucket, entry_ptr, mut map_lock)) => {
                if self.lru_disabled.load(Ordering::Relaxed) == 0 {
                    bucket.maybe_evict(&mut map_lock);
                }

                // need write lock for the ctor, before releasing the map to avoid a race.
                let mut wguard =
                    unsafe { LockingMethod::write(&Blocking, &(*entry_ptr).value).unwrap() };

                // release the map_lock, we dont need it anymore
                drop(map_lock);

                // but we have wguard here which allows us to constuct the inner guts
                *wguard = Some(ctor(key)?);

                // Finally downgrade the lock to a readlock and return the Entry
                Ok(EntryReadGuard {
                    bucket,
                    entry: unsafe { &*entry_ptr },
                    guard: RwLockWriteGuard::downgrade(wguard),
                })
            }
        }
    }

    /// Query an Entry for writing or construct it (atomically)
    pub fn get_or_insert_mut<'a, M, F>(
        &'a self,
        method: M,
        key: &K,
        ctor: F,
    ) -> DynResult<EntryWriteGuard<K, V, N>>
    where
        F: FnOnce(&K) -> DynResult<V>,
        M: 'a + LockingMethod<'a, V>,
    {
        match self.query_or_insert_entry(key) {
            Ok((bucket, entry_ptr)) => Ok(EntryWriteGuard {
                bucket,
                entry: unsafe { &*entry_ptr },
                guard: unsafe { LockingMethod::write(&method, &(*entry_ptr).value)? },
            }),
            Err((bucket, entry_ptr, mut map_lock)) => {
                if self.lru_disabled.load(Ordering::Relaxed) == 0 {
                    bucket.maybe_evict(&mut map_lock);
                }

                // need write lock for the ctor, before releasing the map to avoid a race.
                let mut wguard =
                    unsafe { LockingMethod::write(&Blocking, &(*entry_ptr).value).unwrap() };

                // release the map_lock, we dont need it anymore
                drop(map_lock);

                // but we have wguard here which allows us to constuct the inner guts
                *wguard = Some(ctor(key)?);

                // Finally downgrade the lock to a readlock and return the Entry
                Ok(EntryWriteGuard {
                    bucket,
                    entry: unsafe { &*entry_ptr },
                    guard: wguard,
                })
            }
        }
    }

    /// Disable the LRU eviction. Can be called multiple times, every call should be paired
    /// with a 'enable_lru()' call to reenable the LRU finally. Failing to do so may keep the
    /// CacheDb filling up forever. However this might be intentional to disable the LRU
    /// expiration entirely.
    pub fn disable_lru_eviction(&self) -> &Self {
        self.lru_disabled.fetch_add(1, Ordering::Relaxed);
        self
    }

    /// Re-Enables the LRU eviction after it was disabled. every call must be preceeded by a call to
    /// 'disable_lru()'. Calling it without an matching 'disable_lru()' will panic with an integer underflow.
    pub fn enable_lru_eviction(&self) -> &Self {
        self.lru_disabled.fetch_sub(1, Ordering::Relaxed);
        self
    }

    /// Checks if the CacheDb has the given key stored. Note that this will have a race
    /// condition when other threads access the CacheDb at the same time but may make sense
    /// when lru_eviction is disabled and it can be ensure that no other thread inserts the
    /// key.
    pub fn contains_key(&self, key: &K) -> bool {
        self.buckets[key.bucket::<N>()].lock_map().contains(key)
    }

    /// The 'cache_target' will only recalculated after this many inserts. Should be in the
    /// lower hundreds.
    pub fn config_target_cooldown(&self, target_cooldown: u32) -> &Self {
        for bucket in &self.buckets {
            bucket
                .target_cooldown
                .store(target_cooldown, Ordering::Relaxed);
        }
        self
    }

    /// Sets the lower limit for the 'cache_target' linear interpolation region.  Some
    /// hundreds to thousands of entries are recommended. Should be less than
    /// 'max_capacity_limit'.
    pub fn config_min_capacity_limit(&self, min_capacity_limit: usize) -> &Self {
        for bucket in &self.buckets {
            // divide by N so that each bucket gets its share
            bucket
                .min_capacity_limit
                .store(min_capacity_limit / N, Ordering::Relaxed);
        }
        self
    }

    /// Sets the upper limit for the 'cache_target' linear interpolation region.
    /// Should be fine around the maximum expected number of entries.
    pub fn config_max_capacity_limit(&self, max_capacity_limit: usize) -> &Self {
        for bucket in &self.buckets {
            // divide by N so that each bucket gets its share
            bucket
                .max_capacity_limit
                .store(max_capacity_limit / N, Ordering::Relaxed);
        }
        self
    }

    /// Sets the lower limit for the 'cache_target' in percent at 'max_capacity_limit'. Since
    /// when very much entries are stored it is desireable to have a lower percentage of
    /// cached items for wasting less memory. Note that this counts against the 'capacity' of
    /// the underlying container, not the stored entries. Recommended values are around 5%,
    /// but may vary on the access patterns. Should be lower than 'max_cache_percent'
    pub fn config_min_cache_percent(&self, min_cache_percent: u8) -> &Self {
        assert!(min_cache_percent < 100);
        for bucket in &self.buckets {
            bucket
                .min_cache_percent
                .store(min_cache_percent, Ordering::Relaxed);
        }
        self
    }

    /// Sets the upper limit for the 'cache_target' in percent at 'min_capacity_limit'. When
    /// only few entries are stored in a CacheDb it is reasonable to use a lot space for
    /// caching. Note that this counts against the 'capacity' of the underlying container,
    /// thus it should be not significantly over 60% at most.
    pub fn config_max_cache_percent(&self, max_cache_percent: u8) -> &Self {
        assert!(max_cache_percent < 100);
        for bucket in &self.buckets {
            bucket
                .max_cache_percent
                .store(max_cache_percent, Ordering::Relaxed);
        }
        self
    }

    /// Sets the number of entries removed at once when evicting entries from the cache. Since
    /// evicting branches into the code parts for removing the entries and calling their
    /// destructors it is a bit more cache friendly to batch a few such things together.
    pub fn config_evict_batch(&self, evict_batch: u8) -> &Self {
        for bucket in &self.buckets {
            bucket.evict_batch.store(evict_batch, Ordering::Relaxed);
        }
        self
    }

    /// Evicts up to number entries. The implementation is pretty simple trying to evict number/N from
    /// each bucket. Thus when the distribution is not optimal fewer elements will be removed.
    /// Will not remove any entries when the lru eviction is disabled.
    /// Returns the number of items that got evicted.
    pub fn evict(&self, number: usize) -> usize {
        if self.lru_disabled.load(Ordering::Relaxed) == 0 {
            let mut evicted = number;
            for bucket in &self.buckets {
                evicted -= bucket.evict(number / N, &mut bucket.lock_map());
            }
            evicted
        } else {
            0
        }
    }
}

impl<K, V, const N: usize> Default for CacheDb<K, V, N>
where
    K: KeyTraits,
{
    fn default() -> Self {
        Self::new()
    }
}

/// Result type that boxes the error. Allows constructors to return arbitary errors.
pub type DynResult<T> = std::result::Result<T, Box<dyn std::error::Error>>;

/// The errors that CacheDb implements itself. Note that the constructors can return other
/// errors as well ('DynResult' is returned in those case).
#[derive(Debug)]
pub enum Error {
    /// The Entry was not found
    NoEntry,
    /// Locking an entry failed
    LockUnavailable,
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Error::NoEntry => write!(f, "Entry not found"),
            Error::LockUnavailable => write!(f, "Trying to lock failed"),
        }
    }
}

impl std::error::Error for Error {}

#[cfg(test)]
mod test {
    use std::collections::HashMap;
    use std::env;
    use std::sync::{Arc, Barrier};
    use std::{thread, time};
    use std::sync::atomic::AtomicU64;
    #[cfg(feature = "logging")]
    use std::io::Write;

    use rand::Rng;

    use crate::*;

    #[cfg(feature = "logging")]
    fn init() {
        let counter: AtomicU64 = AtomicU64::new(0);
        let seq_num = move || counter.fetch_add(1, Ordering::SeqCst);

        env_logger::Builder::from_default_env()
            .format(move |buf, record| {
                writeln!(
                    buf,
                    "{:0>12}: {:>5}: {}:{}: {}: {}",
                    seq_num(),
                    record.level().as_str(),
                    record.file().unwrap_or(""),
                    record.line().unwrap_or(0),
                    std::thread::current().name().unwrap_or("UNKNOWN"),
                    record.args()
                )
            })
            .try_init()
            .unwrap();
    }

    #[cfg(not(feature = "logging"))]
    fn init() {}

    // using the default hash based implementation for tests here
    impl Bucketize for String {}
    impl Bucketize for u16 {
        fn bucket<const N: usize>(&self) -> usize {
            let r = *self as usize % N;
            #[cfg(feature = "logging")]
            trace!("key {} falls into bucket {}", self, r);
            r
        }
    }

    impl KeyTraits for String {}
    impl KeyTraits for u16 {}

    #[test]
    fn create() {
        init();
        let cdb = CacheDb::<String, String, 16>::new();

        println!("Debug {:?}", &cdb);
        assert!(cdb.get(Blocking, &"foo".to_string()).is_err());
    }

    #[test]
    fn insert_foobar_onebucket() {
        init();
        let cdb = CacheDb::<String, String, 1>::new();

        assert!(
            cdb.get_or_insert(Blocking, &"foo".to_string(), |_| Ok("bar".to_string()))
                .is_ok()
        );
        assert_eq!(
            *cdb.get(Blocking, &"foo".to_string()).unwrap(),
            "bar".to_string()
        );
        assert!(
            cdb.get_or_insert(Blocking, &"bar".to_string(), |_| Ok("foo".to_string()))
                .is_ok()
        );
        assert_eq!(
            *cdb.get(Blocking, &"bar".to_string()).unwrap(),
            "foo".to_string()
        );
        assert!(
            cdb.get_or_insert(Blocking, &"foo2".to_string(), |_| Ok("bar2".to_string()))
                .is_ok()
        );
        assert_eq!(
            *cdb.get(Blocking, &"foo2".to_string()).unwrap(),
            "bar2".to_string()
        );
        assert!(
            cdb.get_or_insert(Blocking, &"bar2".to_string(), |_| Ok("foo2".to_string()))
                .is_ok()
        );
        assert_eq!(
            *cdb.get(Blocking, &"bar2".to_string()).unwrap(),
            "foo2".to_string()
        );
    }

    #[test]
    fn insert_foobar() {
        init();
        let cdb = CacheDb::<String, String, 16>::new();

        assert!(
            cdb.get_or_insert(Blocking, &"foo".to_string(), |_| Ok("bar".to_string()))
                .is_ok()
        );
        assert_eq!(
            *cdb.get(Blocking, &"foo".to_string()).unwrap(),
            "bar".to_string()
        );
        assert!(
            cdb.get_or_insert(Blocking, &"bar".to_string(), |_| Ok("foo".to_string()))
                .is_ok()
        );
        assert_eq!(
            *cdb.get(Blocking, &"bar".to_string()).unwrap(),
            "foo".to_string()
        );
        assert!(
            cdb.get_or_insert(Blocking, &"foo2".to_string(), |_| Ok("bar2".to_string()))
                .is_ok()
        );
        assert_eq!(
            *cdb.get(Blocking, &"foo2".to_string()).unwrap(),
            "bar2".to_string()
        );
        assert!(
            cdb.get_or_insert(Blocking, &"bar2".to_string(), |_| Ok("foo2".to_string()))
                .is_ok()
        );
        assert_eq!(
            *cdb.get(Blocking, &"bar2".to_string()).unwrap(),
            "foo2".to_string()
        );
    }

    #[test]
    fn insert_unit() {
        init();
        let cdb = CacheDb::<String, (), 16>::new();

        assert!(cdb.insert(&"foo".to_string(), |_| Ok(())).is_ok());
        assert_eq!(*cdb.get(Blocking, &"foo".to_string()).unwrap(), ());

        assert!(cdb.insert(&"bar".to_string(), |_| Ok(())).is_ok());
        assert_eq!(*cdb.get(Blocking, &"bar".to_string()).unwrap(), ());

        assert_eq!(cdb.contains_key(&"foo".to_string()), true);
        assert_eq!(cdb.contains_key(&"bar".to_string()), true);
        assert_eq!(cdb.contains_key(&"baz".to_string()), false);
    }

    #[test]
    fn trylocks() {
        init();
        let cdb = CacheDb::<String, String, 16>::new();

        assert!(
            cdb.get_or_insert(Blocking, &"foo".to_string(), |_| Ok("bar".to_string()))
                .is_ok()
        );
        assert_eq!(
            *cdb.get(TryLock, &"foo".to_string()).unwrap(),
            "bar".to_string()
        );
        assert_eq!(
            *cdb.get(Duration::from_millis(100), &"foo".to_string())
                .unwrap(),
            "bar".to_string()
        );
        assert_eq!(
            *cdb.get(
                Instant::now() + Duration::from_millis(100),
                &"foo".to_string()
            )
            .unwrap(),
            "bar".to_string()
        );
    }

    #[test]
    fn recursivelocks() {
        init();
        let cdb = CacheDb::<String, String, 16>::new();

        assert!(
            cdb.get_or_insert(Blocking, &"foo".to_string(), |_| Ok("bar".to_string()))
                .is_ok()
        );

        let l1 = cdb.get(Recursive(Blocking), &"foo".to_string()).unwrap();
        assert_eq!(*l1, "bar".to_string());

        let l2 = cdb.get(Recursive(TryLock), &"foo".to_string()).unwrap();
        assert_eq!(*l2, "bar".to_string());

        let l3 = cdb
            .get(Recursive(Duration::from_millis(100)), &"foo".to_string())
            .unwrap();
        assert_eq!(*l3, "bar".to_string());

        let l4 = cdb
            .get(
                Recursive(Instant::now() + Duration::from_millis(100)),
                &"foo".to_string(),
            )
            .unwrap();
        assert_eq!(*l4, "bar".to_string());
    }

    #[test]
    fn mutate() {
        init();
        let cdb = CacheDb::<String, String, 16>::new();

        cdb.get_or_insert(Blocking, &"foo".to_string(), |_| Ok("bar".to_string()))
            .unwrap();

        *cdb.get_mut(Blocking, &"foo".to_string()).unwrap() = "baz".to_string();
        assert_eq!(
            *cdb.get(Blocking, &"foo".to_string()).unwrap(),
            "baz".to_string()
        );
    }

    #[test]
    fn insert_mutate() {
        init();
        let cdb = CacheDb::<String, String, 16>::new();

        let mut foo = cdb
            .get_or_insert_mut(Blocking, &"foo".to_string(), |_| Ok("bar".to_string()))
            .unwrap();
        assert_eq!(*foo, "bar".to_string());
        *foo = "baz".to_string();
        assert_eq!(*foo, "baz".to_string());
        drop(foo);
        assert_eq!(
            *cdb.get(Blocking, &"foo".to_string()).unwrap(),
            "baz".to_string()
        );
    }

    #[test]
    pub fn multithreaded_stress() {
        const BUCKETS: usize = 64;
        init();
        let cdb = Arc::new(CacheDb::<u16, u16, BUCKETS>::new());

        let num_threads: usize = env::var("STRESS_THREADS")
            .unwrap_or("10".to_string())
            .parse()
            .unwrap();
        let wait_millis: u64 = env::var("STRESS_WAIT")
            .unwrap_or("5".to_string())
            .parse()
            .unwrap();
        let iterations: u64 = env::var("STRESS_ITERATIONS")
            .unwrap_or("100".to_string())
            .parse()
            .unwrap();
        let range: u16 = env::var("STRESS_RANGE")
            .unwrap_or("1000".to_string())
            .parse()
            .unwrap();

        let mut handles = Vec::with_capacity(num_threads);
        let barrier = Arc::new(Barrier::new(num_threads));
        for thread_num in 0..num_threads {
            let c = Arc::clone(&barrier);
            let cdb = Arc::clone(&cdb);

            handles.push(
                thread::Builder::new()
                    .name(thread_num.to_string())
                    .spawn(
                        // The per thread function
                        move || {
                            let mut rng = rand::thread_rng();
                            c.wait();

                            let mut locked =
                                HashMap::<u16, EntryReadGuard<u16, u16, BUCKETS>>::new();
                            let mut maxlocked: u16 = 0;

                            for _ in 0..iterations {
                                // r is the key we handle
                                let r = rng.gen_range(0..range);
                                // p is the probability of some operation
                                let p = rng.gen_range(0..100);
                                // w is the wait time to simulate thread work
                                let w = if wait_millis > 0 {
                                    Some(time::Duration::from_millis(rng.gen_range(0..wait_millis)))
                                } else {
                                    None
                                };
                                match locked.remove(&r) {
                                    // thread had no lock stored, create a new entry
                                    None => {
                                        if p < 15 {
                                            // TODO: remove
                                        } else if p < 30 {
                                            // TODO: touch
                                        } else if p < 50 {
                                            // #[cfg(feature = "logging")]
                                            // trace!("get_or_insert {} and keep it", r);
                                            // locked.insert(
                                            //     r,
                                            //     cdb.get_or_insert(&r, |_| Ok(!r)).unwrap(),
                                            // );
                                            // #[cfg(feature = "logging")]
                                            // trace!("got {}", r);
                                        } else if p < 55 {
                                            if r > maxlocked {
                                                maxlocked = r;
                                                #[cfg(feature = "logging")]
                                                trace!(
                                                    "get_or_insert_mut {} and then wait/work for {:?}",
                                                    r,
                                                    w
                                                );
                                                let lock =
                                                    cdb.get_or_insert_mut(Duration::from_millis(500), &r, |_| Ok(!r));
                                                #[cfg(feature = "logging")]
                                                trace!("got {}", r);
                                                if let Some(w) = w {
                                                    thread::sleep(w)
                                                }
                                                drop(lock);
                                            } else {
                                                maxlocked = 0;
                                                #[cfg(feature = "logging")]
                                                trace!("drop all stored locks");
                                                locked.clear();
                                            }
                                        } else if p < 60 {
                                            #[cfg(feature = "logging")]
                                            trace!(
                                                "wait/work for {:?} and then get_or_insert_mut {}",
                                                w,
                                                r
                                            );
                                            if let Some(w) = w {
                                                thread::sleep(w)
                                            }
                                            let lock =
                                                cdb.get_or_insert_mut(Duration::from_millis(500), &r, |_| Ok(!r));
                                            #[cfg(feature = "logging")]
                                            trace!("got {}", r);
                                            drop(lock);
                                        } else if p < 80 {
                                            #[cfg(feature = "logging")]
                                            trace!(
                                                "get_or_insert {} and then wait/work for {:?}",
                                                r,
                                                w
                                            );
                                            let lock = cdb.get_or_insert(Blocking, &r, |_| Ok(!r)).unwrap();
                                            #[cfg(feature = "logging")]
                                            trace!("got {}", r);
                                            if let Some(w) = w {
                                                thread::sleep(w)
                                            }
                                            drop(lock);
                                        } else {
                                            #[cfg(feature = "logging")]
                                            trace!(
                                                "wait/work for {:?} and then get_or_insert {}",
                                                w,
                                                r
                                            );
                                            if let Some(w) = w {
                                                thread::sleep(w)
                                            }
                                            let lock = cdb.get_or_insert(Blocking, &r, |_| Ok(!r)).unwrap();
                                            #[cfg(feature = "logging")]
                                            trace!("got {}", r);
                                            drop(lock);
                                        }
                                    }

                                    // locked already for reading, lets drop it
                                    Some(read_guard) => {
                                        if p < 95 {
                                            #[cfg(feature = "logging")]
                                            trace!("unlock kept readguard {}", r);
                                            drop(read_guard);
                                        } else {
                                            // TODO: drop-remove
                                            drop(read_guard);
                                        }
                                    }
                                };
                            }
                            drop(locked);
                        },
                    )
                    .unwrap(),
            );
        }

        // TODO: finally assert that nothing is locked

        for handle in handles {
            handle.join().unwrap();
        }
    }
}

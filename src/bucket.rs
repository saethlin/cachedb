#![allow(clippy::type_complexity)]
use std::collections::{hash_map::DefaultHasher, HashSet};
use std::hash::{Hash, Hasher};
use std::pin::Pin;
use std::sync::atomic::{AtomicU32, AtomicU8, AtomicUsize, Ordering};

use intrusive_collections::LinkedList;
#[allow(unused_imports)]
pub use log::{debug, error, info, trace, warn};
use parking_lot::{Mutex, MutexGuard};

use crate::entry::EntryAdapter;
use crate::Entry;
use crate::KeyTraits;
use crate::UnsafeRef;

/// The internal representation of a Bucket.
///
/// The LRU eviction is per bucket, this is most efficient and catches the corner cases where
/// one bucket sees more entries than others.
///
/// The eviction caclculation adapts itself and works as follows:
///
/// The maximum number of elements used is tracked. This maximum number becomes decreased by
/// 'maxused/maxused_reduction+1' after 'max_cooldown' operations. The number of elements in
/// the LRU list is known as well. These two values are used to determine how percent the
/// cached part makes up. As long the hash table has free entries within its capacity things
/// become cached. When the capacity is depleted it is checked if the percent cached items
/// exceed the configured 'cold_target' percentage. If so, some 'evict_batch' entries are
/// dropped, if not a new entry will just be added to the hashtable which will force it to grow.
///
/// The 'cold_target' percentage is calculated by to be between 'cold_max' to 'cold_min' by by
/// linear interpolation from 'min_entries_limit' to 'max_entries_limit'. Thus allowing a high
/// cache ratio when memory requirements are modest and reduce the memory usage for caching at
/// higher memory loads.
pub(crate) struct Bucket<K, V>
where
    K: KeyTraits,
{
    map:      Mutex<HashSet<Pin<Box<Entry<K, V>>>>>,
    lru_list: Mutex<LinkedList<EntryAdapter<K, V>>>,

    // Stats section
    pub(crate) cold: AtomicUsize,
    maxused:         AtomicUsize,

    // State section
    maxused_countdown:      AtomicU32,
    pub(crate) cold_target: AtomicU8,

    // Configuration
    pub(crate) maxused_cooldown:  AtomicU32,
    pub(crate) maxused_reduction: AtomicUsize,
    pub(crate) max_entries_limit: AtomicUsize,
    pub(crate) min_entries_limit: AtomicUsize,

    pub(crate) cold_max:    AtomicU8,
    pub(crate) cold_min:    AtomicU8,
    pub(crate) evict_batch: AtomicU8,
}

impl<K, V> Bucket<K, V>
where
    K: KeyTraits,
{
    pub(crate) fn new() -> Self {
        Self {
            map:               Mutex::new(HashSet::new()),
            lru_list:          Mutex::new(LinkedList::new(EntryAdapter::new())),
            cold:              AtomicUsize::new(0),
            maxused:           AtomicUsize::new(0),
            maxused_countdown: AtomicU32::new(0),
            cold_target:       AtomicU8::new(50),
            maxused_cooldown:  AtomicU32::new(1000),
            maxused_reduction: AtomicUsize::new(10000),
            max_entries_limit: AtomicUsize::new(10000000),
            min_entries_limit: AtomicUsize::new(1000),
            cold_max:          AtomicU8::new(60),
            cold_min:          AtomicU8::new(5),
            evict_batch:       AtomicU8::new(4),
        }
    }

    pub(crate) fn lock_map(&self) -> MutexGuard<HashSet<Pin<Box<Entry<K, V>>>>> {
        self.map.lock()
    }

    pub(crate) fn use_entry(
        &self,
        entry: &Entry<K, V>,
        map_lock: &MutexGuard<HashSet<Pin<Box<Entry<K, V>>>>>,
    ) {
        let mut lru_lock = self.lru_list.lock();
        if entry.lru_link.is_linked() {
            unsafe { lru_lock.cursor_mut_from_ptr(&*entry).remove() };
            self.cold.fetch_sub(1, Ordering::Relaxed);
            self.update_maxused(map_lock);
        }
        entry.use_count.fetch_add(1, Ordering::Relaxed);
    }

    pub(crate) fn unuse_entry(&self, entry: &Entry<K, V>) {
        let mut lru_lock = self.lru_list.lock();
        if entry.use_count.fetch_sub(1, Ordering::Relaxed) == 1 {
            self.cold.fetch_add(1, Ordering::Relaxed);
            lru_lock.push_back(unsafe { UnsafeRef::from_raw(entry) });
        }
    }

    /// Updates the max used entry stat. This is called before creating a new entry, thus it
    /// preempts this by adding one to the length queried from the map.
    /// returns the adjusted 'maxused' value.
    pub(crate) fn update_maxused(
        &self,
        map_lock: &MutexGuard<HashSet<Pin<Box<Entry<K, V>>>>>,
    ) -> usize {
        // since we got the map locked we can be sloppy with atomics

        let now_used = map_lock.len() + 1 - self.cold.load(Ordering::Relaxed);
        // update maxused
        self.maxused.fetch_max(now_used, Ordering::Relaxed);
        let mut maxused = self.maxused.load(Ordering::Relaxed);

        // maxused_countdown handling
        let countdown = self.maxused_countdown.load(Ordering::Relaxed);
        if countdown > 0 {
            // just keep counting down
            self.maxused_countdown
                .store(countdown - 1, Ordering::Relaxed)
        } else {
            // Do some work, reset it to cooldown period, decrement maxused
            self.maxused_countdown.store(
                self.maxused_cooldown.load(Ordering::Relaxed),
                Ordering::Relaxed,
            );
            if maxused > 0 && maxused != now_used {
                maxused -= maxused / self.maxused_reduction.load(Ordering::Relaxed) + 1;
                self.maxused.store(maxused, Ordering::Relaxed);
            }

            // TODO: recalculate low/highwater
        }

        maxused
    }

    /// evicts up to 'n' entries from the LRU list. Returns the number of evicted entries which
    /// may be less than 'n' in case the list got depleted.
    pub fn evict(
        &self,
        n: usize,
        map_lock: &mut MutexGuard<HashSet<Pin<Box<Entry<K, V>>>>>,
    ) -> usize {
        #[cfg(feature = "logging")]
        debug!("evicting {} elements", n);
        for i in 0..n {
            if let Some(entry) = self.lru_list.lock().pop_front() {
                map_lock.remove(&entry.key);
                self.cold.fetch_sub(1, Ordering::Relaxed);
            } else {
                return i;
            }
        }
        n
    }
}

/// Defines into which bucket a key falls. The default implementation uses the Hash trait for
/// this. Custom implementations can override this to something more simple. It is recommended
/// to implement this because very good distribution of the resulting value is not as
/// important as for the hashmap.
pub trait Bucketize: Hash {
    // Must return an value 0..N-1 otherwise CacheDb will panic with array access out of bounds.
    fn bucket<const N: usize>(&self) -> usize {
        let mut hasher = DefaultHasher::new();
        self.hash(&mut hasher);
        hasher.finish() as usize % N
    }
}

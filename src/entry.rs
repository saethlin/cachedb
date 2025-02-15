use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
#[cfg(feature = "logging")]
use std::fmt::Debug;
use std::marker::PhantomPinned;
use std::ops::{Deref, DerefMut};
use std::pin::Pin;
use std::hash::{Hash, Hasher};
use std::borrow::Borrow;

use intrusive_collections::{intrusive_adapter, LinkedListLink, UnsafeRef};
use parking_lot::{RwLock, RwLockReadGuard, RwLockWriteGuard};

use crate::{bucket::Bucket, Bucketize};

/// Collects the traits a Key must implement, any user defined Key type must implement this
/// trait and any traits it derives from.
/// The 'Debug' trait is only required when the feature 'logging' is enabled.
#[cfg(not(feature = "logging"))]
pub trait KeyTraits: Eq + Clone + Bucketize {}
#[cfg(feature = "logging")]
pub trait KeyTraits: Eq + Clone + Bucketize + Debug {}

/// User data is stored behind RwLocks in an entry. Furthermore some management information
/// like the LRU list node are stored here. Entries have stable addresses and can't be moved
/// in memory.
pub(crate) struct Entry<K, V> {
    pub(crate) key:       K,
    // The Option is only used for delaying the construction with write lock held.
    pub(crate) value:     RwLock<Option<V>>,
    pub(crate) lru_link:  LinkedListLink, // protected by lru_list mutex
    pub(crate) use_count: AtomicUsize,
    pub(crate) expire:    AtomicBool,
    _pin:                 PhantomPinned,
}

intrusive_adapter!(pub(crate) EntryAdapter<K, V> = UnsafeRef<Entry<K, V>>: Entry<K, V> { lru_link: LinkedListLink });

impl<K: KeyTraits, V> Entry<K, V> {
    pub(crate) fn new(key: K) -> Self {
        Entry {
            key,
            value: RwLock::new(None),
            lru_link: LinkedListLink::new(),
            use_count: AtomicUsize::new(1),
            expire: AtomicBool::new(false),
            _pin: PhantomPinned,
        }
    }
}

// Hashes only over the key part.
impl<K: KeyTraits, V> Hash for Entry<K, V> {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.key.hash(state);
    }
}

// Compares only the key.
impl<K: PartialEq, V> PartialEq for Entry<K, V> {
    fn eq(&self, other: &Self) -> bool {
        self.key == other.key
    }
}

impl<K: PartialEq, V> Eq for Entry<K, V> {}

// We need this to be able to lookup a Key in a HashSet containing pinboxed entries.
impl<K, V> Borrow<K> for Pin<Box<Entry<K, V>>>
where
    K: KeyTraits,
{
    fn borrow(&self) -> &K {
        &self.key
    }
}

/// Guard for the read lock. Puts unused entries into the LRU list.
pub struct EntryReadGuard<'a, K, V, const N: usize>
where
    K: KeyTraits,
{
    pub(crate) bucket: &'a Bucket<K, V>,
    pub(crate) entry:  &'a Entry<K, V>,
    pub(crate) guard:  RwLockReadGuard<'a, Option<V>>,
}

impl<'a, K, V, const N: usize> EntryReadGuard<'_, K, V, N>
where
    K: KeyTraits,
{
    /// Mark the entry for expiration. When dropped it will be put in front of the LRU list
    /// and by that evicted soon. Use with care, when many entries become pushed to the front,
    /// they eventually bubble up again.
    fn expire(&mut self) {
        self.entry.expire.store(true, Ordering::Relaxed);
    }
}

impl<'a, K, V, const N: usize> Drop for EntryReadGuard<'_, K, V, N>
where
    K: KeyTraits,
{
    fn drop(&mut self) {
        self.bucket.unuse_entry(self.entry);
    }
}

impl<'a, K, V, const N: usize> Deref for EntryReadGuard<'_, K, V, N>
where
    K: KeyTraits,
{
    type Target = V;

    fn deref(&self) -> &Self::Target {
        // unwrap is safe, the option is only None for a short time while constructing a new value
        (*self.guard).as_ref().unwrap()
    }
}

/// Guard for the write lock. Puts unused entries into the LRU list.
pub struct EntryWriteGuard<'a, K, V, const N: usize>
where
    K: KeyTraits,
{
    pub(crate) bucket: &'a Bucket<K, V>,
    pub(crate) entry:  &'a Entry<K, V>,
    pub(crate) guard:  RwLockWriteGuard<'a, Option<V>>,
}

impl<'a, K, V, const N: usize> EntryWriteGuard<'_, K, V, N>
where
    K: KeyTraits,
{
    /// Mark the entry for expiration. When dropped it will be put in front of the LRU list
    /// and by that evicted soon. Use with care, when many entries become pushed to the front,
    /// they eventually bubble up again.
    fn expire(&mut self) {
        self.entry.expire.store(true, Ordering::Relaxed);
    }
}

impl<'a, K, V, const N: usize> Drop for EntryWriteGuard<'_, K, V, N>
where
    K: KeyTraits,
{
    fn drop(&mut self) {
        self.bucket.unuse_entry(self.entry);
    }
}

impl<'a, K, V, const N: usize> Deref for EntryWriteGuard<'_, K, V, N>
where
    K: KeyTraits,
{
    type Target = V;

    fn deref(&self) -> &Self::Target {
        // unwrap is safe, the option is only None for a short time while constructing a new value
        (*self.guard).as_ref().unwrap()
    }
}

impl<'a, K, V, const N: usize> DerefMut for EntryWriteGuard<'_, K, V, N>
where
    K: KeyTraits,
{
    fn deref_mut(&mut self) -> &mut Self::Target {
        // unwrap is safe, the option is only None for a short time while constructing a new value
        (*self.guard).as_mut().unwrap()
    }
}

//! A concurrent insert only hash map.
//!
//! This crate implements a "memo map" which is in many ways similar to a
//! [`HashMap`] with some crucial differences:
//!
//! * Unlike a regular hash map, a memo map is thread safe and synchronized.
//! * Once a value has been placed in the memo map it can be neither removed nor replaced.
//! * Retrieving a value from a memo map returns a plain old reference.
//!
//! Together these purposes allow one to use this type of structure to
//! implement something similar to lazy loading in places where the API
//! has been constrained to references before.
//!
//! For this to work the value placed in the [`MemoMap`] has to implement
//! [`StableDeref`].  If the value you want to place there does not implement
//! it you can generally wrap it in a [`Box`].
//!
//! ```
//! use memo_map::MemoMap;
//!
//! let memo = MemoMap::new();
//! let one = memo.get_or_insert(&1, || "one".to_string());
//! let one2 = memo.get_or_insert(&1, || "not one".to_string());
//! assert_eq!(one, "one");
//! assert_eq!(one2, "one");
//! ```
//!
//! # Notes on Iteration
//!
//! Because the memo map internally uses a mutex it needs to be held during
//! iteration.  This is potentially dangerous as it means you can easily
//! deadlock yourself when trying to use the memo map while iterating.  The
//! iteration functionality thus has to be used with great care.
use std::borrow::Borrow;
use std::collections::hash_map::{Entry, RandomState};
use std::collections::HashMap;
use std::convert::Infallible;
use std::hash::{BuildHasher, Hash};
use std::mem::{transmute, ManuallyDrop};
use std::sync::{Mutex, MutexGuard};

use stable_deref_trait::StableDeref;

macro_rules! lock {
    ($mutex:expr) => {
        match $mutex.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        }
    };
}

/// An insert only, thread safe hash map to memoize values.
#[derive(Debug)]
pub struct MemoMap<K, V, S = RandomState> {
    inner: Mutex<HashMap<K, V, S>>,
}

impl<K: Clone, V: Clone, S: Clone> Clone for MemoMap<K, V, S> {
    fn clone(&self) -> Self {
        Self {
            inner: Mutex::new(lock!(self.inner).clone()),
        }
    }
}

impl<K, V, S: Default> Default for MemoMap<K, V, S> {
    fn default() -> Self {
        MemoMap {
            inner: Mutex::new(HashMap::default()),
        }
    }
}

impl<K, V> MemoMap<K, V, RandomState> {
    /// Creates an empty `MemoMap`.
    pub fn new() -> MemoMap<K, V, RandomState> {
        MemoMap {
            inner: Mutex::default(),
        }
    }
}

impl<K, V, S> MemoMap<K, V, S> {
    /// Creates an empty `MemoMap` which will use the given hash builder to hash
    /// keys.
    pub fn with_hasher(hash_builder: S) -> MemoMap<K, V, S> {
        MemoMap {
            inner: Mutex::new(HashMap::with_hasher(hash_builder)),
        }
    }
}

impl<K, V, S> MemoMap<K, V, S>
where
    K: Eq + Hash,
    V: StableDeref,
    S: BuildHasher,
{
    /// Inserts a value into the memo map.
    ///
    /// This inserts a value for a specific key into the memo map.  If the
    /// key already exists, this method does nothing and instead returns `false`.
    /// Otherwise the value is inserted and `true` is returned.  It's generally
    /// recommended to instead use [`get_or_insert`](Self::get_or_insert) or
    /// it's sibling [`get_or_try_insert`](Self::get_or_try_insert).
    pub fn insert(&self, key: K, value: V) -> bool {
        let mut inner = lock!(self.inner);
        match inner.entry(key) {
            Entry::Occupied(_) => false,
            Entry::Vacant(vacant) => {
                vacant.insert(value);
                true
            }
        }
    }

    /// Returns true if the map contains a value for the specified key.
    ///
    /// The key may be any borrowed form of the map's key type, but [`Hash`] and
    /// [`Eq`] on the borrowed form must match those for the key type.
    pub fn contains_key<Q>(&self, key: &Q) -> bool
    where
        Q: Hash + Eq + ?Sized,
        K: Borrow<Q>,
    {
        lock!(self.inner).contains_key(key)
    }

    /// Returns a reference to the value corresponding to the key.
    ///
    /// The key may be any borrowed form of the map's key type, but [`Hash`] and
    /// [`Eq`] on the borrowed form must match those for the key type.
    pub fn get<Q>(&self, key: &Q) -> Option<&V>
    where
        Q: Hash + Eq + ?Sized,
        K: Borrow<Q>,
    {
        let inner = lock!(self.inner);
        let value = inner.get(key)?;
        Some(unsafe { transmute::<_, _>(value) })
    }

    /// Returns a reference to the value corresponding to the key or inserts.
    ///
    /// This is the preferred way to work with a memo map: if the value has not
    /// been in the map yet the creator function is invoked to create the value,
    /// otherwise the already stored value is returned.  The creator function itself
    /// can be falliable and the error is passed through.
    ///
    /// If the creator is infallible, [`get_or_insert`](Self::get_or_insert) can be used.
    pub fn get_or_try_insert<Q, F, E>(&self, key: &Q, creator: F) -> Result<&V, E>
    where
        Q: Hash + Eq + ToOwned<Owned = K> + ?Sized,
        K: Borrow<Q>,
        F: FnOnce() -> Result<V, E>,
    {
        let mut inner = lock!(self.inner);
        let value = if let Some(value) = inner.get(key) {
            value
        } else {
            inner.insert(key.to_owned(), creator()?);
            inner.get(key).unwrap()
        };
        Ok(unsafe { transmute::<_, _>(value) })
    }

    /// Returns a reference to the value corresponding to the key or inserts.
    ///
    /// This is the preferred way to work with a memo map: if the value has not
    /// been in the map yet the creator function is invoked to create the value,
    /// otherwise the already stored value is returned.
    ///
    /// If the creator is fallible, [`get_or_try_insert`](Self::get_or_try_insert) can be used.
    ///
    /// # Example
    ///
    /// ```
    /// # use memo_map::MemoMap;
    /// let memo = MemoMap::new();
    ///
    /// // first time inserts
    /// let value = memo.get_or_insert("key", || "23");
    /// assert_eq!(*value, "23");
    ///
    /// // second time returns old value
    /// let value = memo.get_or_insert("key", || "24");
    /// assert_eq!(*value, "23");
    /// ```
    pub fn get_or_insert<Q, F>(&self, key: &Q, creator: F) -> &V
    where
        Q: Hash + Eq + ToOwned<Owned = K> + ?Sized,
        K: Borrow<Q>,
        F: FnOnce() -> V,
    {
        self.get_or_try_insert::<_, _, Infallible>(key, || Ok(creator()))
            .unwrap()
    }

    /// Returns the number of items in the map.
    ///
    /// # Example
    ///
    /// ```
    /// # use memo_map::MemoMap;
    /// let memo = MemoMap::new();
    ///
    /// assert_eq!(memo.len(), 0);
    /// memo.insert(1, "a");
    /// memo.insert(2, "b");
    /// memo.insert(2, "not b");
    /// assert_eq!(memo.len(), 2);
    /// ```
    pub fn len(&self) -> usize {
        lock!(self.inner).len()
    }

    /// Returns `true` if the memo map contains no items.
    pub fn is_empty(&self) -> bool {
        lock!(self.inner).is_empty()
    }

    /// An iterator visiting all key-value pairs in arbitrary order. The
    /// iterator element type is `(&'a K, &'a V)`.
    ///
    /// Important note: during iteration the map is locked!  This means that you
    /// must not perform modifications to the map or you will run into deadlocks.
    pub fn iter(&self) -> Iter<'_, K, V, S> {
        let guard = lock!(self.inner);
        let iter = guard.iter();
        Iter {
            iter: unsafe { transmute::<_, _>(iter) },
            guard: ManuallyDrop::new(guard),
        }
    }

    /// An iterator visiting all keys in arbitrary order. The iterator element
    /// type is `&'a K`.
    pub fn keys(&self) -> Keys<'_, K, V, S> {
        Keys { iter: self.iter() }
    }
}

/// An iterator over the items of a [`MemoMap`].
///
/// This struct is created by the [`iter`](MemoMap::iter) method on [`MemoMap`].
/// See its documentation for more information.
pub struct Iter<'a, K, V, S> {
    guard: ManuallyDrop<MutexGuard<'a, HashMap<K, V, S>>>,
    iter: std::collections::hash_map::Iter<'a, K, V>,
}

impl<'a, K, V, S> Drop for Iter<'a, K, V, S> {
    fn drop(&mut self) {
        unsafe {
            ManuallyDrop::drop(&mut self.guard);
        }
    }
}

impl<'a, K, V, S> Iterator for Iter<'a, K, V, S> {
    type Item = (&'a K, &'a V);

    fn next(&mut self) -> Option<Self::Item> {
        self.iter.next().map(|(k, v)| (k, v))
    }
}

/// An iterator over the keys of a [`MemoMap`].
///
/// This struct is created by the [`keys`](MemoMap::keys) method on [`MemoMap`].
/// See its documentation for more information.
pub struct Keys<'a, K, V, S> {
    iter: Iter<'a, K, V, S>,
}

impl<'a, K, V, S> Iterator for Keys<'a, K, V, S> {
    type Item = &'a K;

    fn next(&mut self) -> Option<Self::Item> {
        self.iter.next().map(|(k, _)| k)
    }
}

#[test]
fn test_insert() {
    let memo = MemoMap::new();
    assert!(memo.insert(23u32, Box::new(1u32)));
    assert!(!memo.insert(23u32, Box::new(2u32)));
    assert_eq!(memo.get(&23u32).cloned(), Some(Box::new(1)));
}

#[test]
fn test_iter() {
    let memo = MemoMap::new();
    memo.insert(1, "one");
    memo.insert(2, "two");
    memo.insert(3, "three");
    let mut values = memo.iter().map(|(k, v)| (*k, *v)).collect::<Vec<_>>();
    values.sort();
    assert_eq!(values, vec![(1, "one"), (2, "two"), (3, "three")]);
}

#[test]
fn test_keys() {
    let memo = MemoMap::new();
    memo.insert(1, "one");
    memo.insert(2, "two");
    memo.insert(3, "three");
    let mut values = memo.keys().map(|k| *k).collect::<Vec<_>>();
    values.sort();
    assert_eq!(values, vec![1, 2, 3]);
}

#[test]
fn test_contains() {
    let memo = MemoMap::new();
    memo.insert(1, "one");
    assert!(memo.contains_key(&1));
    assert!(!memo.contains_key(&2));
}
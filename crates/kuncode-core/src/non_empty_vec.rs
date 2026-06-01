//! A vector type that is statically guaranteed to contain at least one element.

use std::ops::Deref;

use serde::{Deserialize, Serialize};
use thiserror::Error;

/// Returned when an empty [`Vec`] is supplied where a [`NonEmptyVec`] is required.
#[derive(Clone, Debug, PartialEq, Eq, Error)]
#[error("Cannot create NonEmptyVec with an empty vector.")]
pub struct EmptyVecError;

/// A [`Vec`] whose length is guaranteed to be at least one.
///
/// The non-empty invariant lets callers index the first element or treat the
/// collection as a `(head, tail)` pair without runtime checks.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NonEmptyVec<T: Clone> {
    inner: Vec<T>,
}

impl<T: Clone> NonEmptyVec<T> {
    /// Creates a new vector containing a single element.
    pub fn new(first: T) -> Self {
        Self { inner: vec![first] }
    }

    /// Builds a vector from an explicit head element followed by `rest`.
    ///
    /// Useful when destructuring an existing collection into its first item
    /// and the remainder.
    pub fn from_first_rest(first: T, rest: Vec<T>) -> Self {
        let mut inner = Vec::with_capacity(rest.len() + 1);
        inner.push(first);
        inner.extend(rest);

        Self { inner }
    }

    /// Appends an element to the back of the vector.
    pub fn push(&mut self, value: T) {
        self.inner.push(value);
    }

    /// Returns an iterator over the elements.
    pub fn iter(&self) -> std::slice::Iter<'_, T> {
        self.inner.iter()
    }

    /// Consumes `self` and yields the underlying [`Vec`], dropping the
    /// non-empty invariant.
    pub fn into_vec(self) -> Vec<T> {
        self.inner
    }

    /// Returns the first element.
    pub fn first(&self) -> &T {
        // Safe by construction: every constructor and deserializer preserves
        // the non-empty invariant.
        self.inner.first().unwrap()
    }
}

impl<T: Clone> Deref for NonEmptyVec<T> {
    type Target = [T];

    fn deref(&self) -> &Self::Target {
        &self.inner
    }
}

impl<T: Clone> TryFrom<Vec<T>> for NonEmptyVec<T> {
    type Error = EmptyVecError;

    /// Returns [`EmptyVecError`] if `value` is empty; otherwise wraps it
    /// without copying.
    fn try_from(value: Vec<T>) -> Result<Self, Self::Error> {
        if value.is_empty() {
            Err(EmptyVecError)
        } else {
            Ok(Self { inner: value })
        }
    }
}

impl<T: Clone> From<T> for NonEmptyVec<T> {
    fn from(value: T) -> Self {
        Self::new(value)
    }
}

impl<T: Clone> IntoIterator for NonEmptyVec<T> {
    type Item = T;
    type IntoIter = std::vec::IntoIter<T>;
    fn into_iter(self) -> Self::IntoIter {
        self.inner.into_iter()
    }
}

impl<T> Serialize for NonEmptyVec<T>
where
    T: Serialize + Clone,
{
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        self.inner.serialize(serializer)
    }
}

impl<'de, T> Deserialize<'de> for NonEmptyVec<T>
where
    T: Deserialize<'de> + Clone,
{
    /// Deserializes a sequence and rejects empty input with a custom error so
    /// the non-empty invariant is enforced at the data boundary.
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let vec = Vec::<T>::deserialize(deserializer)?;
        NonEmptyVec::try_from(vec)
            .map_err(|e: EmptyVecError| serde::de::Error::custom(e.to_string()))
    }
}

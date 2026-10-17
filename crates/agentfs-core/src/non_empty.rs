//! A list with at least one element, by construction (F8 S3).
//!
//! `config::Remotes::Pushed` holds one, so "pushed, with no remote" cannot
//! be expressed. Bespoke rather than the `nonempty` or `vec1` crates: the
//! one consumer needs construction from a `Vec`, a guaranteed first
//! element and slice access, which this gives in a few lines without a
//! new dependency (`ci/check-deps.sh`).

use std::ops::Deref;

/// A `Vec<T>` that is never empty. Read it as a slice (`Deref`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NonEmpty<T>(Vec<T>);

impl<T> NonEmpty<T> {
    /// `None` when `items` is empty.
    pub fn new(items: Vec<T>) -> Option<Self> {
        let _ = items;
        todo!("F8 S3")
    }

    /// The first element, which always exists.
    pub fn first(&self) -> &T {
        todo!("F8 S3")
    }

    pub fn into_vec(self) -> Vec<T> {
        todo!("F8 S3")
    }
}

impl<T> Deref for NonEmpty<T> {
    type Target = [T];

    fn deref(&self) -> &[T] {
        todo!("F8 S3")
    }
}

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
        (!items.is_empty()).then_some(Self(items))
    }

    /// A list of `item` alone.
    pub fn one(item: T) -> Self {
        Self(vec![item])
    }

    /// The first element, which always exists.
    pub fn first(&self) -> &T {
        // Invariant: never empty (`new` is the only constructor).
        &self.0[0]
    }

    pub fn into_vec(self) -> Vec<T> {
        self.0
    }

    /// The elements, to change in place: the length cannot change.
    pub fn as_mut_slice(&mut self) -> &mut [T] {
        &mut self.0
    }
}

impl<T> Deref for NonEmpty<T> {
    type Target = [T];

    fn deref(&self) -> &[T] {
        &self.0
    }
}

#[cfg(test)]
mod tests {
    use super::NonEmpty;

    #[test]
    fn an_empty_list_is_refused_and_any_other_kept_whole() {
        assert_eq!(NonEmpty::<u8>::new(Vec::new()), None);
        let list = NonEmpty::new(vec![3, 1, 2]).unwrap();
        assert_eq!(*list.first(), 3);
        assert_eq!(&*list, &[3, 1, 2]);
        assert_eq!(list.into_vec(), vec![3, 1, 2]);
    }
}

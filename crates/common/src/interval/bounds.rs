//! Subset of `std::ops::Bound` specialized for our restricted forms of
//! intervals.
use value::heap_size::HeapSize;

use super::key::BinaryKey;

#[derive(Clone, Debug, Eq, PartialEq, Ord, PartialOrd)]
#[cfg_attr(any(test, feature = "testing"), derive(proptest_derive::Arbitrary))]
pub enum Start {
    Included(BinaryKey),
}

impl HeapSize for Start {
    fn heap_size(&self) -> usize {
        match self {
            Start::Included(k) => k.heap_size(),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Ord, PartialOrd)]
#[cfg_attr(any(test, feature = "testing"), derive(proptest_derive::Arbitrary))]
pub enum End {
    Excluded(BinaryKey),
    Unbounded,
}

impl End {
    pub fn after_prefix(key: &BinaryKey) -> Self {
        match key.increment() {
            None => Self::Unbounded,
            Some(key) => Self::Excluded(key),
        }
    }

    /// Is the interval `(-inf, end)` disjoint with `[start, +inf)`?
    pub fn is_disjoint(&self, start: &Start) -> bool {
        match (self, start) {
            (End::Unbounded, _) => false,
            (End::Excluded(ref s), Start::Included(ref t)) => s <= t,
        }
    }

    pub fn is_adjacent(&self, start: &Start) -> bool {
        match (self, start) {
            (End::Unbounded, _) => false,
            (End::Excluded(ref s), Start::Included(ref t)) => s[..].eq(&t[..]),
        }
    }
}

impl HeapSize for End {
    fn heap_size(&self) -> usize {
        match self {
            End::Excluded(k) => k.heap_size(),
            End::Unbounded => 0,
        }
    }
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;

    use super::{
        super::key::BinaryKey,
        End,
    };

    proptest! {
        #![proptest_config(
            ProptestConfig { failure_persistence: None, ..ProptestConfig::default() }
        )]

        #[test]
        fn test_end_ordering(key in any::<BinaryKey>()) {
            assert!(End::Excluded(key) < End::Unbounded);
        }
    }
}

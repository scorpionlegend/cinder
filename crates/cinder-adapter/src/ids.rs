//! Opaque, never-reused identifiers issued by the adapter.
//!
//! Handles are IDs, not pointers (spec §6). Only [`IdAllocator`] mints them, so
//! a duplicate or recycled ID cannot be constructed by product code.

use std::fmt;
use std::sync::atomic::{AtomicU64, Ordering};

macro_rules! id_type {
    ($name:ident, $prefix:literal) => {
        #[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
        pub struct $name(u64);

        impl $name {
            /// Raw value, for logging and persistence only.
            pub fn get(self) -> u64 {
                self.0
            }

            pub(crate) fn from_raw(raw: u64) -> Self {
                Self(raw)
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                write!(f, concat!($prefix, "{}"), self.0)
            }
        }

        impl fmt::Debug for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                fmt::Display::fmt(self, f)
            }
        }
    };
}

id_type!(ViewId, "view-");
id_type!(ProfileId, "profile-");
id_type!(DecisionId, "decision-");

/// Monotonic per-view event sequence number, used to check ordering.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub struct SeqNo(u64);

impl SeqNo {
    pub fn get(self) -> u64 {
        self.0
    }

    pub fn next(self) -> Self {
        Self(self.0 + 1)
    }
}

impl fmt::Display for SeqNo {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "seq-{}", self.0)
    }
}

/// Issues every adapter ID. One allocator per engine process.
#[derive(Debug)]
pub struct IdAllocator {
    next_view: AtomicU64,
    next_profile: AtomicU64,
    next_decision: AtomicU64,
}

impl IdAllocator {
    pub fn new() -> Self {
        Self {
            next_view: AtomicU64::new(1),
            next_profile: AtomicU64::new(1),
            next_decision: AtomicU64::new(1),
        }
    }

    pub fn next_view(&self) -> ViewId {
        ViewId::from_raw(self.next_view.fetch_add(1, Ordering::Relaxed))
    }

    pub fn next_profile(&self) -> ProfileId {
        ProfileId::from_raw(self.next_profile.fetch_add(1, Ordering::Relaxed))
    }

    pub fn next_decision(&self) -> DecisionId {
        DecisionId::from_raw(self.next_decision.fetch_add(1, Ordering::Relaxed))
    }
}

impl Default for IdAllocator {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_are_unique_and_never_reused() {
        let alloc = IdAllocator::new();
        let a = alloc.next_view();
        let b = alloc.next_view();
        assert_ne!(a, b);
        assert!(b.get() > a.get());
        assert_eq!(a.to_string(), "view-1");
        assert_eq!(alloc.next_profile().to_string(), "profile-1");
        assert_eq!(alloc.next_decision().to_string(), "decision-1");
    }

    #[test]
    fn sequence_numbers_advance() {
        let s = SeqNo::default();
        assert_eq!(s.get(), 0);
        assert_eq!(s.next().next().get(), 2);
    }
}

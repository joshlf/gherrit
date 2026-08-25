use std::{fmt, num::NonZeroU64};

/// A one-based publication-history position.
///
/// Remote ref names are untrusted text, but the rest of pre-push never needs
/// to represent `v0`. Converting at that boundary keeps every planned,
/// rendered, and published version valid by construction.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub(super) struct Version(NonZeroU64);

impl Version {
    pub(super) const FIRST: Self = Self(NonZeroU64::MIN);

    pub(super) fn new(value: u64) -> Option<Self> {
        NonZeroU64::new(value).map(Self)
    }

    /// Converts a zero-based history position to its one-based version.
    pub(super) fn from_history_index(index: usize) -> Option<Self> {
        u64::try_from(index).ok()?.checked_add(1).and_then(Self::new)
    }

    pub(super) fn next(self) -> Option<Self> {
        self.get().checked_add(1).and_then(Self::new)
    }

    pub(super) fn get(self) -> u64 {
        self.0.get()
    }
}

impl fmt::Display for Version {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(formatter)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn versions_are_nonzero_and_history_positions_are_one_based() {
        assert_eq!(Version::new(0), None);
        assert_eq!(Version::from_history_index(0), Some(Version::FIRST));
        assert_eq!(Version::from_history_index(1).unwrap().get(), 2);
        assert_eq!(Version::new(u64::MAX).unwrap().next(), None);
    }
}

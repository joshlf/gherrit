use std::num::NonZeroU32;

use eyre::{Result, eyre};
/// A positive pull request number in GitHub's GraphQL `Int` range.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub(in crate::pre_push::publication_attempt) struct PullRequestNumber(NonZeroU32);

impl PullRequestNumber {
    pub(in crate::pre_push::publication_attempt) const MAX: Self =
        Self(NonZeroU32::new(i32::MAX as u32).expect("GraphQL Int maximum is nonzero"));

    pub(in crate::pre_push) fn new(value: u64) -> Result<Self> {
        let value = u32::try_from(value)
            .ok()
            .and_then(NonZeroU32::new)
            .filter(|value| value.get() <= i32::MAX as u32)
            .ok_or_else(|| eyre!("GitHub reported invalid pull request number {value}"))?;
        Ok(Self(value))
    }

    pub(in crate::pre_push::publication_attempt) fn get(self) -> u32 {
        self.0.get()
    }

    #[cfg(test)]
    pub(in crate::pre_push::publication_attempt) fn for_test(value: u32) -> Self {
        Self::new(u64::from(value)).expect("valid test pull request number")
    }
}

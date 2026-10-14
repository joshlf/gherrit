//! Owned-base publication codecs; runtime activation follows separately.
mod github;
mod marker;
mod version;
mod version_object;

mod history;
mod remote;

mod body;
mod failure;
mod refs;

use eyre::{Result, bail};
use gix::ObjectId;

use super::destination::{DefaultBranch, ObservedPublicBranch, RemoteBranchState};
use crate::manage::PublicBranchName;
/// A public branch checked against the repository default observed by the
/// same publication attempt.
#[derive(Clone, Debug, Eq, PartialEq)]
struct PublicBranch(PublicBranchName);

impl PublicBranch {
    fn new(name: PublicBranchName, default_branch: &DefaultBranch) -> Result<Self> {
        if ref_paths_conflict(name.as_str(), default_branch.name()) {
            bail!("A public GHerrit branch cannot conflict with the repository default branch");
        }
        Ok(Self(name))
    }

    fn as_str(&self) -> &str {
        self.0.as_str()
    }
}

/// A checked public branch paired with the exact state observed for it.
struct ObservedPublicProjection {
    branch: PublicBranch,
    state: RemoteBranchState,
}

impl ObservedPublicProjection {
    fn new(observed: ObservedPublicBranch, default_branch: &DefaultBranch) -> Result<Self> {
        let (name, state) = observed.into_parts();
        Ok(Self { branch: PublicBranch::new(name, default_branch)?, state })
    }

    fn into_parts(self) -> (PublicBranch, RemoteBranchState) {
        (self.branch, self.state)
    }

    fn requires_transition_to(&self, desired: ObjectId) -> bool {
        match self.state {
            RemoteBranchState::Absent => true,
            RemoteBranchState::At(current) => current != desired,
        }
    }
}

fn ref_paths_conflict(left: &str, right: &str) -> bool {
    left == right
        || left.strip_prefix(right).is_some_and(|suffix| suffix.starts_with('/'))
        || right.strip_prefix(left).is_some_and(|suffix| suffix.starts_with('/'))
}

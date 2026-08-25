#![cfg_attr(test, allow(dead_code, reason = "later commits activate the complete adapter"))]

//! GitHub boundary for exact-local observation and publication.
//!
//! The active publisher still uses [`super::legacy_github`]. This complete
//! boundary is production-compiled now and becomes reachable only when the
//! planner and publication state machine switch together.

use color_eyre::eyre::{Result, bail};

use super::destination::{DefaultBranch, RepositoryCoordinates};

mod json;
mod mutation;
mod observation;
mod pull_request;
mod transport;

#[cfg(test)]
pub(in crate::pre_push) use mutation::CompleteCreateReceipts;
pub(in crate::pre_push) use mutation::{
    CreatePullRequest, PreparedCreates, PreparedUpdates, UpdatePullRequest,
};
#[cfg(test)]
pub(in crate::pre_push) use observation::ObservedBase;
pub(in crate::pre_push) use observation::{
    AbsentPullRequest, BaseKind, CompleteLocalPullRequests, LocalPullRequestObservation,
    ManagedOpenPullRequest,
};
pub(in crate::pre_push) use pull_request::PullRequestIdentity;
#[allow(unused_imports, reason = "the exact planner activates in a later commit")]
pub(in crate::pre_push) use pull_request::PullRequestNumber;
#[allow(unused_imports, reason = "the exact adapter activates in a later atomic switch")]
pub(in crate::pre_push) use transport::Github;

/// One-use authority to preflight creates against the repository and identity
/// namespaces retained by an exact-local observation.
///
/// Keeping these values together prevents a planner from replacing the
/// observed repository node or starting create-receipt collision checks from
/// an unrelated empty registry.
pub(in crate::pre_push) struct CreatePreparation {
    repository_id: RepositoryNodeId,
    identities: pull_request::PullRequestIdentityRegistry,
}

impl CreatePreparation {
    fn new(
        repository_id: RepositoryNodeId,
        identities: pull_request::PullRequestIdentityRegistry,
    ) -> Self {
        Self { repository_id, identities }
    }

    pub(in crate::pre_push) fn prepare(
        self,
        operations: Vec<CreatePullRequest>,
    ) -> Result<PreparedCreates> {
        PreparedCreates::new(self.repository_id, operations, self.identities)
    }
}

/// A nonempty GitHub node ID for one repository.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub(super) struct RepositoryNodeId(Box<str>);

impl RepositoryNodeId {
    fn new(value: String) -> Result<Self> {
        if value.is_empty() {
            bail!("GitHub reported an empty repository node ID");
        }
        Ok(Self(value.into_boxed_str()))
    }

    pub(super) fn as_str(&self) -> &str {
        &self.0
    }
}

/// Repository facts retained once for one complete exact-local observation.
#[derive(Debug, Eq, PartialEq)]
pub(super) struct Repository {
    coordinates: RepositoryCoordinates,
    node_id: RepositoryNodeId,
    default_branch: DefaultBranch,
}

impl Repository {
    pub(super) fn coordinates(&self) -> &RepositoryCoordinates {
        &self.coordinates
    }

    pub(super) fn node_id(&self) -> &RepositoryNodeId {
        &self.node_id
    }

    pub(super) fn default_branch(&self) -> &DefaultBranch {
        &self.default_branch
    }

    fn into_create_parts(self) -> (RepositoryNodeId, DefaultBranch) {
        (self.node_id, self.default_branch)
    }

    #[cfg(test)]
    fn for_plan_test_with_node(
        coordinates: RepositoryCoordinates,
        default_branch: DefaultBranch,
        node_id: &str,
    ) -> Self {
        Self {
            coordinates,
            node_id: RepositoryNodeId::new(node_id.to_owned())
                .expect("the plan-test repository node ID is valid"),
            default_branch,
        }
    }
}

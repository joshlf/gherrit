//! GitHub boundary for exact-local observation and publication.
//!
//! Observation consumes the client which performs it. The resulting value
//! keeps that client inseparable from its evidence until the publication plan
//! retains the same client for every later mutation.

use color_eyre::eyre::{Result, bail};

use crate::pre_push::destination::{DefaultBranch, RepositoryCoordinates};

/// Maximum number of mutation fields sent in one GraphQL request.
pub(in crate::pre_push::publication_attempt) const MAX_MUTATION_ALIASES: usize = 64;

// A 131,072-byte pull-request body made entirely from U+0001 expands to
// 917,504 bytes after GraphQL-string escaping and then outer-JSON escaping.
// One MiB accommodates that worst case plus the mutation's other supported
// fields while retaining a deterministic preflight request limit.
pub(in crate::pre_push::publication_attempt) const MAX_MUTATION_REQUEST_BYTES: usize = 1024 * 1024;

mod mutation;
mod observation;
mod pull_request;
mod transport;

pub(super) use mutation::{
    ClosePullRequest, CompleteCreateReceipts, CreatePullRequest, PreparedCreates,
    PreparedPullRequestProjection, UpdatePullRequest,
};
#[cfg(test)]
pub(in crate::pre_push::publication_attempt) use mutation::{
    TestClose, TestCreate, TestPullRequestProjection, TestUpdate,
};
#[cfg(test)]
pub(super) use observation::ObservedBase;
pub(super) use observation::{
    AbsentPullRequest, BaseKind, CompleteLocalPullRequests, LocalPullRequestObservation,
    ManagedOpenPullRequestCandidate, ManagedOpenPullRequests,
};
pub(super) use pull_request::{PullRequestIdentity, PullRequestNumber};
pub(super) use transport::Github;

/// Complete GitHub evidence and the exact client which produced it.
///
/// This pair has no independent production accessors. Planning consumes it
/// and retains this same client for every create and final projection.
pub(super) struct ObservedGithub {
    github: Github,
    pull_requests: CompleteLocalPullRequests,
}

#[cfg(test)]
impl std::fmt::Debug for ObservedGithub {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("ObservedGithub(..)")
    }
}

impl ObservedGithub {
    fn new(github: Github, pull_requests: CompleteLocalPullRequests) -> Self {
        Self { github, pull_requests }
    }

    pub(super) fn into_parts(self) -> (Github, CompleteLocalPullRequests) {
        (self.github, self.pull_requests)
    }

    #[cfg(test)]
    pub(super) fn for_plan_test(github: Github, pull_requests: CompleteLocalPullRequests) -> Self {
        Self::new(github, pull_requests)
    }
}

/// One-use authority to preflight creates against the repository and identity
/// namespaces retained by an exact-local observation.
///
/// Keeping these values together prevents a planner from replacing the
/// observed repository node or starting create-receipt collision checks from
/// an unrelated empty registry.
pub(super) struct CreatePreparation {
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

    pub(super) fn prepare(self, operations: Vec<CreatePullRequest>) -> Result<PreparedCreates> {
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

    #[cfg(test)]
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

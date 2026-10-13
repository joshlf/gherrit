#![cfg_attr(test, allow(dead_code, reason = "later commits activate the complete adapter"))]

//! GitHub boundary for exact-local observation and publication.
//!
//! The active publisher still uses [`super::legacy_github`]. This complete
//! boundary is production-compiled now and becomes reachable only when the
//! planner and publication state machine switch together.

use color_eyre::eyre::{Result, bail};

use super::destination::{DefaultBranch, RepositoryCoordinates};

/// Maximum number of mutation fields sent in one GraphQL request.
pub(super) const MAX_MUTATION_ALIASES: usize = 64;

// A 131,072-byte pull-request body made entirely from U+0001 expands to
// 917,504 bytes after GraphQL-string escaping and then outer-JSON escaping.
// One MiB accommodates that worst case plus the mutation's other supported
// fields while retaining a deterministic preflight request limit.
pub(super) const MAX_MUTATION_REQUEST_BYTES: usize = 1024 * 1024;

mod mutation;
mod observation;
mod pull_request;
mod transport;

pub(in crate::pre_push) use pull_request::PullRequestIdentity;
#[allow(unused_imports, reason = "the exact planner activates in a later commit")]
pub(in crate::pre_push) use pull_request::PullRequestNumber;
#[allow(unused_imports, reason = "the exact adapter activates in a later atomic switch")]
pub(in crate::pre_push) use transport::Github;

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
}

//! GitHub evidence used by the exact-local publication model.
//!
//! The active publisher still uses [`super::legacy_github`]. This module is
//! compiled by its adapter tests while the exact-local model is introduced in
//! reviewable slices.

use color_eyre::eyre::{Result, bail, eyre};
use serde::Deserialize;

use super::destination::{DefaultBranch, RepositoryCoordinates};

mod observation;

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

/// GitHub's coupled numeric and GraphQL identities for one pull request.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub(super) struct PullRequestIdentity {
    pub(super) number: u64,
    pub(super) node_id: Box<str>,
}

impl PullRequestIdentity {
    pub(super) fn new(number: u64, node_id: String) -> Result<Self> {
        let number = (1..=i32::MAX as u64)
            .contains(&number)
            .then_some(number)
            .ok_or_else(|| eyre!("GitHub reported an invalid pull request number {number}"))?;
        if node_id.is_empty() {
            bail!("GitHub reported an empty pull request node ID");
        }
        Ok(Self { number, node_id: node_id.into_boxed_str() })
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub(super) enum PullRequestState {
    Open,
    Closed,
    Merged,
}

/// The authority and projection fields retained for one same-repository row.
///
/// Cross-repository rows never become this type. Their wire shape, identity,
/// and requested head are validated before their remaining payload is
/// discarded.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct SameRepositoryPullRequest {
    pub(super) identity: PullRequestIdentity,
    pub(super) title: String,
    pub(super) body: String,
    pub(super) base_branch: String,
    pub(super) base_oid: gix::ObjectId,
    pub(super) head_oid: gix::ObjectId,
    pub(super) state: PullRequestState,
    pub(super) is_draft: bool,
    pub(super) has_auto_merge_request: bool,
    pub(super) is_in_merge_queue: bool,
}

/// Repository facts retained once for one complete exact-local observation.
#[derive(Debug, Eq, PartialEq)]
pub(super) struct Repository {
    pub(super) coordinates: RepositoryCoordinates,
    pub(super) node_id: RepositoryNodeId,
    pub(super) default_branch: DefaultBranch,
}

//! GitHub evidence used by the exact-local publication model.
//!
//! The active publisher still uses [`super::legacy_github`]. This module is
//! compiled by its adapter tests while the exact-local model is introduced in
//! reviewable slices.

use color_eyre::eyre::{Result, bail};

use super::destination::{DefaultBranch, RepositoryCoordinates};

mod observation;
mod pull_request;

pub(in crate::pre_push) use pull_request::PullRequestIdentity;

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

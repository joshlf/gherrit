//! Creation authority for changes absent from exact all-state observation.
//!
//! The parent planner can only obtain a [`CreateAuthority`] by consuming one
//! sealed same-repository absence from a complete exact-local observation and
//! joining it to the same change's validated, markerless Git history. Keeping
//! the fields and constructor here prevents the rest of the planner from
//! manufacturing creation authority from an ordinary change ID.

use color_eyre::eyre::{Result, bail};
use gix::ObjectId;

use super::{MarkerTarget, marker_target};
use crate::pre_push::{
    body::BodyRecipeInput, history::ValidatedChangeHistory, local::GherritPrId,
    pull_request::AbsentPullRequest,
};

/// One validated local change authorized for pull-request creation.
pub(super) struct CreateAuthority {
    history: ValidatedChangeHistory,
    repository_id: String,
    marker: MarkerTarget,
}

impl CreateAuthority {
    pub(super) fn new(
        absence: AbsentPullRequest,
        history: ValidatedChangeHistory,
        repository_id: String,
    ) -> Result<Self> {
        if history.id() != absence.id() {
            bail!(
                "absent pull request identifies '{}', but history identifies '{}'",
                absence.id().as_str(),
                history.id().as_str()
            );
        }
        if history.pull_request_marker().is_some() {
            bail!(
                "GHerrit change '{}' has a pull-request marker but no same-repository pull request",
                absence.id().as_str()
            );
        }
        let marker = marker_target(&history);
        Ok(Self { history, repository_id, marker })
    }

    pub(super) fn history(&self) -> &ValidatedChangeHistory {
        &self.history
    }

    /// Moves the history into body derivation while retaining the sealed
    /// creation and pending-marker evidence needed after rendering.
    pub(super) fn into_body_and_projection(self) -> Result<(BodyRecipeInput, CreatePlanSeed)> {
        let Self { history, repository_id, marker } = self;
        let id = history.id().clone();
        let revision = history.projected_current().revision();
        let body = BodyRecipeInput::missing(id, history)?;
        Ok((
            body,
            CreatePlanSeed {
                repository_id,
                marker,
                head_oid: revision.head(),
                base_oid: revision.first_parent(),
            },
        ))
    }
}

/// Sealed creation evidence retained while provisional bodies are rendered.
pub(super) struct CreatePlanSeed {
    repository_id: String,
    marker: MarkerTarget,
    head_oid: ObjectId,
    base_oid: ObjectId,
}

impl CreatePlanSeed {
    pub(super) fn finish(
        self,
        title: String,
        provisional_body: String,
    ) -> (PlannedCreate, PendingCreatedMarker) {
        let id = self.marker.id.clone();
        (
            PlannedCreate {
                repository_id: self.repository_id,
                id,
                title,
                provisional_body,
                head_oid: self.head_oid,
                base_oid: self.base_oid,
            },
            PendingCreatedMarker { marker: self.marker },
        )
    }
}

/// A create specification constructible only from sealed absence evidence.
pub(in crate::pre_push) struct PlannedCreate {
    repository_id: String,
    id: GherritPrId,
    title: String,
    provisional_body: String,
    head_oid: ObjectId,
    base_oid: ObjectId,
}

impl PlannedCreate {
    pub(in crate::pre_push) fn into_parts(
        self,
    ) -> (String, GherritPrId, String, String, ObjectId, ObjectId) {
        (
            self.repository_id,
            self.id,
            self.title,
            self.provisional_body,
            self.head_oid,
            self.base_oid,
        )
    }
}

/// Marker authority whose pull-request identity is pending an exact receipt.
pub(super) struct PendingCreatedMarker {
    marker: MarkerTarget,
}

impl PendingCreatedMarker {
    pub(super) fn id(&self) -> &GherritPrId {
        &self.marker.id
    }

    pub(super) fn marker(&self) -> &MarkerTarget {
        &self.marker
    }
}

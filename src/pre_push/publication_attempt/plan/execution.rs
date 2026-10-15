//! Destination-bound production execution of the consuming plan.

use super::{
    super::refs::{MarkerTransition, prepare_marker_pushes},
    *,
};
use crate::util;

impl PublicationPlan {
    /// Executes the one fixed publication sequence without reobservation or
    /// retry. Every later effect remains inaccessible until its preceding
    /// durable acknowledgement has completed.
    pub(in crate::pre_push::publication_attempt) async fn execute(
        self,
        repository: &util::Repo,
    ) -> Result<()> {
        let Self { destination, github, effects } = self;
        if !destination.belongs_to(repository)? {
            bail!("Publication plan belongs to a different local repository");
        }
        effects
            .execute_with(&mut RemoteEffectDriver {
                repository,
                destination: &destination,
                github: &github,
            })
            .await
    }
}

/// The sole production effect driver, bound to the destination and GitHub
/// client retained by the complete publication plan.
struct RemoteEffectDriver<'attempt> {
    repository: &'attempt util::Repo,
    destination: &'attempt PushDestination,
    github: &'attempt Github,
}

impl EffectDriver for RemoteEffectDriver<'_> {
    async fn convert_pull_requests_to_draft(
        &mut self,
        conversions: PreparedDraftConversions,
    ) -> Result<()> {
        self.github.convert_pull_requests_to_draft(conversions).await
    }

    async fn publish_initial_refs(&mut self, pushes: PreparedPushes) -> Result<()> {
        pushes.execute(self.destination).await
    }

    async fn create_pull_requests(
        &mut self,
        creates: PreparedCreates,
    ) -> Result<CompleteCreateReceipts> {
        self.github.create_pull_requests(creates).await
    }

    async fn publish_markers(&mut self, markers: Box<[MarkerTemplate]>) -> Result<()> {
        let markers = markers
            .into_vec()
            .into_iter()
            .map(MarkerTemplate::prepare)
            .collect::<Result<Vec<_>>>()?;
        let transitions = markers.iter().map(MarkerTransition::from_prepared).collect::<Vec<_>>();
        let pushes = prepare_marker_pushes(self.destination, &transitions)?;
        for marker in markers {
            marker.materialize(self.repository)?;
        }
        pushes.execute(self.destination).await
    }

    async fn project_pull_requests(
        &mut self,
        projection: PreparedPullRequestProjection,
    ) -> Result<()> {
        self.github.project_pull_requests(projection).await
    }
}

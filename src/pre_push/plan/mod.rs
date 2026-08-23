//! Owned-base publication planning and staged execution.
//!
//! This module consumes complete correlated evidence and freezes one complete
//! staged lifecycle before exposing its first action. Wire data is preflighted
//! before writes except for final updates whose opaque identities and numbered
//! bodies cannot exist until GitHub acknowledges pull request creation.

use std::{borrow::Cow, collections::HashSet};

use color_eyre::eyre::{Result, bail};
use gix::ObjectId;

use super::{
    body::{
        BodyLinkContext, BodyRecipeInput, FinalBodyRecipes, GeneratedBody, RenderedBody,
        StackBodyRecipes,
    },
    destination::DefaultBranch,
    github::{
        CompleteCreateReceipts, CorrelatedRepository, Github, PreparedCreates, PreparedUpdates,
        UpdatePreflight, preflight_updates, prepare_creates, prepare_updates,
    },
    history::{CommitGraphEvidence, NormalizedPublishedHistory, ValidatedChangeHistory},
    local::{GherritPrId, LocalStack},
    publication::{
        MarkerPushPreflight, TuplePushPreflight, preflight_marker_pushes, preflight_tuple_pushes,
    },
    pull_request::{
        BaseKind, ExactLocalPullRequestIdentities, LocalPullRequestObservation, ManagedOpenParts,
        ManagedOpenPullRequest, PullRequestIdentity,
    },
    remote::ActiveRemoteChanges,
};

mod missing_open;

pub(super) use missing_open::PlannedCreate;

/// One exact marker destination and target retained behind typed evidence.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct MarkerTarget {
    id: GherritPrId,
    target: ObjectId,
}

impl MarkerTarget {
    pub(super) fn id(&self) -> &GherritPrId {
        &self.id
    }

    pub(super) fn target(&self) -> ObjectId {
        self.target
    }

    #[cfg(test)]
    pub(super) fn for_test(id: GherritPrId, target: ObjectId) -> Self {
        Self { id, target }
    }
}

/// Executable marker authority from a validated local markerless OPEN PR.
struct ObservedMarkerAuthorization {
    marker: MarkerTarget,
}

/// An update specification which only the planner can construct.
pub(super) struct PlannedUpdate {
    identity: PullRequestIdentity,
    title: Option<String>,
    body: Option<String>,
    base_branch: Option<String>,
}

impl PlannedUpdate {
    fn new(
        identity: PullRequestIdentity,
        title: Option<String>,
        body: Option<String>,
        base_branch: Option<String>,
    ) -> Result<Self> {
        if title.is_none() && body.is_none() && base_branch.is_none() {
            bail!("A planned pull request update must change at least one field");
        }
        Ok(Self { identity, title, body, base_branch })
    }

    pub(super) fn into_parts(
        self,
    ) -> (PullRequestIdentity, Option<String>, Option<String>, Option<String>) {
        (self.identity, self.title, self.body, self.base_branch)
    }
}

/// Planner-owned authority to execute fully preflighted tuple pushes.
pub(super) struct AuthorizedTuplePushes<'destination>(TuplePushPreflight<'destination>);

impl<'destination> AuthorizedTuplePushes<'destination> {
    fn new(preflight: TuplePushPreflight<'destination>) -> Self {
        Self(preflight)
    }

    pub(super) fn into_preflight(self) -> TuplePushPreflight<'destination> {
        self.0
    }

    #[cfg(test)]
    fn effect_batches_for_test(
        &self,
    ) -> super::test_effect::EffectBatches<super::test_effect::TupleEffect> {
        self.0
            .effect_batches_for_test()
            .into_vec()
            .into_iter()
            .map(|batch| {
                batch
                    .into_vec()
                    .into_iter()
                    .map(|effect| match effect {
                        super::test_effect::GitEffect::Tuple(effect) => effect,
                        super::test_effect::GitEffect::Marker(_) => {
                            panic!("the tuple barrier contains a marker effect")
                        }
                    })
                    .collect()
            })
            .collect()
    }
}

/// Planner-owned authority to execute fully preflighted marker pushes.
pub(super) struct AuthorizedMarkerPushes<'destination>(MarkerPushPreflight<'destination>);

impl<'destination> AuthorizedMarkerPushes<'destination> {
    fn new(preflight: MarkerPushPreflight<'destination>) -> Self {
        Self(preflight)
    }

    pub(super) fn into_preflight(self) -> MarkerPushPreflight<'destination> {
        self.0
    }

    #[cfg(test)]
    fn effect_batches_for_test(
        &self,
    ) -> super::test_effect::EffectBatches<super::test_effect::GitEffect> {
        self.0.effect_batches_for_test()
    }
}

/// One complete plan whose lifecycle remains private to this module.
pub(super) struct PublicationPlan<'destination> {
    tuples: Option<AuthorizedTuplePushes<'destination>>,
    after_tuples: AfterTuples<'destination>,
}

impl<'destination> PublicationPlan<'destination> {
    fn new(
        tuples: Option<AuthorizedTuplePushes<'destination>>,
        after_tuples: AfterTuples<'destination>,
    ) -> Self {
        Self { tuples, after_tuples }
    }

    /// Executes the sole reachable lifecycle, consuming each authority once.
    pub(super) async fn execute(self, github: &Github) -> Result<()> {
        if let Some(tuples) = self.tuples {
            super::publication::publish_tuples(tuples).await?;
        }
        let final_projection = match self.after_tuples {
            AfterTuples::Final(final_projection) => final_projection,
            AfterTuples::Markers(markers) => markers.publish().await?,
            AfterTuples::Creates(stage) => {
                let CreateStage { creates, projection } = *stage;
                let count = creates.operation_count();
                let plural = if count == 1 { "" } else { "s" };
                log::info!("Creating {count} PR{plural}...");
                let receipts = github.create_pull_requests(creates).await.into_result()?;
                log::info!("Created {count} PR{plural}.");
                for (_, identity) in receipts.iter() {
                    log::info!(
                        "Created PR #{}: {}",
                        identity.number().get(),
                        github.pull_request_url(identity.number().get())
                    );
                }
                projection.complete(receipts)?.publish().await?
            }
        };
        final_projection.apply(github).await
    }

    #[cfg(test)]
    fn into_create_stage_for_test(self) -> (PreparedCreates, ProjectionSeed<'destination>) {
        match (self.tuples, self.after_tuples) {
            (None, AfterTuples::Creates(stage)) => {
                let CreateStage { creates, projection } = *stage;
                (creates, projection)
            }
            (Some(_), _) => panic!("the test expected no tuple publication"),
            (None, _) => panic!("the test expected a create stage"),
        }
    }

    /// Returns the first semantic action without inspecting transport bytes.
    #[cfg(test)]
    pub(super) fn first_stage_for_test(&self) -> super::test_effect::Stage {
        if let Some(pushes) = &self.tuples {
            return super::test_effect::Stage::Tuples(pushes.effect_batches_for_test());
        }
        match &self.after_tuples {
            AfterTuples::Final(final_projection) => final_projection.stage_for_test(),
            AfterTuples::Markers(markers) => {
                super::test_effect::Stage::Markers(markers.effect_batches_for_test())
            }
            AfterTuples::Creates(stage) => {
                super::test_effect::Stage::Creates(stage.creates.effect_batches_for_test())
            }
        }
    }
}

enum AfterTuples<'destination> {
    Final(FinalProjection),
    Markers(MarkerStage<'destination>),
    Creates(Box<CreateStage<'destination>>),
}

#[derive(Debug)]
enum FinalProjection {
    NoAction,
    Updates(PreparedUpdates),
}

impl FinalProjection {
    fn from_operations(operations: Vec<PlannedUpdate>) -> Result<Self> {
        if operations.is_empty() {
            Ok(Self::NoAction)
        } else {
            Ok(Self::Updates(prepare_updates(operations.into_boxed_slice())?))
        }
    }

    async fn apply(self, github: &Github) -> Result<()> {
        match self {
            Self::NoAction => Ok(()),
            Self::Updates(updates) => {
                let count = updates.operation_count();
                for identity in updates.identities() {
                    log::info!(
                        "Queued update for PR #{}: {}",
                        identity.number().get(),
                        github.pull_request_url(identity.number().get())
                    );
                }
                let plural = if count == 1 { "" } else { "s" };
                log::info!("Updating {count} PR{plural}...");
                github.update_pull_requests(updates).await.into_result()?;
                log::info!("Updated {count} PR{plural}.");
                Ok(())
            }
        }
    }

    #[cfg(test)]
    fn stage_for_test(&self) -> super::test_effect::Stage {
        match self {
            Self::NoAction => super::test_effect::Stage::Done,
            Self::Updates(updates) => {
                super::test_effect::Stage::Updates(updates.effect_batches_for_test())
            }
        }
    }
}

struct MarkerStage<'destination> {
    pushes: AuthorizedMarkerPushes<'destination>,
    final_projection: FinalProjection,
}

#[cfg(test)]
impl std::fmt::Debug for MarkerStage<'_> {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.debug_struct("MarkerStage").finish_non_exhaustive()
    }
}

impl<'destination> MarkerStage<'destination> {
    fn new(
        pushes: AuthorizedMarkerPushes<'destination>,
        final_projection: FinalProjection,
    ) -> Self {
        Self { pushes, final_projection }
    }

    async fn publish(self) -> Result<FinalProjection> {
        super::publication::publish_markers(self.pushes).await?;
        Ok(self.final_projection)
    }

    #[cfg(test)]
    fn effect_batches_for_test(
        &self,
    ) -> super::test_effect::EffectBatches<super::test_effect::MarkerEffect> {
        self.pushes
            .effect_batches_for_test()
            .into_vec()
            .into_iter()
            .map(|batch| {
                batch
                    .into_vec()
                    .into_iter()
                    .map(|effect| match effect {
                        super::test_effect::GitEffect::Marker(effect) => effect,
                        super::test_effect::GitEffect::Tuple(_) => {
                            panic!("the marker barrier contains a tuple effect")
                        }
                    })
                    .collect()
            })
            .collect()
    }
}

struct CreateStage<'destination> {
    creates: PreparedCreates,
    projection: ProjectionSeed<'destination>,
}

impl<'destination> CreateStage<'destination> {
    fn new(creates: PreparedCreates, projection: ProjectionSeed<'destination>) -> Result<Self> {
        let prepared_ids = creates.planned_ids();
        let projection_ids = projection.created_ids();
        let pending_marker_ids = projection.markers.pending_created_ids();
        validate_create_stage_ids(&prepared_ids, &projection_ids, &pending_marker_ids)?;
        Ok(Self { creates, projection })
    }
}

/// Proves the three independently-derived create sequences are one exact,
/// nonempty set before any action is made executable.
fn validate_create_stage_ids(
    planned: &[GherritPrId],
    projected: &[GherritPrId],
    pending_markers: &[GherritPrId],
) -> Result<()> {
    if planned.is_empty() {
        bail!("a create stage requires at least one planned create");
    }
    let mut unique = HashSet::with_capacity(planned.len());
    if !planned.iter().all(|id| unique.insert(id)) {
        bail!("a create stage cannot repeat a planned change");
    }
    if planned != projected {
        bail!("planned creates and projection entries do not have the same exact order");
    }
    if planned != pending_markers {
        bail!("planned creates and pending marker evidence do not have the same exact order");
    }
    Ok(())
}

enum MarkerEvidence {
    Observed(ObservedMarkerAuthorization),
    Pending(missing_open::PendingCreatedMarker),
}

impl MarkerEvidence {
    fn marker(&self) -> &MarkerTarget {
        match self {
            Self::Observed(authorization) => &authorization.marker,
            Self::Pending(authorization) => authorization.marker(),
        }
    }
}

struct PendingMarkerGate<'destination> {
    preflight: MarkerPushPreflight<'destination>,
    evidence: Box<[MarkerEvidence]>,
}

impl<'destination> PendingMarkerGate<'destination> {
    fn new(
        preflight: MarkerPushPreflight<'destination>,
        evidence: Vec<MarkerEvidence>,
    ) -> Result<Self> {
        if evidence.is_empty() {
            bail!("a pending marker gate requires at least one authorization");
        }
        let targets = evidence.iter().map(MarkerEvidence::marker).cloned().collect::<Vec<_>>();
        if !preflight.matches_targets(&targets) {
            bail!("marker preflight does not match the exact ordered authorization set");
        }
        Ok(Self { preflight, evidence: evidence.into_boxed_slice() })
    }

    fn pending_created_ids(&self) -> Box<[GherritPrId]> {
        self.evidence
            .iter()
            .filter_map(|evidence| match evidence {
                MarkerEvidence::Observed(_) => None,
                MarkerEvidence::Pending(authorization) => Some(authorization.id().clone()),
            })
            .collect()
    }

    /// Consumes a complete set whose authority came entirely from observed
    /// markerless OPEN pull requests.
    fn authorize_observed(self) -> Result<AuthorizedMarkerPushes<'destination>> {
        if self.evidence.iter().any(|evidence| matches!(evidence, MarkerEvidence::Pending(_))) {
            bail!("observed marker authorization cannot contain a pending created marker");
        }
        let _evidence = self.evidence;
        Ok(AuthorizedMarkerPushes::new(self.preflight))
    }

    /// Exact create receipts are consumed at this final, infallible move.
    fn authorize(
        self,
        _receipts: super::github::ExactCreateReceipts,
    ) -> AuthorizedMarkerPushes<'destination> {
        let _evidence = self.evidence;
        AuthorizedMarkerPushes::new(self.preflight)
    }
}

/// Frozen facts whose only missing inputs are exact created PR identities.
struct ProjectionSeed<'destination> {
    entries: Box<[ProjectionEntry]>,
    final_bodies: FinalBodyRecipes,
    markers: PendingMarkerGate<'destination>,
}

impl<'destination> ProjectionSeed<'destination> {
    fn created_ids(&self) -> Box<[GherritPrId]> {
        self.entries
            .iter()
            .filter_map(|entry| match entry {
                ProjectionEntry::Created { id, .. } => Some(id.clone()),
                ProjectionEntry::Existing(_) => None,
            })
            .collect()
    }

    /// Consumes exact complete receipts and prepares every final update.
    fn complete(self, receipts: CompleteCreateReceipts) -> Result<MarkerStage<'destination>> {
        let created_ids = self.created_ids();
        let exact = receipts.into_exact(&created_ids)?;
        let bodies = self
            .final_bodies
            .complete(exact.iter().map(|(id, identity)| (id.clone(), identity.number())))?;
        let mut created = exact
            .iter()
            .map(|(id, identity)| (id.clone(), identity.clone()))
            .collect::<Vec<_>>()
            .into_iter();
        let mut operations = Vec::with_capacity(self.entries.len());

        for (entry, rendered) in self.entries.into_vec().into_iter().zip(bodies.into_vec()) {
            let (body_id, body) = rendered.into_parts();
            if body_id != *entry.id() {
                bail!("final body order does not match the projection seed");
            }
            match entry {
                ProjectionEntry::Existing(existing) => {
                    if let Some(update) = existing.into_update(body)? {
                        operations.push(update);
                    }
                }
                ProjectionEntry::Created { id, base_update } => {
                    let Some((receipt_id, identity)) = created.next() else {
                        bail!("createPullRequest receipts end before the projection seed");
                    };
                    if receipt_id != id {
                        bail!(
                            "createPullRequest receipt for '{}' cannot project change '{}'",
                            receipt_id.as_str(),
                            id.as_str()
                        );
                    }
                    operations.push(PlannedUpdate::new(
                        identity,
                        None,
                        Some(body.into_string()),
                        base_update,
                    )?);
                }
            }
        }
        if created.next().is_some() {
            bail!("createPullRequest receipts extend beyond the projection seed");
        }
        let updates = prepare_updates(operations.into_boxed_slice())?;
        let pushes = self.markers.authorize(exact);
        Ok(MarkerStage::new(pushes, FinalProjection::Updates(updates)))
    }
}

enum ExistingMarkerState {
    Present,
    Missing(ObservedMarkerAuthorization),
}

enum LocalReality {
    Existing {
        history: ValidatedChangeHistory,
        pull_request: ManagedOpenPullRequest,
        marker: ExistingMarkerState,
    },
    Missing(missing_open::CreateAuthority),
}

impl LocalReality {
    fn history(&self) -> &ValidatedChangeHistory {
        match self {
            Self::Existing { history, .. } => history,
            Self::Missing(change) => change.history(),
        }
    }

    fn into_body_and_projection(self) -> Result<(BodyRecipeInput, ProjectionDraft)> {
        match self {
            Self::Existing { history, pull_request, marker } => {
                let id = history.id().clone();
                let number = pull_request.identity().number();
                Ok((
                    BodyRecipeInput::existing(id, history, number)?,
                    ProjectionDraft::Existing {
                        pull_request: pull_request.into_validated_parts(),
                        marker,
                    },
                ))
            }
            Self::Missing(change) => {
                let (body, projection) = change.into_body_and_projection()?;
                Ok((body, ProjectionDraft::Create(projection)))
            }
        }
    }
}

enum ProjectionDraft {
    Existing { pull_request: ManagedOpenParts, marker: ExistingMarkerState },
    Create(missing_open::CreatePlanSeed),
}

#[derive(Debug)]
enum ProjectionEntry {
    Existing(ExistingProjection),
    Created {
        id: GherritPrId,
        // Creation always uses the owned base. Only a root needs a final base
        // update after GitHub assigns its identity.
        base_update: Option<String>,
    },
}

impl ProjectionEntry {
    fn id(&self) -> &GherritPrId {
        match self {
            Self::Existing(existing) => &existing.id,
            Self::Created { id, .. } => id,
        }
    }
}

#[derive(Debug)]
struct ExistingProjection {
    id: GherritPrId,
    identity: PullRequestIdentity,
    observed_body: Box<str>,
    title_update: Option<String>,
    base_update: Option<String>,
}

impl ExistingProjection {
    fn into_update(self, body: GeneratedBody) -> Result<Option<PlannedUpdate>> {
        let body = (!bodies_equal(&self.observed_body, body.as_str())).then(|| body.into_string());
        if self.title_update.is_none() && body.is_none() && self.base_update.is_none() {
            return Ok(None);
        }
        Ok(Some(PlannedUpdate::new(self.identity, self.title_update, body, self.base_update)?))
    }
}

/// Consumes complete evidence and constructs one all-or-nothing publication.
pub(super) fn plan_local_publication<'destination>(
    body_context: BodyLinkContext,
    stack: LocalStack,
    correlated: CorrelatedRepository,
    remote: ActiveRemoteChanges<'destination>,
    graph: &CommitGraphEvidence,
) -> Result<PublicationPlan<'destination>> {
    if stack.is_empty() {
        bail!("publication planning requires a nonempty local stack");
    }
    let (destination, default_branch, local) = remote.into_parts();
    if !body_context.agrees_with(destination) {
        bail!("pull request body context came from a different push repository");
    }
    let (repository, correlated) = correlated.into_planning_parts_for(destination)?;
    let (repository_id, github_default_branch) = repository.into_parts();
    let default_branch = DefaultBranch::agree(default_branch, github_default_branch)?;
    validate_stack_default(stack.default_branch(), &default_branch)?;
    let (local_pull_requests, observed_identities) = correlated.into_parts();

    if stack.len() != local.len() || stack.len() != local_pull_requests.len() {
        bail!("local stack, Git, and GitHub evidence have different change counts");
    }
    for (index, ((change, observed), pull_request)) in
        stack.iter().zip(&local).zip(&local_pull_requests).enumerate()
    {
        if observed.id() != change.id() {
            bail!(
                "Git local evidence at stack position {index} identifies '{}', expected '{}'",
                observed.id().as_str(),
                change.id().as_str()
            );
        }
        if pull_request.id() != change.id() {
            bail!(
                "GitHub local evidence at stack position {index} identifies '{}', expected '{}'",
                pull_request.id().as_str(),
                change.id().as_str()
            );
        }
    }
    let realities = stack
        .iter()
        .zip(local.into_vec())
        .zip(local_pull_requests.into_vec())
        .enumerate()
        .map(|(index, ((change, observed), pull_request))| {
            let desired_base = desired_base(index);
            let observed_base = match &pull_request {
                LocalPullRequestObservation::Open(pull_request) => Some(pull_request.base().kind()),
                LocalPullRequestObservation::Absent(_) => None,
            };
            let root_tip = (desired_base == BaseKind::Default
                || observed_base == Some(BaseKind::Default))
            .then(|| default_branch.tip());
            let history = NormalizedPublishedHistory::from_observation(observed, graph)?
                .with_proposal(change, graph)?
                .validate(graph, root_tip)?;
            match pull_request {
                LocalPullRequestObservation::Open(pull_request) => {
                    if history.published_len() == 0 {
                        bail!(
                            "local change '{}' has an OPEN pull request but no published history",
                            change.id().as_str()
                        );
                    }
                    validate_local_pull_request(
                        &history,
                        &pull_request,
                        desired_base,
                        &default_branch,
                    )?;
                    let marker = match history.pull_request_marker() {
                        Some(_) => ExistingMarkerState::Present,
                        None => ExistingMarkerState::Missing(ObservedMarkerAuthorization {
                            marker: marker_target(&history),
                        }),
                    };
                    Ok(LocalReality::Existing { history, pull_request, marker })
                }
                LocalPullRequestObservation::Absent(absence) => Ok(LocalReality::Missing(
                    missing_open::CreateAuthority::new(absence, history, repository_id.clone())?,
                )),
            }
        })
        .collect::<Result<Vec<_>>>()?;
    let history_refs = realities.iter().map(LocalReality::history).collect::<Vec<_>>();
    let tuple_preflight = preflight_tuple_pushes(destination, &history_refs)?;
    let (body_inputs, drafts): (Vec<_>, Vec<_>) = realities
        .into_iter()
        .map(LocalReality::into_body_and_projection)
        .collect::<Result<Vec<_>>>()?
        .into_iter()
        .unzip();
    let recipes = StackBodyRecipes::new(body_context, stack, body_inputs)?;
    let after_tuples =
        prepare_projection(destination, &default_branch, drafts, recipes, observed_identities)?;

    let tuples = tuple_preflight.map(AuthorizedTuplePushes::new);
    Ok(PublicationPlan::new(tuples, after_tuples))
}

fn marker_target(history: &ValidatedChangeHistory) -> MarkerTarget {
    MarkerTarget { id: history.id().clone(), target: history.projected_current().revision().head() }
}

fn validate_stack_default(stack: &DefaultBranch, agreed: &DefaultBranch) -> Result<()> {
    if stack.name() != agreed.name() {
        bail!("local stack and publication evidence use different default branch names");
    }
    if stack.tip() != agreed.tip() {
        bail!("local stack and publication evidence use different default branch tips");
    }
    Ok(())
}

fn desired_base(index: usize) -> BaseKind {
    if index == 0 { BaseKind::Default } else { BaseKind::Owned }
}

fn validate_local_pull_request(
    history: &ValidatedChangeHistory,
    pull_request: &ManagedOpenPullRequest,
    desired_base: BaseKind,
    default_branch: &DefaultBranch,
) -> Result<()> {
    validate_pull_request(
        history.id(),
        pull_request,
        history.contains_published_head(pull_request.head_oid()),
        history.contains_published_first_parent(pull_request.base().oid()),
        history.pull_request_marker().is_some(),
        Some(desired_base),
        default_branch,
    )
}

fn validate_pull_request(
    id: &GherritPrId,
    pull_request: &ManagedOpenPullRequest,
    published_head: bool,
    published_first_parent: bool,
    has_pull_request_marker: bool,
    desired_base: Option<BaseKind>,
    default_branch: &DefaultBranch,
) -> Result<()> {
    if pull_request.id() != id {
        bail!(
            "OPEN pull request for '{}' cannot validate history for '{}'",
            pull_request.id().as_str(),
            id.as_str()
        );
    }
    if !published_head {
        bail!(
            "OPEN pull request for '{}' has a head not present in published history",
            id.as_str()
        );
    }
    if !has_pull_request_marker && pull_request.base().kind() != BaseKind::Owned {
        bail!("Unmarked OPEN pull request for '{}' must still use its owned base", id.as_str());
    }
    match pull_request.base().kind() {
        BaseKind::Default if pull_request.base().oid() != default_branch.tip() => {
            bail!("OPEN pull request for '{}' has the wrong default-branch object ID", id.as_str());
        }
        BaseKind::Owned if !published_first_parent => {
            bail!(
                "OPEN pull request for '{}' has an owned-base object ID not present in published history",
                id.as_str()
            );
        }
        BaseKind::Default | BaseKind::Owned => {}
    }
    if pull_request.has_landing_automation()
        && (pull_request.base().kind() == BaseKind::Owned || desired_base == Some(BaseKind::Owned))
    {
        bail!(
            "OPEN pull request for '{}' cannot use landing automation with an owned base",
            id.as_str()
        );
    }
    Ok(())
}

fn prepare_projection<'destination>(
    destination: &'destination super::destination::PushDestination,
    default_branch: &DefaultBranch,
    drafts: Vec<ProjectionDraft>,
    recipes: StackBodyRecipes,
    observed_identities: ExactLocalPullRequestIdentities,
) -> Result<AfterTuples<'destination>> {
    if drafts.len() != recipes.final_bodies().titles().len() {
        bail!("body recipe and projection evidence have different change counts");
    }
    let (provisional, final_bodies) = recipes.into_parts();
    let mut provisional = provisional.into_vec().into_iter();
    let mut planned_creates = Vec::new();
    let mut marker_evidence = Vec::new();
    let mut entries = Vec::with_capacity(drafts.len());
    let titles = final_bodies.titles().collect::<Vec<_>>();

    for (index, (draft, (title_id, title))) in drafts.into_iter().zip(titles).enumerate() {
        match draft {
            ProjectionDraft::Existing { pull_request: open, marker } => {
                let (id, identity, observed_base, observed_title, observed_body) =
                    open.into_parts();
                if &id != title_id {
                    bail!("title recipe at stack position {index} does not match GitHub evidence");
                }
                let desired_base = desired_base(index);
                let desired_base_name = desired_base.branch_name(default_branch.name(), &id);
                let title_update =
                    (observed_title.as_ref() != title.as_str()).then(|| title.as_str().to_owned());
                let base_update =
                    (observed_base.kind() != desired_base).then_some(desired_base_name);
                entries.push(ProjectionEntry::Existing(ExistingProjection {
                    id,
                    identity,
                    observed_body,
                    title_update,
                    base_update,
                }));
                if let ExistingMarkerState::Missing(authorization) = marker {
                    marker_evidence.push(MarkerEvidence::Observed(authorization));
                }
            }
            ProjectionDraft::Create(seed) => {
                let rendered = provisional.next().ok_or_else(|| {
                    color_eyre::eyre::eyre!(
                        "body recipe omitted provisional change '{}'",
                        title_id.as_str()
                    )
                })?;
                let (body_id, body) = rendered.into_parts();
                if body_id != *title_id {
                    bail!(
                        "provisional body for '{}' cannot create change '{}'",
                        body_id.as_str(),
                        title_id.as_str()
                    );
                }
                let id = title_id.clone();
                let base_update = (desired_base(index) == BaseKind::Default)
                    .then(|| BaseKind::Default.branch_name(default_branch.name(), &id));
                let (planned, pending_marker) =
                    seed.finish(title.as_str().to_owned(), body.into_string());
                planned_creates.push(planned);
                marker_evidence.push(MarkerEvidence::Pending(pending_marker));
                entries.push(ProjectionEntry::Created { id, base_update });
            }
        }
    }
    if let Some(extra) = provisional.next() {
        bail!("body recipe contains unexpected provisional change '{}'", extra.id().as_str());
    }

    let marker_targets =
        marker_evidence.iter().map(MarkerEvidence::marker).cloned().collect::<Vec<_>>();
    let marker_preflight = preflight_marker_pushes(destination, &marker_targets)?;
    if marker_evidence.is_empty() != marker_preflight.is_none() {
        bail!("marker preflight presence does not match marker authorization evidence");
    }

    if planned_creates.is_empty() {
        let bodies = final_bodies.complete([])?;
        let operations = exact_updates(entries, bodies)?;
        let final_projection = FinalProjection::from_operations(operations)?;
        return if marker_evidence.is_empty() {
            Ok(AfterTuples::Final(final_projection))
        } else {
            let preflight = marker_preflight.ok_or_else(|| {
                color_eyre::eyre::eyre!("marker authorization requires preflight")
            })?;
            let pushes =
                PendingMarkerGate::new(preflight, marker_evidence)?.authorize_observed()?;
            Ok(AfterTuples::Markers(MarkerStage::new(pushes, final_projection)))
        };
    }

    let representative = final_bodies.representative_bodies().collect::<Vec<_>>();
    let mut conservative = Vec::new();
    for (entry, (body_id, body)) in entries.iter().zip(representative) {
        if entry.id() != body_id {
            bail!("representative body order does not match projection evidence");
        }
        if let ProjectionEntry::Existing(existing) = entry {
            conservative.push(UpdatePreflight::new(
                &existing.identity,
                existing.title_update.as_deref(),
                Some(body.as_str()),
                existing.base_update.as_deref(),
            )?);
        }
    }
    preflight_updates(&conservative)?;

    let creates = prepare_creates(planned_creates.into_boxed_slice(), observed_identities)?;
    let marker_preflight = marker_preflight
        .ok_or_else(|| color_eyre::eyre::eyre!("created pull requests require marker preflight"))?;
    let markers = PendingMarkerGate::new(marker_preflight, marker_evidence)?;
    let projection = ProjectionSeed { entries: entries.into_boxed_slice(), final_bodies, markers };
    Ok(AfterTuples::Creates(Box::new(CreateStage::new(creates, projection)?)))
}

fn exact_updates(
    entries: Vec<ProjectionEntry>,
    bodies: Box<[RenderedBody]>,
) -> Result<Vec<PlannedUpdate>> {
    if entries.len() != bodies.len() {
        bail!("final body count does not match projection evidence");
    }
    entries
        .into_iter()
        .zip(bodies.into_vec())
        .filter_map(|(entry, rendered)| {
            let (body_id, body) = rendered.into_parts();
            if body_id != *entry.id() {
                return Some(Err(color_eyre::eyre::eyre!(
                    "final body order does not match projection evidence"
                )));
            }
            match entry {
                ProjectionEntry::Existing(existing) => existing.into_update(body).transpose(),
                ProjectionEntry::Created { .. } => Some(Err(color_eyre::eyre::eyre!(
                    "a no-create projection contains a created entry"
                ))),
            }
        })
        .collect()
}

/// Defines GHerrit's sole pull-request body comparison equivalence.
///
/// CRLF and LF spellings of a line ending compare equal. Every other byte is
/// part of the generated projection and must match.
fn canonical_body_for_comparison(body: &str) -> Cow<'_, str> {
    if body.contains("\r\n") { Cow::Owned(body.replace("\r\n", "\n")) } else { Cow::Borrowed(body) }
}

fn bodies_equal(observed: &str, desired: &str) -> bool {
    canonical_body_for_comparison(observed) == canonical_body_for_comparison(desired)
}

#[cfg(test)]
mod semantic_oracle;

#[cfg(test)]
mod tests;

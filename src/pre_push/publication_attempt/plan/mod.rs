//! Planning and staged execution for one exact-local publication attempt.
//!
//! The planner consumes the complete local stack, its validated literal Git
//! histories, and the exhausted GitHub observations for those same changes.
//! It freezes every action which can be known before a write. Later actions
//! remain inside private consuming stages, so the executor cannot cross a
//! durable-effect barrier without the exact acknowledgement required by the
//! preceding stage.

use std::borrow::Cow;

use color_eyre::eyre::{Result, bail};

#[cfg(test)]
use super::refs::prepare_tuple_pushes;
use super::{
    CompletePublicationObservation, ObservedLocalPublication, ObservedPublicProjection,
    PublicBranch,
    body::{GeneratedBody, StackBodyRecipes},
    github::{
        AbsentPullRequest, BaseKind, ClosePullRequest, CompleteCreateReceipts,
        CompleteLocalPullRequests, CreatePreparation, CreatePullRequest, DraftPullRequest, Github,
        LocalPullRequestObservation, ManagedOpenPullRequestCandidate, PreflightedDuplicateCloses,
        PreparedCreates, PreparedDraftConversions, PreparedPullRequestProjection,
        PullRequestIdentity, PullRequestNumber, SelectedOpenPullRequest, UpdatePullRequest,
    },
    history::{Revision, ValidatedChangeHistory},
    marker::MarkerTemplate,
    refs::{
        MarkerTransition, PreparedPushes, PublicBranchTransition, PublicationRevision,
        TupleTransition, prepare_initial_pushes, prepare_marker_pushes,
    },
};
use crate::{
    pre_push::{
        destination::{DefaultBranch, PushDestination, RemoteBranchState},
        local::{GherritPrId, LocalChange, LocalStack, PullRequestTitle},
    },
    util,
};

enum PlannedPublicBranch {
    AlreadyDesired(PublicBranch),
    Transition(PublicBranchTransition),
}

impl PlannedPublicBranch {
    fn branch(&self) -> &PublicBranch {
        match self {
            Self::AlreadyDesired(branch) => branch,
            Self::Transition(transition) => transition.branch(),
        }
    }

    fn transition(&self) -> Option<&PublicBranchTransition> {
        match self {
            Self::AlreadyDesired(_) => None,
            Self::Transition(transition) => Some(transition),
        }
    }
}

/// Consumes management intent and its corresponding exact remote evidence
/// into the optional public projection for this attempt.
fn plan_public_branch(
    observed: Option<ObservedPublicProjection>,
    desired: gix::ObjectId,
) -> Result<Option<PlannedPublicBranch>> {
    observed
        .map(|observed| {
            let (branch, remote) = observed.into_parts();
            match remote {
                RemoteBranchState::Absent => PublicBranchTransition::create(branch, desired),
                RemoteBranchState::At(current) if current == desired => {
                    return Ok(PlannedPublicBranch::AlreadyDesired(branch));
                }
                RemoteBranchState::At(current) => {
                    PublicBranchTransition::advance(branch, current, desired)
                }
            }
            .map(PlannedPublicBranch::Transition)
        })
        .transpose()
}

/// One executable public projection for an otherwise empty local stack.
pub(super) struct EmptyPublicationPlan {
    destination: PushDestination,
    pushes: PreparedPushes,
}

impl EmptyPublicationPlan {
    pub(super) async fn execute(self) -> Result<()> {
        let Self { destination, pushes } = self;
        pushes.execute(&destination).await
    }
}

/// Consumes the sealed local publication and prepares its only possible work.
pub(super) fn plan_empty_publication(
    local: ObservedLocalPublication,
) -> Result<EmptyPublicationPlan> {
    let (destination, stack, observed_public_branch) = local.into_parts();
    if !stack.is_empty() {
        bail!("empty publication planning received a nonempty local stack");
    }
    let public_branch = plan_public_branch(observed_public_branch, stack.tip())?;
    let pushes = prepare_initial_pushes(
        &destination,
        public_branch.as_ref().and_then(PlannedPublicBranch::transition),
        &[],
    )?;
    Ok(EmptyPublicationPlan { destination, pushes })
}

/// One complete executable publication plan.
///
/// Planning consumes the validated Git provenance, destination, and GitHub
/// client. The plan retains its stages privately, and execution consumes this
/// value as one workflow.
pub(super) struct PublicationPlan {
    destination: PushDestination,
    github: Github,
    effects: PlannedPublication,
}

impl PublicationPlan {
    /// Executes the one fixed publication sequence without reobservation or
    /// retry. Every later effect remains inaccessible until its preceding
    /// durable acknowledgement has completed.
    pub(super) async fn execute(self, repository: &util::Repo) -> Result<()> {
        let Self { destination, github, effects } = self;
        effects
            .execute_with(&mut RemoteEffectDriver {
                repository,
                destination: &destination,
                github: &github,
            })
            .await
    }
}

/// Performs the four externally durable effect kinds for one bound attempt.
///
/// The staged plan, rather than the driver, owns their order and every
/// continuation. Each operation consumes an already-preflighted value and
/// returns only the acknowledgement needed to release the next stage.
pub(super) trait EffectDriver {
    async fn convert_pull_requests_to_draft(
        &mut self,
        conversions: PreparedDraftConversions,
    ) -> Result<()>;

    async fn publish_initial_refs(&mut self, pushes: PreparedPushes) -> Result<()>;

    async fn create_pull_requests(
        &mut self,
        creates: PreparedCreates,
    ) -> Result<CompleteCreateReceipts>;

    async fn publish_markers(&mut self, markers: Box<[MarkerTemplate]>) -> Result<()>;

    async fn project_pull_requests(
        &mut self,
        projection: PreparedPullRequestProjection,
    ) -> Result<()>;
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

/// One complete staged publication plan before it receives execution
/// provenance and clients.
///
/// Test-only pure planning inspects this type; production wraps it in the
/// consuming [`PublicationPlan`] above.
pub(super) struct PlannedPublication {
    draft_safety: DraftSafetyStage,
}

impl PlannedPublication {
    /// Consumes the fixed effect sequence through one attempt-bound driver.
    ///
    /// Any error ends the attempt immediately. No later effect is exposed to
    /// the driver, and no effect is retried or reobserved here.
    pub(super) async fn execute_with(self, driver: &mut impl EffectDriver) -> Result<()> {
        let Self { draft_safety } = self;
        let InitialRefsStage { initial_ref_pushes, after_initial_refs } =
            draft_safety.complete_with(driver).await?;
        driver.publish_initial_refs(initial_ref_pushes).await?;
        let marker_stage = match after_initial_refs {
            AfterInitialRefs::Ready(stage) => *stage,
            AfterInitialRefs::Creates(stage) => stage.complete_with(driver).await?,
        };
        driver.publish_markers(marker_stage.markers).await?;
        driver.project_pull_requests(marker_stage.projection).await
    }
}

/// The first effect stage. Conversion is the only transition that permits a
/// ready marker-bound root to enter an owned-base protocol prefix.
struct DraftSafetyStage {
    conversion: Option<PreparedDraftConversions>,
    after_draft_safety: InitialRefsStage,
}

impl DraftSafetyStage {
    async fn complete_with(self, driver: &mut impl EffectDriver) -> Result<InitialRefsStage> {
        let Self { conversion, after_draft_safety } = self;
        if let Some(conversion) = conversion {
            driver.convert_pull_requests_to_draft(conversion).await?;
        }
        Ok(after_draft_safety)
    }
}

/// Initial Git refs can be published only after draft safety acknowledges.
struct InitialRefsStage {
    initial_ref_pushes: PreparedPushes,
    after_initial_refs: AfterInitialRefs,
}

enum AfterInitialRefs {
    /// Every pull request identity was present in the observation, so exact
    /// final closes and updates could be rendered and preflighted during
    /// planning.
    Ready(Box<MarkerStage>),
    /// At least one identity can exist only after an exact create receipt.
    Creates(Box<CreateStage>),
}

/// Marker work followed by an already-preflighted final projection.
struct MarkerStage {
    markers: Box<[MarkerTemplate]>,
    projection: PreparedPullRequestProjection,
}

/// A necessarily nonempty create stage and its inseparable continuation.
struct CreateStage {
    creates: PreparedCreates,
    seed: ProjectionSeed,
}

impl CreateStage {
    async fn complete_with(self, driver: &mut impl EffectDriver) -> Result<MarkerStage> {
        let Self { creates, seed } = self;
        let receipts = driver.create_pull_requests(creates).await?;
        seed.complete(receipts)
    }

    /// Injects synthetic receipts without executing GitHub.
    ///
    /// Production execution must instead consume `self.creates` and pass its
    /// receipts directly to `self.seed`. Keeping this helper test-only makes
    /// it impossible for production code to pair a stage with receipts from
    /// another create operation.
    #[cfg(test)]
    fn complete_for_test(self, receipts: CompleteCreateReceipts) -> Result<MarkerStage> {
        let Self { creates: _, seed } = self;
        seed.complete(receipts)
    }
}

/// Facts whose only missing values are identities assigned by GitHub.
struct ProjectionSeed {
    entries: Box<[PendingProjectionEntry]>,
    recipes: StackBodyRecipes,
    closes: PreflightedDuplicateCloses,
    default_branch: DefaultBranch,
}

impl ProjectionSeed {
    fn complete(self, receipts: CompleteCreateReceipts) -> Result<MarkerStage> {
        let Self { entries, recipes, closes, default_branch } = self;
        let BoundProjection { entries, markers } =
            bind_created_identities(entries, receipts.into_values())?;

        // Receipt-supplied node IDs can still fail exact mutation preflight.
        // Keep markers receipt-gated until the whole projection is prepared.
        let updates = prepare_final_updates(entries, &recipes, default_branch.name())?;
        let projection = closes.prepare_projection(updates)?;
        Ok(MarkerStage { markers, projection })
    }
}

/// The only realities admitted after validating history and GitHub together.
enum ProjectionRealities {
    AllExisting(Vec<ExistingReality>),
    NeedsCreate(CreateRealities),
}

/// An ordered projection which contains at least one missing pull request.
///
/// Separating the first missing entry from the entries around it makes the
/// create requirement structural. Code handling this state never has to
/// rediscover that a create exists or defend against an empty create batch.
struct CreateRealities {
    before: Vec<ExistingReality>,
    first_missing: MissingReality,
    after: Vec<ProjectionReality>,
}

impl CreateRealities {
    fn into_ordered(self) -> Vec<ProjectionReality> {
        self.before
            .into_iter()
            .map(ProjectionReality::Existing)
            .chain([ProjectionReality::Missing(self.first_missing)])
            .chain(self.after)
            .collect()
    }
}

impl ProjectionRealities {
    fn new() -> Self {
        Self::AllExisting(Vec::new())
    }

    fn push_existing(&mut self, existing: ExistingReality) {
        match self {
            Self::AllExisting(entries) => entries.push(existing),
            Self::NeedsCreate(realities) => {
                realities.after.push(ProjectionReality::Existing(existing));
            }
        }
    }

    fn push_missing(&mut self, missing: MissingReality) {
        match self {
            Self::AllExisting(existing) => {
                *self = Self::NeedsCreate(CreateRealities {
                    before: std::mem::take(existing),
                    first_missing: missing,
                    after: Vec::new(),
                });
            }
            Self::NeedsCreate(realities) => {
                realities.after.push(ProjectionReality::Missing(missing));
            }
        }
    }
}

enum ProjectionReality {
    Existing(ExistingReality),
    Missing(MissingReality),
}

struct ExistingReality {
    pull_request: ValidatedOpenPullRequest,
    marker: Option<MarkerTemplate>,
}

/// The only OPEN pull request fields needed after history and policy
/// validation.
///
/// Construction occurs only in [`validate_open`]. Head and base object IDs and
/// landing-automation flags are validation evidence, not projection state, so
/// they cannot survive into effect planning.
#[derive(Debug)]
struct ValidatedOpenPullRequest {
    id: GherritPrId,
    identity: PullRequestIdentity,
    base_safety: CanonicalBaseSafety,
    title: Box<str>,
    body: Box<str>,
    duplicates: Box<[PullRequestIdentity]>,
}

/// The two post-validation base-safety states which later planning can use.
///
/// A canonical row is either already safe for its planned topology, or it is
/// ready on the default branch and must cross the draft barrier before moving
/// to an owned base. In particular, a ready owned-base row cannot be encoded.
#[derive(Debug)]
enum CanonicalBaseSafety {
    AlreadySafe(BaseKind),
    DraftBeforeOwned(DraftPullRequest),
}

impl CanonicalBaseSafety {
    fn observed_base(&self) -> BaseKind {
        match self {
            Self::AlreadySafe(base) => *base,
            Self::DraftBeforeOwned(_) => BaseKind::Default,
        }
    }

    fn draft_conversion(&self) -> Option<&DraftPullRequest> {
        match self {
            Self::AlreadySafe(_) => None,
            Self::DraftBeforeOwned(conversion) => Some(conversion),
        }
    }
}

impl ValidatedOpenPullRequest {
    fn id(&self) -> &GherritPrId {
        &self.id
    }

    fn identity(&self) -> &PullRequestIdentity {
        &self.identity
    }

    fn base(&self) -> BaseKind {
        self.base_safety.observed_base()
    }

    fn title(&self) -> &str {
        &self.title
    }

    fn body(&self) -> &str {
        &self.body
    }

    fn duplicate_identities(&self) -> impl Iterator<Item = &PullRequestIdentity> {
        self.duplicates.iter()
    }

    fn draft_conversion(&self) -> Option<&DraftPullRequest> {
        self.base_safety.draft_conversion()
    }
}

struct MissingReality {
    absence: AbsentPullRequest,
    revision: PublicationRevision,
    marker: MarkerOrigin,
}

/// The validated change facts from which a marker may be bound to one exact
/// pull-request number. Its private construction prevents pending planning
/// state from carrying an arbitrary change ID or tag target.
struct MarkerOrigin {
    id: GherritPrId,
    v1: gix::ObjectId,
}

impl MarkerOrigin {
    fn from_history(history: &ValidatedChangeHistory) -> Self {
        let v1 = history
            .projected_versions()
            .next()
            .expect("a projected history always contains at least the proposal")
            .1
            .head();
        Self { id: history.id().clone(), v1 }
    }

    fn bind(self, number: PullRequestNumber) -> Result<MarkerTemplate> {
        MarkerTemplate::new(self.id, self.v1, number)
    }
}

enum PendingProjectionEntry {
    Existing(Box<ExistingReality>),
    AwaitingCreate { marker: MarkerOrigin },
}

/// Every stack entry after create receipts have bound all missing identities.
/// Marker templates remain inseparable from those bindings until the final
/// projection has been preflighted.
struct BoundProjection {
    entries: Box<[BoundProjectionEntry]>,
    markers: Box<[MarkerTemplate]>,
}

enum BoundProjectionEntry {
    Existing(ValidatedOpenPullRequest),
    Created { id: GherritPrId, identity: PullRequestIdentity },
}

impl BoundProjectionEntry {
    fn id(&self) -> &GherritPrId {
        match self {
            Self::Existing(pull_request) => pull_request.id(),
            Self::Created { id, .. } => id,
        }
    }

    fn identity(&self) -> &PullRequestIdentity {
        match self {
            Self::Existing(pull_request) => pull_request.identity(),
            Self::Created { identity, .. } => identity,
        }
    }
}

/// Consumes complete exact-local evidence into one immutable publication.
pub(super) fn plan_publication(
    observed: CompletePublicationObservation,
) -> Result<PublicationPlan> {
    let (local, histories, observed) = observed.into_parts();
    let (destination, stack, observed_public_branch) = local.into_parts();
    let (github, pull_requests) = observed.into_parts();
    if github.publication_target() != &destination.publication_target() {
        bail!("GitHub publication client belongs to a different repository or push destination");
    }
    let public_branch = plan_public_branch(observed_public_branch, stack.tip())?;
    let effects = plan_bound_effects(&destination, public_branch, stack, histories, pull_requests)?;
    Ok(PublicationPlan { destination, github, effects })
}

fn plan_bound_effects(
    destination: &PushDestination,
    public_branch: Option<PlannedPublicBranch>,
    stack: LocalStack,
    histories: Box<[ValidatedChangeHistory]>,
    pull_requests: CompleteLocalPullRequests,
) -> Result<PlannedPublication> {
    if stack.is_empty() {
        bail!("publication planning requires a nonempty local stack");
    }

    let (github_default, pull_requests, create_preparation) =
        pull_requests.into_planning_parts_for(destination)?;
    let default_branch = DefaultBranch::agree(stack.default_branch().clone(), github_default)?;
    validate_ordered_inputs(&stack, &histories, &pull_requests)?;

    let desired_revisions = histories
        .iter()
        .map(|history| publication_revision(history.proposed()))
        .collect::<Result<Box<[_]>>>()?;
    let realities = build_realities(
        histories.iter().zip(desired_revisions.iter().copied()),
        pull_requests.into_vec(),
        &default_branch,
    )?;
    let draft_conversion = prepare_draft_safety(&realities)?;
    let tuple_transitions = histories
        .iter()
        .zip(&desired_revisions)
        .filter_map(|(history, desired)| tuple_transition(history, *desired))
        .collect::<Result<Vec<_>>>()?;
    let initial_ref_pushes = prepare_initial_pushes(
        destination,
        public_branch.as_ref().and_then(PlannedPublicBranch::transition),
        &tuple_transitions,
    )?;
    let body_branch = public_branch.as_ref().map(|branch| branch.branch().clone());
    let recipes = StackBodyRecipes::new(destination, body_branch, stack, histories.into_vec())?;
    let after_initial_refs =
        prepare_projection(realities, recipes, create_preparation, default_branch)?;

    Ok(PlannedPublication {
        draft_safety: DraftSafetyStage {
            conversion: draft_conversion,
            after_draft_safety: InitialRefsStage { initial_ref_pushes, after_initial_refs },
        },
    })
}

#[cfg(test)]
pub(super) fn plan_effects(
    local: ObservedLocalPublication,
    histories: Box<[ValidatedChangeHistory]>,
    pull_requests: CompleteLocalPullRequests,
) -> Result<PlannedPublication> {
    let (destination, stack, observed_public_branch) = local.into_parts();
    let public_branch = plan_public_branch(observed_public_branch, stack.tip())?;
    plan_bound_effects(&destination, public_branch, stack, histories, pull_requests)
}

/// Checks every count and positional join before any truncating iterator can
/// hide missing, extra, or reordered evidence.
fn validate_ordered_inputs(
    stack: &LocalStack,
    histories: &[ValidatedChangeHistory],
    pull_requests: &[LocalPullRequestObservation],
) -> Result<()> {
    if stack.len() != histories.len() || stack.len() != pull_requests.len() {
        bail!("local stack, Git history, and GitHub evidence have different change counts");
    }

    for (index, ((change, history), pull_request)) in
        stack.iter().zip(histories).zip(pull_requests).enumerate()
    {
        if history.id() != change.id() {
            bail!(
                "Git history at stack position {index} identifies '{}', expected '{}'",
                history.id().as_str(),
                change.id().as_str()
            );
        }
        if pull_request.id() != change.id() {
            bail!(
                "GitHub evidence at stack position {index} identifies '{}', expected '{}'",
                pull_request.id().as_str(),
                change.id().as_str()
            );
        }
        validate_proposal_join(change, history)?;
    }
    Ok(())
}

fn validate_proposal_join(change: &LocalChange, history: &ValidatedChangeHistory) -> Result<()> {
    let proposal = history.proposed();
    if proposal.head() != change.head() || proposal.first_parent() != change.first_parent() {
        bail!(
            "Git history for '{}' does not retain the local proposal and first parent",
            change.id().as_str()
        );
    }
    // The head object also seals the subject and body retained by
    // `LocalChange`: those bytes are part of the same immutable commit. A
    // separate stack-instance token would not prove any additional fact.
    Ok(())
}

fn validate_open(
    history: &ValidatedChangeHistory,
    pull_request: SelectedOpenPullRequest,
    desired_base: BaseKind,
    default_branch: &DefaultBranch,
) -> Result<ValidatedOpenPullRequest> {
    let id = history.id();
    if history.published_len() == 0 {
        bail!("GHerrit change '{}' has an OPEN pull request but no published history", id.as_str());
    }
    let canonical = pull_request.canonical_candidate();
    validate_open_candidate(history, canonical, default_branch)?;
    if canonical.has_landing_automation()
        && (canonical.base().kind() == BaseKind::Owned || desired_base == BaseKind::Owned)
    {
        bail!(
            "OPEN pull request #{} for '{}' cannot use landing automation with an owned base",
            canonical.identity().number().get(),
            id.as_str()
        );
    }
    let base_safety = if canonical.is_draft() {
        CanonicalBaseSafety::AlreadySafe(canonical.base().kind())
    } else if !history.has_pull_request_marker() {
        bail!(
            "Unmarked OPEN pull request #{} for '{}' must already be a draft",
            canonical.identity().number().get(),
            id.as_str()
        );
    } else if canonical.base().kind() == BaseKind::Owned {
        bail!(
            "OPEN pull request #{} for '{}' is ready on an owned base",
            canonical.identity().number().get(),
            id.as_str()
        );
    } else if desired_base == BaseKind::Owned {
        CanonicalBaseSafety::DraftBeforeOwned(DraftPullRequest::from_observation(
            id.clone(),
            canonical.identity().clone(),
            canonical.head_oid(),
            default_branch.name().to_owned(),
            canonical.base().oid(),
        ))
    } else {
        CanonicalBaseSafety::AlreadySafe(BaseKind::Default)
    };
    for duplicate in pull_request.duplicate_candidates() {
        validate_open_candidate(history, duplicate, default_branch)?;
        if duplicate.base().kind() != BaseKind::Owned {
            bail!(
                "Duplicate OPEN pull request #{} for '{}' must use an owned base",
                duplicate.identity().number().get(),
                id.as_str()
            );
        }
        if !duplicate.is_draft() {
            bail!(
                "Duplicate OPEN pull request #{} for '{}' must already be a draft",
                duplicate.identity().number().get(),
                id.as_str()
            );
        }
        if duplicate.has_landing_automation() {
            bail!(
                "Duplicate OPEN pull request #{} for '{}' cannot use landing automation",
                duplicate.identity().number().get(),
                id.as_str()
            );
        }
    }
    Ok(ValidatedOpenPullRequest {
        id: pull_request.id().clone(),
        identity: pull_request.identity().clone(),
        base_safety,
        title: pull_request.title().into(),
        body: pull_request.body().into(),
        duplicates: pull_request.duplicate_identities().cloned().collect(),
    })
}

/// The only protocol-reachable conversion is a marker-bound canonical which
/// was ready on the default branch and now needs an owned base. All other
/// ready rows are rejected before a Git write can make their topology visible.
fn prepare_draft_safety(
    realities: &ProjectionRealities,
) -> Result<Option<PreparedDraftConversions>> {
    let mut conversions = Vec::new();
    let mut retain = |entry: &ExistingReality| {
        if let Some(conversion) = entry.pull_request.draft_conversion() {
            conversions.push(conversion.clone());
        }
    };
    match realities {
        ProjectionRealities::AllExisting(entries) => entries.iter().for_each(&mut retain),
        ProjectionRealities::NeedsCreate(entries) => {
            entries.before.iter().for_each(&mut retain);
            for reality in &entries.after {
                if let ProjectionReality::Existing(entry) = reality {
                    retain(entry);
                }
            }
        }
    }
    (!conversions.is_empty()).then(|| PreparedDraftConversions::prepare(conversions)).transpose()
}

fn validate_open_candidate(
    history: &ValidatedChangeHistory,
    candidate: &ManagedOpenPullRequestCandidate,
    default_branch: &DefaultBranch,
) -> Result<()> {
    let id = history.id();
    let number = candidate.identity().number().get();
    if !history.contains_published_head(candidate.head_oid()) {
        bail!(
            "OPEN pull request #{number} for '{}' has a head not present in published history",
            id.as_str()
        );
    }
    if !history.has_pull_request_marker() && candidate.base().kind() != BaseKind::Owned {
        bail!(
            "Unmarked OPEN pull request #{number} for '{}' must still use its owned base",
            id.as_str()
        );
    }
    match candidate.base().kind() {
        BaseKind::Default if candidate.base().oid() != default_branch.tip() => {
            bail!(
                "OPEN pull request #{number} for '{}' has the wrong default-branch object ID",
                id.as_str()
            );
        }
        BaseKind::Owned if !history.contains_published_first_parent(candidate.base().oid()) => {
            bail!(
                "OPEN pull request #{number} for '{}' has an owned-base object ID not present in published history",
                id.as_str()
            );
        }
        BaseKind::Default | BaseKind::Owned => {}
    }
    Ok(())
}

fn tuple_transition(
    history: &ValidatedChangeHistory,
    desired: PublicationRevision,
) -> Option<Result<TupleTransition>> {
    match history.published_current() {
        None => Some(Ok(TupleTransition::create(history.id().clone(), desired))),
        Some(current) if current.revision() == history.proposed() => None,
        Some(current) => Some(publication_revision(current.revision()).and_then(|expected| {
            TupleTransition::advance(history.id().clone(), expected, desired, current.number())
        })),
    }
}

fn publication_revision(revision: Revision) -> Result<PublicationRevision> {
    PublicationRevision::new(revision.head(), revision.first_parent())
}

fn build_realities<'history>(
    histories: impl Iterator<Item = (&'history ValidatedChangeHistory, PublicationRevision)>,
    pull_requests: Vec<LocalPullRequestObservation>,
    default_branch: &DefaultBranch,
) -> Result<ProjectionRealities> {
    let mut realities = ProjectionRealities::new();
    for (index, ((history, desired), pull_request)) in histories.zip(pull_requests).enumerate() {
        let marker = history.pull_request_marker();
        match pull_request {
            LocalPullRequestObservation::Open(pull_request) => {
                let pull_request = pull_request.select(marker.map(|marker| marker.number()))?;
                let pull_request =
                    validate_open(history, pull_request, desired_base(index), default_branch)?;
                let marker = marker
                    .is_none()
                    .then(|| {
                        MarkerOrigin::from_history(history).bind(pull_request.identity().number())
                    })
                    .transpose()?;
                realities.push_existing(ExistingReality { pull_request, marker });
            }
            LocalPullRequestObservation::Absent(absence) => {
                if marker.is_some() {
                    bail!(
                        "GHerrit change '{}' has a pull request marker but no OPEN pull request",
                        history.id().as_str()
                    );
                }
                realities.push_missing(MissingReality {
                    revision: desired,
                    absence,
                    marker: MarkerOrigin::from_history(history),
                });
            }
        }
    }
    Ok(realities)
}

fn prepare_projection(
    realities: ProjectionRealities,
    recipes: StackBodyRecipes,
    create_preparation: CreatePreparation,
    default_branch: DefaultBranch,
) -> Result<AfterInitialRefs> {
    match realities {
        ProjectionRealities::AllExisting(realities) => {
            let mut markers = Vec::new();
            let mut closes = Vec::new();
            let entries = realities
                .into_iter()
                .map(|ExistingReality { pull_request, marker }| {
                    markers.extend(marker);
                    closes.extend(
                        pull_request
                            .duplicate_identities()
                            .cloned()
                            .map(ClosePullRequest::duplicate),
                    );
                    BoundProjectionEntry::Existing(pull_request)
                })
                .collect::<Box<[_]>>();
            let updates = prepare_final_updates(entries, &recipes, default_branch.name())?;
            let projection = PreparedPullRequestProjection::prepare(closes, updates)?;
            drop(create_preparation);
            Ok(AfterInitialRefs::Ready(Box::new(MarkerStage {
                markers: markers.into_boxed_slice(),
                projection,
            })))
        }
        ProjectionRealities::NeedsCreate(realities) => {
            prepare_create_stage(realities, recipes, create_preparation, default_branch)
                .map(Box::new)
                .map(AfterInitialRefs::Creates)
        }
    }
}

fn prepare_create_stage(
    realities: CreateRealities,
    recipes: StackBodyRecipes,
    create_preparation: CreatePreparation,
    default_branch: DefaultBranch,
) -> Result<CreateStage> {
    let realities = realities.into_ordered();
    let provisional = recipes.provisional_bodies();
    if realities.len() != provisional.len() || realities.len() != recipes.titles().len() {
        bail!("projection evidence and body recipes have different change counts");
    }

    let mut closes = Vec::new();
    let mut create_operations = Vec::new();
    let mut entries = Vec::with_capacity(realities.len());
    for (index, ((reality, rendered), (title_id, title))) in
        realities.into_iter().zip(provisional.into_vec()).zip(recipes.titles()).enumerate()
    {
        let (body_id, body) = rendered.into_parts();
        if &body_id != title_id || reality_id(&reality) != title_id {
            bail!("projection and body recipe order disagree at stack position {index}");
        }

        match reality {
            ProjectionReality::Existing(ExistingReality { pull_request, marker }) => {
                closes.extend(
                    pull_request.duplicate_identities().cloned().map(ClosePullRequest::duplicate),
                );
                entries.push(PendingProjectionEntry::Existing(Box::new(ExistingReality {
                    pull_request,
                    marker,
                })));
            }
            ProjectionReality::Missing(MissingReality { absence, revision, marker }) => {
                create_operations.push(CreatePullRequest::from_absence(
                    absence,
                    title.clone(),
                    body,
                    revision,
                ));
                entries.push(PendingProjectionEntry::AwaitingCreate { marker });
            }
        }
    }
    let closes = PreflightedDuplicateCloses::prepare(closes)?;
    let creates = create_preparation.prepare(create_operations)?;
    let seed =
        ProjectionSeed { entries: entries.into_boxed_slice(), recipes, closes, default_branch };
    Ok(CreateStage { creates, seed })
}

fn reality_id(reality: &ProjectionReality) -> &GherritPrId {
    match reality {
        ProjectionReality::Existing(existing) => existing.pull_request.id(),
        ProjectionReality::Missing(missing) => missing.absence.id(),
    }
}

fn bind_created_identities(
    entries: Box<[PendingProjectionEntry]>,
    receipts: Box<[(GherritPrId, PullRequestIdentity)]>,
) -> Result<BoundProjection> {
    let expected = entries
        .iter()
        .filter(|entry| matches!(entry, PendingProjectionEntry::AwaitingCreate { .. }))
        .count();
    if expected != receipts.len() {
        bail!("create receipts and missing pull requests have different counts");
    }

    let mut receipts = receipts.into_vec().into_iter();
    let mut bound = Vec::with_capacity(entries.len());
    let mut markers = Vec::new();
    for (index, entry) in entries.into_vec().into_iter().enumerate() {
        match entry {
            PendingProjectionEntry::Existing(existing) => {
                let ExistingReality { pull_request, marker } = *existing;
                markers.extend(marker);
                bound.push(BoundProjectionEntry::Existing(pull_request));
            }
            PendingProjectionEntry::AwaitingCreate { marker } => {
                let expected_id = marker.id.clone();
                let (receipt_id, identity) = receipts
                    .next()
                    .expect("receipt count was checked before binding created identities");
                if receipt_id != expected_id {
                    bail!(
                        "create receipt at missing position {index} identifies '{}', expected '{}'",
                        receipt_id.as_str(),
                        expected_id.as_str()
                    );
                }
                markers.push(marker.bind(identity.number())?);
                bound.push(BoundProjectionEntry::Created { id: expected_id, identity });
            }
        }
    }
    debug_assert!(receipts.next().is_none());
    Ok(BoundProjection { entries: bound.into_boxed_slice(), markers: markers.into_boxed_slice() })
}

fn prepare_final_updates(
    entries: Box<[BoundProjectionEntry]>,
    recipes: &StackBodyRecipes,
    default_branch: &str,
) -> Result<Vec<UpdatePullRequest>> {
    if entries.len() != recipes.titles().len() {
        bail!("final projection and body recipes have different change counts");
    }
    let assignments = entries
        .iter()
        .map(|entry| (entry.id().clone(), entry.identity().number()))
        .collect::<Box<[_]>>();
    let bodies = recipes.final_bodies(&assignments)?;
    if bodies.len() != entries.len() {
        bail!("final bodies and projection entries have different change counts");
    }

    let mut operations = Vec::new();
    for (index, ((entry, rendered), (title_id, title))) in
        entries.into_vec().into_iter().zip(bodies.into_vec()).zip(recipes.titles()).enumerate()
    {
        let (body_id, body) = rendered.into_parts();
        if entry.id() != title_id || &body_id != title_id {
            bail!("final projection and body recipe order disagree at stack position {index}");
        }
        if let Some(operation) = final_update(index, entry, title, body, default_branch)? {
            operations.push(operation);
        }
    }
    Ok(operations)
}

fn final_update(
    index: usize,
    entry: BoundProjectionEntry,
    title: &PullRequestTitle,
    body: GeneratedBody,
    default_branch: &str,
) -> Result<Option<UpdatePullRequest>> {
    let desired_kind = desired_base(index);
    match entry {
        BoundProjectionEntry::Existing(pull_request) => {
            let title = (pull_request.title() != title.as_str()).then(|| title.clone());
            let body = (!bodies_equal(pull_request.body(), body.as_str())).then_some(body);
            let base = (pull_request.base() != desired_kind)
                .then(|| desired_base_name(desired_kind, default_branch, pull_request.id()));
            if title.is_none() && body.is_none() && base.is_none() {
                return Ok(None);
            }
            UpdatePullRequest::from_projection(pull_request.identity().clone(), title, body, base)
                .map(Some)
        }
        BoundProjectionEntry::Created { id, identity } => {
            let base = (desired_kind == BaseKind::Default)
                .then(|| desired_base_name(desired_kind, default_branch, &id));
            UpdatePullRequest::from_projection(identity, None, Some(body), base).map(Some)
        }
    }
}

fn desired_base(index: usize) -> BaseKind {
    if index == 0 { BaseKind::Default } else { BaseKind::Owned }
}

fn desired_base_name(kind: BaseKind, default_branch: &str, id: &GherritPrId) -> String {
    match kind {
        BaseKind::Default => default_branch.to_owned(),
        BaseKind::Owned => format!("gherrit-bases/{}", id.as_str()),
    }
}

/// Defines the only equivalence used for generated body comparison.
///
/// GitHub may normalize CRLF to LF. Every other byte is projected content.
fn canonical_body_for_comparison(body: &str) -> Cow<'_, str> {
    if body.contains("\r\n") { Cow::Owned(body.replace("\r\n", "\n")) } else { Cow::Borrowed(body) }
}

fn bodies_equal(observed: &str, desired: &str) -> bool {
    canonical_body_for_comparison(observed) == canonical_body_for_comparison(desired)
}

#[cfg(test)]
mod tests;

//! Pure planning for one exact-local publication attempt.
//!
//! The planner consumes the complete local stack, its validated literal Git
//! histories, and the exhausted GitHub observations for those same changes.
//! It freezes every action which can be known before a write. Later actions
//! remain inside private consuming stages, so an executor cannot cross a
//! durable-effect barrier without the exact acknowledgement required by the
//! preceding stage.

#![cfg_attr(
    test,
    allow(dead_code, reason = "the production executor remains dormant until activation")
)]

use std::borrow::Cow;

use color_eyre::eyre::{Result, bail};

use super::{
    body::{GeneratedBody, StackBodyRecipes},
    destination::{DefaultBranch, PushDestination},
    github::{
        BaseKind, CompleteCreateReceipts, CompleteLocalPullRequests, CreatePreparation,
        CreatePullRequest, Github, LocalPullRequestObservation, ManagedOpenPullRequest,
        PreparedCreates, PreparedUpdates, PullRequestIdentity, UpdatePullRequest,
    },
    history::ValidatedChangeHistory,
    local::{GherritPrId, LocalChange, LocalStack},
    publication::{
        MarkerTransition, PreparedPushes, PublicationRevision, TupleTransition,
        prepare_marker_pushes, prepare_tuple_pushes,
    },
    remote::ValidatedPublication,
};

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
    pub(super) async fn execute(self) -> Result<()> {
        let Self { destination, github, effects } = self;
        let PlannedPublication { tuple_pushes, after_tuples } = effects;
        tuple_pushes.execute(&destination).await?;
        let marker_stage = match after_tuples {
            AfterTuples::Ready(stage) => *stage,
            AfterTuples::Creates(stage) => stage.complete_from_github(&github).await?,
        };
        marker_stage.marker_pushes.execute(&destination).await?;
        github.update_pull_requests(marker_stage.updates).await
    }
}

/// One complete staged publication plan before it receives execution
/// provenance and clients.
///
/// Test-only pure planning inspects this type; production wraps it in the
/// consuming [`PublicationPlan`] above.
struct PlannedPublication {
    tuple_pushes: PreparedPushes,
    after_tuples: AfterTuples,
}

enum AfterTuples {
    /// Every pull request identity was present in the observation, so exact
    /// final updates could be rendered and preflighted during planning.
    Ready(Box<MarkerStage>),
    /// At least one identity can exist only after an exact create receipt.
    Creates(Box<CreateStage>),
}

/// Marker work followed by an already-preflighted final projection.
struct MarkerStage {
    marker_pushes: PreparedPushes,
    updates: PreparedUpdates,
}

/// A necessarily nonempty create stage and its inseparable continuation.
struct CreateStage {
    creates: PreparedCreates,
    seed: ProjectionSeed,
}

impl CreateStage {
    async fn complete_from_github(self, github: &Github) -> Result<MarkerStage> {
        let Self { creates, seed } = self;
        let receipts = github.create_pull_requests(creates).await?;
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
    markers: ReceiptGatedMarkerPushes,
    default_branch: DefaultBranch,
}

impl ProjectionSeed {
    fn complete(self, receipts: CompleteCreateReceipts) -> Result<MarkerStage> {
        let Self { entries, recipes, markers, default_branch } = self;
        let entries = bind_created_identities(entries, receipts.into_values())?;

        // Receipt-supplied node IDs can still fail exact mutation preflight.
        // Keep markers receipt-gated until the whole projection is prepared.
        let updates = prepare_final_updates(entries, &recipes, default_branch.name())?;
        Ok(MarkerStage { marker_pushes: markers.authorize(), updates })
    }
}

/// Marker bytes which include at least one create-receipt authorization.
///
/// Preflight is deliberately allowed before writes, but the prepared value
/// has no escape hatch except the successful receipt-completion transition.
struct ReceiptGatedMarkerPushes(PreparedPushes);

impl ReceiptGatedMarkerPushes {
    fn authorize(self) -> PreparedPushes {
        self.0
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
    pull_request: ManagedOpenPullRequest,
    marker: Option<MarkerTransition>,
}

struct MissingReality {
    absence: super::github::AbsentPullRequest,
    revision: PublicationRevision,
    marker: MarkerTransition,
}

enum PendingProjectionEntry {
    Existing(ManagedOpenPullRequest),
    AwaitingCreate(GherritPrId),
}

enum BoundProjectionEntry {
    Existing(ManagedOpenPullRequest),
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
    validated: ValidatedPublication,
    github: Github,
    public_branch: Option<String>,
    stack: LocalStack,
    pull_requests: CompleteLocalPullRequests,
) -> Result<PublicationPlan> {
    let (histories, destination) = validated.into_parts();
    if github.publication_target() != &destination.publication_target() {
        bail!("GitHub publication client belongs to a different repository or push destination");
    }
    let effects = plan_effects(&destination, public_branch, stack, histories, pull_requests)?;
    Ok(PublicationPlan { destination, github, effects })
}

fn plan_effects(
    destination: &PushDestination,
    public_branch: Option<String>,
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
    validate_reality_set(&histories, &pull_requests, &default_branch)?;

    let desired_revisions = histories
        .iter()
        .map(|history| publication_revision(history.proposed()))
        .collect::<Result<Box<[_]>>>()?;
    let tuple_transitions = histories
        .iter()
        .zip(&desired_revisions)
        .filter_map(|(history, desired)| tuple_transition(history, *desired))
        .collect::<Result<Vec<_>>>()?;
    let tuple_pushes = prepare_tuple_pushes(destination, &tuple_transitions)?;
    let realities =
        build_realities(histories.iter().zip(desired_revisions), pull_requests.into_vec())?;
    let recipes = StackBodyRecipes::new(destination, public_branch, stack, histories.into_vec())?;
    let after_tuples =
        prepare_projection(destination, realities, recipes, create_preparation, default_branch)?;

    Ok(PlannedPublication { tuple_pushes, after_tuples })
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

fn validate_reality_set(
    histories: &[ValidatedChangeHistory],
    pull_requests: &[LocalPullRequestObservation],
    default_branch: &DefaultBranch,
) -> Result<()> {
    histories.iter().zip(pull_requests).enumerate().try_for_each(
        |(index, (history, pull_request))| match pull_request {
            LocalPullRequestObservation::Open(pull_request) => {
                validate_open(history, pull_request, desired_base(index), default_branch)
            }
            LocalPullRequestObservation::Absent(_) => {
                if history.has_pull_request_marker() {
                    bail!(
                        "GHerrit change '{}' has a pull request marker but no OPEN pull request",
                        history.id().as_str()
                    );
                }
                Ok(())
            }
        },
    )
}

fn validate_open(
    history: &ValidatedChangeHistory,
    pull_request: &ManagedOpenPullRequest,
    desired_base: BaseKind,
    default_branch: &DefaultBranch,
) -> Result<()> {
    let id = history.id();
    if history.published_len() == 0 {
        bail!("GHerrit change '{}' has an OPEN pull request but no published history", id.as_str());
    }
    if !history.contains_published_head(pull_request.head_oid()) {
        bail!(
            "OPEN pull request for '{}' has a head not present in published history",
            id.as_str()
        );
    }
    if !history.has_pull_request_marker() && pull_request.base().kind() != BaseKind::Owned {
        bail!("Unmarked OPEN pull request for '{}' must still use its owned base", id.as_str());
    }
    match pull_request.base().kind() {
        BaseKind::Default if pull_request.base().oid() != default_branch.tip() => {
            bail!("OPEN pull request for '{}' has the wrong default-branch object ID", id.as_str());
        }
        BaseKind::Owned if !history.contains_published_first_parent(pull_request.base().oid()) => {
            bail!(
                "OPEN pull request for '{}' has an owned-base object ID not present in published history",
                id.as_str()
            );
        }
        BaseKind::Default | BaseKind::Owned => {}
    }
    if pull_request.has_landing_automation()
        && (pull_request.base().kind() == BaseKind::Owned || desired_base == BaseKind::Owned)
    {
        bail!(
            "OPEN pull request for '{}' cannot use landing automation with an owned base",
            id.as_str()
        );
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

fn publication_revision(revision: super::history::Revision) -> Result<PublicationRevision> {
    PublicationRevision::new(revision.head(), revision.first_parent())
}

fn build_realities<'history>(
    histories: impl Iterator<Item = (&'history ValidatedChangeHistory, PublicationRevision)>,
    pull_requests: Vec<LocalPullRequestObservation>,
) -> Result<ProjectionRealities> {
    let mut realities = ProjectionRealities::new();
    for ((history, desired), pull_request) in histories.zip(pull_requests) {
        let marker = || {
            let (_, v1) = history
                .projected_versions()
                .next()
                .expect("every planned change has a projected v1");
            MarkerTransition::create(history.id().clone(), v1.head())
        };
        match pull_request {
            LocalPullRequestObservation::Open(pull_request) => {
                let marker = (!history.has_pull_request_marker()).then(marker).transpose()?;
                realities.push_existing(ExistingReality { pull_request, marker });
            }
            LocalPullRequestObservation::Absent(absence) => {
                realities.push_missing(MissingReality {
                    revision: desired,
                    absence,
                    marker: marker()?,
                });
            }
        }
    }
    Ok(realities)
}

fn prepare_projection(
    destination: &PushDestination,
    realities: ProjectionRealities,
    recipes: StackBodyRecipes,
    create_preparation: CreatePreparation,
    default_branch: DefaultBranch,
) -> Result<AfterTuples> {
    match realities {
        ProjectionRealities::AllExisting(realities) => {
            let mut marker_transitions = Vec::new();
            let entries = realities
                .into_iter()
                .map(|reality| {
                    marker_transitions.extend(reality.marker);
                    BoundProjectionEntry::Existing(reality.pull_request)
                })
                .collect::<Box<[_]>>();
            let marker_pushes = prepare_marker_pushes(destination, &marker_transitions)?;
            let updates = prepare_final_updates(entries, &recipes, default_branch.name())?;
            drop(create_preparation);
            Ok(AfterTuples::Ready(Box::new(MarkerStage { marker_pushes, updates })))
        }
        ProjectionRealities::NeedsCreate(realities) => prepare_create_stage(
            destination,
            realities,
            recipes,
            create_preparation,
            default_branch,
        )
        .map(Box::new)
        .map(AfterTuples::Creates),
    }
}

fn prepare_create_stage(
    destination: &PushDestination,
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

    let mut marker_transitions = Vec::new();
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
            ProjectionReality::Existing(existing) => {
                marker_transitions.extend(existing.marker);
                entries.push(PendingProjectionEntry::Existing(existing.pull_request));
            }
            ProjectionReality::Missing(missing) => {
                let id = missing.absence.id().clone();
                marker_transitions.push(missing.marker);
                create_operations.push(CreatePullRequest::from_absence(
                    missing.absence,
                    title.clone(),
                    body,
                    missing.revision,
                ));
                entries.push(PendingProjectionEntry::AwaitingCreate(id));
            }
        }
    }
    // Both collections are fully preflighted before tuple publication can be
    // exposed. Marker bytes stay private until receipt completion.
    let marker_pushes = prepare_marker_pushes(destination, &marker_transitions)?;
    let creates = create_preparation.prepare(create_operations)?;
    let seed = ProjectionSeed {
        entries: entries.into_boxed_slice(),
        recipes,
        markers: ReceiptGatedMarkerPushes(marker_pushes),
        default_branch,
    };
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
) -> Result<Box<[BoundProjectionEntry]>> {
    let expected = entries
        .iter()
        .filter(|entry| matches!(entry, PendingProjectionEntry::AwaitingCreate(_)))
        .count();
    if expected != receipts.len() {
        bail!("create receipts and missing pull requests have different counts");
    }

    let mut receipts = receipts.into_vec().into_iter();
    let bound = entries
        .into_vec()
        .into_iter()
        .enumerate()
        .map(|(index, entry)| match entry {
            PendingProjectionEntry::Existing(pull_request) => {
                Ok(BoundProjectionEntry::Existing(pull_request))
            }
            PendingProjectionEntry::AwaitingCreate(expected_id) => {
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
                Ok(BoundProjectionEntry::Created { id: expected_id, identity })
            }
        })
        .collect::<Result<Box<[_]>>>()?;
    debug_assert!(receipts.next().is_none());
    Ok(bound)
}

fn prepare_final_updates(
    entries: Box<[BoundProjectionEntry]>,
    recipes: &StackBodyRecipes,
    default_branch: &str,
) -> Result<PreparedUpdates> {
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
    PreparedUpdates::new(operations)
}

fn final_update(
    index: usize,
    entry: BoundProjectionEntry,
    title: &super::local::PullRequestTitle,
    body: GeneratedBody,
    default_branch: &str,
) -> Result<Option<UpdatePullRequest>> {
    let desired_kind = desired_base(index);
    match entry {
        BoundProjectionEntry::Existing(pull_request) => {
            let title = (pull_request.title() != title.as_str()).then(|| title.clone());
            let body = (!bodies_equal(pull_request.body(), body.as_str())).then_some(body);
            let base = (pull_request.base().kind() != desired_kind)
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

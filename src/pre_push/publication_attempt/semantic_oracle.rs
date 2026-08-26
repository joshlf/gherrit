//! Restart and concurrency checks over literal durable state.
//!
//! This model deliberately knows nothing about refspec text, GraphQL
//! documents, JSON, HTTP, or process output. Adapter tests own those
//! encodings. It preserves only the request and alias boundaries which
//! determine what can be durable after an interrupted attempt.
//!
//! A local intent, durable world, and one query's visibility are separate
//! values. Production planning consumes observations built from those values,
//! and a test-only effect driver executes the real consuming plan directly
//! against the world. It records only batches the production stage machine
//! actually reaches. Restart tests then throw every attempt-local value away
//! and construct a fresh observation and plan.

use std::collections::{HashMap, HashSet};

use color_eyre::eyre::{Result, bail};
use gix::ObjectId;

use super::{
    DefaultBranch, EffectDriver, ObservedPublicBranch, PlannedPublication, PublicBranch,
    PushDestination,
    destination::{RemoteBranchState, RepositoryCoordinates},
    github::{
        AbsentPullRequest, BaseKind, CompleteCreateReceipts, CompleteLocalPullRequests,
        LocalPullRequestObservation, ManagedOpenPullRequest, ObservedBase, PreparedCreates,
        PreparedUpdates, PullRequestIdentity, TestCreate, TestUpdate,
    },
    history::ValidatedChangeHistory,
    local::{GherritPrId, LocalStack},
    plan_effects,
    refs::{PreparedPushes, PublicationRevision, TestPushEffect},
};

const DEFAULT_BRANCH: &str = "main";
const REPOSITORY_ID: &str = "REPOSITORY_NODE";

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct LiteralRevision {
    head: ObjectId,
    first_parent: ObjectId,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct LocalChange {
    id: GherritPrId,
    desired: LiteralRevision,
    title: String,
    commit_body: String,
}

/// One nonempty, contiguous local first-parent stack.
#[derive(Clone, Debug, Eq, PartialEq)]
struct LocalIntent {
    first: LocalChange,
    later: Box<[LocalChange]>,
}

impl LocalIntent {
    fn new(default_tip: ObjectId, changes: impl IntoIterator<Item = LocalChange>) -> Self {
        let mut changes = changes.into_iter();
        let first = changes.next().expect("a local publication intent is nonempty");
        assert_eq!(first.desired.first_parent, default_tip, "the first local change is the root");
        let later = changes.collect::<Box<[_]>>();
        assert!(
            std::iter::once(&first)
                .chain(later.iter())
                .zip(later.iter())
                .all(|(parent, child)| child.desired.first_parent == parent.desired.head),
            "a local intent is one contiguous first-parent stack"
        );
        let mut ids = HashSet::with_capacity(later.len() + 1);
        assert!(
            std::iter::once(&first).chain(later.iter()).all(|change| ids.insert(change.id.clone())),
            "one local intent cannot repeat a change ID"
        );
        Self { first, later }
    }

    fn iter(&self) -> impl Iterator<Item = &LocalChange> {
        std::iter::once(&self.first).chain(self.later.iter())
    }
}

/// One nonempty immutable publication history.
#[derive(Clone, Debug, Eq, PartialEq)]
struct PublishedHistory {
    first: LiteralRevision,
    later: Vec<LiteralRevision>,
}

impl PublishedHistory {
    fn new(first: LiteralRevision) -> Self {
        Self { first, later: Vec::new() }
    }

    fn iter(&self) -> impl Iterator<Item = &LiteralRevision> {
        std::iter::once(&self.first).chain(self.later.iter())
    }

    fn last(&self) -> LiteralRevision {
        self.later.last().copied().unwrap_or(self.first)
    }

    fn len(&self) -> usize {
        self.later.len() + 1
    }

    fn get(&self, index: usize) -> Option<LiteralRevision> {
        if index == 0 { Some(self.first) } else { self.later.get(index - 1).copied() }
    }

    fn push(&mut self, revision: LiteralRevision) {
        self.later.push(revision);
    }

    fn contains_head(&self, head: ObjectId) -> bool {
        self.iter().any(|revision| revision.head == head)
    }
}

/// Literal durable fields of one OPEN pull request.
///
/// The Git head and owned-base object IDs are deliberately absent: those are
/// values of the mutable Git refs. One query can return an older view of them,
/// represented separately by [`QueryVisibility`].
#[derive(Clone, Debug, Eq, PartialEq)]
struct PullRequest {
    identity: PullRequestIdentity,
    title: String,
    body: String,
}

/// Marker state exists only together with the OPEN identity which authorized
/// it. This excludes a marker-without-pull-request world by construction.
#[derive(Clone, Debug, Eq, PartialEq)]
enum ManagedPullRequest {
    Unmarked(PullRequest),
    Marked { pull_request: PullRequest, target: ObjectId, base: BaseKind },
}

impl ManagedPullRequest {
    fn pull_request(&self) -> &PullRequest {
        match self {
            Self::Unmarked(pull_request) | Self::Marked { pull_request, .. } => pull_request,
        }
    }

    fn marker(&self) -> Option<ObjectId> {
        match self {
            Self::Unmarked(_) => None,
            Self::Marked { target, .. } => Some(*target),
        }
    }

    fn base(&self) -> BaseKind {
        match self {
            Self::Unmarked(_) => BaseKind::Owned,
            Self::Marked { base, .. } => *base,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct PublishedChange {
    history: PublishedHistory,
    pull_request: Option<ManagedPullRequest>,
}

/// Literal state shared by every publisher.
///
/// Only published changes exist here. Absence from `changes` is the complete
/// durable representation of an unpublished change; the world does not know
/// the universe of future local intents. Complete commit-DAG validity and the
/// safety of each admitted history are preconditions established by the
/// lower history-validation layer. This model checks only facts it represents
/// completely.
#[derive(Clone, Debug, Eq, PartialEq)]
struct DurableWorld {
    default_tip: ObjectId,
    changes: HashMap<GherritPrId, PublishedChange>,
    public_branches: HashMap<String, ObjectId>,
    next_identity: u32,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct ObservedOpenFields {
    head_oid: ObjectId,
    base: BaseKind,
    base_oid: ObjectId,
    title: String,
    body: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum OpenVisibility {
    Hidden,
    Stale(ObservedOpenFields),
}

/// Per-query departures from the current durable GitHub state.
///
/// An absent entry observes the current OPEN row. A stale row is either
/// captured from one earlier world or assembled from independently selected,
/// validated immutable head/base history slots.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
struct QueryVisibility {
    open: HashMap<GherritPrId, OpenVisibility>,
}

impl QueryVisibility {
    fn hiding(world: &DurableWorld, ids: impl IntoIterator<Item = GherritPrId>) -> Self {
        let mut visibility = Self::default();
        for id in ids {
            assert!(world.open_pull_request(&id).is_some(), "only an existing OPEN row can hide");
            assert!(
                visibility.open.insert(id, OpenVisibility::Hidden).is_none(),
                "one query has one visibility state per change"
            );
        }
        visibility
    }

    fn stale(world: &DurableWorld, ids: impl IntoIterator<Item = GherritPrId>) -> Self {
        let mut visibility = Self::default();
        for id in ids {
            let fields = world.current_open_fields(&id);
            assert!(
                visibility.open.insert(id, OpenVisibility::Stale(fields)).is_none(),
                "one query has one visibility state per change"
            );
        }
        visibility
    }

    /// Captures independently lagged Git head and base-ref observations.
    ///
    /// GitHub can expose the two refs from different immutable history slots.
    /// The OPEN row's non-ref fields are captured at construction time, so a
    /// later durable write cannot retroactively advance this query result.
    fn historical_slots(
        world: &DurableWorld,
        id: GherritPrId,
        head_slot: usize,
        base_slot: usize,
    ) -> Self {
        let mut fields = world.current_open_fields(&id);
        let change = world.published(&id).expect("an OPEN row has published history");
        fields.head_oid = change.history.get(head_slot).expect("valid historical head slot").head;
        let historical_base =
            change.history.get(base_slot).expect("valid historical base slot").first_parent;
        fields.base_oid = match fields.base {
            BaseKind::Default => world.default_tip,
            BaseKind::Owned => historical_base,
        };
        Self { open: HashMap::from([(id, OpenVisibility::Stale(fields))]) }
    }

    fn open(&self, id: &GherritPrId) -> Option<&OpenVisibility> {
        self.open.get(id)
    }
}

impl DurableWorld {
    fn for_intents(default_tip: ObjectId, intents: &[&LocalIntent]) -> Self {
        let mut commits = HashMap::<ObjectId, (&GherritPrId, &LiteralRevision, &str, &str)>::new();
        assert!(!default_tip.is_null(), "the modeled default tip is non-null");
        assert!(
            intents.iter().all(|intent| intent.first.desired.first_parent == default_tip),
            "every scenario intent is rooted at the world's default tip"
        );
        for local in intents.iter().flat_map(|intent| intent.iter()) {
            assert!(!local.desired.head.is_null());
            assert!(!local.desired.first_parent.is_null());
            assert_ne!(local.desired.head, local.desired.first_parent);
            if let Some((id, revision, title, body)) = commits.insert(
                local.desired.head,
                (&local.id, &local.desired, &local.title, &local.commit_body),
            ) {
                assert_eq!(id, &local.id, "one commit object has one change ID");
                assert_eq!(revision, &local.desired, "one commit object has one first parent");
                assert_eq!(title, local.title, "one commit object has one title");
                assert_eq!(body, local.commit_body, "one commit object has one body");
            }
        }
        assert!(!commits.is_empty(), "a modeled scenario has at least one local change");
        let world = Self {
            default_tip,
            changes: HashMap::new(),
            public_branches: HashMap::new(),
            next_identity: 100,
        };
        world.assert_well_formed();
        world
    }

    fn published(&self, id: &GherritPrId) -> Option<&PublishedChange> {
        self.changes.get(id)
    }

    fn published_mut(&mut self, id: &GherritPrId) -> Option<&mut PublishedChange> {
        self.changes.get_mut(id)
    }

    fn open_pull_request(&self, id: &GherritPrId) -> Option<&ManagedPullRequest> {
        self.published(id)?.pull_request.as_ref()
    }

    fn publish_for_setup(&mut self, id: &GherritPrId, revision: LiteralRevision) {
        match self.changes.get_mut(id) {
            None => {
                self.changes.insert(
                    id.clone(),
                    PublishedChange {
                        history: PublishedHistory::new(revision),
                        pull_request: None,
                    },
                );
            }
            Some(change) => change.history.push(revision),
        }
        self.assert_well_formed();
    }

    fn open_for_setup(&mut self, id: &GherritPrId, title: &str, body: &str) {
        let identity = self.allocate_identity();
        let change = self.published_mut(id).expect("an OPEN row requires published history");
        assert!(change.pull_request.is_none(), "test setup cannot replace an OPEN row");
        change.pull_request = Some(ManagedPullRequest::Unmarked(PullRequest {
            identity,
            title: title.to_owned(),
            body: body.to_owned(),
        }));
        self.assert_well_formed();
    }

    fn mark_for_setup(&mut self, id: &GherritPrId, target: ObjectId, base: BaseKind) {
        let change = self.published_mut(id).expect("a marker requires published history");
        assert!(change.history.contains_head(target), "a marker targets immutable history");
        let pull_request = change.pull_request.take().expect("a marker requires an OPEN identity");
        let ManagedPullRequest::Unmarked(pull_request) = pull_request else {
            panic!("test setup cannot move an immutable marker")
        };
        change.pull_request = Some(ManagedPullRequest::Marked { pull_request, target, base });
        self.assert_well_formed();
    }

    fn allocate_identity(&mut self) -> PullRequestIdentity {
        let number = self.next_identity;
        self.next_identity = self.next_identity.checked_add(1).expect("test identity space");
        PullRequestIdentity::for_plan_test(number, &format!("PR_{number}"))
    }

    fn identity(&self, id: &GherritPrId) -> PullRequestIdentity {
        self.open_pull_request(id)
            .expect("the requested change has an OPEN identity")
            .pull_request()
            .identity
            .clone()
    }

    fn current_open_fields(&self, id: &GherritPrId) -> ObservedOpenFields {
        let change = self.published(id).expect("an OPEN row has published history");
        let current = change.history.last();
        let managed = change.pull_request.as_ref().expect("the requested change has an OPEN row");
        let pull_request = managed.pull_request();
        ObservedOpenFields {
            head_oid: current.head,
            base: managed.base(),
            base_oid: match managed.base() {
                BaseKind::Default => self.default_tip,
                BaseKind::Owned => current.first_parent,
            },
            title: pull_request.title.clone(),
            body: pull_request.body.clone(),
        }
    }

    fn plan(
        &self,
        intent: &LocalIntent,
        visibility: &QueryVisibility,
    ) -> Result<PlannedPublication> {
        self.plan_with_public_branch(intent, visibility, None)
    }

    fn plan_with_public_branch(
        &self,
        intent: &LocalIntent,
        visibility: &QueryVisibility,
        public_branch: Option<&str>,
    ) -> Result<PlannedPublication> {
        assert!(
            visibility.open.keys().all(|id| intent.iter().any(|local| &local.id == id)),
            "every query visibility override belongs to the queried intent"
        );
        let destination = PushDestination::for_test();
        let default = DefaultBranch::new(DEFAULT_BRANCH.to_owned(), self.default_tip).unwrap();
        let stack = LocalStack::for_plan_test(
            default.clone(),
            intent.iter().map(|change| {
                (
                    change.id.clone(),
                    change.desired.head,
                    change.desired.first_parent,
                    change.title.clone(),
                    change.commit_body.clone(),
                )
            }),
        );
        let public_branch = public_branch
            .map(|name| {
                let branch = PublicBranch::new(
                    crate::manage::PublicBranchName::new(name.to_owned())?,
                    &default,
                )?;
                let remote = self
                    .public_branches
                    .get(name)
                    .copied()
                    .map_or(RemoteBranchState::Absent, RemoteBranchState::At);
                ObservedPublicBranch { branch, remote }.plan(stack.tip())
            })
            .transpose()?;
        let histories = intent
            .iter()
            .map(|local| {
                let (published, marker) = match self.published(&local.id) {
                    None => (Vec::new(), false),
                    Some(change) => (
                        change
                            .history
                            .iter()
                            .map(|revision| (revision.head, revision.first_parent))
                            .collect(),
                        change.pull_request.as_ref().and_then(ManagedPullRequest::marker).is_some(),
                    ),
                };
                ValidatedChangeHistory::for_plan_test(
                    local.id.clone(),
                    &published,
                    (local.desired.head, local.desired.first_parent),
                    marker,
                )
            })
            .collect::<Vec<_>>()
            .into_boxed_slice();
        let pull_requests =
            intent.iter().map(|local| self.observe_pull_request(&local.id, visibility)).collect();
        let pull_requests = CompleteLocalPullRequests::for_plan_test(
            RepositoryCoordinates::for_test("owner", "repo"),
            default,
            pull_requests,
            &[],
        )?;
        plan_effects(&destination, public_branch, stack, histories, pull_requests)
    }

    async fn run_attempt(
        &mut self,
        intent: &LocalIntent,
        visibility: &QueryVisibility,
        interruption: Option<Interruption>,
    ) -> Result<AttemptReport> {
        let plan = self.plan(intent, visibility)?;
        self.execute_plan(plan, interruption).await
    }

    async fn execute_plan(
        &mut self,
        plan: PlannedPublication,
        interruption: Option<Interruption>,
    ) -> Result<AttemptReport> {
        let mut driver = WorldDriver::new(self, interruption);
        let result = plan.execute_with(&mut driver).await;
        driver.finish(result)
    }

    fn observe_pull_request(
        &self,
        id: &GherritPrId,
        visibility: &QueryVisibility,
    ) -> LocalPullRequestObservation {
        let Some(pull_request) = self.open_pull_request(id) else {
            assert!(visibility.open(id).is_none(), "a query cannot override a nonexistent row");
            return LocalPullRequestObservation::Absent(AbsentPullRequest::for_plan_test(
                id.clone(),
            ));
        };
        let fields = match visibility.open(id) {
            Some(OpenVisibility::Hidden) => {
                return LocalPullRequestObservation::Absent(AbsentPullRequest::for_plan_test(
                    id.clone(),
                ));
            }
            Some(OpenVisibility::Stale(fields)) => fields.clone(),
            None => self.current_open_fields(id),
        };
        LocalPullRequestObservation::Open(ManagedOpenPullRequest::for_plan_test(
            id.clone(),
            pull_request.pull_request().identity.clone(),
            fields.head_oid,
            ObservedBase::for_plan_test(fields.base, fields.base_oid),
            &fields.title,
            &fields.body,
            false,
        ))
    }

    fn assert_well_formed(&self) {
        assert!(
            self.public_branches.iter().all(|(name, oid)| !name.is_empty() && !oid.is_null()),
            "every modeled public branch has a name and non-null target"
        );
        let mut identities = HashSet::new();
        let mut parents = HashMap::<ObjectId, ObjectId>::new();
        for published in self.changes.values() {
            for revision in published.history.iter() {
                assert!(!revision.head.is_null());
                assert!(!revision.first_parent.is_null());
                assert_ne!(revision.head, revision.first_parent);
                if let Some(previous) = parents.insert(revision.head, revision.first_parent) {
                    assert_eq!(previous, revision.first_parent, "one commit object has one parent");
                }
            }
            let Some(pull_request) = &published.pull_request else {
                continue;
            };
            let pull_request_fields = pull_request.pull_request();
            assert!(
                identities.insert(pull_request_fields.identity.clone()),
                "durable OPEN identities are unique"
            );
            assert!(
                pull_request_fields.identity.number().get() < self.next_identity,
                "the allocation cursor follows every durable OPEN identity"
            );
            if let Some(marker) = pull_request.marker() {
                assert!(
                    published.history.contains_head(marker),
                    "an immutable marker targets published history"
                );
            }
        }
    }
}

fn id(value: &str) -> GherritPrId {
    GherritPrId::from_ref_component(value.as_bytes()).unwrap()
}

fn oid(value: u16) -> ObjectId {
    let mut bytes = [0_u8; 20];
    bytes[18..].copy_from_slice(&value.to_be_bytes());
    ObjectId::from_bytes_or_panic(&bytes)
}

fn local_change(
    name: &str,
    desired: LiteralRevision,
    title: &str,
    commit_body: &str,
) -> LocalChange {
    LocalChange {
        id: id(name),
        desired,
        title: title.to_owned(),
        commit_body: commit_body.to_owned(),
    }
}

fn root_intent(default_tip: ObjectId, name: &str, revision: LiteralRevision) -> LocalIntent {
    LocalIntent::new(
        default_tip,
        [local_change(name, revision, &format!("Title for {name}"), &format!("Body for {name}"))],
    )
}

type EffectBatches<T> = Vec<Box<[T]>>;

#[derive(Clone, Debug, Eq, PartialEq)]
struct TupleEffect {
    id: GherritPrId,
    expected: Option<LiteralRevision>,
    desired: LiteralRevision,
    version: u64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct PublicBranchEffect {
    branch: String,
    expected: Option<ObjectId>,
    desired: ObjectId,
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum InitialRefEffect {
    Tuple(TupleEffect),
    PublicBranch(PublicBranchEffect),
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct MarkerEffect {
    id: GherritPrId,
    target: ObjectId,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
struct AttemptTrace {
    tuples: EffectBatches<TupleEffect>,
    creates: Option<EffectBatches<TestCreate>>,
    markers: EffectBatches<MarkerEffect>,
    updates: EffectBatches<TestUpdate>,
}

impl AttemptTrace {
    fn is_empty(&self) -> bool {
        self.tuples.is_empty()
            && self.creates.is_none()
            && self.markers.is_empty()
            && self.updates.is_empty()
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum AttemptOutcome {
    Acknowledged,
    Stopped(StopReason),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum StopReason {
    Indeterminate,
    Rejected,
}

#[derive(Clone, Debug)]
enum Interruption {
    InitialRefs { batch: usize, applied: bool },
    Create { batch: usize, applied_aliases: Box<[usize]> },
    Marker { batch: usize, applied: bool },
    Update { batch: usize, applied_aliases: Box<[usize]> },
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct AttemptReport {
    outcome: AttemptOutcome,
    trace: AttemptTrace,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum EffectStage {
    InitialRefs,
    Create,
    Marker,
    Update,
}

struct BatchCompetitor {
    after_stage: EffectStage,
    after_batch: usize,
    competing: Option<(PlannedPublication, Option<Interruption>)>,
    report: Option<AttemptReport>,
}

/// Executes the real consuming publication plan directly against literal
/// durable state. The trace is evidence of calls the production stage machine
/// actually reached; it is never replayed to drive another attempt.
struct WorldDriver<'world> {
    world: &'world mut DurableWorld,
    interruption: Option<Interruption>,
    interruption_consumed: bool,
    failure: Option<StopReason>,
    trace: AttemptTrace,
    batch_competitor: Option<BatchCompetitor>,
}

impl<'world> WorldDriver<'world> {
    fn new(world: &'world mut DurableWorld, interruption: Option<Interruption>) -> Self {
        Self {
            world,
            interruption,
            interruption_consumed: false,
            failure: None,
            trace: AttemptTrace::default(),
            batch_competitor: None,
        }
    }

    fn with_batch_competitor(
        world: &'world mut DurableWorld,
        after_stage: EffectStage,
        after_batch: usize,
        competing: PlannedPublication,
        competing_interruption: Option<Interruption>,
    ) -> Self {
        let mut driver = Self::new(world, None);
        driver.batch_competitor = Some(BatchCompetitor {
            after_stage,
            after_batch,
            competing: Some((competing, competing_interruption)),
            report: None,
        });
        driver
    }

    fn take_interruption(&mut self, stage: EffectStage, batch: usize) -> Option<Interruption> {
        let matches = match self.interruption.as_ref() {
            Some(Interruption::InitialRefs { batch: stopped, .. }) => {
                stage == EffectStage::InitialRefs && *stopped == batch
            }
            Some(Interruption::Create { batch: stopped, .. }) => {
                stage == EffectStage::Create && *stopped == batch
            }
            Some(Interruption::Marker { batch: stopped, .. }) => {
                stage == EffectStage::Marker && *stopped == batch
            }
            Some(Interruption::Update { batch: stopped, .. }) => {
                stage == EffectStage::Update && *stopped == batch
            }
            None => false,
        };
        if !matches {
            return None;
        }
        assert!(!self.interruption_consumed, "one attempt has only one stop point");
        self.interruption_consumed = true;
        self.interruption.clone()
    }

    fn stop<T>(&mut self, reason: StopReason) -> Result<T> {
        assert!(self.failure.replace(reason).is_none(), "one attempt stops only once");
        bail!("injected or observed external interruption")
    }

    async fn run_competing_after_batch(
        &mut self,
        completed_stage: EffectStage,
        completed_batch: usize,
    ) -> Result<()> {
        let Some(schedule) = self.batch_competitor.as_mut() else {
            return Ok(());
        };
        if schedule.after_stage != completed_stage || schedule.after_batch != completed_batch {
            return Ok(());
        }
        let (plan, interruption) =
            schedule.competing.take().expect("one between-request competing process runs once");
        let mut driver = WorldDriver::new(self.world, interruption);
        let result = Box::pin(plan.execute_with(&mut driver)).await;
        let report = driver.finish(result)?;
        self.batch_competitor.as_mut().unwrap().report = Some(report);
        Ok(())
    }

    fn batch_competing_report(&self) -> Option<&AttemptReport> {
        self.batch_competitor.as_ref()?.report.as_ref()
    }

    fn finish(self, result: Result<()>) -> Result<AttemptReport> {
        if self.interruption.is_some() {
            assert!(self.interruption_consumed, "the configured interruption was not reached");
        }
        let outcome = match (result, self.failure) {
            (Ok(()), None) => AttemptOutcome::Acknowledged,
            (Err(_), Some(outcome)) => AttemptOutcome::Stopped(outcome),
            (Err(error), None) => return Err(error),
            (Ok(()), Some(_)) => panic!("a stopped driver cannot release a later stage"),
        };
        if let Some(schedule) = &self.batch_competitor {
            assert!(schedule.competing.is_none(), "the configured batch boundary was not reached");
            assert!(schedule.report.is_some(), "the competing process produced no report");
        }
        Ok(AttemptReport { outcome, trace: self.trace })
    }
}

/// Runs one independently planned publisher after a primary stage barrier.
///
/// Both publishers still execute the production consuming state machine. The
/// wrapper only chooses when the competing process runs; it never extracts or
/// replays either plan's effects.
struct InterleavingDriver<'world> {
    primary: WorldDriver<'world>,
    after: EffectStage,
    competing: Option<(PlannedPublication, Option<Interruption>)>,
    competing_report: Option<AttemptReport>,
}

impl<'world> InterleavingDriver<'world> {
    fn new(
        world: &'world mut DurableWorld,
        after: EffectStage,
        competing: PlannedPublication,
        competing_interruption: Option<Interruption>,
    ) -> Self {
        assert_ne!(after, EffectStage::Update, "nothing follows the final update stage");
        Self {
            primary: WorldDriver::new(world, None),
            after,
            competing: Some((competing, competing_interruption)),
            competing_report: None,
        }
    }

    async fn run_competing_after(&mut self, stage: EffectStage) -> Result<()> {
        if self.after != stage {
            return Ok(());
        }
        let (plan, interruption) = self.competing.take().expect("one competing process runs once");
        let mut driver = WorldDriver::new(self.primary.world, interruption);
        let result = plan.execute_with(&mut driver).await;
        self.competing_report = Some(driver.finish(result)?);
        Ok(())
    }

    fn finish(self, result: Result<()>) -> Result<(AttemptReport, AttemptReport)> {
        let Self { primary, competing, competing_report, .. } = self;
        let primary = primary.finish(result)?;
        assert!(competing.is_none(), "the selected stage barrier was not reached");
        let competing = competing_report.expect("the competing process produced one report");
        Ok((primary, competing))
    }
}

impl EffectDriver for InterleavingDriver<'_> {
    async fn publish_initial_refs(&mut self, pushes: PreparedPushes) -> Result<()> {
        self.primary.publish_initial_refs(pushes).await?;
        self.run_competing_after(EffectStage::InitialRefs).await
    }

    async fn create_pull_requests(
        &mut self,
        creates: PreparedCreates,
    ) -> Result<CompleteCreateReceipts> {
        let receipts = self.primary.create_pull_requests(creates).await?;
        self.run_competing_after(EffectStage::Create).await?;
        Ok(receipts)
    }

    async fn publish_markers(&mut self, pushes: PreparedPushes) -> Result<()> {
        self.primary.publish_markers(pushes).await?;
        self.run_competing_after(EffectStage::Marker).await
    }

    async fn update_pull_requests(&mut self, updates: PreparedUpdates) -> Result<()> {
        self.primary.update_pull_requests(updates).await
    }
}

async fn execute_interleaved(
    world: &mut DurableWorld,
    primary: PlannedPublication,
    competing: PlannedPublication,
    after: EffectStage,
    competing_interruption: Option<Interruption>,
) -> Result<(AttemptReport, AttemptReport)> {
    let mut driver = InterleavingDriver::new(world, after, competing, competing_interruption);
    let result = primary.execute_with(&mut driver).await;
    driver.finish(result)
}

async fn execute_batch_interleaved(
    world: &mut DurableWorld,
    primary: PlannedPublication,
    competing: PlannedPublication,
    after_stage: EffectStage,
    after_batch: usize,
    competing_interruption: Option<Interruption>,
) -> Result<(AttemptReport, AttemptReport)> {
    let mut driver = WorldDriver::with_batch_competitor(
        world,
        after_stage,
        after_batch,
        competing,
        competing_interruption,
    );
    let result = primary.execute_with(&mut driver).await;
    let competing = driver.batch_competing_report().cloned();
    let primary = driver.finish(result)?;
    Ok((primary, competing.expect("the competing process produced one report")))
}

fn validated_aliases(aliases: &[usize], batch_len: usize) -> HashSet<usize> {
    let selected = aliases.iter().copied().collect::<HashSet<_>>();
    assert_eq!(selected.len(), aliases.len(), "an interruption cannot repeat an alias");
    assert!(selected.iter().all(|index| *index < batch_len), "interrupted alias is in range");
    selected
}

impl EffectDriver for WorldDriver<'_> {
    async fn publish_initial_refs(&mut self, pushes: PreparedPushes) -> Result<()> {
        for (index, batch) in pushes.batches().enumerate() {
            let effects = batch
                .semantic_effects_for_test()
                .iter()
                .map(|effect| match effect {
                    TestPushEffect::PublicBranch { branch, expected, desired } => {
                        InitialRefEffect::PublicBranch(PublicBranchEffect {
                            branch: branch.clone(),
                            expected: *expected,
                            desired: *desired,
                        })
                    }
                    TestPushEffect::Tuple { id, expected, desired, version } => {
                        InitialRefEffect::Tuple(TupleEffect {
                            id: id.clone(),
                            expected: expected.map(literal_revision),
                            desired: literal_revision(*desired),
                            version: *version,
                        })
                    }
                    TestPushEffect::Marker { .. } => {
                        panic!("the initial Git stage cannot contain marker effects")
                    }
                })
                .collect::<Box<[_]>>();
            self.trace.tuples.push(
                effects
                    .iter()
                    .filter_map(|effect| match effect {
                        InitialRefEffect::Tuple(effect) => Some(effect.clone()),
                        InitialRefEffect::PublicBranch(_) => None,
                    })
                    .collect(),
            );
            if let Some(Interruption::InitialRefs { applied, .. }) =
                self.take_interruption(EffectStage::InitialRefs, index)
            {
                if applied
                    && apply_atomic_initial_ref_batch(self.world, &effects)
                        != EffectOutcome::Acknowledged
                {
                    return self.stop(StopReason::Rejected);
                }
                return self.stop(StopReason::Indeterminate);
            }
            let outcome = apply_atomic_initial_ref_batch(self.world, &effects);
            if outcome != EffectOutcome::Acknowledged {
                return self.stop(outcome.stop_reason());
            }
            self.run_competing_after_batch(EffectStage::InitialRefs, index).await?;
        }
        Ok(())
    }

    async fn create_pull_requests(
        &mut self,
        creates: PreparedCreates,
    ) -> Result<CompleteCreateReceipts> {
        assert_eq!(creates.repository_id_for_test(), REPOSITORY_ID);
        self.trace.creates = Some(Vec::new());
        let mut receipts = Vec::with_capacity(creates.operations_for_test().len());
        for (index, batch) in creates.batches_for_test().enumerate() {
            let batch = batch.to_vec().into_boxed_slice();
            self.trace.creates.as_mut().unwrap().push(batch.clone());
            let interruption = self.take_interruption(EffectStage::Create, index);
            let selected = match &interruption {
                Some(Interruption::Create { applied_aliases, .. }) => {
                    Some(validated_aliases(applied_aliases, batch.len()))
                }
                _ => None,
            };
            let mut exact = true;
            for (alias, create) in batch.iter().enumerate() {
                if selected.as_ref().is_some_and(|selected| !selected.contains(&alias)) {
                    continue;
                }
                let outcome = self.world.apply_effect(&ExternalEffect::Create(create.clone()));
                exact &= outcome == EffectOutcome::Acknowledged;
                if outcome == EffectOutcome::Acknowledged {
                    receipts.push((create.id.clone(), self.world.identity(&create.id)));
                }
            }
            if interruption.is_some() || !exact {
                // A GraphQL response with any missing, duplicate, or mismatched
                // alias is indeterminate even when some siblings durably land.
                return self.stop(StopReason::Indeterminate);
            }
            self.run_competing_after_batch(EffectStage::Create, index).await?;
        }
        Ok(CompleteCreateReceipts::for_plan_test(receipts))
    }

    async fn publish_markers(&mut self, pushes: PreparedPushes) -> Result<()> {
        for (index, batch) in pushes.batches().enumerate() {
            let effects = batch
                .semantic_effects_for_test()
                .iter()
                .map(|effect| match effect {
                    TestPushEffect::Marker { id, target } => {
                        MarkerEffect { id: id.clone(), target: *target }
                    }
                    TestPushEffect::Tuple { .. } => {
                        panic!("the marker stage cannot contain tuple effects")
                    }
                    TestPushEffect::PublicBranch { .. } => {
                        panic!("the marker stage cannot contain public branch effects")
                    }
                })
                .collect::<Box<[_]>>();
            self.trace.markers.push(effects.clone());
            if let Some(Interruption::Marker { applied, .. }) =
                self.take_interruption(EffectStage::Marker, index)
            {
                if applied
                    && apply_atomic_git_batch(self.world, &effects, ExternalEffect::Marker)
                        != EffectOutcome::Acknowledged
                {
                    return self.stop(StopReason::Rejected);
                }
                return self.stop(StopReason::Indeterminate);
            }
            let outcome = apply_atomic_git_batch(self.world, &effects, ExternalEffect::Marker);
            if outcome != EffectOutcome::Acknowledged {
                return self.stop(outcome.stop_reason());
            }
            self.run_competing_after_batch(EffectStage::Marker, index).await?;
        }
        Ok(())
    }

    async fn update_pull_requests(&mut self, updates: PreparedUpdates) -> Result<()> {
        for (index, batch) in updates.batches_for_test().enumerate() {
            let batch = batch.to_vec().into_boxed_slice();
            self.trace.updates.push(batch.clone());
            let interruption = self.take_interruption(EffectStage::Update, index);
            let selected = match &interruption {
                Some(Interruption::Update { applied_aliases, .. }) => {
                    Some(validated_aliases(applied_aliases, batch.len()))
                }
                _ => None,
            };
            let mut exact = true;
            for (alias, update) in batch.iter().enumerate() {
                if selected.as_ref().is_some_and(|selected| !selected.contains(&alias)) {
                    continue;
                }
                exact &= self.world.apply_effect(&ExternalEffect::Update(update.clone()))
                    == EffectOutcome::Acknowledged;
            }
            if interruption.is_some() || !exact {
                return self.stop(StopReason::Indeterminate);
            }
            self.run_competing_after_batch(EffectStage::Update, index).await?;
        }
        Ok(())
    }
}

fn literal_revision(revision: PublicationRevision) -> LiteralRevision {
    LiteralRevision { head: revision.head(), first_parent: revision.owned_base() }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum EffectOutcome {
    Acknowledged,
    Indeterminate,
    Rejected,
}

impl EffectOutcome {
    fn stop_reason(self) -> StopReason {
        match self {
            Self::Indeterminate => StopReason::Indeterminate,
            Self::Rejected => StopReason::Rejected,
            Self::Acknowledged => panic!("an acknowledged effect cannot stop its attempt"),
        }
    }
}

#[derive(Clone, Debug)]
enum ExternalEffect {
    Tuple(TupleEffect),
    Create(TestCreate),
    Marker(MarkerEffect),
    Update(TestUpdate),
}

impl ExternalEffect {
    fn target(&self) -> GherritPrId {
        match self {
            Self::Tuple(effect) => effect.id.clone(),
            Self::Create(effect) => effect.id.clone(),
            Self::Marker(effect) => effect.id.clone(),
            Self::Update(effect) => effect.id.clone(),
        }
    }
}

impl DurableWorld {
    fn apply_effect(&mut self, effect: &ExternalEffect) -> EffectOutcome {
        let before = self.clone();
        let target = effect.target();
        let outcome = match effect {
            ExternalEffect::Tuple(effect) => self.apply_tuple(effect),
            ExternalEffect::Create(effect) => self.apply_create(effect),
            ExternalEffect::Marker(effect) => self.apply_marker(effect),
            ExternalEffect::Update(effect) => self.apply_update(effect),
        };
        match outcome {
            EffectOutcome::Rejected => {
                assert_eq!(*self, before, "a rejected effect cannot alter durable state");
            }
            EffectOutcome::Acknowledged | EffectOutcome::Indeterminate => {
                self.assert_local_transition(&before, &target, effect);
                self.assert_well_formed();
            }
        }
        outcome
    }

    fn apply_tuple(&mut self, effect: &TupleEffect) -> EffectOutcome {
        match self.changes.get_mut(&effect.id) {
            None => {
                if effect.expected.is_some() || effect.version != 1 {
                    return EffectOutcome::Rejected;
                }
                self.changes.insert(
                    effect.id.clone(),
                    PublishedChange {
                        history: PublishedHistory::new(effect.desired),
                        pull_request: None,
                    },
                );
                EffectOutcome::Acknowledged
            }
            Some(change) => {
                let version_index =
                    effect.version.checked_sub(1).and_then(|index| usize::try_from(index).ok());
                if change.history.last() == effect.desired
                    && version_index.and_then(|index| change.history.get(index))
                        == Some(effect.desired)
                {
                    return EffectOutcome::Acknowledged;
                }
                if effect.expected != Some(change.history.last())
                    || effect.version != u64::try_from(change.history.len()).unwrap() + 1
                {
                    return EffectOutcome::Rejected;
                }
                change.history.push(effect.desired);
                EffectOutcome::Acknowledged
            }
        }
    }

    fn apply_public_branch(&mut self, effect: &PublicBranchEffect) -> EffectOutcome {
        let before = self.clone();
        let current = self.public_branches.get(&effect.branch).copied();
        let outcome = if current == Some(effect.desired) {
            // Git reports an up-to-date source/destination pair as success even
            // when an earlier absence or old-value lease has become stale.
            EffectOutcome::Acknowledged
        } else if current == effect.expected {
            self.public_branches.insert(effect.branch.clone(), effect.desired);
            EffectOutcome::Acknowledged
        } else {
            EffectOutcome::Rejected
        };

        if outcome == EffectOutcome::Rejected {
            assert_eq!(*self, before, "a rejected public branch effect cannot alter state");
            return outcome;
        }
        assert_eq!(self.default_tip, before.default_tip);
        assert_eq!(self.changes, before.changes);
        assert_eq!(self.next_identity, before.next_identity);
        assert_eq!(self.public_branches.get(&effect.branch), Some(&effect.desired));
        for (branch, oid) in &before.public_branches {
            if branch != &effect.branch {
                assert_eq!(self.public_branches.get(branch), Some(oid));
            }
        }
        self.assert_well_formed();
        outcome
    }

    fn apply_create(&mut self, effect: &TestCreate) -> EffectOutcome {
        let current = self
            .published(&effect.id)
            .expect("a planned create follows acknowledged tuple publication");
        if current.pull_request.is_some() {
            return EffectOutcome::Rejected;
        }
        let current = current.history.last();
        let identity = self.allocate_identity();
        self.published_mut(&effect.id).unwrap().pull_request =
            Some(ManagedPullRequest::Unmarked(PullRequest {
                identity,
                title: effect.title.clone(),
                body: effect.body.clone(),
            }));
        if effect.head_oid == current.head && effect.base_oid == current.first_parent {
            EffectOutcome::Acknowledged
        } else {
            EffectOutcome::Indeterminate
        }
    }

    fn apply_marker(&mut self, effect: &MarkerEffect) -> EffectOutcome {
        let change =
            self.published_mut(&effect.id).expect("a planned marker has published history");
        if !change.history.contains_head(effect.target) {
            return EffectOutcome::Rejected;
        }
        let Some(pull_request) = change.pull_request.take() else {
            return EffectOutcome::Rejected;
        };
        match pull_request {
            ManagedPullRequest::Unmarked(pull_request) => {
                change.pull_request = Some(ManagedPullRequest::Marked {
                    pull_request,
                    target: effect.target,
                    base: BaseKind::Owned,
                });
                EffectOutcome::Acknowledged
            }
            marked @ ManagedPullRequest::Marked { target, .. } => {
                change.pull_request = Some(marked);
                if target == effect.target {
                    EffectOutcome::Acknowledged
                } else {
                    EffectOutcome::Rejected
                }
            }
        }
    }

    fn apply_update(&mut self, effect: &TestUpdate) -> EffectOutcome {
        let Some(change) = self.changes.get_mut(&effect.id) else {
            return EffectOutcome::Rejected;
        };
        let Some(managed) = change.pull_request.as_mut() else {
            return EffectOutcome::Rejected;
        };
        if managed.pull_request().identity != effect.identity {
            return EffectOutcome::Rejected;
        }
        let base = match effect.base_branch.as_deref() {
            None => None,
            Some(DEFAULT_BRANCH) => Some(BaseKind::Default),
            Some(base) if base == owned_base_name(&effect.id) => Some(BaseKind::Owned),
            Some(_) => return EffectOutcome::Rejected,
        };
        let ManagedPullRequest::Marked { pull_request, base: current_base, .. } = managed else {
            return EffectOutcome::Rejected;
        };
        if let Some(base) = base {
            *current_base = base;
        }
        if let Some(title) = &effect.title {
            pull_request.title.clone_from(title);
        }
        if let Some(body) = &effect.body {
            pull_request.body.clone_from(body);
        }
        EffectOutcome::Acknowledged
    }

    fn assert_local_transition(
        &self,
        before: &Self,
        target: &GherritPrId,
        effect: &ExternalEffect,
    ) {
        assert_eq!(self.default_tip, before.default_tip, "publication cannot move the default");
        assert_eq!(
            self.next_identity,
            before.next_identity + u32::from(matches!(effect, ExternalEffect::Create(_))),
            "only one durably applied create allocates exactly one identity"
        );
        let inserted = !before.changes.contains_key(target);
        assert_eq!(
            self.changes.len(),
            before.changes.len() + usize::from(inserted),
            "one tuple can publish only its exact target"
        );
        assert!(!inserted || matches!(effect, ExternalEffect::Tuple(_)));
        for (id, old) in &before.changes {
            let new = self.changes.get(id).expect("a published change cannot disappear");
            if id != target {
                assert_eq!(old, new, "one exact-local effect cannot mutate another change");
                continue;
            }
            assert!(
                new.history.len() >= old.history.len(),
                "immutable version history cannot shrink"
            );
            assert!(
                new.history.iter().zip(old.history.iter()).all(|(new, old)| new == old),
                "immutable version history cannot be rewritten"
            );
            if new.history != old.history {
                assert!(matches!(effect, ExternalEffect::Tuple(_)), "only a tuple adds history");
            }
            match (&old.pull_request, &new.pull_request) {
                (Some(old), Some(new)) => {
                    assert_eq!(
                        old.pull_request().identity,
                        new.pull_request().identity,
                        "an OPEN identity cannot be replaced"
                    );
                    if old.marker().is_some() {
                        assert_eq!(old.marker(), new.marker(), "an immutable marker cannot move");
                    }
                    if old.marker() != new.marker() {
                        assert!(
                            matches!(effect, ExternalEffect::Marker(_)),
                            "only marker publication can establish a marker"
                        );
                    }
                    if old.base() != new.base()
                        || old.pull_request().title != new.pull_request().title
                        || old.pull_request().body != new.pull_request().body
                    {
                        assert!(
                            matches!(effect, ExternalEffect::Update(_)),
                            "only a projection update changes OPEN fields"
                        );
                    }
                }
                (Some(_), None) => panic!("an OPEN pull request cannot disappear"),
                (None, Some(_)) => assert!(
                    matches!(effect, ExternalEffect::Create(_)),
                    "only create can establish an OPEN pull request"
                ),
                (None, None) => {}
            }
        }
        for id in self.changes.keys() {
            assert!(before.changes.contains_key(id) || id == target, "only the target can publish");
        }
    }
}

fn owned_base_name(id: &GherritPrId) -> String {
    format!("gherrit-bases/{}", id.as_str())
}

fn apply_atomic_git_batch<T: Clone>(
    world: &mut DurableWorld,
    batch: &[T],
    wrap: impl Fn(T) -> ExternalEffect,
) -> EffectOutcome {
    let before = world.clone();
    for effect in batch.iter().cloned().map(wrap) {
        if world.apply_effect(&effect) != EffectOutcome::Acknowledged {
            *world = before;
            return EffectOutcome::Rejected;
        }
    }
    EffectOutcome::Acknowledged
}

fn apply_atomic_initial_ref_batch(
    world: &mut DurableWorld,
    batch: &[InitialRefEffect],
) -> EffectOutcome {
    let before = world.clone();
    for effect in batch {
        let outcome = match effect {
            InitialRefEffect::Tuple(effect) => {
                world.apply_effect(&ExternalEffect::Tuple(effect.clone()))
            }
            InitialRefEffect::PublicBranch(effect) => world.apply_public_branch(effect),
        };
        if outcome != EffectOutcome::Acknowledged {
            *world = before;
            return EffectOutcome::Rejected;
        }
    }
    EffectOutcome::Acknowledged
}

fn flatten<T: Clone>(batches: &[Box<[T]>]) -> Box<[T]> {
    batches.iter().flat_map(|batch| batch.iter().cloned()).collect()
}

async fn assert_restart_converges(
    mut world: DurableWorld,
    intent: &LocalIntent,
    label: &str,
) -> DurableWorld {
    let retry = world
        .run_attempt(intent, &QueryVisibility::default(), None)
        .await
        .unwrap_or_else(|error| panic!("{label}: fresh planning failed: {error}"));
    assert_eq!(
        retry.outcome,
        AttemptOutcome::Acknowledged,
        "{label}: a fresh stable-intent attempt must receive usable acknowledgements"
    );
    let done = world
        .run_attempt(intent, &QueryVisibility::default(), None)
        .await
        .unwrap_or_else(|error| panic!("{label}: final observation failed: {error}"));
    assert!(done.trace.is_empty(), "{label}: the next fresh attempt must have no durable work");
    world
}

fn two_change_intent() -> LocalIntent {
    let default = oid(10);
    let root = LiteralRevision { head: oid(20), first_parent: default };
    let child = LiteralRevision { head: oid(21), first_parent: root.head };
    LocalIntent::new(
        default,
        [
            local_change("Groot", root, "Root title", "Root body"),
            local_change("Gchild", child, "Child title", "Child body"),
        ],
    )
}

fn many_bounded_ids_intent(change_count: usize, id_bytes: usize) -> LocalIntent {
    assert!((8..=250).contains(&id_bytes));
    let default = oid(1);
    let mut first_parent = default;
    LocalIntent::new(
        default,
        (0..change_count).map(|index| {
            let head = oid(u16::try_from(index + 2).unwrap());
            let prefix = format!("G{index:04}");
            let suffix = "a".repeat(id_bytes - prefix.len());
            let change = local_change(
                &format!("{prefix}{suffix}"),
                LiteralRevision { head, first_parent },
                &format!("Change {index}"),
                &format!("Body for change {index}"),
            );
            first_parent = head;
            change
        }),
    )
}

fn multi_request_intent() -> LocalIntent {
    const BODY_BYTES_BEFORE_JSON_ESCAPING: usize = 90_000;

    let default = oid(10);
    let body = "\u{1}".repeat(BODY_BYTES_BEFORE_JSON_ESCAPING);
    let mut first_parent = default;
    LocalIntent::new(
        default,
        (0..3).map(|index| {
            let head = oid(u16::try_from(index + 20).unwrap());
            let change = local_change(
                &format!("G{index}"),
                LiteralRevision { head, first_parent },
                &format!("Change {index}"),
                &body,
            );
            first_parent = head;
            change
        }),
    )
}

#[derive(serde::Serialize)]
struct UpdateSnapshot {
    id: String,
    number: u32,
    node_id: String,
    title: Option<String>,
    body: Option<Box<[String]>>,
    base_branch: Option<String>,
}

fn update_snapshot(trace: &AttemptTrace) -> Vec<UpdateSnapshot> {
    flatten(&trace.updates)
        .iter()
        .map(|update| UpdateSnapshot {
            id: update.id.as_str().to_owned(),
            number: update.identity.number().get(),
            node_id: update.identity.node_id_for_test().to_owned(),
            title: update.title.clone(),
            body: update.body.as_deref().map(|body| body.split('\n').map(str::to_owned).collect()),
            base_branch: update.base_branch.clone(),
        })
        .collect()
}

#[derive(serde::Serialize)]
struct UpdateAlternatives {
    publisher_a: Vec<UpdateSnapshot>,
    publisher_b: Vec<UpdateSnapshot>,
}

fn establish_marked(world: &mut DurableWorld, change: &LocalChange, base: BaseKind, body: &str) {
    world.publish_for_setup(&change.id, change.desired);
    world.open_for_setup(&change.id, &change.title, body);
    world.mark_for_setup(&change.id, change.desired.head, base);
}

fn assert_same_protocol_state_ignoring_identities(left: &DurableWorld, right: &DurableWorld) {
    assert_eq!(left.default_tip, right.default_tip);
    assert_eq!(left.changes.len(), right.changes.len());
    for (id, left_change) in &left.changes {
        let right_change = right.changes.get(id).expect("both worlds publish the same IDs");
        assert_eq!(left_change.history, right_change.history);
        match (&left_change.pull_request, &right_change.pull_request) {
            (None, None) => {}
            (Some(left), Some(right)) => {
                assert_eq!(left.marker(), right.marker());
                assert_eq!(left.base(), right.base());
                assert_eq!(left.pull_request().title, right.pull_request().title);
                // Generated navigation embeds dynamically allocated PR
                // numbers, so bodies need not commute literally.
            }
            _ => panic!("both worlds have the same OPEN-row presence"),
        }
    }
}

#[tokio::test]
async fn every_small_attempt_prefix_restarts_from_literal_durable_state() {
    let intent = two_change_intent();
    let initial = DurableWorld::for_intents(oid(10), &[&intent]);
    let mut completed = initial.clone();
    let report = completed.run_attempt(&intent, &QueryVisibility::default(), None).await.unwrap();
    assert_eq!(report.outcome, AttemptOutcome::Acknowledged);
    assert_eq!(report.trace.tuples.iter().map(|batch| batch.len()).collect::<Vec<_>>(), [2]);
    assert_eq!(
        report.trace.creates.as_ref().unwrap().iter().map(|batch| batch.len()).collect::<Vec<_>>(),
        [2]
    );
    assert_eq!(report.trace.markers.iter().map(|batch| batch.len()).collect::<Vec<_>>(), [2]);
    assert_eq!(report.trace.updates.iter().map(|batch| batch.len()).collect::<Vec<_>>(), [2]);
    insta::assert_yaml_snapshot!("two_change_final_updates", update_snapshot(&report.trace));

    let mut misbound = flatten(&report.trace.updates)[0].clone();
    assert_eq!(misbound.id, id("Groot"));
    misbound.identity = completed.identity(&id("Gchild"));
    let before_misbound = completed.clone();
    assert_eq!(
        completed.apply_effect(&ExternalEffect::Update(misbound)),
        EffectOutcome::Rejected,
        "a stable change ID cannot be rebound through another row's identity"
    );
    assert_eq!(completed, before_misbound);

    let interruptions = [
        Interruption::InitialRefs { batch: 0, applied: false },
        Interruption::InitialRefs { batch: 0, applied: true },
        Interruption::Create { batch: 0, applied_aliases: Box::new([]) },
        Interruption::Create { batch: 0, applied_aliases: Box::new([0]) },
        Interruption::Create { batch: 0, applied_aliases: Box::new([1]) },
        Interruption::Create { batch: 0, applied_aliases: Box::new([0, 1]) },
        Interruption::Marker { batch: 0, applied: false },
        Interruption::Marker { batch: 0, applied: true },
        Interruption::Update { batch: 0, applied_aliases: Box::new([]) },
        Interruption::Update { batch: 0, applied_aliases: Box::new([0]) },
        Interruption::Update { batch: 0, applied_aliases: Box::new([1]) },
        Interruption::Update { batch: 0, applied_aliases: Box::new([0, 1]) },
    ];
    for (index, interruption) in interruptions.into_iter().enumerate() {
        let mut interrupted = initial.clone();
        let stopped = interrupted
            .run_attempt(&intent, &QueryVisibility::default(), Some(interruption.clone()))
            .await
            .unwrap();
        assert_eq!(stopped.outcome, AttemptOutcome::Stopped(StopReason::Indeterminate));
        if matches!(interruption, Interruption::Create { .. }) {
            assert!(interrupted.changes.values().all(|change| {
                change
                    .pull_request
                    .as_ref()
                    .is_none_or(|pull_request| pull_request.marker().is_none())
            }));
            assert!(stopped.trace.markers.is_empty() && stopped.trace.updates.is_empty());
        }
        assert_restart_converges(interrupted, &intent, &format!("prefix-{index}")).await;
    }

    let done = completed.run_attempt(&intent, &QueryVisibility::default(), None).await.unwrap();
    assert!(done.trace.is_empty());
}

#[tokio::test]
async fn git_restarts_expose_only_complete_atomic_batch_prefixes() {
    let tuple_intent = many_bounded_ids_intent(10, 250);
    let tuple_initial = DurableWorld::for_intents(oid(1), &[&tuple_intent]);
    let mut tuple_complete = tuple_initial.clone();
    let tuple_report =
        tuple_complete.run_attempt(&tuple_intent, &QueryVisibility::default(), None).await.unwrap();
    assert!(tuple_report.trace.tuples.len() > 1, "the fixture crosses the tuple push budget");
    for stopped_batch in 0..tuple_report.trace.tuples.len() {
        for applied in [false, true] {
            let mut world = tuple_initial.clone();
            let stopped = world
                .run_attempt(
                    &tuple_intent,
                    &QueryVisibility::default(),
                    Some(Interruption::InitialRefs { batch: stopped_batch, applied }),
                )
                .await
                .unwrap();
            assert_eq!(stopped.trace.tuples, tuple_report.trace.tuples[..=stopped_batch]);
            let retry =
                world.run_attempt(&tuple_intent, &QueryVisibility::default(), None).await.unwrap();
            assert_eq!(
                flatten(&retry.trace.tuples),
                flatten(&tuple_report.trace.tuples[stopped_batch + usize::from(applied)..]),
                "a tuple retry retains the exact unpublished atomic-batch suffix"
            );
        }
    }

    let marker_intent = many_bounded_ids_intent(28, 250);
    let mut before_markers = DurableWorld::for_intents(oid(1), &[&marker_intent]);
    for local in marker_intent.iter() {
        before_markers.publish_for_setup(&local.id, local.desired);
        before_markers.open_for_setup(&local.id, &local.title, "provisional");
    }
    let mut marker_complete = before_markers.clone();
    let marker_report = marker_complete
        .run_attempt(&marker_intent, &QueryVisibility::default(), None)
        .await
        .unwrap();
    assert!(marker_report.trace.tuples.is_empty());
    assert!(marker_report.trace.creates.is_none());
    assert!(marker_report.trace.markers.len() > 1, "the fixture crosses the marker push budget");
    for stopped_batch in 0..marker_report.trace.markers.len() {
        for applied in [false, true] {
            let mut world = before_markers.clone();
            let stopped = world
                .run_attempt(
                    &marker_intent,
                    &QueryVisibility::default(),
                    Some(Interruption::Marker { batch: stopped_batch, applied }),
                )
                .await
                .unwrap();
            assert_eq!(stopped.trace.markers, marker_report.trace.markers[..=stopped_batch]);
            let retry =
                world.run_attempt(&marker_intent, &QueryVisibility::default(), None).await.unwrap();
            assert_eq!(
                flatten(&retry.trace.markers),
                flatten(&marker_report.trace.markers[stopped_batch + usize::from(applied)..]),
                "a marker retry retains the exact unpublished atomic-batch suffix"
            );
        }
    }

    let mixed = LocalIntent::new(
        oid(1),
        [
            local_change(
                "GleaseA",
                LiteralRevision { head: oid(300), first_parent: oid(1) },
                "A",
                "A",
            ),
            local_change(
                "GleaseB",
                LiteralRevision { head: oid(301), first_parent: oid(300) },
                "B",
                "B",
            ),
            local_change(
                "GleaseC",
                LiteralRevision { head: oid(302), first_parent: oid(301) },
                "C",
                "C",
            ),
        ],
    );
    let initial = DurableWorld::for_intents(oid(1), &[&mixed]);
    let stale_plan = initial.plan(&mixed, &QueryVisibility::default()).unwrap();
    let mut raced = initial;
    let conflicting = LiteralRevision { head: oid(399), first_parent: oid(300) };
    raced.publish_for_setup(&id("GleaseB"), conflicting);
    let before = raced.clone();
    let rejected = raced.execute_plan(stale_plan, None).await.unwrap();
    assert_eq!(rejected.outcome, AttemptOutcome::Stopped(StopReason::Rejected));
    assert_eq!(raced, before, "one conflicting lease rolls back the whole mixed Git batch");
    assert!(raced.published(&id("GleaseA")).is_none());
    assert!(raced.published(&id("GleaseC")).is_none());
}

#[tokio::test]
async fn graphql_restarts_stop_after_an_indeterminate_current_request() {
    let intent = multi_request_intent();
    let initial = DurableWorld::for_intents(oid(10), &[&intent]);
    let mut complete = initial.clone();
    let completed = complete.run_attempt(&intent, &QueryVisibility::default(), None).await.unwrap();
    let creates = completed.trace.creates.as_ref().unwrap();
    assert_eq!(
        creates.iter().map(|batch| batch.len()).collect::<Vec<_>>(),
        [1, 1, 1],
        "the fixture uses the production serialized-byte request boundary"
    );
    assert_eq!(
        completed.trace.updates.iter().map(|batch| batch.len()).collect::<Vec<_>>(),
        [1, 1, 1],
        "create and update serialization preserve the same byte boundary"
    );

    let mut interrupted_create = initial.clone();
    let stopped = interrupted_create
        .run_attempt(
            &intent,
            &QueryVisibility::default(),
            Some(Interruption::Create { batch: 1, applied_aliases: Box::new([0]) }),
        )
        .await
        .unwrap();
    assert_eq!(stopped.outcome, AttemptOutcome::Stopped(StopReason::Indeterminate));
    assert_eq!(
        stopped.trace.creates.as_ref().unwrap().iter().map(|batch| batch.len()).collect::<Vec<_>>(),
        [1, 1],
        "an earlier request is acknowledged, the current response is lost, and the tail is unsent"
    );
    assert!(stopped.trace.markers.is_empty() && stopped.trace.updates.is_empty());
    assert!(interrupted_create.open_pull_request(&creates[0][0].id).is_some());
    assert!(interrupted_create.open_pull_request(&creates[1][0].id).is_some());
    assert!(interrupted_create.open_pull_request(&creates[2][0].id).is_none());
    assert_restart_converges(interrupted_create, &intent, "multi-request-create").await;

    let mut before_updates = DurableWorld::for_intents(oid(10), &[&intent]);
    for (index, local) in intent.iter().enumerate() {
        establish_marked(
            &mut before_updates,
            local,
            if index == 0 { BaseKind::Default } else { BaseKind::Owned },
            "provisional",
        );
    }
    let stopped = before_updates
        .run_attempt(
            &intent,
            &QueryVisibility::default(),
            Some(Interruption::Update { batch: 1, applied_aliases: Box::new([0]) }),
        )
        .await
        .unwrap();
    assert_eq!(stopped.trace.updates.iter().map(|batch| batch.len()).collect::<Vec<_>>(), [1, 1]);
    for (index, local) in intent.iter().enumerate() {
        let body = &before_updates.open_pull_request(&local.id).unwrap().pull_request().body;
        if index < 2 {
            assert_ne!(body, "provisional");
        } else {
            assert_eq!(body, "provisional");
        }
    }
    assert_restart_converges(before_updates, &intent, "multi-request-update").await;

    let duplicate_intent = two_change_intent();
    let duplicate_initial = DurableWorld::for_intents(oid(10), &[&duplicate_intent]);
    let stale_plan =
        duplicate_initial.plan(&duplicate_intent, &QueryVisibility::default()).unwrap();
    let mut duplicate_world = duplicate_initial;
    let first = duplicate_world
        .run_attempt(
            &duplicate_intent,
            &QueryVisibility::default(),
            Some(Interruption::Create { batch: 0, applied_aliases: Box::new([0]) }),
        )
        .await
        .unwrap();
    assert_eq!(first.outcome, AttemptOutcome::Stopped(StopReason::Indeterminate));
    let duplicate_creates = first.trace.creates.as_ref().unwrap();
    let retained_identity = duplicate_world.identity(&duplicate_creates[0][0].id);
    let next_before = duplicate_world.next_identity;
    let duplicate = duplicate_world.execute_plan(stale_plan, None).await.unwrap();
    assert_eq!(duplicate.outcome, AttemptOutcome::Stopped(StopReason::Indeterminate));
    assert_eq!(duplicate.trace.creates.as_ref().unwrap().len(), 1);
    assert!(duplicate.trace.markers.is_empty() && duplicate.trace.updates.is_empty());
    assert_eq!(duplicate_world.identity(&duplicate_creates[0][0].id), retained_identity);
    assert!(duplicate_world.open_pull_request(&duplicate_creates[0][1].id).is_some());
    assert_eq!(duplicate_world.next_identity, next_before + 1, "only the sibling create allocates");
    assert_restart_converges(duplicate_world, &duplicate_intent, "mixed-duplicate-create").await;
}

#[tokio::test]
async fn hidden_open_rows_retry_only_the_unmarked_stable_create_key() {
    let default = oid(1);
    let revision = LiteralRevision { head: oid(2), first_parent: default };
    let intent = root_intent(default, "Ghidden", revision);
    let mut world = DurableWorld::for_intents(default, &[&intent]);
    let first = world
        .run_attempt(
            &intent,
            &QueryVisibility::default(),
            Some(Interruption::Create { batch: 0, applied_aliases: Box::new([0]) }),
        )
        .await
        .unwrap();
    assert_eq!(first.outcome, AttemptOutcome::Stopped(StopReason::Indeterminate));

    let hidden = QueryVisibility::hiding(&world, [id("Ghidden")]);
    let before_duplicate = world.clone();
    let duplicate = world.run_attempt(&intent, &hidden, None).await.unwrap();
    assert!(duplicate.trace.tuples.is_empty());
    assert_eq!(
        duplicate.trace.creates, first.trace.creates,
        "the stable create payload is identical"
    );
    assert_eq!(
        duplicate.outcome,
        AttemptOutcome::Stopped(StopReason::Indeterminate),
        "a duplicate GraphQL alias makes the request indeterminate"
    );
    assert_eq!(world, before_duplicate);
    assert!(world.open_pull_request(&id("Ghidden")).unwrap().marker().is_none());

    let world = assert_restart_converges(world, &intent, "hidden-unmarked").await;
    let hidden_marked = QueryVisibility::hiding(&world, [id("Ghidden")]);
    let error = world.plan(&intent, &hidden_marked).err().unwrap();
    assert!(error.to_string().contains("marker"));
    assert!(error.to_string().contains("no OPEN pull request"));
}

#[tokio::test]
async fn stale_open_oids_and_body_bytes_do_not_advance_with_durable_writes() {
    let default = oid(1);
    let old_root = LiteralRevision { head: oid(10), first_parent: default };
    let old_child = LiteralRevision { head: oid(20), first_parent: old_root.head };
    let new_root = LiteralRevision { head: oid(11), first_parent: default };
    let new_child = LiteralRevision { head: oid(21), first_parent: new_root.head };
    let old_intent = LocalIntent::new(
        default,
        [
            local_change("Groot", old_root, "Old root", "Old root body"),
            local_change("Gchild", old_child, "Old child", "Old child body"),
        ],
    );
    let new_intent = LocalIntent::new(
        default,
        [
            local_change("Groot", new_root, "Amended root", "New root body"),
            local_change("Gchild", new_child, "Rebased child", "New child body"),
        ],
    );
    let mut world = DurableWorld::for_intents(default, &[&old_intent, &new_intent]);
    for (index, local) in old_intent.iter().enumerate() {
        world.publish_for_setup(&local.id, local.desired);
        world.open_for_setup(&local.id, &local.title, &local.commit_body);
        world.mark_for_setup(
            &local.id,
            local.desired.head,
            if index == 0 { BaseKind::Default } else { BaseKind::Owned },
        );
    }
    let stale = QueryVisibility::stale(&world, [id("Groot"), id("Gchild")]);
    let attempt = world.run_attempt(&new_intent, &stale, None).await.unwrap();
    assert_eq!(attempt.outcome, AttemptOutcome::Acknowledged);
    assert_eq!(
        flatten(&attempt.trace.tuples)
            .iter()
            .map(|tuple| (tuple.id.clone(), tuple.version))
            .collect::<Vec<_>>(),
        [(id("Groot"), 2), (id("Gchild"), 2)]
    );
    assert!(attempt.trace.markers.is_empty(), "older durable markers remain authoritative");
    assert!(flatten(&attempt.trace.updates).iter().all(|update| update.base_branch.is_none()));
    insta::assert_yaml_snapshot!("stale_amend_rebase_updates", update_snapshot(&attempt.trace));

    let stale_retry = world.run_attempt(&new_intent, &stale, None).await.unwrap();
    assert!(stale_retry.trace.tuples.is_empty());
    assert_eq!(
        flatten(&stale_retry.trace.updates),
        flatten(&attempt.trace.updates),
        "neither tuple publication nor updates mutate an already-returned query view"
    );
    assert!(
        world
            .run_attempt(&new_intent, &QueryVisibility::default(), None)
            .await
            .unwrap()
            .trace
            .is_empty(),
        "an explicitly current observation sees convergence"
    );
}

#[tokio::test]
async fn independently_lagged_head_and_base_history_slots_are_safe() {
    let default = oid(1);
    let old_root = LiteralRevision { head: oid(40), first_parent: default };
    let old_child = LiteralRevision { head: oid(41), first_parent: old_root.head };
    let new_root = LiteralRevision { head: oid(42), first_parent: default };
    let new_child = LiteralRevision { head: oid(43), first_parent: new_root.head };
    let old_intent = LocalIntent::new(
        default,
        [
            local_change("Gslotroot", old_root, "Old root", "Old root body"),
            local_change("Gslotchild", old_child, "Old child", "Old child body"),
        ],
    );
    let new_intent = LocalIntent::new(
        default,
        [
            local_change("Gslotroot", new_root, "New root", "New root body"),
            local_change("Gslotchild", new_child, "New child", "New child body"),
        ],
    );
    let mut initial = DurableWorld::for_intents(default, &[&old_intent, &new_intent]);
    for (old, new, base) in [
        (&old_intent.first, &new_intent.first, BaseKind::Default),
        (&old_intent.later[0], &new_intent.later[0], BaseKind::Owned),
    ] {
        initial.publish_for_setup(&old.id, old.desired);
        initial.publish_for_setup(&new.id, new.desired);
        initial.open_for_setup(&old.id, &old.title, "provisional");
        initial.mark_for_setup(&old.id, old.desired.head, base);
    }

    for head_slot in 0..2 {
        for base_slot in 0..2 {
            let visibility =
                QueryVisibility::historical_slots(&initial, id("Gslotchild"), head_slot, base_slot);
            let mut world = initial.clone();
            let report = world.run_attempt(&new_intent, &visibility, None).await.unwrap();
            assert_eq!(report.outcome, AttemptOutcome::Acknowledged);
            assert!(report.trace.tuples.is_empty());
            assert!(report.trace.creates.is_none());
            assert!(report.trace.markers.is_empty());
            assert_eq!(
                world.published(&id("Gslotchild")).unwrap().history,
                initial.published(&id("Gslotchild")).unwrap().history,
                "query lag cannot create immutable history"
            );
            assert!(
                world
                    .run_attempt(&new_intent, &QueryVisibility::default(), None)
                    .await
                    .unwrap()
                    .trace
                    .is_empty()
            );
        }
    }
}

#[tokio::test]
async fn reorder_keeps_old_markers_and_projects_exact_new_stack_positions() {
    let default = oid(1);
    let old_root = LiteralRevision { head: oid(20), first_parent: default };
    let new_root = LiteralRevision { head: oid(10), first_parent: default };
    let moved_child = LiteralRevision { head: oid(21), first_parent: new_root.head };
    let old_intent = root_intent(default, "Gmove", old_root);
    let new_intent = LocalIntent::new(
        default,
        [
            local_change("Gnew", new_root, "New root", "New root body"),
            local_change("Gmove", moved_child, "Moved child", "Moved child body"),
        ],
    );
    let mut world = DurableWorld::for_intents(default, &[&old_intent, &new_intent]);
    world.publish_for_setup(&id("Gmove"), old_root);
    world.open_for_setup(&id("Gmove"), "Old root", "Old root body");
    world.mark_for_setup(&id("Gmove"), old_root.head, BaseKind::Default);
    let stale = QueryVisibility::stale(&world, [id("Gmove")]);

    let attempt = world.run_attempt(&new_intent, &stale, None).await.unwrap();
    assert_eq!(
        flatten(&attempt.trace.tuples).iter().map(|tuple| tuple.version).collect::<Vec<_>>(),
        [1, 2]
    );
    assert_eq!(
        attempt
            .trace
            .creates
            .as_ref()
            .unwrap()
            .iter()
            .flatten()
            .map(|create| create.id.clone())
            .collect::<Vec<_>>(),
        [id("Gnew")]
    );
    assert_eq!(
        flatten(&attempt.trace.markers).iter().map(|marker| marker.id.clone()).collect::<Vec<_>>(),
        [id("Gnew")]
    );
    insta::assert_yaml_snapshot!("reorder_exact_final_updates", update_snapshot(&attempt.trace));
    assert_eq!(
        world.open_pull_request(&id("Gmove")).unwrap().marker(),
        Some(old_root.head),
        "the immutable marker remains on the older published version"
    );
    assert!(
        world
            .run_attempt(&new_intent, &QueryVisibility::default(), None)
            .await
            .unwrap()
            .trace
            .is_empty()
    );
}

#[tokio::test]
async fn competing_publishers_resume_across_every_authority_barrier() {
    let default = oid(1);
    let old_revision = LiteralRevision { head: oid(10), first_parent: default };
    let new_revision = LiteralRevision { head: oid(11), first_parent: default };
    let old_intent = root_intent(default, "Gtuplecreate", old_revision);
    let new_intent = root_intent(default, "Gtuplecreate", new_revision);
    let initial = DurableWorld::for_intents(default, &[&old_intent, &new_intent]);

    let primary = initial.plan(&old_intent, &QueryVisibility::default()).unwrap();
    let mut post_primary_tuple = initial.clone();
    let preview = post_primary_tuple
        .run_attempt(
            &old_intent,
            &QueryVisibility::default(),
            Some(Interruption::Create { batch: 0, applied_aliases: Box::new([]) }),
        )
        .await
        .unwrap();
    assert_eq!(flatten(&preview.trace.tuples).len(), 1);
    let competing = post_primary_tuple.plan(&new_intent, &QueryVisibility::default()).unwrap();

    let mut world = initial;
    let (primary, competing) = execute_interleaved(
        &mut world,
        primary,
        competing,
        EffectStage::InitialRefs,
        Some(Interruption::Create { batch: 0, applied_aliases: Box::new([]) }),
    )
    .await
    .unwrap();
    assert_eq!(competing.outcome, AttemptOutcome::Stopped(StopReason::Indeterminate));
    assert_eq!(flatten(&competing.trace.tuples).len(), 1);
    assert!(competing.trace.markers.is_empty() && competing.trace.updates.is_empty());
    assert_eq!(
        primary.outcome,
        AttemptOutcome::Stopped(StopReason::Indeterminate),
        "the suspended create lands, but its stale object IDs cannot release a marker"
    );
    assert!(primary.trace.markers.is_empty() && primary.trace.updates.is_empty());
    assert_eq!(
        world.published(&id("Gtuplecreate")).unwrap().history.iter().copied().collect::<Vec<_>>(),
        [old_revision, new_revision]
    );
    assert_eq!(world.open_pull_request(&id("Gtuplecreate")).unwrap().marker(), None);
    assert_restart_converges(world, &new_intent, "tuple-create-interleaving").await;

    let intent = root_intent(
        default,
        "Gcreatemarker",
        LiteralRevision { head: oid(20), first_parent: default },
    );
    let initial = DurableWorld::for_intents(default, &[&intent]);
    let primary = initial.plan(&intent, &QueryVisibility::default()).unwrap();
    let competing = initial.plan(&intent, &QueryVisibility::default()).unwrap();
    let mut world = initial;
    let (primary, competing) =
        execute_interleaved(&mut world, primary, competing, EffectStage::Create, None)
            .await
            .unwrap();
    assert_eq!(primary.outcome, AttemptOutcome::Acknowledged);
    assert_eq!(
        competing.outcome,
        AttemptOutcome::Stopped(StopReason::Indeterminate),
        "the competing stable-key create cannot consume the primary receipt"
    );
    assert!(competing.trace.markers.is_empty() && competing.trace.updates.is_empty());
    assert!(
        flatten(&primary.trace.markers).len() == 1 && flatten(&primary.trace.updates).len() == 1
    );
    assert!(
        world
            .run_attempt(&intent, &QueryVisibility::default(), None)
            .await
            .unwrap()
            .trace
            .is_empty()
    );

    let root = LiteralRevision { head: oid(30), first_parent: default };
    let a_child = LiteralRevision { head: oid(31), first_parent: root.head };
    let b_child = LiteralRevision { head: oid(32), first_parent: root.head };
    let root_change = local_change("Gbarrierroot", root, "Root", "Root body");
    let a_intent = LocalIntent::new(
        default,
        [root_change.clone(), local_change("Gbarriera", a_child, "A", "A body")],
    );
    let b_intent =
        LocalIntent::new(default, [root_change, local_change("Gbarrierb", b_child, "B", "B body")]);
    let mut world = DurableWorld::for_intents(default, &[&a_intent, &b_intent]);
    for change in [&a_intent.first, &a_intent.later[0], &b_intent.later[0]] {
        world.publish_for_setup(&change.id, change.desired);
        world.open_for_setup(&change.id, &change.title, "provisional");
    }
    let primary = world.plan(&a_intent, &QueryVisibility::default()).unwrap();
    let competing = world.plan(&b_intent, &QueryVisibility::default()).unwrap();
    let (primary, competing) =
        execute_interleaved(&mut world, primary, competing, EffectStage::Marker, None)
            .await
            .unwrap();
    assert_eq!(primary.outcome, AttemptOutcome::Acknowledged);
    assert_eq!(competing.outcome, AttemptOutcome::Acknowledged);
    assert_eq!(flatten(&primary.trace.markers).len(), 2);
    assert_eq!(flatten(&competing.trace.markers).len(), 2);
    let root_identity = world.identity(&id("Gbarrierroot"));
    let primary_root_body = flatten(&primary.trace.updates)
        .iter()
        .find(|update| update.identity == root_identity)
        .and_then(|update| update.body.clone())
        .unwrap();
    assert_eq!(
        &world.open_pull_request(&id("Gbarrierroot")).unwrap().pull_request().body,
        &primary_root_body,
        "the resumed primary projection is the last complete alias"
    );
    assert!(
        world
            .run_attempt(&a_intent, &QueryVisibility::default(), None)
            .await
            .unwrap()
            .trace
            .is_empty()
    );
}

#[tokio::test]
async fn competing_publishers_interleave_between_tuple_batches() {
    let mut competing_tuple_intent = many_bounded_ids_intent(10, 250);
    let primary_tuple_intent = competing_tuple_intent.clone();
    let competing_last = &mut competing_tuple_intent.later.last_mut().unwrap().desired;
    competing_last.head = oid(500);
    let initial =
        DurableWorld::for_intents(oid(1), &[&primary_tuple_intent, &competing_tuple_intent]);
    let primary = initial.plan(&primary_tuple_intent, &QueryVisibility::default()).unwrap();
    let competing = initial.plan(&competing_tuple_intent, &QueryVisibility::default()).unwrap();
    let mut world = initial;
    let (primary, competing) = execute_batch_interleaved(
        &mut world,
        primary,
        competing,
        EffectStage::InitialRefs,
        0,
        Some(Interruption::Create { batch: 0, applied_aliases: Box::new([]) }),
    )
    .await
    .unwrap();
    assert_eq!(competing.outcome, AttemptOutcome::Stopped(StopReason::Indeterminate));
    assert_eq!(primary.outcome, AttemptOutcome::Stopped(StopReason::Rejected));
    assert_eq!(primary.trace.tuples.len(), 2);
    assert!(primary.trace.creates.is_none());
    let last_id = &primary_tuple_intent.later.last().unwrap().id;
    assert_eq!(world.published(last_id).unwrap().history.last().head, oid(500));
    assert_restart_converges(world, &primary_tuple_intent, "between-tuple-batches").await;
}

#[tokio::test]
async fn competing_publishers_interleave_between_marker_batches() {
    let old_marker_intent = many_bounded_ids_intent(28, 250);
    let mut new_marker_intent = old_marker_intent.clone();
    let new_marker_revision = {
        let revision = &mut new_marker_intent.later.last_mut().unwrap().desired;
        revision.head = oid(600);
        *revision
    };
    let mut initial = DurableWorld::for_intents(oid(1), &[&old_marker_intent, &new_marker_intent]);
    for local in old_marker_intent.iter() {
        initial.publish_for_setup(&local.id, local.desired);
        initial.open_for_setup(&local.id, &local.title, "provisional");
    }
    let primary = initial.plan(&old_marker_intent, &QueryVisibility::default()).unwrap();
    let mut world = initial;
    let last_id = &new_marker_intent.later.last().unwrap().id;
    world.publish_for_setup(last_id, new_marker_revision);
    let competing = world.plan(&new_marker_intent, &QueryVisibility::default()).unwrap();
    let (primary, competing) =
        execute_batch_interleaved(&mut world, primary, competing, EffectStage::Marker, 0, None)
            .await
            .unwrap();
    assert_eq!(competing.outcome, AttemptOutcome::Acknowledged);
    assert_eq!(primary.outcome, AttemptOutcome::Stopped(StopReason::Rejected));
    assert_eq!(primary.trace.markers.len(), 2);
    assert!(primary.trace.updates.is_empty());
    assert_eq!(world.open_pull_request(last_id).unwrap().marker(), Some(oid(600)));
    assert_restart_converges(world, &old_marker_intent, "between-marker-batches").await;
}

#[tokio::test]
async fn stale_competitor_loses_completed_create_request_and_primary_resumes() {
    let create_intent = multi_request_intent();
    let initial = DurableWorld::for_intents(oid(10), &[&create_intent]);
    let primary = initial.plan(&create_intent, &QueryVisibility::default()).unwrap();
    let competing = initial.plan(&create_intent, &QueryVisibility::default()).unwrap();
    let mut world = initial;
    let (primary, competing) =
        execute_batch_interleaved(&mut world, primary, competing, EffectStage::Create, 0, None)
            .await
            .unwrap();
    assert_eq!(primary.outcome, AttemptOutcome::Acknowledged);
    assert_eq!(competing.outcome, AttemptOutcome::Stopped(StopReason::Indeterminate));
    assert_eq!(
        primary.trace.creates.as_ref().unwrap().iter().map(|batch| batch.len()).collect::<Vec<_>>(),
        [1, 1, 1]
    );
    assert_eq!(competing.trace.creates.as_ref().unwrap().len(), 1);
    assert!(competing.trace.markers.is_empty() && competing.trace.updates.is_empty());
    assert!(
        world
            .run_attempt(&create_intent, &QueryVisibility::default(), None)
            .await
            .unwrap()
            .trace
            .is_empty()
    );
}

#[tokio::test]
async fn fresh_competitor_wins_future_create_requests_and_stale_primary_stops() {
    // A publisher which starts after the first request observes and retains
    // that identity, then creates the later requests before the original
    // publisher resumes its stale plan.
    let create_intent = multi_request_intent();
    let initial = DurableWorld::for_intents(oid(10), &[&create_intent]);
    let mut after_first_request = initial.clone();
    let stopped = after_first_request
        .run_attempt(
            &create_intent,
            &QueryVisibility::default(),
            Some(Interruption::Create { batch: 0, applied_aliases: Box::new([0]) }),
        )
        .await
        .unwrap();
    assert_eq!(stopped.outcome, AttemptOutcome::Stopped(StopReason::Indeterminate));
    let primary = initial.plan(&create_intent, &QueryVisibility::default()).unwrap();
    let competing = after_first_request.plan(&create_intent, &QueryVisibility::default()).unwrap();
    let mut world = initial;
    let (primary, competing) =
        execute_batch_interleaved(&mut world, primary, competing, EffectStage::Create, 0, None)
            .await
            .unwrap();
    assert_eq!(primary.outcome, AttemptOutcome::Stopped(StopReason::Indeterminate));
    assert_eq!(competing.outcome, AttemptOutcome::Acknowledged);
    assert_eq!(primary.trace.creates.as_ref().unwrap().len(), 2);
    assert_eq!(
        competing
            .trace
            .creates
            .as_ref()
            .unwrap()
            .iter()
            .map(|batch| batch.len())
            .collect::<Vec<_>>(),
        [1, 1]
    );
    assert!(primary.trace.markers.is_empty() && primary.trace.updates.is_empty());
    assert!(
        world
            .run_attempt(&create_intent, &QueryVisibility::default(), None)
            .await
            .unwrap()
            .trace
            .is_empty()
    );
}

#[tokio::test]
async fn competing_publishers_interleave_between_update_requests() {
    let shared = multi_request_intent();
    let shared_tip = shared.later.last().unwrap().desired.head;
    let a_intent = LocalIntent::new(
        oid(10),
        shared.iter().cloned().chain([local_change(
            "Grequesta",
            LiteralRevision { head: oid(700), first_parent: shared_tip },
            "A child",
            "A body",
        )]),
    );
    let b_intent = LocalIntent::new(
        oid(10),
        shared.iter().cloned().chain([local_change(
            "Grequestb",
            LiteralRevision { head: oid(701), first_parent: shared_tip },
            "B child",
            "B body",
        )]),
    );
    let mut world = DurableWorld::for_intents(oid(10), &[&a_intent, &b_intent]);
    for change in a_intent.iter().chain(std::iter::once(b_intent.later.last().unwrap())) {
        establish_marked(
            &mut world,
            change,
            if change.id == a_intent.first.id { BaseKind::Default } else { BaseKind::Owned },
            "provisional",
        );
    }
    let primary = world.plan(&a_intent, &QueryVisibility::default()).unwrap();
    let competing = world.plan(&b_intent, &QueryVisibility::default()).unwrap();
    let (primary, competing) =
        execute_batch_interleaved(&mut world, primary, competing, EffectStage::Update, 0, None)
            .await
            .unwrap();
    assert_eq!(primary.outcome, AttemptOutcome::Acknowledged);
    assert_eq!(competing.outcome, AttemptOutcome::Acknowledged);
    assert_eq!(
        primary.trace.updates.iter().map(|batch| batch.len()).collect::<Vec<_>>(),
        [1, 1, 2]
    );
    assert_eq!(
        competing.trace.updates.iter().map(|batch| batch.len()).collect::<Vec<_>>(),
        [1, 1, 2]
    );
    let shared_root = &a_intent.first.id;
    let competing_root_body = flatten(&competing.trace.updates)
        .iter()
        .find(|update| &update.id == shared_root)
        .and_then(|update| update.body.clone())
        .unwrap();
    assert_eq!(
        &world.open_pull_request(shared_root).unwrap().pull_request().body,
        &competing_root_body,
        "the competitor remains the last writer for the completed first request"
    );
    let resumed_id = &a_intent.later.first().unwrap().id;
    let primary_resumed_body = flatten(&primary.trace.updates)
        .iter()
        .find(|update| &update.id == resumed_id)
        .and_then(|update| update.body.clone())
        .unwrap();
    assert_eq!(
        &world.open_pull_request(resumed_id).unwrap().pull_request().body,
        &primary_resumed_body,
        "the primary resumes and becomes the last writer for a later request"
    );
    assert_restart_converges(world, &a_intent, "between-update-requests").await;
}

#[tokio::test]
async fn competing_different_marker_targets_release_only_the_winners_projection() {
    let default = oid(1);
    let old_revision = LiteralRevision { head: oid(40), first_parent: default };
    let new_revision = LiteralRevision { head: oid(41), first_parent: default };
    let old_intent = root_intent(default, "Gmarkerrace", old_revision);
    let new_intent = root_intent(default, "Gmarkerrace", new_revision);
    let mut initial = DurableWorld::for_intents(default, &[&old_intent, &new_intent]);
    initial.publish_for_setup(&id("Gmarkerrace"), old_revision);
    initial.open_for_setup(&id("Gmarkerrace"), "provisional", "provisional");

    let mut advanced = initial.clone();
    let advance = advanced
        .run_attempt(
            &new_intent,
            &QueryVisibility::default(),
            Some(Interruption::Marker { batch: 0, applied: false }),
        )
        .await
        .unwrap();
    assert_eq!(flatten(&advance.trace.tuples).len(), 1);
    assert_eq!(advanced.open_pull_request(&id("Gmarkerrace")).unwrap().marker(), None);

    for old_is_primary in [true, false] {
        let old_plan = initial.plan(&old_intent, &QueryVisibility::default()).unwrap();
        let new_plan = advanced.plan(&new_intent, &QueryVisibility::default()).unwrap();
        let (primary, competing, primary_intent, winning_target) = if old_is_primary {
            (old_plan, new_plan, &old_intent, new_revision.head)
        } else {
            (new_plan, old_plan, &new_intent, old_revision.head)
        };
        let mut world = advanced.clone();
        let (primary, competing) =
            execute_interleaved(&mut world, primary, competing, EffectStage::InitialRefs, None)
                .await
                .unwrap();
        assert_eq!(competing.outcome, AttemptOutcome::Acknowledged);
        assert_eq!(
            primary.outcome,
            AttemptOutcome::Stopped(StopReason::Rejected),
            "the immutable marker with the other target rejects the suspended publisher"
        );
        assert!(primary.trace.updates.is_empty());
        assert_eq!(
            world.open_pull_request(&id("Gmarkerrace")).unwrap().marker(),
            Some(winning_target)
        );
        assert_restart_converges(world, primary_intent, "different-marker-targets").await;
    }
}

#[tokio::test]
async fn disjoint_and_identical_publishers_have_only_protocol_outcomes() {
    let default = oid(1);
    let a_revision = LiteralRevision { head: oid(10), first_parent: default };
    let b_revision = LiteralRevision { head: oid(20), first_parent: default };
    let a_intent = root_intent(default, "GA", a_revision);
    let b_intent = root_intent(default, "GB", b_revision);
    let initial = DurableWorld::for_intents(default, &[&a_intent, &b_intent]);

    let mut ab = initial.clone();
    let a = initial.plan(&a_intent, &QueryVisibility::default()).unwrap();
    let b = initial.plan(&b_intent, &QueryVisibility::default()).unwrap();
    assert_eq!(ab.execute_plan(a, None).await.unwrap().outcome, AttemptOutcome::Acknowledged);
    assert!(ab.published(&id("GB")).is_none(), "publisher A cannot publish B's change");
    assert_eq!(ab.execute_plan(b, None).await.unwrap().outcome, AttemptOutcome::Acknowledged);

    let mut ba = initial.clone();
    let b = initial.plan(&b_intent, &QueryVisibility::default()).unwrap();
    let a = initial.plan(&a_intent, &QueryVisibility::default()).unwrap();
    assert_eq!(ba.execute_plan(b, None).await.unwrap().outcome, AttemptOutcome::Acknowledged);
    assert!(ba.published(&id("GA")).is_none(), "publisher B cannot publish A's change");
    assert_eq!(ba.execute_plan(a, None).await.unwrap().outcome, AttemptOutcome::Acknowledged);
    assert_ne!(ab.identity(&id("GA")), ba.identity(&id("GA")), "allocation order is durable");
    assert_same_protocol_state_ignoring_identities(&ab, &ba);
    assert!(
        ab.run_attempt(&a_intent, &QueryVisibility::default(), None)
            .await
            .unwrap()
            .trace
            .is_empty()
    );
    assert!(
        ab.run_attempt(&b_intent, &QueryVisibility::default(), None)
            .await
            .unwrap()
            .trace
            .is_empty()
    );
    assert!(
        ba.run_attempt(&a_intent, &QueryVisibility::default(), None)
            .await
            .unwrap()
            .trace
            .is_empty()
    );
    assert!(
        ba.run_attempt(&b_intent, &QueryVisibility::default(), None)
            .await
            .unwrap()
            .trace
            .is_empty()
    );

    let same_revision = LiteralRevision { head: oid(30), first_parent: default };
    let same_intent = root_intent(default, "Gsame", same_revision);
    let same_initial = DurableWorld::for_intents(default, &[&same_intent]);
    let same_a = same_initial.plan(&same_intent, &QueryVisibility::default()).unwrap();
    let same_b = same_initial.plan(&same_intent, &QueryVisibility::default()).unwrap();
    let mut same_world = same_initial;
    assert_eq!(
        same_world.execute_plan(same_a, None).await.unwrap().outcome,
        AttemptOutcome::Acknowledged
    );
    let after_first = same_world.clone();
    assert_eq!(
        same_world.execute_plan(same_b, None).await.unwrap().outcome,
        AttemptOutcome::Stopped(StopReason::Indeterminate),
        "the tuple is already desired, then the stable-key create loses the race"
    );
    assert_eq!(same_world, after_first, "a duplicate create changes nothing");
    assert!(
        same_world
            .run_attempt(&same_intent, &QueryVisibility::default(), None)
            .await
            .unwrap()
            .trace
            .is_empty()
    );

    let marker_revision = LiteralRevision { head: oid(40), first_parent: default };
    let marker_intent = root_intent(default, "Gmarker", marker_revision);
    let mut marker_world = DurableWorld::for_intents(default, &[&marker_intent]);
    marker_world.publish_for_setup(&id("Gmarker"), marker_revision);
    marker_world.open_for_setup(&id("Gmarker"), "provisional", "provisional");
    let marker_a = marker_world.plan(&marker_intent, &QueryVisibility::default()).unwrap();
    let marker_b = marker_world.plan(&marker_intent, &QueryVisibility::default()).unwrap();
    let marker_a = marker_world.execute_plan(marker_a, None).await.unwrap();
    assert!(marker_a.trace.tuples.is_empty() && marker_a.trace.creates.is_none());
    assert_eq!(flatten(&marker_a.trace.markers).len(), 1);
    let after_marker = marker_world.clone();
    assert_eq!(
        marker_world.execute_plan(marker_b, None).await.unwrap().outcome,
        AttemptOutcome::Acknowledged,
        "the same immutable marker and projection are idempotent"
    );
    assert_eq!(marker_world, after_marker);
    assert!(
        marker_world
            .run_attempt(&marker_intent, &QueryVisibility::default(), None)
            .await
            .unwrap()
            .trace
            .is_empty()
    );
}

#[tokio::test]
async fn conflicting_publishers_retry_from_the_winner_and_indeterminate_create() {
    let default = oid(1);
    let v1 = LiteralRevision { head: oid(10), first_parent: default };
    let a_revision = LiteralRevision { head: oid(11), first_parent: default };
    let b_revision = LiteralRevision { head: oid(12), first_parent: default };
    let a_intent = root_intent(default, "Gconflict", a_revision);
    let b_intent = root_intent(default, "Gconflict", b_revision);
    let mut initial = DurableWorld::for_intents(default, &[&a_intent, &b_intent]);
    initial.publish_for_setup(&id("Gconflict"), v1);
    for (winner_intent, loser_intent, winner, loser) in [
        (&a_intent, &b_intent, a_revision, b_revision),
        (&b_intent, &a_intent, b_revision, a_revision),
    ] {
        let mut world = initial.clone();
        let winner_attempt = initial.plan(winner_intent, &QueryVisibility::default()).unwrap();
        let loser_attempt = initial.plan(loser_intent, &QueryVisibility::default()).unwrap();
        assert_eq!(
            world.execute_plan(winner_attempt, None).await.unwrap().outcome,
            AttemptOutcome::Acknowledged
        );
        let after_winner = world.clone();
        assert_eq!(
            world.execute_plan(loser_attempt, None).await.unwrap().outcome,
            AttemptOutcome::Stopped(StopReason::Rejected),
            "a stale tuple lease stops every later stage of that attempt"
        );
        assert_eq!(world, after_winner);

        let retry =
            world.run_attempt(loser_intent, &QueryVisibility::default(), None).await.unwrap();
        assert_eq!(
            flatten(&retry.trace.tuples).as_ref(),
            &[TupleEffect {
                id: id("Gconflict"),
                expected: Some(winner),
                desired: loser,
                version: 3,
            }]
        );
        assert_eq!(retry.outcome, AttemptOutcome::Acknowledged);
        assert_eq!(
            world.published(&id("Gconflict")).unwrap().history.iter().copied().collect::<Vec<_>>(),
            [v1, winner, loser]
        );
        assert!(
            world
                .run_attempt(loser_intent, &QueryVisibility::default(), None)
                .await
                .unwrap()
                .trace
                .is_empty()
        );
    }

    let old_revision = LiteralRevision { head: oid(20), first_parent: default };
    let new_revision = LiteralRevision { head: oid(21), first_parent: default };
    let old_intent = root_intent(default, "Gcreate", old_revision);
    let new_intent = root_intent(default, "Gcreate", new_revision);
    let mut world = DurableWorld::for_intents(default, &[&old_intent, &new_intent]);
    let first = world
        .run_attempt(
            &old_intent,
            &QueryVisibility::default(),
            Some(Interruption::Create { batch: 0, applied_aliases: Box::new([]) }),
        )
        .await
        .unwrap();
    assert_eq!(flatten(&first.trace.tuples).len(), 1);
    let old_create_attempt = world.plan(&old_intent, &QueryVisibility::default()).unwrap();
    let concurrent = world
        .run_attempt(
            &new_intent,
            &QueryVisibility::default(),
            Some(Interruption::Create { batch: 0, applied_aliases: Box::new([]) }),
        )
        .await
        .unwrap();
    assert_eq!(flatten(&concurrent.trace.tuples).len(), 1);
    let indeterminate = world.execute_plan(old_create_attempt, None).await.unwrap();
    assert_eq!(
        indeterminate.outcome,
        AttemptOutcome::Stopped(StopReason::Indeterminate),
        "the stable-key create may land after its observed branch OIDs move"
    );
    assert!(indeterminate.trace.tuples.is_empty());
    assert_eq!(indeterminate.trace.creates.as_ref().unwrap().len(), 1);
    assert!(
        world.open_pull_request(&id("Gcreate")).unwrap().marker().is_none(),
        "an indeterminate create receipt cannot authorize later attempt stages"
    );
    let world = assert_restart_converges(world, &new_intent, "indeterminate-create").await;
    assert_eq!(
        world.published(&id("Gcreate")).unwrap().history.iter().copied().collect::<Vec<_>>(),
        [old_revision, new_revision]
    );
}

#[tokio::test]
async fn divergent_navigation_is_last_writer_wins_then_freshly_repairable() {
    let default = oid(1);
    let root = LiteralRevision { head: oid(10), first_parent: default };
    let a_child = LiteralRevision { head: oid(11), first_parent: root.head };
    let b_child = LiteralRevision { head: oid(12), first_parent: root.head };
    let root_change = local_change("Groot", root, "Shared root", "Root body");
    let a_intent = LocalIntent::new(
        default,
        [root_change.clone(), local_change("GA", a_child, "A child", "A body")],
    );
    let b_intent =
        LocalIntent::new(default, [root_change, local_change("GB", b_child, "B child", "B body")]);
    let mut initial = DurableWorld::for_intents(default, &[&a_intent, &b_intent]);
    for (change, base) in [
        (&a_intent.first, BaseKind::Default),
        (&a_intent.later[0], BaseKind::Owned),
        (&b_intent.later[0], BaseKind::Owned),
    ] {
        establish_marked(&mut initial, change, base, "stale body");
    }

    let mut a_preview = initial.clone();
    let a_attempt =
        a_preview.run_attempt(&a_intent, &QueryVisibility::default(), None).await.unwrap();
    let mut b_preview = initial.clone();
    let b_attempt =
        b_preview.run_attempt(&b_intent, &QueryVisibility::default(), None).await.unwrap();
    assert!(a_attempt.trace.tuples.is_empty() && a_attempt.trace.markers.is_empty());
    assert!(b_attempt.trace.tuples.is_empty() && b_attempt.trace.markers.is_empty());
    insta::assert_yaml_snapshot!(
        "divergent_child_exact_update_alternatives",
        UpdateAlternatives {
            publisher_a: update_snapshot(&a_attempt.trace),
            publisher_b: update_snapshot(&b_attempt.trace),
        }
    );

    let a_updates = flatten(&a_attempt.trace.updates);
    let b_updates = flatten(&b_attempt.trace.updates);
    assert_eq!((a_updates.len(), b_updates.len()), (2, 2));
    let root_identity = initial.identity(&id("Groot"));
    let a_root_body = a_updates
        .iter()
        .find(|update| update.identity == root_identity)
        .and_then(|update| update.body.clone())
        .unwrap();
    let b_root_body = b_updates
        .iter()
        .find(|update| update.identity == root_identity)
        .and_then(|update| update.body.clone())
        .unwrap();
    assert_ne!(a_root_body, b_root_body);

    for (first_intent, second_intent, expected, repair_intent, nonlocal_id) in [
        (&a_intent, &b_intent, &b_root_body, &a_intent, id("GB")),
        (&b_intent, &a_intent, &a_root_body, &b_intent, id("GA")),
    ] {
        let mut world = initial.clone();
        let first = initial.plan(first_intent, &QueryVisibility::default()).unwrap();
        let second = initial.plan(second_intent, &QueryVisibility::default()).unwrap();
        assert_eq!(
            world.execute_plan(first, None).await.unwrap().outcome,
            AttemptOutcome::Acknowledged
        );
        assert_eq!(
            world.execute_plan(second, None).await.unwrap().outcome,
            AttemptOutcome::Acknowledged
        );
        assert_eq!(
            &world.open_pull_request(&id("Groot")).unwrap().pull_request().body,
            expected,
            "the later stale update request wins at the stage boundary"
        );
        let nonlocal = world.published(&nonlocal_id).cloned();
        let stabilized = assert_restart_converges(world, repair_intent, "navigation-repair").await;
        assert_eq!(stabilized.published(&nonlocal_id), nonlocal.as_ref());
    }
}

#[tokio::test]
async fn stale_precomputed_projection_lands_safely_and_fresh_intent_repairs_it() {
    let default = oid(1);
    let old_revision = LiteralRevision { head: oid(10), first_parent: default };
    let new_revision = LiteralRevision { head: oid(11), first_parent: default };
    let old_intent =
        LocalIntent::new(default, [local_change("Gstale", old_revision, "Old title", "Old body")]);
    let new_intent =
        LocalIntent::new(default, [local_change("Gstale", new_revision, "New title", "New body")]);
    let mut initial = DurableWorld::for_intents(default, &[&old_intent, &new_intent]);
    establish_marked(&mut initial, &old_intent.first, BaseKind::Default, "outdated body");
    let stale_attempt = initial.plan(&old_intent, &QueryVisibility::default()).unwrap();
    let new_attempt = initial.plan(&new_intent, &QueryVisibility::default()).unwrap();

    let mut world = initial;
    assert_eq!(
        world.execute_plan(new_attempt, None).await.unwrap().outcome,
        AttemptOutcome::Acknowledged
    );
    let stale = world.execute_plan(stale_attempt, None).await.unwrap();
    assert_eq!(stale.outcome, AttemptOutcome::Acknowledged);
    let stale_updates = flatten(&stale.trace.updates);
    assert_eq!(stale_updates.len(), 1);
    assert_eq!(
        world.open_pull_request(&id("Gstale")).unwrap().pull_request().body,
        stale_updates[0].body.clone().unwrap()
    );
    let world = assert_restart_converges(world, &new_intent, "stale-projection").await;
    assert_eq!(
        world.published(&id("Gstale")).unwrap().history.last(),
        new_revision,
        "repair never rewrites the winning immutable tuple"
    );
}

#[tokio::test]
async fn stale_nonroot_visibility_can_project_a_root_without_touching_its_old_parent() {
    let default = oid(1);
    let old_parent = LiteralRevision { head: oid(30), first_parent: default };
    let old_child = LiteralRevision { head: oid(31), first_parent: old_parent.head };
    let moved_root = LiteralRevision { head: oid(32), first_parent: default };
    let old_intent = LocalIntent::new(
        default,
        [
            local_change("Gparent", old_parent, "Old parent", "Parent body"),
            local_change("Gmove", old_child, "Old child", "Old child body"),
        ],
    );
    let new_intent = LocalIntent::new(
        default,
        [local_change("Gmove", moved_root, "Moved root", "Moved root body")],
    );
    let mut world = DurableWorld::for_intents(default, &[&old_intent, &new_intent]);
    world.publish_for_setup(&id("Gparent"), old_parent);
    world.publish_for_setup(&id("Gmove"), old_child);
    world.open_for_setup(&id("Gmove"), "Old child", "Old child body");
    world.mark_for_setup(&id("Gmove"), old_child.head, BaseKind::Owned);
    let parent_before = world.published(&id("Gparent")).cloned();
    let stale = QueryVisibility::stale(&world, [id("Gmove")]);

    let attempt = world.run_attempt(&new_intent, &stale, None).await.unwrap();
    assert_eq!(flatten(&attempt.trace.tuples).len(), 1);
    assert!(attempt.trace.markers.is_empty());
    assert_eq!(flatten(&attempt.trace.updates).len(), 1);
    assert_eq!(flatten(&attempt.trace.updates)[0].base_branch.as_deref(), Some(DEFAULT_BRANCH));
    insta::assert_yaml_snapshot!(
        "nonroot_to_root_stale_open_exact_update",
        update_snapshot(&attempt.trace)
    );
    assert_eq!(world.published(&id("Gparent")), parent_before.as_ref());
    let stale_retry = world.run_attempt(&new_intent, &stale, None).await.unwrap();
    assert_eq!(
        flatten(&stale_retry.trace.updates),
        flatten(&attempt.trace.updates),
        "durable effects cannot advance an already-returned stale observation"
    );
    assert!(
        world
            .run_attempt(&new_intent, &QueryVisibility::default(), None)
            .await
            .unwrap()
            .trace
            .is_empty()
    );
}

#[tokio::test]
async fn public_branch_lease_conflict_rolls_back_its_atomic_tuple_batch() {
    let default = oid(1);
    let revision = LiteralRevision { head: oid(2), first_parent: default };
    let intent = root_intent(default, "Gpublic", revision);
    let mut world = DurableWorld::for_intents(default, &[&intent]);
    let plan = world
        .plan_with_public_branch(&intent, &QueryVisibility::default(), Some("release-candidate"))
        .unwrap();

    // This write occurs after the plan's exact absence observation. The
    // public-branch lease must reject, and atomicity must also roll back the
    // tuple which preceded it in the same batch.
    world.public_branches.insert("release-candidate".to_owned(), oid(99));
    let before = world.clone();
    let report = world.execute_plan(plan, None).await.unwrap();

    assert_eq!(report.outcome, AttemptOutcome::Stopped(StopReason::Rejected));
    assert_eq!(world, before);
    assert!(world.published(&id("Gpublic")).is_none());
    assert!(report.trace.creates.is_none());

    // A fresh attempt observes the winning value and may deliberately replace
    // it because a managed public branch is a GHerrit-owned projection.
    let retry = world
        .plan_with_public_branch(&intent, &QueryVisibility::default(), Some("release-candidate"))
        .unwrap();
    assert_eq!(
        world.execute_plan(retry, None).await.unwrap().outcome,
        AttemptOutcome::Acknowledged
    );
    assert_eq!(world.public_branches["release-candidate"], revision.head);
}

#[tokio::test]
async fn lost_initial_ref_acknowledgement_recovers_public_branch_and_tuple_together() {
    let default = oid(1);
    let revision = LiteralRevision { head: oid(2), first_parent: default };
    let intent = root_intent(default, "Gpublic", revision);
    let mut world = DurableWorld::for_intents(default, &[&intent]);
    let first = world
        .plan_with_public_branch(&intent, &QueryVisibility::default(), Some("release-candidate"))
        .unwrap();

    let interrupted = world
        .execute_plan(first, Some(Interruption::InitialRefs { batch: 0, applied: true }))
        .await
        .unwrap();
    assert_eq!(interrupted.outcome, AttemptOutcome::Stopped(StopReason::Indeterminate));
    assert_eq!(world.public_branches["release-candidate"], revision.head);
    assert_eq!(world.published(&id("Gpublic")).unwrap().history.last(), revision);
    assert!(world.open_pull_request(&id("Gpublic")).is_none());

    let retry = world
        .plan_with_public_branch(&intent, &QueryVisibility::default(), Some("release-candidate"))
        .unwrap();
    assert_eq!(
        world.execute_plan(retry, None).await.unwrap().outcome,
        AttemptOutcome::Acknowledged
    );
    assert_eq!(world.public_branches["release-candidate"], revision.head);
    assert!(world.open_pull_request(&id("Gpublic")).is_some());

    let converged = world
        .plan_with_public_branch(&intent, &QueryVisibility::default(), Some("release-candidate"))
        .unwrap();
    assert!(world.execute_plan(converged, None).await.unwrap().trace.is_empty());
}

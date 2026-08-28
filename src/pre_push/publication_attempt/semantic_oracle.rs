//! Restart checks over literal durable state.
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
    body::StackBodyRecipes,
    github::{
        AbsentPullRequest, BaseKind, CompleteCreateReceipts, CompleteLocalPullRequests,
        LocalPullRequestObservation, ManagedOpenPullRequest, ObservedBase, PreparedCreates,
        PreparedUpdates, PullRequestIdentity, PullRequestNumber, TestCreate, TestUpdate,
    },
    history::ValidatedChangeHistory,
    marker::{MarkerTemplate, PullRequestMarker},
    plan::{EffectDriver, PlannedPublication, plan_effects},
    refs::{
        MarkerTransition, PreparedPushes, PublicationRevision, TestPushEffect,
        prepare_marker_pushes,
    },
};
use crate::pre_push::{
    destination::{DefaultBranch, PushDestination, RepositoryCoordinates},
    local::{GherritPrId, LocalStack},
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
    is_draft: bool,
}

/// The exact immutable identity of one canonical marker object.
///
/// The containing change supplies the change ID. Together, that ID, this v1
/// target, and the pull request number determine the canonical annotated tag
/// bytes and object ID. The model therefore cannot represent a noncanonical
/// tag object beside otherwise valid semantic marker fields.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct MarkerIdentity {
    v1: ObjectId,
    marker: PullRequestMarker,
}

impl MarkerIdentity {
    fn number(self) -> PullRequestNumber {
        self.marker.number()
    }

    fn tag(self, id: &GherritPrId) -> ObjectId {
        MarkerTemplate::new(id.clone(), self.v1, self.number()).unwrap().prepare().unwrap().tag()
    }
}

/// Marker state exists only together with the OPEN identity which authorized
/// it. This excludes a marker-without-pull-request world by construction.
#[derive(Clone, Debug, Eq, PartialEq)]
enum ManagedPullRequest {
    Unmarked(PullRequest),
    Marked { pull_request: PullRequest, marker: MarkerIdentity, base: BaseKind },
}

impl ManagedPullRequest {
    fn pull_request(&self) -> &PullRequest {
        match self {
            Self::Unmarked(pull_request) | Self::Marked { pull_request, .. } => pull_request,
        }
    }

    fn marker(&self) -> Option<MarkerIdentity> {
        match self {
            Self::Unmarked(_) => None,
            Self::Marked { marker, .. } => Some(*marker),
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
    next_identity: u32,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct ObservedOpenFields {
    head_oid: ObjectId,
    base: BaseKind,
    base_oid: ObjectId,
    title: String,
    body: String,
    is_draft: bool,
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
        let world = Self { default_tip, changes: HashMap::new(), next_identity: 100 };
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
            is_draft: false,
        }));
        self.assert_well_formed();
    }

    fn mark_for_setup(&mut self, id: &GherritPrId, v1: ObjectId, base: BaseKind) {
        let change = self.published_mut(id).expect("a marker requires published history");
        assert_eq!(change.history.first.head, v1, "a marker targets exactly v1");
        let pull_request = change.pull_request.take().expect("a marker requires an OPEN identity");
        let ManagedPullRequest::Unmarked(pull_request) = pull_request else {
            panic!("test setup cannot move an immutable marker")
        };
        let marker = marker_identity(v1, pull_request.identity.number());
        change.pull_request = Some(ManagedPullRequest::Marked { pull_request, marker, base });
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
            is_draft: pull_request.is_draft,
        }
    }

    fn plan(
        &self,
        intent: &LocalIntent,
        visibility: &QueryVisibility,
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
        let histories = intent
            .iter()
            .map(|local| {
                let (published, marker) = match self.published(&local.id) {
                    None => (Vec::new(), None),
                    Some(change) => (
                        change
                            .history
                            .iter()
                            .map(|revision| (revision.head, revision.first_parent))
                            .collect(),
                        change
                            .pull_request
                            .as_ref()
                            .and_then(ManagedPullRequest::marker)
                            .map(MarkerIdentity::number),
                    ),
                };
                validated_history(
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
        plan_effects(&destination, None, stack, histories, pull_requests)
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
            fields.is_draft,
            false,
        ))
    }

    fn assert_well_formed(&self) {
        let mut numbers = HashSet::new();
        let mut node_ids = HashSet::new();
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
                numbers.insert(pull_request_fields.identity.number()),
                "durable OPEN pull request numbers are unique"
            );
            assert!(
                node_ids.insert(pull_request_fields.identity.node_id_for_test().to_owned()),
                "durable OPEN pull request node IDs are unique"
            );
            assert!(
                pull_request_fields.identity.number().get() < self.next_identity,
                "the allocation cursor follows every durable OPEN identity"
            );
            if let Some(marker) = pull_request.marker() {
                assert_eq!(
                    marker.v1, published.history.first.head,
                    "an immutable marker targets exactly v1"
                );
                assert_eq!(
                    marker.number(),
                    pull_request_fields.identity.number(),
                    "a marker retains the exact OPEN identity which authorized it"
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

fn validated_history(
    id: GherritPrId,
    published: &[(ObjectId, ObjectId)],
    proposal: (ObjectId, ObjectId),
    pull_request_number: Option<PullRequestNumber>,
) -> ValidatedChangeHistory {
    ValidatedChangeHistory::for_plan_test(id, published, proposal, pull_request_number)
}

fn marker_identity(v1: ObjectId, number: PullRequestNumber) -> MarkerIdentity {
    MarkerIdentity { v1, marker: PullRequestMarker::new(number) }
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

/// One semantic effect in the attempt's first atomic ref stage.
///
/// Activation adds the optional public branch to this enum without changing
/// the executor boundary or conflating it with a change tuple.
#[derive(Clone, Debug, Eq, PartialEq)]
enum InitialRefEffect {
    Tuple(TupleEffect),
}

impl InitialRefEffect {
    fn tuple(&self) -> &TupleEffect {
        let Self::Tuple(tuple) = self;
        tuple
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct MarkerEffect {
    id: GherritPrId,
    marker: MarkerIdentity,
}

/// One GitHub update request annotated only after resolving its node ID.
#[derive(Clone, Debug, Eq, PartialEq)]
struct UpdateEffect {
    resolved_id: Option<GherritPrId>,
    operation: TestUpdate,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
struct AttemptTrace {
    initial_refs: EffectBatches<InitialRefEffect>,
    creates: EffectBatches<TestCreate>,
    markers: EffectBatches<MarkerEffect>,
    updates: EffectBatches<UpdateEffect>,
}

impl AttemptTrace {
    fn is_empty(&self) -> bool {
        self.initial_refs.is_empty()
            && self.creates.is_empty()
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

/// Executes the real consuming publication plan directly against literal
/// durable state. The trace is evidence of calls the production stage machine
/// actually reached; it is never replayed to drive another attempt.
struct WorldDriver<'world> {
    world: &'world mut DurableWorld,
    interruption: Option<Interruption>,
    interruption_consumed: bool,
    failure: Option<StopReason>,
    trace: AttemptTrace,
}

impl<'world> WorldDriver<'world> {
    fn new(world: &'world mut DurableWorld, interruption: Option<Interruption>) -> Self {
        Self {
            world,
            interruption,
            interruption_consumed: false,
            failure: None,
            trace: AttemptTrace::default(),
        }
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
        Ok(AttemptReport { outcome, trace: self.trace })
    }
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
                    TestPushEffect::Tuple { id, expected, desired, version } => {
                        InitialRefEffect::Tuple(TupleEffect {
                            id: id.clone(),
                            expected: expected.map(literal_revision),
                            desired: literal_revision(*desired),
                            version: *version,
                        })
                    }
                    TestPushEffect::Marker { .. } => {
                        panic!("the initial-ref stage cannot contain marker effects")
                    }
                })
                .collect::<Box<[_]>>();
            self.trace.initial_refs.push(effects.clone());
            if let Some(Interruption::InitialRefs { applied, .. }) =
                self.take_interruption(EffectStage::InitialRefs, index)
            {
                if applied
                    && apply_atomic_git_batch(self.world, &effects, ExternalEffect::InitialRef)
                        != EffectOutcome::Acknowledged
                {
                    return self.stop(StopReason::Rejected);
                }
                return self.stop(StopReason::Indeterminate);
            }
            let outcome = apply_atomic_git_batch(self.world, &effects, ExternalEffect::InitialRef);
            if outcome != EffectOutcome::Acknowledged {
                return self.stop(outcome.stop_reason());
            }
        }
        Ok(())
    }

    async fn create_pull_requests(
        &mut self,
        creates: PreparedCreates,
    ) -> Result<CompleteCreateReceipts> {
        assert_eq!(creates.repository_id_for_test(), REPOSITORY_ID);
        let mut receipts = Vec::with_capacity(creates.operations_for_test().len());
        for (index, batch) in creates.batches_for_test().enumerate() {
            let batch = batch.to_vec().into_boxed_slice();
            self.trace.creates.push(batch.clone());
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
        }
        Ok(CompleteCreateReceipts::for_plan_test(receipts))
    }

    async fn publish_markers(&mut self, markers: Box<[MarkerTemplate]>) -> Result<()> {
        let prepared = markers
            .into_vec()
            .into_iter()
            .map(|template| {
                let (id, v1, number) = template.test_parts();
                let effect = MarkerEffect { id: id.clone(), marker: marker_identity(v1, number) };
                let marker = template.prepare()?;
                assert_eq!(
                    marker.tag(),
                    effect.marker.tag(&effect.id),
                    "canonical marker preparation agrees with the durable model"
                );
                Ok((marker, effect))
            })
            .collect::<Result<Vec<_>>>()?;
        let transitions = prepared
            .iter()
            .map(|(marker, _)| MarkerTransition::from_prepared(marker))
            .collect::<Vec<_>>();
        let destination = PushDestination::for_test();
        let pushes = prepare_marker_pushes(&destination, &transitions)?;
        for (index, batch) in pushes.batches().enumerate() {
            let effects = batch
                .semantic_effects_for_test()
                .iter()
                .map(|effect| match effect {
                    TestPushEffect::Marker { id, target } => prepared
                        .iter()
                        .find(|(marker, _)| marker.id() == id && marker.tag() == *target)
                        .map(|(_, effect)| effect.clone())
                        .expect("each prepared marker push retains its canonical identity"),
                    TestPushEffect::Tuple { .. } => {
                        panic!("the marker stage cannot contain tuple effects")
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
        }
        Ok(())
    }

    async fn update_pull_requests(&mut self, updates: PreparedUpdates) -> Result<()> {
        for (index, batch) in updates.batches_for_test().enumerate() {
            let batch = batch
                .iter()
                .cloned()
                .map(|operation| self.world.resolve_update(operation))
                .collect::<Box<[_]>>();
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
    InitialRef(InitialRefEffect),
    Create(TestCreate),
    Marker(MarkerEffect),
    Update(UpdateEffect),
}

impl ExternalEffect {
    fn target(&self) -> Option<GherritPrId> {
        match self {
            Self::InitialRef(effect) => Some(effect.tuple().id.clone()),
            Self::Create(effect) => Some(effect.id.clone()),
            Self::Marker(effect) => Some(effect.id.clone()),
            Self::Update(effect) => effect.resolved_id.clone(),
        }
    }
}

impl DurableWorld {
    fn resolve_update(&self, operation: TestUpdate) -> UpdateEffect {
        let requested_node = operation.identity.node_id_for_test();
        let resolved_id = self.changes.iter().find_map(|(id, change)| {
            let pull_request = change.pull_request.as_ref()?.pull_request();
            (pull_request.identity.node_id_for_test() == requested_node).then(|| id.clone())
        });
        UpdateEffect { resolved_id, operation }
    }

    fn apply_effect(&mut self, effect: &ExternalEffect) -> EffectOutcome {
        let before = self.clone();
        let target = effect.target();
        let outcome = match effect {
            ExternalEffect::InitialRef(effect) => self.apply_initial_ref(effect),
            ExternalEffect::Create(effect) => self.apply_create(effect),
            ExternalEffect::Marker(effect) => self.apply_marker(effect),
            ExternalEffect::Update(effect) => self.apply_update(effect),
        };
        match outcome {
            EffectOutcome::Rejected => {
                assert_eq!(*self, before, "a rejected effect cannot alter durable state");
            }
            EffectOutcome::Acknowledged | EffectOutcome::Indeterminate => {
                match target.as_ref() {
                    Some(target) => self.assert_local_transition(&before, target, effect),
                    None => assert_eq!(
                        *self, before,
                        "an update for an unknown node ID cannot alter durable state"
                    ),
                }
                self.assert_well_formed();
            }
        }
        outcome
    }

    fn apply_initial_ref(&mut self, effect: &InitialRefEffect) -> EffectOutcome {
        match effect {
            InitialRefEffect::Tuple(tuple) => self.apply_tuple(tuple),
        }
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
                is_draft: false,
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
        if change.history.first.head != effect.marker.v1 {
            return EffectOutcome::Rejected;
        }
        let Some(pull_request) = change.pull_request.take() else {
            return EffectOutcome::Rejected;
        };
        match pull_request {
            ManagedPullRequest::Unmarked(pull_request) => {
                if pull_request.identity.number() != effect.marker.number() {
                    change.pull_request = Some(ManagedPullRequest::Unmarked(pull_request));
                    return EffectOutcome::Rejected;
                }
                change.pull_request = Some(ManagedPullRequest::Marked {
                    pull_request,
                    marker: effect.marker,
                    base: BaseKind::Owned,
                });
                EffectOutcome::Acknowledged
            }
            marked @ ManagedPullRequest::Marked { marker, .. } => {
                change.pull_request = Some(marked);
                if marker == effect.marker {
                    EffectOutcome::Acknowledged
                } else {
                    EffectOutcome::Rejected
                }
            }
        }
    }

    fn apply_update(&mut self, effect: &UpdateEffect) -> EffectOutcome {
        let Some(resolved_id) = effect.resolved_id.as_ref() else {
            return EffectOutcome::Indeterminate;
        };
        let base = match effect.operation.base_branch.as_deref() {
            None => None,
            Some(DEFAULT_BRANCH) => Some(BaseKind::Default),
            Some(base) if base == owned_base_name(resolved_id) => Some(BaseKind::Owned),
            Some(_) => return EffectOutcome::Indeterminate,
        };
        let change = self
            .changes
            .get_mut(resolved_id)
            .expect("a resolved update belongs to a durable published change");
        let managed = change
            .pull_request
            .as_mut()
            .expect("a resolved update belongs to a durable OPEN pull request");
        let ManagedPullRequest::Marked { pull_request, base: current_base, .. } = managed else {
            panic!("the planner cannot update an unmarked OPEN pull request")
        };
        if let Some(base) = base {
            *current_base = base;
        }
        if let Some(title) = &effect.operation.title {
            pull_request.title.clone_from(title);
        }
        if let Some(body) = &effect.operation.body {
            pull_request.body.clone_from(body);
        }
        if pull_request.identity == effect.operation.identity {
            EffectOutcome::Acknowledged
        } else {
            // GitHub routes by node ID. A mismatched expected number is known
            // only after the full patch has landed and its receipt is decoded.
            EffectOutcome::Indeterminate
        }
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
        assert!(
            !inserted || matches!(effect, ExternalEffect::InitialRef(InitialRefEffect::Tuple(_)))
        );
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
                assert!(
                    matches!(effect, ExternalEffect::InitialRef(InitialRefEffect::Tuple(_))),
                    "only a tuple adds history"
                );
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

fn flatten<T: Clone>(batches: &[Box<[T]>]) -> Box<[T]> {
    batches.iter().flat_map(|batch| batch.iter().cloned()).collect()
}

async fn assert_restart_converges(
    mut world: DurableWorld,
    intent: &LocalIntent,
    label: &str,
) -> DurableWorld {
    run_acknowledged_retry(&mut world, intent, label).await;
    assert_quiescent(&mut world, intent, label).await;
    world
}

/// Runs the exact first attempt after an interruption and returns its trace.
///
/// Keeping this attempt observable lets restart tests prove which durable
/// aliases were omitted, rather than checking only eventual convergence.
async fn run_acknowledged_retry(
    world: &mut DurableWorld,
    intent: &LocalIntent,
    label: &str,
) -> AttemptReport {
    let retry = world
        .run_attempt(intent, &QueryVisibility::default(), None)
        .await
        .unwrap_or_else(|error| panic!("{label}: fresh planning failed: {error}"));
    assert_eq!(
        retry.outcome,
        AttemptOutcome::Acknowledged,
        "{label}: a fresh stable-intent attempt must receive usable acknowledgements"
    );
    retry
}

async fn assert_quiescent(world: &mut DurableWorld, intent: &LocalIntent, label: &str) {
    let done = world
        .run_attempt(intent, &QueryVisibility::default(), None)
        .await
        .unwrap_or_else(|error| panic!("{label}: final observation failed: {error}"));
    assert_eq!(
        done.outcome,
        AttemptOutcome::Acknowledged,
        "{label}: a quiescent attempt must still complete with usable acknowledgements"
    );
    assert!(done.trace.is_empty(), "{label}: the next fresh attempt must have no durable work");
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
    assert!((8..=128).contains(&id_bytes));
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

const MULTI_REQUEST_BODY_BYTES_BEFORE_JSON_ESCAPING: usize = 90_000;

fn multi_request_intent() -> LocalIntent {
    let default = oid(10);
    let body = "\u{1}".repeat(MULTI_REQUEST_BODY_BYTES_BEFORE_JSON_ESCAPING);
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
struct CreateSnapshot {
    id: String,
    title: String,
    head_oid: String,
    base_oid: String,
    body: Box<[String]>,
}

fn create_snapshot(trace: &AttemptTrace) -> Vec<CreateSnapshot> {
    flatten(&trace.creates)
        .iter()
        .map(|create| CreateSnapshot {
            id: create.id.as_str().to_owned(),
            title: create.title.clone(),
            head_oid: create.head_oid.to_string(),
            base_oid: create.base_oid.to_string(),
            // `split` keeps a final empty element when the body ends in LF,
            // so the readable line sequence still preserves exact bytes.
            body: create.body.split('\n').map(str::to_owned).collect(),
        })
        .collect()
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
            id: update
                .resolved_id
                .as_ref()
                .expect("a planned update resolves to its observed OPEN row")
                .as_str()
                .to_owned(),
            number: update.operation.identity.number().get(),
            node_id: update.operation.identity.node_id_for_test().to_owned(),
            title: update.operation.title.clone(),
            body: update
                .operation
                .body
                .as_deref()
                .map(|body| body.split('\n').map(str::to_owned).collect()),
            base_branch: update.operation.base_branch.clone(),
        })
        .collect()
}

#[derive(serde::Serialize)]
struct MultiRequestUpdateRestartSnapshot {
    before_retry: Vec<UpdateSnapshot>,
    retry: Vec<UpdateSnapshot>,
}

fn multi_request_update_snapshot(trace: &AttemptTrace) -> Vec<UpdateSnapshot> {
    const PAYLOAD_PLACEHOLDER: &str = "{ repeated: U+0001, count: 90000 }";

    let payload = "\u{1}".repeat(MULTI_REQUEST_BODY_BYTES_BEFORE_JSON_ESCAPING);
    update_snapshot(trace)
        .into_iter()
        .map(|mut update| {
            let body = update
                .body
                .take()
                .expect("every multi-request update carries its rendered body")
                .join("\n");
            assert_eq!(
                body.match_indices(&payload).count(),
                1,
                "the exact large fixture payload occurs once in each rendered body"
            );
            let body = body.replacen(&payload, PAYLOAD_PLACEHOLDER, 1);
            assert!(
                !body.contains('\u{1}'),
                "no repeated fixture byte remains outside the exact replaced payload"
            );
            update.body = Some(body.split('\n').map(str::to_owned).collect());
            update
        })
        .collect()
}

/// Returns exactly the request aliases which a fresh observation must still
/// plan after one response became indeterminate.
fn unapplied_request_suffix<T: Clone>(
    complete: &[Box<[T]>],
    interrupted_batch: usize,
    applied_aliases: &[usize],
) -> Box<[T]> {
    let interrupted =
        complete.get(interrupted_batch).expect("the interruption selects a complete request batch");
    let applied = validated_aliases(applied_aliases, interrupted.len());
    complete
        .iter()
        .enumerate()
        .skip(interrupted_batch)
        .flat_map(|(batch_index, batch)| {
            let applied = &applied;
            batch
                .iter()
                .enumerate()
                .filter(move |(alias, _)| {
                    batch_index != interrupted_batch || !applied.contains(alias)
                })
                .map(|(_, effect)| effect.clone())
        })
        .collect()
}

/// Returns exactly the request aliases which were durable before an
/// indeterminate response stopped one request.
fn applied_request_prefix<T: Clone>(
    complete: &[Box<[T]>],
    interrupted_batch: usize,
    applied_aliases: &[usize],
) -> Box<[T]> {
    let interrupted =
        complete.get(interrupted_batch).expect("the interruption selects a complete request batch");
    let applied = validated_aliases(applied_aliases, interrupted.len());
    complete
        .iter()
        .enumerate()
        .take(interrupted_batch + 1)
        .flat_map(|(batch_index, batch)| {
            let applied = &applied;
            batch
                .iter()
                .enumerate()
                .filter(move |(alias, _)| {
                    batch_index != interrupted_batch || applied.contains(alias)
                })
                .map(|(_, effect)| effect.clone())
        })
        .collect()
}

fn assert_interrupted_create_state(
    world: &DurableWorld,
    intent: &LocalIntent,
    batch: &[TestCreate],
    applied_aliases: &[usize],
) {
    let applied = validated_aliases(applied_aliases, batch.len());
    for (alias, create) in batch.iter().enumerate() {
        let local = intent
            .iter()
            .find(|local| local.id == create.id)
            .expect("every planned create belongs to the local intent");
        assert_eq!(create.title, local.title);
        assert_eq!(create.head_oid, local.desired.head);
        assert_eq!(create.base_oid, local.desired.first_parent);
        let published = world
            .published(&create.id)
            .expect("the create stage follows durable tuple publication");
        let current = published.history.last();
        assert_eq!(current.head, create.head_oid);
        assert_eq!(current.first_parent, create.base_oid);
        if applied.contains(&alias) {
            let Some(ManagedPullRequest::Unmarked(pull_request)) = &published.pull_request else {
                panic!("a selected create alias must establish one unmarked OPEN row")
            };
            assert_eq!(pull_request.title, create.title);
            assert!(
                pull_request.body == create.body,
                "durable create body for {} differs from its applied create ({} != {} bytes)",
                create.id.as_str(),
                pull_request.body.len(),
                create.body.len()
            );
        } else {
            assert!(
                published.pull_request.is_none(),
                "an unselected create alias cannot establish an OPEN row"
            );
        }
    }
}

fn assert_interrupted_create_request_prefix(
    world: &DurableWorld,
    intent: &LocalIntent,
    complete: &[Box<[TestCreate]>],
    interrupted_batch: usize,
    applied_aliases: &[usize],
) {
    let applied = applied_request_prefix(complete, interrupted_batch, applied_aliases)
        .into_iter()
        .map(|create| (create.id.clone(), create))
        .collect::<HashMap<_, _>>();
    assert_eq!(
        applied.len(),
        complete.iter().take(interrupted_batch).map(|batch| batch.len()).sum::<usize>()
            + applied_aliases.len(),
        "one interrupted create prefix cannot repeat a local change"
    );
    for create in flatten(complete) {
        let selected = applied.get(&create.id);
        assert_interrupted_create_state(
            world,
            intent,
            std::slice::from_ref(&create),
            if selected.is_some() { &[0] } else { &[] },
        );
    }
}

fn body_recipes(world: &DurableWorld, intent: &LocalIntent) -> StackBodyRecipes {
    let default = DefaultBranch::new(DEFAULT_BRANCH.to_owned(), world.default_tip).unwrap();
    let stack = LocalStack::for_plan_test(
        default,
        intent.iter().map(|local| {
            (
                local.id.clone(),
                local.desired.head,
                local.desired.first_parent,
                local.title.clone(),
                local.commit_body.clone(),
            )
        }),
    );
    let histories = intent
        .iter()
        .map(|local| {
            let published = world.published(&local.id).expect("every local change is published");
            validated_history(
                local.id.clone(),
                &published
                    .history
                    .iter()
                    .map(|revision| (revision.head, revision.first_parent))
                    .collect::<Vec<_>>(),
                (local.desired.head, local.desired.first_parent),
                published
                    .pull_request
                    .as_ref()
                    .and_then(ManagedPullRequest::marker)
                    .map(MarkerIdentity::number),
            )
        })
        .collect();
    StackBodyRecipes::new(&PushDestination::for_test(), None, stack, histories).unwrap()
}

fn assert_complete_create_stage(
    retry: &AttemptReport,
    world: &DurableWorld,
    intent: &LocalIntent,
    label: &str,
) {
    let expected_bodies = body_recipes(world, intent)
        .provisional_bodies()
        .into_iter()
        .map(|body| body.into_parts())
        .map(|(id, body)| (id, body.into_string()))
        .collect::<HashMap<_, _>>();
    let creates = flatten(&retry.trace.creates);
    assert_eq!(
        creates.len(),
        intent.iter().count(),
        "{label}: retry must create every pull request after the Git barrier"
    );
    for (local, create) in intent.iter().zip(creates) {
        assert_eq!(create.id, local.id, "{label}: each create targets its exact local change");
        assert_eq!(create.title, local.title, "{label}: each create carries the exact title");
        assert_eq!(
            create.head_oid, local.desired.head,
            "{label}: each create uses the acknowledged local head"
        );
        assert_eq!(
            create.base_oid, local.desired.first_parent,
            "{label}: each create uses the acknowledged owned base"
        );
        assert_eq!(
            Some(&create.body),
            expected_bodies.get(&local.id),
            "{label}: each create carries its exact provisional body"
        );
    }
}

fn assert_create_retry_finishes_projection(
    retry: &AttemptReport,
    world: &DurableWorld,
    intent: &LocalIntent,
    label: &str,
) {
    let expected_markers =
        intent.iter().map(|local| expected_marker_effect(world, local)).collect::<Box<[_]>>();
    assert_eq!(
        flatten(&retry.trace.markers),
        expected_markers,
        "{label}: retry must mark every durable OPEN row at its exact local head"
    );

    assert_retry_finishes_projection(retry, world, intent, label);
}

fn assert_retry_finishes_projection(
    retry: &AttemptReport,
    world: &DurableWorld,
    intent: &LocalIntent,
    label: &str,
) {
    // Body rendering has its own exact snapshot tests. Re-rendering here from
    // the durable identities checks the stage boundary independently: the
    // retry must route every corresponding final body into an update and make
    // that exact value durable.
    let expected_bodies = expected_final_bodies(world, intent);

    let actual_updates = flatten(&retry.trace.updates);
    assert_eq!(
        actual_updates.len(),
        intent.iter().count(),
        "{label}: retry must finalize every provisional OPEN row"
    );

    for (index, (local, update)) in intent.iter().zip(actual_updates).enumerate() {
        assert_eq!(
            update.resolved_id.as_ref(),
            Some(&local.id),
            "{label}: each final update targets its exact local change"
        );
        assert_eq!(
            update.operation.identity,
            world.identity(&local.id),
            "{label}: each final update uses the durable OPEN identity"
        );
        let expected_body = expected_bodies.get(&local.id).unwrap();
        assert_eq!(
            update.operation.body.as_ref(),
            Some(expected_body),
            "{label}: each final update carries its exact rendered body"
        );
        assert_eq!(
            update.operation.base_branch.as_deref(),
            (index == 0).then_some(DEFAULT_BRANCH),
            "{label}: only the root update retargets from its owned base"
        );
    }

    assert_final_projection_state(world, intent, label);
}

fn expected_final_bodies(
    world: &DurableWorld,
    intent: &LocalIntent,
) -> HashMap<GherritPrId, String> {
    let assignments = intent
        .iter()
        .map(|local| (local.id.clone(), world.identity(&local.id).number()))
        .collect::<Vec<_>>();
    body_recipes(world, intent)
        .final_bodies(&assignments)
        .unwrap()
        .into_iter()
        .map(|body| body.into_parts())
        .map(|(id, body)| (id, body.into_string()))
        .collect()
}

fn expected_marker_effect(world: &DurableWorld, local: &LocalChange) -> MarkerEffect {
    let published = world.published(&local.id).expect("every marked local change is published");
    let v1 = published.history.first.head;
    let number = world.identity(&local.id).number();
    MarkerEffect { id: local.id.clone(), marker: marker_identity(v1, number) }
}

fn assert_final_projection_state(world: &DurableWorld, intent: &LocalIntent, label: &str) {
    let expected_bodies = expected_final_bodies(world, intent);
    for (index, local) in intent.iter().enumerate() {
        let published = world.published(&local.id).expect("every local change is published");
        assert_eq!(
            published.history.last(),
            local.desired,
            "{label}: each durable tuple reaches the exact local revision"
        );
        let Some(ManagedPullRequest::Marked { pull_request, marker, base }) =
            &published.pull_request
        else {
            panic!("{label}: every durable OPEN row must be marked after the retry")
        };
        assert_eq!(
            *marker,
            expected_marker_effect(world, local).marker,
            "{label}: the marker retains the exact canonical v1 and pull request identity"
        );
        assert_eq!(
            *base,
            if index == 0 { BaseKind::Default } else { BaseKind::Owned },
            "{label}: the finalized pull request has its exact local base kind"
        );
        assert_eq!(pull_request.title, local.title, "{label}: the finalized title is exact");
        let expected_body = expected_bodies.get(&local.id).unwrap();
        assert_eq!(
            &pull_request.body, expected_body,
            "{label}: each exact final body becomes durable"
        );
    }
}

fn establish_marked(world: &mut DurableWorld, change: &LocalChange, base: BaseKind, body: &str) {
    world.publish_for_setup(&change.id, change.desired);
    world.open_for_setup(&change.id, &change.title, body);
    world.mark_for_setup(&change.id, change.desired.head, base);
}

#[test]
fn canonical_marker_effect_binds_v1_and_the_exact_open_identity() {
    let default = oid(1);
    let first = LiteralRevision { head: oid(2), first_parent: default };
    let amended = LiteralRevision { head: oid(3), first_parent: default };
    let local = local_change("Gmarker", amended, "Marker", "Marker body");
    let intent = LocalIntent::new(default, [local.clone()]);
    let mut world = DurableWorld::for_intents(default, &[&intent]);
    world.publish_for_setup(&local.id, first);
    world.publish_for_setup(&local.id, amended);
    world.open_for_setup(&local.id, &local.title, "provisional");

    let number = world.identity(&local.id).number();
    let correct =
        MarkerEffect { id: local.id.clone(), marker: marker_identity(first.head, number) };
    let wrong_number = MarkerEffect {
        id: local.id.clone(),
        marker: marker_identity(first.head, PullRequestNumber::for_test(number.get() + 1)),
    };
    assert_eq!(
        world.apply_effect(&ExternalEffect::Marker(wrong_number)),
        EffectOutcome::Rejected,
        "a canonical marker for another pull request cannot bind this OPEN row"
    );

    let wrong_v1 =
        MarkerEffect { id: local.id.clone(), marker: marker_identity(amended.head, number) };
    assert_eq!(
        world.apply_effect(&ExternalEffect::Marker(wrong_v1)),
        EffectOutcome::Rejected,
        "a marker cannot target a later immutable version"
    );

    assert_eq!(
        world.apply_effect(&ExternalEffect::Marker(correct.clone())),
        EffectOutcome::Acknowledged
    );
    assert_eq!(world.open_pull_request(&local.id).unwrap().marker(), Some(correct.marker));
    assert_eq!(
        world.apply_effect(&ExternalEffect::Marker(correct)),
        EffectOutcome::Acknowledged,
        "an identical canonical marker is an acknowledged no-op"
    );
}

#[test]
fn updates_route_by_node_id_and_validate_the_complete_receipt_after_mutation() {
    let default = oid(1);
    let revision = LiteralRevision { head: oid(2), first_parent: default };
    let intent = root_intent(default, "Groute", revision);
    let mut world = DurableWorld::for_intents(default, &[&intent]);
    establish_marked(&mut world, &intent.first, BaseKind::Owned, "old body");

    let unknown = world.resolve_update(TestUpdate {
        identity: PullRequestIdentity::for_plan_test(999, "UNKNOWN_NODE"),
        title: Some("not applied".to_owned()),
        body: Some("not applied".to_owned()),
        base_branch: Some(DEFAULT_BRANCH.to_owned()),
    });
    assert_eq!(unknown.resolved_id, None);
    let before_unknown = world.clone();
    assert_eq!(world.apply_effect(&ExternalEffect::Update(unknown)), EffectOutcome::Indeterminate);
    assert_eq!(world, before_unknown, "an unknown node ID cannot select a durable row");

    let identity = world.identity(&id("Groute"));
    let invalid_base = world.resolve_update(TestUpdate {
        identity: identity.clone(),
        title: Some("not applied".to_owned()),
        body: Some("not applied".to_owned()),
        base_branch: Some(owned_base_name(&id("Gother"))),
    });
    assert_eq!(invalid_base.resolved_id, Some(id("Groute")));
    let before_invalid_base = world.clone();
    assert_eq!(
        world.apply_effect(&ExternalEffect::Update(invalid_base)),
        EffectOutcome::Indeterminate
    );
    assert_eq!(world, before_invalid_base, "base validation precedes every field mutation");

    let mismatched_receipt = world.resolve_update(TestUpdate {
        identity: PullRequestIdentity::for_plan_test(
            identity.number().get() + 1,
            identity.node_id_for_test(),
        ),
        title: Some("new title".to_owned()),
        body: Some("new body".to_owned()),
        base_branch: Some(DEFAULT_BRANCH.to_owned()),
    });
    assert_eq!(mismatched_receipt.resolved_id, Some(id("Groute")));
    assert_eq!(
        world.apply_effect(&ExternalEffect::Update(mismatched_receipt)),
        EffectOutcome::Indeterminate,
        "a wrong expected number makes the receipt indeterminate"
    );
    let ManagedPullRequest::Marked { pull_request, base, .. } =
        world.open_pull_request(&id("Groute")).unwrap()
    else {
        panic!("the fixture remains marked")
    };
    assert_eq!(*base, BaseKind::Default);
    assert_eq!(pull_request.title, "new title");
    assert_eq!(pull_request.body, "new body");
}

#[test]
#[should_panic(expected = "planner cannot update an unmarked OPEN pull request")]
fn updating_an_unmarked_open_row_is_an_oracle_invariant_violation() {
    let default = oid(1);
    let revision = LiteralRevision { head: oid(2), first_parent: default };
    let intent = root_intent(default, "Gunmarked", revision);
    let mut world = DurableWorld::for_intents(default, &[&intent]);
    world.publish_for_setup(&intent.first.id, revision);
    world.open_for_setup(&intent.first.id, "old title", "old body");
    let operation = TestUpdate {
        identity: world.identity(&intent.first.id),
        title: Some("new title".to_owned()),
        body: None,
        base_branch: None,
    };
    let effect = world.resolve_update(operation);
    world.apply_effect(&ExternalEffect::Update(effect));
}

#[tokio::test]
async fn every_small_attempt_prefix_restarts_from_literal_durable_state() {
    let intent = two_change_intent();
    let initial = DurableWorld::for_intents(oid(10), &[&intent]);
    let mut completed = initial.clone();
    let report = completed.run_attempt(&intent, &QueryVisibility::default(), None).await.unwrap();
    assert_eq!(report.outcome, AttemptOutcome::Acknowledged);
    assert_eq!(report.trace.initial_refs.iter().map(|batch| batch.len()).collect::<Vec<_>>(), [2]);
    assert_eq!(report.trace.creates.iter().map(|batch| batch.len()).collect::<Vec<_>>(), [2]);
    assert_eq!(report.trace.markers.iter().map(|batch| batch.len()).collect::<Vec<_>>(), [2]);
    assert_eq!(report.trace.updates.iter().map(|batch| batch.len()).collect::<Vec<_>>(), [2]);
    insta::assert_yaml_snapshot!("two_change_provisional_creates", create_snapshot(&report.trace));
    insta::assert_yaml_snapshot!("two_change_final_updates", update_snapshot(&report.trace));

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
        if let Interruption::Create { batch, applied_aliases } = &interruption {
            assert!(interrupted.changes.values().all(|change| {
                change
                    .pull_request
                    .as_ref()
                    .is_none_or(|pull_request| pull_request.marker().is_none())
            }));
            assert!(stopped.trace.markers.is_empty() && stopped.trace.updates.is_empty());
            assert_interrupted_create_state(
                &interrupted,
                &intent,
                &stopped.trace.creates[*batch],
                applied_aliases,
            );
        }
        let label = format!("prefix-{index}");
        let retry = run_acknowledged_retry(&mut interrupted, &intent, &label).await;
        if let Interruption::Create { batch, applied_aliases } = &interruption {
            assert!(retry.trace.initial_refs.is_empty());
            assert_eq!(
                flatten(&retry.trace.creates),
                unapplied_request_suffix(&report.trace.creates, *batch, applied_aliases),
                "{label}: retry must create exactly the aliases which are not durable"
            );
            assert_create_retry_finishes_projection(&retry, &interrupted, &intent, &label);
        }
        if let Interruption::Update { batch, applied_aliases } = &interruption {
            assert!(retry.trace.initial_refs.is_empty());
            assert!(retry.trace.creates.is_empty());
            assert!(retry.trace.markers.is_empty());
            assert_eq!(
                flatten(&retry.trace.updates),
                unapplied_request_suffix(&report.trace.updates, *batch, applied_aliases),
                "{label}: retry must omit exactly the update aliases already durable"
            );
        }
        assert_quiescent(&mut interrupted, &intent, &label).await;
    }

    let done = completed.run_attempt(&intent, &QueryVisibility::default(), None).await.unwrap();
    assert!(done.trace.is_empty());
}

#[tokio::test]
async fn git_restarts_expose_only_complete_atomic_batch_prefixes() {
    let tuple_intent = many_bounded_ids_intent(30, 128);
    let tuple_initial = DurableWorld::for_intents(oid(1), &[&tuple_intent]);
    let mut tuple_complete = tuple_initial.clone();
    let tuple_report =
        tuple_complete.run_attempt(&tuple_intent, &QueryVisibility::default(), None).await.unwrap();
    assert!(
        tuple_report.trace.initial_refs.len() > 1,
        "the fixture crosses the initial-ref push budget"
    );
    for stopped_batch in 0..tuple_report.trace.initial_refs.len() {
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
            assert_eq!(
                stopped.trace.initial_refs,
                tuple_report.trace.initial_refs[..=stopped_batch]
            );
            let label = format!("initial-ref-batch-{stopped_batch}-applied-{applied}");
            let retry = run_acknowledged_retry(&mut world, &tuple_intent, &label).await;
            assert_eq!(
                flatten(&retry.trace.initial_refs),
                flatten(&tuple_report.trace.initial_refs[stopped_batch + usize::from(applied)..]),
                "an initial-ref retry retains the exact unpublished atomic-batch suffix"
            );
            assert_complete_create_stage(&retry, &world, &tuple_intent, &label);
            assert_create_retry_finishes_projection(&retry, &world, &tuple_intent, &label);
            assert_quiescent(&mut world, &tuple_intent, &label).await;
        }
    }

    let marker_intent = many_bounded_ids_intent(90, 128);
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
    assert!(marker_report.trace.initial_refs.is_empty());
    assert!(marker_report.trace.creates.is_empty());
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
            let label = format!("marker-batch-{stopped_batch}-applied-{applied}");
            let retry = run_acknowledged_retry(&mut world, &marker_intent, &label).await;
            assert!(retry.trace.initial_refs.is_empty());
            assert!(retry.trace.creates.is_empty());
            assert_eq!(
                flatten(&retry.trace.markers),
                flatten(&marker_report.trace.markers[stopped_batch + usize::from(applied)..]),
                "a marker retry retains the exact unpublished atomic-batch suffix"
            );
            assert_retry_finishes_projection(&retry, &world, &marker_intent, &label);
            assert_quiescent(&mut world, &marker_intent, &label).await;
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
    let creates = &completed.trace.creates;
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

    let applied_alias_schedules: [&[usize]; 2] = [&[], &[0]];
    for applied_aliases in applied_alias_schedules {
        let label = format!("multi-request-create-applied-{applied_aliases:?}");
        let mut interrupted_create = initial.clone();
        let stopped = interrupted_create
            .run_attempt(
                &intent,
                &QueryVisibility::default(),
                Some(Interruption::Create { batch: 1, applied_aliases: applied_aliases.into() }),
            )
            .await
            .unwrap();
        assert_eq!(stopped.outcome, AttemptOutcome::Stopped(StopReason::Indeterminate));
        assert_eq!(
            stopped.trace.creates.iter().map(|batch| batch.len()).collect::<Vec<_>>(),
            [1, 1],
            "an earlier request is acknowledged, the current response is lost, and the tail is unsent"
        );
        assert!(stopped.trace.markers.is_empty() && stopped.trace.updates.is_empty());
        assert_interrupted_create_request_prefix(
            &interrupted_create,
            &intent,
            creates,
            1,
            applied_aliases,
        );
        let retry = run_acknowledged_retry(&mut interrupted_create, &intent, &label).await;
        assert!(retry.trace.initial_refs.is_empty());
        assert_eq!(
            flatten(&retry.trace.creates),
            unapplied_request_suffix(creates, 1, applied_aliases),
            "{label}: retry must contain exactly the unapplied create-request suffix"
        );
        assert_create_retry_finishes_projection(&retry, &interrupted_create, &intent, &label);
        assert_quiescent(&mut interrupted_create, &intent, &label).await;
    }

    for applied_aliases in applied_alias_schedules {
        let label = format!("multi-request-update-applied-{applied_aliases:?}");
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
                Some(Interruption::Update { batch: 1, applied_aliases: applied_aliases.into() }),
            )
            .await
            .unwrap();
        assert_eq!(
            stopped.trace.updates.iter().map(|batch| batch.len()).collect::<Vec<_>>(),
            [1, 1]
        );
        let durable_updates = applied_request_prefix(&completed.trace.updates, 1, applied_aliases);
        for local in intent.iter() {
            let actual = &before_updates.open_pull_request(&local.id).unwrap().pull_request().body;
            let expected = durable_updates
                .iter()
                .find(|update| update.resolved_id.as_ref() == Some(&local.id))
                .map(|update| {
                    update.operation.body.as_deref().expect("the fixture changes every body")
                })
                .unwrap_or("provisional");
            assert!(
                actual == expected,
                "{label}: durable body for {} differs from its applied prefix ({} != {} bytes)",
                local.id.as_str(),
                actual.len(),
                expected.len()
            );
        }
        let retry = run_acknowledged_retry(&mut before_updates, &intent, &label).await;
        assert!(retry.trace.initial_refs.is_empty());
        assert!(retry.trace.creates.is_empty());
        assert!(retry.trace.markers.is_empty());
        assert_eq!(
            flatten(&retry.trace.updates),
            unapplied_request_suffix(&completed.trace.updates, 1, applied_aliases),
            "{label}: retry must contain exactly the unapplied update-request suffix"
        );
        assert_final_projection_state(&before_updates, &intent, &label);
        if applied_aliases == [0] {
            let snapshot = MultiRequestUpdateRestartSnapshot {
                before_retry: multi_request_update_snapshot(&stopped.trace),
                retry: multi_request_update_snapshot(&retry.trace),
            };
            insta::assert_yaml_snapshot!("multi_request_update_restart", snapshot);
        }
        assert_quiescent(&mut before_updates, &intent, &label).await;
    }

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
    let duplicate_creates = &first.trace.creates;
    let retained_identity = duplicate_world.identity(&duplicate_creates[0][0].id);
    let next_before = duplicate_world.next_identity;
    let duplicate = duplicate_world.execute_plan(stale_plan, None).await.unwrap();
    assert_eq!(duplicate.outcome, AttemptOutcome::Stopped(StopReason::Indeterminate));
    assert_eq!(duplicate.trace.creates.len(), 1);
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
    assert!(duplicate.trace.initial_refs.is_empty());
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
        flatten(&attempt.trace.initial_refs)
            .iter()
            .map(|effect| {
                let tuple = effect.tuple();
                (tuple.id.clone(), tuple.version)
            })
            .collect::<Vec<_>>(),
        [(id("Groot"), 2), (id("Gchild"), 2)]
    );
    assert!(attempt.trace.markers.is_empty(), "older durable markers remain authoritative");
    assert!(
        flatten(&attempt.trace.updates).iter().all(|update| update.operation.base_branch.is_none())
    );
    insta::assert_yaml_snapshot!("stale_amend_rebase_updates", update_snapshot(&attempt.trace));

    let stale_retry = world.run_attempt(&new_intent, &stale, None).await.unwrap();
    assert!(stale_retry.trace.initial_refs.is_empty());
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
            assert!(report.trace.initial_refs.is_empty());
            assert!(report.trace.creates.is_empty());
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
    let old_marker = world.open_pull_request(&id("Gmove")).unwrap().marker();
    let stale = QueryVisibility::stale(&world, [id("Gmove")]);

    let attempt = world.run_attempt(&new_intent, &stale, None).await.unwrap();
    assert_eq!(
        flatten(&attempt.trace.initial_refs)
            .iter()
            .map(|effect| effect.tuple().version)
            .collect::<Vec<_>>(),
        [1, 2]
    );
    assert_eq!(
        attempt.trace.creates.iter().flatten().map(|create| create.id.clone()).collect::<Vec<_>>(),
        [id("Gnew")]
    );
    assert_eq!(
        flatten(&attempt.trace.markers).iter().map(|marker| marker.id.clone()).collect::<Vec<_>>(),
        [id("Gnew")]
    );
    insta::assert_yaml_snapshot!("reorder_exact_final_updates", update_snapshot(&attempt.trace));
    assert_eq!(
        world.open_pull_request(&id("Gmove")).unwrap().marker(),
        old_marker,
        "the immutable canonical marker remains byte-for-byte unchanged"
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
    assert_eq!(flatten(&attempt.trace.initial_refs).len(), 1);
    assert!(attempt.trace.creates.is_empty());
    assert!(attempt.trace.markers.is_empty());
    let updates = flatten(&attempt.trace.updates);
    assert_eq!(updates.len(), 1);
    assert_eq!(updates[0].resolved_id, Some(id("Gmove")));
    assert_eq!(updates[0].operation.title.as_deref(), Some("Moved root"));
    assert_eq!(updates[0].operation.base_branch.as_deref(), Some(DEFAULT_BRANCH));
    assert_eq!(world.published(&id("Gparent")), parent_before.as_ref());

    let stale_retry = world.run_attempt(&new_intent, &stale, None).await.unwrap();
    assert_eq!(
        flatten(&stale_retry.trace.updates),
        updates,
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

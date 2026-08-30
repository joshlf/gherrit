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
    body::StackBodyRecipes,
    github::{
        AbsentPullRequest, BaseKind, CompleteCreateReceipts, CompleteLocalPullRequests,
        LocalPullRequestObservation, ManagedOpenPullRequests, ObservedBase, PreparedCreates,
        PreparedPullRequestProjection, PullRequestIdentity, TestClose, TestCreate,
        TestPullRequestProjection, TestUpdate,
    },
    history::ValidatedChangeHistory,
    plan::{EffectDriver, PlannedPublication, plan_effects, plan_public_branch},
    refs::{PreparedPushes, PublicationRevision, TestPushEffect},
};
use crate::{
    manage::PublicBranchName,
    pre_push::{
        destination::{
            DefaultBranch, ObservedPublicBranch, PushDestination, RemoteBranchState,
            RepositoryCoordinates,
        },
        local::{GherritPrId, LocalStack},
    },
};

const DEFAULT_BRANCH: &str = "main";
const PUBLIC_BRANCH: &str = "release-candidate";
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
    public_branch: Option<PublicBranchName>,
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
        Self { first, later, public_branch: None }
    }

    fn with_public_branch(mut self, branch: &str) -> Self {
        assert!(self.public_branch.is_none(), "a local intent selects at most one public branch");
        self.public_branch = Some(public_branch_name(branch));
        self
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

/// Literal durable fields of one pull request row.
///
/// The Git head and owned-base object IDs are deliberately absent: those are
/// values of the mutable Git refs. One query can return an older view of them,
/// represented separately by [`QueryVisibility`].
#[derive(Clone, Debug, Eq, PartialEq)]
struct PullRequest {
    identity: PullRequestIdentity,
    title: String,
    body: String,
    base: BaseKind,
    state: PullRequestState,
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum PullRequestState {
    Open,
    Closed,
}

impl PullRequest {
    fn open_fields(&self) -> Option<(&str, &str, BaseKind)> {
        match self.state {
            PullRequestState::Open => Some((&self.title, &self.body, self.base)),
            PullRequestState::Closed => None,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct PublishedChange {
    history: PublishedHistory,
    marker: Option<ObjectId>,
    pull_requests: Vec<PullRequest>,
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
    public_branches: HashMap<PublicBranchName, ObjectId>,
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
    open: HashMap<PullRequestIdentity, OpenVisibility>,
}

impl QueryVisibility {
    fn hiding_identities(
        world: &DurableWorld,
        identities: impl IntoIterator<Item = PullRequestIdentity>,
    ) -> Self {
        let mut visibility = Self::default();
        for identity in identities {
            assert!(
                world.exact_open_pull_request(&identity).is_some(),
                "only an OPEN row can hide"
            );
            assert!(
                visibility.open.insert(identity, OpenVisibility::Hidden).is_none(),
                "one query has one visibility state per identity"
            );
        }
        visibility
    }

    fn stale(world: &DurableWorld, ids: impl IntoIterator<Item = GherritPrId>) -> Self {
        let mut visibility = Self::default();
        for id in ids {
            let identities = world
                .open_pull_requests(&id)
                .map(|pull_request| pull_request.identity.clone())
                .collect::<Vec<_>>();
            assert!(!identities.is_empty(), "a stale OPEN connection contains an OPEN row");
            for identity in identities {
                let fields = world.current_open_fields(&id, &identity);
                assert!(
                    visibility.open.insert(identity, OpenVisibility::Stale(fields)).is_none(),
                    "one query has one visibility state per identity"
                );
            }
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
        let identity = world.identity(&id);
        let mut fields = world.current_open_fields(&id, &identity);
        let change = world.published(&id).expect("an OPEN row has published history");
        fields.head_oid = change.history.get(head_slot).expect("valid historical head slot").head;
        let historical_base =
            change.history.get(base_slot).expect("valid historical base slot").first_parent;
        fields.base_oid = match fields.base {
            BaseKind::Default => world.default_tip,
            BaseKind::Owned => historical_base,
        };
        Self { open: HashMap::from([(identity, OpenVisibility::Stale(fields))]) }
    }

    fn open(&self, identity: &PullRequestIdentity) -> Option<&OpenVisibility> {
        self.open.get(identity)
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

    fn open_pull_requests(&self, id: &GherritPrId) -> impl Iterator<Item = &PullRequest> {
        self.published(id)
            .into_iter()
            .flat_map(|change| change.pull_requests.iter())
            .filter(|pull_request| pull_request.open_fields().is_some())
    }

    fn exact_open_pull_request(&self, identity: &PullRequestIdentity) -> Option<&PullRequest> {
        self.changes.values().flat_map(|change| change.pull_requests.iter()).find(|pull_request| {
            pull_request.identity == *identity && pull_request.open_fields().is_some()
        })
    }

    fn open_pull_request(&self, id: &GherritPrId) -> Option<&PullRequest> {
        self.open_pull_requests(id).min_by_key(|pull_request| pull_request.identity.number().get())
    }

    fn publish_for_setup(&mut self, id: &GherritPrId, revision: LiteralRevision) {
        match self.changes.get_mut(id) {
            None => {
                self.changes.insert(
                    id.clone(),
                    PublishedChange {
                        history: PublishedHistory::new(revision),
                        marker: None,
                        pull_requests: Vec::new(),
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
        change.pull_requests.push(PullRequest {
            identity,
            title: title.to_owned(),
            body: body.to_owned(),
            base: BaseKind::Owned,
            state: PullRequestState::Open,
        });
        self.assert_well_formed();
    }

    fn mark_for_setup(&mut self, id: &GherritPrId, target: ObjectId, base: BaseKind) {
        let change = self.published_mut(id).expect("a marker requires published history");
        assert!(change.history.contains_head(target), "a marker targets immutable history");
        assert!(change.marker.replace(target).is_none(), "test setup cannot move a marker");
        let canonical = change
            .pull_requests
            .iter_mut()
            .filter(|pull_request| pull_request.open_fields().is_some())
            .min_by_key(|pull_request| pull_request.identity.number().get())
            .expect("a marker requires an OPEN identity");
        canonical.base = base;
        self.assert_well_formed();
    }

    fn allocate_identity(&mut self) -> PullRequestIdentity {
        let number = self.next_identity;
        self.next_identity = self.next_identity.checked_add(1).expect("test identity space");
        PullRequestIdentity::for_plan_test(number, &format!("PR_{number}"))
    }

    fn identity(&self, id: &GherritPrId) -> PullRequestIdentity {
        self.open_pull_requests(id)
            .min_by_key(|pull_request| pull_request.identity.number().get())
            .expect("the requested change has an OPEN identity")
            .identity
            .clone()
    }

    fn latest_identity(&self, id: &GherritPrId) -> PullRequestIdentity {
        self.open_pull_requests(id)
            .max_by_key(|pull_request| pull_request.identity.number().get())
            .expect("the requested change has an OPEN identity")
            .identity
            .clone()
    }

    fn current_open_fields(
        &self,
        id: &GherritPrId,
        identity: &PullRequestIdentity,
    ) -> ObservedOpenFields {
        let change = self.published(id).expect("an OPEN row has published history");
        let current = change.history.last();
        let pull_request = change
            .pull_requests
            .iter()
            .find(|pull_request| pull_request.identity == *identity)
            .expect("the requested change has the exact OPEN row");
        let (title, body, base) = pull_request.open_fields().expect("the requested row is OPEN");
        ObservedOpenFields {
            head_oid: current.head,
            base,
            base_oid: match base {
                BaseKind::Default => self.default_tip,
                BaseKind::Owned => current.first_parent,
            },
            title: title.to_owned(),
            body: body.to_owned(),
        }
    }

    /// Returns the OPEN rows seen by this one query.
    ///
    /// A stale override is an earlier query result, so it remains visible even
    /// if the durable row has since closed. Without an override, only the
    /// row's current OPEN state is visible. Keeping only that materialization
    /// rule shared makes the duplicate oracle and the production planner
    /// consume the same modeled response while selecting the canonical row
    /// independently.
    fn observed_open_rows(
        &self,
        id: &GherritPrId,
        visibility: &QueryVisibility,
    ) -> Vec<(PullRequestIdentity, ObservedOpenFields)> {
        self.published(id)
            .into_iter()
            .flat_map(|change| change.pull_requests.iter())
            .filter_map(|pull_request| match visibility.open(&pull_request.identity) {
                Some(OpenVisibility::Hidden) => None,
                Some(OpenVisibility::Stale(fields)) => {
                    Some((pull_request.identity.clone(), fields.clone()))
                }
                None if pull_request.open_fields().is_some() => Some((
                    pull_request.identity.clone(),
                    self.current_open_fields(id, &pull_request.identity),
                )),
                None => None,
            })
            .collect()
    }

    fn plan(
        &self,
        intent: &LocalIntent,
        visibility: &QueryVisibility,
    ) -> Result<PlannedPublication> {
        assert!(
            visibility.open.keys().all(|identity| {
                intent.iter().any(|local| {
                    self.published(&local.id).is_some_and(|change| {
                        change.pull_requests.iter().any(|row| &row.identity == identity)
                    })
                })
            }),
            "every visibility override belongs to an exact row in the queried intent"
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
        let observed_public_branch = intent.public_branch.clone().map(|name| {
            let state = self
                .public_branches
                .get(&name)
                .copied()
                .map_or(RemoteBranchState::Absent, RemoteBranchState::At);
            ObservedPublicBranch::for_test(name, state)
        });
        let public_branch = plan_public_branch(observed_public_branch, &default, stack.tip())?;
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
                        change.marker.is_some(),
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
        let pull_requests = intent
            .iter()
            .map(|local| self.observe_pull_request(&local.id, visibility))
            .collect::<Result<Vec<_>>>()?;
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
        let expected_closes = self.expected_duplicate_identities(intent, visibility);
        let plan = self.plan(intent, visibility)?;
        let report = self.execute_plan(plan, interruption).await?;
        let emitted = flatten(&report.trace.projections)
            .iter()
            .filter_map(|effect| match effect {
                ProjectionEffect::Close { operation, .. } => Some(operation.identity.clone()),
                ProjectionEffect::Update(_) => None,
            })
            .collect::<HashSet<_>>();
        assert!(
            emitted.is_subset(&expected_closes),
            "the planner may close only higher identities visible to its observation"
        );
        if report.outcome == AttemptOutcome::Acknowledged {
            assert_eq!(
                emitted, expected_closes,
                "an acknowledged projection closes every higher visible identity"
            );
        }
        Ok(report)
    }

    fn expected_duplicate_identities(
        &self,
        intent: &LocalIntent,
        visibility: &QueryVisibility,
    ) -> HashSet<PullRequestIdentity> {
        intent
            .iter()
            .flat_map(|local| {
                let visible = self.observed_open_rows(&local.id, visibility);
                let canonical = visible.iter().map(|(identity, _)| identity.number().get()).min();
                visible
                    .into_iter()
                    .filter(move |(identity, _)| Some(identity.number().get()) != canonical)
                    .map(|(identity, _)| identity)
            })
            .collect()
    }

    async fn execute_plan(
        &mut self,
        plan: PlannedPublication,
        interruption: Option<Interruption>,
    ) -> Result<AttemptReport> {
        let mut driver = WorldDriver::new(self, interruption);
        let result = plan.execute_with(&mut driver).await;
        Ok(driver.finish(result)?.into_solo())
    }

    fn observe_pull_request(
        &self,
        id: &GherritPrId,
        visibility: &QueryVisibility,
    ) -> Result<LocalPullRequestObservation> {
        let mut visible = self.observed_open_rows(id, visibility);
        visible.sort_by_key(|(identity, _)| identity.number().get());
        let Some((canonical_identity, canonical_fields)) = visible.first().cloned() else {
            if self.published(id).is_some_and(|change| {
                change
                    .pull_requests
                    .iter()
                    .any(|pull_request| pull_request.state == PullRequestState::Closed)
            }) {
                bail!("Terminal pull request history exists without a visible OPEN row");
            }
            return Ok(LocalPullRequestObservation::Absent(AbsentPullRequest::for_plan_test(
                id.clone(),
            )));
        };
        Ok(LocalPullRequestObservation::Open(
            ManagedOpenPullRequests::for_plan_test(
                id.clone(),
                canonical_identity,
                canonical_fields.head_oid,
                ObservedBase::for_plan_test(canonical_fields.base, canonical_fields.base_oid),
                &canonical_fields.title,
                &canonical_fields.body,
                false,
            )
            .with_duplicates_for_plan_test(
                visible
                    .into_iter()
                    .skip(1)
                    .map(|(identity, fields)| {
                        (
                            identity,
                            fields.head_oid,
                            ObservedBase::for_plan_test(fields.base, fields.base_oid),
                            false,
                        )
                    })
                    .collect(),
            ),
        ))
    }

    fn assert_well_formed(&self) {
        for (name, target) in &self.public_branches {
            assert!(!target.is_null(), "a durable public branch target is non-null");
            assert!(
                self.public_branches
                    .keys()
                    .filter(|other| *other != name)
                    .all(|other| !ref_paths_conflict(name.as_str(), other.as_str())),
                "durable public branch refs cannot have a directory/file conflict"
            );
        }
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
            for pull_request in &published.pull_requests {
                assert!(
                    numbers.insert(pull_request.identity.number()),
                    "durable pull request numbers are unique"
                );
                assert!(
                    node_ids.insert(pull_request.identity.node_id_for_test().to_owned()),
                    "durable pull request node IDs are unique"
                );
                assert!(
                    pull_request.identity.number().get() < self.next_identity,
                    "the allocation cursor follows every durable identity"
                );
            }
            if let Some(marker) = published.marker {
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

fn public_branch_name(value: &str) -> PublicBranchName {
    PublicBranchName::new(value.to_owned()).unwrap()
}

fn ref_paths_conflict(left: &str, right: &str) -> bool {
    left == right
        || left.strip_prefix(right).is_some_and(|suffix| suffix.starts_with('/'))
        || right.strip_prefix(left).is_some_and(|suffix| suffix.starts_with('/'))
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

/// One semantic effect in the attempt's first atomic ref stage.
///
/// The optional public projection remains distinct from each indivisible
/// change tuple even when both share one atomic batch.
#[derive(Clone, Debug, Eq, PartialEq)]
enum InitialRefEffect {
    Tuple(TupleEffect),
    PublicBranch { branch: PublicBranchName, expected: Option<ObjectId>, desired: ObjectId },
}

impl InitialRefEffect {
    fn tuple(&self) -> &TupleEffect {
        match self {
            Self::Tuple(tuple) => tuple,
            Self::PublicBranch { .. } => panic!("the requested initial-ref effect is not a tuple"),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct MarkerEffect {
    id: GherritPrId,
    target: ObjectId,
}

/// One GitHub projection request annotated only after resolving its node ID.
#[derive(Clone, Debug, Eq, PartialEq)]
struct UpdateEffect {
    resolved_id: Option<GherritPrId>,
    operation: TestUpdate,
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum ProjectionEffect {
    Close { resolved_id: Option<GherritPrId>, operation: TestClose },
    Update(UpdateEffect),
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
struct AttemptTrace {
    initial_refs: EffectBatches<InitialRefEffect>,
    creates: EffectBatches<TestCreate>,
    markers: EffectBatches<MarkerEffect>,
    projections: EffectBatches<ProjectionEffect>,
}

impl AttemptTrace {
    fn is_empty(&self) -> bool {
        self.initial_refs.is_empty()
            && self.creates.is_empty()
            && self.markers.is_empty()
            && self.projections.is_empty()
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

/// The only process outcomes which deliberately stop a modeled Git stage.
///
/// A known rejection proves that the atomic batch did not land. An
/// indeterminate result carries the two durable worlds a lost process result
/// permits: the whole batch landed or none of it did.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum GitStop {
    Rejected,
    Indeterminate { applied: bool },
}

#[derive(Clone, Debug)]
enum Interruption {
    InitialRefs { batch: usize, stop: GitStop },
    Create { batch: usize, applied_aliases: Box<[usize]> },
    Marker { batch: usize, stop: GitStop },
    Projection { batch: usize, applied_aliases: Box<[usize]> },
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
    Projection,
}

/// A point where another complete publisher may run while the primary keeps
/// its already-planned, attempt-local authority.
///
/// Only the first three stages release later authority. A between-batches
/// point is reached after the named batch is acknowledged and only when a
/// later serialized batch remains.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum CompetitionBoundary {
    AfterInitialRefs,
    AfterCreates,
    AfterMarkers,
    BetweenBatches { stage: EffectStage, completed_batch: usize },
}

enum Competition {
    Pending {
        boundary: CompetitionBoundary,
        plan: PlannedPublication,
        interruption: Option<Interruption>,
    },
    Complete(AttemptReport),
}

enum DriverReport {
    Solo(AttemptReport),
    Competition { primary: AttemptReport, competitor: AttemptReport },
}

impl DriverReport {
    fn into_solo(self) -> AttemptReport {
        let Self::Solo(report) = self else {
            panic!("this execution expected one publisher but received two reports")
        };
        report
    }

    fn into_competition(self) -> (AttemptReport, AttemptReport) {
        let Self::Competition { primary, competitor } = self else {
            panic!("a scheduled driver must produce both reports")
        };
        (primary, competitor)
    }
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
    competition: Option<Competition>,
}

impl<'world> WorldDriver<'world> {
    fn new(world: &'world mut DurableWorld, interruption: Option<Interruption>) -> Self {
        Self {
            world,
            interruption,
            interruption_consumed: false,
            failure: None,
            trace: AttemptTrace::default(),
            competition: None,
        }
    }

    fn with_competition(
        world: &'world mut DurableWorld,
        boundary: CompetitionBoundary,
        plan: PlannedPublication,
        interruption: Option<Interruption>,
    ) -> Self {
        let mut driver = Self::new(world, None);
        driver.competition = Some(Competition::Pending { boundary, plan, interruption });
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
            Some(Interruption::Projection { batch: stopped, .. }) => {
                stage == EffectStage::Projection && *stopped == batch
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

    fn stop_git_batch<T: Clone>(
        &mut self,
        stop: GitStop,
        effects: &[T],
        wrap: impl Fn(T) -> ExternalEffect,
    ) -> Result<()> {
        match stop {
            GitStop::Rejected => self.stop(StopReason::Rejected),
            GitStop::Indeterminate { applied: false } => self.stop(StopReason::Indeterminate),
            GitStop::Indeterminate { applied: true } => {
                let outcome = apply_atomic_git_batch(self.world, effects, wrap);
                assert_eq!(
                    outcome,
                    EffectOutcome::Acknowledged,
                    "the selected fully-applied world requires the whole atomic batch to apply"
                );
                self.stop(StopReason::Indeterminate)
            }
        }
    }

    /// Runs an independently planned publisher at one reached production
    /// boundary. Both publishers consume their real plans through the same
    /// driver implementation; the schedule never extracts or replays effects.
    async fn run_competition_at(&mut self, boundary: CompetitionBoundary) -> Result<()> {
        let Some(competition) = self.competition.take() else {
            return Ok(());
        };
        let (plan, interruption) = match competition {
            Competition::Complete(report) => {
                self.competition = Some(Competition::Complete(report));
                return Ok(());
            }
            Competition::Pending { boundary: scheduled, plan, interruption }
                if scheduled == boundary =>
            {
                (plan, interruption)
            }
            pending @ Competition::Pending { .. } => {
                self.competition = Some(pending);
                return Ok(());
            }
        };
        let mut driver = WorldDriver::new(self.world, interruption);
        let result = Box::pin(plan.execute_with(&mut driver)).await;
        let report = driver.finish(result)?.into_solo();
        self.competition = Some(Competition::Complete(report));
        Ok(())
    }

    fn finish_attempt(&self, result: Result<()>) -> Result<AttemptOutcome> {
        if self.interruption.is_some() {
            assert!(self.interruption_consumed, "the configured interruption was not reached");
        }
        let outcome = match (result, self.failure) {
            (Ok(()), None) => AttemptOutcome::Acknowledged,
            (Err(_), Some(outcome)) => AttemptOutcome::Stopped(outcome),
            (Err(error), None) => return Err(error),
            (Ok(()), Some(_)) => panic!("a stopped driver cannot release a later stage"),
        };
        Ok(outcome)
    }

    fn finish(self, result: Result<()>) -> Result<DriverReport> {
        let outcome = self.finish_attempt(result)?;
        let primary = AttemptReport { outcome, trace: self.trace };
        Ok(match self.competition {
            None => DriverReport::Solo(primary),
            Some(Competition::Complete(competitor)) => {
                DriverReport::Competition { primary, competitor }
            }
            Some(Competition::Pending { .. }) => {
                panic!("the configured competition boundary was not reached")
            }
        })
    }
}

async fn execute_with_competition(
    world: &mut DurableWorld,
    primary: PlannedPublication,
    competing: PlannedPublication,
    boundary: CompetitionBoundary,
    competing_interruption: Option<Interruption>,
) -> Result<(AttemptReport, AttemptReport)> {
    let mut driver =
        WorldDriver::with_competition(world, boundary, competing, competing_interruption);
    let result = primary.execute_with(&mut driver).await;
    Ok(driver.finish(result)?.into_competition())
}

fn validated_aliases(aliases: &[usize], batch_len: usize) -> HashSet<usize> {
    let selected = aliases.iter().copied().collect::<HashSet<_>>();
    assert_eq!(selected.len(), aliases.len(), "an interruption cannot repeat an alias");
    assert!(selected.iter().all(|index| *index < batch_len), "interrupted alias is in range");
    selected
}

impl EffectDriver for WorldDriver<'_> {
    async fn publish_initial_refs(&mut self, pushes: PreparedPushes) -> Result<()> {
        let batch_count = pushes.batches().len();
        for (index, batch) in pushes.batches().enumerate() {
            let effects = batch
                .semantic_effects_for_test()
                .iter()
                .map(|effect| match effect {
                    TestPushEffect::PublicBranch { branch, expected, desired } => {
                        InitialRefEffect::PublicBranch {
                            branch: PublicBranchName::new(branch.clone())
                                .expect("production emitted a checked public branch"),
                            expected: *expected,
                            desired: *desired,
                        }
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
                        panic!("the initial-ref stage cannot contain marker effects")
                    }
                })
                .collect::<Box<[_]>>();
            self.trace.initial_refs.push(effects.clone());
            if let Some(Interruption::InitialRefs { stop, .. }) =
                self.take_interruption(EffectStage::InitialRefs, index)
            {
                return self.stop_git_batch(stop, &effects, ExternalEffect::InitialRef);
            }
            let outcome = apply_atomic_git_batch(self.world, &effects, ExternalEffect::InitialRef);
            if outcome != EffectOutcome::Acknowledged {
                return self.stop(outcome.stop_reason());
            }
            if index + 1 < batch_count {
                self.run_competition_at(CompetitionBoundary::BetweenBatches {
                    stage: EffectStage::InitialRefs,
                    completed_batch: index,
                })
                .await?;
            }
        }
        self.run_competition_at(CompetitionBoundary::AfterInitialRefs).await
    }

    async fn create_pull_requests(
        &mut self,
        creates: PreparedCreates,
    ) -> Result<CompleteCreateReceipts> {
        assert_eq!(creates.repository_id_for_test(), REPOSITORY_ID);
        let mut receipts = Vec::with_capacity(creates.operations_for_test().len());
        let batch_count = creates.batches_for_test().len();
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
                    receipts.push((create.id.clone(), self.world.latest_identity(&create.id)));
                }
            }
            if interruption.is_some() || !exact {
                // A GraphQL response with any missing, duplicate, or mismatched
                // alias is indeterminate even when some siblings durably land.
                return self.stop(StopReason::Indeterminate);
            }
            if index + 1 < batch_count {
                self.run_competition_at(CompetitionBoundary::BetweenBatches {
                    stage: EffectStage::Create,
                    completed_batch: index,
                })
                .await?;
            }
        }
        let receipts = CompleteCreateReceipts::for_plan_test(receipts);
        self.run_competition_at(CompetitionBoundary::AfterCreates).await?;
        Ok(receipts)
    }

    async fn publish_markers(&mut self, pushes: PreparedPushes) -> Result<()> {
        let batch_count = pushes.batches().len();
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
                        panic!("the marker stage cannot contain public-branch effects")
                    }
                })
                .collect::<Box<[_]>>();
            assert!(
                effects
                    .iter()
                    .all(|effect| { self.world.open_pull_requests(&effect.id).next().is_some() }),
                "the production stage may publish a marker only after an OPEN receipt"
            );
            self.trace.markers.push(effects.clone());
            if let Some(Interruption::Marker { stop, .. }) =
                self.take_interruption(EffectStage::Marker, index)
            {
                return self.stop_git_batch(stop, &effects, ExternalEffect::Marker);
            }
            let outcome = apply_atomic_git_batch(self.world, &effects, ExternalEffect::Marker);
            if outcome != EffectOutcome::Acknowledged {
                return self.stop(outcome.stop_reason());
            }
            if index + 1 < batch_count {
                self.run_competition_at(CompetitionBoundary::BetweenBatches {
                    stage: EffectStage::Marker,
                    completed_batch: index,
                })
                .await?;
            }
        }
        self.run_competition_at(CompetitionBoundary::AfterMarkers).await
    }

    async fn project_pull_requests(
        &mut self,
        projection: PreparedPullRequestProjection,
    ) -> Result<()> {
        let batch_count = projection.projection_batches_for_test().len();
        for (index, batch) in projection.projection_batches_for_test().enumerate() {
            let batch = batch
                .iter()
                .cloned()
                .map(|operation| self.world.resolve_projection(operation))
                .collect::<Box<[_]>>();
            assert!(
                batch.iter().all(|effect| {
                    let resolved_id = match effect {
                        ProjectionEffect::Close { resolved_id, .. }
                        | ProjectionEffect::Update(UpdateEffect { resolved_id, .. }) => resolved_id,
                    };
                    resolved_id.as_ref().is_none_or(|id| {
                        self.world.published(id).is_some_and(|change| change.marker.is_some())
                    })
                }),
                "the production stage may project only after the change marker"
            );
            self.trace.projections.push(batch.clone());
            let interruption = self.take_interruption(EffectStage::Projection, index);
            let selected = match &interruption {
                Some(Interruption::Projection { applied_aliases, .. }) => {
                    Some(validated_aliases(applied_aliases, batch.len()))
                }
                _ => None,
            };
            let mut exact = true;
            for (alias, operation) in batch.iter().enumerate() {
                if selected.as_ref().is_some_and(|selected| !selected.contains(&alias)) {
                    continue;
                }
                exact &= self.world.apply_effect(&ExternalEffect::Projection(operation.clone()))
                    == EffectOutcome::Acknowledged;
            }
            if interruption.is_some() || !exact {
                return self.stop(StopReason::Indeterminate);
            }
            if index + 1 < batch_count {
                self.run_competition_at(CompetitionBoundary::BetweenBatches {
                    stage: EffectStage::Projection,
                    completed_batch: index,
                })
                .await?;
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
    Projection(ProjectionEffect),
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum ExternalTarget {
    Change(GherritPrId),
    PublicBranch(PublicBranchName),
    Unknown,
}

impl ExternalEffect {
    fn target(&self) -> ExternalTarget {
        match self {
            Self::InitialRef(InitialRefEffect::Tuple(effect)) => {
                ExternalTarget::Change(effect.id.clone())
            }
            Self::InitialRef(InitialRefEffect::PublicBranch { branch, .. }) => {
                ExternalTarget::PublicBranch(branch.clone())
            }
            Self::Create(effect) => ExternalTarget::Change(effect.id.clone()),
            Self::Marker(effect) => ExternalTarget::Change(effect.id.clone()),
            Self::Projection(ProjectionEffect::Close { resolved_id, .. })
            | Self::Projection(ProjectionEffect::Update(UpdateEffect { resolved_id, .. })) => {
                resolved_id.clone().map_or(ExternalTarget::Unknown, ExternalTarget::Change)
            }
        }
    }
}

impl DurableWorld {
    fn resolve_identity(&self, identity: &PullRequestIdentity) -> Option<GherritPrId> {
        let requested_node = identity.node_id_for_test();
        self.changes.iter().find_map(|(id, change)| {
            change
                .pull_requests
                .iter()
                .any(|pull_request| pull_request.identity.node_id_for_test() == requested_node)
                .then(|| id.clone())
        })
    }

    fn resolve_projection(&self, operation: TestPullRequestProjection) -> ProjectionEffect {
        match operation {
            TestPullRequestProjection::Close(operation) => ProjectionEffect::Close {
                resolved_id: self.resolve_identity(&operation.identity),
                operation,
            },
            TestPullRequestProjection::Update(operation) => {
                ProjectionEffect::Update(UpdateEffect {
                    resolved_id: self.resolve_identity(&operation.identity),
                    operation,
                })
            }
        }
    }

    fn apply_effect(&mut self, effect: &ExternalEffect) -> EffectOutcome {
        let before = self.clone();
        let target = effect.target();
        let outcome = match effect {
            ExternalEffect::InitialRef(effect) => self.apply_initial_ref(effect),
            ExternalEffect::Create(effect) => self.apply_create(effect),
            ExternalEffect::Marker(effect) => self.apply_marker(effect),
            ExternalEffect::Projection(effect) => self.apply_projection(effect),
        };
        match outcome {
            EffectOutcome::Rejected => {
                assert_eq!(*self, before, "a rejected effect cannot alter durable state");
            }
            EffectOutcome::Acknowledged | EffectOutcome::Indeterminate => {
                match &target {
                    ExternalTarget::Change(target) => {
                        self.assert_local_transition(&before, target, effect)
                    }
                    ExternalTarget::PublicBranch(target) => {
                        self.assert_public_transition(&before, target, effect)
                    }
                    ExternalTarget::Unknown => assert_eq!(
                        *self, before,
                        "an effect with an unknown target cannot alter durable state"
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
            InitialRefEffect::PublicBranch { branch, expected, desired } => {
                self.apply_public_branch(branch, *expected, *desired)
            }
        }
    }

    fn apply_public_branch(
        &mut self,
        branch: &PublicBranchName,
        expected: Option<ObjectId>,
        desired: ObjectId,
    ) -> EffectOutcome {
        match self.public_branches.get(branch).copied() {
            Some(current) if current == desired => EffectOutcome::Acknowledged,
            None if expected.is_none()
                && self
                    .public_branches
                    .keys()
                    .all(|other| !ref_paths_conflict(branch.as_str(), other.as_str())) =>
            {
                self.public_branches.insert(branch.clone(), desired);
                EffectOutcome::Acknowledged
            }
            Some(current) if expected == Some(current) => {
                self.public_branches.insert(branch.clone(), desired);
                EffectOutcome::Acknowledged
            }
            None | Some(_) => EffectOutcome::Rejected,
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
                        marker: None,
                        pull_requests: Vec::new(),
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
        let current = current.history.last();
        let identity = self.allocate_identity();
        self.published_mut(&effect.id).unwrap().pull_requests.push(PullRequest {
            identity,
            title: effect.title.clone(),
            body: effect.body.clone(),
            base: BaseKind::Owned,
            state: PullRequestState::Open,
        });
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
        match change.marker {
            None => {
                change.marker = Some(effect.target);
                EffectOutcome::Acknowledged
            }
            Some(target) => {
                if target == effect.target {
                    EffectOutcome::Acknowledged
                } else {
                    EffectOutcome::Rejected
                }
            }
        }
    }

    fn apply_projection(&mut self, effect: &ProjectionEffect) -> EffectOutcome {
        match effect {
            ProjectionEffect::Close { resolved_id, operation } => {
                self.apply_close(resolved_id.as_ref(), operation)
            }
            ProjectionEffect::Update(update) => self.apply_update(update),
        }
    }

    fn apply_close(
        &mut self,
        resolved_id: Option<&GherritPrId>,
        operation: &TestClose,
    ) -> EffectOutcome {
        let Some(resolved_id) = resolved_id else {
            return EffectOutcome::Indeterminate;
        };
        let change = self.changes.get_mut(resolved_id).expect("resolved close target exists");
        let pull_request = change
            .pull_requests
            .iter_mut()
            .find(|pull_request| {
                pull_request.identity.node_id_for_test() == operation.identity.node_id_for_test()
            })
            .expect("resolved close identity exists");
        if pull_request.open_fields().is_none() {
            return EffectOutcome::Indeterminate;
        }
        pull_request.state = PullRequestState::Closed;
        if pull_request.identity == operation.identity {
            EffectOutcome::Acknowledged
        } else {
            EffectOutcome::Indeterminate
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
        let pull_request = change
            .pull_requests
            .iter_mut()
            .find(|pull_request| {
                pull_request.identity.node_id_for_test()
                    == effect.operation.identity.node_id_for_test()
            })
            .expect("a resolved update belongs to a durable pull request");
        if pull_request.open_fields().is_none() {
            return EffectOutcome::Indeterminate;
        }
        if let Some(base) = base {
            pull_request.base = base;
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
            self.public_branches, before.public_branches,
            "a change-local effect cannot mutate a public branch"
        );
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
            if old.marker.is_some() {
                assert_eq!(old.marker, new.marker, "an immutable marker cannot move");
            }
            if old.marker != new.marker {
                assert!(
                    matches!(effect, ExternalEffect::Marker(_)),
                    "only marker publication can establish a marker"
                );
            }
            assert!(
                new.pull_requests.starts_with(&old.pull_requests)
                    || matches!(effect, ExternalEffect::Projection(_)),
                "only projection may mutate an existing pull request row"
            );
            assert_eq!(
                new.pull_requests.len(),
                old.pull_requests.len() + usize::from(matches!(effect, ExternalEffect::Create(_))),
                "only create appends exactly one durable pull request row"
            );
        }
        for id in self.changes.keys() {
            assert!(before.changes.contains_key(id) || id == target, "only the target can publish");
        }
    }

    fn assert_public_transition(
        &self,
        before: &Self,
        target: &PublicBranchName,
        effect: &ExternalEffect,
    ) {
        assert_eq!(self.default_tip, before.default_tip, "publication cannot move the default");
        assert_eq!(self.next_identity, before.next_identity);
        assert_eq!(
            self.changes, before.changes,
            "a public-branch effect cannot mutate any change-local state"
        );
        let ExternalEffect::InitialRef(InitialRefEffect::PublicBranch { branch, desired, .. }) =
            effect
        else {
            panic!("only an initial public-branch effect has a public target")
        };
        assert_eq!(branch, target);
        let mut expected = before.public_branches.clone();
        expected.insert(target.clone(), *desired);
        assert_eq!(
            self.public_branches, expected,
            "one public-branch effect mutates only its exact target"
        );
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

fn update_effects(trace: &AttemptTrace) -> Box<[UpdateEffect]> {
    only_updates(&flatten(&trace.projections))
}

fn only_updates(effects: &[ProjectionEffect]) -> Box<[UpdateEffect]> {
    effects
        .iter()
        .filter_map(|effect| match effect {
            ProjectionEffect::Close { .. } => None,
            ProjectionEffect::Update(update) => Some(update.clone()),
        })
        .collect()
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

fn public_multibatch_intent() -> LocalIntent {
    many_bounded_ids_intent(30, 128).with_public_branch(PUBLIC_BRANCH)
}

fn public_branch_target(world: &DurableWorld) -> Option<ObjectId> {
    world.public_branches.get(&public_branch_name(PUBLIC_BRANCH)).copied()
}

fn mixed_final_batch(trace: &AttemptTrace) -> (usize, &[InitialRefEffect]) {
    assert!(trace.initial_refs.len() > 1, "the fixture crosses the initial-ref push budget");
    let final_index = trace.initial_refs.len() - 1;
    let final_batch = &trace.initial_refs[final_index];
    assert!(final_batch.iter().any(|effect| matches!(effect, InitialRefEffect::Tuple(_))));
    assert!(
        final_batch.iter().any(|effect| matches!(effect, InitialRefEffect::PublicBranch { .. })),
        "the public projection shares the last tuple batch"
    );
    assert_eq!(
        flatten(&trace.initial_refs)
            .iter()
            .filter(|effect| matches!(effect, InitialRefEffect::PublicBranch { .. }))
            .count(),
        1,
        "only the final initial-ref batch contains the public projection"
    );
    (final_index, final_batch)
}

fn assert_tuple_effects_published(world: &DurableWorld, effects: &[InitialRefEffect]) {
    for effect in effects {
        let InitialRefEffect::Tuple(tuple) = effect else {
            panic!("the public projection is ordered after every earlier tuple batch")
        };
        assert_eq!(world.published(&tuple.id).unwrap().history.last(), tuple.desired);
    }
}

fn expected_public_retry_batch(
    final_batch: &[InitialRefEffect],
    current: ObjectId,
) -> Box<[InitialRefEffect]> {
    final_batch
        .iter()
        .cloned()
        .map(|effect| match effect {
            InitialRefEffect::PublicBranch { branch, desired, .. } => {
                InitialRefEffect::PublicBranch { branch, expected: Some(current), desired }
            }
            tuple @ InitialRefEffect::Tuple(_) => tuple,
        })
        .collect()
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
    update_effects(trace)
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
            assert!(published.marker.is_none());
            let pull_request = world
                .open_pull_requests(&create.id)
                .max_by_key(|pull_request| pull_request.identity.number().get())
                .expect("a selected create alias establishes one OPEN row");
            assert_eq!(pull_request.title, create.title);
            assert_eq!(pull_request.body, create.body);
        } else {
            assert!(
                published.pull_requests.is_empty(),
                "an unselected create alias cannot establish an OPEN row"
            );
        }
    }
}

fn assert_create_retry_finishes_projection(
    retry: &AttemptReport,
    world: &DurableWorld,
    intent: &LocalIntent,
    label: &str,
) {
    let expected_markers = intent
        .iter()
        .map(|local| MarkerEffect { id: local.id.clone(), target: local.desired.head })
        .collect::<Box<[_]>>();
    assert_eq!(
        flatten(&retry.trace.markers),
        expected_markers,
        "{label}: retry must mark every durable OPEN row at its exact local head"
    );

    // Body rendering has its own exact snapshot tests. Re-rendering here from
    // the durable identities checks the stage boundary independently: the
    // retry must route every corresponding final body into an update and make
    // that exact value durable.
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
            ValidatedChangeHistory::for_plan_test(
                local.id.clone(),
                &published
                    .history
                    .iter()
                    .map(|revision| (revision.head, revision.first_parent))
                    .collect::<Vec<_>>(),
                (local.desired.head, local.desired.first_parent),
                true,
            )
        })
        .collect();
    let assignments = intent
        .iter()
        .map(|local| (local.id.clone(), world.identity(&local.id).number()))
        .collect::<Vec<_>>();
    let expected_bodies =
        StackBodyRecipes::new(&PushDestination::for_test(), None, stack, histories)
            .unwrap()
            .final_bodies(&assignments)
            .unwrap()
            .into_iter()
            .map(|body| body.into_parts())
            .map(|(id, body)| (id, body.into_string()))
            .collect::<HashMap<_, _>>();

    let actual_updates = update_effects(&retry.trace);
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
        let published = world.published(&local.id).expect("every local change is published");
        let target = published.marker.expect("the retry establishes the marker");
        let pull_request = world
            .open_pull_request(&local.id)
            .expect("every durable change retains a canonical OPEN row");
        assert_eq!(
            update.operation.identity, pull_request.identity,
            "{label}: each final update uses the durable OPEN identity"
        );
        assert_eq!(target, local.desired.head, "{label}: a marker targets the exact local head");
        assert_eq!(
            pull_request.base,
            if index == 0 { BaseKind::Default } else { BaseKind::Owned },
            "{label}: the finalized pull request has its exact local base kind"
        );
        assert_eq!(pull_request.title, local.title, "{label}: the finalized title is exact");
        let expected_body = expected_bodies.get(&local.id).unwrap();
        assert_eq!(
            update.operation.body.as_ref(),
            Some(expected_body),
            "{label}: each final update carries its exact rendered body"
        );
        assert_eq!(
            &pull_request.body, expected_body,
            "{label}: each exact final body becomes durable"
        );
    }
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

fn assert_projection_matches_update(world: &DurableWorld, update: &UpdateEffect) {
    let id =
        update.resolved_id.as_ref().expect("a planned update resolves to one durable OPEN row");
    let managed = world.open_pull_request(id).expect("the update target remains OPEN");
    let pull_request = managed;
    if let Some(title) = &update.operation.title {
        assert_eq!(&pull_request.title, title);
    }
    if let Some(body) = &update.operation.body {
        assert_eq!(&pull_request.body, body);
    }
    if let Some(base) = &update.operation.base_branch {
        let expected = if base == DEFAULT_BRANCH {
            BaseKind::Default
        } else {
            assert_eq!(base, &owned_base_name(id));
            BaseKind::Owned
        };
        assert_eq!(managed.base, expected);
    }
}

/// Compares the facts which commute when disjoint publishers allocate pull
/// request identities in different orders. Generated navigation bodies embed
/// those identities, so neither identities nor bodies are expected to match.
fn assert_same_histories_markers_bases_and_titles(left: &DurableWorld, right: &DurableWorld) {
    assert_eq!(left.default_tip, right.default_tip);
    assert_eq!(left.public_branches, right.public_branches);
    assert_eq!(left.changes.len(), right.changes.len());
    for (id, left_change) in &left.changes {
        let right_change = right.changes.get(id).expect("both worlds publish the same IDs");
        assert_eq!(left_change.history, right_change.history);
        assert_eq!(left_change.marker, right_change.marker);
        match (left.open_pull_request(id), right.open_pull_request(id)) {
            (None, None) => {}
            (Some(left), Some(right)) => {
                assert_eq!(left.base, right.base);
                assert_eq!(left.title, right.title);
                // Generated navigation embeds dynamically allocated PR
                // numbers, so bodies need not commute literally.
            }
            _ => panic!("both worlds have the same OPEN-row presence"),
        }
    }
}

#[test]
#[should_panic(expected = "directory/file conflict")]
fn durable_public_branches_cannot_represent_d_f_conflicts() {
    let intent =
        root_intent(oid(1), "Gdfinvariant", LiteralRevision { head: oid(2), first_parent: oid(1) });
    let mut world = DurableWorld::for_intents(oid(1), &[&intent]);
    world.public_branches.insert(public_branch_name("release-"), oid(10));
    world.public_branches.insert(public_branch_name("release-/child"), oid(11));
    world.assert_well_formed();
}

#[test]
fn public_create_lease_rejects_a_d_f_conflict_without_mutation() {
    let intent =
        root_intent(oid(1), "Gdflease", LiteralRevision { head: oid(2), first_parent: oid(1) });
    let mut world = DurableWorld::for_intents(oid(1), &[&intent]);
    world.public_branches.insert(public_branch_name("release-"), oid(10));
    world.assert_well_formed();
    let before = world.clone();
    let effect = ExternalEffect::InitialRef(InitialRefEffect::PublicBranch {
        branch: public_branch_name("release-/child"),
        expected: None,
        desired: oid(11),
    });
    assert_eq!(world.apply_effect(&effect), EffectOutcome::Rejected);
    assert_eq!(world, before);
}

#[test]
fn updates_route_by_node_id_and_validate_the_complete_receipt_after_mutation() {
    let default = oid(1);
    let revision = LiteralRevision { head: oid(2), first_parent: default };
    let intent = root_intent(default, "Groute", revision);
    let mut world = DurableWorld::for_intents(default, &[&intent]);
    establish_marked(&mut world, &intent.first, BaseKind::Owned, "old body");

    let unknown = world.resolve_projection(TestPullRequestProjection::Update(TestUpdate {
        identity: PullRequestIdentity::for_plan_test(999, "UNKNOWN_NODE"),
        title: Some("not applied".to_owned()),
        body: Some("not applied".to_owned()),
        base_branch: Some(DEFAULT_BRANCH.to_owned()),
    }));
    let ProjectionEffect::Update(unknown) = unknown else { unreachable!() };
    assert_eq!(unknown.resolved_id, None);
    let before_unknown = world.clone();
    assert_eq!(
        world.apply_effect(&ExternalEffect::Projection(ProjectionEffect::Update(unknown))),
        EffectOutcome::Indeterminate
    );
    assert_eq!(world, before_unknown, "an unknown node ID cannot select a durable row");

    let identity = world.identity(&id("Groute"));
    let invalid_base = world.resolve_projection(TestPullRequestProjection::Update(TestUpdate {
        identity: identity.clone(),
        title: Some("not applied".to_owned()),
        body: Some("not applied".to_owned()),
        base_branch: Some(owned_base_name(&id("Gother"))),
    }));
    let ProjectionEffect::Update(invalid_base) = invalid_base else { unreachable!() };
    assert_eq!(invalid_base.resolved_id, Some(id("Groute")));
    let before_invalid_base = world.clone();
    assert_eq!(
        world.apply_effect(&ExternalEffect::Projection(ProjectionEffect::Update(invalid_base))),
        EffectOutcome::Indeterminate
    );
    assert_eq!(world, before_invalid_base, "base validation precedes every field mutation");

    let mismatched_receipt =
        world.resolve_projection(TestPullRequestProjection::Update(TestUpdate {
            identity: PullRequestIdentity::for_plan_test(
                identity.number().get() + 1,
                identity.node_id_for_test(),
            ),
            title: Some("new title".to_owned()),
            body: Some("new body".to_owned()),
            base_branch: Some(DEFAULT_BRANCH.to_owned()),
        }));
    let ProjectionEffect::Update(mismatched_receipt) = mismatched_receipt else { unreachable!() };
    assert_eq!(mismatched_receipt.resolved_id, Some(id("Groute")));
    assert_eq!(
        world.apply_effect(&ExternalEffect::Projection(ProjectionEffect::Update(
            mismatched_receipt
        ))),
        EffectOutcome::Indeterminate,
        "a wrong expected number makes the receipt indeterminate"
    );
    assert!(world.published(&id("Groute")).unwrap().marker.is_some());
    let pull_request = world.open_pull_request(&id("Groute")).unwrap();
    assert_eq!(pull_request.base, BaseKind::Default);
    assert_eq!(pull_request.title, "new title");
    assert_eq!(pull_request.body, "new body");
}

#[test]
fn literal_services_do_not_enforce_cross_backend_stage_policy() {
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
    let effect = world.resolve_projection(TestPullRequestProjection::Update(operation));
    assert_eq!(
        world.apply_effect(&ExternalEffect::Projection(effect)),
        EffectOutcome::Acknowledged,
        "GitHub can update an exact OPEN row without observing a Git marker"
    );
    assert_eq!(world.open_pull_request(&intent.first.id).unwrap().title, "new title");

    let marker_only = root_intent(
        default,
        "Gmarkerwithoutopen",
        LiteralRevision { head: oid(3), first_parent: default },
    );
    let mut marker_world = DurableWorld::for_intents(default, &[&marker_only]);
    marker_world.publish_for_setup(&marker_only.first.id, marker_only.first.desired);
    assert_eq!(
        marker_world.apply_effect(&ExternalEffect::Marker(MarkerEffect {
            id: marker_only.first.id.clone(),
            target: marker_only.first.desired.head,
        })),
        EffectOutcome::Acknowledged,
        "Git can publish a marker without observing GitHub lifecycle"
    );
    assert_eq!(
        marker_world.published(&marker_only.first.id).unwrap().marker,
        Some(marker_only.first.desired.head)
    );
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
    assert_eq!(report.trace.projections.iter().map(|batch| batch.len()).collect::<Vec<_>>(), [2]);
    insta::assert_yaml_snapshot!("two_change_provisional_creates", create_snapshot(&report.trace));
    insta::assert_yaml_snapshot!("two_change_final_updates", update_snapshot(&report.trace));

    let interruptions = [
        Interruption::InitialRefs { batch: 0, stop: GitStop::Rejected },
        Interruption::InitialRefs { batch: 0, stop: GitStop::Indeterminate { applied: false } },
        Interruption::InitialRefs { batch: 0, stop: GitStop::Indeterminate { applied: true } },
        Interruption::Create { batch: 0, applied_aliases: Box::new([]) },
        Interruption::Create { batch: 0, applied_aliases: Box::new([0]) },
        Interruption::Create { batch: 0, applied_aliases: Box::new([1]) },
        Interruption::Create { batch: 0, applied_aliases: Box::new([0, 1]) },
        Interruption::Marker { batch: 0, stop: GitStop::Rejected },
        Interruption::Marker { batch: 0, stop: GitStop::Indeterminate { applied: false } },
        Interruption::Marker { batch: 0, stop: GitStop::Indeterminate { applied: true } },
        Interruption::Projection { batch: 0, applied_aliases: Box::new([]) },
        Interruption::Projection { batch: 0, applied_aliases: Box::new([0]) },
        Interruption::Projection { batch: 0, applied_aliases: Box::new([1]) },
        Interruption::Projection { batch: 0, applied_aliases: Box::new([0, 1]) },
    ];
    for (index, interruption) in interruptions.into_iter().enumerate() {
        let expected_stop = match &interruption {
            Interruption::InitialRefs { stop: GitStop::Rejected, .. }
            | Interruption::Marker { stop: GitStop::Rejected, .. } => StopReason::Rejected,
            _ => StopReason::Indeterminate,
        };
        let mut interrupted = initial.clone();
        let stopped = interrupted
            .run_attempt(&intent, &QueryVisibility::default(), Some(interruption.clone()))
            .await
            .unwrap();
        assert_eq!(stopped.outcome, AttemptOutcome::Stopped(expected_stop));
        if let Interruption::Create { batch, applied_aliases } = &interruption {
            assert!(interrupted.changes.values().all(|change| { change.marker.is_none() }));
            assert!(stopped.trace.markers.is_empty() && stopped.trace.projections.is_empty());
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
        if let Interruption::Projection { batch, applied_aliases } = &interruption {
            assert!(retry.trace.initial_refs.is_empty());
            assert!(retry.trace.creates.is_empty());
            assert!(retry.trace.markers.is_empty());
            assert_eq!(
                update_effects(&retry.trace),
                only_updates(&unapplied_request_suffix(
                    &report.trace.projections,
                    *batch,
                    applied_aliases,
                )),
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
                    Some(Interruption::InitialRefs {
                        batch: stopped_batch,
                        stop: GitStop::Indeterminate { applied },
                    }),
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
                    Some(Interruption::Marker {
                        batch: stopped_batch,
                        stop: GitStop::Indeterminate { applied },
                    }),
                )
                .await
                .unwrap();
            assert_eq!(stopped.trace.markers, marker_report.trace.markers[..=stopped_batch]);
            let label = format!("marker-batch-{stopped_batch}-applied-{applied}");
            let retry = run_acknowledged_retry(&mut world, &marker_intent, &label).await;
            assert_eq!(
                flatten(&retry.trace.markers),
                flatten(&marker_report.trace.markers[stopped_batch + usize::from(applied)..]),
                "a marker retry retains the exact unpublished atomic-batch suffix"
            );
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
async fn known_public_batch_rejection_retains_only_earlier_atomic_batches() {
    let intent = public_multibatch_intent();
    let initial = DurableWorld::for_intents(oid(1), &[&intent]);
    let mut completed = initial.clone();
    let complete = completed.run_attempt(&intent, &QueryVisibility::default(), None).await.unwrap();
    assert_eq!(complete.outcome, AttemptOutcome::Acknowledged);
    let (final_batch_index, final_batch) = mixed_final_batch(&complete.trace);
    let final_batch = final_batch.to_vec().into_boxed_slice();

    let mut rejected_world = initial.clone();
    let rejected = rejected_world
        .run_attempt(
            &intent,
            &QueryVisibility::default(),
            Some(Interruption::InitialRefs { batch: final_batch_index, stop: GitStop::Rejected }),
        )
        .await
        .unwrap();
    assert_eq!(rejected.outcome, AttemptOutcome::Stopped(StopReason::Rejected));
    assert_eq!(rejected.trace.initial_refs, complete.trace.initial_refs);
    assert!(
        rejected.trace.creates.is_empty()
            && rejected.trace.markers.is_empty()
            && rejected.trace.projections.is_empty()
    );
    assert_tuple_effects_published(
        &rejected_world,
        &flatten(&complete.trace.initial_refs[..final_batch_index]),
    );
    for effect in &final_batch {
        match effect {
            InitialRefEffect::Tuple(tuple) => {
                assert!(rejected_world.published(&tuple.id).is_none())
            }
            InitialRefEffect::PublicBranch { branch, .. } => {
                assert_eq!(branch.as_str(), PUBLIC_BRANCH);
                assert!(!rejected_world.public_branches.contains_key(branch));
            }
        }
    }

    let retry =
        rejected_world.run_attempt(&intent, &QueryVisibility::default(), None).await.unwrap();
    assert_eq!(retry.outcome, AttemptOutcome::Acknowledged);
    assert_eq!(retry.trace.initial_refs.as_slice(), std::slice::from_ref(&final_batch));
    assert_eq!(
        public_branch_target(&rejected_world),
        Some(intent.later.last().unwrap().desired.head)
    );
    assert!(
        rejected_world
            .run_attempt(&intent, &QueryVisibility::default(), None)
            .await
            .unwrap()
            .trace
            .is_empty()
    );
}

#[tokio::test]
async fn lost_public_batch_acknowledgement_replans_no_initial_refs() {
    let intent = public_multibatch_intent();
    let initial = DurableWorld::for_intents(oid(1), &[&intent]);
    let mut completed = initial.clone();
    let complete = completed.run_attempt(&intent, &QueryVisibility::default(), None).await.unwrap();
    let (final_batch_index, _) = mixed_final_batch(&complete.trace);
    let mut lost_world = initial.clone();
    let lost = lost_world
        .run_attempt(
            &intent,
            &QueryVisibility::default(),
            Some(Interruption::InitialRefs {
                batch: final_batch_index,
                stop: GitStop::Indeterminate { applied: true },
            }),
        )
        .await
        .unwrap();
    assert_eq!(lost.outcome, AttemptOutcome::Stopped(StopReason::Indeterminate));
    assert_eq!(lost.trace.initial_refs, complete.trace.initial_refs);
    assert_eq!(public_branch_target(&lost_world), Some(intent.later.last().unwrap().desired.head));
    assert!(intent.iter().all(|local| {
        lost_world.published(&local.id).is_some_and(|published| {
            published.history.last() == local.desired && published.pull_requests.is_empty()
        })
    }));
    let lost_retry =
        lost_world.run_attempt(&intent, &QueryVisibility::default(), None).await.unwrap();
    assert_eq!(lost_retry.outcome, AttemptOutcome::Acknowledged);
    assert!(
        lost_retry.trace.initial_refs.is_empty(),
        "fresh observation skips every initial ref whose lost request landed"
    );
    assert!(
        lost_world
            .run_attempt(&intent, &QueryVisibility::default(), None)
            .await
            .unwrap()
            .trace
            .is_empty()
    );
}

#[tokio::test]
async fn stale_public_lease_rolls_back_its_sibling_tuples() {
    let intent = public_multibatch_intent();
    let initial = DurableWorld::for_intents(oid(1), &[&intent]);
    let mut raced_world = initial;
    raced_world.public_branches.insert(public_branch_name(PUBLIC_BRANCH), oid(900));
    raced_world.assert_well_formed();
    let stale_plan = raced_world.plan(&intent, &QueryVisibility::default()).unwrap();
    let mut uncontended = raced_world.clone();
    let expected =
        uncontended.run_attempt(&intent, &QueryVisibility::default(), None).await.unwrap();
    let expected_final_index = expected.trace.initial_refs.len() - 1;
    let expected_final = &expected.trace.initial_refs[expected_final_index];
    assert!(expected_final.iter().any(|effect| matches!(effect, InitialRefEffect::Tuple(_))));
    assert!(expected_final.iter().any(|effect| {
        matches!(
            effect,
            InitialRefEffect::PublicBranch { expected: Some(value), .. } if *value == oid(900)
        )
    }));

    raced_world.public_branches.insert(public_branch_name(PUBLIC_BRANCH), oid(901));
    let stale = raced_world.execute_plan(stale_plan, None).await.unwrap();
    assert_eq!(stale.outcome, AttemptOutcome::Stopped(StopReason::Rejected));
    assert_eq!(
        public_branch_target(&raced_world),
        Some(oid(901)),
        "a stale public lease preserves the competing branch value"
    );
    for effect in expected_final {
        if let InitialRefEffect::Tuple(tuple) = effect {
            assert!(
                raced_world.published(&tuple.id).is_none(),
                "the stale public lease rolls back sibling tuples in its atomic batch"
            );
        }
    }
    assert_tuple_effects_published(
        &raced_world,
        &flatten(&expected.trace.initial_refs[..expected_final_index]),
    );

    let raced_retry =
        raced_world.run_attempt(&intent, &QueryVisibility::default(), None).await.unwrap();
    assert_eq!(raced_retry.outcome, AttemptOutcome::Acknowledged);
    assert_eq!(raced_retry.trace.initial_refs.len(), 1);
    let expected_retry = expected_public_retry_batch(expected_final, oid(901));
    assert_eq!(raced_retry.trace.initial_refs[0], expected_retry);
    assert_eq!(public_branch_target(&raced_world), Some(intent.later.last().unwrap().desired.head));
    assert!(
        raced_world
            .run_attempt(&intent, &QueryVisibility::default(), None)
            .await
            .unwrap()
            .trace
            .is_empty()
    );
}

#[tokio::test]
#[should_panic(expected = "selected fully-applied world requires the whole atomic batch")]
async fn indeterminate_applied_world_cannot_select_a_rejected_batch() {
    let intent = root_intent(
        oid(1),
        "Gappliedworld",
        LiteralRevision { head: oid(980), first_parent: oid(1) },
    )
    .with_public_branch(PUBLIC_BRANCH);
    let mut world = DurableWorld::for_intents(oid(1), &[&intent]);
    world.public_branches.insert(public_branch_name(PUBLIC_BRANCH), oid(900));
    let stale_plan = world.plan(&intent, &QueryVisibility::default()).unwrap();
    world.public_branches.insert(public_branch_name(PUBLIC_BRANCH), oid(901));
    let _ = world
        .execute_plan(
            stale_plan,
            Some(Interruption::InitialRefs {
                batch: 0,
                stop: GitStop::Indeterminate { applied: true },
            }),
        )
        .await;
}

#[tokio::test]
async fn competing_public_publisher_wins_between_primary_initial_ref_batches() {
    let primary_intent = public_multibatch_intent();
    let competitor_intent = root_intent(
        oid(1),
        "Gpubliccompetitor",
        LiteralRevision { head: oid(975), first_parent: oid(1) },
    )
    .with_public_branch(PUBLIC_BRANCH);
    let competitor_tip = competitor_intent.first.desired.head;
    let mut initial = DurableWorld::for_intents(oid(1), &[&primary_intent, &competitor_intent]);
    initial.public_branches.insert(public_branch_name(PUBLIC_BRANCH), oid(900));
    initial.assert_well_formed();

    let primary_plan = initial.plan(&primary_intent, &QueryVisibility::default()).unwrap();
    let competitor_plan = initial.plan(&competitor_intent, &QueryVisibility::default()).unwrap();
    let mut reference_world = initial.clone();
    let reference = reference_world
        .run_attempt(&primary_intent, &QueryVisibility::default(), None)
        .await
        .unwrap();
    let (final_index, final_batch) = mixed_final_batch(&reference.trace);
    assert!(final_batch.iter().any(|effect| {
        matches!(
            effect,
            InitialRefEffect::PublicBranch { expected: Some(value), .. } if *value == oid(900)
        )
    }));

    let mut world = initial;
    let (primary, competitor) = execute_with_competition(
        &mut world,
        primary_plan,
        competitor_plan,
        CompetitionBoundary::BetweenBatches { stage: EffectStage::InitialRefs, completed_batch: 0 },
        None,
    )
    .await
    .unwrap();
    assert_eq!(competitor.outcome, AttemptOutcome::Acknowledged);
    assert_eq!(competitor.trace.initial_refs.len(), 1);
    assert_eq!(flatten(&competitor.trace.creates).len(), 1);
    assert_eq!(flatten(&competitor.trace.markers).len(), 1);
    assert_eq!(update_effects(&competitor.trace).len(), 1);
    assert_eq!(primary.outcome, AttemptOutcome::Stopped(StopReason::Rejected));
    assert_eq!(primary.trace.initial_refs, reference.trace.initial_refs);
    assert!(
        primary.trace.creates.is_empty()
            && primary.trace.markers.is_empty()
            && primary.trace.projections.is_empty(),
        "the stale public lease cannot release any primary GitHub stage"
    );
    assert_eq!(public_branch_target(&world), Some(competitor_tip));
    assert_tuple_effects_published(&world, &flatten(&reference.trace.initial_refs[..final_index]));
    for effect in final_batch {
        if let InitialRefEffect::Tuple(tuple) = effect {
            assert!(
                world.published(&tuple.id).is_none(),
                "the stale public lease rolls back sibling primary tuples"
            );
        }
    }

    let competitor_change = world.published(&competitor_intent.first.id).cloned().unwrap();
    let retry =
        world.run_attempt(&primary_intent, &QueryVisibility::default(), None).await.unwrap();
    assert_eq!(retry.outcome, AttemptOutcome::Acknowledged);
    assert_eq!(retry.trace.initial_refs.len(), 1);
    assert_eq!(
        retry.trace.initial_refs[0],
        expected_public_retry_batch(final_batch, competitor_tip)
    );
    assert_eq!(
        public_branch_target(&world),
        Some(primary_intent.later.last().unwrap().desired.head)
    );
    assert_eq!(world.published(&competitor_intent.first.id), Some(&competitor_change));
    assert!(
        world
            .run_attempt(&primary_intent, &QueryVisibility::default(), None)
            .await
            .unwrap()
            .trace
            .is_empty()
    );
}

#[tokio::test]
async fn preplanned_public_effects_accept_already_desired_receipts() {
    let duplicate_intent = root_intent(
        oid(1),
        "Gpublicduplicate",
        LiteralRevision { head: oid(950), first_parent: oid(1) },
    )
    .with_public_branch(PUBLIC_BRANCH);
    let duplicate_initial = DurableWorld::for_intents(oid(1), &[&duplicate_intent]);
    let first_plan =
        duplicate_initial.plan(&duplicate_intent, &QueryVisibility::default()).unwrap();
    let duplicate_plan =
        duplicate_initial.plan(&duplicate_intent, &QueryVisibility::default()).unwrap();
    let mut duplicate_world = duplicate_initial;
    let first = duplicate_world
        .execute_plan(
            first_plan,
            Some(Interruption::InitialRefs {
                batch: 0,
                stop: GitStop::Indeterminate { applied: true },
            }),
        )
        .await
        .unwrap();
    assert_eq!(first.outcome, AttemptOutcome::Stopped(StopReason::Indeterminate));
    let duplicate = duplicate_world.execute_plan(duplicate_plan, None).await.unwrap();
    assert_eq!(duplicate.outcome, AttemptOutcome::Acknowledged);
    assert_eq!(duplicate.trace.initial_refs, first.trace.initial_refs);
    assert!(
        duplicate_world
            .run_attempt(&duplicate_intent, &QueryVisibility::default(), None)
            .await
            .unwrap()
            .trace
            .is_empty(),
        "an already-desired public ref receipt releases the preplanned duplicate"
    );
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
        completed.trace.projections.iter().map(|batch| batch.len()).collect::<Vec<_>>(),
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
        stopped.trace.creates.iter().map(|batch| batch.len()).collect::<Vec<_>>(),
        [1, 1],
        "an earlier request is acknowledged, the current response is lost, and the tail is unsent"
    );
    assert!(stopped.trace.markers.is_empty() && stopped.trace.projections.is_empty());
    assert!(interrupted_create.open_pull_request(&creates[0][0].id).is_some());
    assert!(interrupted_create.open_pull_request(&creates[1][0].id).is_some());
    assert!(interrupted_create.open_pull_request(&creates[2][0].id).is_none());
    let retry =
        run_acknowledged_retry(&mut interrupted_create, &intent, "multi-request-create").await;
    assert!(retry.trace.initial_refs.is_empty());
    assert_eq!(
        flatten(&retry.trace.creates),
        unapplied_request_suffix(creates, 1, &[0]),
        "the create retry must contain only the unsent request suffix"
    );
    assert_create_retry_finishes_projection(
        &retry,
        &interrupted_create,
        &intent,
        "multi-request-create",
    );
    assert_quiescent(&mut interrupted_create, &intent, "multi-request-create").await;

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
            Some(Interruption::Projection { batch: 1, applied_aliases: Box::new([0]) }),
        )
        .await
        .unwrap();
    assert_eq!(
        stopped.trace.projections.iter().map(|batch| batch.len()).collect::<Vec<_>>(),
        [1, 1]
    );
    for (index, local) in intent.iter().enumerate() {
        let body = &before_updates.open_pull_request(&local.id).unwrap().body;
        if index < 2 {
            assert_ne!(body, "provisional");
        } else {
            assert_eq!(body, "provisional");
        }
    }
    let retry = run_acknowledged_retry(&mut before_updates, &intent, "multi-request-update").await;
    assert!(retry.trace.initial_refs.is_empty());
    assert!(retry.trace.creates.is_empty());
    assert!(retry.trace.markers.is_empty());
    assert_eq!(
        update_effects(&retry.trace),
        only_updates(&unapplied_request_suffix(&completed.trace.projections, 1, &[0])),
        "the update retry must contain only the unsent request suffix"
    );
    assert_quiescent(&mut before_updates, &intent, "multi-request-update").await;

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
    assert_eq!(duplicate.outcome, AttemptOutcome::Acknowledged);
    assert_eq!(duplicate.trace.creates.len(), 1);
    assert!(!duplicate.trace.markers.is_empty() && !duplicate.trace.projections.is_empty());
    assert_eq!(duplicate_world.identity(&duplicate_creates[0][0].id), retained_identity);
    assert!(duplicate_world.open_pull_request(&duplicate_creates[0][1].id).is_some());
    assert_eq!(
        duplicate_world.next_identity,
        next_before + 2,
        "both stale create aliases append rows"
    );
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

    let hidden = QueryVisibility::hiding_identities(&world, [world.identity(&id("Ghidden"))]);
    let duplicate = world.run_attempt(&intent, &hidden, None).await.unwrap();
    assert!(duplicate.trace.initial_refs.is_empty());
    assert_eq!(
        duplicate.trace.creates, first.trace.creates,
        "the stable create payload is identical"
    );
    assert_eq!(
        duplicate.outcome,
        AttemptOutcome::Acknowledged,
        "the service accepts a same-key duplicate create"
    );
    assert_eq!(world.open_pull_requests(&id("Ghidden")).count(), 2);
    assert!(world.published(&id("Ghidden")).unwrap().marker.is_some());

    let world = assert_restart_converges(world, &intent, "hidden-unmarked").await;
    let hidden_marked =
        QueryVisibility::hiding_identities(&world, [world.identity(&id("Ghidden"))]);
    let error = world.plan(&intent, &hidden_marked).err().unwrap();
    assert!(error.to_string().contains("Terminal pull request history"));
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
        update_effects(&attempt.trace).iter().all(|update| update.operation.base_branch.is_none())
    );
    insta::assert_yaml_snapshot!("stale_amend_rebase_updates", update_snapshot(&attempt.trace));

    let stale_retry = world.run_attempt(&new_intent, &stale, None).await.unwrap();
    assert!(stale_retry.trace.initial_refs.is_empty());
    assert_eq!(
        update_effects(&stale_retry.trace),
        update_effects(&attempt.trace),
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
        world.published(&id("Gmove")).unwrap().marker,
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
    let updates = update_effects(&attempt.trace);
    assert_eq!(updates.len(), 1);
    assert_eq!(updates[0].resolved_id, Some(id("Gmove")));
    assert_eq!(updates[0].operation.title.as_deref(), Some("Moved root"));
    assert_eq!(updates[0].operation.base_branch.as_deref(), Some(DEFAULT_BRANCH));
    assert_eq!(world.published(&id("Gparent")), parent_before.as_ref());

    let stale_retry = world.run_attempt(&new_intent, &stale, None).await.unwrap();
    assert_eq!(
        update_effects(&stale_retry.trace),
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

#[tokio::test]
async fn competing_publishers_resume_across_every_authority_barrier() {
    let default = oid(1);
    let old_revision = LiteralRevision { head: oid(10), first_parent: default };
    let new_revision = LiteralRevision { head: oid(11), first_parent: default };
    let old_intent = root_intent(default, "Gtuplecreate", old_revision);
    let new_intent = root_intent(default, "Gtuplecreate", new_revision);
    let initial = DurableWorld::for_intents(default, &[&old_intent, &new_intent]);

    let primary = initial.plan(&old_intent, &QueryVisibility::default()).unwrap();
    let mut post_primary_initial_refs = initial.clone();
    let preview = post_primary_initial_refs
        .run_attempt(
            &old_intent,
            &QueryVisibility::default(),
            Some(Interruption::Create { batch: 0, applied_aliases: Box::new([]) }),
        )
        .await
        .unwrap();
    assert_eq!(flatten(&preview.trace.initial_refs).len(), 1);
    let competing =
        post_primary_initial_refs.plan(&new_intent, &QueryVisibility::default()).unwrap();

    let mut world = initial;
    let (primary, competing) = execute_with_competition(
        &mut world,
        primary,
        competing,
        CompetitionBoundary::AfterInitialRefs,
        Some(Interruption::Create { batch: 0, applied_aliases: Box::new([]) }),
    )
    .await
    .unwrap();
    assert_eq!(competing.outcome, AttemptOutcome::Stopped(StopReason::Indeterminate));
    assert_eq!(flatten(&competing.trace.initial_refs).len(), 1);
    assert!(competing.trace.markers.is_empty() && competing.trace.projections.is_empty());
    assert_eq!(
        primary.outcome,
        AttemptOutcome::Stopped(StopReason::Indeterminate),
        "the suspended create lands, but its stale object IDs cannot release a marker"
    );
    assert!(primary.trace.markers.is_empty() && primary.trace.projections.is_empty());
    assert_eq!(
        world.published(&id("Gtuplecreate")).unwrap().history.iter().copied().collect::<Vec<_>>(),
        [old_revision, new_revision]
    );
    assert_eq!(world.published(&id("Gtuplecreate")).unwrap().marker, None);
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
    let (primary, competing) = execute_with_competition(
        &mut world,
        primary,
        competing,
        CompetitionBoundary::AfterCreates,
        None,
    )
    .await
    .unwrap();
    assert_eq!(primary.outcome, AttemptOutcome::Acknowledged);
    assert_eq!(competing.outcome, AttemptOutcome::Acknowledged);
    assert_eq!(world.open_pull_requests(&id("Gcreatemarker")).count(), 2);
    assert!(
        flatten(&primary.trace.markers).len() == 1 && update_effects(&primary.trace).len() == 1
    );
    run_acknowledged_retry(&mut world, &intent, "same-key-duplicate").await;
    assert_quiescent(&mut world, &intent, "same-key-duplicate").await;

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
    let (primary, competing) = execute_with_competition(
        &mut world,
        primary,
        competing,
        CompetitionBoundary::AfterMarkers,
        None,
    )
    .await
    .unwrap();
    assert_eq!(primary.outcome, AttemptOutcome::Acknowledged);
    assert_eq!(competing.outcome, AttemptOutcome::Acknowledged);
    assert_eq!(flatten(&primary.trace.markers).len(), 2);
    assert_eq!(flatten(&competing.trace.markers).len(), 2);
    let primary_root_body = update_effects(&primary.trace)
        .iter()
        .find(|update| update.resolved_id.as_ref() == Some(&id("Gbarrierroot")))
        .and_then(|update| update.operation.body.clone())
        .unwrap();
    assert_eq!(
        &world.open_pull_request(&id("Gbarrierroot")).unwrap().body,
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
async fn fresh_competitor_marks_and_updates_after_primary_creates() {
    let primary_intent = two_change_intent();
    let competing_intent = LocalIntent::new(oid(10), std::iter::once(primary_intent.first.clone()));
    let initial = DurableWorld::for_intents(oid(10), &[&primary_intent, &competing_intent]);
    let primary = initial.plan(&primary_intent, &QueryVisibility::default()).unwrap();

    // Build only the observation from the post-create checkpoint. The actual
    // schedule below starts again from `initial`; no durable preview state or
    // attempt-local receipt is shared between publishers.
    let mut post_create = initial.clone();
    let preview = post_create
        .run_attempt(
            &primary_intent,
            &QueryVisibility::default(),
            Some(Interruption::Marker {
                batch: 0,
                stop: GitStop::Indeterminate { applied: false },
            }),
        )
        .await
        .unwrap();
    assert_eq!(preview.outcome, AttemptOutcome::Stopped(StopReason::Indeterminate));
    assert_eq!(flatten(&preview.trace.creates).len(), 2);
    assert!(preview.trace.projections.is_empty());
    let competing = post_create.plan(&competing_intent, &QueryVisibility::default()).unwrap();

    let mut world = initial;
    let (primary, competing) = execute_with_competition(
        &mut world,
        primary,
        competing,
        CompetitionBoundary::AfterCreates,
        None,
    )
    .await
    .unwrap();
    assert_eq!(primary.outcome, AttemptOutcome::Acknowledged);
    assert_eq!(competing.outcome, AttemptOutcome::Acknowledged);
    assert!(competing.trace.initial_refs.is_empty());
    assert!(competing.trace.creates.is_empty());
    assert_eq!(flatten(&competing.trace.markers).len(), 1);
    assert_eq!(update_effects(&competing.trace).len(), 1);
    assert_eq!(flatten(&primary.trace.markers).len(), 2);
    assert_eq!(update_effects(&primary.trace).len(), 2);

    let root_id = &primary_intent.first.id;
    let primary_updates = update_effects(&primary.trace);
    let primary_body = primary_updates
        .iter()
        .find(|update| update.resolved_id.as_ref() == Some(root_id))
        .and_then(|update| update.operation.body.as_ref())
        .expect("the resumed two-change publisher updates the shared root body");
    let competing_updates = update_effects(&competing.trace);
    let competing_body = competing_updates
        .iter()
        .find(|update| update.resolved_id.as_ref() == Some(root_id))
        .and_then(|update| update.operation.body.as_ref())
        .expect("the fresh root-only publisher updates the shared root body");
    assert_ne!(primary_body, competing_body);
    assert_eq!(
        &world.open_pull_request(root_id).unwrap().body,
        primary_body,
        "the primary resumes after its competitor and is the observable last writer"
    );
    assert!(
        world
            .run_attempt(&primary_intent, &QueryVisibility::default(), None)
            .await
            .unwrap()
            .trace
            .is_empty()
    );
}

#[tokio::test]
async fn competing_publishers_interleave_between_initial_ref_batches() {
    let mut competing_tuple_intent = many_bounded_ids_intent(30, 128);
    let primary_tuple_intent = competing_tuple_intent.clone();
    let competing_last = &mut competing_tuple_intent.later.last_mut().unwrap().desired;
    competing_last.head = oid(500);
    let initial =
        DurableWorld::for_intents(oid(1), &[&primary_tuple_intent, &competing_tuple_intent]);
    let primary = initial.plan(&primary_tuple_intent, &QueryVisibility::default()).unwrap();
    let competing = initial.plan(&competing_tuple_intent, &QueryVisibility::default()).unwrap();
    let mut world = initial;
    let (primary, competing) = execute_with_competition(
        &mut world,
        primary,
        competing,
        CompetitionBoundary::BetweenBatches { stage: EffectStage::InitialRefs, completed_batch: 0 },
        Some(Interruption::Create { batch: 0, applied_aliases: Box::new([]) }),
    )
    .await
    .unwrap();
    assert_eq!(competing.outcome, AttemptOutcome::Stopped(StopReason::Indeterminate));
    assert_eq!(primary.outcome, AttemptOutcome::Stopped(StopReason::Rejected));
    assert_eq!(primary.trace.initial_refs.len(), 2);
    assert!(primary.trace.creates.is_empty());
    let last_id = &primary_tuple_intent.later.last().unwrap().id;
    assert_eq!(world.published(last_id).unwrap().history.last().head, oid(500));
    assert_restart_converges(world, &primary_tuple_intent, "between-initial-ref-batches").await;
}

#[tokio::test]
async fn competing_publishers_interleave_between_marker_batches() {
    let old_marker_intent = many_bounded_ids_intent(90, 128);
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
    let (primary, competing) = execute_with_competition(
        &mut world,
        primary,
        competing,
        CompetitionBoundary::BetweenBatches { stage: EffectStage::Marker, completed_batch: 0 },
        None,
    )
    .await
    .unwrap();
    assert_eq!(competing.outcome, AttemptOutcome::Acknowledged);
    assert_eq!(primary.outcome, AttemptOutcome::Stopped(StopReason::Rejected));
    assert_eq!(primary.trace.markers.len(), 2);
    assert!(primary.trace.projections.is_empty());
    assert_eq!(world.published(last_id).unwrap().marker, Some(oid(600)));
    assert_restart_converges(world, &old_marker_intent, "between-marker-batches").await;
}

#[tokio::test]
async fn stale_create_batches_append_duplicates_and_converge() {
    let create_intent = multi_request_intent();
    let initial = DurableWorld::for_intents(oid(10), &[&create_intent]);
    for completed_batch in 0..2 {
        let primary = initial.plan(&create_intent, &QueryVisibility::default()).unwrap();
        let competing = initial.plan(&create_intent, &QueryVisibility::default()).unwrap();
        let mut world = initial.clone();
        let (primary, competing) = execute_with_competition(
            &mut world,
            primary,
            competing,
            CompetitionBoundary::BetweenBatches { stage: EffectStage::Create, completed_batch },
            None,
        )
        .await
        .unwrap();
        assert_eq!(primary.outcome, AttemptOutcome::Acknowledged);
        assert_eq!(competing.outcome, AttemptOutcome::Acknowledged);
        assert_eq!(
            primary.trace.creates.iter().map(|batch| batch.len()).collect::<Vec<_>>(),
            [1, 1, 1]
        );
        assert_eq!(competing.trace.creates.len(), 3);
        run_acknowledged_retry(&mut world, &create_intent, "stale-create-batches").await;
        assert_quiescent(&mut world, &create_intent, "stale-create-batches").await;
    }
}

#[tokio::test]
async fn fresh_and_stale_create_batches_append_duplicates_and_converge() {
    // A publisher which starts after a non-final request retains completed
    // identities and creates the tail. The stale publisher can still append
    // every row in its frozen plan; a fresh attempt closes the duplicates.
    let create_intent = multi_request_intent();
    let initial = DurableWorld::for_intents(oid(10), &[&create_intent]);
    for completed_batch in 0..2 {
        let mut after_completed_requests = initial.clone();
        let preview = after_completed_requests
            .run_attempt(
                &create_intent,
                &QueryVisibility::default(),
                Some(Interruption::Create {
                    batch: completed_batch + 1,
                    applied_aliases: Box::new([]),
                }),
            )
            .await
            .unwrap();
        assert_eq!(preview.outcome, AttemptOutcome::Stopped(StopReason::Indeterminate));
        assert_eq!(preview.trace.creates.len(), completed_batch + 2);

        let primary = initial.plan(&create_intent, &QueryVisibility::default()).unwrap();
        let competing =
            after_completed_requests.plan(&create_intent, &QueryVisibility::default()).unwrap();
        let mut world = initial.clone();
        let (primary, competing) = execute_with_competition(
            &mut world,
            primary,
            competing,
            CompetitionBoundary::BetweenBatches { stage: EffectStage::Create, completed_batch },
            None,
        )
        .await
        .unwrap();
        assert_eq!(primary.outcome, AttemptOutcome::Acknowledged);
        assert_eq!(competing.outcome, AttemptOutcome::Acknowledged);
        assert_eq!(primary.trace.creates.len(), 3);
        assert_eq!(
            competing.trace.creates.iter().map(|batch| batch.len()).collect::<Vec<_>>(),
            vec![1; 2 - completed_batch]
        );
        assert!(!primary.trace.markers.is_empty() && !primary.trace.projections.is_empty());
        assert_eq!(flatten(&competing.trace.markers).len(), 3);
        assert_eq!(update_effects(&competing.trace).len(), 3);
        run_acknowledged_retry(&mut world, &create_intent, "fresh-create-batches").await;
        assert_quiescent(&mut world, &create_intent, "fresh-create-batches").await;
    }
}

#[tokio::test]
async fn competing_publishers_interleave_between_projection_requests() {
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
    let initial = {
        let mut world = DurableWorld::for_intents(oid(10), &[&a_intent, &b_intent]);
        for change in a_intent.iter().chain(std::iter::once(b_intent.later.last().unwrap())) {
            establish_marked(
                &mut world,
                change,
                if change.id == a_intent.first.id { BaseKind::Default } else { BaseKind::Owned },
                "provisional",
            );
        }
        world
    };

    for completed_batch in 0..2 {
        let primary = initial.plan(&a_intent, &QueryVisibility::default()).unwrap();
        let competing = initial.plan(&b_intent, &QueryVisibility::default()).unwrap();
        let mut world = initial.clone();
        let (primary, competing) = execute_with_competition(
            &mut world,
            primary,
            competing,
            CompetitionBoundary::BetweenBatches { stage: EffectStage::Projection, completed_batch },
            None,
        )
        .await
        .unwrap();
        assert_eq!(primary.outcome, AttemptOutcome::Acknowledged);
        assert_eq!(competing.outcome, AttemptOutcome::Acknowledged);
        assert_eq!(
            primary.trace.projections.iter().map(|batch| batch.len()).collect::<Vec<_>>(),
            [1, 1, 2]
        );
        assert_eq!(
            competing.trace.projections.iter().map(|batch| batch.len()).collect::<Vec<_>>(),
            [1, 1, 2]
        );

        let primary_count =
            primary.trace.projections.iter().map(|batch| batch.len()).sum::<usize>();
        let primary_writes = primary
            .trace
            .projections
            .iter()
            .enumerate()
            .flat_map(|(batch, projections)| {
                projections.iter().cloned().map(move |projection| {
                    let ProjectionEffect::Update(update) = projection else {
                        panic!("this update interleaving fixture cannot emit duplicate closes")
                    };
                    let id = update
                        .resolved_id
                        .clone()
                        .expect("every primary update has an exact target");
                    (id, (batch, update))
                })
            })
            .collect::<HashMap<_, _>>();
        assert_eq!(primary_writes.len(), primary_count);
        let competing_count =
            competing.trace.projections.iter().map(|batch| batch.len()).sum::<usize>();
        let competing_writes = update_effects(&competing.trace)
            .iter()
            .cloned()
            .map(|update| {
                let id =
                    update.resolved_id.clone().expect("every competing update has an exact target");
                (id, update)
            })
            .collect::<HashMap<_, _>>();
        assert_eq!(competing_writes.len(), competing_count);

        let primary_ids = primary_writes.keys().cloned().collect::<HashSet<_>>();
        let competing_ids = competing_writes.keys().cloned().collect::<HashSet<_>>();
        assert_eq!(
            primary_ids,
            a_intent.iter().map(|change| change.id.clone()).collect::<HashSet<_>>()
        );
        assert_eq!(
            competing_ids,
            b_intent.iter().map(|change| change.id.clone()).collect::<HashSet<_>>()
        );
        for shared_id in primary_ids.intersection(&competing_ids) {
            let primary_body = primary_writes[shared_id]
                .1
                .operation
                .body
                .as_ref()
                .expect("every shared primary update replaces its body");
            let competing_body = competing_writes[shared_id]
                .operation
                .body
                .as_ref()
                .expect("every shared competing update replaces its body");
            assert_ne!(
                primary_body, competing_body,
                "the last-writer assertion for {shared_id:?} must be observable"
            );
        }
        let ids =
            primary_writes.keys().chain(competing_writes.keys()).cloned().collect::<HashSet<_>>();
        for id in ids {
            let expected = match (primary_writes.get(&id), competing_writes.get(&id)) {
                (Some((batch, _)), Some(competing)) if *batch <= completed_batch => competing,
                (Some((_, primary)), _) => primary,
                (None, Some(competing)) => competing,
                (None, None) => unreachable!("the ID came from one writer map"),
            };
            assert_projection_matches_update(&world, expected);
        }

        assert_restart_converges(
            world,
            &a_intent,
            &format!("between-update-request-{completed_batch}"),
        )
        .await;
    }
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
            Some(Interruption::Marker {
                batch: 0,
                stop: GitStop::Indeterminate { applied: false },
            }),
        )
        .await
        .unwrap();
    assert_eq!(flatten(&advance.trace.initial_refs).len(), 1);
    assert_eq!(advanced.published(&id("Gmarkerrace")).unwrap().marker, None);

    for old_is_primary in [true, false] {
        let old_plan = initial.plan(&old_intent, &QueryVisibility::default()).unwrap();
        let new_plan = advanced.plan(&new_intent, &QueryVisibility::default()).unwrap();
        let (primary, competing, primary_intent, winning_target) = if old_is_primary {
            (old_plan, new_plan, &old_intent, new_revision.head)
        } else {
            (new_plan, old_plan, &new_intent, old_revision.head)
        };
        let mut world = advanced.clone();
        let (primary, competing) = execute_with_competition(
            &mut world,
            primary,
            competing,
            CompetitionBoundary::AfterInitialRefs,
            None,
        )
        .await
        .unwrap();
        assert_eq!(competing.outcome, AttemptOutcome::Acknowledged);
        assert_eq!(
            primary.outcome,
            AttemptOutcome::Stopped(StopReason::Rejected),
            "the immutable marker with the other target rejects the suspended publisher"
        );
        assert!(primary.trace.projections.is_empty());
        assert_eq!(world.published(&id("Gmarkerrace")).unwrap().marker, Some(winning_target));
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
    assert_same_histories_markers_bases_and_titles(&ab, &ba);
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
        AttemptOutcome::Acknowledged,
        "the service accepts both same-key creates"
    );
    assert_ne!(same_world, after_first, "the second create appends a durable row");
    run_acknowledged_retry(&mut same_world, &same_intent, "identical-create-race").await;
    assert_quiescent(&mut same_world, &same_intent, "identical-create-race").await;

    let marker_revision = LiteralRevision { head: oid(40), first_parent: default };
    let marker_intent = root_intent(default, "Gmarker", marker_revision);
    let mut marker_world = DurableWorld::for_intents(default, &[&marker_intent]);
    marker_world.publish_for_setup(&id("Gmarker"), marker_revision);
    marker_world.open_for_setup(&id("Gmarker"), "provisional", "provisional");
    let marker_a = marker_world.plan(&marker_intent, &QueryVisibility::default()).unwrap();
    let marker_b = marker_world.plan(&marker_intent, &QueryVisibility::default()).unwrap();
    let marker_a = marker_world.execute_plan(marker_a, None).await.unwrap();
    assert!(marker_a.trace.initial_refs.is_empty() && marker_a.trace.creates.is_empty());
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
            flatten(&retry.trace.initial_refs).as_ref(),
            &[InitialRefEffect::Tuple(TupleEffect {
                id: id("Gconflict"),
                expected: Some(winner),
                desired: loser,
                version: 3,
            })]
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
    assert_eq!(flatten(&first.trace.initial_refs).len(), 1);
    let old_create_attempt = world.plan(&old_intent, &QueryVisibility::default()).unwrap();
    let concurrent = world
        .run_attempt(
            &new_intent,
            &QueryVisibility::default(),
            Some(Interruption::Create { batch: 0, applied_aliases: Box::new([]) }),
        )
        .await
        .unwrap();
    assert_eq!(flatten(&concurrent.trace.initial_refs).len(), 1);
    let indeterminate = world.execute_plan(old_create_attempt, None).await.unwrap();
    assert_eq!(
        indeterminate.outcome,
        AttemptOutcome::Stopped(StopReason::Indeterminate),
        "the stable-key create may land after its observed branch OIDs move"
    );
    assert!(indeterminate.trace.initial_refs.is_empty());
    assert_eq!(indeterminate.trace.creates.len(), 1);
    assert!(
        world.published(&id("Gcreate")).unwrap().marker.is_none(),
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
    assert!(a_attempt.trace.initial_refs.is_empty() && a_attempt.trace.markers.is_empty());
    assert!(b_attempt.trace.initial_refs.is_empty() && b_attempt.trace.markers.is_empty());
    insta::assert_yaml_snapshot!(
        "divergent_child_exact_update_alternatives",
        UpdateAlternatives {
            publisher_a: update_snapshot(&a_attempt.trace),
            publisher_b: update_snapshot(&b_attempt.trace),
        }
    );

    let a_updates = update_effects(&a_attempt.trace);
    let b_updates = update_effects(&b_attempt.trace);
    assert_eq!((a_updates.len(), b_updates.len()), (2, 2));
    let a_root_body = a_updates
        .iter()
        .find(|update| update.resolved_id.as_ref() == Some(&id("Groot")))
        .and_then(|update| update.operation.body.clone())
        .unwrap();
    let b_root_body = b_updates
        .iter()
        .find(|update| update.resolved_id.as_ref() == Some(&id("Groot")))
        .and_then(|update| update.operation.body.clone())
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
            &world.open_pull_request(&id("Groot")).unwrap().body,
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
    let stale_updates = update_effects(&stale.trace);
    assert_eq!(stale_updates.len(), 1);
    assert_eq!(stale_updates[0].resolved_id, Some(id("Gstale")));
    assert_eq!(
        world.open_pull_request(&id("Gstale")).unwrap().body,
        stale_updates[0].operation.body.clone().unwrap()
    );
    let world = assert_restart_converges(world, &new_intent, "stale-projection").await;
    assert_eq!(
        world.published(&id("Gstale")).unwrap().history.last(),
        new_revision,
        "repair never rewrites the winning immutable tuple"
    );
}

fn three_open_root() -> (DurableWorld, LocalIntent, [PullRequestIdentity; 3]) {
    let default = oid(1);
    let revision = LiteralRevision { head: oid(2), first_parent: default };
    let intent = root_intent(default, "Gduplicates", revision);
    let mut world = DurableWorld::for_intents(default, &[&intent]);
    world.publish_for_setup(&intent.first.id, revision);
    for suffix in ["first", "second", "third"] {
        world.open_for_setup(&intent.first.id, &format!("stale {suffix}"), "stale body");
    }
    world.mark_for_setup(&intent.first.id, revision.head, BaseKind::Default);
    let identities = world
        .open_pull_requests(&intent.first.id)
        .map(|row| row.identity.clone())
        .collect::<Vec<_>>()
        .try_into()
        .unwrap();
    (world, intent, identities)
}

#[tokio::test]
async fn complete_stale_open_connection_survives_duplicate_cleanup() {
    let (mut world, intent, identities) = three_open_root();
    let stale = QueryVisibility::stale(&world, [intent.first.id.clone()]);

    world.run_attempt(&intent, &QueryVisibility::default(), None).await.unwrap();
    assert_eq!(world.open_pull_requests(&intent.first.id).count(), 1);
    let repaired = world.clone();

    let stopped = world.run_attempt(&intent, &stale, None).await.unwrap();
    assert_eq!(stopped.outcome, AttemptOutcome::Stopped(StopReason::Indeterminate));
    assert_eq!(
        flatten(&stopped.trace.projections)
            .iter()
            .map(|effect| match effect {
                ProjectionEffect::Close { operation, .. } => {
                    ("close", operation.identity.number().get())
                }
                ProjectionEffect::Update(update) => {
                    ("update", update.operation.identity.number().get())
                }
            })
            .collect::<Vec<_>>(),
        [("close", 101), ("close", 102), ("update", 100)]
    );
    assert_eq!(
        world, repaired,
        "stale duplicate closures stay indeterminate and the canonical update is idempotent"
    );
    assert_eq!(world.identity(&intent.first.id), identities[0]);
    assert_quiescent(&mut world, &intent, "stale-duplicate-connection").await;
}

#[tokio::test]
async fn mixed_duplicate_projection_recovers_from_every_alias_subset() {
    let (initial, intent, identities) = three_open_root();
    for mask in 0_u8..8 {
        let aliases = (0..3).filter(|alias| mask & (1 << alias) != 0).collect::<Box<[_]>>();
        let mut world = initial.clone();
        let stopped = world
            .run_attempt(
                &intent,
                &QueryVisibility::default(),
                Some(Interruption::Projection { batch: 0, applied_aliases: aliases }),
            )
            .await
            .unwrap();
        assert_eq!(stopped.outcome, AttemptOutcome::Stopped(StopReason::Indeterminate));
        assert_eq!(stopped.trace.projections.len(), 1);
        assert_eq!(
            stopped.trace.projections[0]
                .iter()
                .map(|effect| match effect {
                    ProjectionEffect::Close { operation, .. } => {
                        ("close", operation.identity.number().get())
                    }
                    ProjectionEffect::Update(update) => {
                        ("update", update.operation.identity.number().get())
                    }
                })
                .collect::<Vec<_>>(),
            [("close", 101), ("close", 102), ("update", 100)]
        );
        assert_eq!(world.published(&intent.first.id).unwrap().marker, Some(oid(2)));
        let converged =
            assert_restart_converges(world, &intent, &format!("alias-mask-{mask:03b}")).await;
        assert_eq!(converged.open_pull_requests(&intent.first.id).count(), 1);
        assert_eq!(converged.identity(&intent.first.id), identities[0]);
    }
}

#[tokio::test]
async fn mixed_projection_multibatch_prefix_converges() {
    let default = oid(1);
    let revision = LiteralRevision { head: oid(2), first_parent: default };
    let intent = root_intent(default, "Gmanyduplicates", revision);
    let mut initial = DurableWorld::for_intents(default, &[&intent]);
    initial.publish_for_setup(&intent.first.id, revision);
    for index in 0..66 {
        initial.open_for_setup(&intent.first.id, &format!("stale {index}"), "stale body");
    }
    initial.mark_for_setup(&intent.first.id, revision.head, BaseKind::Default);

    let cases = [
        ("first-none", 0, Box::new([]) as Box<[usize]>),
        ("first-partial", 0, Box::new([0, 31, 63])),
        ("first-all", 0, (0..64).collect()),
        ("second-none", 1, Box::new([])),
        ("second-close", 1, Box::new([0])),
        ("second-update", 1, Box::new([1])),
        ("second-all", 1, Box::new([0, 1])),
    ];
    for (label, batch, applied_aliases) in cases {
        let expected_remaining =
            if batch == 0 { 66 - applied_aliases.len() } else { 2 - applied_aliases.len() };
        let mut world = initial.clone();
        let stopped = world
            .run_attempt(
                &intent,
                &QueryVisibility::default(),
                Some(Interruption::Projection { batch, applied_aliases }),
            )
            .await
            .unwrap();
        assert_eq!(stopped.outcome, AttemptOutcome::Stopped(StopReason::Indeterminate));
        assert_eq!(stopped.trace.projections.len(), batch + 1, "{label}");
        assert_eq!(stopped.trace.projections[0].len(), 64, "{label}");
        let retry = run_acknowledged_retry(&mut world, &intent, label).await;
        assert_eq!(flatten(&retry.trace.projections).len(), expected_remaining, "{label}");
        assert_quiescent(&mut world, &intent, label).await;
    }
}

#[tokio::test]
async fn per_identity_visibility_never_authorizes_closing_an_unseen_lower_row() {
    let (initial, intent, identities) = three_open_root();
    for (label, hidden) in
        [("lower", vec![identities[0].clone()]), ("higher", vec![identities[2].clone()])]
    {
        let mut world = initial.clone();
        let visibility = QueryVisibility::hiding_identities(&world, hidden);
        world.run_attempt(&intent, &visibility, None).await.unwrap();
        assert!(world.exact_open_pull_request(&identities[0]).is_some());
        let converged = assert_restart_converges(world, &intent, label).await;
        assert_eq!(converged.identity(&intent.first.id), identities[0]);
    }

    let mut all_hidden = initial;
    let visibility = QueryVisibility::hiding_identities(&all_hidden, identities);
    let before = all_hidden.clone();
    let error = all_hidden.run_attempt(&intent, &visibility, None).await.unwrap_err();
    assert!(error.to_string().contains("marker"));
    assert_eq!(all_hidden, before, "marked absence cannot recreate or close any row");
}

#[tokio::test]
async fn delayed_create_after_root_retarget_is_repaired_as_a_duplicate() {
    let default = oid(1);
    let revision = LiteralRevision { head: oid(2), first_parent: default };
    let intent = root_intent(default, "Gdelayed", revision);
    let initial = DurableWorld::for_intents(default, &[&intent]);
    let stale = initial.plan(&intent, &QueryVisibility::default()).unwrap();
    let mut world = initial;
    world.run_attempt(&intent, &QueryVisibility::default(), None).await.unwrap();
    assert_eq!(world.open_pull_request(&intent.first.id).unwrap().base, BaseKind::Default);

    assert_eq!(
        world.execute_plan(stale, None).await.unwrap().outcome,
        AttemptOutcome::Acknowledged
    );
    assert_eq!(world.open_pull_requests(&intent.first.id).count(), 2);
    let retry = run_acknowledged_retry(&mut world, &intent, "delayed-root-create").await;
    assert!(matches!(
        flatten(&retry.trace.projections).as_ref(),
        [ProjectionEffect::Close { operation, .. }] if operation.identity.number().get() == 101
    ));
    assert_quiescent(&mut world, &intent, "delayed-root-create").await;
}

#[tokio::test]
async fn stale_update_stops_when_its_exact_selected_row_was_closed() {
    let default = oid(1);
    let revision = LiteralRevision { head: oid(2), first_parent: default };
    let intent = root_intent(default, "Gclosed", revision);
    let mut world = DurableWorld::for_intents(default, &[&intent]);
    establish_marked(&mut world, &intent.first, BaseKind::Default, "stale body");
    let stale_plan = world.plan(&intent, &QueryVisibility::default()).unwrap();
    let stale_visibility = QueryVisibility::stale(&world, [intent.first.id.clone()]);
    let identity = world.identity(&intent.first.id);
    let close = world.resolve_projection(TestPullRequestProjection::Close(TestClose {
        identity: identity.clone(),
    }));
    assert_eq!(
        world.apply_effect(&ExternalEffect::Projection(close)),
        EffectOutcome::Acknowledged,
        "the service deliberately permits closing any exact OPEN identity"
    );
    let stopped = world.execute_plan(stale_plan, None).await.unwrap();
    assert_eq!(stopped.outcome, AttemptOutcome::Stopped(StopReason::Indeterminate));
    assert_eq!(stopped.trace.projections.len(), 1);

    let before = world.clone();
    let stopped = world.run_attempt(&intent, &stale_visibility, None).await.unwrap();
    assert_eq!(stopped.outcome, AttemptOutcome::Stopped(StopReason::Indeterminate));
    assert!(matches!(
        flatten(&stopped.trace.projections).as_ref(),
        [ProjectionEffect::Update(update)] if update.operation.identity == identity
    ));
    assert_eq!(
        world, before,
        "a stale OPEN query remains visible, but cannot update its now-CLOSED exact row"
    );

    let error = world.run_attempt(&intent, &QueryVisibility::default(), None).await.unwrap_err();
    assert!(error.to_string().contains("Terminal pull request history"));
    assert_eq!(world, before, "terminal-only history cannot trigger recreation");
}

//! Restart and concurrency checks over literal durable state.
//!
//! This model deliberately knows nothing about refspec text, ls-remote,
//! GraphQL documents, JSON, or HTTP. Focused adapter tests own those encodings.
//! It retains Git request and GraphQL alias boundaries because those boundaries
//! determine which durable prefixes are reachable after interruption. A local
//! intent is planned against a shared durable world, typed planner effects are
//! applied with the external system semantics which matter to the protocol,
//! and every retry constructs a fresh plan.

use std::collections::{HashMap, HashSet};

use gix::ObjectId;

use super::plan_local_publication;
use crate::pre_push::{
    body::BodyLinkContext,
    destination::{DefaultBranch, PushDestination},
    github::CorrelatedRepository,
    history::CommitGraphEvidence,
    local::{GherritPrId, LocalStack},
    pull_request::{
        BaseKind, LocalPullRequestObservation, ManagedOpenPullRequest, PullRequestIdentity,
    },
    remote::{ActiveRemoteChanges, ObservedChangeHistory},
    test_effect::{
        CreateEffect, EffectBatches, MarkerEffect, RevisionEffect, Stage, TupleEffect, UpdateEffect,
    },
};

const DEFAULT_BRANCH: &str = "main";
const REPOSITORY_ID: &str = "R_recovery";
const CONVERGENCE_STEP_LIMIT: usize = 8;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct LiteralRevision {
    head: ObjectId,
    first_parent: ObjectId,
}

impl LiteralRevision {
    fn effect(self) -> RevisionEffect {
        RevisionEffect { head: self.head, first_parent: self.first_parent }
    }

    fn from_effect(effect: RevisionEffect) -> Self {
        Self { head: effect.head, first_parent: effect.first_parent }
    }
}

/// Literal durable fields of one OPEN pull request.
///
/// Body bytes have no "fresh" marker. Whether they are desired is a property
/// of one local intent, not a property of durable GitHub state.
#[derive(Clone, Debug, Eq, PartialEq)]
struct PullRequest {
    identity: PullRequestIdentity,
    base: BaseKind,
    title: String,
    body: String,
}

/// Durable state owned by one stable change ID.
#[derive(Clone, Debug, Eq, PartialEq)]
struct PublishedChange {
    id: GherritPrId,
    published: Vec<LiteralRevision>,
    // Marker and projection are deliberately independent. A marker may target
    // an older version while an amended pull request remains stale.
    marker: Option<ObjectId>,
    pull_request: Option<PullRequest>,
}

/// State shared by every publisher.
#[derive(Clone, Debug, Eq, PartialEq)]
struct DurableWorld {
    default_tip: ObjectId,
    changes: Vec<PublishedChange>,
}

/// One publisher's immutable local proposal.
#[derive(Clone, Debug, Eq, PartialEq)]
struct LocalChange {
    id: GherritPrId,
    desired: LiteralRevision,
    title: String,
    commit_body: String,
}

/// One ordered local stack. Multiple intents may overlap the same durable IDs.
#[derive(Clone, Debug, Eq, PartialEq)]
struct LocalIntent {
    changes: Vec<LocalChange>,
}

#[derive(Clone, Debug, Default)]
struct Visibility {
    hidden_open: HashSet<GherritPrId>,
}

impl Visibility {
    fn hiding(id: &GherritPrId) -> Self {
        Self { hidden_open: [id.clone()].into_iter().collect() }
    }

    fn hides(&self, id: &GherritPrId) -> bool {
        self.hidden_open.contains(id)
    }
}

#[derive(Clone, Debug)]
enum ExternalEffect {
    Tuple(TupleEffect),
    Create(CreateEffect),
    Marker(MarkerEffect),
    Update(UpdateEffect),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum EffectOutcome {
    Acknowledged,
    AppliedButIndeterminate,
    Rejected,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Publisher {
    A,
    B,
}

#[derive(Clone, Debug)]
struct AppliedEffect {
    publisher: Publisher,
    index: usize,
    outcome: EffectOutcome,
}

#[derive(Clone, Debug)]
struct InterleavingResult {
    world: DurableWorld,
    applied: Vec<AppliedEffect>,
}

fn id(value: &str) -> GherritPrId {
    GherritPrId::from_ref_component(value.as_bytes()).unwrap()
}

fn oid(byte: u8) -> ObjectId {
    ObjectId::from_bytes_or_panic(&[byte; 20])
}

fn indexed_oid(index: usize) -> ObjectId {
    let mut bytes = [0x42; 20];
    bytes[12..].copy_from_slice(&u64::try_from(index).unwrap().to_be_bytes());
    ObjectId::from_bytes_or_panic(&bytes)
}

fn identity(index: usize) -> PullRequestIdentity {
    PullRequestIdentity::new(100 + u64::try_from(index).unwrap(), format!("PR_{}", 100 + index))
        .unwrap()
}

fn owned_base_name(id: &GherritPrId) -> String {
    format!("gherrit-bases/{}", id.as_str())
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

fn intent(changes: Vec<LocalChange>) -> LocalIntent {
    let mut ids = HashSet::with_capacity(changes.len());
    assert!(
        changes.iter().all(|change| ids.insert(change.id.clone())),
        "one local intent cannot repeat a stable change ID"
    );
    LocalIntent { changes }
}

impl DurableWorld {
    fn for_intents(default_tip: ObjectId, intents: &[&LocalIntent]) -> Self {
        let mut changes = Vec::<PublishedChange>::new();
        for local in intents.iter().flat_map(|intent| &intent.changes) {
            if changes.iter().any(|change| change.id == local.id) {
                continue;
            }
            changes.push(PublishedChange {
                id: local.id.clone(),
                published: Vec::new(),
                marker: None,
                pull_request: None,
            });
        }
        let world = Self { default_tip, changes };
        world.assert_safe();
        world
    }

    fn position(&self, id: &GherritPrId) -> usize {
        self.changes.iter().position(|change| change.id == *id).unwrap()
    }

    fn change(&self, id: &GherritPrId) -> &PublishedChange {
        &self.changes[self.position(id)]
    }

    fn change_mut(&mut self, id: &GherritPrId) -> &mut PublishedChange {
        let position = self.position(id);
        &mut self.changes[position]
    }

    fn publish_for_setup(&mut self, id: &GherritPrId, revision: LiteralRevision) {
        self.change_mut(id).published.push(revision);
        self.assert_safe();
    }

    fn open_for_setup(&mut self, id: &GherritPrId, base: BaseKind, title: &str, body: &str) {
        let position = self.position(id);
        let change = &mut self.changes[position];
        assert!(!change.published.is_empty(), "an OPEN pull request requires published history");
        assert!(change.pull_request.is_none(), "test setup cannot replace an OPEN pull request");
        change.pull_request = Some(PullRequest {
            identity: identity(position),
            base,
            title: title.to_owned(),
            body: body.to_owned(),
        });
        self.assert_safe();
    }

    fn mark_for_setup(&mut self, id: &GherritPrId, target: ObjectId) {
        let change = self.change_mut(id);
        assert!(change.published.iter().any(|revision| revision.head == target));
        assert!(change.pull_request.is_some(), "a reachable marker requires an OPEN identity");
        assert!(change.marker.replace(target).is_none(), "test setup cannot move a marker");
        self.assert_safe();
    }

    fn plan(&self, intent: &LocalIntent, visibility: &Visibility) -> Result<Stage, String> {
        let destination = PushDestination::for_test(
            "origin",
            "https://github.com/owner/repository.git",
            Vec::new(),
        )
        .unwrap();
        let default_branch =
            DefaultBranch::new(DEFAULT_BRANCH.to_owned(), self.default_tip).unwrap();
        let stack = LocalStack::for_test_with_content(
            self.default_tip,
            intent.changes.iter().map(|change| {
                (
                    change.id.clone(),
                    change.desired.head,
                    change.title.clone(),
                    change.commit_body.clone(),
                )
            }),
        )
        .unwrap();
        let remote_changes = intent
            .changes
            .iter()
            .map(|local| {
                let durable = self.change(&local.id);
                let published = durable
                    .published
                    .iter()
                    .map(|revision| (revision.head, revision.first_parent))
                    .collect::<Vec<_>>();
                ObservedChangeHistory::from_typed_for_test(
                    local.id.clone(),
                    &published,
                    durable.marker,
                )
                .unwrap()
            })
            .collect();
        let active = ActiveRemoteChanges::from_typed_for_test(
            &destination,
            default_branch.clone(),
            remote_changes,
        );
        let local_pull_requests = intent
            .changes
            .iter()
            .map(|local| {
                let durable = self.change(&local.id);
                match &durable.pull_request {
                    Some(pull_request) if !visibility.hides(&local.id) => {
                        let current = durable
                            .published
                            .last()
                            .expect("an observed OPEN pull request has published history");
                        let base_oid = match pull_request.base {
                            BaseKind::Default => self.default_tip,
                            BaseKind::Owned => current.first_parent,
                        };
                        LocalPullRequestObservation::Open(
                            ManagedOpenPullRequest::from_typed_for_test(
                                local.id.clone(),
                                pull_request.identity.clone(),
                                current.head,
                                pull_request.base,
                                base_oid,
                                pull_request.title.clone(),
                                pull_request.body.clone(),
                            ),
                        )
                    }
                    Some(_) | None => LocalPullRequestObservation::Absent(
                        super::super::pull_request::AbsentPullRequest::for_test(local.id.clone()),
                    ),
                }
            })
            .collect::<Vec<_>>();
        let correlated = CorrelatedRepository::from_typed_for_test(
            &destination,
            REPOSITORY_ID.to_owned(),
            default_branch,
            local_pull_requests,
        )
        .unwrap();
        let context = BodyLinkContext::from_destination(&destination, None).unwrap();
        plan_local_publication(context, stack, correlated, active, &self.graph(intent))
            .map(|plan| plan.first_stage_for_test())
            .map_err(|error| error.to_string())
    }

    /// Supplies graph evidence only for IDs in this intent.
    ///
    /// The literal world may contain many other publishers' durable changes,
    /// but an exact-local plan neither observes nor traverses them.
    fn graph(&self, intent: &LocalIntent) -> CommitGraphEvidence {
        let mut commits = HashMap::<ObjectId, (Vec<ObjectId>, Vec<GherritPrId>)>::new();
        commits.insert(self.default_tip, (Vec::new(), Vec::new()));
        for local in &intent.changes {
            let durable = self.change(&local.id);
            for revision in durable.published.iter().chain([&local.desired]) {
                let value = (vec![revision.first_parent], vec![local.id.clone()]);
                if let Some(previous) = commits.insert(revision.head, value.clone()) {
                    assert_eq!(previous, value, "one object cannot represent two literal commits");
                }
            }
        }
        CommitGraphEvidence::from_literal_commits_for_test(
            commits.into_iter().map(|(head, (parents, identities))| (head, parents, identities)),
        )
        .unwrap()
    }

    fn assert_safe(&self) {
        let mut ids = HashSet::with_capacity(self.changes.len());
        let mut identities = Vec::<PullRequestIdentity>::new();
        for change in &self.changes {
            assert!(ids.insert(change.id.clone()), "durable state repeats a stable change ID");
            if let Some(marker) = change.marker {
                assert!(
                    change.published.iter().any(|revision| revision.head == marker),
                    "a marker must target immutable published history"
                );
                assert!(
                    change.pull_request.is_some(),
                    "a reachable marker must have been authorized by an OPEN identity"
                );
            }
            if let Some(pull_request) = &change.pull_request {
                assert!(
                    !change.published.is_empty(),
                    "an OPEN pull request must have a published head and owned base"
                );
                assert!(
                    !identities.contains(&pull_request.identity),
                    "literal GitHub identities must be unique"
                );
                identities.push(pull_request.identity.clone());
            }
        }
    }

    fn apply_effect(&mut self, effect: &ExternalEffect) -> EffectOutcome {
        let before = self.clone();
        let target = effect.target_id(&before);
        let outcome = match effect {
            ExternalEffect::Tuple(effect) => self.apply_tuple(effect),
            ExternalEffect::Create(effect) => self.apply_create(effect),
            ExternalEffect::Marker(effect) => self.apply_marker(effect),
            ExternalEffect::Update(effect) => self.apply_update(effect),
        };

        match outcome {
            EffectOutcome::Rejected => {
                assert_eq!(*self, before, "a rejected external effect must be state-preserving");
            }
            EffectOutcome::Acknowledged | EffectOutcome::AppliedButIndeterminate => {
                let target = target.expect("an applied effect has one durable target");
                self.assert_transition(&before, &target, effect);
                self.assert_safe();
            }
        }
        outcome
    }

    fn apply_tuple(&mut self, effect: &TupleEffect) -> EffectOutcome {
        let Some(position) = self.changes.iter().position(|change| change.id == effect.id) else {
            return EffectOutcome::Rejected;
        };
        let change = &mut self.changes[position];
        if tuple_is_already_desired(change, effect) {
            // Git does not need to update an already-desired destination, so
            // it reports the ref as up to date without enforcing the stale
            // lease which guarded the now-unnecessary update.
            return EffectOutcome::Acknowledged;
        }
        let previous = change.published.last().copied().map(LiteralRevision::effect);
        if previous != effect.previous
            || effect.version != u64::try_from(change.published.len()).unwrap() + 1
        {
            return EffectOutcome::Rejected;
        }
        change.published.push(LiteralRevision::from_effect(effect.desired));
        EffectOutcome::Acknowledged
    }

    fn apply_create(&mut self, effect: &CreateEffect) -> EffectOutcome {
        let Some(position) = self.changes.iter().position(|change| change.id == effect.id) else {
            return EffectOutcome::Rejected;
        };
        let change = &mut self.changes[position];
        if effect.repository_id != REPOSITORY_ID
            || effect.base_branch != owned_base_name(&effect.id)
            || change.published.is_empty()
            || change.pull_request.is_some()
        {
            return EffectOutcome::Rejected;
        }
        let current = *change.published.last().unwrap();
        change.pull_request = Some(PullRequest {
            identity: identity(position),
            base: BaseKind::Owned,
            title: effect.title.clone(),
            body: effect.body.clone(),
        });
        if effect.head_oid == current.head && effect.base_oid == current.first_parent {
            EffectOutcome::Acknowledged
        } else {
            // The create request names branches, not object IDs. A concurrent
            // tuple can therefore move both branches before GitHub evaluates
            // the mutation. The returned OIDs fail receipt validation, but the
            // provisional pull request is still durable and safe.
            EffectOutcome::AppliedButIndeterminate
        }
    }

    fn apply_marker(&mut self, effect: &MarkerEffect) -> EffectOutcome {
        let Some(position) = self.changes.iter().position(|change| change.id == effect.id) else {
            return EffectOutcome::Rejected;
        };
        let change = &mut self.changes[position];
        if change.marker == Some(effect.target) {
            // As with an already-desired tuple ref, Git acknowledges this
            // create-only ref as up to date before its absence lease matters.
            return EffectOutcome::Acknowledged;
        }
        if change.marker.is_some()
            || change.pull_request.is_none()
            || !change.published.iter().any(|revision| revision.head == effect.target)
        {
            return EffectOutcome::Rejected;
        }
        change.marker = Some(effect.target);
        EffectOutcome::Acknowledged
    }

    fn apply_update(&mut self, effect: &UpdateEffect) -> EffectOutcome {
        let Some(position) = self.changes.iter().position(|change| {
            change
                .pull_request
                .as_ref()
                .is_some_and(|pull_request| pull_request.identity == effect.identity)
        }) else {
            return EffectOutcome::Rejected;
        };
        let id = self.changes[position].id.clone();
        let base = match effect.base_branch.as_deref() {
            None => None,
            Some(DEFAULT_BRANCH) => Some(BaseKind::Default),
            Some(base) if base == owned_base_name(&id) => Some(BaseKind::Owned),
            Some(_) => return EffectOutcome::Rejected,
        };
        let pull_request = self.changes[position].pull_request.as_mut().unwrap();
        if let Some(title) = &effect.title {
            pull_request.title = title.clone();
        }
        if let Some(body) = &effect.body {
            pull_request.body = body.clone();
        }
        if let Some(base) = base {
            pull_request.base = base;
        }
        EffectOutcome::Acknowledged
    }

    fn assert_transition(&self, before: &Self, target: &GherritPrId, effect: &ExternalEffect) {
        assert_eq!(self.default_tip, before.default_tip, "publication cannot move the default");
        assert_eq!(self.changes.len(), before.changes.len());
        for (old, new) in before.changes.iter().zip(&self.changes) {
            assert_eq!(old.id, new.id);
            if old.id != *target {
                assert_eq!(old, new, "one exact-local effect cannot mutate another change");
                continue;
            }
            assert!(
                new.published.starts_with(&old.published),
                "immutable version history cannot be rewritten"
            );
            if old.marker.is_some() {
                assert_eq!(old.marker, new.marker, "an immutable marker cannot move");
            }
            if let (Some(old), Some(new)) = (&old.pull_request, &new.pull_request) {
                assert_eq!(old.identity, new.identity, "an OPEN identity cannot be replaced");
            }
        }

        let old = before.change(target);
        let new = self.change(target);
        match effect {
            ExternalEffect::Tuple(effect) => {
                if tuple_is_already_desired(old, effect) {
                    assert_eq!(new, old, "an already-desired tuple is an acknowledged no-op");
                } else {
                    assert_eq!(new.published.len(), old.published.len() + 1);
                    assert_eq!(
                        new.published.last().copied(),
                        Some(LiteralRevision::from_effect(effect.desired))
                    );
                }
                assert_eq!(new.marker, old.marker);
                assert_eq!(new.pull_request, old.pull_request);
            }
            ExternalEffect::Create(_) => {
                assert_eq!(new.published, old.published);
                assert_eq!(new.marker, old.marker);
                assert!(old.pull_request.is_none());
                assert_eq!(new.pull_request.as_ref().unwrap().base, BaseKind::Owned);
            }
            ExternalEffect::Marker(effect) => {
                if old.marker == Some(effect.target) {
                    assert_eq!(new, old, "an already-desired marker is an acknowledged no-op");
                } else {
                    assert_eq!(new.published, old.published);
                    assert_eq!(old.marker, None);
                    assert_eq!(new.marker, Some(effect.target));
                    assert_eq!(new.pull_request, old.pull_request);
                }
            }
            ExternalEffect::Update(effect) => {
                assert_eq!(new.published, old.published);
                assert_eq!(new.marker, old.marker);
                let old = old.pull_request.as_ref().unwrap();
                let new = new.pull_request.as_ref().unwrap();
                if effect.title.is_none() {
                    assert_eq!(new.title, old.title);
                }
                if effect.body.is_none() {
                    assert_eq!(new.body, old.body);
                }
                if effect.base_branch.is_none() {
                    assert_eq!(new.base, old.base);
                }
            }
        }
    }
}

/// All three destinations in a tuple are already desired when its mutable
/// branches name `desired` and its immutable version tag already records the
/// same revision. Git then acknowledges every ref as up to date without an
/// update, even if the leases captured by an older plan are stale.
fn tuple_is_already_desired(change: &PublishedChange, effect: &TupleEffect) -> bool {
    let desired = LiteralRevision::from_effect(effect.desired);
    let version_index = effect.version.checked_sub(1).and_then(|index| usize::try_from(index).ok());
    change.published.last() == Some(&desired)
        && version_index.and_then(|index| change.published.get(index)) == Some(&desired)
}

impl ExternalEffect {
    fn target_id(&self, world: &DurableWorld) -> Option<GherritPrId> {
        match self {
            Self::Tuple(effect) => Some(effect.id.clone()),
            Self::Create(effect) => Some(effect.id.clone()),
            Self::Marker(effect) => Some(effect.id.clone()),
            Self::Update(effect) => world
                .changes
                .iter()
                .find(|change| {
                    change
                        .pull_request
                        .as_ref()
                        .is_some_and(|pull_request| pull_request.identity == effect.identity)
                })
                .map(|change| change.id.clone()),
        }
    }
}

fn flatten_batches<T: Clone>(batches: &[Box<[T]>]) -> Box<[T]> {
    batches.iter().flat_map(|batch| batch.iter().cloned()).collect()
}

fn tuple_batches(stage: Stage) -> EffectBatches<TupleEffect> {
    let Stage::Tuples(batches) = stage else { panic!("expected tuples") };
    batches
}

/// Flattens tuple batches only for assertions where every batch is known to
/// be acknowledged. Tests of interruption preserve the batch boundaries.
fn tuples(stage: Stage) -> Box<[TupleEffect]> {
    flatten_batches(&tuple_batches(stage))
}

fn create_batches(stage: Stage) -> EffectBatches<CreateEffect> {
    let Stage::Creates(batches) = stage else { panic!("expected creates") };
    batches
}

/// Flattens create batches only where transport interruption is irrelevant.
fn creates(stage: Stage) -> Box<[CreateEffect]> {
    flatten_batches(&create_batches(stage))
}

fn marker_batches(stage: Stage) -> EffectBatches<MarkerEffect> {
    let Stage::Markers(batches) = stage else { panic!("expected markers") };
    batches
}

/// Flattens marker batches only for complete-stage assertions.
fn markers(stage: Stage) -> Box<[MarkerEffect]> {
    flatten_batches(&marker_batches(stage))
}

fn update_batches(stage: Stage) -> EffectBatches<UpdateEffect> {
    let Stage::Updates(batches) = stage else { panic!("expected updates") };
    batches
}

/// Flattens update batches only where transport interruption is irrelevant.
fn updates(stage: Stage) -> Box<[UpdateEffect]> {
    flatten_batches(&update_batches(stage))
}

/// Returns the effects of one completely sent stage for the bounded
/// concurrency schedules below.
///
/// Git schedules use a single one-effect atomic batch. GraphQL schedules use
/// a single request whose aliases may complete independently. Cross-request
/// interruption is tested separately without erasing request boundaries.
fn one_sent_batch_effects(stage: &Stage) -> Vec<ExternalEffect> {
    match stage {
        Stage::Tuples(batches) => {
            assert_eq!(batches.len(), 1, "the concurrency fixture has one Git batch");
            assert_eq!(batches[0].len(), 1, "the concurrency fixture has one atomic Git effect");
            batches[0].iter().cloned().map(ExternalEffect::Tuple).collect()
        }
        Stage::Creates(batches) => {
            assert_eq!(batches.len(), 1, "the concurrency fixture has one GraphQL request");
            batches[0].iter().cloned().map(ExternalEffect::Create).collect()
        }
        Stage::Markers(batches) => {
            assert_eq!(batches.len(), 1, "the concurrency fixture has one Git batch");
            assert_eq!(batches[0].len(), 1, "the concurrency fixture has one atomic Git effect");
            batches[0].iter().cloned().map(ExternalEffect::Marker).collect()
        }
        Stage::Updates(batches) => {
            assert_eq!(batches.len(), 1, "the concurrency fixture has one GraphQL request");
            batches[0].iter().cloned().map(ExternalEffect::Update).collect()
        }
        Stage::Done => Vec::new(),
    }
}

fn assert_done(world: &DurableWorld, intent: &LocalIntent, label: &str) {
    assert_eq!(
        world
            .plan(intent, &Visibility::default())
            .unwrap_or_else(|error| panic!("{label}: {error}")),
        Stage::Done,
        "{label}: converged durable state must require no action"
    );
}

fn apply_acknowledged(world: &mut DurableWorld, stage: &Stage) {
    match stage {
        Stage::Tuples(batches) => {
            for batch in batches {
                assert_eq!(
                    apply_atomic_git_batch(world, batch, ExternalEffect::Tuple),
                    EffectOutcome::Acknowledged,
                    "a freshly planned atomic tuple batch must satisfy every lease"
                );
            }
        }
        Stage::Markers(batches) => {
            for batch in batches {
                assert_eq!(
                    apply_atomic_git_batch(world, batch, ExternalEffect::Marker),
                    EffectOutcome::Acknowledged,
                    "a freshly planned atomic marker batch must satisfy every lease"
                );
            }
        }
        Stage::Creates(batches) => {
            for batch in batches {
                let outcomes =
                    apply_graphql_aliases(world, batch, |_| true, ExternalEffect::Create);
                assert!(
                    outcomes.iter().all(|outcome| *outcome == EffectOutcome::Acknowledged),
                    "a freshly planned create request must have exact acknowledgements"
                );
            }
        }
        Stage::Updates(batches) => {
            for batch in batches {
                let outcomes =
                    apply_graphql_aliases(world, batch, |_| true, ExternalEffect::Update);
                assert!(
                    outcomes.iter().all(|outcome| *outcome == EffectOutcome::Acknowledged),
                    "a freshly planned update request must have exact acknowledgements"
                );
            }
        }
        Stage::Done => {}
    }
}

/// Applies one Git request atomically. A failed lease rejects the whole
/// request, including effects whose individual preconditions happened to
/// hold, exactly as `git push --atomic` does.
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

/// Applies an arbitrary subset of the aliases in one already-sent GraphQL
/// request. A missing or invalid acknowledgement stops later requests, but it
/// cannot unsend sibling aliases in the current request.
fn apply_graphql_aliases<T: Clone>(
    world: &mut DurableWorld,
    batch: &[T],
    mut selected: impl FnMut(usize) -> bool,
    wrap: impl Fn(T) -> ExternalEffect,
) -> Vec<EffectOutcome> {
    batch
        .iter()
        .cloned()
        .enumerate()
        .filter(|(index, _)| selected(*index))
        .map(|(_, effect)| world.apply_effect(&wrap(effect)))
        .collect()
}

/// Applies the durable part of one interrupted GraphQL attempt: every earlier
/// request completed, an arbitrary subset of the current request completed,
/// and no later request was sent.
fn apply_interrupted_graphql_attempt<T: Clone>(
    world: &mut DurableWorld,
    batches: &[Box<[T]>],
    current: usize,
    current_aliases: &[usize],
    wrap: impl Fn(T) -> ExternalEffect + Copy,
) -> Vec<EffectOutcome> {
    for batch in &batches[..current] {
        let outcomes = apply_graphql_aliases(world, batch, |_| true, wrap);
        assert!(outcomes.iter().all(|outcome| *outcome == EffectOutcome::Acknowledged));
    }
    apply_graphql_aliases(world, &batches[current], |index| current_aliases.contains(&index), wrap)
}

fn stage_measure(stage: &Stage) -> (u8, usize) {
    match stage {
        Stage::Tuples(effects) => (4, effects.len()),
        Stage::Creates(effects) => (3, effects.len()),
        Stage::Markers(effects) => (2, effects.len()),
        Stage::Updates(effects) => (
            1,
            effects
                .iter()
                .flatten()
                .map(|effect| {
                    usize::from(effect.title.is_some())
                        + usize::from(effect.body.is_some())
                        + usize::from(effect.base_branch.is_some())
                })
                .sum(),
        ),
        Stage::Done => (0, 0),
    }
}

/// Repeatedly applies freshly planned, acknowledged work for one stable intent.
fn converge(mut world: DurableWorld, intent: &LocalIntent, label: &str) -> DurableWorld {
    let mut stage = world
        .plan(intent, &Visibility::default())
        .unwrap_or_else(|error| panic!("{label}/initial: {error}"));
    for step in 0..CONVERGENCE_STEP_LIMIT {
        if stage == Stage::Done {
            return world;
        }
        let before = stage_measure(&stage);
        apply_acknowledged(&mut world, &stage);
        let after_stage = world
            .plan(intent, &Visibility::default())
            .unwrap_or_else(|error| panic!("{label}/step-{step}/after: {error}"));
        let after = stage_measure(&after_stage);
        assert!(
            after < before,
            "{label}/step-{step}: an acknowledged stable-intent stage must make progress: \
             before={before:?}, after={after:?}"
        );
        stage = after_stage;
    }
    panic!("{label}: stable intent did not converge within {CONVERGENCE_STEP_LIMIT} stages");
}

fn enumerate_interleavings(a_len: usize, b_len: usize) -> Vec<Vec<Publisher>> {
    fn append(
        remaining_a: usize,
        remaining_b: usize,
        prefix: &mut Vec<Publisher>,
        schedules: &mut Vec<Vec<Publisher>>,
    ) {
        if remaining_a == 0 && remaining_b == 0 {
            schedules.push(prefix.clone());
            return;
        }
        if remaining_a != 0 {
            prefix.push(Publisher::A);
            append(remaining_a - 1, remaining_b, prefix, schedules);
            prefix.pop();
        }
        if remaining_b != 0 {
            prefix.push(Publisher::B);
            append(remaining_a, remaining_b - 1, prefix, schedules);
            prefix.pop();
        }
    }

    let mut schedules = Vec::new();
    append(a_len, b_len, &mut Vec::new(), &mut schedules);
    schedules
}

fn run_interleaving(
    initial: &DurableWorld,
    a: &[ExternalEffect],
    b: &[ExternalEffect],
    schedule: &[Publisher],
) -> InterleavingResult {
    // These bounded schedules use one-operation Git/create stages or
    // independently applicable update aliases. A GraphQL batch is already
    // wholly sent before one bad alias receipt is known, so this helper must
    // not pretend that such a receipt suppresses sibling aliases.
    let mut world = initial.clone();
    let mut next = [0_usize; 2];
    let mut applied = Vec::new();
    for publisher in schedule {
        let publisher_index = match publisher {
            Publisher::A => 0,
            Publisher::B => 1,
        };
        let effects = match publisher {
            Publisher::A => a,
            Publisher::B => b,
        };
        let index = next[publisher_index];
        next[publisher_index] += 1;
        let outcome = world.apply_effect(&effects[index]);
        assert!(
            outcome == EffectOutcome::Acknowledged || effects.len() == 1,
            "a non-acknowledged multi-alias stage requires a batch-level model"
        );
        applied.push(AppliedEffect { publisher: *publisher, index, outcome });
    }
    assert_eq!(next, [a.len(), b.len()]);
    InterleavingResult { world, applied }
}

fn root_intent(name: &str, revision: LiteralRevision, title: &str) -> LocalIntent {
    intent(vec![local_change(name, revision, title, &format!("Body for {name}"))])
}

fn git_batch_intent() -> LocalIntent {
    const CHANGE_COUNT: usize = 33;
    const ID_BYTES: usize = 200;

    let default_tip = indexed_oid(1);
    let mut first_parent = default_tip;
    intent(
        (0..CHANGE_COUNT)
            .map(|index| {
                let head = indexed_oid(index + 2);
                let prefix = format!("G{index:02}");
                let change_id = format!("{prefix}{}", "a".repeat(ID_BYTES - prefix.len()));
                let change = local_change(
                    &change_id,
                    LiteralRevision { head, first_parent },
                    &format!("Change {index}"),
                    &format!("Body for change {index}"),
                );
                first_parent = head;
                change
            })
            .collect(),
    )
}

fn graphql_batch_intent(head_offset: usize) -> LocalIntent {
    const CHANGE_COUNT: usize = 5;
    const COMMIT_BODY_BYTES: usize = 50_000;

    let default_tip = indexed_oid(1);
    let commit_body = "\u{1}".repeat(COMMIT_BODY_BYTES);
    let mut first_parent = default_tip;
    intent(
        (0..CHANGE_COUNT)
            .map(|index| {
                let head = indexed_oid(head_offset + index);
                let change = local_change(
                    &format!("G{index}"),
                    LiteralRevision { head, first_parent },
                    &format!("Change {index}"),
                    &commit_body,
                );
                first_parent = head;
                change
            })
            .collect(),
    )
}

#[test]
fn fresh_root_recovers_at_every_barrier_and_fails_closed_after_marker() {
    let root = LiteralRevision { head: oid(10), first_parent: oid(1) };
    let intent = root_intent("G0", root, "Root");
    let mut world = DurableWorld::for_intents(oid(1), &[&intent]);
    let exact = Visibility::default();

    let tuple_stage = world.plan(&intent, &exact).unwrap();
    let tuple_effects = tuples(tuple_stage.clone());
    assert_eq!(
        tuple_effects,
        [TupleEffect { id: id("G0"), previous: None, desired: root.effect(), version: 1 }].into()
    );
    apply_acknowledged(&mut world, &tuple_stage);

    let create_stage = world.plan(&intent, &exact).unwrap();
    let create_effects = creates(create_stage.clone());
    assert_eq!(create_effects.len(), 1);
    assert_eq!(create_effects[0].id, id("G0"));
    assert_eq!(create_effects[0].repository_id, REPOSITORY_ID);
    assert_eq!(create_effects[0].base_branch, "gherrit-bases/G0");
    assert_eq!(create_effects[0].title, "Root");
    assert!(!create_effects[0].body.is_empty());
    assert_eq!(create_effects[0].head_oid, root.head);
    assert_eq!(create_effects[0].base_oid, root.first_parent);
    insta::assert_snapshot!("fresh_create_body", create_effects[0].body);
    apply_acknowledged(&mut world, &create_stage);

    let hidden = Visibility::hiding(&id("G0"));
    assert_eq!(
        creates(world.plan(&intent, &hidden).unwrap()),
        create_effects,
        "a hidden provisional PR retries the identical stable create key and payload"
    );

    let marker_stage = world.plan(&intent, &exact).unwrap();
    let marker_effects = markers(marker_stage.clone());
    assert_eq!(marker_effects, [MarkerEffect { id: id("G0"), target: root.head }].into());
    apply_acknowledged(&mut world, &marker_stage);

    let error = world.plan(&intent, &hidden).unwrap_err();
    assert!(error.contains("G0"), "{error}");
    assert!(error.contains("marker"), "{error}");
    assert!(error.contains("pull-request"), "{error}");

    let update_stage = world.plan(&intent, &exact).unwrap();
    let update_effects = updates(update_stage.clone());
    assert_eq!(update_effects.len(), 1);
    assert_eq!(update_effects[0].identity, identity(0));
    assert!(update_effects[0].body.is_some());
    assert_eq!(update_effects[0].base_branch.as_deref(), Some(DEFAULT_BRANCH));
    insta::assert_snapshot!(
        "fresh_update_body",
        update_effects[0].body.as_deref().expect("the provisional body needs its final numbers")
    );
    assert_eq!(
        updates(world.plan(&intent, &exact).unwrap()),
        update_effects,
        "a lost update acknowledgement leaves the exact literal patch on retry"
    );
    apply_acknowledged(&mut world, &update_stage);
    assert_done(&world, &intent, "fresh/done");
}

#[test]
fn two_changes_recover_from_holey_create_and_update_results() {
    let first = LiteralRevision { head: oid(10), first_parent: oid(1) };
    let second = LiteralRevision { head: oid(11), first_parent: first.head };
    let intent = intent(vec![
        local_change("G0", first, "First", "Body for G0"),
        local_change("G1", second, "Second", "Body for G1"),
    ]);
    let mut published = DurableWorld::for_intents(oid(1), &[&intent]);
    let exact = Visibility::default();
    let tuple_stage = published.plan(&intent, &exact).unwrap();
    assert_eq!(tuples(tuple_stage.clone()).len(), 2);
    apply_acknowledged(&mut published, &tuple_stage);
    let initial_create_batches = create_batches(published.plan(&intent, &exact).unwrap());
    assert_eq!(initial_create_batches.len(), 1, "the small fixture uses one GraphQL request");
    let initial_creates = &initial_create_batches[0];
    assert_eq!(initial_creates.len(), 2);

    for create_mask in 0..4 {
        let mut world = published.clone();
        let outcomes = apply_graphql_aliases(
            &mut world,
            initial_creates,
            |index| create_mask & (1 << index) != 0,
            ExternalEffect::Create,
        );
        assert!(outcomes.iter().all(|outcome| *outcome == EffectOutcome::Acknowledged));
        if create_mask != 3 {
            let remaining = world.plan(&intent, &exact).unwrap();
            assert!(matches!(remaining, Stage::Creates(_)));
            apply_acknowledged(&mut world, &remaining);
        }

        let marker_stage = world.plan(&intent, &exact).unwrap();
        assert_eq!(markers(marker_stage.clone()).len(), 2);
        apply_acknowledged(&mut world, &marker_stage);
        let all_update_batches = update_batches(world.plan(&intent, &exact).unwrap());
        assert_eq!(all_update_batches.len(), 1, "the small fixture uses one GraphQL request");
        let all_updates = &all_update_batches[0];
        assert_eq!(all_updates.len(), 2);

        for update_mask in 0..4 {
            let mut restarted = world.clone();
            let outcomes = apply_graphql_aliases(
                &mut restarted,
                all_updates,
                |index| update_mask & (1 << index) != 0,
                ExternalEffect::Update,
            );
            assert!(outcomes.iter().all(|outcome| *outcome == EffectOutcome::Acknowledged));
            if update_mask != 3 {
                let remaining_stage = restarted.plan(&intent, &exact).unwrap();
                let remaining = updates(remaining_stage.clone());
                let expected = all_updates
                    .iter()
                    .enumerate()
                    .filter(|(index, _)| update_mask & (1 << index) == 0)
                    .map(|(_, effect)| effect.clone())
                    .collect::<Box<[_]>>();
                assert_eq!(
                    remaining, expected,
                    "a restart must retain exactly the unapplied literal aliases"
                );
                apply_acknowledged(&mut restarted, &remaining_stage);
            }
            assert_done(
                &restarted,
                &intent,
                &format!("two/create-{create_mask}/update-{update_mask}"),
            );
        }
    }
}

#[test]
fn git_batches_expose_only_atomic_prefixes_and_replan_exact_suffixes() {
    let intent = git_batch_intent();
    let initial = DurableWorld::for_intents(indexed_oid(1), &[&intent]);
    let exact = Visibility::default();
    let all_tuple_batches = tuple_batches(initial.plan(&intent, &exact).unwrap());
    assert!(all_tuple_batches.len() > 1, "the fixture must cross the Git argument budget");

    for prefix_len in 0..=all_tuple_batches.len() {
        let mut restarted = initial.clone();
        for batch in &all_tuple_batches[..prefix_len] {
            assert_eq!(
                apply_atomic_git_batch(&mut restarted, batch, ExternalEffect::Tuple),
                EffectOutcome::Acknowledged
            );
        }
        let stage = restarted.plan(&intent, &exact).unwrap();
        if prefix_len == all_tuple_batches.len() {
            assert!(matches!(stage, Stage::Creates(_)));
        } else {
            assert_eq!(
                tuple_batches(stage).as_ref(),
                &all_tuple_batches[prefix_len..],
                "a restart must retain the exact unpublished atomic-batch suffix"
            );
        }
    }

    let mut rejected = initial.clone();
    let mut conflicting_effect =
        all_tuple_batches[0].last().expect("the first batch is nonempty").clone();
    conflicting_effect.desired.head = indexed_oid(10_000);
    assert_eq!(
        rejected.apply_effect(&ExternalEffect::Tuple(conflicting_effect)),
        EffectOutcome::Acknowledged
    );
    let before_rejected_batch = rejected.clone();
    assert_eq!(
        apply_atomic_git_batch(&mut rejected, &all_tuple_batches[0], ExternalEffect::Tuple),
        EffectOutcome::Rejected
    );
    assert_eq!(rejected, before_rejected_batch);

    let mut established = initial;
    apply_acknowledged(&mut established, &Stage::Tuples(all_tuple_batches));
    let creates = established.plan(&intent, &exact).unwrap();
    apply_acknowledged(&mut established, &creates);
    let all_marker_batches = marker_batches(established.plan(&intent, &exact).unwrap());
    assert!(all_marker_batches.len() > 1, "the fixture must cross the Git argument budget");

    for prefix_len in 0..=all_marker_batches.len() {
        let mut restarted = established.clone();
        for batch in &all_marker_batches[..prefix_len] {
            assert_eq!(
                apply_atomic_git_batch(&mut restarted, batch, ExternalEffect::Marker),
                EffectOutcome::Acknowledged
            );
        }
        let stage = restarted.plan(&intent, &exact).unwrap();
        if prefix_len == all_marker_batches.len() {
            assert!(matches!(stage, Stage::Updates(_)));
        } else {
            assert_eq!(
                marker_batches(stage).as_ref(),
                &all_marker_batches[prefix_len..],
                "a restart must retain the exact unpublished atomic-batch suffix"
            );
        }
    }

    let mut rejected = established;
    let marker = all_marker_batches[0].last().expect("the first batch is nonempty");
    let current = *rejected
        .change(&marker.id)
        .published
        .last()
        .expect("a marker target has published history");
    let conflicting_revision =
        LiteralRevision { head: indexed_oid(10_001), first_parent: current.first_parent };
    let conflicting_tuple = TupleEffect {
        id: marker.id.clone(),
        previous: Some(current.effect()),
        desired: conflicting_revision.effect(),
        version: u64::try_from(rejected.change(&marker.id).published.len()).unwrap() + 1,
    };
    assert_eq!(
        rejected.apply_effect(&ExternalEffect::Tuple(conflicting_tuple)),
        EffectOutcome::Acknowledged
    );
    assert_eq!(
        rejected.apply_effect(&ExternalEffect::Marker(MarkerEffect {
            id: marker.id.clone(),
            target: conflicting_revision.head,
        })),
        EffectOutcome::Acknowledged
    );
    let before_rejected_batch = rejected.clone();
    assert_eq!(
        apply_atomic_git_batch(&mut rejected, &all_marker_batches[0], ExternalEffect::Marker),
        EffectOutcome::Rejected
    );
    assert_eq!(rejected, before_rejected_batch);
}

#[test]
fn graphql_batches_stop_after_a_holey_current_request() {
    let intent = graphql_batch_intent(10);
    let mut published = DurableWorld::for_intents(indexed_oid(1), &[&intent]);
    let exact = Visibility::default();
    let tuples = published.plan(&intent, &exact).unwrap();
    apply_acknowledged(&mut published, &tuples);

    let create_batches = create_batches(published.plan(&intent, &exact).unwrap());
    let create_shape = create_batches.iter().map(|batch| batch.len()).collect::<Vec<_>>();
    assert_eq!(create_shape, [2, 2, 1], "use production request-size boundaries");
    let mut interrupted_create = published.clone();
    let outcomes = apply_interrupted_graphql_attempt(
        &mut interrupted_create,
        &create_batches,
        1,
        &[1],
        ExternalEffect::Create,
    );
    assert_eq!(outcomes, [EffectOutcome::Acknowledged]);
    assert!(
        create_batches[0]
            .iter()
            .chain([&create_batches[1][1]])
            .all(|effect| interrupted_create.change(&effect.id).pull_request.is_some())
    );
    assert!(
        create_batches[1][..1]
            .iter()
            .chain(create_batches[2].iter())
            .all(|effect| interrupted_create.change(&effect.id).pull_request.is_none())
    );
    assert!(matches!(interrupted_create.plan(&intent, &exact).unwrap(), Stage::Creates(_)));

    let mut created = published;
    apply_acknowledged(&mut created, &Stage::Creates(create_batches));
    let markers = created.plan(&intent, &exact).unwrap();
    apply_acknowledged(&mut created, &markers);
    let update_batches = update_batches(created.plan(&intent, &exact).unwrap());
    let update_shape = update_batches.iter().map(|batch| batch.len()).collect::<Vec<_>>();
    assert_eq!(update_shape, [2, 2, 1], "use production request-size boundaries");
    let future_before = update_batches[2]
        .iter()
        .map(|effect| {
            let target = ExternalEffect::Update(effect.clone())
                .target_id(&created)
                .expect("a prepared update has one exact-local target");
            (target.clone(), created.change(&target).clone())
        })
        .collect::<Vec<_>>();
    let outcomes = apply_interrupted_graphql_attempt(
        &mut created,
        &update_batches,
        1,
        &[1],
        ExternalEffect::Update,
    );
    assert_eq!(outcomes, [EffectOutcome::Acknowledged]);
    for (target, before) in future_before {
        assert_eq!(created.change(&target), &before, "the later request was not sent");
    }
    assert!(matches!(created.plan(&intent, &exact).unwrap(), Stage::Updates(_)));
}

#[test]
fn reorder_and_amend_keep_an_older_marker_while_converging_projection() {
    let default = oid(1);
    let new_root = LiteralRevision { head: oid(10), first_parent: default };
    let old_second = LiteralRevision { head: oid(20), first_parent: default };
    let amended_second = LiteralRevision { head: oid(21), first_parent: new_root.head };
    let intent = intent(vec![
        local_change("G0", new_root, "New root", "Body for G0"),
        local_change("G1", amended_second, "Second amended", "Body for G1"),
    ]);
    let mut world = DurableWorld::for_intents(default, &[&intent]);
    world.publish_for_setup(&id("G1"), old_second);
    world.open_for_setup(&id("G1"), BaseKind::Default, "Old second", "old final body");
    world.mark_for_setup(&id("G1"), old_second.head);

    let tuple_stage = world.plan(&intent, &Visibility::default()).unwrap();
    assert_eq!(
        tuples(tuple_stage.clone()).iter().map(|effect| effect.version).collect::<Vec<_>>(),
        [1, 2]
    );
    apply_acknowledged(&mut world, &tuple_stage);
    assert_eq!(world.change(&id("G1")).marker, Some(old_second.head));

    let create_stage = world.plan(&intent, &Visibility::default()).unwrap();
    let create_effects = creates(create_stage.clone());
    assert_eq!(
        create_effects.iter().map(|effect| effect.id.clone()).collect::<Vec<_>>(),
        [id("G0")]
    );
    apply_acknowledged(&mut world, &create_stage);
    let marker_stage = world.plan(&intent, &Visibility::default()).unwrap();
    let marker_effects = markers(marker_stage.clone());
    assert_eq!(marker_effects, [MarkerEffect { id: id("G0"), target: new_root.head }].into());
    apply_acknowledged(&mut world, &marker_stage);

    let update_effects = updates(world.plan(&intent, &Visibility::default()).unwrap());
    assert_eq!(update_effects.len(), 2);
    assert_eq!(update_effects[0].base_branch.as_deref(), Some(DEFAULT_BRANCH));
    assert_eq!(update_effects[1].base_branch.as_deref(), Some("gherrit-bases/G1"));
    assert_eq!(
        world.apply_effect(&ExternalEffect::Update(update_effects[0].clone())),
        EffectOutcome::Acknowledged
    );
    let remaining_stage = world.plan(&intent, &Visibility::default()).unwrap();
    let remaining = updates(remaining_stage.clone());
    assert_eq!(remaining, [update_effects[1].clone()].into());
    apply_acknowledged(&mut world, &remaining_stage);
    assert_done(&world, &intent, "reorder/done");
    assert_eq!(world.change(&id("G1")).marker, Some(old_second.head));
}

#[test]
fn disjoint_tuple_interleavings_commute_and_never_touch_nonlocal_state() {
    let default = oid(1);
    let a_revision = LiteralRevision { head: oid(10), first_parent: default };
    let b_revision = LiteralRevision { head: oid(20), first_parent: default };
    let a_intent = root_intent("GA", a_revision, "A");
    let b_intent = root_intent("GB", b_revision, "B");
    let initial = DurableWorld::for_intents(default, &[&a_intent, &b_intent]);
    let a = one_sent_batch_effects(&initial.plan(&a_intent, &Visibility::default()).unwrap());
    let b = one_sent_batch_effects(&initial.plan(&b_intent, &Visibility::default()).unwrap());
    assert!(matches!(a.as_slice(), [ExternalEffect::Tuple(_)]));
    assert!(matches!(b.as_slice(), [ExternalEffect::Tuple(_)]));

    let mut completed = Vec::new();
    for schedule in enumerate_interleavings(a.len(), b.len()) {
        let result = run_interleaving(&initial, &a, &b, &schedule);
        assert!(result.applied.iter().all(|effect| effect.outcome == EffectOutcome::Acknowledged));
        assert_eq!(result.world.change(&id("GA")).published, [a_revision]);
        assert_eq!(result.world.change(&id("GB")).published, [b_revision]);

        let b_before = result.world.change(&id("GB")).clone();
        let after_a = converge(result.world.clone(), &a_intent, "disjoint/stabilize-a");
        assert_eq!(
            after_a.change(&id("GB")),
            &b_before,
            "publisher A cannot inspect or mutate B's nonlocal change"
        );
        let a_before = result.world.change(&id("GA")).clone();
        let after_b = converge(result.world.clone(), &b_intent, "disjoint/stabilize-b");
        assert_eq!(
            after_b.change(&id("GA")),
            &a_before,
            "publisher B cannot inspect or mutate A's nonlocal change"
        );
        completed.push(result.world);
    }
    assert!(completed.windows(2).all(|worlds| worlds[0] == worlds[1]));
}

#[test]
fn identical_tuple_publishers_acknowledge_an_update_and_an_already_desired_no_op() {
    let default = oid(1);
    let v1 = LiteralRevision { head: oid(10), first_parent: default };
    let v2 = LiteralRevision { head: oid(11), first_parent: default };
    let local = root_intent("Gsame", v2, "Same revision");
    let mut initial = DurableWorld::for_intents(default, &[&local]);
    initial.publish_for_setup(&id("Gsame"), v1);
    let a = one_sent_batch_effects(&initial.plan(&local, &Visibility::default()).unwrap());
    let b = one_sent_batch_effects(&initial.plan(&local, &Visibility::default()).unwrap());
    assert_eq!(a.len(), 1);
    assert_eq!(b.len(), 1);

    for schedule in enumerate_interleavings(1, 1) {
        let result = run_interleaving(&initial, &a, &b, &schedule);
        assert_eq!(
            result.applied.iter().map(|effect| effect.outcome).collect::<Vec<_>>(),
            [EffectOutcome::Acknowledged, EffectOutcome::Acknowledged]
        );
        assert_eq!(result.world.change(&id("Gsame")).published, [v1, v2]);
        assert!(
            !matches!(result.world.plan(&local, &Visibility::default()).unwrap(), Stage::Tuples(_)),
            "a fresh retry observes the already-desired immutable tuple"
        );
        let converged = converge(result.world, &local, "identical-tuple/stabilize");
        assert_done(&converged, &local, "identical-tuple/done");
    }
}

#[test]
fn conflicting_tuple_publishers_advance_the_losers_intent_to_the_next_version() {
    let default = oid(1);
    let v1 = LiteralRevision { head: oid(10), first_parent: default };
    let a_revision = LiteralRevision { head: oid(11), first_parent: default };
    let b_revision = LiteralRevision { head: oid(12), first_parent: default };
    let a_intent = root_intent("Gconflict", a_revision, "A revision");
    let b_intent = root_intent("Gconflict", b_revision, "B revision");
    let mut initial = DurableWorld::for_intents(default, &[&a_intent, &b_intent]);
    initial.publish_for_setup(&id("Gconflict"), v1);
    let a = one_sent_batch_effects(&initial.plan(&a_intent, &Visibility::default()).unwrap());
    let b = one_sent_batch_effects(&initial.plan(&b_intent, &Visibility::default()).unwrap());

    for schedule in enumerate_interleavings(1, 1) {
        let result = run_interleaving(&initial, &a, &b, &schedule);
        assert_eq!(
            result.applied.iter().map(|effect| effect.outcome).collect::<Vec<_>>(),
            [EffectOutcome::Acknowledged, EffectOutcome::Rejected]
        );
        let (winner, loser, loser_intent) = match schedule[0] {
            Publisher::A => (a_revision, b_revision, &b_intent),
            Publisher::B => (b_revision, a_revision, &a_intent),
        };
        assert_eq!(result.world.change(&id("Gconflict")).published, [v1, winner]);
        let retry = tuples(result.world.plan(loser_intent, &Visibility::default()).unwrap());
        assert_eq!(retry.len(), 1);
        assert_eq!(retry[0].previous, Some(winner.effect()));
        assert_eq!(retry[0].desired, loser.effect());
        assert_eq!(retry[0].version, 3);

        let converged = converge(result.world, loser_intent, "conflicting-tuple/stabilize-loser");
        assert_eq!(converged.change(&id("Gconflict")).published, [v1, winner, loser]);
        assert_done(&converged, loser_intent, "conflicting-tuple/done");
    }
}

#[test]
fn simultaneous_stable_key_creates_leave_one_open_pull_request() {
    let default = oid(1);
    let revision = LiteralRevision { head: oid(10), first_parent: default };
    let local = root_intent("Gcreate", revision, "Create");
    let mut initial = DurableWorld::for_intents(default, &[&local]);
    initial.publish_for_setup(&id("Gcreate"), revision);
    let a = one_sent_batch_effects(&initial.plan(&local, &Visibility::default()).unwrap());
    let b = one_sent_batch_effects(&initial.plan(&local, &Visibility::default()).unwrap());
    assert!(matches!(a.as_slice(), [ExternalEffect::Create(_)]));
    assert!(matches!(b.as_slice(), [ExternalEffect::Create(_)]));

    for schedule in enumerate_interleavings(1, 1) {
        let result = run_interleaving(&initial, &a, &b, &schedule);
        assert_eq!(
            result.applied.iter().map(|effect| effect.outcome).collect::<Vec<_>>(),
            [EffectOutcome::Acknowledged, EffectOutcome::Rejected]
        );
        let created = result.world.change(&id("Gcreate")).pull_request.as_ref().unwrap();
        assert_eq!(created.base, BaseKind::Owned);
        assert!(
            !matches!(
                result.world.plan(&local, &Visibility::default()).unwrap(),
                Stage::Creates(_)
            ),
            "a fresh exact observation identifies the one durable OPEN row"
        );
        let converged = converge(result.world, &local, "same-key-create/stabilize");
        assert_done(&converged, &local, "same-key-create/done");
    }
}

#[test]
fn a_multi_alias_create_recovers_after_a_concurrent_tuple_and_stopped_request() {
    let old_intent = graphql_batch_intent(10);
    let mut new_intent = old_intent.clone();
    let mut first_parent = new_intent.changes[0].desired.head;
    for (index, change) in new_intent.changes.iter_mut().enumerate().skip(1) {
        let head = indexed_oid(30 + index);
        change.desired = LiteralRevision { head, first_parent };
        change.title = format!("Amended change {index}");
        first_parent = head;
    }
    let mut world = DurableWorld::for_intents(indexed_oid(1), &[&old_intent, &new_intent]);
    let exact = Visibility::default();
    let old_tuples = world.plan(&old_intent, &exact).unwrap();
    apply_acknowledged(&mut world, &old_tuples);
    let create_batches = create_batches(world.plan(&old_intent, &exact).unwrap());
    assert_eq!(create_batches.iter().map(|batch| batch.len()).collect::<Vec<_>>(), [2, 2, 1]);

    let concurrent_tuples = world.plan(&new_intent, &exact).unwrap();
    assert_eq!(flatten_batches(&tuple_batches(concurrent_tuples.clone())).len(), 4);
    apply_acknowledged(&mut world, &concurrent_tuples);
    let outcomes = apply_interrupted_graphql_attempt(
        &mut world,
        &create_batches,
        0,
        &[0, 1],
        ExternalEffect::Create,
    );
    assert_eq!(
        outcomes,
        [EffectOutcome::Acknowledged, EffectOutcome::AppliedButIndeterminate],
        "the moved second tuple invalidates only its create receipt"
    );
    assert!(create_batches[0].iter().all(|effect| world.change(&effect.id).pull_request.is_some()));
    assert!(
        create_batches[1..]
            .iter()
            .flatten()
            .all(|effect| world.change(&effect.id).pull_request.is_none())
    );
    assert!(world.changes.iter().all(|change| change.marker.is_none()));
    assert!(matches!(world.plan(&new_intent, &exact).unwrap(), Stage::Creates(_)));

    let converged = converge(world, &new_intent, "multi-create-after-tuple/stabilize");
    assert_eq!(
        converged.change(&id("G1")).published,
        [old_intent.changes[1].desired, new_intent.changes[1].desired]
    );
    assert_done(&converged, &new_intent, "multi-create-after-tuple/done");
}

#[test]
fn identical_marker_publishers_acknowledge_a_create_and_an_already_desired_no_op() {
    let default = oid(1);
    let revision = LiteralRevision { head: oid(10), first_parent: default };
    let local = root_intent("Gmarker", revision, "Marker");
    let mut initial = DurableWorld::for_intents(default, &[&local]);
    initial.publish_for_setup(&id("Gmarker"), revision);
    initial.open_for_setup(&id("Gmarker"), BaseKind::Owned, "Marker", "provisional");
    let a = one_sent_batch_effects(&initial.plan(&local, &Visibility::default()).unwrap());
    let b = one_sent_batch_effects(&initial.plan(&local, &Visibility::default()).unwrap());
    assert!(matches!(a.as_slice(), [ExternalEffect::Marker(_)]));
    assert!(matches!(b.as_slice(), [ExternalEffect::Marker(_)]));

    for schedule in enumerate_interleavings(1, 1) {
        let result = run_interleaving(&initial, &a, &b, &schedule);
        assert_eq!(
            result.applied.iter().map(|effect| effect.outcome).collect::<Vec<_>>(),
            [EffectOutcome::Acknowledged, EffectOutcome::Acknowledged]
        );
        assert_eq!(result.world.change(&id("Gmarker")).marker, Some(revision.head));
        assert!(
            !matches!(
                result.world.plan(&local, &Visibility::default()).unwrap(),
                Stage::Markers(_)
            ),
            "a fresh retry observes the already-created immutable marker"
        );
        let converged = converge(result.world, &local, "marker-race/stabilize");
        assert_done(&converged, &local, "marker-race/done");
    }
}

#[test]
fn divergent_children_make_reachable_last_writer_wins_navigation_bodies() {
    let default = oid(1);
    let root = LiteralRevision { head: oid(10), first_parent: default };
    let a_child = LiteralRevision { head: oid(11), first_parent: root.head };
    let b_child = LiteralRevision { head: oid(12), first_parent: root.head };
    let root_change = local_change("Groot", root, "Shared root", "Root body");
    let a_intent =
        intent(vec![root_change.clone(), local_change("GA", a_child, "A child", "A body")]);
    let b_intent = intent(vec![root_change, local_change("GB", b_child, "B child", "B body")]);
    let mut initial = DurableWorld::for_intents(default, &[&a_intent, &b_intent]);
    for (name, revision, base, title) in [
        ("Groot", root, BaseKind::Default, "Shared root"),
        ("GA", a_child, BaseKind::Owned, "A child"),
        ("GB", b_child, BaseKind::Owned, "B child"),
    ] {
        let change_id = id(name);
        initial.publish_for_setup(&change_id, revision);
        initial.open_for_setup(&change_id, base, title, "stale body");
        initial.mark_for_setup(&change_id, revision.head);
    }

    let a_stage = initial.plan(&a_intent, &Visibility::default()).unwrap();
    let b_stage = initial.plan(&b_intent, &Visibility::default()).unwrap();
    let a_updates = updates(a_stage.clone());
    let b_updates = updates(b_stage.clone());
    assert_eq!(a_updates.len(), 2);
    assert_eq!(b_updates.len(), 2);
    let root_identity = identity(initial.position(&id("Groot")));
    let a_root_body = a_updates
        .iter()
        .find(|effect| effect.identity == root_identity)
        .and_then(|effect| effect.body.clone())
        .unwrap();
    let b_root_body = b_updates
        .iter()
        .find(|effect| effect.identity == root_identity)
        .and_then(|effect| effect.body.clone())
        .unwrap();
    assert_ne!(
        a_root_body, b_root_body,
        "two reachable divergent children must produce different root navigation"
    );

    let a = one_sent_batch_effects(&a_stage);
    let b = one_sent_batch_effects(&b_stage);
    assert!(a.iter().all(|effect| matches!(effect, ExternalEffect::Update(_))));
    assert!(b.iter().all(|effect| matches!(effect, ExternalEffect::Update(_))));
    for schedule in enumerate_interleavings(a.len(), b.len()) {
        let result = run_interleaving(&initial, &a, &b, &schedule);
        assert!(result.applied.iter().all(|effect| effect.outcome == EffectOutcome::Acknowledged));
        let last_root_writer = result
            .applied
            .iter()
            .rfind(|applied| {
                let effects = match applied.publisher {
                    Publisher::A => &a,
                    Publisher::B => &b,
                };
                effects[applied.index].target_id(&initial).as_ref() == Some(&id("Groot"))
            })
            .unwrap()
            .publisher;
        let expected_root_body = match last_root_writer {
            Publisher::A => &a_root_body,
            Publisher::B => &b_root_body,
        };
        assert_eq!(
            &result.world.change(&id("Groot")).pull_request.as_ref().unwrap().body,
            expected_root_body,
            "the final root body must be the last complete update alias"
        );

        let b_nonlocal = result.world.change(&id("GB")).clone();
        let stabilized_a = converge(result.world.clone(), &a_intent, "navigation/stabilize-a");
        assert_eq!(stabilized_a.change(&id("GB")), &b_nonlocal);
        assert_eq!(
            stabilized_a.change(&id("Groot")).pull_request.as_ref().unwrap().body,
            a_root_body
        );
        assert_done(&stabilized_a, &a_intent, "navigation/a-done");

        let a_nonlocal = result.world.change(&id("GA")).clone();
        let stabilized_b = converge(result.world, &b_intent, "navigation/stabilize-b");
        assert_eq!(stabilized_b.change(&id("GA")), &a_nonlocal);
        assert_eq!(
            stabilized_b.change(&id("Groot")).pull_request.as_ref().unwrap().body,
            b_root_body
        );
        assert_done(&stabilized_b, &b_intent, "navigation/b-done");
    }
}

#[test]
fn a_precomputed_safe_projection_may_land_after_a_conflicting_tuple() {
    let default = oid(1);
    let v1 = LiteralRevision { head: oid(10), first_parent: default };
    let v2 = LiteralRevision { head: oid(11), first_parent: default };
    let old_intent = root_intent("Gstale", v1, "Old projection");
    let new_intent = root_intent("Gstale", v2, "New projection");
    let mut initial = DurableWorld::for_intents(default, &[&old_intent, &new_intent]);
    initial.publish_for_setup(&id("Gstale"), v1);
    initial.open_for_setup(&id("Gstale"), BaseKind::Default, "stale", "stale");
    initial.mark_for_setup(&id("Gstale"), v1.head);

    let stale_updates =
        one_sent_batch_effects(&initial.plan(&old_intent, &Visibility::default()).unwrap());
    let new_tuple =
        one_sent_batch_effects(&initial.plan(&new_intent, &Visibility::default()).unwrap());
    assert!(stale_updates.iter().all(|effect| matches!(effect, ExternalEffect::Update(_))));
    assert!(matches!(new_tuple.as_slice(), [ExternalEffect::Tuple(_)]));

    let mut world = initial;
    assert_eq!(world.apply_effect(&new_tuple[0]), EffectOutcome::Acknowledged);
    for update in &stale_updates {
        assert_eq!(world.apply_effect(update), EffectOutcome::Acknowledged);
    }
    assert_eq!(world.change(&id("Gstale")).published, [v1, v2]);
    assert_eq!(world.change(&id("Gstale")).marker, Some(v1.head));
    assert_eq!(world.change(&id("Gstale")).pull_request.as_ref().unwrap().title, "Old projection");

    let converged = converge(world, &new_intent, "stale-projection/stabilize-new-intent");
    assert_eq!(converged.change(&id("Gstale")).published, [v1, v2]);
    assert_eq!(
        converged.change(&id("Gstale")).pull_request.as_ref().unwrap().title,
        "New projection"
    );
    assert_done(&converged, &new_intent, "stale-projection/done");
}

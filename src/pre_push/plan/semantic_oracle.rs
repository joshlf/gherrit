//! Restart checks over literal durable state and typed planner effects.
//!
//! This model deliberately knows nothing about refspecs, `ls-remote`, GraphQL,
//! aliases, JSON, or HTTP. Focused adapter tests own those encodings. Here a
//! planner effect is applied to durable state, all process-local authority is
//! discarded, and a fresh plan must describe exactly the remaining work.

use std::collections::{HashMap, HashSet};

use gix::ObjectId;

use super::plan_publication;
use crate::pre_push::{
    body::BodyLinkContext,
    destination::{DefaultBranch, PushDestination},
    github::{CorrelatedRepository, RepositoryTerminalHistories},
    history::CommitGraphEvidence,
    local::{GherritPrId, LocalStack},
    pull_request::{
        BaseKind, LocalPullRequestObservation, ManagedOpenPullRequest, PullRequestIdentity,
        TerminalHistories,
    },
    remote::{ActiveRemoteChanges, ObservedChangeHistory},
    test_effect::{
        CreateEffect, EffectBatches, MarkerEffect, RevisionEffect, Stage, TupleEffect, UpdateEffect,
    },
};

const DEFAULT_BRANCH: &str = "main";
const REPOSITORY_ID: &str = "R_recovery";

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct LiteralRevision {
    head: ObjectId,
    first_parent: ObjectId,
}

impl LiteralRevision {
    fn effect(self) -> RevisionEffect {
        RevisionEffect { head: self.head, first_parent: self.first_parent }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum Body {
    /// The bytes are valid observed content but not the current final body.
    Stale(String),
    /// These exact bytes came from applying the planner's last body patch.
    AppliedDesired(String),
}

impl Body {
    fn as_str(&self) -> &str {
        match self {
            Self::Stale(body) | Self::AppliedDesired(body) => body,
        }
    }

    fn needs_update(&self) -> bool {
        matches!(self, Self::Stale(_))
    }
}

#[derive(Clone, Debug)]
struct PullRequest {
    identity: PullRequestIdentity,
    base: BaseKind,
    title: String,
    body: Body,
}

#[derive(Clone, Debug)]
struct Change {
    id: GherritPrId,
    desired: LiteralRevision,
    title: String,
    commit_body: String,
    published: Vec<LiteralRevision>,
    // Marker and projection are deliberately independent. In particular, a
    // marker may target an older version while an amended PR remains stale.
    marker: Option<ObjectId>,
    pull_request: Option<PullRequest>,
}

#[derive(Clone, Debug)]
struct World {
    default_tip: ObjectId,
    changes: Vec<Change>,
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

#[derive(Clone, Debug, Eq, PartialEq)]
struct CreateExpectation {
    id: GherritPrId,
    repository_id: String,
    base_branch: String,
    title: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct UpdateExpectation {
    identity: PullRequestIdentity,
    title: Option<String>,
    body: bool,
    base_branch: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum Expected {
    RejectMarkedAbsence(GherritPrId),
    Tuples(Box<[TupleEffect]>),
    Creates(Box<[CreateExpectation]>),
    Markers(Box<[MarkerEffect]>),
    Updates(Box<[UpdateExpectation]>),
    Done,
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

impl World {
    fn expected(&self, visibility: &Visibility) -> Expected {
        if let Some(change) = self
            .changes
            .iter()
            .find(|change| change.marker.is_some() && self.open_is_absent(change, visibility))
        {
            return Expected::RejectMarkedAbsence(change.id.clone());
        }

        let tuples = self
            .changes
            .iter()
            .filter(|change| change.published.last().copied() != Some(change.desired))
            .map(|change| TupleEffect {
                id: change.id.clone(),
                previous: change.published.last().copied().map(LiteralRevision::effect),
                desired: change.desired.effect(),
                version: u64::try_from(change.published.len()).unwrap() + 1,
            })
            .collect::<Box<[_]>>();
        if !tuples.is_empty() {
            return Expected::Tuples(tuples);
        }

        let creates = self
            .changes
            .iter()
            .filter(|change| self.open_is_absent(change, visibility))
            .map(|change| CreateExpectation {
                id: change.id.clone(),
                repository_id: REPOSITORY_ID.to_owned(),
                base_branch: owned_base_name(&change.id),
                title: change.title.clone(),
            })
            .collect::<Box<[_]>>();
        if !creates.is_empty() {
            return Expected::Creates(creates);
        }

        let markers = self
            .changes
            .iter()
            .filter(|change| change.marker.is_none())
            .map(|change| MarkerEffect { id: change.id.clone(), target: change.desired.head })
            .collect::<Box<[_]>>();
        if !markers.is_empty() {
            return Expected::Markers(markers);
        }

        let updates = self
            .changes
            .iter()
            .enumerate()
            .filter_map(|(index, change)| {
                let pull_request = change.pull_request.as_ref().unwrap();
                let desired_base = if index == 0 { BaseKind::Default } else { BaseKind::Owned };
                let expectation = UpdateExpectation {
                    identity: pull_request.identity.clone(),
                    title: (pull_request.title != change.title).then(|| change.title.clone()),
                    body: pull_request.body.needs_update(),
                    base_branch: (pull_request.base != desired_base).then(|| {
                        if index == 0 {
                            DEFAULT_BRANCH.to_owned()
                        } else {
                            owned_base_name(&change.id)
                        }
                    }),
                };
                (expectation.title.is_some()
                    || expectation.body
                    || expectation.base_branch.is_some())
                .then_some(expectation)
            })
            .collect::<Box<[_]>>();
        if updates.is_empty() { Expected::Done } else { Expected::Updates(updates) }
    }

    fn open_is_absent(&self, change: &Change, visibility: &Visibility) -> bool {
        change.pull_request.is_none() || visibility.hides(&change.id)
    }

    fn plan(&self, visibility: &Visibility) -> Result<Stage, String> {
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
            self.changes.iter().map(|change| {
                (
                    change.id.clone(),
                    change.desired.head,
                    change.title.clone(),
                    change.commit_body.clone(),
                )
            }),
        )
        .unwrap();
        let remote_changes = self
            .changes
            .iter()
            .map(|change| {
                let published = change
                    .published
                    .iter()
                    .map(|revision| (revision.head, revision.first_parent))
                    .collect::<Vec<_>>();
                ObservedChangeHistory::from_typed_for_test(
                    change.id.clone(),
                    &published,
                    change.marker,
                )
                .unwrap()
            })
            .collect();
        let active = ActiveRemoteChanges::from_typed_for_test(
            &destination,
            default_branch.clone(),
            remote_changes,
        );
        let local_pull_requests = self
            .changes
            .iter()
            .map(|change| match &change.pull_request {
                Some(pull_request) if !visibility.hides(&change.id) => {
                    let current = change
                        .published
                        .last()
                        .expect("an observed OPEN pull request has published history");
                    let base_oid = match pull_request.base {
                        BaseKind::Default => self.default_tip,
                        BaseKind::Owned => current.first_parent,
                    };
                    LocalPullRequestObservation::Open(ManagedOpenPullRequest::from_typed_for_test(
                        change.id.clone(),
                        pull_request.identity.clone(),
                        current.head,
                        pull_request.base,
                        base_oid,
                        pull_request.title.clone(),
                        pull_request.body.as_str().to_owned(),
                    ))
                }
                Some(_) | None => {
                    LocalPullRequestObservation::NeedsTerminalProof(change.id.clone())
                }
            })
            .collect::<Vec<_>>();
        let missing = local_pull_requests
            .iter()
            .filter_map(|pull_request| match pull_request {
                LocalPullRequestObservation::Open(_) => None,
                LocalPullRequestObservation::NeedsTerminalProof(id) => Some(id.clone()),
            })
            .collect::<Vec<_>>();
        let correlated = CorrelatedRepository::from_typed_for_test(
            &destination,
            REPOSITORY_ID.to_owned(),
            default_branch,
            local_pull_requests,
        )
        .unwrap();
        let terminal = RepositoryTerminalHistories::for_test(
            &destination,
            TerminalHistories::empty_for_test(missing),
        );
        let context = BodyLinkContext::from_destination(&destination, None).unwrap();
        plan_publication(context, stack, correlated, terminal, active, &self.graph())
            .map(|plan| plan.first_stage_for_test())
            .map_err(|error| error.to_string())
    }

    fn graph(&self) -> CommitGraphEvidence {
        let mut commits = HashMap::<ObjectId, (Vec<ObjectId>, Vec<GherritPrId>)>::new();
        commits.insert(self.default_tip, (Vec::new(), Vec::new()));
        for change in &self.changes {
            for revision in change.published.iter().chain([&change.desired]) {
                let value = (vec![revision.first_parent], vec![change.id.clone()]);
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

    fn apply_tuples(&mut self, effects: &[TupleEffect]) {
        for effect in effects {
            let change = self.change_mut(&effect.id);
            assert_eq!(
                effect.previous,
                change.published.last().copied().map(LiteralRevision::effect)
            );
            assert_eq!(effect.desired, change.desired.effect());
            assert_eq!(effect.version, u64::try_from(change.published.len()).unwrap() + 1);
            change.published.push(change.desired);
        }
    }

    fn apply_creates(&mut self, effects: &[CreateEffect], mask: usize) {
        self.apply_creates_where(effects, |index| mask & (1 << index) != 0);
    }

    fn apply_creates_where(
        &mut self,
        effects: &[CreateEffect],
        mut selected: impl FnMut(usize) -> bool,
    ) {
        for (index, effect) in effects.iter().enumerate() {
            if !selected(index) {
                continue;
            }
            let position = self.position(&effect.id);
            let change = &mut self.changes[position];
            assert!(change.pull_request.is_none(), "a create subset cannot recreate an OPEN row");
            change.pull_request = Some(PullRequest {
                identity: identity(position),
                base: BaseKind::Owned,
                title: effect.title.clone(),
                body: Body::Stale(effect.body.clone()),
            });
        }
    }

    fn apply_markers(&mut self, effects: &[MarkerEffect]) {
        for effect in effects {
            let change = self.change_mut(&effect.id);
            assert!(change.marker.is_none());
            assert!(change.published.iter().any(|revision| revision.head == effect.target));
            change.marker = Some(effect.target);
        }
    }

    fn apply_updates(&mut self, effects: &[UpdateEffect], mask: usize) {
        self.apply_updates_where(effects, |index| mask & (1 << index) != 0);
    }

    fn apply_updates_where(
        &mut self,
        effects: &[UpdateEffect],
        mut selected: impl FnMut(usize) -> bool,
    ) {
        for (index, effect) in effects.iter().enumerate() {
            if !selected(index) {
                continue;
            }
            let change_index = self
                .changes
                .iter()
                .position(|change| {
                    change
                        .pull_request
                        .as_ref()
                        .is_some_and(|pull_request| pull_request.identity == effect.identity)
                })
                .expect("an update identifies one modeled OPEN pull request");
            let owned_base = owned_base_name(&self.changes[change_index].id);
            let pull_request = self.changes[change_index].pull_request.as_mut().unwrap();
            if let Some(title) = &effect.title {
                pull_request.title = title.clone();
            }
            if let Some(body) = &effect.body {
                pull_request.body = Body::AppliedDesired(body.clone());
            }
            if let Some(base) = &effect.base_branch {
                pull_request.base = if base == DEFAULT_BRANCH {
                    BaseKind::Default
                } else {
                    assert_eq!(base, &owned_base);
                    BaseKind::Owned
                };
            }
        }
    }

    fn position(&self, id: &GherritPrId) -> usize {
        self.changes.iter().position(|change| change.id == *id).unwrap()
    }

    fn change_mut(&mut self, id: &GherritPrId) -> &mut Change {
        let position = self.position(id);
        &mut self.changes[position]
    }
}

fn flatten_batches<T: Clone>(batches: &[Box<[T]>]) -> Box<[T]> {
    batches.iter().flat_map(|batch| batch.iter().cloned()).collect()
}

fn assert_plan(world: &World, visibility: &Visibility, label: &str) -> Stage {
    let expected = world.expected(visibility);
    let actual = world.plan(visibility);
    match expected {
        Expected::RejectMarkedAbsence(id) => {
            let error = actual.expect_err(&format!("{label}: marked absence must reject"));
            assert_eq!(
                error,
                format!(
                    "GHerrit change '{}' has a pull-request marker but no OPEN pull request",
                    id.as_str()
                ),
                "{label}: exact fail-closed marker diagnostic"
            );
            Stage::Done
        }
        Expected::Tuples(expected) => {
            let Stage::Tuples(batches) = actual.unwrap_or_else(|error| panic!("{label}: {error}"))
            else {
                panic!("{label}: expected tuple stage")
            };
            let actual = flatten_batches(&batches);
            assert_eq!(actual, expected, "{label}: exact tuple effects");
            Stage::Tuples(batches)
        }
        Expected::Creates(expected) => {
            let Stage::Creates(batches) = actual.unwrap_or_else(|error| panic!("{label}: {error}"))
            else {
                panic!("{label}: expected create stage")
            };
            let actual = flatten_batches(&batches);
            let cores = actual
                .iter()
                .map(|effect| CreateExpectation {
                    id: effect.id.clone(),
                    repository_id: effect.repository_id.clone(),
                    base_branch: effect.base_branch.clone(),
                    title: effect.title.clone(),
                })
                .collect::<Box<[_]>>();
            assert_eq!(cores, expected, "{label}: exact create non-body fields");
            assert!(actual.iter().all(|effect| !effect.body.is_empty()));
            Stage::Creates(batches)
        }
        Expected::Markers(expected) => {
            let Stage::Markers(batches) = actual.unwrap_or_else(|error| panic!("{label}: {error}"))
            else {
                panic!("{label}: expected marker stage")
            };
            let actual = flatten_batches(&batches);
            assert_eq!(actual, expected, "{label}: exact marker identities and targets");
            Stage::Markers(batches)
        }
        Expected::Updates(expected) => {
            let Stage::Updates(batches) = actual.unwrap_or_else(|error| panic!("{label}: {error}"))
            else {
                panic!("{label}: expected update stage")
            };
            let actual = flatten_batches(&batches);
            let cores = actual
                .iter()
                .map(|effect| UpdateExpectation {
                    identity: effect.identity.clone(),
                    title: effect.title.clone(),
                    body: effect.body.is_some(),
                    base_branch: effect.base_branch.clone(),
                })
                .collect::<Box<[_]>>();
            assert_eq!(cores, expected, "{label}: exact update field presence");
            assert!(
                actual
                    .iter()
                    .filter_map(|effect| effect.body.as_ref())
                    .all(|body| !body.is_empty())
            );
            Stage::Updates(batches)
        }
        Expected::Done => {
            assert_eq!(actual.unwrap_or_else(|error| panic!("{label}: {error}")), Stage::Done);
            Stage::Done
        }
    }
}

fn tuples(stage: Stage) -> Box<[TupleEffect]> {
    flatten_batches(&tuple_batches(stage))
}

fn tuple_batches(stage: Stage) -> EffectBatches<TupleEffect> {
    let Stage::Tuples(batches) = stage else { panic!("expected tuples") };
    batches
}

fn creates(stage: Stage) -> Box<[CreateEffect]> {
    flatten_batches(&create_batches(stage))
}

fn create_batches(stage: Stage) -> EffectBatches<CreateEffect> {
    let Stage::Creates(batches) = stage else { panic!("expected creates") };
    batches
}

fn markers(stage: Stage) -> Box<[MarkerEffect]> {
    flatten_batches(&marker_batches(stage))
}

fn marker_batches(stage: Stage) -> EffectBatches<MarkerEffect> {
    let Stage::Markers(batches) = stage else { panic!("expected markers") };
    batches
}

fn updates(stage: Stage) -> Box<[UpdateEffect]> {
    flatten_batches(&update_batches(stage))
}

fn update_batches(stage: Stage) -> EffectBatches<UpdateEffect> {
    let Stage::Updates(batches) = stage else { panic!("expected updates") };
    batches
}

fn new_change(name: &str, desired: LiteralRevision, title: &str) -> Change {
    Change {
        id: id(name),
        desired,
        title: title.to_owned(),
        commit_body: format!("Body for {name}"),
        published: Vec::new(),
        marker: None,
        pull_request: None,
    }
}

fn large_fresh_world(change_count: usize) -> World {
    let default_tip = indexed_oid(1);
    let mut first_parent = default_tip;
    let changes = (0..change_count)
        .map(|index| {
            let head = indexed_oid(index + 2);
            let change = new_change(
                &format!("G{index}"),
                LiteralRevision { head, first_parent },
                &format!("Change {index}"),
            );
            first_parent = head;
            change
        })
        .collect();
    World { default_tip, changes }
}

#[test]
fn fresh_root_recovers_at_every_barrier_and_fails_closed_after_marker() {
    let root = LiteralRevision { head: oid(10), first_parent: oid(1) };
    let mut world = World { default_tip: oid(1), changes: vec![new_change("G0", root, "Root")] };
    let exact = Visibility::default();

    let tuple_effects = tuples(assert_plan(&world, &exact, "fresh/tuples"));
    world.apply_tuples(&tuple_effects);
    let create_effects = creates(assert_plan(&world, &exact, "fresh/creates"));
    insta::assert_snapshot!("fresh_create_body", create_effects[0].body);
    world.apply_creates(&create_effects, 1);

    let hidden = Visibility::hiding(&id("G0"));
    let retry = creates(assert_plan(&world, &hidden, "fresh/create-visible-late"));
    assert_eq!(retry, create_effects, "a hidden provisional PR retries the identical create");

    let marker_effects = markers(assert_plan(&world, &exact, "fresh/markers"));
    world.apply_markers(&marker_effects);
    assert_plan(&world, &hidden, "fresh/marked-visible-late");

    let update_effects = updates(assert_plan(&world, &exact, "fresh/updates"));
    insta::assert_snapshot!(
        "fresh_update_body",
        update_effects[0].body.as_deref().expect("the provisional body needs its final numbers")
    );
    assert_eq!(updates(assert_plan(&world, &exact, "fresh/lost-update-ack")), update_effects);
    world.apply_updates(&update_effects, 1);
    assert_eq!(assert_plan(&world, &exact, "fresh/done"), Stage::Done);
}

#[test]
fn two_changes_recover_from_holey_create_and_update_results() {
    let first = LiteralRevision { head: oid(10), first_parent: oid(1) };
    let second = LiteralRevision { head: oid(11), first_parent: first.head };
    let mut published = World {
        default_tip: oid(1),
        changes: vec![new_change("G0", first, "First"), new_change("G1", second, "Second")],
    };
    let exact = Visibility::default();
    let tuple_effects = tuples(assert_plan(&published, &exact, "two/tuples"));
    published.apply_tuples(&tuple_effects);
    let initial_creates = creates(assert_plan(&published, &exact, "two/creates"));
    assert_eq!(initial_creates.len(), 2);

    for create_mask in 0..4 {
        let mut world = published.clone();
        world.apply_creates(&initial_creates, create_mask);
        if create_mask != 3 {
            let remaining = creates(assert_plan(
                &world,
                &exact,
                &format!("two/create-mask-{create_mask}/restart"),
            ));
            let all = (1 << remaining.len()) - 1;
            world.apply_creates(&remaining, all);
        }

        let marker_effects =
            markers(assert_plan(&world, &exact, &format!("two/create-mask-{create_mask}/markers")));
        world.apply_markers(&marker_effects);
        let all_updates =
            updates(assert_plan(&world, &exact, &format!("two/create-mask-{create_mask}/updates")));
        assert_eq!(all_updates.len(), 2);

        for update_mask in 0..4 {
            let mut restarted = world.clone();
            restarted.apply_updates(&all_updates, update_mask);
            if update_mask != 3 {
                let remaining = updates(assert_plan(
                    &restarted,
                    &exact,
                    &format!("two/create-mask-{create_mask}/update-mask-{update_mask}/restart"),
                ));
                let all = (1 << remaining.len()) - 1;
                restarted.apply_updates(&remaining, all);
            }
            assert_eq!(
                assert_plan(
                    &restarted,
                    &exact,
                    &format!("two/create-mask-{create_mask}/update-mask-{update_mask}/done"),
                ),
                Stage::Done
            );
        }
    }
}

#[test]
fn multiple_transport_batches_recover_only_from_reachable_prefixes() {
    const CHANGE_COUNT: usize = 65;

    let unpublished = large_fresh_world(CHANGE_COUNT);
    let exact = Visibility::default();
    let initial_tuple_batches = tuple_batches(assert_plan(&unpublished, &exact, "batch/tuples"));
    assert!(initial_tuple_batches.len() > 1, "the fixture must cross the Git tuple budget");

    for prefix_len in 0..=initial_tuple_batches.len() {
        let mut restarted = unpublished.clone();
        for batch in &initial_tuple_batches[..prefix_len] {
            restarted.apply_tuples(batch);
        }
        let stage =
            assert_plan(&restarted, &exact, &format!("batch/tuple-prefix-{prefix_len}/restart"));
        if prefix_len == initial_tuple_batches.len() {
            assert!(matches!(stage, Stage::Creates(_)));
        } else {
            assert!(matches!(stage, Stage::Tuples(_)));
        }
    }

    let mut created = unpublished.clone();
    for batch in &initial_tuple_batches {
        created.apply_tuples(batch);
    }
    let initial_create_batches = create_batches(assert_plan(&created, &exact, "batch/creates"));
    assert!(initial_create_batches.len() > 1, "the fixture must cross the GraphQL alias limit");

    for prefix_len in 0..=initial_create_batches.len() {
        let mut restarted = created.clone();
        for batch in &initial_create_batches[..prefix_len] {
            restarted.apply_creates_where(batch, |_| true);
        }
        let stage =
            assert_plan(&restarted, &exact, &format!("batch/create-prefix-{prefix_len}/restart"));
        assert_eq!(matches!(stage, Stage::Markers(_)), prefix_len == initial_create_batches.len());
        assert_eq!(matches!(stage, Stage::Creates(_)), prefix_len < initial_create_batches.len());
    }
    for (current_batch, batch) in initial_create_batches.iter().enumerate() {
        if batch.len() < 2 {
            continue;
        }
        let mut restarted = created.clone();
        for complete in &initial_create_batches[..current_batch] {
            restarted.apply_creates_where(complete, |_| true);
        }
        // A GraphQL failure can follow complete earlier requests and a holey
        // subset of the aliases in the current request, but no later one.
        restarted.apply_creates_where(batch, |index| index.is_multiple_of(2));
        assert!(matches!(
            assert_plan(&restarted, &exact, &format!("batch/create-{current_batch}-holey/restart"),),
            Stage::Creates(_)
        ));
    }

    for batch in &initial_create_batches {
        created.apply_creates_where(batch, |_| true);
    }
    let mut projected = created;
    let marker_effects = markers(assert_plan(&projected, &exact, "batch/markers"));
    for effect in &marker_effects {
        projected.apply_markers(std::slice::from_ref(effect));
    }
    let initial_update_batches = update_batches(assert_plan(&projected, &exact, "batch/updates"));
    assert!(initial_update_batches.len() > 1, "the fixture must cross the GraphQL alias limit");

    for prefix_len in 0..=initial_update_batches.len() {
        let mut restarted = projected.clone();
        for batch in &initial_update_batches[..prefix_len] {
            restarted.apply_updates_where(batch, |_| true);
        }
        let stage =
            assert_plan(&restarted, &exact, &format!("batch/update-prefix-{prefix_len}/restart"));
        assert_eq!(stage == Stage::Done, prefix_len == initial_update_batches.len());
        assert_eq!(matches!(stage, Stage::Updates(_)), prefix_len < initial_update_batches.len());
    }
    for (current_batch, batch) in initial_update_batches.iter().enumerate() {
        if batch.len() < 2 {
            continue;
        }
        let mut restarted = projected.clone();
        for complete in &initial_update_batches[..current_batch] {
            restarted.apply_updates_where(complete, |_| true);
        }
        restarted.apply_updates_where(batch, |index| index.is_multiple_of(2));
        assert!(matches!(
            assert_plan(&restarted, &exact, &format!("batch/update-{current_batch}-holey/restart"),),
            Stage::Updates(_)
        ));
    }
}

#[test]
fn marker_pushes_recover_from_every_atomic_batch_prefix() {
    let default_tip = indexed_oid(1_000);
    let long_suffix = "a".repeat(4_500);
    let first = LiteralRevision { head: indexed_oid(1_001), first_parent: default_tip };
    let second = LiteralRevision { head: indexed_oid(1_002), first_parent: first.head };
    let mut changes = [
        new_change(&format!("G{long_suffix}"), first, "First"),
        new_change(&format!("H{long_suffix}"), second, "Second"),
    ];
    for (index, change) in changes.iter_mut().enumerate() {
        change.published.push(change.desired);
        change.pull_request = Some(PullRequest {
            identity: identity(index),
            base: BaseKind::Owned,
            title: change.title.clone(),
            body: Body::Stale("stale".to_owned()),
        });
    }
    let world = World { default_tip, changes: changes.into() };
    let exact = Visibility::default();
    let initial_marker_batches =
        marker_batches(assert_plan(&world, &exact, "marker-batches/initial"));
    assert!(initial_marker_batches.len() > 1, "the fixture must cross the Git marker budget");

    // Each push is atomic, so the only durable states between sequential
    // requests are complete request prefixes. Prefix N+1 also represents an
    // applied request whose acknowledgement was lost.
    for prefix_len in 0..=initial_marker_batches.len() {
        let mut restarted = world.clone();
        for batch in &initial_marker_batches[..prefix_len] {
            restarted.apply_markers(batch);
        }
        let stage =
            assert_plan(&restarted, &exact, &format!("marker-batches/prefix-{prefix_len}/restart"));
        if prefix_len == initial_marker_batches.len() {
            assert!(matches!(stage, Stage::Updates(_)));
        } else {
            assert!(matches!(stage, Stage::Markers(_)));
        }
    }
}

#[test]
fn reorder_and_amend_keep_an_older_marker_while_converging_projection() {
    let default = oid(1);
    let new_root = LiteralRevision { head: oid(10), first_parent: default };
    let old_second = LiteralRevision { head: oid(20), first_parent: default };
    let amended_second = LiteralRevision { head: oid(21), first_parent: new_root.head };
    let mut second = new_change("G1", amended_second, "Second amended");
    second.published.push(old_second);
    second.marker = Some(old_second.head);
    second.pull_request = Some(PullRequest {
        identity: identity(1),
        base: BaseKind::Default,
        title: "Old second".to_owned(),
        body: Body::Stale("old final body".to_owned()),
    });
    let mut world = World {
        default_tip: default,
        changes: vec![new_change("G0", new_root, "New root"), second],
    };
    let exact = Visibility::default();

    let tuple_effects = tuples(assert_plan(&world, &exact, "reorder/tuples"));
    assert_eq!(tuple_effects.iter().map(|effect| effect.version).collect::<Vec<_>>(), [1, 2]);
    world.apply_tuples(&tuple_effects);
    assert_eq!(world.changes[1].marker, Some(old_second.head));

    let create_effects = creates(assert_plan(&world, &exact, "reorder/create-new-root"));
    assert_eq!(
        create_effects.iter().map(|effect| effect.id.clone()).collect::<Vec<_>>(),
        [id("G0")]
    );
    world.apply_creates(&create_effects, 1);
    let marker_effects = markers(assert_plan(&world, &exact, "reorder/marker-new-root"));
    assert_eq!(marker_effects, [MarkerEffect { id: id("G0"), target: new_root.head }].into());
    world.apply_markers(&marker_effects);

    let update_effects = updates(assert_plan(&world, &exact, "reorder/updates"));
    assert_eq!(update_effects.len(), 2);
    assert_eq!(update_effects[0].base_branch.as_deref(), Some(DEFAULT_BRANCH));
    assert_eq!(update_effects[1].base_branch.as_deref(), Some("gherrit-bases/G1"));
    world.apply_updates(&update_effects, 1);
    let remaining = updates(assert_plan(&world, &exact, "reorder/update-prefix-restart"));
    assert_eq!(
        remaining.iter().map(|effect| effect.identity.clone()).collect::<Vec<_>>(),
        [identity(1)]
    );
    world.apply_updates(&remaining, 1);
    assert_eq!(assert_plan(&world, &exact, "reorder/done"), Stage::Done);
    assert_eq!(world.changes[1].marker, Some(old_second.head));
}

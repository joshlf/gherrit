//! Independent finite-state oracle for owned-base crash and retry behavior.
//!
//! The oracle reduces body contents to final, provisional, and stale
//! representatives. Focused planner tests own exact body comparison and its
//! CRLF-only equivalence.
//!
//! The expected model deliberately does not call production normalization,
//! graph, rendering, batching, request, action, or planning helpers. A
//! separate adapter turns the model's literal evidence into the same raw
//! observations consumed by production and summarizes only prepared actions.

use std::{
    collections::{BTreeMap, BTreeSet},
    fmt::Write as _,
    time::Instant,
};

use gix::ObjectId;
use rayon::prelude::*;
use serde_json::{Value, json};

use super::{FinalProjection, PublicationPlan, ReadyProjection, plan_publication};
use crate::pre_push::{
    body::BodyLinkContext,
    destination::PushDestination,
    github::{
        OpenObservation, RepositoryTerminalHistories, TerminalPullRequest,
        TerminalPullRequestEvidence, TerminalPullRequestPage, TerminalPullRequestState,
    },
    history::CommitGraphEvidence,
    local::{GherritPrId, LocalStack},
    pull_request::{PullRequestIdentity, TerminalExhaustionAccumulator},
    remote::parse_remote_heads_for_destination_for_test,
};

const REPOSITORY_ID: &str = "R_repository";
const DEFAULT_BRANCH: &str = "main";
const FIXED_PUSH_OPTIONS: [&str; 7] = [
    "--porcelain",
    "--atomic",
    "--no-verify",
    "--no-follow-tags",
    "--recurse-submodules=no",
    "--no-signed",
    "--no-force-if-includes",
];

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
struct Commit(u32);

impl Commit {
    fn oid(self) -> ObjectId {
        let mut bytes = [0_u8; 20];
        bytes[0] = 0x47;
        bytes[1..5].copy_from_slice(&self.0.to_be_bytes());
        ObjectId::from_bytes_or_panic(&bytes)
    }

    fn from_oid(oid: ObjectId) -> Self {
        let bytes = oid.as_bytes();
        assert_eq!(bytes.len(), 20, "the oracle catalogue uses SHA-1");
        assert_eq!(bytes[0], 0x47, "object ID is outside the oracle catalogue");
        assert!(bytes[5..].iter().all(|byte| *byte == 0));
        Self(u32::from_be_bytes(bytes[1..5].try_into().unwrap()))
    }
}

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
struct Id(String);

impl Id {
    fn new(value: impl Into<String>) -> Self {
        let value = value.into();
        assert!(!value.is_empty() && value.bytes().all(|byte| byte.is_ascii_alphanumeric()));
        Self(value)
    }

    fn as_str(&self) -> &str {
        &self.0
    }

    fn production(&self) -> GherritPrId {
        GherritPrId::from_ref_component(self.0.as_bytes()).unwrap()
    }
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
struct Revision {
    head: Commit,
    first_parent: Commit,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct GraphNode {
    parents: Vec<Commit>,
    identities: Vec<Id>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct Graph {
    nodes: BTreeMap<Commit, GraphNode>,
}

impl Graph {
    fn new(default_tip: Commit) -> Self {
        let mut graph = Self { nodes: BTreeMap::new() };
        graph.insert(default_tip, [], []);
        graph
    }

    fn insert(
        &mut self,
        commit: Commit,
        parents: impl IntoIterator<Item = Commit>,
        identities: impl IntoIterator<Item = Id>,
    ) {
        let previous = self.nodes.insert(
            commit,
            GraphNode {
                parents: parents.into_iter().collect(),
                identities: identities.into_iter().collect(),
            },
        );
        assert!(previous.is_none(), "literal commit labels are unique");
    }

    fn first_parent(&self, commit: Commit) -> Option<Commit> {
        self.nodes.get(&commit).and_then(|node| node.parents.first()).copied()
    }

    /// Returns whether `ancestor` is reachable while walking from
    /// `descendant` through every literal parent edge.
    fn reaches(&self, descendant: Commit, ancestor: Commit) -> bool {
        let mut pending = vec![descendant];
        let mut seen = BTreeSet::new();
        while let Some(commit) = pending.pop() {
            if commit == ancestor {
                return true;
            }
            if seen.insert(commit) {
                pending.extend(self.nodes[&commit].parents.iter().copied());
            }
        }
        false
    }

    fn identity_count(&self, descendant: Commit, id: &Id) -> usize {
        let mut pending = vec![descendant];
        let mut seen = BTreeSet::new();
        let mut count = 0;
        while let Some(commit) = pending.pop() {
            if !seen.insert(commit) {
                continue;
            }
            let node = &self.nodes[&commit];
            count += node.identities.iter().filter(|candidate| *candidate == id).count();
            pending.extend(node.parents.iter().copied());
        }
        count
    }

    fn has_exact_head_identity(&self, head: Commit, id: &Id) -> bool {
        matches!(self.nodes[&head].identities.as_slice(), [only] if only == id)
    }

    fn production(&self) -> CommitGraphEvidence {
        CommitGraphEvidence::from_literal_commits_for_test(self.nodes.iter().map(
            |(commit, node)| {
                (
                    commit.oid(),
                    node.parents.iter().map(|parent| parent.oid()).collect(),
                    node.identities
                        .iter()
                        .map(|identity| identity.as_str().as_bytes().to_vec())
                        .collect(),
                )
            },
        ))
        .unwrap()
    }
}

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
struct RemoteChange {
    head_ref: Option<Commit>,
    owned_base_ref: Option<Commit>,
    versions: Vec<Revision>,
    marker_target: Option<Commit>,
}

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
struct Identity {
    number: u64,
    node_id: String,
}

impl Identity {
    fn production(&self) -> PullRequestIdentity {
        PullRequestIdentity::new(self.number, self.node_id.clone()).unwrap()
    }
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
enum BaseKind {
    Default,
    Owned,
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
struct Base {
    kind: BaseKind,
    oid: Commit,
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
enum Landing {
    None,
    AutoMerge,
    MergeQueue,
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
enum TitleState {
    Final,
    Stale(u8),
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
enum BodyState {
    Final,
    Provisional,
    Stale(u8),
}

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
struct OpenPr {
    identity: Identity,
    head_oid: Commit,
    base: Base,
    title: TitleState,
    body: BodyState,
    landing: Landing,
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
enum TerminalState {
    Closed,
    Merged,
}

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
enum DurablePr {
    Absent,
    Open(OpenPr),
    Retired { identity: Identity, state: TerminalState },
}

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
struct World {
    default_tip: Commit,
    changes: BTreeMap<Id, RemoteChange>,
    pull_requests: BTreeMap<Id, DurablePr>,
    other_open_identities: BTreeSet<Identity>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct LocalIntent {
    id: Id,
    proposal: Commit,
    title: String,
    body: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct Intent {
    changes: Vec<LocalIntent>,
}

impl Intent {
    fn position(&self, id: &Id) -> Option<usize> {
        self.changes.iter().position(|change| &change.id == id)
    }

    fn get(&self, id: &Id) -> Option<&LocalIntent> {
        self.changes.iter().find(|change| &change.id == id)
    }

    fn desired_base(&self, index: usize, world: &World) -> Base {
        if index == 0 {
            Base { kind: BaseKind::Default, oid: world.default_tip }
        } else {
            Base { kind: BaseKind::Owned, oid: self.changes[index - 1].proposal }
        }
    }
}

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
struct OpenRow {
    identity: Identity,
    head_oid: Commit,
    base: Base,
    title: TitleState,
    body: BodyState,
    landing: Landing,
}

#[derive(Clone, Debug, Default, Eq, Ord, PartialEq, PartialOrd)]
struct OpenView {
    rows: BTreeMap<Id, OpenRow>,
}

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
enum TerminalRow {
    Empty,
    Retired { identity: Identity, state: TerminalState },
}

#[derive(Clone, Debug, Default, Eq, Ord, PartialEq, PartialOrd)]
struct TerminalView {
    rows: Vec<(Id, TerminalRow)>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct GitTuple {
    id: Id,
    expected_head: Option<Commit>,
    expected_base: Option<Commit>,
    desired_head: Commit,
    desired_base: Commit,
    version: u64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct CreateAction {
    id: Id,
    base_branch: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct MarkerAction {
    id: Id,
    target: Commit,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
struct FieldMask {
    title: bool,
    body: bool,
    base: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct UpdateAction {
    id: Id,
    fields: FieldMask,
    raw_body: Option<String>,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
struct LogicalPlan {
    git: Vec<GitTuple>,
    creates: Vec<CreateAction>,
    markers: Vec<MarkerAction>,
    updates: Vec<UpdateAction>,
}

impl LogicalPlan {
    fn is_done(&self) -> bool {
        self.git.is_empty()
            && self.creates.is_empty()
            && self.markers.is_empty()
            && self.updates.is_empty()
    }

    fn without_raw_bodies(&self) -> Self {
        let mut plan = self.clone();
        for update in &mut plan.updates {
            update.raw_body = None;
        }
        plan
    }
}

#[derive(Clone, Debug, Default)]
struct ActualPlan {
    git: Vec<Vec<GitTuple>>,
    creates: Vec<Vec<CreateAction>>,
    markers: Vec<Vec<MarkerAction>>,
    updates: Vec<Vec<UpdateAction>>,
    git_gate: bool,
    create_gate: bool,
    marker_gate: bool,
}

impl ActualPlan {
    fn logical(&self) -> LogicalPlan {
        LogicalPlan {
            git: self.git.iter().flatten().cloned().collect(),
            creates: self.creates.iter().flatten().cloned().collect(),
            markers: self.markers.iter().flatten().cloned().collect(),
            updates: self.updates.iter().flatten().cloned().collect(),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum OracleOutcome {
    Reject,
    Plan(LogicalPlan),
}

fn oracle_plan(
    world: &World,
    intent: &Intent,
    open: &OpenView,
    terminal: &TerminalView,
    graph: &Graph,
) -> OracleOutcome {
    if intent.changes.is_empty() {
        return OracleOutcome::Reject;
    }
    let local_ids = intent.changes.iter().map(|change| &change.id).collect::<BTreeSet<_>>();
    if local_ids.len() != intent.changes.len()
        || local_ids.iter().any(|id| !world.changes.contains_key(*id))
        || open.rows.keys().any(|id| !world.changes.contains_key(id))
    {
        return OracleOutcome::Reject;
    }

    let mut numbers = BTreeSet::new();
    let mut node_ids = BTreeSet::new();
    for identity in
        world.other_open_identities.iter().chain(open.rows.values().map(|row| &row.identity))
    {
        if !numbers.insert(identity.number) || !node_ids.insert(&identity.node_id) {
            return OracleOutcome::Reject;
        }
    }

    let missing_ids = intent
        .changes
        .iter()
        .filter(|change| !open.rows.contains_key(&change.id))
        .map(|change| change.id.clone())
        .collect::<Vec<_>>();
    if terminal.rows.len() != missing_ids.len()
        || terminal.rows.iter().map(|(id, _)| id).ne(missing_ids.iter())
    {
        return OracleOutcome::Reject;
    }
    for row in terminal.rows.iter().map(|(_, row)| row) {
        if let TerminalRow::Retired { identity, .. } = row
            && (!numbers.insert(identity.number) || !node_ids.insert(&identity.node_id))
        {
            return OracleOutcome::Reject;
        }
    }

    let mut terminal_rows = terminal.rows.iter().map(|(_, row)| row);
    let mut plan = LogicalPlan::default();
    for (index, local) in intent.changes.iter().enumerate() {
        let remote = &world.changes[&local.id];
        let row = open.rows.get(&local.id);
        let observed_default = row.is_some_and(|row| row.base.kind == BaseKind::Default);
        if !valid_history(
            &local.id,
            remote,
            Some(local.proposal),
            index == 0 || observed_default,
            world.default_tip,
            graph,
        ) {
            return OracleOutcome::Reject;
        }

        match row {
            Some(row) => {
                if remote.versions.is_empty()
                    || !remote.versions.iter().any(|revision| revision.head == row.head_oid)
                    || !valid_open_base(row.base, remote, world.default_tip)
                    || graph.reaches(row.base.oid, row.head_oid)
                    || remote.marker_target.is_none() && row.base.kind != BaseKind::Owned
                    || row.landing != Landing::None
                        && (row.base.kind == BaseKind::Owned
                            || intent.desired_base(index, world).kind == BaseKind::Owned)
                {
                    return OracleOutcome::Reject;
                }
            }
            None => {
                let Some(terminal) = terminal_rows.next() else {
                    return OracleOutcome::Reject;
                };
                if !matches!(terminal, TerminalRow::Empty) || remote.marker_target.is_some() {
                    return OracleOutcome::Reject;
                }
                plan.creates.push(CreateAction {
                    id: local.id.clone(),
                    base_branch: owned_base_name(&local.id),
                });
            }
        }

        if remote.versions.last().map(|revision| revision.head) != Some(local.proposal) {
            plan.git.push(GitTuple {
                id: local.id.clone(),
                expected_head: remote.head_ref,
                expected_base: remote.owned_base_ref,
                desired_head: local.proposal,
                desired_base: match graph.first_parent(local.proposal) {
                    Some(parent) => parent,
                    None => return OracleOutcome::Reject,
                },
                version: remote.versions.len() as u64 + 1,
            });
        }

        if remote.marker_target.is_none() {
            plan.markers.push(MarkerAction { id: local.id.clone(), target: local.proposal });
        }

        let fields = match row {
            Some(row) => FieldMask {
                title: row.title != TitleState::Final,
                body: row.body != BodyState::Final,
                base: row.base.kind != intent.desired_base(index, world).kind,
            },
            None => FieldMask { title: false, body: true, base: index == 0 },
        };
        if fields != FieldMask::default() {
            plan.updates.push(UpdateAction { id: local.id.clone(), fields, raw_body: None });
        }
    }
    if terminal_rows.next().is_some() {
        return OracleOutcome::Reject;
    }

    for (id, row) in open.rows.iter().filter(|(id, _)| !local_ids.contains(id)) {
        let remote = &world.changes[id];
        if !valid_history(
            id,
            remote,
            None,
            row.base.kind == BaseKind::Default,
            world.default_tip,
            graph,
        ) || remote.versions.is_empty()
            || !remote.versions.iter().any(|revision| revision.head == row.head_oid)
            || !valid_open_base(row.base, remote, world.default_tip)
            || graph.reaches(row.base.oid, row.head_oid)
            || remote.marker_target.is_none() && row.base.kind != BaseKind::Owned
            || row.landing != Landing::None && row.base.kind == BaseKind::Owned
        {
            return OracleOutcome::Reject;
        }
    }

    OracleOutcome::Plan(plan)
}

fn valid_history(
    id: &Id,
    remote: &RemoteChange,
    proposal: Option<Commit>,
    root_evidence_needed: bool,
    default_tip: Commit,
    graph: &Graph,
) -> bool {
    if remote.versions.is_empty() {
        if remote.head_ref.is_some()
            || remote.owned_base_ref.is_some()
            || remote.marker_target.is_some()
        {
            return false;
        }
    } else {
        let current = remote.versions.last().unwrap();
        if remote.head_ref != Some(current.head)
            || remote.owned_base_ref != Some(current.first_parent)
            || remote.marker_target.is_some_and(|target| {
                !remote.versions.iter().any(|revision| revision.head == target)
            })
        {
            return false;
        }
    }
    if remote.versions.windows(2).any(|pair| pair[0] == pair[1]) {
        return false;
    }
    let revisions = remote
        .versions
        .iter()
        .copied()
        .chain(proposal.map(|head| Revision {
            head,
            first_parent: graph.first_parent(head).unwrap_or(Commit(0)),
        }))
        .collect::<Vec<_>>();
    if revisions.iter().any(|revision| {
        graph.first_parent(revision.head) != Some(revision.first_parent)
            || !graph.has_exact_head_identity(revision.head, id)
            || graph.identity_count(revision.head, id) != 1
    }) {
        return false;
    }
    if revisions
        .iter()
        .any(|head| revisions.iter().any(|base| graph.reaches(base.first_parent, head.head)))
    {
        return false;
    }
    !root_evidence_needed
        || revisions.iter().all(|revision| !graph.reaches(default_tip, revision.head))
}

fn valid_open_base(base: Base, remote: &RemoteChange, default_tip: Commit) -> bool {
    match base.kind {
        BaseKind::Default => base.oid == default_tip,
        BaseKind::Owned => remote.versions.iter().any(|revision| revision.first_parent == base.oid),
    }
}

fn owned_base_name(id: &Id) -> String {
    format!("gherrit-bases/{}", id.as_str())
}

#[derive(Clone, Debug, Default)]
struct EvidenceBodies {
    final_bodies: BTreeMap<Id, String>,
}

#[derive(Debug)]
enum ProductionOutcome {
    Reject(String),
    Plan(ActualPlan),
}

struct ProductionHarness {
    destination: PushDestination,
    graph: CommitGraphEvidence,
}

impl ProductionHarness {
    fn new(graph: &Graph) -> Self {
        Self {
            destination: PushDestination::for_test(
                "origin",
                "https://github.com/owner/repository.git",
                Vec::new(),
            )
            .unwrap(),
            graph: graph.production(),
        }
    }
}

fn assigned_identities(world: &World, intent: &Intent) -> BTreeMap<Id, Identity> {
    let mut used_numbers = world
        .other_open_identities
        .iter()
        .map(|identity| identity.number)
        .chain(world.pull_requests.values().filter_map(|state| match state {
            DurablePr::Open(open) => Some(open.identity.number),
            DurablePr::Retired { identity, .. } => Some(identity.number),
            DurablePr::Absent => None,
        }))
        .collect::<BTreeSet<_>>();
    let mut used_node_ids = world
        .other_open_identities
        .iter()
        .map(|identity| identity.node_id.clone())
        .chain(world.pull_requests.values().filter_map(|state| match state {
            DurablePr::Open(open) => Some(open.identity.node_id.clone()),
            DurablePr::Retired { identity, .. } => Some(identity.node_id.clone()),
            DurablePr::Absent => None,
        }))
        .collect::<BTreeSet<_>>();
    let mut assigned = BTreeMap::new();
    for (index, change) in intent.changes.iter().enumerate() {
        let identity = match &world.pull_requests[&change.id] {
            DurablePr::Open(open) => open.identity.clone(),
            DurablePr::Retired { identity, .. } => identity.clone(),
            DurablePr::Absent => {
                let mut number = 100 + index as u64;
                while used_numbers.contains(&number) {
                    number += intent.changes.len() as u64 + 1;
                }
                let mut node_id = format!("PR_CREATED_{}", change.id.as_str());
                while used_node_ids.contains(&node_id) {
                    node_id.push('X');
                }
                Identity { number, node_id }
            }
        };
        used_numbers.insert(identity.number);
        used_node_ids.insert(identity.node_id.clone());
        assigned.insert(change.id.clone(), identity);
    }
    assigned
}

fn exact_open_view(world: &World) -> OpenView {
    OpenView {
        rows: world
            .pull_requests
            .iter()
            .filter_map(|(id, state)| match state {
                DurablePr::Open(open) => Some((
                    id.clone(),
                    OpenRow {
                        identity: open.identity.clone(),
                        head_oid: open.head_oid,
                        base: open.base,
                        title: open.title,
                        body: open.body,
                        landing: open.landing,
                    },
                )),
                DurablePr::Absent | DurablePr::Retired { .. } => None,
            })
            .collect(),
    }
}

fn exact_terminal_view(world: &World, intent: &Intent, open: &OpenView) -> TerminalView {
    TerminalView {
        rows: intent
            .changes
            .iter()
            .filter(|change| !open.rows.contains_key(&change.id))
            .map(|change| {
                let row = match &world.pull_requests[&change.id] {
                    DurablePr::Absent | DurablePr::Open(_) => TerminalRow::Empty,
                    DurablePr::Retired { identity, state } => {
                        TerminalRow::Retired { identity: identity.clone(), state: *state }
                    }
                };
                (change.id.clone(), row)
            })
            .collect(),
    }
}

fn metadata(intent: &Intent, index: usize) -> String {
    let parent = index.checked_sub(1).map(|position| &intent.changes[position].id);
    let child = intent.changes.get(index + 1).map(|change| &change.id);
    format!(
        "<!-- gherrit-meta: {{\"id\":\"{}\",\"parent\":{},\"child\":{}}} -->",
        intent.changes[index].id.as_str(),
        metadata_id(parent),
        metadata_id(child),
    )
}

fn metadata_id(id: Option<&Id>) -> String {
    id.map(|id| format!("\"{}\"", id.as_str())).unwrap_or_else(|| "null".to_owned())
}

fn observed_title(state: TitleState, change: &LocalIntent) -> String {
    match state {
        TitleState::Final => change.title.clone(),
        TitleState::Stale(cell) => format!("Stale title {cell} for {}", change.id.as_str()),
    }
}

fn observed_body(state: BodyState, id: &Id, intent: &Intent, bodies: &EvidenceBodies) -> String {
    let index = intent.position(id).expect("only local bodies are projected");
    match state {
        BodyState::Final => bodies.final_bodies[id].clone(),
        BodyState::Provisional => {
            format!("provisional {}\n\n{}", id.as_str(), metadata(intent, index))
        }
        BodyState::Stale(cell) => {
            format!("stale body {cell} for {}\n\n{}", id.as_str(), metadata(intent, index))
        }
    }
}

fn production_plan(
    world: &World,
    intent: &Intent,
    open: &OpenView,
    terminal: &TerminalView,
    bodies: &EvidenceBodies,
    assigned: &BTreeMap<Id, Identity>,
    harness: &ProductionHarness,
) -> ProductionOutcome {
    production_plan_result(world, intent, open, terminal, bodies, assigned, harness)
        .unwrap_or_else(|error| ProductionOutcome::Reject(error.to_string()))
}

fn production_plan_result(
    world: &World,
    intent: &Intent,
    open: &OpenView,
    terminal: &TerminalView,
    bodies: &EvidenceBodies,
    assigned: &BTreeMap<Id, Identity>,
    harness: &ProductionHarness,
) -> color_eyre::eyre::Result<ProductionOutcome> {
    let destination = &harness.destination;
    let stack = LocalStack::for_test_with_content(
        world.default_tip.oid(),
        intent.changes.iter().map(|change| {
            (
                change.id.production(),
                change.proposal.oid(),
                change.title.clone(),
                change.body.clone(),
            )
        }),
    )?;
    let local_ids = intent.changes.iter().map(|change| change.id.production()).collect::<Vec<_>>();
    let local_names = intent.changes.iter().map(|change| &change.id).collect::<BTreeSet<_>>();
    let nonlocal_ids = open
        .rows
        .keys()
        .filter(|id| !local_names.contains(id))
        .map(Id::production)
        .collect::<Vec<_>>();
    let heads = parse_remote_heads_for_destination_for_test(
        destination,
        remote_head_advertisement(world).as_bytes(),
    )?;
    let github = OpenObservation::from_complete_response_for_test(
        "owner",
        "repository",
        open_response(world, intent, open, bodies),
    )?;
    let correlated = github.correlate(local_ids.iter(), &heads)?;
    let managed_ids = local_names
        .iter()
        .copied()
        .chain(open.rows.keys().filter(|id| !local_names.contains(id)))
        .cloned()
        .collect::<BTreeSet<_>>();
    let active = heads.into_active_for_test(
        &local_ids,
        &nonlocal_ids,
        managed_tag_advertisement(world, &managed_ids).as_bytes(),
    )?;
    let requested = terminal.rows.iter().map(|(id, _)| id.production()).collect::<Vec<_>>();
    let mut accumulator = TerminalExhaustionAccumulator::new(requested.iter().cloned())?;
    for (id, row) in &terminal.rows {
        let pull_requests = match row {
            TerminalRow::Empty => Vec::new(),
            TerminalRow::Retired { identity, state } => vec![TerminalPullRequest {
                number: identity.number,
                node_id: identity.node_id.clone(),
                state: match state {
                    TerminalState::Closed => TerminalPullRequestState::Closed,
                    TerminalState::Merged => TerminalPullRequestState::Merged,
                },
            }],
        };
        accumulator = accumulator.record_page(TerminalPullRequestEvidence::for_test(
            id.production(),
            None,
            TerminalPullRequestPage { pull_requests, next_cursor: None },
        ))?;
    }
    let terminal = RepositoryTerminalHistories::for_test(
        active.destination(),
        accumulator.into_terminal_histories()?,
    );
    let context = BodyLinkContext::from_destination(active.destination(), None)?;
    let publication =
        plan_publication(context, stack, correlated, terminal, active, &harness.graph)?;
    Ok(ProductionOutcome::Plan(actual_plan(publication, assigned, intent, bodies)?))
}

fn remote_head_advertisement(world: &World) -> String {
    let default = world.default_tip.oid();
    let mut output = format!(
        "ref: refs/heads/{DEFAULT_BRANCH}\tHEAD\n{default}\tHEAD\n{default}\trefs/heads/{DEFAULT_BRANCH}\n"
    );
    for (id, change) in &world.changes {
        if let Some(head) = change.head_ref {
            writeln!(output, "{}\trefs/heads/{}", head.oid(), id.as_str()).unwrap();
        }
        if let Some(base) = change.owned_base_ref {
            writeln!(output, "{}\trefs/heads/gherrit-bases/{}", base.oid(), id.as_str()).unwrap();
        }
    }
    output
}

fn managed_tag_advertisement(world: &World, active: &BTreeSet<Id>) -> String {
    let mut output = String::new();
    for id in active {
        let change = &world.changes[id];
        for (index, revision) in change.versions.iter().enumerate() {
            writeln!(
                output,
                "{}\trefs/tags/gherrit/{}/v{}",
                revision.head.oid(),
                id.as_str(),
                index + 1,
            )
            .unwrap();
        }
        if let Some(target) = change.marker_target {
            writeln!(output, "{}\trefs/tags/gherrit/{}/pr", target.oid(), id.as_str()).unwrap();
        }
    }
    output
}

fn open_response(
    world: &World,
    intent: &Intent,
    view: &OpenView,
    bodies: &EvidenceBodies,
) -> Value {
    let mut nodes = Vec::new();
    for (id, row) in &view.rows {
        let title = intent.get(id).map_or_else(
            || format!("Nonlocal {}", id.as_str()),
            |change| observed_title(row.title, change),
        );
        let body = intent.get(id).map_or_else(
            || format!(
                "nonlocal\n\n<!-- gherrit-meta: {{\"id\":\"{}\",\"parent\":null,\"child\":null}} -->",
                id.as_str()
            ),
            |_| observed_body(row.body, id, intent, bodies),
        );
        nodes.push(json!({
            "number": row.identity.number,
            "id": row.identity.node_id,
            "title": title,
            "body": body,
            "baseRefName": match row.base.kind {
                BaseKind::Default => DEFAULT_BRANCH.to_owned(),
                BaseKind::Owned => owned_base_name(id),
            },
            "baseRefOid": row.base.oid.oid().to_string(),
            "headRefName": id.as_str(),
            "headRefOid": row.head_oid.oid().to_string(),
            "state": "OPEN",
            "isCrossRepository": false,
            "autoMergeRequest": match row.landing {
                Landing::AutoMerge => json!({ "enabledAt": "now" }),
                Landing::None | Landing::MergeQueue => Value::Null,
            },
            "isInMergeQueue": row.landing == Landing::MergeQueue,
        }));
    }
    let unrelated = world.default_tip.oid().to_string();
    for (index, identity) in world.other_open_identities.iter().enumerate() {
        nodes.push(json!({
            "number": identity.number,
            "id": identity.node_id,
            "title": "unmanaged",
            "body": "",
            "baseRefName": DEFAULT_BRANCH,
            "baseRefOid": world.default_tip.oid().to_string(),
            "headRefName": if index == 0 {
                "topic".to_owned()
            } else {
                intent.changes[0].id.as_str().to_owned()
            },
            "headRefOid": unrelated,
            "state": "OPEN",
            "isCrossRepository": index != 0,
            "autoMergeRequest": Value::Null,
            "isInMergeQueue": false,
        }));
    }
    json!({
        "data": {
            "repository": {
                "id": REPOSITORY_ID,
                "defaultBranchRef": {
                    "name": DEFAULT_BRANCH,
                    "target": { "oid": world.default_tip.oid().to_string() },
                },
                "pullRequests": {
                    "nodes": nodes,
                    "pageInfo": { "hasNextPage": false, "endCursor": null },
                },
            },
        },
    })
}

fn actual_plan(
    publication: PublicationPlan<'_>,
    assigned: &BTreeMap<Id, Identity>,
    intent: &Intent,
    bodies: &EvidenceBodies,
) -> color_eyre::eyre::Result<ActualPlan> {
    // Decode only serialized/prepared actions. The expected oracle never sees
    // these production values.
    let git = publication
        .push_arguments_for_test()
        .iter()
        .map(|(options, refspecs)| parse_git_request(options, refspecs))
        .collect::<Vec<_>>();
    let git_gate = !git.is_empty();
    match publication.into_projection_for_test() {
        ReadyProjection::Final(final_projection) => Ok(ActualPlan {
            git,
            updates: parse_final_projection(final_projection, assigned, intent, bodies),
            git_gate,
            ..ActualPlan::default()
        }),
        ReadyProjection::Markers(markers) => Ok(ActualPlan {
            git,
            markers: markers
                .arguments_for_test()
                .iter()
                .map(|(options, refspecs)| parse_marker_request(options, refspecs))
                .collect(),
            updates: parse_update_text(&markers.request_text(), assigned, intent, bodies),
            git_gate,
            marker_gate: true,
            ..ActualPlan::default()
        }),
        ReadyProjection::Creates { creates, projection } => {
            let create_batches = parse_create_requests(creates.request_batches_for_test(), intent);
            let receipts = create_batches
                .iter()
                .flatten()
                .map(|create| {
                    let identity = &assigned[&create.id];
                    (create.id.production(), identity.production())
                })
                .collect();
            let receipts = creates.complete_for_test(receipts)?;
            let markers = projection.complete(receipts)?;
            Ok(ActualPlan {
                git,
                creates: create_batches,
                markers: markers
                    .arguments_for_test()
                    .iter()
                    .map(|(options, refspecs)| parse_marker_request(options, refspecs))
                    .collect(),
                updates: parse_update_text(&markers.request_text(), assigned, intent, bodies),
                git_gate,
                create_gate: true,
                marker_gate: true,
            })
        }
    }
}

fn parse_final_projection(
    projection: FinalProjection,
    assigned: &BTreeMap<Id, Identity>,
    intent: &Intent,
    bodies: &EvidenceBodies,
) -> Vec<Vec<UpdateAction>> {
    match projection {
        FinalProjection::NoAction => Vec::new(),
        FinalProjection::Updates(updates) => {
            parse_update_requests(updates.request_batches_for_test(), assigned, intent, bodies)
        }
    }
}

fn parse_git_request(options: &[String], refspecs: &[String]) -> Vec<GitTuple> {
    let leases = push_leases(options, refspecs);
    assert_eq!(leases.len() % 3, 0);
    leases
        .chunks(3)
        .zip(refspecs.chunks(3))
        .map(|(leases, refs)| parse_git_tuple(leases, refs))
        .collect()
}

fn push_leases<'a>(options: &'a [String], refspecs: &[String]) -> &'a [String] {
    assert!(options.len() >= FIXED_PUSH_OPTIONS.len());
    assert_eq!(
        options[..FIXED_PUSH_OPTIONS.len()],
        FIXED_PUSH_OPTIONS,
        "publication must retain the exact fixed atomic push policy",
    );
    let leases = &options[FIXED_PUSH_OPTIONS.len()..];
    assert_eq!(leases.len(), refspecs.len());
    leases
}

fn parse_git_tuple(leases: &[String], refs: &[String]) -> GitTuple {
    let (head_ref, expected_head) = parse_lease(&leases[0]);
    let (base_ref, expected_base) = parse_lease(&leases[1]);
    let (tag_ref, expected_tag) = parse_lease(&leases[2]);
    let id = Id::new(head_ref.strip_prefix("refs/heads/").unwrap());
    assert_eq!(base_ref, format!("refs/heads/gherrit-bases/{}", id.as_str()));
    let tag_prefix = format!("refs/tags/gherrit/{}/v", id.as_str());
    let version = tag_ref.strip_prefix(&tag_prefix).unwrap().parse().unwrap();
    assert_eq!(expected_tag, None);
    let (desired_head, head_destination) = parse_refspec(&refs[0]);
    let (desired_base, base_destination) = parse_refspec(&refs[1]);
    let (tag_head, tag_destination) = parse_refspec(&refs[2]);
    assert_eq!(head_destination, head_ref);
    assert_eq!(base_destination, base_ref);
    assert_eq!(tag_destination, tag_ref);
    assert_eq!(desired_head, tag_head);
    GitTuple { id, expected_head, expected_base, desired_head, desired_base, version }
}

fn parse_marker_request(options: &[String], refspecs: &[String]) -> Vec<MarkerAction> {
    push_leases(options, refspecs)
        .iter()
        .zip(refspecs)
        .map(|(lease, refspec)| {
            let (reference, expected) = parse_lease(lease);
            assert_eq!(expected, None, "a marker is absent-leased");
            let id = Id::new(
                reference
                    .strip_prefix("refs/tags/gherrit/")
                    .and_then(|value| value.strip_suffix("/pr"))
                    .unwrap(),
            );
            let (target, destination) = parse_refspec(refspec);
            assert_eq!(destination, reference);
            MarkerAction { id, target }
        })
        .collect()
}

fn parse_lease(option: &str) -> (String, Option<Commit>) {
    let value = option.strip_prefix("--force-with-lease=").unwrap();
    let (reference, expected) = value.rsplit_once(':').unwrap();
    let expected = (!expected.is_empty())
        .then(|| Commit::from_oid(ObjectId::from_hex(expected.as_bytes()).unwrap()));
    (reference.to_owned(), expected)
}

fn parse_refspec(refspec: &str) -> (Commit, String) {
    let (source, destination) = refspec.split_once(':').unwrap();
    (Commit::from_oid(ObjectId::from_hex(source.as_bytes()).unwrap()), destination.to_owned())
}

fn parse_create_requests<'request>(
    requests: impl IntoIterator<Item = &'request Value>,
    intent: &Intent,
) -> Vec<Vec<CreateAction>> {
    requests
        .into_iter()
        .map(|request| {
            parse_mutation_request(request)
                .into_iter()
                .map(|operation| {
                    assert_eq!(operation.name, "createPullRequest");
                    assert_eq!(operation.fields.len(), 6);
                    assert_eq!(operation.fields["repositoryId"], REPOSITORY_ID);
                    let id = Id::new(operation.fields["headRefName"].clone());
                    let index = intent.position(&id).expect("only local changes are created");
                    assert_eq!(operation.fields["title"], intent.changes[index].title);
                    assert!(operation.fields["body"].contains(&metadata(intent, index)));
                    assert_eq!(
                        operation.fields["clientMutationId"],
                        format!("gherrit:create:{}", id.as_str())
                    );
                    CreateAction { id, base_branch: operation.fields["baseRefName"].clone() }
                })
                .collect()
        })
        .collect()
}

fn parse_update_text(
    text: &str,
    assigned: &BTreeMap<Id, Identity>,
    intent: &Intent,
    bodies: &EvidenceBodies,
) -> Vec<Vec<UpdateAction>> {
    let values = serde_json::Deserializer::from_str(text)
        .into_iter::<Value>()
        .map(|value| value.unwrap())
        .collect::<Vec<_>>();
    parse_update_requests(values.iter(), assigned, intent, bodies)
}

fn parse_update_requests<'request>(
    requests: impl IntoIterator<Item = &'request Value>,
    assigned: &BTreeMap<Id, Identity>,
    intent: &Intent,
    bodies: &EvidenceBodies,
) -> Vec<Vec<UpdateAction>> {
    let by_node = assigned
        .iter()
        .map(|(id, identity)| (identity.node_id.as_str(), id))
        .collect::<BTreeMap<_, _>>();
    requests
        .into_iter()
        .map(|request| {
            parse_mutation_request(request)
                .into_iter()
                .map(|operation| {
                    assert_eq!(operation.name, "updatePullRequest");
                    assert!(operation.fields.len() >= 3 && operation.fields.len() <= 5);
                    let node_id = &operation.fields["pullRequestId"];
                    let id = (*by_node.get(node_id.as_str()).unwrap()).clone();
                    assert_eq!(
                        operation.fields["clientMutationId"],
                        format!("gherrit:update:{node_id}")
                    );
                    let index = intent.position(&id).unwrap();
                    if let Some(title) = operation.fields.get("title") {
                        assert_eq!(title, &intent.changes[index].title);
                    }
                    if let (Some(body), Some(expected)) =
                        (operation.fields.get("body"), bodies.final_bodies.get(&id))
                    {
                        assert_eq!(body, expected);
                    }
                    if let Some(base) = operation.fields.get("baseRefName") {
                        assert_eq!(base, &desired_base_branch(intent, index));
                    }
                    UpdateAction {
                        id,
                        fields: FieldMask {
                            title: operation.fields.contains_key("title"),
                            body: operation.fields.contains_key("body"),
                            base: operation.fields.contains_key("baseRefName"),
                        },
                        raw_body: operation.fields.get("body").cloned(),
                    }
                })
                .collect()
        })
        .collect()
}

fn desired_base_branch(intent: &Intent, index: usize) -> String {
    if index == 0 { DEFAULT_BRANCH.to_owned() } else { owned_base_name(&intent.changes[index].id) }
}

#[derive(Debug)]
struct ParsedMutationOperation {
    name: String,
    fields: BTreeMap<String, String>,
}

fn parse_mutation_request(request: &Value) -> Vec<ParsedMutationOperation> {
    let object = request.as_object().expect("a mutation request is a JSON object");
    assert_eq!(object.len(), 1);
    let query = object["query"].as_str().expect("the mutation query is text");
    let mut parser = GraphqlParser::new(query);
    parser.expect_word("mutation");
    parser.expect_byte(b'{');
    let mut operations = Vec::new();
    while !parser.consume_byte(b'}') {
        let alias = parser.identifier();
        assert_eq!(alias, format!("op{}", operations.len()));
        parser.expect_byte(b':');
        let name = parser.identifier();
        parser.expect_byte(b'(');
        assert_eq!(parser.identifier(), "input");
        parser.expect_byte(b':');
        parser.expect_byte(b'{');
        let mut fields = BTreeMap::new();
        while !parser.consume_byte(b'}') {
            let field = parser.identifier();
            parser.expect_byte(b':');
            let value = parser.json_string();
            assert!(fields.insert(field, value).is_none());
            if !parser.consume_byte(b',') {
                parser.expect_next_byte(b'}');
            }
        }
        parser.expect_byte(b')');
        parser.skip_balanced_selection();
        operations.push(ParsedMutationOperation { name, fields });
    }
    parser.finish();
    operations
}

struct GraphqlParser<'a> {
    input: &'a [u8],
    position: usize,
}

impl<'a> GraphqlParser<'a> {
    fn new(input: &'a str) -> Self {
        Self { input: input.as_bytes(), position: 0 }
    }

    fn skip_whitespace(&mut self) {
        while self.input.get(self.position).is_some_and(u8::is_ascii_whitespace) {
            self.position += 1;
        }
    }

    fn expect_word(&mut self, expected: &str) {
        assert_eq!(self.identifier(), expected);
    }

    fn identifier(&mut self) -> String {
        self.skip_whitespace();
        let start = self.position;
        while self
            .input
            .get(self.position)
            .is_some_and(|byte| byte.is_ascii_alphanumeric() || *byte == b'_')
        {
            self.position += 1;
        }
        assert!(self.position > start, "expected GraphQL identifier");
        String::from_utf8(self.input[start..self.position].to_vec()).unwrap()
    }

    fn consume_byte(&mut self, expected: u8) -> bool {
        self.skip_whitespace();
        if self.input.get(self.position) == Some(&expected) {
            self.position += 1;
            true
        } else {
            false
        }
    }

    fn expect_byte(&mut self, expected: u8) {
        assert!(self.consume_byte(expected), "expected GraphQL byte {expected:?}");
    }

    fn expect_next_byte(&mut self, expected: u8) {
        self.skip_whitespace();
        assert_eq!(self.input.get(self.position), Some(&expected));
    }

    fn json_string(&mut self) -> String {
        self.skip_whitespace();
        assert_eq!(self.input.get(self.position), Some(&b'"'));
        let start = self.position;
        self.position += 1;
        let mut escaped = false;
        loop {
            let byte = *self.input.get(self.position).expect("unterminated GraphQL string");
            self.position += 1;
            if escaped {
                escaped = false;
            } else if byte == b'\\' {
                escaped = true;
            } else if byte == b'"' {
                break;
            }
        }
        serde_json::from_slice(&self.input[start..self.position]).unwrap()
    }

    fn skip_balanced_selection(&mut self) {
        self.expect_byte(b'{');
        let mut depth = 1;
        while depth != 0 {
            let byte = *self.input.get(self.position).expect("unterminated GraphQL selection");
            self.position += 1;
            match byte {
                b'{' => depth += 1,
                b'}' => depth -= 1,
                b'"' => {
                    self.position -= 1;
                    let _ = self.json_string();
                }
                _ => {}
            }
        }
    }

    fn finish(&mut self) {
        self.skip_whitespace();
        assert_eq!(self.position, self.input.len());
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Presentation {
    Clean,
    BodyStale,
}

impl Presentation {
    const ALL: [Self; 2] = [Self::Clean, Self::BodyStale];

    fn label(self) -> &'static str {
        match self {
            Self::Clean => "clean",
            Self::BodyStale => "body-stale",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Topology {
    One,
    TwoAb,
    TwoBa,
    Three,
}

impl Topology {
    const ALL: [Self; 4] = [Self::One, Self::TwoAb, Self::TwoBa, Self::Three];

    fn label(self) -> &'static str {
        match self {
            Self::One => "one",
            Self::TwoAb => "two-ab",
            Self::TwoBa => "two-ba",
            Self::Three => "three",
        }
    }

    fn ids(self) -> Vec<Id> {
        match self {
            Self::One => vec![Id::new("Ga")],
            Self::TwoAb => vec![Id::new("Ga"), Id::new("Gb")],
            Self::TwoBa => vec![Id::new("Gb"), Id::new("Ga")],
            Self::Three => vec![Id::new("Ga"), Id::new("Gb"), Id::new("Gc")],
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Phase {
    New,
    PublishedNoPr,
    UnmarkedOwnedOpen,
    MarkedProvisional,
    MarkedFinal,
}

impl Phase {
    const ALL: [Self; 5] = [
        Self::New,
        Self::PublishedNoPr,
        Self::UnmarkedOwnedOpen,
        Self::MarkedProvisional,
        Self::MarkedFinal,
    ];

    fn index(self) -> usize {
        Self::ALL.iter().position(|candidate| *candidate == self).unwrap()
    }

    fn has_history(self) -> bool {
        self != Self::New
    }

    fn has_open(self) -> bool {
        matches!(self, Self::UnmarkedOwnedOpen | Self::MarkedProvisional | Self::MarkedFinal)
    }

    fn has_marker(self) -> bool {
        matches!(self, Self::MarkedProvisional | Self::MarkedFinal)
    }
}

struct Case {
    label: String,
    topology: Topology,
    phases: Vec<Phase>,
    presentation: Presentation,
    graph: Graph,
    intent: Intent,
    world: World,
    assigned: BTreeMap<Id, Identity>,
    bodies: EvidenceBodies,
}

fn main_case(topology: Topology, phases: Vec<Phase>, presentation: Presentation) -> Case {
    let ids = topology.ids();
    assert_eq!(ids.len(), phases.len());
    let default_tip = Commit(1);
    let (graph, proposals, published) = catalogue_graph(topology, &ids, default_tip);
    let intent = Intent {
        changes: ids
            .iter()
            .zip(proposals)
            .enumerate()
            .map(|(index, (id, proposal))| LocalIntent {
                id: id.clone(),
                proposal,
                title: format!("Title {}", id.as_str()),
                body: format!("Body {} at position {index}", id.as_str()),
            })
            .collect(),
    };
    let mut changes = BTreeMap::new();
    let mut pull_requests = BTreeMap::new();
    for (index, ((id, phase), revision)) in ids.iter().zip(&phases).zip(published).enumerate() {
        let versions = phase.has_history().then_some(revision).into_iter().collect::<Vec<_>>();
        let (head_ref, owned_base_ref) = versions
            .last()
            .map(|current| (Some(current.head), Some(current.first_parent)))
            .unwrap_or((None, None));
        changes.insert(
            id.clone(),
            RemoteChange {
                head_ref,
                owned_base_ref,
                versions,
                marker_target: phase.has_marker().then_some(revision.head),
            },
        );
        let durable = if phase.has_open() {
            let base = match phase {
                Phase::UnmarkedOwnedOpen | Phase::MarkedProvisional => {
                    Base { kind: BaseKind::Owned, oid: revision.first_parent }
                }
                Phase::MarkedFinal if topology == Topology::TwoBa => {
                    if id.as_str() == "Ga" {
                        Base { kind: BaseKind::Default, oid: default_tip }
                    } else {
                        Base { kind: BaseKind::Owned, oid: revision.first_parent }
                    }
                }
                Phase::MarkedFinal => {
                    if index == 0 {
                        Base { kind: BaseKind::Default, oid: default_tip }
                    } else {
                        Base { kind: BaseKind::Owned, oid: revision.first_parent }
                    }
                }
                Phase::New | Phase::PublishedNoPr => unreachable!(),
            };
            let body = match (presentation, phase) {
                (Presentation::BodyStale, _) => BodyState::Stale(index as u8),
                (Presentation::Clean, Phase::MarkedFinal) => BodyState::Final,
                (Presentation::Clean, _) => BodyState::Provisional,
            };
            DurablePr::Open(OpenPr {
                identity: Identity {
                    number: 10 + index as u64,
                    node_id: format!("PR_OPEN_{}", id.as_str()),
                },
                head_oid: revision.head,
                base,
                title: TitleState::Final,
                body,
                landing: Landing::None,
            })
        } else {
            DurablePr::Absent
        };
        pull_requests.insert(id.clone(), durable);
    }
    let world = World {
        default_tip,
        changes,
        pull_requests,
        other_open_identities: [
            Identity { number: 900, node_id: "PR_UNMANAGED".to_owned() },
            Identity { number: 901, node_id: "PR_FORK".to_owned() },
        ]
        .into_iter()
        .collect(),
    };
    let assigned = assigned_identities(&world, &intent);
    let bodies = evidence_bodies(&world, &intent, &graph, &assigned);
    let digits =
        phases.iter().map(|phase| char::from(b'0' + phase.index() as u8)).collect::<String>();
    Case {
        label: format!("{}-{digits}-{}", topology.label(), presentation.label()),
        topology,
        phases,
        presentation,
        graph,
        intent,
        world,
        assigned,
        bodies,
    }
}

fn catalogue_graph(
    topology: Topology,
    ids: &[Id],
    default_tip: Commit,
) -> (Graph, Vec<Commit>, Vec<Revision>) {
    let mut graph = Graph::new(default_tip);
    graph.insert(Commit(2), [default_tip], []);
    if topology == Topology::TwoBa {
        let ga = Id::new("Ga");
        let gb = Id::new("Gb");
        let old_a = Commit(101);
        let old_b = Commit(102);
        let new_b = Commit(103);
        let new_a = Commit(104);
        graph.insert(old_a, [default_tip], [ga.clone()]);
        graph.insert(old_b, [old_a], [gb.clone()]);
        graph.insert(new_b, [default_tip], [gb]);
        graph.insert(new_a, [new_b], [ga]);
        return (
            graph,
            vec![new_b, new_a],
            vec![
                Revision { head: old_b, first_parent: old_a },
                Revision { head: old_a, first_parent: default_tip },
            ],
        );
    }
    let mut parent = default_tip;
    let mut proposals = Vec::new();
    let mut revisions = Vec::new();
    for (index, id) in ids.iter().enumerate() {
        let head = Commit(110 + index as u32);
        graph.insert(head, [parent], [id.clone()]);
        proposals.push(head);
        revisions.push(Revision { head, first_parent: parent });
        parent = head;
    }
    (graph, proposals, revisions)
}

fn evidence_bodies(
    world: &World,
    intent: &Intent,
    graph: &Graph,
    assigned: &BTreeMap<Id, Identity>,
) -> EvidenceBodies {
    // This is an actual-side equivalence adapter, not expected-model input.
    // Production renders one opaque representative for `BodyState::Final`;
    // the oracle independently treats every legal unequal body as stale and
    // decides only whether the exact body field must change. Focused body
    // recipe tests own the renderer's text and link correctness.
    let mut projected = world.clone();
    for (index, local) in intent.changes.iter().enumerate() {
        let first_parent = graph.first_parent(local.proposal).unwrap();
        let remote = projected.changes.get_mut(&local.id).unwrap();
        if remote.versions.last().map(|revision| revision.head) != Some(local.proposal) {
            remote.versions.push(Revision { head: local.proposal, first_parent });
        }
        remote.head_ref = Some(local.proposal);
        remote.owned_base_ref = Some(first_parent);
        remote.marker_target.get_or_insert(local.proposal);
        projected.pull_requests.insert(
            local.id.clone(),
            DurablePr::Open(OpenPr {
                identity: assigned[&local.id].clone(),
                head_oid: local.proposal,
                base: intent.desired_base(index, &projected),
                title: TitleState::Final,
                body: BodyState::Stale(250),
                landing: Landing::None,
            }),
        );
    }
    let open = exact_open_view(&projected);
    let terminal = exact_terminal_view(&projected, intent, &open);
    let harness = ProductionHarness::new(graph);
    let actual = match production_plan(
        &projected,
        intent,
        &open,
        &terminal,
        &EvidenceBodies::default(),
        assigned,
        &harness,
    ) {
        ProductionOutcome::Reject(error) => panic!("body evidence bootstrap rejected: {error}"),
        ProductionOutcome::Plan(actual) => actual,
    };
    let logical = actual.logical();
    assert!(logical.git.is_empty());
    assert!(logical.creates.is_empty());
    assert!(logical.markers.is_empty());
    assert_eq!(logical.updates.len(), intent.changes.len());
    let final_bodies = logical
        .updates
        .into_iter()
        .map(|update| {
            assert_eq!(update.fields, FieldMask { title: false, body: true, base: false });
            (update.id, update.raw_body.unwrap())
        })
        .collect();
    EvidenceBodies { final_bodies }
}

fn phase_vectors(width: usize) -> Vec<Vec<Phase>> {
    (0..5_usize.pow(width as u32))
        .map(|mut encoded| {
            (0..width)
                .map(|_| {
                    let phase = Phase::ALL[encoded % Phase::ALL.len()];
                    encoded /= Phase::ALL.len();
                    phase
                })
                .collect()
        })
        .collect()
}

fn apply_git_batch(world: &World, batch: &[GitTuple], graph: &Graph) -> World {
    for tuple in batch {
        let remote = &world.changes[&tuple.id];
        assert_eq!(remote.head_ref, tuple.expected_head);
        assert_eq!(remote.owned_base_ref, tuple.expected_base);
        assert_eq!(tuple.version, remote.versions.len() as u64 + 1);
        assert_eq!(graph.first_parent(tuple.desired_head), Some(tuple.desired_base));
    }
    let mut next = world.clone();
    for tuple in batch {
        let remote = next.changes.get_mut(&tuple.id).unwrap();
        let immutable = remote.versions.clone();
        remote.head_ref = Some(tuple.desired_head);
        remote.owned_base_ref = Some(tuple.desired_base);
        remote
            .versions
            .push(Revision { head: tuple.desired_head, first_parent: tuple.desired_base });
        assert_eq!(&remote.versions[..immutable.len()], immutable);
        if let DurablePr::Open(open) = next.pull_requests.get_mut(&tuple.id).unwrap() {
            open.head_oid = tuple.desired_head;
            if open.base.kind == BaseKind::Owned {
                open.base.oid = tuple.desired_base;
            }
        }
    }
    settle_valid_effect(world, next, graph, "Git tuple publication")
}

fn apply_create(
    world: &World,
    action: &CreateAction,
    identity: &Identity,
    graph: &Graph,
) -> (World, bool) {
    assert_eq!(action.base_branch, owned_base_name(&action.id));
    let remote = &world.changes[&action.id];
    let head = remote.head_ref.expect("Git publication precedes create");
    let base_oid = remote.owned_base_ref.expect("Git publication precedes create");
    match &world.pull_requests[&action.id] {
        DurablePr::Absent => {
            let mut next = world.clone();
            next.pull_requests.insert(
                action.id.clone(),
                DurablePr::Open(OpenPr {
                    identity: identity.clone(),
                    head_oid: head,
                    base: Base { kind: BaseKind::Owned, oid: base_oid },
                    title: TitleState::Final,
                    body: BodyState::Provisional,
                    landing: Landing::None,
                }),
            );
            (settle_valid_effect(world, next, graph, "pull request creation"), true)
        }
        DurablePr::Open(open) => {
            // GitHub's same-repository OPEN head/base-pair uniqueness turns a
            // retry after an omitted row into an atomic duplicate failure.
            assert_eq!(open.head_oid, head);
            assert_eq!(open.base, Base { kind: BaseKind::Owned, oid: base_oid });
            (settle_valid_effect(world, world.clone(), graph, "same-key create rejection"), false)
        }
        DurablePr::Retired { .. } => panic!("terminal pull requests are absorbing"),
    }
}

fn apply_marker_batch(world: &World, batch: &[MarkerAction], graph: &Graph) -> World {
    for action in batch {
        let remote = &world.changes[&action.id];
        assert_eq!(remote.marker_target, None, "marker creation is absent-leased");
        assert_eq!(remote.versions.last().map(|revision| revision.head), Some(action.target));
    }
    let mut next = world.clone();
    for action in batch {
        let remote = next.changes.get_mut(&action.id).unwrap();
        let immutable_versions = remote.versions.clone();
        assert_eq!(remote.marker_target.replace(action.target), None);
        assert_eq!(remote.versions, immutable_versions);
    }
    settle_valid_effect(world, next, graph, "pull request marker publication")
}

fn apply_update(world: &World, intent: &Intent, action: &UpdateAction, graph: &Graph) -> World {
    let index = intent.position(&action.id).unwrap();
    let mut next = world.clone();
    let DurablePr::Open(open) = next.pull_requests.get_mut(&action.id).unwrap() else {
        panic!("only an OPEN pull request can be updated")
    };
    if action.fields.title {
        open.title = TitleState::Final;
    }
    if action.fields.body {
        assert!(action.raw_body.is_some());
        open.body = BodyState::Final;
    }
    if action.fields.base {
        open.base = intent.desired_base(index, world);
    }
    settle_valid_effect(world, next, graph, "pull request projection")
}

fn settle_pull_request_lifecycle(world: &World, graph: &Graph) -> World {
    let mut settled = world.clone();
    for state in settled.pull_requests.values_mut() {
        let DurablePr::Open(open) = state else {
            continue;
        };
        if graph.reaches(open.base.oid, open.head_oid) {
            *state = DurablePr::Retired {
                identity: open.identity.clone(),
                state: TerminalState::Merged,
            };
        }
    }
    settled
}

fn retired_lifecycle(world: &World) -> BTreeMap<Id, (Identity, TerminalState)> {
    world
        .pull_requests
        .iter()
        .filter_map(|(id, state)| match state {
            DurablePr::Retired { identity, state } => {
                Some((id.clone(), (identity.clone(), *state)))
            }
            DurablePr::Absent | DurablePr::Open(_) => None,
        })
        .collect()
}

/// Applies GitHub's permanent merge transition at every modeled effect
/// boundary and proves that a valid production effect introduced no new
/// retirement. Returning the settled value keeps lifecycle state in the
/// durable model rather than using settlement only as a discarded tripwire.
fn settle_valid_effect(before: &World, effect: World, graph: &Graph, effect_name: &str) -> World {
    let retired_before = retired_lifecycle(before);
    let settled = settle_pull_request_lifecycle(&effect, graph);
    assert_eq!(
        settled.pull_requests, effect.pull_requests,
        "{effect_name} must not indirectly merge an OPEN pull request",
    );
    assert_eq!(
        retired_lifecycle(&settled),
        retired_before,
        "{effect_name} must not introduce a retired identity",
    );
    settled
}

fn assert_safe(world: &World, intent: &Intent, graph: &Graph) {
    let settled = settle_pull_request_lifecycle(world, graph);
    assert_eq!(
        settled.pull_requests, world.pull_requests,
        "a safe prefix never permanently merges an OPEN pull request"
    );
    let mut numbers = BTreeSet::new();
    let mut node_ids = BTreeSet::new();
    for identity in
        world.other_open_identities.iter().chain(world.pull_requests.values().filter_map(|state| {
            match state {
                DurablePr::Absent => None,
                DurablePr::Open(open) => Some(&open.identity),
                DurablePr::Retired { identity, .. } => Some(identity),
            }
        }))
    {
        assert!(numbers.insert(identity.number), "pull request numbers are globally unique");
        assert!(node_ids.insert(&identity.node_id), "pull request node IDs are globally unique");
    }

    for (id, remote) in &world.changes {
        if remote.versions.is_empty() {
            assert_eq!(remote.head_ref, None);
            assert_eq!(remote.owned_base_ref, None);
            assert_eq!(remote.marker_target, None);
        } else {
            let current = remote.versions.last().unwrap();
            assert_eq!(remote.head_ref, Some(current.head));
            assert_eq!(remote.owned_base_ref, Some(current.first_parent));
            if let Some(marker) = remote.marker_target {
                assert!(remote.versions.iter().any(|revision| revision.head == marker));
            }
        }
        assert!(remote.versions.windows(2).all(|pair| pair[0] != pair[1]));
        for revision in &remote.versions {
            assert_eq!(graph.first_parent(revision.head), Some(revision.first_parent));
            assert!(graph.has_exact_head_identity(revision.head, id));
            assert_eq!(graph.identity_count(revision.head, id), 1);
        }
        let revisions = remote
            .versions
            .iter()
            .copied()
            .chain(intent.get(id).map(|local| Revision {
                head: local.proposal,
                first_parent: graph.first_parent(local.proposal).unwrap(),
            }))
            .collect::<Vec<_>>();
        let default_relevant = intent.position(id) == Some(0)
            || matches!(
                world.pull_requests.get(id),
                Some(DurablePr::Open(open)) if open.base.kind == BaseKind::Default
            );
        if default_relevant {
            for revision in &revisions {
                assert!(!graph.reaches(world.default_tip, revision.head));
            }
        }
        for head in &revisions {
            for base in &revisions {
                assert!(!graph.reaches(base.first_parent, head.head));
            }
        }
        if let Some(DurablePr::Open(open)) = world.pull_requests.get(id) {
            assert!(!remote.versions.is_empty());
            assert!(remote.versions.iter().any(|revision| revision.head == open.head_oid));
            assert!(valid_open_base(open.base, remote, world.default_tip));
            assert!(!graph.reaches(open.base.oid, open.head_oid));
            if remote.marker_target.is_none() {
                assert_eq!(open.base.kind, BaseKind::Owned);
            }
            if open.base.kind == BaseKind::Owned {
                assert_eq!(owned_base_name(id), format!("gherrit-bases/{}", id.as_str()));
            }
        }
    }
}

fn deficit(world: &World, intent: &Intent) -> (usize, usize, usize, usize) {
    let tuples = intent
        .changes
        .iter()
        .filter(|local| {
            world.changes[&local.id].versions.last().map(|revision| revision.head)
                != Some(local.proposal)
        })
        .count();
    let missing = intent
        .changes
        .iter()
        .filter(|local| matches!(world.pull_requests[&local.id], DurablePr::Absent))
        .count();
    let markers = intent
        .changes
        .iter()
        .filter(|local| world.changes[&local.id].marker_target.is_none())
        .count();
    let stale = intent
        .changes
        .iter()
        .enumerate()
        .filter_map(|(index, local)| match &world.pull_requests[&local.id] {
            DurablePr::Open(open) => Some(
                usize::from(open.title != TitleState::Final)
                    + usize::from(open.body != BodyState::Final)
                    + usize::from(open.base.kind != intent.desired_base(index, world).kind),
            ),
            DurablePr::Absent | DurablePr::Retired { .. } => None,
        })
        .sum();
    (tuples, missing, markers, stale)
}

fn observation_views(world: &World, intent: &Intent) -> Vec<(OpenView, TerminalView)> {
    let local_open = intent
        .changes
        .iter()
        .filter(|local| matches!(world.pull_requests[&local.id], DurablePr::Open(_)))
        .map(|local| local.id.clone())
        .collect::<Vec<_>>();
    let exact = exact_open_view(world);
    let mut views = BTreeSet::new();
    for mask in 0..(1_usize << local_open.len()) {
        let mut open = exact.clone();
        for (bit, id) in local_open.iter().enumerate() {
            if mask & (1 << bit) == 0 {
                open.rows.remove(id);
            }
        }
        let terminal = exact_terminal_view(world, intent, &open);
        views.insert((open, terminal));
    }
    views.into_iter().collect()
}

struct ComparisonContext<'a> {
    intent: &'a Intent,
    graph: &'a Graph,
    bodies: &'a EvidenceBodies,
    assigned: &'a BTreeMap<Id, Identity>,
    production: &'a ProductionHarness,
}

impl<'a> ComparisonContext<'a> {
    fn for_case(case: &'a Case, production: &'a ProductionHarness) -> Self {
        Self {
            intent: &case.intent,
            graph: &case.graph,
            bodies: &case.bodies,
            assigned: &case.assigned,
            production,
        }
    }
}

fn assert_matching_plan(
    world: &World,
    open: &OpenView,
    terminal: &TerminalView,
    context: &ComparisonContext<'_>,
    coverage: &mut Coverage,
    label: &str,
) -> Option<ActualPlan> {
    coverage.production_calls += 1;
    let expected = oracle_plan(world, context.intent, open, terminal, context.graph);
    let actual = production_plan(
        world,
        context.intent,
        open,
        terminal,
        context.bodies,
        context.assigned,
        context.production,
    );
    match (expected, actual) {
        (OracleOutcome::Reject, ProductionOutcome::Reject(_)) => {
            coverage.safe_rejections += 1;
            None
        }
        (OracleOutcome::Reject, ProductionOutcome::Plan(plan)) => {
            panic!("{label}: production accepted oracle rejection: {:#?}", plan.logical())
        }
        (OracleOutcome::Plan(_), ProductionOutcome::Reject(error)) => {
            panic!("{label}: production rejected oracle plan: {error}")
        }
        (OracleOutcome::Plan(expected), ProductionOutcome::Plan(actual)) => {
            coverage.matched_plans += 1;
            assert_eq!(actual.git_gate, !actual.git.is_empty(), "{label}: initial Git gate");
            assert_eq!(actual.create_gate, !actual.creates.is_empty(), "{label}: create gate");
            assert_eq!(actual.marker_gate, !actual.markers.is_empty(), "{label}: marker gate");
            assert!(actual.git.iter().all(|batch| !batch.is_empty()));
            assert!(actual.creates.iter().all(|batch| !batch.is_empty()));
            assert!(actual.markers.iter().all(|batch| !batch.is_empty()));
            assert!(actual.updates.iter().all(|batch| !batch.is_empty()));
            assert_eq!(
                actual.logical().without_raw_bodies(),
                expected.without_raw_bodies(),
                "{label}: production and independent semantic actions differ"
            );
            Some(actual)
        }
    }
}

#[derive(Clone, Copy)]
enum VisibleTuplePart {
    Head,
    Base,
    VersionTag,
}

const VISIBILITY_ORDERS: [[VisibleTuplePart; 3]; 6] = [
    [VisibleTuplePart::Head, VisibleTuplePart::Base, VisibleTuplePart::VersionTag],
    [VisibleTuplePart::Head, VisibleTuplePart::VersionTag, VisibleTuplePart::Base],
    [VisibleTuplePart::Base, VisibleTuplePart::Head, VisibleTuplePart::VersionTag],
    [VisibleTuplePart::Base, VisibleTuplePart::VersionTag, VisibleTuplePart::Head],
    [VisibleTuplePart::VersionTag, VisibleTuplePart::Head, VisibleTuplePart::Base],
    [VisibleTuplePart::VersionTag, VisibleTuplePart::Base, VisibleTuplePart::Head],
];

fn assert_visibility_orders(
    world: &World,
    batches: &[Vec<GitTuple>],
    intent: &Intent,
    graph: &Graph,
    coverage: &mut Coverage,
) {
    // One durable Git batch is atomic. These prefixes model only GitHub's
    // transient visibility of refs after that durable effect, so none is fed
    // back to the planner as a fresh Git observation. Cross-change orders
    // factor into these per-change permutations: a pull request reads only
    // its own head and owned base (or the unchanged default), while a version
    // tag cannot affect reachability. Projecting any global interleaving onto
    // one change therefore yields exactly one prefix checked below.
    assert_eq!(
        batches.iter().flatten().map(|tuple| &tuple.id).collect::<BTreeSet<_>>().len(),
        batches.iter().flatten().count(),
        "one publication plan contains at most one tuple per change",
    );
    for tuple in batches.iter().flatten() {
        for order in VISIBILITY_ORDERS {
            let mut head = tuple.expected_head;
            let mut base = tuple.expected_base;
            let mut tag = None;
            for part in order {
                match part {
                    VisibleTuplePart::Head => head = Some(tuple.desired_head),
                    VisibleTuplePart::Base => base = Some(tuple.desired_base),
                    VisibleTuplePart::VersionTag => tag = Some(tuple.desired_head),
                }
                assert_safe(world, intent, graph);
                assert!(head == tuple.expected_head || head == Some(tuple.desired_head));
                assert!(base == tuple.expected_base || base == Some(tuple.desired_base));
                assert!(tag.is_none() || tag == Some(tuple.desired_head));
                assert_eq!(graph.first_parent(tuple.desired_head), Some(tuple.desired_base));
                if let DurablePr::Open(open) = &world.pull_requests[&tuple.id] {
                    let visible_head = head.expect("an OPEN PR has a visible head");
                    let visible_base = if open.base.kind == BaseKind::Default {
                        open.base.oid
                    } else {
                        base.expect("an OPEN owned base is visible")
                    };
                    assert!(!graph.reaches(visible_base, visible_head));
                    let mut visible_world = world.clone();
                    let DurablePr::Open(visible) =
                        visible_world.pull_requests.get_mut(&tuple.id).unwrap()
                    else {
                        unreachable!()
                    };
                    visible.head_oid = visible_head;
                    visible.base.oid = visible_base;
                    let settled = settle_pull_request_lifecycle(&visible_world, graph);
                    assert_eq!(settled.pull_requests, visible_world.pull_requests);
                }
                coverage.visibility_prefixes += 1;
            }
        }
    }
}

fn git_restart_worlds(
    initial: &World,
    batches: &[Vec<GitTuple>],
    intent: &Intent,
    graph: &Graph,
    coverage: &mut Coverage,
) -> BTreeSet<World> {
    let mut worlds = BTreeSet::new();
    let mut prefix = initial.clone();
    worlds.insert(prefix.clone());
    for batch in batches {
        let new = apply_git_batch(&prefix, batch, graph);
        assert_safe(&new, intent, graph);
        worlds.insert(prefix.clone());
        worlds.insert(new.clone());
        coverage.git_lost_ack_outcomes += 2;
        prefix = new;
    }
    worlds
}

fn create_restart_worlds(
    initial: &World,
    batches: &[Vec<CreateAction>],
    assigned: &BTreeMap<Id, Identity>,
    intent: &Intent,
    graph: &Graph,
    coverage: &mut Coverage,
) -> BTreeSet<World> {
    let mut worlds = BTreeSet::new();
    let mut prefix = initial.clone();
    worlds.insert(prefix.clone());
    for batch in batches {
        assert!(batch.len() < usize::BITS as usize);
        for mask in 0..(1_usize << batch.len()) {
            let mut subset = prefix.clone();
            for (index, action) in batch.iter().enumerate() {
                if mask & (1 << index) != 0 {
                    let (next, created) =
                        apply_create(&subset, action, &assigned[&action.id], graph);
                    assert!(created, "the exact-view initial create is fresh");
                    subset = next;
                    assert_safe(&subset, intent, graph);
                }
            }
            worlds.insert(subset);
            coverage.create_alias_masks += 1;
        }
        for action in batch {
            let (next, created) = apply_create(&prefix, action, &assigned[&action.id], graph);
            assert!(created);
            prefix = next;
        }
    }
    worlds
}

fn marker_restart_worlds(
    initial: &World,
    batches: &[Vec<MarkerAction>],
    intent: &Intent,
    graph: &Graph,
    coverage: &mut Coverage,
) -> BTreeSet<World> {
    let mut worlds = BTreeSet::new();
    let mut prefix = initial.clone();
    worlds.insert(prefix.clone());
    for batch in batches {
        let new = apply_marker_batch(&prefix, batch, graph);
        assert_safe(&new, intent, graph);
        worlds.insert(prefix.clone());
        worlds.insert(new.clone());
        coverage.marker_lost_ack_outcomes += 2;
        prefix = new;
    }
    worlds
}

fn update_restart_worlds(
    initial: &World,
    batches: &[Vec<UpdateAction>],
    intent: &Intent,
    graph: &Graph,
    coverage: &mut Coverage,
) -> BTreeSet<World> {
    let mut worlds = BTreeSet::new();
    let mut prefix = initial.clone();
    worlds.insert(prefix.clone());
    for batch in batches {
        assert!(batch.len() < usize::BITS as usize);
        for mask in 0..(1_usize << batch.len()) {
            let mut subset = prefix.clone();
            for (index, action) in batch.iter().enumerate() {
                if mask & (1 << index) != 0 {
                    subset = apply_update(&subset, intent, action, graph);
                    assert_safe(&subset, intent, graph);
                }
            }
            worlds.insert(subset);
            coverage.update_alias_masks += 1;
        }
        for action in batch {
            prefix = apply_update(&prefix, intent, action, graph);
        }
    }
    worlds
}

fn execute_all(
    mut world: World,
    actual: &ActualPlan,
    intent: &Intent,
    graph: &Graph,
    assigned: &BTreeMap<Id, Identity>,
) -> World {
    for batch in &actual.git {
        world = apply_git_batch(&world, batch, graph);
        assert_safe(&world, intent, graph);
    }
    for batch in &actual.creates {
        for action in batch {
            let (next, created) = apply_create(&world, action, &assigned[&action.id], graph);
            assert!(created, "stable exact observations never duplicate create");
            world = next;
            assert_safe(&world, intent, graph);
        }
    }
    for batch in &actual.markers {
        world = apply_marker_batch(&world, batch, graph);
        assert_safe(&world, intent, graph);
    }
    for batch in &actual.updates {
        for action in batch {
            world = apply_update(&world, intent, action, graph);
            assert_safe(&world, intent, graph);
        }
    }
    world
}

#[derive(Clone, Copy)]
enum ConvergenceClass {
    Primary,
    MarkerlessHistory,
}

fn require_stable_convergence(
    mut world: World,
    case: &Case,
    harness: &ProductionHarness,
    coverage: &mut Coverage,
    label: &str,
    class: ConvergenceClass,
) {
    let context = ComparisonContext::for_case(case, harness);
    for attempt in 0..8 {
        assert_safe(&world, &case.intent, &case.graph);
        let open = exact_open_view(&world);
        let terminal = exact_terminal_view(&world, &case.intent, &open);
        let before = deficit(&world, &case.intent);
        let actual = assert_matching_plan(
            &world,
            &open,
            &terminal,
            &context,
            coverage,
            &format!("{label}/stable-{attempt}"),
        )
        .expect("a primary stable view remains valid");
        if actual.logical().is_done() {
            assert_eq!(before, (0, 0, 0, 0));
            match class {
                ConvergenceClass::Primary => coverage.converged_worlds += 1,
                ConvergenceClass::MarkerlessHistory => {
                    coverage.markerless_history_convergences += 1
                }
            }
            return;
        }
        let next = execute_all(world, &actual, &case.intent, &case.graph, &case.assigned);
        let after = deficit(&next, &case.intent);
        assert!(after < before, "{label}: an all-ack path makes strict progress");
        world = next;
    }
    panic!("{label}: stable observations did not converge")
}

fn exercise_omitted_unmarked_attempt(
    world: &World,
    actual: &ActualPlan,
    omitted: &[Id],
    case: &Case,
    coverage: &mut Coverage,
) {
    let before = deficit(world, &case.intent);
    let mut after_git = world.clone();
    for batch in &actual.git {
        after_git = apply_git_batch(&after_git, batch, &case.graph);
    }
    let mut saw_duplicate = false;
    let mut prefix = after_git;
    for batch in &actual.creates {
        for mask in 0..(1_usize << batch.len()) {
            let mut subset = prefix.clone();
            for (index, action) in batch.iter().enumerate() {
                if mask & (1 << index) != 0 {
                    let (next, created) =
                        apply_create(&subset, action, &case.assigned[&action.id], &case.graph);
                    if omitted.contains(&action.id) {
                        assert!(!created);
                        saw_duplicate = true;
                    }
                    subset = next;
                    assert_safe(&subset, &case.intent, &case.graph);
                }
            }
            assert!(deficit(&subset, &case.intent) <= before);
            coverage.omission_alias_masks += 1;
        }
        for action in batch {
            let (next, _) = apply_create(&prefix, action, &case.assigned[&action.id], &case.graph);
            prefix = next;
        }
    }
    assert!(saw_duplicate, "every omitted unmarked OPEN row retries its stable create key");
    coverage.duplicate_create_no_effect += 1;
}

#[derive(Default)]
struct Coverage {
    primary_cases: usize,
    topology_cases: BTreeMap<&'static str, usize>,
    clean_cases: usize,
    stale_cases: usize,
    phase_cells: [usize; 5],
    planned_tuples: usize,
    planned_creates: usize,
    planned_markers: usize,
    planned_updates: usize,
    visibility_prefixes: usize,
    git_lost_ack_outcomes: usize,
    create_alias_masks: usize,
    marker_lost_ack_outcomes: usize,
    update_alias_masks: usize,
    distinct_restart_worlds: usize,
    open_views: usize,
    omitted_unmarked_views: usize,
    omitted_marked_rejections: usize,
    omission_alias_masks: usize,
    duplicate_create_no_effect: usize,
    production_calls: usize,
    matched_plans: usize,
    safe_rejections: usize,
    converged_worlds: usize,
    factor_cases: usize,
    receipt_registry_cases: usize,
    multi_batch_constructed_worlds: usize,
    multi_batch_distinct_worlds: usize,
    multi_batch_replanned_worlds: usize,
    multi_batch_retry_state_replans: usize,
    multi_batch_equivalent_retry_reuses: usize,
    multi_batch_convergences: usize,
    multi_batch_targeted_omission_views: usize,
    observed_field_views: usize,
    identity_boundary_cases: usize,
    lifecycle_transition_cases: usize,
    nonlocal_omission_views: usize,
    markerless_history_views: usize,
    markerless_history_omission_views: usize,
    markerless_history_convergences: usize,
}

impl Coverage {
    fn summary(&self) -> String {
        format!(
            "primary cases: {}\n\
             topology cases: one={}, two-ab={}, two-ba={}, three={}\n\
             presentation cases: clean={}, body-stale={}\n\
             per-phase cells [new, published, unmarked, provisional, final]: {:?}\n\
             initial logical actions: tuples={}, creates={}, markers={}, updates={}\n\
             tuple visibility prefixes: {}\n\
             lost-ack outcomes: initial-git={}, marker-git={}\n\
             alias masks: create={}, final-update={}, omission-create={}\n\
             distinct durable restart worlds: {}\n\
             OPEN observation views: {}\n\
             omitted unmarked views: {}\n\
             omitted marked safe rejections: {}\n\
             duplicate same-key creates with no durable effect: {}\n\
             production comparisons: calls={}, plans={}, safe rejections={}\n\
             stable-view convergences: {}\n\
             separate factor cases: {}\n\
             receipt registry cases: {}\n\
             multi-batch worlds: constructed={}, distinct={}, replanned={}, converged={}\n\
             multi-batch retry states: replanned={}, equivalent-reuses={}\n\
             multi-batch targeted omission views: {}\n\
             separately stale observed-field views: {}\n\
             exact revision-identity boundary cases: {}\n\
             retained lifecycle-transition cases: {}\n\
             omitted validation-only nonlocal views: {}\n\
             markerless multi-version views: {}, omission={}, converged={}",
            self.primary_cases,
            self.topology_cases.get("one").copied().unwrap_or_default(),
            self.topology_cases.get("two-ab").copied().unwrap_or_default(),
            self.topology_cases.get("two-ba").copied().unwrap_or_default(),
            self.topology_cases.get("three").copied().unwrap_or_default(),
            self.clean_cases,
            self.stale_cases,
            self.phase_cells,
            self.planned_tuples,
            self.planned_creates,
            self.planned_markers,
            self.planned_updates,
            self.visibility_prefixes,
            self.git_lost_ack_outcomes,
            self.marker_lost_ack_outcomes,
            self.create_alias_masks,
            self.update_alias_masks,
            self.omission_alias_masks,
            self.distinct_restart_worlds,
            self.open_views,
            self.omitted_unmarked_views,
            self.omitted_marked_rejections,
            self.duplicate_create_no_effect,
            self.production_calls,
            self.matched_plans,
            self.safe_rejections,
            self.converged_worlds,
            self.factor_cases,
            self.receipt_registry_cases,
            self.multi_batch_constructed_worlds,
            self.multi_batch_distinct_worlds,
            self.multi_batch_replanned_worlds,
            self.multi_batch_convergences,
            self.multi_batch_retry_state_replans,
            self.multi_batch_equivalent_retry_reuses,
            self.multi_batch_targeted_omission_views,
            self.observed_field_views,
            self.identity_boundary_cases,
            self.lifecycle_transition_cases,
            self.nonlocal_omission_views,
            self.markerless_history_views,
            self.markerless_history_omission_views,
            self.markerless_history_convergences,
        )
    }
}

fn explore_primary_case(case: &Case, coverage: &mut Coverage) {
    assert_safe(&case.world, &case.intent, &case.graph);
    coverage.primary_cases += 1;
    *coverage.topology_cases.entry(case.topology.label()).or_default() += 1;
    match case.presentation {
        Presentation::Clean => coverage.clean_cases += 1,
        Presentation::BodyStale => coverage.stale_cases += 1,
    }
    for phase in &case.phases {
        coverage.phase_cells[phase.index()] += 1;
    }

    let harness = ProductionHarness::new(&case.graph);
    let context = ComparisonContext::for_case(case, &harness);
    let open = exact_open_view(&case.world);
    let terminal = exact_terminal_view(&case.world, &case.intent, &open);
    let actual =
        assert_matching_plan(&case.world, &open, &terminal, &context, coverage, &case.label)
            .expect("every primary exact view is valid");
    assert!(actual.git.len() <= 1);
    assert!(actual.creates.len() <= 1);
    assert!(actual.markers.len() <= 1);
    assert!(actual.updates.len() <= 1);
    let logical = actual.logical();
    for (index, (local, phase)) in case.intent.changes.iter().zip(&case.phases).enumerate() {
        let tuple = logical.git.iter().find(|action| action.id == local.id);
        let expected_tuple = *phase == Phase::New || case.topology == Topology::TwoBa;
        assert_eq!(tuple.is_some(), expected_tuple, "{}: tuple cell {index}", case.label);

        let create = logical.creates.iter().find(|action| action.id == local.id);
        let expected_create = matches!(phase, Phase::New | Phase::PublishedNoPr);
        assert_eq!(create.is_some(), expected_create, "{}: create cell {index}", case.label);
        if let Some(create) = create {
            assert_eq!(create.base_branch, owned_base_name(&local.id));
        }

        let marker = logical.markers.iter().find(|action| action.id == local.id);
        assert_eq!(marker.is_some(), !phase.has_marker(), "{}: marker cell {index}", case.label);
        if let Some(marker) = marker {
            assert_eq!(marker.target, local.proposal);
        }

        let expected_fields = if expected_create {
            FieldMask { title: false, body: true, base: index == 0 }
        } else {
            let DurablePr::Open(open) = &case.world.pull_requests[&local.id] else {
                unreachable!()
            };
            FieldMask {
                title: open.title != TitleState::Final,
                body: open.body != BodyState::Final,
                base: open.base.kind != case.intent.desired_base(index, &case.world).kind,
            }
        };
        let update = logical.updates.iter().find(|action| action.id == local.id);
        assert_eq!(
            update.map(|action| action.fields),
            (expected_fields != FieldMask::default()).then_some(expected_fields),
            "{}: update cell {index}",
            case.label,
        );
    }
    coverage.planned_tuples += logical.git.len();
    coverage.planned_creates += logical.creates.len();
    coverage.planned_markers += logical.markers.len();
    coverage.planned_updates += logical.updates.len();
    assert_visibility_orders(&case.world, &actual.git, &case.intent, &case.graph, coverage);

    let mut restart_worlds =
        git_restart_worlds(&case.world, &actual.git, &case.intent, &case.graph, coverage);
    let mut after_git = case.world.clone();
    for batch in &actual.git {
        after_git = apply_git_batch(&after_git, batch, &case.graph);
    }
    restart_worlds.extend(create_restart_worlds(
        &after_git,
        &actual.creates,
        &case.assigned,
        &case.intent,
        &case.graph,
        coverage,
    ));
    let mut after_creates = after_git;
    for batch in &actual.creates {
        for action in batch {
            let (next, created) =
                apply_create(&after_creates, action, &case.assigned[&action.id], &case.graph);
            assert!(created);
            after_creates = next;
        }
    }
    restart_worlds.extend(marker_restart_worlds(
        &after_creates,
        &actual.markers,
        &case.intent,
        &case.graph,
        coverage,
    ));
    let mut after_markers = after_creates;
    for batch in &actual.markers {
        after_markers = apply_marker_batch(&after_markers, batch, &case.graph);
    }
    restart_worlds.extend(update_restart_worlds(
        &after_markers,
        &actual.updates,
        &case.intent,
        &case.graph,
        coverage,
    ));
    coverage.distinct_restart_worlds += restart_worlds.len();

    for (world_index, world) in restart_worlds.into_iter().enumerate() {
        assert_safe(&world, &case.intent, &case.graph);
        for (view_index, (open, terminal)) in
            observation_views(&world, &case.intent).into_iter().enumerate()
        {
            coverage.open_views += 1;
            let omitted = case
                .intent
                .changes
                .iter()
                .filter(|local| {
                    matches!(world.pull_requests[&local.id], DurablePr::Open(_))
                        && !open.rows.contains_key(&local.id)
                })
                .map(|local| local.id.clone())
                .collect::<Vec<_>>();
            let marked_omitted = omitted.iter().any(|id| world.changes[id].marker_target.is_some());
            let attempt = assert_matching_plan(
                &world,
                &open,
                &terminal,
                &context,
                coverage,
                &format!("{}/world-{world_index}/view-{view_index}", case.label),
            );
            if marked_omitted {
                assert!(attempt.is_none(), "a marked omitted PR fails closed");
                coverage.omitted_marked_rejections += 1;
            } else {
                let attempt = attempt.expect("every legal unmarked view is recoverable");
                if !omitted.is_empty() {
                    coverage.omitted_unmarked_views += 1;
                    exercise_omitted_unmarked_attempt(&world, &attempt, &omitted, case, coverage);
                }
            }
        }
        require_stable_convergence(
            world,
            case,
            &harness,
            coverage,
            &format!("{}/world-{world_index}", case.label),
            ConvergenceClass::Primary,
        );
    }
}

fn refresh_projection(case: &mut Case) {
    case.assigned = assigned_identities(&case.world, &case.intent);
    case.bodies = evidence_bodies(&case.world, &case.intent, &case.graph, &case.assigned);
}

fn compare_factor(
    case: &Case,
    coverage: &mut Coverage,
    label: &str,
    expect_reject: bool,
) -> Option<ActualPlan> {
    coverage.factor_cases += 1;
    let open = exact_open_view(&case.world);
    let terminal = exact_terminal_view(&case.world, &case.intent, &open);
    let harness = ProductionHarness::new(&case.graph);
    let context = ComparisonContext::for_case(case, &harness);
    let result = assert_matching_plan(
        &case.world,
        &open,
        &terminal,
        &context,
        coverage,
        &format!("factor/{label}"),
    );
    assert_eq!(result.is_none(), expect_reject, "factor/{label}");
    if let Some(actual) = &result {
        let final_world =
            execute_all(case.world.clone(), actual, &case.intent, &case.graph, &case.assigned);
        assert_eq!(deficit(&final_world, &case.intent), (0, 0, 0, 0));
    }
    result
}

fn history_factor(mode: usize) -> Case {
    let mut case = main_case(Topology::One, vec![Phase::MarkedFinal], Presentation::Clean);
    let id = case.intent.changes[0].id.clone();
    let default = case.world.default_tip;
    let current = case.world.changes[&id].versions[0];
    match mode {
        0 => {}
        1 => {
            let amend = Commit(120);
            case.graph.insert(amend, [default], [id.clone()]);
            case.intent.changes[0].proposal = amend;
        }
        2 => {
            let new_base = Commit(3);
            let amend = Commit(120);
            let rebase = Commit(121);
            case.graph.insert(new_base, [default], []);
            case.graph.insert(amend, [new_base], [id.clone()]);
            case.graph.insert(rebase, [default], [id.clone()]);
            let remote = case.world.changes.get_mut(&id).unwrap();
            remote.versions.push(Revision { head: amend, first_parent: new_base });
            remote.head_ref = Some(amend);
            remote.owned_base_ref = Some(new_base);
            case.intent.changes[0].proposal = rebase;
        }
        3 => {
            let middle = Commit(120);
            case.graph.insert(middle, [default], [id.clone()]);
            let remote = case.world.changes.get_mut(&id).unwrap();
            remote.versions =
                vec![current, Revision { head: middle, first_parent: default }, current];
            remote.head_ref = Some(current.head);
            remote.owned_base_ref = Some(current.first_parent);
            remote.marker_target = Some(middle);
        }
        _ => unreachable!(),
    }
    refresh_projection(&mut case);
    case
}

fn exercise_history_factors(coverage: &mut Coverage) {
    for (mode, label) in ["current", "amend", "rebase", "a-b-a"].into_iter().enumerate() {
        let case = history_factor(mode);
        assert_safe(&case.world, &case.intent, &case.graph);
        let actual = compare_factor(&case, coverage, &format!("history-{label}"), false).unwrap();
        assert_eq!(!actual.git.is_empty(), matches!(mode, 1 | 2));
        if mode != 0 {
            assert!(case.world.changes[&case.intent.changes[0].id].marker_target.is_some());
        }
    }

    for mode in 0..3 {
        let mut case = history_factor(2);
        let id = case.intent.changes[0].id.clone();
        let remote = case.world.changes.get_mut(&id).unwrap();
        match mode {
            0 => {
                remote.versions[1] = remote.versions[0];
                remote.head_ref = Some(remote.versions[1].head);
                remote.owned_base_ref = Some(remote.versions[1].first_parent);
            }
            1 => remote.head_ref = Some(remote.versions[0].head),
            2 => remote.owned_base_ref = Some(Commit(2)),
            _ => unreachable!(),
        }
        compare_factor(&case, coverage, &format!("history-invalid-{mode}"), true);
    }
    let mut marker_without_history =
        main_case(Topology::One, vec![Phase::New], Presentation::Clean);
    let id = marker_without_history.intent.changes[0].id.clone();
    marker_without_history.world.changes.get_mut(&id).unwrap().marker_target =
        Some(marker_without_history.intent.changes[0].proposal);
    compare_factor(&marker_without_history, coverage, "marker-without-history", true);
}

fn exercise_revision_identity_boundary_factors(coverage: &mut Coverage) {
    for mode in 0..5 {
        let mut case = main_case(Topology::One, vec![Phase::MarkedFinal], Presentation::Clean);
        let id = case.intent.changes[0].id.clone();
        let head = case.intent.changes[0].proposal;
        let ancestor = case.graph.first_parent(head).unwrap();
        let wrong = Id::new("Gz");
        let (ancestor_identities, head_identities, label) = match mode {
            // The ancestry-wide count is exactly one in the first two cases,
            // but the revision head itself has no exact identity claim.
            0 => (vec![id.clone()], Vec::new(), "head-missing-ancestor-matches"),
            1 => (vec![id.clone()], vec![wrong.clone()], "head-wrong-ancestor-matches"),
            // These cases make the exact-head and ancestry-wide predicates
            // fail in different combinations, including literal duplicates.
            2 => (Vec::new(), vec![id.clone(), wrong], "head-extra-ancestry-count-exact"),
            3 => (Vec::new(), vec![id.clone(), id.clone()], "head-duplicate"),
            4 => (vec![id.clone()], vec![id.clone()], "head-exact-ancestor-duplicate"),
            _ => unreachable!(),
        };
        case.graph.nodes.get_mut(&ancestor).unwrap().identities = ancestor_identities;
        case.graph.nodes.get_mut(&head).unwrap().identities = head_identities;
        compare_factor(&case, coverage, &format!("revision-identity-{label}"), true);
        coverage.identity_boundary_cases += 1;
    }
}

fn exercise_update_mask_factors(coverage: &mut Coverage) {
    for encoded in 0_u8..8 {
        let mut case = main_case(Topology::One, vec![Phase::MarkedFinal], Presentation::Clean);
        let id = case.intent.changes[0].id.clone();
        let DurablePr::Open(open) = case.world.pull_requests.get_mut(&id).unwrap() else {
            unreachable!()
        };
        if encoded & 1 != 0 {
            open.title = TitleState::Stale(encoded);
        }
        if encoded & 2 != 0 {
            open.body = BodyState::Stale(encoded);
        }
        if encoded & 4 != 0 {
            open.base = Base {
                kind: BaseKind::Owned,
                oid: case.world.changes[&id].versions[0].first_parent,
            };
        }
        assert_safe(&case.world, &case.intent, &case.graph);
        let actual =
            compare_factor(&case, coverage, &format!("update-mask-{encoded}"), false).unwrap();
        let expected =
            FieldMask { title: encoded & 1 != 0, body: encoded & 2 != 0, base: encoded & 4 != 0 };
        assert_eq!(
            actual.logical().updates.first().map(|action| action.fields),
            (expected != FieldMask::default()).then_some(expected),
        );
    }
}

fn exercise_stale_observation_factors(coverage: &mut Coverage) {
    let mut case = history_factor(2);
    let open = exact_open_view(&case.world);
    let terminal = exact_terminal_view(&case.world, &case.intent, &open);
    let harness = ProductionHarness::new(&case.graph);
    let bootstrap = {
        let context = ComparisonContext::for_case(&case, &harness);
        assert_matching_plan(
            &case.world,
            &open,
            &terminal,
            &context,
            coverage,
            "factor/stale-observation-bootstrap",
        )
        .unwrap()
    };
    case.world = execute_all(case.world, &bootstrap, &case.intent, &case.graph, &case.assigned);
    assert_eq!(deficit(&case.world, &case.intent), (0, 0, 0, 0));
    let id = case.intent.changes[0].id.clone();
    let remote = &case.world.changes[&id];
    let heads = remote.versions.iter().map(|revision| revision.head).collect::<Vec<_>>();
    let bases = [
        Base { kind: BaseKind::Default, oid: case.world.default_tip },
        Base { kind: BaseKind::Owned, oid: remote.versions[0].first_parent },
        Base { kind: BaseKind::Owned, oid: remote.versions[1].first_parent },
    ];
    assert_eq!(heads.len(), 3);
    assert_ne!(bases[1], bases[2]);
    let context = ComparisonContext::for_case(&case, &harness);
    for (head_index, head) in heads.into_iter().enumerate() {
        for (base_index, base) in bases.into_iter().enumerate() {
            for stale_title in [false, true] {
                for stale_body in [false, true] {
                    let mut open = exact_open_view(&case.world);
                    let row = open.rows.get_mut(&id).unwrap();
                    row.head_oid = head;
                    row.base = base;
                    row.title =
                        if stale_title { TitleState::Stale(210) } else { TitleState::Final };
                    row.body = if stale_body { BodyState::Stale(210) } else { BodyState::Final };
                    let terminal = exact_terminal_view(&case.world, &case.intent, &open);
                    let actual = assert_matching_plan(
                        &case.world,
                        &open,
                        &terminal,
                        &context,
                        coverage,
                        &format!(
                            "factor/stale-observation-{head_index}-{base_index}-{stale_title}-{stale_body}"
                        ),
                    )
                    .unwrap();
                    let expected = FieldMask {
                        title: stale_title,
                        body: stale_body,
                        base: base.kind == BaseKind::Owned,
                    };
                    assert_eq!(
                        actual.logical().updates.first().map(|update| update.fields),
                        (expected != FieldMask::default()).then_some(expected),
                    );
                    let applied = execute_all(
                        case.world.clone(),
                        &actual,
                        &case.intent,
                        &case.graph,
                        &case.assigned,
                    );
                    assert_eq!(deficit(&applied, &case.intent), (0, 0, 0, 0));
                    coverage.observed_field_views += 1;
                    coverage.factor_cases += 1;
                }
            }
        }
    }
}

fn exercise_markerless_multi_version_observations(coverage: &mut Coverage) {
    let mut case = oid_grid_case(2, 2);
    case.label = "markerless-multi-version".to_owned();
    let id = case.intent.changes[1].id.clone();
    case.world.changes.get_mut(&id).unwrap().marker_target = None;
    let remote = &case.world.changes[&id];
    let heads = remote.versions.iter().map(|revision| revision.head).collect::<Vec<_>>();
    let bases = remote
        .versions
        .iter()
        .map(|revision| Base { kind: BaseKind::Owned, oid: revision.first_parent })
        .collect::<Vec<_>>();
    assert_eq!(heads.len(), 3);
    assert_eq!(bases.len(), 3);
    assert_eq!(heads.iter().copied().collect::<BTreeSet<_>>().len(), heads.len());
    assert_eq!(bases.iter().copied().collect::<BTreeSet<_>>().len(), bases.len());
    assert_safe(&case.world, &case.intent, &case.graph);

    let harness = ProductionHarness::new(&case.graph);
    let context = ComparisonContext::for_case(&case, &harness);
    for (head_index, head) in heads.into_iter().enumerate() {
        for (base_index, base) in bases.iter().copied().enumerate() {
            let mut open = exact_open_view(&case.world);
            let row = open.rows.get_mut(&id).unwrap();
            row.head_oid = head;
            row.base = base;
            let terminal = exact_terminal_view(&case.world, &case.intent, &open);
            let actual = assert_matching_plan(
                &case.world,
                &open,
                &terminal,
                &context,
                coverage,
                &format!(
                    "factor/markerless-multi-version/head-{head_index}/base-{base_index}/view-exact"
                ),
            )
            .expect("every validated historical head/owned-base pair is recoverable");
            let logical = actual.logical();
            assert_eq!(logical.git.len(), 1);
            assert_ne!(logical.git[0].id, id);
            assert!(logical.creates.is_empty());
            assert_eq!(
                logical.markers,
                vec![MarkerAction {
                    id: id.clone(),
                    target: case.world.changes[&id].versions.last().unwrap().head,
                }]
            );
            assert!(logical.updates.is_empty());
            coverage.markerless_history_views += 1;
            coverage.factor_cases += 1;
        }
    }

    // Row omission is orthogonal to which validated historical OIDs a stale
    // row exposed: after omission only the durable marker bit and stable ref
    // names remain observable. Exercise it once at the current pair, then let
    // exact visibility recover through the marker acknowledgement barrier.
    let mut open = exact_open_view(&case.world);
    assert!(open.rows.remove(&id).is_some());
    let terminal = exact_terminal_view(&case.world, &case.intent, &open);
    let omitted = assert_matching_plan(
        &case.world,
        &open,
        &terminal,
        &context,
        coverage,
        "factor/markerless-multi-version/view-omitted",
    )
    .expect("an omitted markerless OPEN row retains stable-key create authority");
    assert!(omitted.logical().creates.iter().any(|create| create.id == id));
    exercise_omitted_unmarked_attempt(
        &case.world,
        &omitted,
        std::slice::from_ref(&id),
        &case,
        coverage,
    );
    coverage.markerless_history_omission_views += 1;
    coverage.factor_cases += 1;
    require_stable_convergence(
        case.world.clone(),
        &case,
        &harness,
        coverage,
        "factor/markerless-multi-version/recovery",
        ConvergenceClass::MarkerlessHistory,
    );
}

fn exercise_marker_only_factor(coverage: &mut Coverage) {
    let mut case = main_case(
        Topology::TwoAb,
        vec![Phase::MarkedFinal, Phase::UnmarkedOwnedOpen],
        Presentation::Clean,
    );
    let child = case.intent.changes[1].id.clone();
    let DurablePr::Open(open) = case.world.pull_requests.get_mut(&child).unwrap() else {
        unreachable!()
    };
    open.body = BodyState::Final;
    let actual = compare_factor(&case, coverage, "marker-only-no-action", false).unwrap();
    assert!(actual.git.is_empty());
    assert!(actual.creates.is_empty());
    assert_eq!(actual.markers.iter().flatten().count(), 1);
    assert!(actual.updates.is_empty());
    assert!(actual.marker_gate);
}

fn exercise_automation_factors(coverage: &mut Coverage) {
    for desired_owned in [false, true] {
        for observed_owned in [false, true] {
            for landing in [Landing::None, Landing::AutoMerge, Landing::MergeQueue] {
                let (topology, phases, index) = if desired_owned {
                    (Topology::TwoAb, vec![Phase::MarkedFinal; 2], 1)
                } else {
                    (Topology::One, vec![Phase::MarkedFinal], 0)
                };
                let mut case = main_case(topology, phases, Presentation::Clean);
                let id = case.intent.changes[index].id.clone();
                let remote = &case.world.changes[&id];
                let DurablePr::Open(open) = case.world.pull_requests.get_mut(&id).unwrap() else {
                    unreachable!()
                };
                open.base = if observed_owned {
                    Base { kind: BaseKind::Owned, oid: remote.versions[0].first_parent }
                } else {
                    Base { kind: BaseKind::Default, oid: case.world.default_tip }
                };
                open.landing = landing;
                let reject = landing != Landing::None && (observed_owned || desired_owned);
                compare_factor(
                    &case,
                    coverage,
                    &format!(
                        "automation-{}-{}-{landing:?}",
                        if desired_owned { "desired-owned" } else { "desired-default" },
                        if observed_owned { "observed-owned" } else { "observed-default" },
                    ),
                    reject,
                );
            }
        }
    }
}

fn exercise_terminal_factors(coverage: &mut Coverage) {
    for index in 0..3 {
        for state in [TerminalState::Closed, TerminalState::Merged] {
            for marked in [false, true] {
                let mut case =
                    main_case(Topology::Three, vec![Phase::PublishedNoPr; 3], Presentation::Clean);
                let id = case.intent.changes[index].id.clone();
                if marked {
                    let head = case.world.changes[&id].versions[0].head;
                    case.world.changes.get_mut(&id).unwrap().marker_target = Some(head);
                }
                case.world.pull_requests.insert(
                    id,
                    DurablePr::Retired {
                        identity: Identity {
                            number: 700 + index as u64,
                            node_id: format!("PR_TERMINAL_{index}_{state:?}_{marked}"),
                        },
                        state,
                    },
                );
                case.assigned = assigned_identities(&case.world, &case.intent);
                assert_safe(&case.world, &case.intent, &case.graph);
                compare_factor(
                    &case,
                    coverage,
                    &format!("terminal-{index}-{state:?}-marked-{marked}"),
                    true,
                );
            }
        }
    }
}

fn exercise_absorbing_lifecycle_transition(coverage: &mut Coverage) {
    let mut case = main_case(Topology::One, vec![Phase::MarkedFinal], Presentation::Clean);
    let id = case.intent.changes[0].id.clone();
    let head = case.world.changes[&id].versions[0].head;
    let unsafe_base = Commit(170);
    case.graph.insert(unsafe_base, [head], []);

    // This deliberately models an external unsafe ref visibility transition,
    // not a valid GHerrit effect. Once the head is reachable from the base,
    // GitHub's lifecycle transition is retained as durable Merged state.
    let DurablePr::Open(open) = case.world.pull_requests.get_mut(&id).unwrap() else {
        unreachable!()
    };
    open.base = Base { kind: BaseKind::Owned, oid: unsafe_base };
    let merged = settle_pull_request_lifecycle(&case.world, &case.graph);
    let identity = match &merged.pull_requests[&id] {
        DurablePr::Retired { identity, state: TerminalState::Merged } => identity.clone(),
        state => panic!("unsafe visibility did not durably merge the pull request: {state:?}"),
    };
    assert_eq!(
        settle_pull_request_lifecycle(&merged, &case.graph),
        merged,
        "Merged is an absorbing lifecycle state",
    );

    // Settlement discards the unsafe transient base value but retains the
    // identity and lifecycle. A fresh planner therefore sees terminal
    // evidence and cannot construct a create or update which reopens it.
    case.world = merged;
    case.assigned = assigned_identities(&case.world, &case.intent);
    assert_eq!(
        case.assigned[&id], identity,
        "retirement retains the original pull request identity",
    );
    assert_safe(&case.world, &case.intent, &case.graph);
    let open = exact_open_view(&case.world);
    let terminal = exact_terminal_view(&case.world, &case.intent, &open);
    assert!(matches!(
        oracle_plan(&case.world, &case.intent, &open, &terminal, &case.graph),
        OracleOutcome::Reject,
    ));
    coverage.production_calls += 1;
    let ProductionOutcome::Reject(error) = production_plan(
        &case.world,
        &case.intent,
        &open,
        &terminal,
        &case.bodies,
        &case.assigned,
        &ProductionHarness::new(&case.graph),
    ) else {
        panic!("a fresh planner reopened a durably merged identity")
    };
    assert!(error.contains("Cannot push to merged PR #10"), "{error}");
    coverage.safe_rejections += 1;
    coverage.factor_cases += 1;
    coverage.lifecycle_transition_cases += 1;
}

fn oid_grid_case(head_index: usize, base_index: usize) -> Case {
    let mut case = main_case(Topology::TwoAb, vec![Phase::MarkedFinal; 2], Presentation::Clean);
    let id = case.intent.changes[1].id.clone();
    let first = case.world.changes[&id].versions[0];
    let default = case.world.default_tip;
    let base_two = Commit(2);
    let base_three = Commit(3);
    let head_two = Commit(130);
    let head_three = Commit(131);
    let root_id = case.intent.changes[0].id.clone();
    case.graph.insert(base_three, [default], [root_id]);
    case.graph.insert(head_two, [base_two], [id.clone()]);
    case.graph.insert(head_three, [base_three], [id.clone()]);
    let revisions = [
        first,
        Revision { head: head_two, first_parent: base_two },
        Revision { head: head_three, first_parent: base_three },
    ];
    let remote = case.world.changes.get_mut(&id).unwrap();
    remote.versions = revisions.to_vec();
    remote.head_ref = Some(head_three);
    remote.owned_base_ref = Some(base_three);
    remote.marker_target = Some(first.head);
    case.intent.changes[0].proposal = base_three;
    case.intent.changes[1].proposal = head_three;
    let DurablePr::Open(open) = case.world.pull_requests.get_mut(&id).unwrap() else {
        unreachable!()
    };
    open.head_oid = revisions[head_index].head;
    open.base = Base { kind: BaseKind::Owned, oid: revisions[base_index].first_parent };
    refresh_projection(&mut case);
    case
}

fn exercise_oid_factors(coverage: &mut Coverage) {
    for head in 0..3 {
        for base in 0..3 {
            let case = oid_grid_case(head, base);
            assert_safe(&case.world, &case.intent, &case.graph);
            compare_factor(&case, coverage, &format!("published-oid-{head}-{base}"), false);
        }
    }

    for mode in 0..6 {
        let mut case = if mode == 0 {
            history_factor(1)
        } else {
            main_case(Topology::One, vec![Phase::MarkedFinal], Presentation::Clean)
        };
        let id = case.intent.changes[0].id.clone();
        match mode {
            0 => {
                let proposal = case.intent.changes[0].proposal;
                let DurablePr::Open(open) = case.world.pull_requests.get_mut(&id).unwrap() else {
                    unreachable!()
                };
                open.head_oid = proposal;
            }
            1 => {
                let DurablePr::Open(open) = case.world.pull_requests.get_mut(&id).unwrap() else {
                    unreachable!()
                };
                open.head_oid = Commit(2);
            }
            2 => {
                let wrong = Commit(150);
                case.graph.insert(wrong, [case.world.default_tip], [Id::new("Gz")]);
                let revision = Revision { head: wrong, first_parent: case.world.default_tip };
                let remote = case.world.changes.get_mut(&id).unwrap();
                remote.versions = vec![revision];
                remote.head_ref = Some(wrong);
                remote.owned_base_ref = Some(case.world.default_tip);
                remote.marker_target = Some(wrong);
                case.intent.changes[0].proposal = wrong;
                let DurablePr::Open(open) = case.world.pull_requests.get_mut(&id).unwrap() else {
                    unreachable!()
                };
                open.head_oid = wrong;
            }
            3 => {
                let DurablePr::Open(open) = case.world.pull_requests.get_mut(&id).unwrap() else {
                    unreachable!()
                };
                open.base = Base { kind: BaseKind::Default, oid: Commit(2) };
            }
            4 => {
                let DurablePr::Open(open) = case.world.pull_requests.get_mut(&id).unwrap() else {
                    unreachable!()
                };
                open.base = Base { kind: BaseKind::Owned, oid: Commit(2) };
            }
            5 => case.world.changes.get_mut(&id).unwrap().marker_target = Some(Commit(2)),
            _ => unreachable!(),
        }
        compare_factor(&case, coverage, &format!("invalid-oid-{mode}"), true);
    }
}

fn exercise_nonlocal_factors(coverage: &mut Coverage) {
    for mode in 0..5 {
        let mut case = main_case(Topology::One, vec![Phase::MarkedFinal], Presentation::Clean);
        let id = Id::new("Gz");
        let head = Commit(160);
        case.graph.insert(head, [case.world.default_tip], [id.clone()]);
        let revision = Revision { head, first_parent: case.world.default_tip };
        let (remote, base, landing) = match mode {
            0 => (
                RemoteChange {
                    head_ref: Some(head),
                    owned_base_ref: Some(case.world.default_tip),
                    versions: vec![revision],
                    marker_target: Some(head),
                },
                Base { kind: BaseKind::Default, oid: case.world.default_tip },
                Landing::None,
            ),
            1 => (
                RemoteChange {
                    head_ref: Some(head),
                    owned_base_ref: Some(case.world.default_tip),
                    versions: vec![revision],
                    marker_target: None,
                },
                Base { kind: BaseKind::Owned, oid: case.world.default_tip },
                Landing::None,
            ),
            2 => (
                RemoteChange {
                    head_ref: Some(head),
                    owned_base_ref: Some(case.world.default_tip),
                    versions: vec![revision],
                    marker_target: Some(head),
                },
                Base { kind: BaseKind::Owned, oid: case.world.default_tip },
                Landing::AutoMerge,
            ),
            3 => (
                RemoteChange {
                    head_ref: None,
                    owned_base_ref: None,
                    versions: Vec::new(),
                    marker_target: None,
                },
                Base { kind: BaseKind::Owned, oid: case.world.default_tip },
                Landing::None,
            ),
            4 => (
                RemoteChange {
                    head_ref: Some(head),
                    owned_base_ref: Some(case.world.default_tip),
                    versions: vec![revision],
                    marker_target: Some(head),
                },
                Base { kind: BaseKind::Default, oid: Commit(2) },
                Landing::None,
            ),
            _ => unreachable!(),
        };
        case.world.changes.insert(id.clone(), remote);
        case.world.pull_requests.insert(
            id,
            DurablePr::Open(OpenPr {
                identity: Identity { number: 800, node_id: format!("PR_NONLOCAL_{mode}") },
                head_oid: head,
                base,
                title: TitleState::Final,
                body: BodyState::Final,
                landing,
            }),
        );
        if mode < 2 {
            assert_safe(&case.world, &case.intent, &case.graph);
        }
        compare_factor(&case, coverage, &format!("nonlocal-{mode}"), mode >= 2);
    }

    let mut case = main_case(Topology::One, vec![Phase::MarkedFinal], Presentation::Clean);
    let id = Id::new("Gz");
    let head = Commit(160);
    case.graph.insert(head, [case.world.default_tip], [id.clone()]);
    case.world.changes.insert(
        id.clone(),
        RemoteChange {
            head_ref: Some(head),
            owned_base_ref: Some(case.world.default_tip),
            versions: vec![Revision { head, first_parent: case.world.default_tip }],
            marker_target: None,
        },
    );
    case.world.pull_requests.insert(
        id.clone(),
        DurablePr::Open(OpenPr {
            identity: Identity { number: 800, node_id: "PR_NONLOCAL_OMITTED".to_owned() },
            head_oid: head,
            base: Base { kind: BaseKind::Owned, oid: case.world.default_tip },
            title: TitleState::Final,
            body: BodyState::Final,
            landing: Landing::None,
        }),
    );
    assert_safe(&case.world, &case.intent, &case.graph);
    assert!(case.world.changes[&id].head_ref.is_some());
    assert!(case.world.changes[&id].owned_base_ref.is_some());
    let mut open = exact_open_view(&case.world);
    assert!(open.rows.remove(&id).is_some());
    let terminal = exact_terminal_view(&case.world, &case.intent, &open);
    let harness = ProductionHarness::new(&case.graph);
    let context = ComparisonContext::for_case(&case, &harness);
    let actual = assert_matching_plan(
        &case.world,
        &open,
        &terminal,
        &context,
        coverage,
        "factor/nonlocal-validation-only-omitted/view-exact-local",
    )
    .expect("an omitted validation-only nonlocal row leaves safe inactive refs");
    assert!(actual.logical().is_done());
    coverage.nonlocal_omission_views += 1;
    coverage.factor_cases += 1;
}

fn exercise_receipt_registry_factors(coverage: &mut Coverage) {
    let one = main_case(Topology::One, vec![Phase::New], Presentation::Clean);
    let open = exact_open_view(&one.world);
    let terminal = exact_terminal_view(&one.world, &one.intent, &open);
    assert!(matches!(
        oracle_plan(&one.world, &one.intent, &open, &terminal, &one.graph),
        OracleOutcome::Plan(_)
    ));
    for (label, identity) in [
        ("initial-number", Identity { number: 900, node_id: "PR_RECEIPT_NEW".to_owned() }),
        ("initial-node", Identity { number: 777, node_id: "PR_FORK".to_owned() }),
    ] {
        let mut assigned = one.assigned.clone();
        assigned.insert(one.intent.changes[0].id.clone(), identity);
        let ProductionOutcome::Reject(error) = production_plan(
            &one.world,
            &one.intent,
            &open,
            &terminal,
            &one.bodies,
            &assigned,
            &ProductionHarness::new(&one.graph),
        ) else {
            panic!("receipt collision {label} was accepted")
        };
        assert!(error.contains("repeats initial OPEN pull request"));
        coverage.receipt_registry_cases += 1;
        coverage.factor_cases += 1;
    }

    let two = main_case(Topology::TwoAb, vec![Phase::New; 2], Presentation::Clean);
    let open = exact_open_view(&two.world);
    let terminal = exact_terminal_view(&two.world, &two.intent, &open);
    let duplicate = Identity { number: 777, node_id: "PR_RECEIPT_DUPLICATE".to_owned() };
    let assigned =
        two.intent.changes.iter().map(|local| (local.id.clone(), duplicate.clone())).collect();
    let ProductionOutcome::Reject(error) = production_plan(
        &two.world,
        &two.intent,
        &open,
        &terminal,
        &two.bodies,
        &assigned,
        &ProductionHarness::new(&two.graph),
    ) else {
        panic!("duplicate create receipts were accepted")
    };
    assert!(error.contains("repeats created pull request number"), "{error}");
    coverage.receipt_registry_cases += 1;
    coverage.factor_cases += 1;
}

fn custom_new_case(ids: Vec<Id>, body_bytes: usize, body_byte: char) -> Case {
    assert!(body_byte.len_utf8() == 1);
    let default_tip = Commit(1);
    let mut graph = Graph::new(default_tip);
    graph.insert(Commit(2), [default_tip], []);
    let mut parent = default_tip;
    let mut changes = BTreeMap::new();
    let mut pull_requests = BTreeMap::new();
    let mut locals = Vec::new();
    for (index, id) in ids.iter().enumerate() {
        let proposal = Commit(200 + index as u32);
        graph.insert(proposal, [parent], [id.clone()]);
        changes.insert(
            id.clone(),
            RemoteChange {
                head_ref: None,
                owned_base_ref: None,
                versions: Vec::new(),
                marker_target: None,
            },
        );
        pull_requests.insert(id.clone(), DurablePr::Absent);
        locals.push(LocalIntent {
            id: id.clone(),
            proposal,
            title: format!("Batch {index}"),
            body: format!("{index}{}", body_byte.to_string().repeat(body_bytes)),
        });
        parent = proposal;
    }
    let intent = Intent { changes: locals };
    let world = World {
        default_tip,
        changes,
        pull_requests,
        other_open_identities: [
            Identity { number: 900, node_id: "PR_UNMANAGED".to_owned() },
            Identity { number: 901, node_id: "PR_FORK".to_owned() },
        ]
        .into_iter()
        .collect(),
    };
    let assigned = assigned_identities(&world, &intent);
    let bodies = evidence_bodies(&world, &intent, &graph, &assigned);
    Case {
        label: "batch-factor".to_owned(),
        topology: Topology::Three,
        phases: vec![Phase::New; ids.len()],
        presentation: Presentation::Clean,
        graph,
        intent,
        world,
        assigned,
        bodies,
    }
}

fn record_multi_batch_stage(
    catalogue: &mut BTreeMap<World, String>,
    stage: &str,
    worlds: BTreeSet<World>,
    coverage: &mut Coverage,
) {
    coverage.multi_batch_constructed_worlds += worlds.len();
    for (index, world) in worlds.into_iter().enumerate() {
        catalogue.entry(world).or_insert_with(|| format!("{stage}/world-{index}"));
    }
}

fn require_multi_batch_convergence(
    mut world: World,
    mut actual: ActualPlan,
    case: &Case,
    harness: &ProductionHarness,
    coverage: &mut Coverage,
    label: &str,
    retry_cache: &mut BTreeMap<World, ActualPlan>,
) {
    let context = ComparisonContext::for_case(case, harness);
    for attempt in 0..8 {
        assert_safe(&world, &case.intent, &case.graph);
        let before = deficit(&world, &case.intent);
        if actual.logical().is_done() {
            assert_eq!(before, (0, 0, 0, 0));
            coverage.multi_batch_convergences += 1;
            return;
        }
        let next = execute_all(world, &actual, &case.intent, &case.graph, &case.assigned);
        let after = deficit(&next, &case.intent);
        assert!(after < before, "{label}: an all-ack path makes strict progress");
        world = next;

        // Every starting batch-prefix world receives its own production
        // comparison. Later retries often reach a byte-for-byte identical
        // durable world in this same fixed intent/graph/body context. Planning
        // that equality class once is sufficient: exact OPEN and terminal
        // views are pure functions of the World key, and the independent
        // oracle is still re-evaluated at every cache hit below.
        actual = if let Some((cached_world, cached)) = retry_cache.get_key_value(&world) {
            assert_eq!(cached_world, &world, "retry reuse requires exact durable-world equality");
            let open = exact_open_view(&world);
            let terminal = exact_terminal_view(&world, &case.intent, &open);
            let OracleOutcome::Plan(expected) =
                oracle_plan(&world, &case.intent, &open, &terminal, &case.graph)
            else {
                panic!("{label}/stable-{}: cached retry state became invalid", attempt + 1)
            };
            assert_eq!(
                cached.logical().without_raw_bodies(),
                expected.without_raw_bodies(),
                "{label}/stable-{}: equivalent retry changed semantic plan",
                attempt + 1,
            );
            coverage.multi_batch_equivalent_retry_reuses += 1;
            cached.clone()
        } else {
            let open = exact_open_view(&world);
            let terminal = exact_terminal_view(&world, &case.intent, &open);
            let planned = assert_matching_plan(
                &world,
                &open,
                &terminal,
                &context,
                coverage,
                &format!("{label}/stable-{}", attempt + 1),
            )
            .expect("a stable multi-batch retry remains valid");
            coverage.multi_batch_retry_state_replans += 1;
            retry_cache.insert(world.clone(), planned.clone());
            planned
        };
    }
    panic!("{label}: stable observations did not converge")
}

fn verify_multi_batch_restarts(
    case: &Case,
    catalogue: BTreeMap<World, String>,
    coverage: &mut Coverage,
) {
    let harness = ProductionHarness::new(&case.graph);
    let context = ComparisonContext::for_case(case, &harness);
    let mut retry_cache = BTreeMap::new();
    let catalogue = catalogue.into_iter().collect::<Vec<_>>();
    coverage.multi_batch_distinct_worlds += catalogue.len();

    // These comparisons share one immutable case but no durable state or
    // counter. Running them in parallel changes only wall time; indexed
    // collection and the sequential fold below retain deterministic labels,
    // plans, counters, retry ordering, and snapshot output.
    let compared = catalogue
        .par_iter()
        .map(|(world, label)| {
            assert_safe(world, &case.intent, &case.graph);
            let open = exact_open_view(world);
            let terminal = exact_terminal_view(world, &case.intent, &open);
            let mut local_coverage = Coverage::default();
            let actual = assert_matching_plan(
                world,
                &open,
                &terminal,
                &context,
                &mut local_coverage,
                &format!("multi-batch/{label}/view-exact"),
            )
            .expect("every exact durable multi-batch prefix is recoverable");
            assert_eq!(local_coverage.production_calls, 1);
            assert_eq!(local_coverage.matched_plans, 1);
            assert_eq!(local_coverage.safe_rejections, 0);
            actual
        })
        .collect::<Vec<_>>();
    coverage.production_calls += compared.len();
    coverage.matched_plans += compared.len();

    for ((world, label), actual) in catalogue.into_iter().zip(compared) {
        coverage.multi_batch_replanned_worlds += 1;
        if let Some(previous) = retry_cache.insert(world.clone(), actual.clone()) {
            assert_eq!(
                previous.logical().without_raw_bodies(),
                actual.logical().without_raw_bodies(),
                "multi-batch/{label}: exact-world replanning must be deterministic",
            );
        }
        require_multi_batch_convergence(
            world,
            actual,
            case,
            &harness,
            coverage,
            &format!("multi-batch/{label}/convergence"),
            &mut retry_cache,
        );
    }
}

fn exercise_batch_prefix_factors(coverage: &mut Coverage) {
    let long_ids =
        (0..6).map(|index| Id::new(format!("G{}{}", "a".repeat(1_500), index))).collect();
    let long_ref_case = custom_new_case(long_ids, 0, 'x');
    let long_actual =
        compare_factor(&long_ref_case, coverage, "git-marker-batches", false).unwrap();
    assert!(long_actual.git.len() > 1);
    assert!(long_actual.markers.len() > 1);
    let git_worlds = git_restart_worlds(
        &long_ref_case.world,
        &long_actual.git,
        &long_ref_case.intent,
        &long_ref_case.graph,
        coverage,
    );
    let mut after_git = long_ref_case.world.clone();
    for batch in &long_actual.git {
        after_git = apply_git_batch(&after_git, batch, &long_ref_case.graph);
    }
    let mut after_creates = after_git;
    for batch in &long_actual.creates {
        for action in batch {
            let (next, created) = apply_create(
                &after_creates,
                action,
                &long_ref_case.assigned[&action.id],
                &long_ref_case.graph,
            );
            assert!(created);
            after_creates = next;
        }
    }
    let marker_worlds = marker_restart_worlds(
        &after_creates,
        &long_actual.markers,
        &long_ref_case.intent,
        &long_ref_case.graph,
        coverage,
    );
    let mut long_catalogue = BTreeMap::new();
    record_multi_batch_stage(&mut long_catalogue, "git-marker-batches/git", git_worlds, coverage);
    record_multi_batch_stage(
        &mut long_catalogue,
        "git-marker-batches/marker",
        marker_worlds,
        coverage,
    );

    // Test each ID in the first acknowledged marker batch separately. Marker
    // absence is per change, every other OPEN row remains exact, and neither
    // another change's marker nor a version tag participates in correlation.
    // Thus products of simultaneous omissions add no new planner dependency.
    let after_first_marker =
        apply_marker_batch(&after_creates, &long_actual.markers[0], &long_ref_case.graph);
    let long_harness = ProductionHarness::new(&long_ref_case.graph);
    let long_context = ComparisonContext::for_case(&long_ref_case, &long_harness);
    for action in &long_actual.markers[0] {
        assert!(after_first_marker.changes[&action.id].marker_target.is_some());
        let mut open = exact_open_view(&after_first_marker);
        assert!(open.rows.remove(&action.id).is_some());
        let terminal = exact_terminal_view(&after_first_marker, &long_ref_case.intent, &open);
        let attempt = assert_matching_plan(
            &after_first_marker,
            &open,
            &terminal,
            &long_context,
            coverage,
            &format!(
                "multi-batch/git-marker-batches/marker-after-ack/id-{}/view-omitted-marked",
                action.id.as_str(),
            ),
        );
        assert!(attempt.is_none(), "an omitted marked OPEN row fails closed");
        coverage.multi_batch_targeted_omission_views += 1;
    }
    verify_multi_batch_restarts(&long_ref_case, long_catalogue, coverage);

    // U+0001 expands sevenfold across GraphQL and outer-JSON escaping. Nine
    // 17,000-byte bodies therefore retain the exact production byte-limit
    // split (eight operations followed by one) and all 524 durable worlds,
    // while avoiding repeated 120,000-byte body rendering in this semantic
    // proof. Focused request-limit tests own the exact one-MiB boundary.
    let large_body_case = custom_new_case(
        (0..9).map(|index| Id::new(format!("G{index}"))).collect(),
        17_000,
        '\u{1}',
    );
    let large_actual =
        compare_factor(&large_body_case, coverage, "graphql-batches", false).unwrap();
    assert_eq!(large_actual.creates.iter().map(Vec::len).collect::<Vec<_>>(), [8, 1]);
    assert_eq!(large_actual.updates.iter().map(Vec::len).collect::<Vec<_>>(), [8, 1]);
    let mut after_git = large_body_case.world.clone();
    for batch in &large_actual.git {
        after_git = apply_git_batch(&after_git, batch, &large_body_case.graph);
    }
    let create_worlds = create_restart_worlds(
        &after_git,
        &large_actual.creates,
        &large_body_case.assigned,
        &large_body_case.intent,
        &large_body_case.graph,
        coverage,
    );
    let mut after_creates = after_git;
    for batch in &large_actual.creates {
        for action in batch {
            let (next, created) = apply_create(
                &after_creates,
                action,
                &large_body_case.assigned[&action.id],
                &large_body_case.graph,
            );
            assert!(created);
            after_creates = next;
        }
    }
    for batch in &large_actual.markers {
        after_creates = apply_marker_batch(&after_creates, batch, &large_body_case.graph);
    }
    let update_worlds = update_restart_worlds(
        &after_creates,
        &large_actual.updates,
        &large_body_case.intent,
        &large_body_case.graph,
        coverage,
    );
    let mut large_catalogue = BTreeMap::new();
    record_multi_batch_stage(
        &mut large_catalogue,
        "graphql-batches/create",
        create_worlds,
        coverage,
    );
    record_multi_batch_stage(
        &mut large_catalogue,
        "graphql-batches/update",
        update_worlds,
        coverage,
    );

    // A row omitted after the first acknowledged create batch is independent
    // per identity: every retry uses that identity's same permanent head/base
    // names, and all other rows remain exact. Exercise every ID from that
    // batch individually instead of taking an exponential omission product
    // with the nine large bodies.
    let mut after_first_create = large_body_case.world.clone();
    for batch in &large_actual.git {
        after_first_create = apply_git_batch(&after_first_create, batch, &large_body_case.graph);
    }
    for action in &large_actual.creates[0] {
        let (next, created) = apply_create(
            &after_first_create,
            action,
            &large_body_case.assigned[&action.id],
            &large_body_case.graph,
        );
        assert!(created);
        after_first_create = next;
    }
    let large_harness = ProductionHarness::new(&large_body_case.graph);
    let large_context = ComparisonContext::for_case(&large_body_case, &large_harness);
    for action in &large_actual.creates[0] {
        let mut open = exact_open_view(&after_first_create);
        assert!(open.rows.remove(&action.id).is_some());
        let terminal = exact_terminal_view(&after_first_create, &large_body_case.intent, &open);
        let attempt = assert_matching_plan(
            &after_first_create,
            &open,
            &terminal,
            &large_context,
            coverage,
            &format!(
                "multi-batch/graphql-batches/create-after-ack/id-{}/view-omitted-unmarked",
                action.id.as_str(),
            ),
        )
        .expect("an omitted unmarked OPEN row retries its stable create key");
        assert!(attempt.logical().creates.iter().any(|create| create.id == action.id));
        exercise_omitted_unmarked_attempt(
            &after_first_create,
            &attempt,
            std::slice::from_ref(&action.id),
            &large_body_case,
            coverage,
        );
        coverage.multi_batch_targeted_omission_views += 1;
    }
    verify_multi_batch_restarts(&large_body_case, large_catalogue, coverage);
}

#[test]
fn owned_base_and_marker_publication_exhaustively_survive_restarts() {
    let started = Instant::now();
    let mut coverage = Coverage::default();
    for topology in Topology::ALL {
        for phases in phase_vectors(topology.ids().len()) {
            for presentation in Presentation::ALL {
                explore_primary_case(
                    &main_case(topology, phases.clone(), presentation),
                    &mut coverage,
                );
            }
        }
    }
    exercise_history_factors(&mut coverage);
    exercise_revision_identity_boundary_factors(&mut coverage);
    exercise_oid_factors(&mut coverage);
    exercise_automation_factors(&mut coverage);
    exercise_terminal_factors(&mut coverage);
    exercise_absorbing_lifecycle_transition(&mut coverage);
    exercise_nonlocal_factors(&mut coverage);
    exercise_update_mask_factors(&mut coverage);
    exercise_stale_observation_factors(&mut coverage);
    exercise_markerless_multi_version_observations(&mut coverage);
    exercise_marker_only_factor(&mut coverage);
    exercise_receipt_registry_factors(&mut coverage);
    exercise_batch_prefix_factors(&mut coverage);

    assert_eq!(coverage.primary_cases, 360);
    assert_eq!(coverage.topology_cases["one"], 10);
    assert_eq!(coverage.topology_cases["two-ab"], 50);
    assert_eq!(coverage.topology_cases["two-ba"], 50);
    assert_eq!(coverage.topology_cases["three"], 250);
    assert_eq!(coverage.clean_cases, 180);
    assert_eq!(coverage.stale_cases, 180);
    assert_eq!(coverage.phase_cells, [192; 5]);
    assert_eq!(coverage.converged_worlds, coverage.distinct_restart_worlds);
    assert_eq!(coverage.factor_cases, 119);
    assert_eq!(coverage.receipt_registry_cases, 3);
    assert_eq!(coverage.multi_batch_replanned_worlds, coverage.multi_batch_distinct_worlds,);
    assert_eq!(coverage.multi_batch_convergences, coverage.multi_batch_distinct_worlds);
    assert_eq!(coverage.identity_boundary_cases, 5);
    assert_eq!(coverage.lifecycle_transition_cases, 1);
    assert_eq!(coverage.nonlocal_omission_views, 1);
    assert_eq!(coverage.markerless_history_views, 9);
    assert_eq!(coverage.markerless_history_omission_views, 1);
    assert_eq!(coverage.markerless_history_convergences, 1);
    insta::assert_snapshot!(coverage.summary(), @r###"
    primary cases: 360
    topology cases: one=10, two-ab=50, two-ba=50, three=250
    presentation cases: clean=180, body-stale=180
    per-phase cells [new, published, unmarked, provisional, final]: [192, 192, 192, 192, 192]
    initial logical actions: tuples=272, creates=384, markers=576, updates=874
    tuple visibility prefixes: 4896
    lost-ack outcomes: initial-git=396, marker-git=652
    alias masks: create=1058, final-update=2384, omission-create=6330
    distinct durable restart worlds: 3181
    OPEN observation views: 19748
    omitted unmarked views: 1610
    omitted marked safe rejections: 14957
    duplicate same-key creates with no durable effect: 1619
    production comparisons: calls=26768, plans=11769, safe rejections=14999
    stable-view convergences: 3181
    separate factor cases: 119
    receipt registry cases: 3
    multi-batch worlds: constructed=524, distinct=524, replanned=524, converged=524
    multi-batch retry states: replanned=2, equivalent-reuses=521
    multi-batch targeted omission views: 13
    separately stale observed-field views: 36
    exact revision-identity boundary cases: 5
    retained lifecycle-transition cases: 1
    omitted validation-only nonlocal views: 1
    markerless multi-version views: 9, omission=1, converged=1
    "###);
    eprintln!(
        "semantic oracle runtime: {:?} with {} Rayon workers",
        started.elapsed(),
        rayon::current_num_threads(),
    );
}

use super::*;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct LiteralRevision {
    pub(super) head: ObjectId,
    pub(super) first_parent: ObjectId,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct LocalChange {
    pub(super) id: GherritPrId,
    pub(super) desired: LiteralRevision,
    pub(super) title: String,
    pub(super) commit_body: String,
}

/// One nonempty, contiguous local first-parent stack.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct LocalIntent {
    pub(super) first: LocalChange,
    pub(super) later: Box<[LocalChange]>,
    pub(super) public_branch: Option<PublicBranchName>,
}

impl LocalIntent {
    pub(super) fn new(
        default_tip: ObjectId,
        changes: impl IntoIterator<Item = LocalChange>,
    ) -> Self {
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

    pub(super) fn with_public_branch(mut self, branch: &str) -> Self {
        assert!(self.public_branch.is_none(), "a local intent selects at most one public branch");
        self.public_branch = Some(public_branch_name(branch));
        self
    }

    pub(super) fn iter(&self) -> impl Iterator<Item = &LocalChange> {
        std::iter::once(&self.first).chain(self.later.iter())
    }
}

/// One nonempty immutable publication history.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct PublishedHistory {
    pub(super) first: LiteralRevision,
    pub(super) later: Vec<LiteralRevision>,
}

impl PublishedHistory {
    pub(super) fn new(first: LiteralRevision) -> Self {
        Self { first, later: Vec::new() }
    }

    pub(super) fn iter(&self) -> impl Iterator<Item = &LiteralRevision> {
        std::iter::once(&self.first).chain(self.later.iter())
    }

    pub(super) fn last(&self) -> LiteralRevision {
        self.later.last().copied().unwrap_or(self.first)
    }

    pub(super) fn len(&self) -> usize {
        self.later.len() + 1
    }

    pub(super) fn get(&self, index: usize) -> Option<LiteralRevision> {
        if index == 0 { Some(self.first) } else { self.later.get(index - 1).copied() }
    }

    pub(super) fn push(&mut self, revision: LiteralRevision) {
        self.later.push(revision);
    }
}

/// One literal durable pull request row.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct PullRequest {
    pub(super) identity: PullRequestIdentity,
    pub(super) state: PullRequestState,
    pub(super) terminal_fields: Option<OpenFields>,
}

/// Query visibility is separate from fields retained in external storage.
/// Terminal projections survive so a stale mutation may take effect before
/// its unusable receipt stops later effects.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) enum PullRequestState {
    Open(OpenFields),
    Closed,
    Merged,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct OpenFields {
    pub(super) title: String,
    pub(super) body: String,
    pub(super) base: BaseKind,
    pub(super) is_draft: bool,
}

impl PullRequest {
    pub(super) fn open_fields(&self) -> Option<&OpenFields> {
        match &self.state {
            PullRequestState::Open(fields) => Some(fields),
            PullRequestState::Closed | PullRequestState::Merged => None,
        }
    }

    pub(super) fn open_fields_mut(&mut self) -> Option<&mut OpenFields> {
        match &mut self.state {
            PullRequestState::Open(fields) => Some(fields),
            PullRequestState::Closed | PullRequestState::Merged => None,
        }
    }
}

/// The exact semantic identity encoded by one canonical marker.
///
/// The marker tag object ID is transport evidence derived from these fields;
/// it is deliberately not durable semantic state in this model.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct MarkerIdentity {
    pub(super) v1: ObjectId,
    pub(super) marker: PullRequestMarker,
}

impl MarkerIdentity {
    pub(super) fn number(self) -> PullRequestNumber {
        self.marker.number()
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct PublishedChange {
    pub(super) history: PublishedHistory,
    pub(super) marker: Option<MarkerIdentity>,
    pub(super) pull_requests: Vec<PullRequest>,
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
pub(super) struct DurableWorld {
    pub(super) default_tip: ObjectId,
    pub(super) changes: HashMap<GherritPrId, PublishedChange>,
    pub(super) public_branches: HashMap<PublicBranchName, ObjectId>,
    pub(super) next_identity: u32,
    pub(super) git_refs: testutil::git_model::RefStore,
}

impl DurableWorld {
    pub(super) fn for_intents(default_tip: ObjectId, intents: &[&LocalIntent]) -> Self {
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
            git_refs: testutil::git_model::RefStore::default(),
        };
        world.assert_well_formed();
        world
    }

    pub(super) fn published(&self, id: &GherritPrId) -> Option<&PublishedChange> {
        self.changes.get(id)
    }

    pub(super) fn published_mut(&mut self, id: &GherritPrId) -> Option<&mut PublishedChange> {
        self.changes.get_mut(id)
    }

    pub(super) fn open_pull_requests(
        &self,
        id: &GherritPrId,
    ) -> impl Iterator<Item = &PullRequest> {
        self.published(id)
            .into_iter()
            .flat_map(|change| change.pull_requests.iter())
            .filter(|pull_request| pull_request.open_fields().is_some())
    }

    pub(super) fn exact_pull_request(
        &self,
        identity: &PullRequestIdentity,
    ) -> Option<&PullRequest> {
        self.changes
            .values()
            .flat_map(|change| change.pull_requests.iter())
            .find(|pull_request| pull_request.identity == *identity)
    }

    pub(super) fn exact_open_pull_request(
        &self,
        identity: &PullRequestIdentity,
    ) -> Option<&PullRequest> {
        self.exact_pull_request(identity)
            .filter(|pull_request| pull_request.open_fields().is_some())
    }

    pub(super) fn selected_open_pull_request(&self, id: &GherritPrId) -> Option<&PullRequest> {
        let change = self.published(id)?;
        match change.marker {
            Some(marker) => change.pull_requests.iter().find(|pull_request| {
                pull_request.identity.number() == marker.number()
                    && pull_request.open_fields().is_some()
            }),
            None => self
                .open_pull_requests(id)
                .min_by_key(|pull_request| pull_request.identity.number().get()),
        }
    }

    pub(super) fn publish_for_setup(&mut self, id: &GherritPrId, revision: LiteralRevision) {
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
        let change = self.published(id).unwrap();
        let version = change.history.len();
        let head = revision.head.to_string();
        self.git_refs.0.insert(format!("refs/heads/{}", id.as_str()), head.clone());
        self.git_refs.0.insert(
            format!("refs/heads/gherrit-bases/{}", id.as_str()),
            revision.first_parent.to_string(),
        );
        self.git_refs.0.insert(format!("refs/tags/gherrit/{}/v{version}", id.as_str()), head);
        self.assert_well_formed();
    }

    pub(super) fn open_for_setup(&mut self, id: &GherritPrId, title: &str, body: &str) {
        let identity = self.allocate_identity();
        let change = self.published_mut(id).expect("an OPEN row requires published history");
        change.pull_requests.push(PullRequest {
            identity,
            terminal_fields: None,
            state: PullRequestState::Open(OpenFields {
                title: title.to_owned(),
                body: body.to_owned(),
                base: BaseKind::Owned,
                is_draft: true,
            }),
        });
        self.assert_well_formed();
    }

    pub(super) fn mark_for_setup(&mut self, id: &GherritPrId, v1: ObjectId, base: BaseKind) {
        let identity = self
            .open_pull_requests(id)
            .min_by_key(|pull_request| pull_request.identity.number().get())
            .expect("the setup marker selects an OPEN row")
            .identity
            .clone();
        self.mark_identity_for_setup(id, v1, &identity, base);
    }

    pub(super) fn mark_identity_for_setup(
        &mut self,
        id: &GherritPrId,
        v1: ObjectId,
        identity: &PullRequestIdentity,
        base: BaseKind,
    ) {
        let change = self.published_mut(id).expect("a marker requires published history");
        assert_eq!(change.history.first.head, v1, "a marker targets exactly v1");
        assert!(change.marker.is_none(), "test setup cannot move an immutable marker");
        let selected = change
            .pull_requests
            .iter_mut()
            .find(|pull_request| pull_request.identity == *identity)
            .expect("a setup marker names a durable row");
        selected.open_fields_mut().expect("the setup marker selects an OPEN row").base = base;
        change.marker = Some(marker_identity(v1, identity.number()));
        self.assert_well_formed();
    }

    pub(super) fn mark_ready_on_default_for_setup(&mut self, id: &GherritPrId, v1: ObjectId) {
        self.mark_for_setup(id, v1, BaseKind::Default);
        let marker = self.published(id).unwrap().marker.unwrap();
        let row = self
            .published_mut(id)
            .unwrap()
            .pull_requests
            .iter_mut()
            .find(|row| row.identity.number() == marker.number())
            .expect("a setup marker selects its durable row");
        row.open_fields_mut().unwrap().is_draft = false;
        self.assert_well_formed();
    }

    pub(super) fn set_marker_for_setup(
        &mut self,
        id: &GherritPrId,
        v1: ObjectId,
        number: PullRequestNumber,
    ) {
        let change = self.published_mut(id).expect("a marker requires published history");
        assert_eq!(change.history.first.head, v1, "a marker targets exactly v1");
        assert!(
            change.marker.replace(marker_identity(v1, number)).is_none(),
            "test setup cannot move an immutable marker"
        );
        self.assert_well_formed();
    }

    pub(super) fn close_for_setup(&mut self, identity: &PullRequestIdentity) {
        self.set_terminal_for_setup(identity, PullRequestState::Closed);
    }

    pub(super) fn merge_for_setup(&mut self, identity: &PullRequestIdentity) {
        self.set_terminal_for_setup(identity, PullRequestState::Merged);
    }

    pub(super) fn reopen_for_setup(
        &mut self,
        identity: &PullRequestIdentity,
        title: &str,
        body: &str,
        base: BaseKind,
    ) {
        let pull_request = self
            .changes
            .values_mut()
            .flat_map(|change| change.pull_requests.iter_mut())
            .find(|pull_request| pull_request.identity == *identity)
            .expect("a setup reopen names a durable row");
        assert!(matches!(pull_request.state, PullRequestState::Closed));
        pull_request.terminal_fields = None;
        pull_request.state = PullRequestState::Open(OpenFields {
            title: title.to_owned(),
            body: body.to_owned(),
            base,
            is_draft: true,
        });
        self.assert_well_formed();
    }

    pub(super) fn set_terminal_for_setup(
        &mut self,
        identity: &PullRequestIdentity,
        state: PullRequestState,
    ) {
        assert!(matches!(state, PullRequestState::Closed | PullRequestState::Merged));
        let pull_request = self
            .changes
            .values_mut()
            .flat_map(|change| change.pull_requests.iter_mut())
            .find(|pull_request| pull_request.identity == *identity)
            .expect("a setup lifecycle change names a durable row");
        assert!(
            pull_request.open_fields().is_some(),
            "a setup lifecycle change consumes an OPEN row"
        );
        pull_request.terminal_fields = pull_request.open_fields().cloned();
        pull_request.state = state;
        self.assert_well_formed();
    }

    pub(super) fn allocate_identity(&mut self) -> PullRequestIdentity {
        let number = self.next_identity;
        self.next_identity = self.next_identity.checked_add(1).expect("test identity space");
        PullRequestIdentity::for_plan_test(number, &format!("PR_{number}"))
    }

    pub(super) fn identity(&self, id: &GherritPrId) -> PullRequestIdentity {
        self.selected_open_pull_request(id)
            .expect("the requested change has an OPEN identity")
            .identity
            .clone()
    }

    pub(super) fn latest_identity(&self, id: &GherritPrId) -> PullRequestIdentity {
        self.open_pull_requests(id)
            .max_by_key(|pull_request| pull_request.identity.number().get())
            .expect("the requested change has an OPEN identity")
            .identity
            .clone()
    }

    pub(super) fn current_open_fields(
        &self,
        id: &GherritPrId,
        identity: &PullRequestIdentity,
    ) -> ObservedOpenFields {
        let change = self.published(id).expect("an OPEN row has published history");
        let pull_request = change
            .pull_requests
            .iter()
            .find(|pull_request| pull_request.identity == *identity)
            .expect("the requested change has the exact row");
        let fields = pull_request.open_fields().expect("the requested row is OPEN");
        ObservedOpenFields {
            head_oid: ObjectId::from_hex(
                self.git_refs.0[&format!("refs/heads/{}", id.as_str())].as_bytes(),
            )
            .unwrap(),
            base: fields.base,
            base_oid: match fields.base {
                BaseKind::Default => self.default_tip,
                BaseKind::Owned => ObjectId::from_hex(
                    self.git_refs.0[&format!("refs/heads/gherrit-bases/{}", id.as_str())]
                        .as_bytes(),
                )
                .unwrap(),
            },
            title: fields.title.clone(),
            body: fields.body.clone(),
            is_draft: fields.is_draft,
        }
    }

    pub(super) fn current_open_rows(&self, id: &GherritPrId) -> Vec<ObservedOpenPullRequest> {
        self.open_pull_requests(id)
            .map(|pull_request| ObservedOpenPullRequest {
                identity: pull_request.identity.clone(),
                fields: self.current_open_fields(id, &pull_request.identity),
            })
            .collect()
    }

    pub(super) fn observed_open_rows(
        &self,
        id: &GherritPrId,
        visibility: &QueryVisibility,
    ) -> Vec<ObservedOpenPullRequest> {
        visibility
            .rows(id)
            .map_or_else(|| self.current_open_rows(id), <[ObservedOpenPullRequest]>::to_vec)
    }

    pub(super) fn plan(
        &self,
        intent: &LocalIntent,
        visibility: &QueryVisibility,
    ) -> Result<PlannedPublication> {
        assert!(
            visibility.captured.iter().all(|(id, rows)| {
                rows.iter().map(|row| &row.identity).collect::<HashSet<_>>().len() == rows.len()
                    && intent.iter().any(|local| local.id == *id)
                    && rows.iter().all(|observed| {
                        self.published(id).is_some_and(|change| {
                            change
                                .pull_requests
                                .iter()
                                .any(|durable| durable.identity == observed.identity)
                        })
                    })
            }),
            "each captured OPEN connection has unique durable rows for its queried change"
        );
        for (id, change) in &self.changes {
            let current = change.history.last();
            if self.git_refs.0.get(&format!("refs/heads/{}", id.as_str()))
                != Some(&current.head.to_string())
                || self.git_refs.0.get(&format!("refs/heads/gherrit-bases/{}", id.as_str()))
                    != Some(&current.first_parent.to_string())
            {
                bail!("Observed literal refs disagree with the immutable submission ledger");
            }
        }
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
                        change.marker.map(MarkerIdentity::number),
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
        let local =
            ObservedLocalPublication::for_plan_test(destination, stack, observed_public_branch);
        plan_effects(local, histories, pull_requests)
    }

    pub(super) async fn run_attempt(
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
                ProjectionEffect::Close { identity, .. } => Some(identity.clone()),
                ProjectionEffect::Update(_) => None,
            })
            .collect::<HashSet<_>>();
        assert!(
            emitted.is_subset(&expected_closes),
            "the planner may close only observed noncanonical OPEN identities"
        );
        if report.outcome == AttemptOutcome::Acknowledged {
            assert_eq!(
                emitted, expected_closes,
                "an acknowledged projection closes every observed noncanonical identity"
            );
        }
        Ok(report)
    }

    pub(super) fn expected_duplicate_identities(
        &self,
        intent: &LocalIntent,
        visibility: &QueryVisibility,
    ) -> HashSet<PullRequestIdentity> {
        intent
            .iter()
            .flat_map(|local| {
                let visible = self.observed_open_rows(&local.id, visibility);
                let marker = self.published(&local.id).and_then(|change| change.marker);
                let selected = marker.map_or_else(
                    || visible.iter().map(|row| row.identity.number()).min_by_key(|n| n.get()),
                    |marker| Some(marker.number()),
                );
                visible.into_iter().filter_map(move |row| {
                    (Some(row.identity.number()) != selected).then_some(row.identity)
                })
            })
            .collect()
    }

    pub(super) async fn execute_plan(
        &mut self,
        plan: PlannedPublication,
        interruption: Option<Interruption>,
    ) -> Result<AttemptReport> {
        let mut driver = WorldDriver::new(self, interruption);
        let result = plan.execute_with(&mut driver).await;
        Ok(driver.finish(result)?.into_solo())
    }

    pub(super) fn observe_pull_request(
        &self,
        id: &GherritPrId,
        visibility: &QueryVisibility,
    ) -> LocalPullRequestObservation {
        let mut visible = self.observed_open_rows(id, visibility);
        visible.sort_by_key(|row| row.identity.number().get());
        let Some(first) = visible.first().cloned() else {
            return LocalPullRequestObservation::Absent(AbsentPullRequest::for_plan_test(
                id.clone(),
            ));
        };
        LocalPullRequestObservation::Open(
            ManagedOpenPullRequests::for_plan_test(
                id.clone(),
                first.identity,
                first.fields.head_oid,
                ObservedBase::for_plan_test(first.fields.base, first.fields.base_oid),
                &first.fields.title,
                &first.fields.body,
                first.fields.is_draft,
                false,
            )
            .with_candidates_for_plan_test(
                visible
                    .into_iter()
                    .skip(1)
                    .map(|row| {
                        (
                            row.identity,
                            row.fields.head_oid,
                            ObservedBase::for_plan_test(row.fields.base, row.fields.base_oid),
                            row.fields.title,
                            row.fields.body,
                            row.fields.is_draft,
                            false,
                        )
                    })
                    .collect(),
            ),
        )
    }

    pub(super) fn assert_well_formed(&self) {
        for (id, change) in &self.changes {
            let current = change.history.last();
            assert_eq!(
                self.git_refs.0.get(&format!("refs/heads/{}", id.as_str())),
                Some(&current.head.to_string()),
                "current head must agree with latest immutable submission after the real storage operation"
            );
            assert_eq!(
                self.git_refs.0.get(&format!("refs/heads/gherrit-bases/{}", id.as_str())),
                Some(&current.first_parent.to_string()),
                "current base must agree with latest submission"
            );
        }

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
                assert_eq!(
                    marker.v1, published.history.first.head,
                    "an immutable marker targets exactly v1"
                );
            }
        }
    }
}

pub(super) fn literal_revision(revision: PublicationRevision) -> LiteralRevision {
    LiteralRevision { head: revision.head(), first_parent: revision.owned_base() }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum EffectOutcome {
    Acknowledged,
    Indeterminate,
    Rejected,
}

impl EffectOutcome {
    pub(super) fn stop_reason(self) -> StopReason {
        match self {
            Self::Indeterminate => StopReason::Indeterminate,
            Self::Rejected => StopReason::Rejected,
            Self::Acknowledged => panic!("an acknowledged effect cannot stop its attempt"),
        }
    }
}

#[derive(Clone, Debug)]
pub(super) enum ExternalEffect {
    Draft(DraftEffect),
    InitialRef(InitialRefEffect),
    Create(TestCreate),
    Marker(MarkerEffect),
    Projection(ProjectionEffect),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) enum ExternalTarget {
    Change(GherritPrId),
    PublicBranch(PublicBranchName),
    Unknown,
}

impl ExternalEffect {
    pub(super) fn target(&self) -> ExternalTarget {
        match self {
            Self::Draft(effect) => {
                effect.resolved_id.clone().map_or(ExternalTarget::Unknown, ExternalTarget::Change)
            }
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
    pub(super) fn marker_authorizes_draft(&self, operation: &TestDraft) -> bool {
        self.published(&operation.id).is_some_and(|change| {
            change.marker.is_some_and(|marker| marker.number() == operation.identity.number())
                && change.pull_requests.iter().any(|row| row.identity == operation.identity)
        })
    }

    pub(super) fn local_open_rows_are_draft_safe(&self, id: &GherritPrId) -> bool {
        self.open_pull_requests(id).all(|row| {
            let fields = row.open_fields().expect("the iterator retains only OPEN rows");
            fields.base != BaseKind::Owned || fields.is_draft
        })
    }

    pub(super) fn resolve_identity(&self, identity: &PullRequestIdentity) -> Option<GherritPrId> {
        let requested_node = identity.node_id_for_test();
        self.changes.iter().find_map(|(id, change)| {
            change
                .pull_requests
                .iter()
                .any(|pull_request| pull_request.identity.node_id_for_test() == requested_node)
                .then(|| id.clone())
        })
    }

    pub(super) fn resolve_projection(
        &self,
        operation: TestPullRequestProjection,
    ) -> ProjectionEffect {
        match operation {
            TestPullRequestProjection::Close(operation) => ProjectionEffect::Close {
                resolved_id: self.resolve_identity(&operation.identity),
                identity: operation.identity,
            },
            TestPullRequestProjection::Update(operation) => {
                ProjectionEffect::Update(UpdateEffect {
                    resolved_id: self.resolve_identity(&operation.identity),
                    operation,
                })
            }
        }
    }

    pub(super) fn resolve_draft(&self, operation: TestDraft) -> DraftEffect {
        DraftEffect { resolved_id: self.resolve_identity(&operation.identity), operation }
    }

    pub(super) fn resolve_close(&self, identity: PullRequestIdentity) -> ProjectionEffect {
        ProjectionEffect::Close { resolved_id: self.resolve_identity(&identity), identity }
    }

    pub(super) fn apply_effect(&mut self, effect: &ExternalEffect) -> EffectOutcome {
        let before = self.clone();
        let target = effect.target();
        let outcome = match effect {
            ExternalEffect::Draft(effect) => self.apply_draft(effect),
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

    pub(super) fn apply_initial_ref(&mut self, effect: &InitialRefEffect) -> EffectOutcome {
        match effect {
            InitialRefEffect::Tuple(tuple) => self.apply_tuple(tuple),
            InitialRefEffect::PublicBranch { branch, expected, desired } => {
                self.apply_public_branch(branch, *expected, *desired)
            }
        }
    }

    pub(super) fn apply_public_branch(
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

    pub(super) fn apply_tuple(&mut self, effect: &TupleEffect) -> EffectOutcome {
        use testutil::git_model::Update;
        let id = effect.id.as_str();
        let version_ref = format!("refs/tags/gherrit/{id}/v{}", effect.version);
        let is_new_version = !self.git_refs.0.contains_key(&version_ref);
        let updates = [
            Update {
                name: format!("refs/heads/{id}"),
                expected: effect.expected.map(|r| r.head.to_string()),
                desired: effect.desired.head.to_string(),
            },
            Update {
                name: format!("refs/heads/gherrit-bases/{id}"),
                expected: effect.expected.map(|r| r.first_parent.to_string()),
                desired: effect.desired.first_parent.to_string(),
            },
            Update { name: version_ref, expected: None, desired: effect.publication.to_string() },
        ];
        if self.git_refs.push(&updates).is_err() {
            return EffectOutcome::Rejected;
        }
        // This ledger describes submitted revisions; it does not supply leases
        // or repair current refs. A bad storage outcome remains representable.
        match self.changes.get_mut(&effect.id) {
            None => {
                self.changes.insert(
                    effect.id.clone(),
                    PublishedChange {
                        history: PublishedHistory::new(effect.desired),
                        marker: None,
                        pull_requests: Vec::new(),
                    },
                );
            }
            Some(change) if is_new_version => change.history.push(effect.desired),
            Some(_) => {}
        }
        EffectOutcome::Acknowledged
    }

    pub(super) fn apply_create(&mut self, effect: &TestCreate) -> EffectOutcome {
        let current = self
            .published(&effect.id)
            .expect("a planned create follows acknowledged tuple publication");
        let current = current.history.last();
        let identity = self.allocate_identity();
        self.published_mut(&effect.id).unwrap().pull_requests.push(PullRequest {
            identity,
            terminal_fields: None,
            state: PullRequestState::Open(OpenFields {
                title: effect.title.clone(),
                body: effect.body.clone(),
                base: BaseKind::Owned,
                is_draft: true,
            }),
        });
        if effect.head_oid == current.head && effect.base_oid == current.first_parent {
            EffectOutcome::Acknowledged
        } else {
            EffectOutcome::Indeterminate
        }
    }

    pub(super) fn apply_draft(&mut self, effect: &DraftEffect) -> EffectOutcome {
        let Some(resolved_id) = effect.resolved_id.as_ref() else {
            return EffectOutcome::Indeterminate;
        };
        let change = self
            .changes
            .get_mut(resolved_id)
            .expect("a resolved draft conversion belongs to a published change");
        let current = change.history.last();
        let pull_request = change
            .pull_requests
            .iter_mut()
            .find(|row| {
                row.identity.node_id_for_test() == effect.operation.identity.node_id_for_test()
            })
            .expect("a resolved draft conversion belongs to a durable pull request");
        let exact_identity = pull_request.identity == effect.operation.identity;
        let Some(fields) = pull_request.open_fields_mut() else {
            return EffectOutcome::Indeterminate;
        };
        // The mutation may return the already-draft row. It grants authority
        // only when the same exact receipt would also acknowledge a fresh
        // ready-to-draft transition.
        fields.is_draft = true;
        let exact = resolved_id == &effect.operation.id
            && exact_identity
            && current.head == effect.operation.head_oid
            && fields.base == BaseKind::Default
            && effect.operation.default_branch.name() == DEFAULT_BRANCH
            && effect.operation.default_branch.tip() == self.default_tip;
        if exact { EffectOutcome::Acknowledged } else { EffectOutcome::Indeterminate }
    }

    pub(super) fn apply_marker(&mut self, effect: &MarkerEffect) -> EffectOutcome {
        let change =
            self.published_mut(&effect.id).expect("a planned marker has published history");
        if change.history.first.head != effect.marker.v1 {
            return EffectOutcome::Rejected;
        }
        match change.marker {
            None => {
                change.marker = Some(effect.marker);
                EffectOutcome::Acknowledged
            }
            Some(marker) => {
                if marker == effect.marker {
                    EffectOutcome::Acknowledged
                } else {
                    EffectOutcome::Rejected
                }
            }
        }
    }

    pub(super) fn apply_projection(&mut self, effect: &ProjectionEffect) -> EffectOutcome {
        match effect {
            ProjectionEffect::Close { resolved_id, identity } => {
                self.apply_close(resolved_id.as_ref(), identity)
            }
            ProjectionEffect::Update(update) => self.apply_update(update),
        }
    }

    pub(super) fn apply_close(
        &mut self,
        resolved_id: Option<&GherritPrId>,
        identity: &PullRequestIdentity,
    ) -> EffectOutcome {
        let Some(resolved_id) = resolved_id else {
            return EffectOutcome::Indeterminate;
        };
        let change = self.changes.get_mut(resolved_id).expect("resolved close target exists");
        let pull_request = change
            .pull_requests
            .iter_mut()
            .find(|pull_request| {
                pull_request.identity.node_id_for_test() == identity.node_id_for_test()
            })
            .expect("resolved close identity exists");
        if pull_request.open_fields().is_none() {
            return EffectOutcome::Indeterminate;
        }
        let exact = pull_request.identity == *identity;
        pull_request.terminal_fields = pull_request.open_fields().cloned();
        pull_request.state = PullRequestState::Closed;
        if exact { EffectOutcome::Acknowledged } else { EffectOutcome::Indeterminate }
    }

    pub(super) fn apply_update(&mut self, effect: &UpdateEffect) -> EffectOutcome {
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
        let is_open = pull_request.open_fields().is_some();
        let fields = if is_open {
            pull_request.open_fields_mut().unwrap()
        } else {
            pull_request.terminal_fields.as_mut().expect("terminal projection retained")
        };
        if let Some(base) = base {
            fields.base = base;
        }
        if let Some(title) = &effect.operation.title {
            fields.title.clone_from(title);
        }
        if let Some(body) = &effect.operation.body {
            fields.body.clone_from(body);
        }
        let returned_base = match fields.base {
            BaseKind::Default => DEFAULT_BRANCH.to_owned(),
            BaseKind::Owned => owned_base_name(resolved_id),
        };
        let topology_matches = returned_base == effect.operation.expected_base
            && (fields.base != BaseKind::Owned || fields.is_draft);
        if is_open && pull_request.identity == effect.operation.identity && topology_matches {
            EffectOutcome::Acknowledged
        } else {
            // GitHub routes by node ID. A mismatched expected number is known
            // only after the full patch has landed and its receipt is decoded.
            EffectOutcome::Indeterminate
        }
    }

    pub(super) fn assert_local_transition(
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
                    || matches!(effect, ExternalEffect::Draft(_) | ExternalEffect::Projection(_)),
                "only a pull request mutation may change an existing row"
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

    pub(super) fn assert_public_transition(
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

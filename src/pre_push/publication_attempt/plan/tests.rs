use gix::ObjectId;

use super::{
    super::{
        github::{
            AbsentPullRequest, MAX_MUTATION_REQUEST_BYTES, ObservedBase, ObservedGithub,
            TestPullRequestProjection,
        },
        history::{ObservedPullRequestMarker, ValidatedChangeHistory},
        refs::TestPushEffect,
    },
    *,
};
use crate::{
    manage::PublicBranchName,
    pre_push::{
        destination::{ObservedPublicBranch, RepositoryCoordinates},
        local::{GherritPrId, LocalStack},
    },
};

const DEFAULT_NAME: &str = "main";

fn oid(value: u16) -> ObjectId {
    let mut bytes = [0_u8; 20];
    bytes[18..].copy_from_slice(&value.to_be_bytes());
    if value == 0 {
        bytes[17] = 1;
    }
    ObjectId::from_bytes_or_panic(&bytes)
}

fn id(value: &str) -> GherritPrId {
    GherritPrId::from_ref_component(value.as_bytes()).expect("valid test change ID")
}

fn identity(number: u32, node: &str) -> PullRequestIdentity {
    PullRequestIdentity::for_plan_test(number, node)
}

fn update_operations(
    projection: &PreparedPullRequestProjection,
) -> Vec<&super::super::github::TestUpdate> {
    projection
        .projection_operations_for_test()
        .iter()
        .map(|operation| match operation {
            TestPullRequestProjection::Update(update) => update,
            TestPullRequestProjection::Close(_) => {
                panic!("this update-focused fixture cannot emit duplicate closes")
            }
        })
        .collect()
}

fn default_branch(name: &str, tip: ObjectId) -> DefaultBranch {
    DefaultBranch::new(name.to_owned(), tip).expect("valid test default branch")
}

fn validated_history(
    id: GherritPrId,
    published: &[(ObjectId, ObjectId)],
    proposal: (ObjectId, ObjectId),
    has_pull_request_marker: bool,
) -> ValidatedChangeHistory {
    let marker = has_pull_request_marker.then(|| {
        let v1 = published.first().expect("a marked test history must have v1").0;
        ObservedPullRequestMarker::for_plan_test(v1)
    });
    ValidatedChangeHistory::for_plan_test(id, published, proposal, marker)
}

#[test]
fn public_branch_cannot_conflict_with_the_default_branch_ref_path() {
    for (public, default) in [
        ("release-v1", "release-v1"),
        ("release-v1/work", "release-v1"),
        ("release-v1", "release-v1/stable"),
    ] {
        let name = PublicBranchName::new(public.to_owned()).unwrap();
        assert!(PublicBranch::new(name, &default_branch(default, oid(10))).is_err());
    }

    let name = PublicBranchName::new("release-v1/work".to_owned()).unwrap();
    assert!(PublicBranch::new(name, &default_branch("release-v2", oid(10))).is_ok());
}

#[test]
fn public_branch_plan_retains_exactly_the_observed_transition_state() {
    let desired = oid(20);
    for (remote, needs_transition) in [
        (RemoteBranchState::Absent, true),
        (RemoteBranchState::At(desired), false),
        (RemoteBranchState::At(oid(19)), true),
    ] {
        let default = default_branch(DEFAULT_NAME, oid(10));
        let name = PublicBranchName::new("release-candidate".to_owned()).unwrap();
        let observed = ObservedPublicBranch::for_test(name, remote);
        let observed = ObservedPublicProjection::new(observed, &default).unwrap();
        let planned = plan_public_branch(Some(observed), desired).unwrap().unwrap();
        assert_eq!(planned.branch().as_str(), "release-candidate");
        assert_eq!(planned.transition().is_some(), needs_transition);
    }
}

#[derive(Clone)]
struct HistorySpec {
    published: Vec<(ObjectId, ObjectId)>,
    marker: bool,
}

impl HistorySpec {
    fn absent() -> Self {
        Self { published: Vec::new(), marker: false }
    }

    fn current(head: ObjectId, first_parent: ObjectId, marker: bool) -> Self {
        Self { published: vec![(head, first_parent)], marker }
    }
}

#[derive(Clone)]
enum PullRequestSpec {
    Absent,
    Open(OpenSpec),
    OpenWithDuplicates(OpenSpec, Vec<OpenSpec>),
}

#[derive(Clone)]
struct OpenSpec {
    number: u32,
    node: String,
    head: ObjectId,
    base_kind: BaseKind,
    base: ObjectId,
    title: Option<String>,
    body: Option<String>,
    is_draft: bool,
    landing_automation: bool,
}

impl OpenSpec {
    fn new(number: u32, head: ObjectId, base_kind: BaseKind, base: ObjectId) -> Self {
        Self {
            number,
            node: format!("PR_{number}"),
            head,
            base_kind,
            base,
            title: None,
            body: None,
            is_draft: true,
            landing_automation: false,
        }
    }
}

#[derive(Clone)]
struct EntrySpec {
    id: &'static str,
    history: HistorySpec,
    pull_request: PullRequestSpec,
}

struct Inputs {
    destination: PushDestination,
    stack: LocalStack,
    histories: Box<[ValidatedChangeHistory]>,
    pull_requests: CompleteLocalPullRequests,
}

fn inputs(specs: &[EntrySpec]) -> Inputs {
    inputs_with_repository(specs, "owner", "repo", DEFAULT_NAME, oid(10))
}

fn inputs_with_repository(
    specs: &[EntrySpec],
    owner: &str,
    repository: &str,
    github_default_name: &str,
    github_default_tip: ObjectId,
) -> Inputs {
    assert!(!specs.is_empty());
    let destination = PushDestination::for_test();
    let local_default = default_branch(DEFAULT_NAME, oid(10));
    let mut parent = local_default.tip();
    let local = specs
        .iter()
        .enumerate()
        .map(|(index, spec)| {
            let head = oid(20 + u16::try_from(index).unwrap());
            let entry = (id(spec.id), head, parent, desired_title(spec.id), desired_body(spec.id));
            parent = head;
            entry
        })
        .collect::<Vec<_>>();
    let stack = LocalStack::for_plan_test(local_default, local.clone());
    let histories = specs
        .iter()
        .zip(&local)
        .map(|(spec, (change_id, head, first_parent, _, _))| {
            validated_history(
                change_id.clone(),
                &spec.history.published,
                (*head, *first_parent),
                spec.history.marker,
            )
        })
        .collect::<Vec<_>>()
        .into_boxed_slice();
    let observations = specs
        .iter()
        .map(|spec| match &spec.pull_request {
            PullRequestSpec::Absent => {
                LocalPullRequestObservation::Absent(AbsentPullRequest::for_plan_test(id(spec.id)))
            }
            PullRequestSpec::Open(open) | PullRequestSpec::OpenWithDuplicates(open, _) => {
                let title = open.title.clone().unwrap_or_else(|| desired_title(spec.id));
                let body = open.body.clone().unwrap_or_else(|| desired_body(spec.id));
                let observed = ManagedOpenPullRequests::for_plan_test(
                    id(spec.id),
                    identity(open.number, &open.node),
                    open.head,
                    ObservedBase::for_plan_test(open.base_kind, open.base),
                    &title,
                    &body,
                    open.is_draft,
                    open.landing_automation,
                );
                let duplicates = match &spec.pull_request {
                    PullRequestSpec::OpenWithDuplicates(_, duplicates) => duplicates
                        .iter()
                        .map(|duplicate| {
                            (
                                identity(duplicate.number, &duplicate.node),
                                duplicate.head,
                                ObservedBase::for_plan_test(duplicate.base_kind, duplicate.base),
                                duplicate.landing_automation,
                            )
                        })
                        .collect(),
                    PullRequestSpec::Absent | PullRequestSpec::Open(_) => Vec::new(),
                };
                LocalPullRequestObservation::Open(
                    observed.with_duplicates_for_plan_test(duplicates),
                )
            }
        })
        .collect();
    let pull_requests = CompleteLocalPullRequests::for_plan_test(
        RepositoryCoordinates::for_test(owner, repository),
        default_branch(github_default_name, github_default_tip),
        observations,
        &[],
    )
    .unwrap();
    Inputs { destination, stack, histories, pull_requests }
}

fn desired_title(change_id: &str) -> String {
    format!("Title for {}", content_key(change_id))
}

fn desired_body(change_id: &str) -> String {
    let key = content_key(change_id);
    format!("Body for {key}.\n\nSecond paragraph for {key}.")
}

fn content_key(change_id: &str) -> &str {
    &change_id[..change_id.len().min(32)]
}

fn plan(specs: &[EntrySpec]) -> Result<PlannedPublication> {
    plan_with_public_branch(specs, None)
}

fn plan_private_effects(
    destination: PushDestination,
    stack: LocalStack,
    histories: Box<[ValidatedChangeHistory]>,
    pull_requests: CompleteLocalPullRequests,
) -> Result<PlannedPublication> {
    plan_effects(
        ObservedLocalPublication::for_plan_test(destination, stack, None),
        histories,
        pull_requests,
    )
}

fn plan_with_public_branch(
    specs: &[EntrySpec],
    public_branch: Option<String>,
) -> Result<PlannedPublication> {
    let Inputs { destination, stack, histories, pull_requests } = inputs(specs);
    let observed = public_branch
        .map(PublicBranchName::new)
        .transpose()?
        .map(|name| ObservedPublicBranch::for_test(name, RemoteBranchState::Absent));
    let local = ObservedLocalPublication::for_plan_test(destination, stack, observed);
    plan_effects(local, histories, pull_requests)
}

fn observed_for_plan(
    destination: &PushDestination,
    pull_requests: CompleteLocalPullRequests,
) -> ObservedGithub {
    let github = Github::for_plan_test(destination);
    ObservedGithub::for_plan_test(github, pull_requests)
}

fn tuple_count(pushes: &PreparedPushes) -> usize {
    pushes
        .batches()
        .flat_map(|batch| batch.semantic_effects_for_test())
        .filter(|effect| matches!(effect, TestPushEffect::Tuple { .. }))
        .count()
}

fn marker_destinations(pushes: &PreparedPushes) -> Vec<String> {
    marker_refspecs(pushes)
        .into_iter()
        .map(|refspec| refspec.split_once(':').unwrap().1.to_owned())
        .collect()
}

fn marker_refspecs(pushes: &PreparedPushes) -> Vec<String> {
    pushes.batches().flat_map(|batch| batch.refspecs()).map(str::to_owned).collect()
}

fn tuple_for_test(history: &ValidatedChangeHistory) -> Option<Result<TupleTransition>> {
    tuple_transition(history, publication_revision(history.proposed()).unwrap())
}

fn ready(plan: PlannedPublication) -> MarkerStage {
    match plan.after_initial_refs {
        AfterInitialRefs::Ready(stage) => *stage,
        AfterInitialRefs::Creates(_) => panic!("expected an all-existing projection"),
    }
}

fn creates(plan: PlannedPublication) -> CreateStage {
    match plan.after_initial_refs {
        AfterInitialRefs::Creates(stage) => *stage,
        AfterInitialRefs::Ready(_) => panic!("expected a create-dependent projection"),
    }
}

fn receipts(values: &[(&str, u32, &str)]) -> CompleteCreateReceipts {
    CompleteCreateReceipts::for_plan_test(
        values
            .iter()
            .map(|(change_id, number, node)| (id(change_id), identity(*number, node)))
            .collect(),
    )
}

fn current_open(
    change_id: &'static str,
    proposal_head: ObjectId,
    proposal_parent: ObjectId,
    marker: bool,
    number: u32,
    base_kind: BaseKind,
    base: ObjectId,
) -> EntrySpec {
    EntrySpec {
        id: change_id,
        history: HistorySpec::current(proposal_head, proposal_parent, marker),
        pull_request: PullRequestSpec::Open(OpenSpec::new(number, proposal_head, base_kind, base)),
    }
}

fn single_desired_body() -> String {
    let destination = PushDestination::for_test();
    let default = default_branch(DEFAULT_NAME, oid(10));
    let stack = LocalStack::for_plan_test(
        default,
        [(id("Gone"), oid(20), oid(10), desired_title("Gone"), desired_body("Gone"))],
    );
    let history = validated_history(id("Gone"), &[(oid(20), oid(10))], (oid(20), oid(10)), true);
    StackBodyRecipes::new(&destination, None, stack, vec![history])
        .unwrap()
        .final_bodies(&[(id("Gone"), super::super::github::PullRequestNumber::for_test(7))])
        .unwrap()
        .into_vec()
        .pop()
        .unwrap()
        .into_parts()
        .1
        .into_string()
}

#[test]
fn planner_accepts_exactly_the_four_supported_local_realities() {
    let fresh = plan(&[EntrySpec {
        id: "Gfresh",
        history: HistorySpec::absent(),
        pull_request: PullRequestSpec::Absent,
    }])
    .unwrap();
    assert_eq!(tuple_count(&fresh.initial_ref_pushes), 1);
    let fresh = creates(fresh);
    assert_eq!(fresh.creates.operations_for_test()[0].id, id("Gfresh"));

    let recovery = plan(&[EntrySpec {
        id: "Grecovery",
        history: HistorySpec::current(oid(20), oid(10), false),
        pull_request: PullRequestSpec::Absent,
    }])
    .unwrap();
    assert_eq!(tuple_count(&recovery.initial_ref_pushes), 0);
    assert_eq!(creates(recovery).creates.operations_for_test().len(), 1);

    let unmarked =
        plan(&[current_open("Gunmarked", oid(20), oid(10), false, 7, BaseKind::Owned, oid(10))])
            .unwrap();
    let unmarked = ready(unmarked);
    assert_eq!(marker_destinations(&unmarked.marker_pushes), ["refs/tags/gherrit/Gunmarked/pr"]);

    let marked =
        plan(&[current_open("Gmarked", oid(20), oid(10), true, 7, BaseKind::Default, oid(10))])
            .unwrap();
    assert!(marker_destinations(&ready(marked).marker_pushes).is_empty());

    let unexplained = plan(&[EntrySpec {
        id: "Gunexplained",
        history: HistorySpec::absent(),
        pull_request: PullRequestSpec::Open(OpenSpec::new(7, oid(20), BaseKind::Owned, oid(10))),
    }])
    .err()
    .unwrap();
    assert!(unexplained.to_string().contains("OPEN pull request but no published history"));

    let marked_absent = plan(&[EntrySpec {
        id: "Gmarkedabsent",
        history: HistorySpec::current(oid(20), oid(10), true),
        pull_request: PullRequestSpec::Absent,
    }])
    .err()
    .unwrap();
    assert!(marked_absent.to_string().contains("marker but no OPEN pull request"));
}

#[test]
fn planner_rejects_an_empty_stack_before_deriving_any_stage() {
    let destination = PushDestination::for_test();
    let default = default_branch(DEFAULT_NAME, oid(10));
    let stack = LocalStack::for_plan_test(
        default.clone(),
        Vec::<(GherritPrId, ObjectId, ObjectId, String, String)>::new(),
    );
    let pull_requests = CompleteLocalPullRequests::for_plan_test(
        RepositoryCoordinates::for_test("owner", "repo"),
        default,
        Vec::new(),
        &[],
    )
    .unwrap();

    let local = ObservedLocalPublication::for_plan_test(destination, stack, None);
    let error = plan_effects(local, Box::new([]), pull_requests).err().unwrap();
    assert!(error.to_string().contains("requires a nonempty local stack"));
}

fn empty_publication_plan(public_state: Option<RemoteBranchState>) -> Result<EmptyPublicationPlan> {
    let destination = PushDestination::for_test();
    let default = default_branch(DEFAULT_NAME, oid(10));
    let stack = LocalStack::for_plan_test(
        default,
        Vec::<(GherritPrId, ObjectId, ObjectId, String, String)>::new(),
    );
    let public_branch = public_state.map(|state| {
        let name = PublicBranchName::new("release-candidate".to_owned()).unwrap();
        ObservedPublicBranch::for_test(name, state)
    });
    plan_empty_publication(ObservedLocalPublication::for_plan_test(
        destination,
        stack,
        public_branch,
    ))
}

#[test]
fn empty_private_and_already_current_publication_have_no_git_effects() {
    for public_state in [None, Some(RemoteBranchState::At(oid(10)))] {
        let plan = empty_publication_plan(public_state).unwrap();
        assert_eq!(plan.pushes.batches().len(), 0);
    }
}

#[test]
fn empty_publication_creates_or_advances_its_public_branch_to_the_default_tip() {
    for (state, expected) in
        [(RemoteBranchState::Absent, None), (RemoteBranchState::At(oid(9)), Some(oid(9)))]
    {
        let plan = empty_publication_plan(Some(state)).unwrap();
        let effects = plan
            .pushes
            .batches()
            .flat_map(|batch| batch.semantic_effects_for_test())
            .cloned()
            .collect::<Vec<_>>();
        assert_eq!(
            effects,
            [TestPushEffect::PublicBranch {
                branch: "release-candidate".to_owned(),
                expected,
                desired: oid(10),
            }]
        );
    }
}

#[test]
fn empty_publication_planning_rejects_a_nonempty_stack() {
    let Inputs { destination, stack, .. } = inputs(&[EntrySpec {
        id: "Gone",
        history: HistorySpec::absent(),
        pull_request: PullRequestSpec::Absent,
    }]);
    let local = ObservedLocalPublication::for_plan_test(destination, stack, None);

    let error = plan_empty_publication(local).err().unwrap();
    assert!(error.to_string().contains("received a nonempty local stack"));
}

#[tokio::test]
async fn publication_plan_retains_its_matching_client_and_exact_git_target() {
    let spec = EntrySpec {
        id: "Gone",
        history: HistorySpec::absent(),
        pull_request: PullRequestSpec::Absent,
    };
    let Inputs { destination, stack, histories, pull_requests } = inputs(&[spec]);
    let observed = observed_for_plan(&destination, pull_requests);
    let expected_target = destination.publication_target();
    let local = ObservedLocalPublication::for_plan_test(destination, stack, None);
    let observed = CompletePublicationObservation::for_plan_test(local, histories, observed);
    let plan = plan_publication(observed).unwrap();
    assert!(plan.github.publication_target() == &expected_target);
    assert!(plan.destination.publication_target() == expected_target);
}

#[tokio::test]
async fn one_attempt_rejects_a_client_for_another_literal_git_destination() {
    let spec = EntrySpec {
        id: "Gone",
        history: HistorySpec::absent(),
        pull_request: PullRequestSpec::Absent,
    };
    let Inputs { destination, stack, histories, pull_requests } = inputs(&[spec]);
    let repository = crate::util::Repo::open(".").unwrap();
    let other = PushDestination::for_test_url_in(&repository, "git@github.com:owner/repo.git");
    let observed = observed_for_plan(&other, pull_requests);
    let local = ObservedLocalPublication::for_plan_test(destination, stack, None);
    let observed = CompletePublicationObservation::for_plan_test(local, histories, observed);
    let error = plan_publication(observed)
        .err()
        .expect("different literal destinations must not share one attempt");
    assert!(error.to_string().contains("different repository or push destination"));
}

#[tokio::test]
async fn one_attempt_rejects_a_client_for_another_local_repository() {
    let spec = EntrySpec {
        id: "Gone",
        history: HistorySpec::absent(),
        pull_request: PullRequestSpec::Absent,
    };
    let Inputs { destination, stack, histories, pull_requests } = inputs(&[spec]);
    let directory = tempfile::tempdir().unwrap();
    gix::init_bare(directory.path()).unwrap();
    let repository = crate::util::Repo::open(directory.path().to_str().unwrap()).unwrap();
    let other = PushDestination::for_test_in(&repository);
    let observed = observed_for_plan(&other, pull_requests);
    let local = ObservedLocalPublication::for_plan_test(destination, stack, None);
    let observed = CompletePublicationObservation::for_plan_test(local, histories, observed);
    let error = plan_publication(observed)
        .err()
        .expect("different local repositories must not share one attempt");
    assert!(error.to_string().contains("different repository or push destination"));
}

#[test]
fn all_counts_and_ordered_joins_are_checked_before_planning() {
    let specs = [
        EntrySpec {
            id: "Gone",
            history: HistorySpec::absent(),
            pull_request: PullRequestSpec::Absent,
        },
        EntrySpec {
            id: "Gtwo",
            history: HistorySpec::absent(),
            pull_request: PullRequestSpec::Absent,
        },
    ];

    for change in ["missing history", "extra history", "missing GitHub", "extra GitHub"] {
        let Inputs { destination, stack, mut histories, pull_requests } = inputs(&specs);
        let (github_default, observations, preparation) =
            pull_requests.into_planning_parts_for(&destination).unwrap();
        let pull_requests = match change {
            "missing history" => {
                let mut values = histories.into_vec();
                values.truncate(1);
                histories = values.into_boxed_slice();
                CompleteLocalPullRequests::for_plan_test(
                    RepositoryCoordinates::for_test("owner", "repo"),
                    github_default,
                    observations.into_vec(),
                    &[],
                )
                .unwrap()
            }
            "extra history" => {
                histories = histories
                    .into_vec()
                    .into_iter()
                    .chain([validated_history(id("Gextra"), &[], (oid(99), oid(98)), false)])
                    .collect::<Vec<_>>()
                    .into_boxed_slice();
                CompleteLocalPullRequests::for_plan_test(
                    RepositoryCoordinates::for_test("owner", "repo"),
                    github_default,
                    observations.into_vec(),
                    &[],
                )
                .unwrap()
            }
            "missing GitHub" => {
                let values = vec![LocalPullRequestObservation::Absent(
                    AbsentPullRequest::for_plan_test(id("Gone")),
                )];
                CompleteLocalPullRequests::for_plan_test(
                    RepositoryCoordinates::for_test("owner", "repo"),
                    github_default,
                    values,
                    &[],
                )
                .unwrap()
            }
            "extra GitHub" => {
                let mut values = observations.into_vec();
                values.push(LocalPullRequestObservation::Absent(AbsentPullRequest::for_plan_test(
                    id("Gextra"),
                )));
                CompleteLocalPullRequests::for_plan_test(
                    RepositoryCoordinates::for_test("owner", "repo"),
                    github_default,
                    values,
                    &[],
                )
                .unwrap()
            }
            _ => unreachable!(),
        };
        drop(preparation);
        let error =
            plan_private_effects(destination, stack, histories, pull_requests).err().unwrap();
        assert!(error.to_string().contains("different change counts"), "case={change}");
    }

    let Inputs { destination, stack, mut histories, pull_requests } = inputs(&specs);
    histories.swap(0, 1);
    let error = plan_private_effects(destination, stack, histories, pull_requests).err().unwrap();
    assert!(error.to_string().contains("Git history at stack position 0"));

    let Inputs { destination, stack, histories, pull_requests } = inputs(&specs);
    let (github_default, observations, preparation) =
        pull_requests.into_planning_parts_for(&destination).unwrap();
    let mut observations = observations.into_vec();
    observations.swap(0, 1);
    drop(preparation);
    let pull_requests = CompleteLocalPullRequests::for_plan_test(
        RepositoryCoordinates::for_test("owner", "repo"),
        github_default,
        observations,
        &[],
    )
    .unwrap();
    let error = plan_private_effects(destination, stack, histories, pull_requests).err().unwrap();
    assert!(error.to_string().contains("GitHub evidence at stack position 0"));
}

#[test]
fn repository_default_and_proposal_facts_must_match() {
    let spec = EntrySpec {
        id: "Gone",
        history: HistorySpec::absent(),
        pull_request: PullRequestSpec::Absent,
    };
    for (owner, repository) in [("other", "repo"), ("owner", "other")] {
        let Inputs { destination, stack, histories, pull_requests } = inputs_with_repository(
            std::slice::from_ref(&spec),
            owner,
            repository,
            DEFAULT_NAME,
            oid(10),
        );
        let error =
            plan_private_effects(destination, stack, histories, pull_requests).err().unwrap();
        assert!(error.to_string().contains("different push repository"));
    }

    for (name, tip) in [("trunk", oid(10)), (DEFAULT_NAME, oid(11))] {
        let Inputs { destination, stack, histories, pull_requests } =
            inputs_with_repository(std::slice::from_ref(&spec), "owner", "repo", name, tip);
        let error =
            plan_private_effects(destination, stack, histories, pull_requests).err().unwrap();
        assert!(error.to_string().contains("disagree"));
    }

    let Inputs { destination, stack, mut histories, pull_requests } =
        inputs(std::slice::from_ref(&spec));
    histories[0] = validated_history(id("Gone"), &[], (oid(99), oid(10)), false);
    let error = plan_private_effects(destination, stack, histories, pull_requests).err().unwrap();
    assert!(error.to_string().contains("local proposal and first parent"));

    let Inputs { destination, stack, mut histories, pull_requests } =
        inputs(std::slice::from_ref(&spec));
    histories[0] = validated_history(id("Gone"), &[], (oid(20), oid(99)), false);
    let error = plan_private_effects(destination, stack, histories, pull_requests).err().unwrap();
    assert!(error.to_string().contains("local proposal and first parent"));
}

fn open_for_validation(
    history: &ValidatedChangeHistory,
    head: ObjectId,
    base_kind: BaseKind,
    base: ObjectId,
    is_draft: bool,
    landing_automation: bool,
) -> ManagedOpenPullRequests {
    ManagedOpenPullRequests::for_plan_test(
        history.id().clone(),
        identity(7, "PR_7"),
        head,
        ObservedBase::for_plan_test(base_kind, base),
        "Test change",
        "observed body",
        is_draft,
        landing_automation,
    )
}

#[test]
fn every_published_owned_head_and_base_pair_is_independently_valid() {
    let heads = [oid(101), oid(102), oid(103)];
    let bases = [oid(201), oid(202), oid(203)];
    let published = heads.into_iter().zip(bases).collect::<Vec<_>>();
    let history = validated_history(id("Ghistory"), &published, (oid(20), oid(10)), true);
    let default = default_branch(DEFAULT_NAME, oid(10));

    for head in heads {
        for base in bases {
            let pull_request =
                open_for_validation(&history, head, BaseKind::Owned, base, true, false);
            validate_open(&history, pull_request, BaseKind::Owned, &default).unwrap();
        }
    }

    let proposal = open_for_validation(&history, oid(20), BaseKind::Owned, bases[0], true, false);
    assert!(validate_open(&history, proposal, BaseKind::Owned, &default).is_err());

    let wrong_owned =
        open_for_validation(&history, heads[0], BaseKind::Owned, oid(999), true, false);
    assert!(validate_open(&history, wrong_owned, BaseKind::Owned, &default).is_err());

    let exact_default =
        open_for_validation(&history, heads[0], BaseKind::Default, default.tip(), true, false);
    validate_open(&history, exact_default, BaseKind::Default, &default).unwrap();
    let wrong_default =
        open_for_validation(&history, heads[0], BaseKind::Default, oid(999), true, false);
    assert!(validate_open(&history, wrong_default, BaseKind::Default, &default).is_err());
}

#[test]
fn marker_base_and_landing_automation_rules_form_the_exact_truth_table() {
    let default = default_branch(DEFAULT_NAME, oid(10));
    for marker in [false, true] {
        let history =
            validated_history(id("Gtruth"), &[(oid(20), oid(10))], (oid(20), oid(10)), marker);
        for observed in [BaseKind::Default, BaseKind::Owned] {
            for desired in [BaseKind::Default, BaseKind::Owned] {
                for landing_automation in [false, true] {
                    let pull_request = open_for_validation(
                        &history,
                        oid(20),
                        observed,
                        oid(10),
                        true,
                        landing_automation,
                    );
                    let accepted = validate_open(&history, pull_request, desired, &default).is_ok();
                    let expected = (marker || observed == BaseKind::Owned)
                        && (!landing_automation
                            || (observed == BaseKind::Default && desired == BaseKind::Default));
                    assert_eq!(
                        accepted, expected,
                        "marker={marker}, observed={observed:?}, desired={desired:?}, automation={landing_automation}"
                    );
                }
            }
        }
    }
}

#[test]
fn duplicate_opens_close_before_the_canonical_update_in_one_projection_batch() {
    let canonical = OpenSpec::new(1, oid(20), BaseKind::Default, oid(10));
    let duplicates = vec![
        OpenSpec::new(2, oid(20), BaseKind::Owned, oid(10)),
        OpenSpec::new(3, oid(20), BaseKind::Default, oid(10)),
    ];
    let spec = EntrySpec {
        id: "Gone",
        history: HistorySpec::current(oid(20), oid(10), true),
        pull_request: PullRequestSpec::OpenWithDuplicates(canonical, duplicates),
    };

    let stage = ready(plan(&[spec]).unwrap());
    let operations = stage.projection.projection_operations_for_test();
    assert!(matches!(
        operations,
        [
            TestPullRequestProjection::Close(close_two),
            TestPullRequestProjection::Close(close_three),
            TestPullRequestProjection::Update(update),
        ] if close_two.identity.number().get() == 2
            && close_three.identity.number().get() == 3
            && update.identity.number().get() == 1
    ));
    assert_eq!(stage.projection.projection_batches_for_test().len(), 1);
}

#[test]
fn duplicate_repair_does_not_invent_a_canonical_update() {
    let canonical = OpenSpec {
        body: Some(single_desired_body()),
        ..OpenSpec::new(7, oid(20), BaseKind::Default, oid(10))
    };
    let duplicate = OpenSpec::new(8, oid(20), BaseKind::Owned, oid(10));
    let spec = EntrySpec {
        id: "Gone",
        history: HistorySpec::current(oid(20), oid(10), true),
        pull_request: PullRequestSpec::OpenWithDuplicates(canonical, vec![duplicate]),
    };

    let stage = ready(plan(&[spec]).unwrap());
    assert!(matches!(
        stage.projection.projection_operations_for_test(),
        [TestPullRequestProjection::Close(close)]
            if close.identity.number().get() == 8
    ));
}

#[test]
fn markerless_multiple_opens_are_repairable_after_every_candidate_validates() {
    let canonical = OpenSpec::new(1, oid(20), BaseKind::Owned, oid(10));
    let duplicate = OpenSpec::new(2, oid(20), BaseKind::Owned, oid(10));
    let spec = EntrySpec {
        id: "Gone",
        history: HistorySpec::current(oid(20), oid(10), false),
        pull_request: PullRequestSpec::OpenWithDuplicates(canonical, vec![duplicate]),
    };

    let stage = ready(plan(&[spec]).unwrap());
    assert_eq!(marker_destinations(&stage.marker_pushes), ["refs/tags/gherrit/Gone/pr"]);
    assert!(matches!(
        stage.projection.projection_operations_for_test(),
        [
            TestPullRequestProjection::Close(close),
            TestPullRequestProjection::Update(update),
        ] if close.identity.number().get() == 2
            && update.identity.number().get() == 1
            && update.base_branch.as_deref() == Some(DEFAULT_NAME)
    ));
}

#[test]
fn every_duplicate_must_have_valid_history_base_and_inert_landing_state() {
    let default = default_branch(DEFAULT_NAME, oid(10));
    let history = validated_history(id("Gone"), &[(oid(20), oid(10))], (oid(20), oid(10)), true);
    for desired in [BaseKind::Default, BaseKind::Owned] {
        for duplicate in [
            (identity(8, "BAD_HEAD"), oid(99), BaseKind::Owned, oid(10), false),
            (identity(8, "BAD_BASE"), oid(20), BaseKind::Owned, oid(99), false),
            (identity(8, "AUTO_DEFAULT"), oid(20), BaseKind::Default, oid(10), true),
            (identity(8, "AUTO_OWNED"), oid(20), BaseKind::Owned, oid(10), true),
        ] {
            let candidate =
                open_for_validation(&history, oid(20), BaseKind::Default, oid(10), true, false)
                    .with_duplicates_for_plan_test(vec![(
                        duplicate.0,
                        duplicate.1,
                        ObservedBase::for_plan_test(duplicate.2, duplicate.3),
                        duplicate.4,
                    )]);
            let node_id =
                candidate.duplicate_identities().next().unwrap().node_id_for_test().to_owned();
            assert!(
                validate_open(&history, candidate, desired, &default).is_err(),
                "accepted duplicate {} with desired base {desired:?}",
                node_id
            );
        }
    }

    let invalid_last =
        open_for_validation(&history, oid(20), BaseKind::Default, oid(10), true, false)
            .with_duplicates_for_plan_test(vec![
                (
                    identity(8, "VALID_FIRST"),
                    oid(20),
                    ObservedBase::for_plan_test(BaseKind::Owned, oid(10)),
                    false,
                ),
                (
                    identity(9, "INVALID_LAST"),
                    oid(20),
                    ObservedBase::for_plan_test(BaseKind::Default, oid(10)),
                    true,
                ),
            ]);
    let error = validate_open(&history, invalid_last, BaseKind::Default, &default).unwrap_err();
    assert!(error.to_string().contains("#9"));
}

#[test]
fn tuple_selection_uses_only_absence_or_a_changed_current_revision() {
    let absent = validated_history(id("Gabsent"), &[], (oid(20), oid(10)), false);
    assert!(tuple_for_test(&absent).unwrap().is_ok());

    let current =
        validated_history(id("Gcurrent"), &[(oid(20), oid(10))], (oid(20), oid(10)), false);
    assert!(tuple_for_test(&current).is_none());

    let changed = validated_history(
        id("Gchanged"),
        &[(oid(101), oid(201)), (oid(102), oid(202)), (oid(101), oid(201))],
        (oid(20), oid(10)),
        false,
    );
    let transition = tuple_for_test(&changed).unwrap().unwrap();
    let pushes = prepare_tuple_pushes(&PushDestination::for_test(), &[transition]).unwrap();
    let refspecs = pushes.batches().flat_map(|batch| batch.refspecs()).collect::<Vec<_>>();
    assert!(refspecs.iter().any(|value| value.ends_with("refs/tags/gherrit/Gchanged/v4")));

    let reused_noncurrent = validated_history(
        id("Greused"),
        &[(oid(101), oid(201)), (oid(102), oid(202))],
        (oid(101), oid(201)),
        false,
    );
    assert!(tuple_for_test(&reused_noncurrent).is_some());

    let reused_current = validated_history(
        id("Greused"),
        &[(oid(101), oid(201)), (oid(102), oid(202)), (oid(101), oid(201))],
        (oid(101), oid(201)),
        false,
    );
    assert!(tuple_for_test(&reused_current).is_none());
}

#[test]
fn every_new_marker_targets_the_single_desired_revision() {
    let existing = EntrySpec {
        id: "Gexisting",
        history: HistorySpec { published: vec![(oid(101), oid(10))], marker: false },
        pull_request: PullRequestSpec::Open(OpenSpec::new(7, oid(101), BaseKind::Owned, oid(10))),
    };
    let existing = plan(&[existing]).unwrap();
    assert_eq!(tuple_count(&existing.initial_ref_pushes), 1);
    assert_eq!(
        marker_refspecs(&ready(existing).marker_pushes),
        [format!("{}:refs/tags/gherrit/Gexisting/pr", oid(20))]
    );

    let absent = EntrySpec {
        id: "Gabsent",
        history: HistorySpec { published: vec![(oid(101), oid(10))], marker: false },
        pull_request: PullRequestSpec::Absent,
    };
    let absent = plan(&[absent]).unwrap();
    assert_eq!(tuple_count(&absent.initial_ref_pushes), 1);
    let absent = creates(absent);
    assert_eq!(absent.creates.operations_for_test()[0].head_oid, oid(20));
    let absent = absent.complete_for_test(receipts(&[("Gabsent", 8, "PR_8")])).unwrap();
    assert_eq!(
        marker_refspecs(&absent.marker_pushes),
        [format!("{}:refs/tags/gherrit/Gabsent/pr", oid(20))]
    );
}

fn mixed_specs() -> [EntrySpec; 4] {
    [
        current_open("Gone", oid(20), oid(10), true, 11, BaseKind::Default, oid(10)),
        EntrySpec {
            id: "Gtwo",
            history: HistorySpec::absent(),
            pull_request: PullRequestSpec::Absent,
        },
        current_open("Gthree", oid(22), oid(21), false, 33, BaseKind::Owned, oid(21)),
        EntrySpec {
            id: "Gfour",
            history: HistorySpec::current(oid(23), oid(22), false),
            pull_request: PullRequestSpec::Absent,
        },
    ]
}

#[test]
fn mixed_projection_has_one_create_order_and_one_final_identity_order() {
    let plan =
        plan_with_public_branch(&mixed_specs(), Some("release-candidate".to_owned())).unwrap();
    assert_eq!(tuple_count(&plan.initial_ref_pushes), 1);
    assert_eq!(
        plan.initial_ref_pushes
            .batches()
            .flat_map(|batch| batch.semantic_effects_for_test())
            .filter(|effect| matches!(effect, TestPushEffect::PublicBranch { .. }))
            .count(),
        1
    );
    let stage = creates(plan);
    let create_operations = stage.creates.operations_for_test();
    assert_eq!(create_operations.len(), 2);
    assert_eq!(
        create_operations.iter().map(|operation| operation.id.as_str()).collect::<Vec<_>>(),
        ["Gtwo", "Gfour"]
    );
    assert_eq!(create_operations[0].head_oid, oid(21));
    assert_eq!(create_operations[0].base_oid, oid(20));
    assert_eq!(create_operations[1].head_oid, oid(23));
    assert_eq!(create_operations[1].base_oid, oid(22));
    assert!(create_operations.iter().all(|operation| !operation.body.contains("\n- ")));
    for (operation, expected_id) in create_operations.iter().zip(["Gtwo", "Gfour"]) {
        assert_eq!(operation.title, desired_title(expected_id));
        assert!(operation.body.contains(&desired_body(expected_id)));
        assert!(
            operation.body.contains("[release\\-candidate](/owner/repo/tree/release-candidate)")
        );
        assert!(operation.body.contains(&format!("refs/heads/{expected_id}")));
    }

    let final_stage = stage
        .complete_for_test(receipts(&[("Gtwo", 22, "PR_22"), ("Gfour", 44, "PR_44")]))
        .unwrap();
    assert_eq!(
        marker_destinations(&final_stage.marker_pushes),
        ["refs/tags/gherrit/Gtwo/pr", "refs/tags/gherrit/Gthree/pr", "refs/tags/gherrit/Gfour/pr",]
    );
    let updates = update_operations(&final_stage.projection);
    assert_eq!(
        updates.iter().map(|update| update.identity.number().get()).collect::<Vec<_>>(),
        [11, 22, 33, 44]
    );
    for update in &updates {
        let body = update.body.as_deref().expect("every stale or created body is updated");
        for number in [11, 22, 33, 44] {
            assert!(body.contains(&format!("#{number}")));
        }
    }
    assert!(updates.iter().all(|update| update.title.is_none()));
    assert!(updates.iter().all(|update| update.base_branch.is_none()));
    for (update, expected_id) in updates.iter().zip(["Gone", "Gtwo", "Gthree", "Gfour"]) {
        let body = update.body.as_deref().unwrap();
        assert!(body.contains(&desired_body(expected_id)));
        assert!(body.contains("[release\\-candidate](/owner/repo/tree/release-candidate)"));
        assert!(body.contains(&format!("refs/heads/{expected_id}")));
        for other_id in ["Gone", "Gtwo", "Gthree", "Gfour"] {
            assert_eq!(body.contains(&desired_body(other_id)), other_id == expected_id);
        }
    }
}

#[test]
fn create_receipts_preserve_known_duplicate_closes_in_the_final_projection() {
    let specs = [
        EntrySpec {
            id: "Gexisting",
            history: HistorySpec::current(oid(20), oid(10), true),
            pull_request: PullRequestSpec::OpenWithDuplicates(
                OpenSpec::new(1, oid(20), BaseKind::Default, oid(10)),
                vec![OpenSpec::new(2, oid(20), BaseKind::Owned, oid(10))],
            ),
        },
        EntrySpec {
            id: "Gmissing",
            history: HistorySpec::absent(),
            pull_request: PullRequestSpec::Absent,
        },
    ];
    let stage = creates(plan(&specs).unwrap());
    let final_stage = stage.complete_for_test(receipts(&[("Gmissing", 3, "PR_3")])).unwrap();

    assert!(matches!(
        final_stage.projection.projection_operations_for_test(),
        [
            TestPullRequestProjection::Close(close),
            TestPullRequestProjection::Update(existing),
            TestPullRequestProjection::Update(created),
        ] if close.identity.number().get() == 2
            && existing.identity.number().get() == 1
            && created.identity.number().get() == 3
    ));
}

#[test]
fn root_and_nonroot_creates_share_the_owned_key_but_only_root_moves_final_base() {
    let specs = [
        EntrySpec {
            id: "Groot",
            history: HistorySpec::absent(),
            pull_request: PullRequestSpec::Absent,
        },
        EntrySpec {
            id: "Gtip",
            history: HistorySpec::absent(),
            pull_request: PullRequestSpec::Absent,
        },
    ];
    let stage = creates(plan(&specs).unwrap());
    let creates = stage.creates.operations_for_test();
    assert_eq!(creates[0].id.as_str(), "Groot");
    assert_eq!(creates[1].id.as_str(), "Gtip");
    assert_eq!(creates[0].base_oid, oid(10));
    assert_eq!(creates[1].base_oid, oid(20));

    let final_stage =
        stage.complete_for_test(receipts(&[("Groot", 1, "PR_1"), ("Gtip", 2, "PR_2")])).unwrap();
    let updates = update_operations(&final_stage.projection);
    assert_eq!(updates.len(), 2);
    assert_eq!(updates[0].base_branch.as_deref(), Some(DEFAULT_NAME));
    assert_eq!(updates[1].base_branch, None);
    assert!(updates.iter().all(|update| update.title.is_none() && update.body.is_some()));
}

#[test]
fn a_marked_pull_request_moved_below_the_root_returns_to_its_owned_base() {
    let specs = [
        current_open("Groot", oid(20), oid(10), true, 1, BaseKind::Default, oid(10)),
        current_open("Gmoved", oid(21), oid(20), true, 2, BaseKind::Default, oid(10)),
    ];

    let stage = ready(plan(&specs).unwrap());
    let moved = stage.projection;
    let moved = update_operations(&moved)
        .into_iter()
        .find(|update| update.identity.number().get() == 2)
        .expect("the moved pull request requires a projection update");
    assert_eq!(moved.base_branch.as_deref(), Some("gherrit-bases/Gmoved"));
}

#[test]
fn receipt_completion_rejects_every_wrong_sequence_before_markers_escape() {
    let specs = [
        EntrySpec {
            id: "Gone",
            history: HistorySpec::absent(),
            pull_request: PullRequestSpec::Absent,
        },
        EntrySpec {
            id: "Gtwo",
            history: HistorySpec::absent(),
            pull_request: PullRequestSpec::Absent,
        },
    ];
    for values in [
        vec![],
        vec![("Gone", 1, "PR_1")],
        vec![("Gtwo", 2, "PR_2"), ("Gone", 1, "PR_1")],
        vec![("Gwrong", 1, "PR_1"), ("Gtwo", 2, "PR_2")],
        vec![("Gone", 1, "PR_1"), ("Gtwo", 2, "PR_2"), ("Gextra", 3, "PR_3")],
    ] {
        assert!(creates(plan(&specs).unwrap()).complete_for_test(receipts(&values)).is_err());
    }

    let duplicate_number = receipts(&[("Gone", 1, "PR_1"), ("Gtwo", 1, "PR_2")]);
    assert!(creates(plan(&specs).unwrap()).complete_for_test(duplicate_number).is_err());

    let duplicate_node = receipts(&[("Gone", 1, "PR_SAME"), ("Gtwo", 2, "PR_SAME")]);
    assert!(creates(plan(&specs).unwrap()).complete_for_test(duplicate_node).is_err());
}

#[test]
fn exact_update_preflight_after_receipts_still_precedes_marker_release() {
    let spec = EntrySpec {
        id: "Gone",
        history: HistorySpec::absent(),
        pull_request: PullRequestSpec::Absent,
    };
    let oversized_node = "N".repeat(MAX_MUTATION_REQUEST_BYTES);
    let receipts =
        CompleteCreateReceipts::for_plan_test(vec![(id("Gone"), identity(1, &oversized_node))]);
    let error = creates(plan(&[spec]).unwrap()).complete_for_test(receipts).err().unwrap();
    let limit = MAX_MUTATION_REQUEST_BYTES;
    assert!(error.to_string().contains(&format!("exceeds the {limit}-byte request limit")));
}

#[test]
fn known_duplicate_closes_are_preflighted_before_a_create_dependent_plan_escapes() {
    let mut duplicate = OpenSpec::new(2, oid(20), BaseKind::Default, oid(10));
    duplicate.node = "N".repeat(MAX_MUTATION_REQUEST_BYTES);
    let specs = [
        EntrySpec {
            id: "Gexisting",
            history: HistorySpec::current(oid(20), oid(10), true),
            pull_request: PullRequestSpec::OpenWithDuplicates(
                OpenSpec::new(1, oid(20), BaseKind::Default, oid(10)),
                vec![duplicate],
            ),
        },
        EntrySpec {
            id: "Gmissing",
            history: HistorySpec::absent(),
            pull_request: PullRequestSpec::Absent,
        },
    ];

    let error = plan(&specs).err().expect("known close preflight must prevent a staged plan");
    assert!(error.to_string().contains("GraphQL pull request projection at item 0 serializes to"));
    assert!(
        error
            .to_string()
            .contains(&format!("exceeds the {MAX_MUTATION_REQUEST_BYTES}-byte request limit"))
    );
}

#[test]
fn graphql_stages_are_preflighted_before_a_tuple_plan_can_escape() {
    let fresh = EntrySpec {
        id: "Gfresh",
        history: HistorySpec::absent(),
        pull_request: PullRequestSpec::Absent,
    };
    let Inputs { destination, stack, histories, pull_requests } = inputs(&[fresh]);
    let (github_default, observations, _) =
        pull_requests.into_planning_parts_for(&destination).unwrap();
    let oversized_repository = "R".repeat(MAX_MUTATION_REQUEST_BYTES);
    let pull_requests = CompleteLocalPullRequests::for_plan_test_with_repository_node(
        RepositoryCoordinates::for_test("owner", "repo"),
        github_default,
        observations.into_vec(),
        &[],
        &oversized_repository,
    )
    .unwrap();
    let error = plan_private_effects(destination, stack, histories, pull_requests).err().unwrap();
    assert!(error.to_string().contains("GraphQL create mutation at item 0"));

    let oversized_node = "N".repeat(MAX_MUTATION_REQUEST_BYTES);
    let spec = EntrySpec {
        id: "Gupdate",
        history: HistorySpec { published: vec![(oid(101), oid(10))], marker: true },
        pull_request: PullRequestSpec::Open(OpenSpec {
            number: 7,
            node: oversized_node,
            head: oid(101),
            base_kind: BaseKind::Default,
            base: oid(10),
            title: Some("stale title".to_owned()),
            body: None,
            is_draft: true,
            landing_automation: false,
        }),
    };
    let error = plan(&[spec]).err().unwrap();
    let limit = MAX_MUTATION_REQUEST_BYTES;
    assert!(error.to_string().contains(&format!("exceeds the {limit}-byte request limit")));
}

#[test]
fn existing_projection_emits_exact_desired_values_for_differing_fields() {
    let desired_title = desired_title("Gone");
    let desired_body = single_desired_body();
    for mask in 0_u8..8 {
        let title_differs = mask & 0b001 != 0;
        let body_differs = mask & 0b010 != 0;
        let base_differs = mask & 0b100 != 0;
        let open = OpenSpec {
            number: 7,
            node: "PR_7".to_owned(),
            head: oid(20),
            base_kind: if base_differs { BaseKind::Owned } else { BaseKind::Default },
            base: oid(10),
            title: title_differs.then(|| "stale title".to_owned()),
            body: Some(if body_differs { "stale body".to_owned() } else { desired_body.clone() }),
            is_draft: true,
            landing_automation: false,
        };
        let spec = EntrySpec {
            id: "Gone",
            history: HistorySpec::current(oid(20), oid(10), true),
            pull_request: PullRequestSpec::Open(open),
        };
        let stage = ready(plan(&[spec]).unwrap());
        let updates = update_operations(&stage.projection);
        assert_eq!(updates.len(), usize::from(mask != 0), "mask={mask:03b}");
        if let Some(update) = updates.first() {
            assert_eq!(
                update.title.as_deref(),
                title_differs.then_some(desired_title.as_str()),
                "mask={mask:03b}"
            );
            assert_eq!(
                update.body.as_deref(),
                body_differs.then_some(desired_body.as_str()),
                "mask={mask:03b}"
            );
            assert_eq!(update.base_branch.as_deref(), base_differs.then_some(DEFAULT_NAME));
        }
    }
}

#[test]
fn body_comparison_normalizes_only_crlf_pairs() {
    let desired = single_desired_body();
    assert!(bodies_equal(&desired, &desired.replace('\n', "\r\n")));
    for changed in [
        desired.replace('\n', "\r"),
        format!("{desired}\n"),
        format!(" {desired}"),
        format!("{desired} "),
        desired.replace("GHerrit", "Gherrit"),
    ] {
        assert!(!bodies_equal(&desired, &changed));
    }
    assert!(!bodies_equal("é", "e\u{301}"));

    let spec = EntrySpec {
        id: "Gone",
        history: HistorySpec::current(oid(20), oid(10), true),
        pull_request: PullRequestSpec::Open(OpenSpec {
            number: 7,
            node: "PR_7".to_owned(),
            head: oid(20),
            base_kind: BaseKind::Default,
            base: oid(10),
            title: None,
            body: Some(desired.replace('\n', "\r\n")),
            is_draft: true,
            landing_automation: false,
        }),
    };
    let stage = ready(plan(&[spec]).unwrap());
    assert!(stage.projection.projection_operations_for_test().is_empty());
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum EffectBoundary {
    InitialRefs,
    Creates,
    Markers,
    Projection,
}

#[derive(Clone, Debug, Eq, PartialEq, serde::Serialize)]
struct GitBatchAttempt {
    options: Box<[String]>,
    refspecs: Box<[String]>,
}

#[derive(Clone, Debug, Eq, PartialEq, serde::Serialize)]
struct CreateAttempt {
    id: String,
    title: String,
    body: Box<[String]>,
    head: String,
    base: String,
}

#[derive(Clone, Debug, Eq, PartialEq, serde::Serialize)]
struct UpdateAttempt {
    number: u32,
    node_id: String,
    title: Option<String>,
    body: Option<Box<[String]>>,
    base_branch: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq, serde::Serialize)]
enum ProjectionAttempt {
    Close { number: u32, node_id: String },
    Update(UpdateAttempt),
}

#[derive(Clone, Debug, Eq, PartialEq, serde::Serialize)]
enum DurableEffectAttempt {
    InitialRefs(Box<[GitBatchAttempt]>),
    Creates(Box<[CreateAttempt]>),
    Markers(Box<[GitBatchAttempt]>),
    Projection(Box<[ProjectionAttempt]>),
}

impl DurableEffectAttempt {
    fn boundary(&self) -> EffectBoundary {
        match self {
            Self::InitialRefs(_) => EffectBoundary::InitialRefs,
            Self::Creates(_) => EffectBoundary::Creates,
            Self::Markers(_) => EffectBoundary::Markers,
            Self::Projection(_) => EffectBoundary::Projection,
        }
    }
}

/// A deterministic driver which can fail exactly once at one reached stage.
///
/// The adapter tests own distinctions between rejected and indeterminate
/// acknowledgements. This driver verifies only that any returned error stops
/// the consuming stage machine before a later effect or same-attempt retry.
struct ScriptedEffectDriver {
    failure: Option<EffectBoundary>,
    attempts: Vec<DurableEffectAttempt>,
}

impl ScriptedEffectDriver {
    fn new(failure: Option<EffectBoundary>) -> Self {
        Self { failure, attempts: Vec::new() }
    }

    fn record(&mut self, attempt: DurableEffectAttempt) -> Result<()> {
        let boundary = attempt.boundary();
        self.attempts.push(attempt);
        if self.failure == Some(boundary) {
            self.failure = None;
            return Err(color_eyre::eyre::eyre!("injected failure at {boundary:?}"));
        }
        Ok(())
    }

    fn assert_consumed(&self) {
        assert_eq!(self.failure, None, "the configured failure was not reached");
    }

    fn push_attempts(pushes: &PreparedPushes) -> Box<[GitBatchAttempt]> {
        pushes
            .batches()
            .map(|batch| GitBatchAttempt {
                options: batch.options().map(str::to_owned).collect(),
                refspecs: batch.refspecs().map(str::to_owned).collect(),
            })
            .collect()
    }

    fn body_lines(body: &str) -> Box<[String]> {
        body.split('\n').map(str::to_owned).collect()
    }
}

impl EffectDriver for ScriptedEffectDriver {
    async fn publish_initial_refs(&mut self, pushes: PreparedPushes) -> Result<()> {
        let attempts = Self::push_attempts(&pushes);
        if attempts.is_empty() {
            Ok(())
        } else {
            self.record(DurableEffectAttempt::InitialRefs(attempts))
        }
    }

    async fn create_pull_requests(
        &mut self,
        creates: PreparedCreates,
    ) -> Result<CompleteCreateReceipts> {
        let operations = creates.operations_for_test();
        assert!(!operations.is_empty(), "a create stage must contain a create");
        let receipts = operations
            .iter()
            .enumerate()
            .map(|(index, operation)| {
                let number = 100 + u32::try_from(index).unwrap();
                (operation.id.clone(), identity(number, &format!("CREATED_PULL_REQUEST_{number}")))
            })
            .collect();
        let attempts = operations
            .iter()
            .map(|operation| CreateAttempt {
                id: operation.id.as_str().to_owned(),
                title: operation.title.clone(),
                body: Self::body_lines(&operation.body),
                head: operation.head_oid.to_string(),
                base: operation.base_oid.to_string(),
            })
            .collect();
        self.record(DurableEffectAttempt::Creates(attempts))?;
        Ok(CompleteCreateReceipts::for_plan_test(receipts))
    }

    async fn publish_markers(&mut self, pushes: PreparedPushes) -> Result<()> {
        let attempts = Self::push_attempts(&pushes);
        if attempts.is_empty() {
            Ok(())
        } else {
            self.record(DurableEffectAttempt::Markers(attempts))
        }
    }

    async fn project_pull_requests(
        &mut self,
        projection: PreparedPullRequestProjection,
    ) -> Result<()> {
        let attempts = projection
            .projection_operations_for_test()
            .iter()
            .map(|operation| match operation {
                TestPullRequestProjection::Close(close) => ProjectionAttempt::Close {
                    number: close.identity.number().get(),
                    node_id: close.identity.node_id_for_test().to_owned(),
                },
                TestPullRequestProjection::Update(update) => {
                    ProjectionAttempt::Update(UpdateAttempt {
                        number: update.identity.number().get(),
                        node_id: update.identity.node_id_for_test().to_owned(),
                        title: update.title.clone(),
                        body: update.body.as_deref().map(Self::body_lines),
                        base_branch: update.base_branch.clone(),
                    })
                }
            })
            .collect::<Box<[_]>>();
        if attempts.is_empty() {
            Ok(())
        } else {
            self.record(DurableEffectAttempt::Projection(attempts))
        }
    }
}

fn fresh_execution_plan() -> PlannedPublication {
    plan_with_public_branch(
        &[EntrySpec {
            id: "Gfresh",
            history: HistorySpec::absent(),
            pull_request: PullRequestSpec::Absent,
        }],
        Some("release-candidate".to_owned()),
    )
    .unwrap()
}

fn all_existing_execution_plan() -> PlannedPublication {
    plan(&[EntrySpec {
        id: "Gexisting",
        history: HistorySpec { published: vec![(oid(101), oid(10))], marker: false },
        pull_request: PullRequestSpec::Open(OpenSpec::new(7, oid(101), BaseKind::Owned, oid(10))),
    }])
    .unwrap()
}

async fn execute_scripted(
    plan: PlannedPublication,
    failure: Option<EffectBoundary>,
) -> (Result<()>, ScriptedEffectDriver) {
    let mut driver = ScriptedEffectDriver::new(failure);
    let result = plan.execute_with(&mut driver).await;
    (result, driver)
}

#[tokio::test]
async fn durable_effect_barriers_release_only_the_next_reachable_stage() {
    let (result, acknowledged) = execute_scripted(fresh_execution_plan(), None).await;
    result.unwrap();
    acknowledged.assert_consumed();
    assert_eq!(
        acknowledged.attempts.iter().map(DurableEffectAttempt::boundary).collect::<Vec<_>>(),
        [
            EffectBoundary::InitialRefs,
            EffectBoundary::Creates,
            EffectBoundary::Markers,
            EffectBoundary::Projection,
        ]
    );
    insta::assert_yaml_snapshot!("acknowledged_publication_effects", acknowledged.attempts);

    for boundary in [
        EffectBoundary::InitialRefs,
        EffectBoundary::Creates,
        EffectBoundary::Markers,
        EffectBoundary::Projection,
    ] {
        let (result, interrupted) = execute_scripted(fresh_execution_plan(), Some(boundary)).await;
        let error = result.expect_err("the configured durable effect must interrupt the attempt");
        interrupted.assert_consumed();
        let prefix_len = acknowledged
            .attempts
            .iter()
            .position(|attempt| attempt.boundary() == boundary)
            .unwrap()
            + 1;
        assert_eq!(interrupted.attempts, acknowledged.attempts[..prefix_len]);
        assert!(error.to_string().contains(&format!("{boundary:?}")));
    }
}

#[tokio::test]
async fn all_existing_publication_skips_create_without_reordering_effects() {
    let (result, driver) = execute_scripted(all_existing_execution_plan(), None).await;
    result.unwrap();
    driver.assert_consumed();
    assert_eq!(
        driver.attempts.iter().map(DurableEffectAttempt::boundary).collect::<Vec<_>>(),
        [EffectBoundary::InitialRefs, EffectBoundary::Markers, EffectBoundary::Projection]
    );
    insta::assert_yaml_snapshot!("all_existing_publication_effects", driver.attempts);
}

#[tokio::test]
async fn empty_effect_stages_cross_without_attempting_a_durable_write() {
    let plan = plan(&[EntrySpec {
        id: "Gone",
        history: HistorySpec::current(oid(20), oid(10), true),
        pull_request: PullRequestSpec::Open(OpenSpec {
            number: 7,
            node: "PR_7".to_owned(),
            head: oid(20),
            base_kind: BaseKind::Default,
            base: oid(10),
            title: None,
            body: Some(single_desired_body()),
            is_draft: true,
            landing_automation: false,
        }),
    }])
    .unwrap();
    let (result, driver) = execute_scripted(plan, None).await;
    result.unwrap();
    driver.assert_consumed();
    assert!(driver.attempts.is_empty());
}

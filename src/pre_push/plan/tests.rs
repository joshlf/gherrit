use std::fmt::Write as _;

use gix::ObjectId;
use serde_json::{Value, json};
use tempfile::TempDir;

use super::*;
use crate::{
    pre_push::{
        destination::PushDestination,
        github::{
            OpenObservation, TerminalPullRequest, TerminalPullRequestEvidence,
            TerminalPullRequestPage, TerminalPullRequestState,
        },
        pull_request::{PullRequestIdentity, TerminalExhaustionAccumulator, TerminalHistories},
        remote::{
            RemoteHeads, parse_remote_heads_for_destination_for_test, parse_remote_heads_for_test,
        },
    },
    util,
};

struct TestRepository {
    directory: TempDir,
    writer: gix::Repository,
}

impl TestRepository {
    fn new() -> Self {
        let directory = tempfile::tempdir().unwrap();
        let writer = gix::init_bare(directory.path()).unwrap();
        Self { directory, writer }
    }

    fn commit(&self, subject: &str, parents: &[ObjectId], id: Option<&str>) -> ObjectId {
        let message = match id {
            Some(id) => format!("{subject}\n\ngherrit-pr-id: {id}\n"),
            None => subject.to_owned(),
        };
        let signature = gix::actor::Signature {
            name: "GHerrit test".into(),
            email: "test@example.com".into(),
            time: gix::actor::date::Time::new(0, 0),
        };
        self.writer
            .write_object(&gix::objs::Commit {
                tree: ObjectId::empty_tree(self.writer.object_hash()),
                parents: parents.iter().copied().collect(),
                author: signature.clone(),
                committer: signature,
                encoding: None,
                message: message.into(),
                extra_headers: Vec::new(),
            })
            .unwrap()
            .detach()
    }

    fn graph(&self, roots: impl IntoIterator<Item = ObjectId>) -> CommitGraphEvidence {
        let repository = util::Repo::open(self.directory.path().to_str().unwrap()).unwrap();
        CommitGraphEvidence::load(&repository, roots).unwrap()
    }
}

fn id(value: &str) -> GherritPrId {
    GherritPrId::from_ref_component(value.as_bytes()).unwrap()
}

fn head_advertisement(default: ObjectId, managed: &[(&str, ObjectId, ObjectId)]) -> String {
    let mut output =
        format!("ref: refs/heads/main\tHEAD\n{default}\tHEAD\n{default}\trefs/heads/main\n");
    for (id, head, base) in managed {
        writeln!(output, "{head}\trefs/heads/{id}").unwrap();
        writeln!(output, "{base}\trefs/heads/gherrit-bases/{id}").unwrap();
    }
    output
}

fn heads(default: ObjectId, managed: &[(&str, ObjectId, ObjectId)]) -> RemoteHeads<'static> {
    parse_remote_heads_for_test(head_advertisement(default, managed).as_bytes()).unwrap()
}

fn heads_for<'destination>(
    destination: &'destination PushDestination,
    default: ObjectId,
    managed: &[(&str, ObjectId, ObjectId)],
) -> RemoteHeads<'destination> {
    parse_remote_heads_for_destination_for_test(
        destination,
        head_advertisement(default, managed).as_bytes(),
    )
    .unwrap()
}

fn versions(entries: &[(&str, u64, ObjectId)]) -> String {
    entries
        .iter()
        .map(|(id, version, oid)| format!("{oid}\trefs/tags/gherrit/{id}/v{version}\n"))
        .collect()
}

fn open_node(
    number: u64,
    id: &str,
    base: &str,
    base_oid: ObjectId,
    head_oid: ObjectId,
    landing: bool,
) -> Value {
    json!({
        "number": number,
        "id": format!("PR_{number}"),
        "title": "Title",
        "body": "Observed body",
        "baseRefName": base,
        "baseRefOid": base_oid.to_string(),
        "headRefName": id,
        "headRefOid": head_oid.to_string(),
        "state": "OPEN",
        "isCrossRepository": false,
        "autoMergeRequest": if landing { json!({ "enabledAt": "now" }) } else { Value::Null },
        "isInMergeQueue": false,
    })
}

fn open_response(default: ObjectId, nodes: Vec<Value>) -> Value {
    json!({
        "data": {
            "repository": {
                "id": "R_repository",
                "defaultBranchRef": {
                    "name": "main",
                    "target": { "oid": default.to_string() },
                },
                "pullRequests": {
                    "nodes": nodes,
                    "pageInfo": { "hasNextPage": false, "endCursor": null },
                },
            },
        },
    })
}

fn exact_terminal_histories(ids: &[GherritPrId]) -> TerminalHistories {
    let mut accumulator = TerminalExhaustionAccumulator::new(ids.iter().cloned()).unwrap();
    for id in ids {
        accumulator = accumulator
            .record_page(TerminalPullRequestEvidence::for_test(
                id.clone(),
                None,
                TerminalPullRequestPage { pull_requests: Vec::new(), next_cursor: None },
            ))
            .unwrap();
    }
    accumulator.into_terminal_histories().unwrap()
}

fn empty_terminal_histories(
    active: &ActiveRemoteChanges<'_>,
    ids: &[GherritPrId],
) -> RepositoryTerminalHistories {
    RepositoryTerminalHistories::for_test(active.destination(), exact_terminal_histories(ids))
}

fn retired_terminal_history(
    active: &ActiveRemoteChanges<'_>,
    id: GherritPrId,
    number: u64,
    state: TerminalPullRequestState,
) -> RepositoryTerminalHistories {
    let histories = TerminalExhaustionAccumulator::new([id.clone()])
        .unwrap()
        .record_page(TerminalPullRequestEvidence::for_test(
            id,
            None,
            TerminalPullRequestPage {
                pull_requests: vec![TerminalPullRequest {
                    number,
                    node_id: format!("PR_{number}"),
                    state,
                }],
                next_cursor: None,
            },
        ))
        .unwrap()
        .into_terminal_histories()
        .unwrap();
    RepositoryTerminalHistories::for_test(active.destination(), histories)
}

fn correlate_and_activate<'destination>(
    remote_heads: RemoteHeads<'destination>,
    stack: &LocalStack,
    response: Value,
    version_output: &str,
) -> (CorrelatedRepository<'destination>, ActiveRemoteChanges<'destination>) {
    let ids = stack.iter().map(|change| change.id().clone()).collect::<Vec<_>>();
    let open =
        OpenObservation::from_complete_response_for_test("owner", "repository", response).unwrap();
    let correlated = open.correlate(ids.iter(), &remote_heads).unwrap();
    let active = remote_heads.into_active_for_test(&ids, &[], version_output.as_bytes()).unwrap();
    (correlated, active)
}

fn correlate_and_activate_with_nonlocal<'destination>(
    remote_heads: RemoteHeads<'destination>,
    stack: &LocalStack,
    nonlocal_ids: &[GherritPrId],
    response: Value,
    version_output: &str,
) -> (CorrelatedRepository<'destination>, ActiveRemoteChanges<'destination>) {
    let local_ids = stack.iter().map(|change| change.id().clone()).collect::<Vec<_>>();
    let open =
        OpenObservation::from_complete_response_for_test("owner", "repository", response).unwrap();
    let correlated = open.correlate(local_ids.iter(), &remote_heads).unwrap();
    let active = remote_heads
        .into_active_for_test(&local_ids, nonlocal_ids, version_output.as_bytes())
        .unwrap();
    (correlated, active)
}

fn first_publication_plan<'destination>(
    repository: &TestRepository,
    destination: &'destination PushDestination,
) -> PublicationPlan<'destination> {
    let default = repository.commit("root", &[], None);
    let proposed = repository.commit("change", &[default], Some("Gone"));
    let stack = LocalStack::for_test_with_content(
        default,
        [(id("Gone"), proposed, "Title".to_owned(), "Commit body".to_owned())],
    )
    .unwrap();
    let remote_heads = heads_for(destination, default, &[]);
    let (correlated, active) =
        correlate_and_activate(remote_heads, &stack, open_response(default, Vec::new()), "");
    let context = BodyLinkContext::from_destination(active.destination(), None).unwrap();
    plan_publication(
        context,
        stack,
        correlated,
        empty_terminal_histories(&active, &[id("Gone")]),
        active,
        &repository.graph([proposed]),
    )
    .unwrap()
}

fn missing_root_plan(repository: &TestRepository, value: &str) -> PublicationPlan<'static> {
    let default = repository.commit("root", &[], None);
    let proposed = repository.commit("change", &[default], Some(value));
    let stack = LocalStack::for_test_with_content(
        default,
        [(id(value), proposed, "Title".to_owned(), "Commit body".to_owned())],
    )
    .unwrap();
    let remote_heads = heads(default, &[]);
    let (correlated, active) =
        correlate_and_activate(remote_heads, &stack, open_response(default, Vec::new()), "");
    let context = BodyLinkContext::from_destination(active.destination(), None).unwrap();
    plan_publication(
        context,
        stack,
        correlated,
        empty_terminal_histories(&active, &[id(value)]),
        active,
        &repository.graph([proposed]),
    )
    .unwrap()
}

fn missing_nonroot_plan(repository: &TestRepository, value: &str) -> PublicationPlan<'static> {
    let default = repository.commit("root", &[], None);
    let parent = repository.commit("parent", &[default], Some("Gparent"));
    let proposed = repository.commit("change", &[parent], Some(value));
    let stack = LocalStack::for_test_with_content(
        default,
        [
            (id("Gparent"), parent, "Parent title".to_owned(), String::new()),
            (id(value), proposed, "Title".to_owned(), "Commit body".to_owned()),
        ],
    )
    .unwrap();
    let remote_heads = heads(default, &[("Gparent", parent, default)]);
    let response =
        open_response(default, vec![open_node(7, "Gparent", "main", default, parent, false)]);
    let (correlated, active) = correlate_and_activate(
        remote_heads,
        &stack,
        response,
        &versions(&[("Gparent", 1, parent)]),
    );
    let context = BodyLinkContext::from_destination(active.destination(), None).unwrap();
    plan_publication(
        context,
        stack,
        correlated,
        empty_terminal_histories(&active, &[id(value)]),
        active,
        &repository.graph([proposed]),
    )
    .unwrap()
}

fn requests_for_test(plan: &PublicationPlan<'_>) -> Vec<(Vec<String>, Vec<String>)> {
    plan.push_arguments_for_test()
}

fn into_projection_for_test(plan: PublicationPlan<'_>) -> ReadyProjection {
    plan.projection
}

fn only_query(request_text: String) -> String {
    assert_eq!(request_text.lines().count(), 1, "expected exactly one mutation request");
    serde_json::from_str::<Value>(&request_text).unwrap()["query"].as_str().unwrap().to_owned()
}

fn serialized_create_key(query: &str) -> &str {
    let start = query.find("repositoryId:").expect("create request has a repository ID");
    let end = start
        + query[start..]
            .find(", title:")
            .expect("create request has a title after its repository/head/base key");
    &query[start..end]
}

#[cfg(unix)]
fn fake_git_destination(script: &str) -> (TempDir, PushDestination, std::path::PathBuf) {
    use std::os::unix::fs::PermissionsExt as _;

    let directory = tempfile::tempdir().unwrap();
    let executable = directory.path().join("git");
    let argument_log = directory.path().join("arguments");
    std::fs::write(&executable, script).unwrap();
    let mut permissions = std::fs::metadata(&executable).unwrap().permissions();
    permissions.set_mode(0o700);
    std::fs::set_permissions(&executable, permissions).unwrap();
    let environment = vec![
        (std::ffi::OsString::from("PATH"), directory.path().as_os_str().to_owned()),
        (std::ffi::OsString::from("GHERRIT_TEST_ARGV"), argument_log.as_os_str().to_owned()),
    ];
    let destination =
        PushDestination::for_test("origin", "https://github.com/owner/repository.git", environment)
            .unwrap();
    (directory, destination, argument_log)
}

#[cfg(unix)]
const SUCCESSFUL_PUSH: &str = r#"#!/bin/sh
: > "$GHERRIT_TEST_ARGV"
for argument in "$@"; do
    printf '%s\n' "$argument" >> "$GHERRIT_TEST_ARGV"
done
printf 'To private-destination\n'
for argument in "$@"; do
    case "$argument" in
        *:refs/heads/*|*:refs/tags/*)
            printf '*\t%s\t[new reference]\n' "$argument"
            ;;
    esac
done
printf 'Done\n'
"#;

#[cfg(unix)]
const INDETERMINATE_PUSH: &str = r#"#!/bin/sh
: > "$GHERRIT_TEST_ARGV"
printf 'To private-destination\nDone\n'
"#;

#[test]
fn first_publication_hides_create_and_update_work_behind_git_publication() {
    let repository = TestRepository::new();
    let default = repository.commit("root", &[], None);
    let proposed = repository.commit("change", &[default], Some("Gone"));
    let stack = LocalStack::for_test_with_content(
        default,
        [(id("Gone"), proposed, "Title".to_owned(), "Commit body".to_owned())],
    )
    .unwrap();
    let remote_heads = heads(default, &[]);
    let (correlated, active) =
        correlate_and_activate(remote_heads, &stack, open_response(default, Vec::new()), "");
    let context = BodyLinkContext::from_destination(active.destination(), None).unwrap();
    let plan = plan_publication(
        context,
        stack,
        correlated,
        empty_terminal_histories(&active, &[id("Gone")]),
        active,
        &repository.graph([proposed]),
    )
    .unwrap();

    let git = plan;
    let requests = requests_for_test(&git);
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].1.len(), 3);
    let ReadyProjection::Creates { creates, projection } = into_projection_for_test(git) else {
        panic!("first publication must create its pull request");
    };
    assert_eq!(creates.operation_count(), 1);
    let receipts = creates
        .complete_for_test(vec![(
            id("Gone"),
            PullRequestIdentity::new(42, "PR_42".to_owned()).unwrap(),
        )])
        .unwrap();
    let updates = projection.complete(receipts).unwrap();
    assert_eq!(updates.operation_count(), 1);
    assert!(updates.request_text().contains("updatePullRequest"));
}

#[test]
fn new_root_uses_owned_create_key_then_moves_to_the_default_base() {
    let repository = TestRepository::new();
    let plan = missing_root_plan(&repository, "Gone");
    let ReadyProjection::Creates { creates, projection } = into_projection_for_test(plan) else {
        panic!("the missing root must be created");
    };

    let create = only_query(creates.request_text());
    assert!(create.contains("headRefName: \"Gone\""));
    assert!(create.contains("baseRefName: \"gherrit-bases/Gone\""));
    assert!(!create.contains("baseRefName: \"main\""));

    let receipts = creates
        .complete_for_test(vec![(
            id("Gone"),
            PullRequestIdentity::new(42, "PR_42".to_owned()).unwrap(),
        )])
        .unwrap();
    let update = only_query(projection.complete(receipts).unwrap().request_text());
    assert!(update.contains("body:"));
    assert!(update.contains("#42"));
    assert!(update.contains("baseRefName: \"main\""));
    assert!(!update.contains("baseRefName: \"gherrit-bases/Gone\""));
}

#[test]
fn new_nonroot_uses_owned_create_key_without_a_redundant_base_update() {
    let repository = TestRepository::new();
    let plan = missing_nonroot_plan(&repository, "Gone");
    let ReadyProjection::Creates { creates, projection } = into_projection_for_test(plan) else {
        panic!("the missing nonroot must be created");
    };

    let create = only_query(creates.request_text());
    assert!(create.contains("headRefName: \"Gone\""));
    assert!(create.contains("baseRefName: \"gherrit-bases/Gone\""));
    assert!(!create.contains("baseRefName: \"main\""));

    let receipts = creates
        .complete_for_test(vec![(
            id("Gone"),
            PullRequestIdentity::new(42, "PR_42".to_owned()).unwrap(),
        )])
        .unwrap();
    let update = only_query(projection.complete(receipts).unwrap().request_text());
    assert!(update.contains("body:"));
    assert!(update.contains("#42"));
    assert!(!update.contains("baseRefName:"));
}

#[test]
fn root_status_does_not_change_the_create_repository_head_base_key() {
    let root_repository = TestRepository::new();
    let root = missing_root_plan(&root_repository, "Gone");
    let ReadyProjection::Creates { creates: root, .. } = into_projection_for_test(root) else {
        panic!("the missing root must be created");
    };
    let root = only_query(root.request_text());

    let nonroot_repository = TestRepository::new();
    let nonroot = missing_nonroot_plan(&nonroot_repository, "Gone");
    let ReadyProjection::Creates { creates: nonroot, .. } = into_projection_for_test(nonroot)
    else {
        panic!("the missing nonroot must be created");
    };
    let nonroot = only_query(nonroot.request_text());

    assert_eq!(serialized_create_key(&root), serialized_create_key(&nonroot));
    assert_eq!(
        serialized_create_key(&root),
        "repositoryId: \"R_repository\", baseRefName: \"gherrit-bases/Gone\", headRefName: \"Gone\""
    );
}

#[test]
fn create_and_response_derived_update_preflight_at_their_typed_boundaries() {
    let repository = TestRepository::new();
    let default = repository.commit("root", &[], None);
    let proposed = repository.commit("change", &[default], Some("Gone"));
    let stack = LocalStack::for_test_with_content(
        default,
        [(id("Gone"), proposed, "Title".to_owned(), String::new())],
    )
    .unwrap();
    let remote_heads = heads(default, &[]);
    let mut response = open_response(default, Vec::new());
    response["data"]["repository"]["id"] = json!("R".repeat(1_100_000));
    let (correlated, active) = correlate_and_activate(remote_heads, &stack, response, "");
    let context = BodyLinkContext::from_destination(active.destination(), None).unwrap();
    let error = match plan_publication(
        context,
        stack,
        correlated,
        empty_terminal_histories(&active, &[id("Gone")]),
        active,
        &repository.graph([proposed]),
    ) {
        Ok(_) => panic!("an oversized exact create request must not escape planning"),
        Err(error) => error,
    };
    assert!(error.to_string().contains("GraphQL create mutation"));
    assert!(error.to_string().contains("exceeds the 1048576-byte request limit"));

    let repository = TestRepository::new();
    let plan = missing_root_plan(&repository, "Gone");
    let ReadyProjection::Creates { creates, projection } = into_projection_for_test(plan) else {
        panic!("the missing root must be created");
    };
    let receipts = creates
        .complete_for_test(vec![(
            id("Gone"),
            PullRequestIdentity::new(42, "N".repeat(1_100_000)).unwrap(),
        )])
        .unwrap();
    let error = projection.complete(receipts).unwrap_err();
    assert!(error.to_string().contains("GraphQL update mutation"));
    assert!(error.to_string().contains("exceeds the 1048576-byte request limit"));
}

#[cfg(unix)]
#[tokio::test(flavor = "current_thread")]
async fn pending_publication_executes_the_exact_request_before_releasing_projection() {
    let (_directory, destination, argument_log) = fake_git_destination(SUCCESSFUL_PUSH);
    let repository = TestRepository::new();
    let plan = first_publication_plan(&repository, &destination);
    let requests = requests_for_test(&plan);
    assert_eq!(requests.len(), 1);
    let expected_options = requests[0].0.clone();
    let expected_refspecs = requests[0].1.clone();

    assert!(matches!(plan.publish().await.unwrap(), ReadyProjection::Creates { .. }));

    let arguments = std::fs::read_to_string(argument_log)
        .unwrap()
        .lines()
        .map(str::to_owned)
        .collect::<Vec<_>>();
    let push = arguments.iter().position(|argument| argument == "push").unwrap();
    let separator = arguments[push + 1..]
        .iter()
        .position(|argument| argument == "--")
        .map(|offset| push + 1 + offset)
        .unwrap();
    assert_eq!(&arguments[push + 1..separator], expected_options);
    assert_eq!(arguments[separator + 1], "gherrit-publication");
    assert_eq!(&arguments[separator + 2..], expected_refspecs);
}

#[cfg(unix)]
#[tokio::test(flavor = "current_thread")]
async fn pending_publication_does_not_release_projection_for_indeterminate_receipts() {
    let (_directory, destination, _argument_log) = fake_git_destination(INDETERMINATE_PUSH);
    let repository = TestRepository::new();
    let plan = first_publication_plan(&repository, &destination);

    let error = plan.publish().await.unwrap_err();

    assert!(
        error.to_string().contains("Could not acknowledge `git push` for GHerrit remote 'origin'")
    );
}

#[tokio::test(flavor = "current_thread")]
async fn published_without_open_pr_is_create_recovery_with_a_ready_no_op() {
    let repository = TestRepository::new();
    let default = repository.commit("root", &[], None);
    let published = repository.commit("change", &[default], Some("Gone"));
    let stack = LocalStack::for_test_with_content(
        default,
        [(id("Gone"), published, "Title".to_owned(), String::new())],
    )
    .unwrap();
    let remote_heads = heads(default, &[("Gone", published, default)]);
    let history = versions(&[("Gone", 1, published)]);
    let (correlated, active) =
        correlate_and_activate(remote_heads, &stack, open_response(default, Vec::new()), &history);
    let context = BodyLinkContext::from_destination(active.destination(), None).unwrap();
    let plan = plan_publication(
        context,
        stack,
        correlated,
        empty_terminal_histories(&active, &[id("Gone")]),
        active,
        &repository.graph([published]),
    )
    .unwrap();

    assert!(requests_for_test(&plan).is_empty());
    assert!(matches!(plan.publish().await.unwrap(), ReadyProjection::Creates { .. }));
}

#[test]
fn missing_open_terminal_history_table_is_exact() {
    for terminal in
        [None, Some(TerminalPullRequestState::Closed), Some(TerminalPullRequestState::Merged)]
    {
        let repository = TestRepository::new();
        let default = repository.commit("root", &[], None);
        let published = repository.commit("change", &[default], Some("Gone"));
        let stack = LocalStack::for_test(default, [(id("Gone"), published)]);
        let remote_heads = heads(default, &[("Gone", published, default)]);
        let history = versions(&[("Gone", 1, published)]);
        let (correlated, active) = correlate_and_activate(
            remote_heads,
            &stack,
            open_response(default, Vec::new()),
            &history,
        );
        let terminal_histories = terminal.map_or_else(
            || empty_terminal_histories(&active, &[id("Gone")]),
            |state| retired_terminal_history(&active, id("Gone"), 17, state),
        );
        let context = BodyLinkContext::from_destination(active.destination(), None).unwrap();
        let result = plan_publication(
            context,
            stack,
            correlated,
            terminal_histories,
            active,
            &repository.graph([published]),
        );

        assert_eq!(result.is_ok(), terminal.is_none(), "terminal={terminal:?}");
        match result {
            Ok(plan) => {
                assert!(matches!(into_projection_for_test(plan), ReadyProjection::Creates { .. }));
            }
            Err(error) => {
                let state = match terminal.unwrap() {
                    TerminalPullRequestState::Closed => "closed",
                    TerminalPullRequestState::Merged => "merged",
                };
                assert_eq!(
                    error.to_string(),
                    format!(
                        "Cannot push to {state} PR #17. Please open a new PR or reopen the existing one.\n\
                         You may want to rebase on the latest changes before pushing."
                    )
                );
            }
        }
    }
}

#[test]
fn planner_aggregates_retired_histories_in_requested_order() {
    let repository = TestRepository::new();
    let default = repository.commit("root", &[], None);
    let empty = repository.commit("empty", &[default], Some("Gempty"));
    let closed = repository.commit("closed", &[empty], Some("Gclosed"));
    let merged = repository.commit("merged", &[closed], Some("Gmerged"));
    let ids = [id("Gempty"), id("Gclosed"), id("Gmerged")];
    let stack = LocalStack::for_test(
        default,
        [(ids[0].clone(), empty), (ids[1].clone(), closed), (ids[2].clone(), merged)],
    );
    let remote_heads = heads(default, &[]);
    let (correlated, active) =
        correlate_and_activate(remote_heads, &stack, open_response(default, Vec::new()), "");
    let terminal_histories = TerminalExhaustionAccumulator::new(ids.iter().cloned())
        .unwrap()
        .record_page(TerminalPullRequestEvidence::for_test(
            ids[2].clone(),
            None,
            TerminalPullRequestPage {
                pull_requests: vec![TerminalPullRequest {
                    number: 22,
                    node_id: "PR_22".to_owned(),
                    state: TerminalPullRequestState::Merged,
                }],
                next_cursor: None,
            },
        ))
        .unwrap()
        .record_page(TerminalPullRequestEvidence::for_test(
            ids[0].clone(),
            None,
            TerminalPullRequestPage { pull_requests: Vec::new(), next_cursor: None },
        ))
        .unwrap()
        .record_page(TerminalPullRequestEvidence::for_test(
            ids[1].clone(),
            None,
            TerminalPullRequestPage {
                pull_requests: vec![TerminalPullRequest {
                    number: 11,
                    node_id: "PR_11".to_owned(),
                    state: TerminalPullRequestState::Closed,
                }],
                next_cursor: None,
            },
        ))
        .unwrap()
        .into_terminal_histories()
        .unwrap();
    let terminal_histories =
        RepositoryTerminalHistories::for_test(active.destination(), terminal_histories);
    let context = BodyLinkContext::from_destination(active.destination(), None).unwrap();

    let error = match plan_publication(
        context,
        stack,
        correlated,
        terminal_histories,
        active,
        &repository.graph([merged]),
    ) {
        Ok(_) => panic!("retired histories must reject before publication planning"),
        Err(error) => error,
    };

    assert_eq!(
        error.to_string(),
        "Cannot push to closed PR #11. Please open a new PR or reopen the existing one.\n\
         Cannot push to merged PR #22. Please open a new PR or reopen the existing one.\n\
         You may want to rebase on the latest changes before pushing."
    );
}

#[test]
fn correlation_and_active_evidence_require_the_same_destination_capability() {
    let repository = TestRepository::new();
    let default = repository.commit("root", &[], None);
    let proposed = repository.commit("change", &[default], Some("Gone"));
    let stack = LocalStack::for_test(default, [(id("Gone"), proposed)]);
    let correlation_heads = heads(default, &[]);
    let active_heads = heads(default, &[]);
    let ids = [id("Gone")];
    let open = OpenObservation::from_complete_response_for_test(
        "owner",
        "repository",
        open_response(default, Vec::new()),
    )
    .unwrap();
    let correlated = open.correlate(ids.iter(), &correlation_heads).unwrap();
    let active = active_heads.into_active_for_test(&ids, &[], b"").unwrap();
    let context = BodyLinkContext::from_destination(active.destination(), None).unwrap();
    let error = match plan_publication(
        context,
        stack,
        correlated,
        empty_terminal_histories(&active, &ids),
        active,
        &repository.graph([proposed]),
    ) {
        Ok(_) => panic!("different push-destination capabilities must not be joined"),
        Err(error) => error,
    };

    assert!(error.to_string().contains("different push-destination capabilities"));
}

#[test]
fn terminal_history_for_another_repository_cannot_enter_planning() {
    let repository = TestRepository::new();
    let default = repository.commit("root", &[], None);
    let proposed = repository.commit("change", &[default], Some("Gone"));
    let stack = LocalStack::for_test(default, [(id("Gone"), proposed)]);
    let remote_heads = heads(default, &[]);
    let (correlated, active) =
        correlate_and_activate(remote_heads, &stack, open_response(default, Vec::new()), "");
    let other_destination =
        PushDestination::for_test("other", "https://github.com/another/repository.git", Vec::new())
            .unwrap();
    let terminal_histories = RepositoryTerminalHistories::for_test(
        &other_destination,
        exact_terminal_histories(&[id("Gone")]),
    );
    let context = BodyLinkContext::from_destination(active.destination(), None).unwrap();
    let error = match plan_publication(
        context,
        stack,
        correlated,
        terminal_histories,
        active,
        &repository.graph([proposed]),
    ) {
        Ok(_) => panic!("another repository's terminal evidence must not enter planning"),
        Err(error) => error,
    };

    assert!(
        error
            .to_string()
            .contains("terminal pull request evidence came from a different GitHub repository")
    );
}

#[test]
fn retained_stack_default_must_match_the_agreed_publication_default() {
    let repository = TestRepository::new();
    let remote_default = repository.commit("remote root", &[], None);
    let local_default = repository.commit("local root", &[], None);

    for (label, stack_default, parent, diagnostic) in [
        (
            "name",
            DefaultBranch::new("trunk".to_owned(), remote_default).unwrap(),
            remote_default,
            "different default branch names",
        ),
        (
            "tip",
            DefaultBranch::new("main".to_owned(), local_default).unwrap(),
            local_default,
            "different default branch tips",
        ),
    ] {
        let proposed = repository.commit("change", &[parent], Some("Gone"));
        let stack = LocalStack::for_test_with_default(
            stack_default,
            [(id("Gone"), proposed, "Title".to_owned(), String::new())],
        )
        .unwrap();
        let remote_heads = heads(remote_default, &[]);
        let (correlated, active) = correlate_and_activate(
            remote_heads,
            &stack,
            open_response(remote_default, Vec::new()),
            "",
        );
        let context = BodyLinkContext::from_destination(active.destination(), None).unwrap();
        let error = match plan_publication(
            context,
            stack,
            correlated,
            empty_terminal_histories(&active, &[id("Gone")]),
            active,
            &repository.graph([proposed]),
        ) {
            Ok(_) => panic!("retained stack default mismatch must fail for {label}"),
            Err(error) => error,
        };

        assert!(error.to_string().contains(diagnostic), "case={label}: {error}");
    }
}

#[test]
fn an_open_pr_may_retain_an_older_published_head() {
    let repository = TestRepository::new();
    let default = repository.commit("root", &[], None);
    let first = repository.commit("first", &[default], Some("Gone"));
    let current = repository.commit("current", &[default], Some("Gone"));
    let stack = LocalStack::for_test_with_content(
        default,
        [(id("Gone"), current, "Title".to_owned(), String::new())],
    )
    .unwrap();
    let remote_heads = heads(default, &[("Gone", current, default)]);
    let history = versions(&[("Gone", 1, first), ("Gone", 2, current)]);
    let response = open_response(default, vec![open_node(7, "Gone", "main", default, first, true)]);
    let (correlated, active) = correlate_and_activate(remote_heads, &stack, response, &history);
    let context = BodyLinkContext::from_destination(active.destination(), None).unwrap();
    let plan = plan_publication(
        context,
        stack,
        correlated,
        empty_terminal_histories(&active, &[]),
        active,
        &repository.graph([current, first]),
    )
    .unwrap();

    let git = plan;
    assert!(requests_for_test(&git).is_empty());
    let ReadyProjection::Updates(updates) = into_projection_for_test(git) else {
        panic!("the stale body must be updated");
    };
    assert_eq!(updates.operation_count(), 1);
    let request = updates.request_text();
    assert!(request.contains("body:"));
    assert!(!request.contains("baseRefName:"));
    assert!(!request.contains("title:"));
}

#[test]
fn owned_pr_head_and_base_may_be_independent_published_versions() {
    let repository = TestRepository::new();
    let default = repository.commit("root", &[], None);
    let parent_first = repository.commit("parent first", &[default], Some("Gone"));
    let parent_current = repository.commit("parent current", &[default], Some("Gone"));
    let child_first = repository.commit("child first", &[parent_first], Some("Gtwo"));
    let child_current = repository.commit("child current", &[parent_current], Some("Gtwo"));
    let stack = LocalStack::for_test_with_content(
        default,
        [
            (id("Gone"), parent_current, "Title".to_owned(), String::new()),
            (id("Gtwo"), child_current, "Title".to_owned(), String::new()),
        ],
    )
    .unwrap();
    let remote_heads = heads(
        default,
        &[("Gone", parent_current, default), ("Gtwo", child_current, parent_current)],
    );
    let history = versions(&[
        ("Gone", 1, parent_first),
        ("Gone", 2, parent_current),
        ("Gtwo", 1, child_first),
        ("Gtwo", 2, child_current),
    ]);
    let response = open_response(
        default,
        vec![
            open_node(7, "Gone", "main", default, parent_first, false),
            open_node(8, "Gtwo", "gherrit-bases/Gtwo", parent_first, child_first, false),
        ],
    );
    let (correlated, active) = correlate_and_activate(remote_heads, &stack, response, &history);
    let context = BodyLinkContext::from_destination(active.destination(), None).unwrap();
    let plan = plan_publication(
        context,
        stack,
        correlated,
        empty_terminal_histories(&active, &[]),
        active,
        &repository.graph([child_first, child_current]),
    )
    .unwrap();

    assert!(requests_for_test(&plan).is_empty());
    assert!(matches!(into_projection_for_test(plan), ReadyProjection::Updates(_)));
}

#[test]
fn every_cross_version_owned_head_base_pair_is_accepted() {
    let repository = TestRepository::new();
    let default = repository.commit("root", &[], None);
    let parent_first = repository.commit("parent first", &[default], Some("Gone"));
    let parent_current = repository.commit("parent current", &[default], Some("Gone"));
    let child_first = repository.commit("child first", &[parent_first], Some("Gtwo"));
    let child_current = repository.commit("child current", &[parent_current], Some("Gtwo"));

    for observed_head in [child_first, child_current] {
        for observed_base in [parent_first, parent_current] {
            let stack = LocalStack::for_test_with_content(
                default,
                [
                    (id("Gone"), parent_current, "Title".to_owned(), String::new()),
                    (id("Gtwo"), child_current, "Title".to_owned(), String::new()),
                ],
            )
            .unwrap();
            let remote_heads = heads(
                default,
                &[("Gone", parent_current, default), ("Gtwo", child_current, parent_current)],
            );
            let history = versions(&[
                ("Gone", 1, parent_first),
                ("Gone", 2, parent_current),
                ("Gtwo", 1, child_first),
                ("Gtwo", 2, child_current),
            ]);
            let response = open_response(
                default,
                vec![
                    open_node(7, "Gone", "main", default, parent_first, false),
                    open_node(8, "Gtwo", "gherrit-bases/Gtwo", observed_base, observed_head, false),
                ],
            );
            let (correlated, active) =
                correlate_and_activate(remote_heads, &stack, response, &history);
            let context = BodyLinkContext::from_destination(active.destination(), None).unwrap();
            let plan = plan_publication(
                context,
                stack,
                correlated,
                empty_terminal_histories(&active, &[]),
                active,
                &repository.graph([child_first, child_current]),
            )
            .unwrap();

            assert!(requests_for_test(&plan).is_empty());
        }
    }
}

#[test]
fn all_nine_three_version_owned_head_base_pairs_are_accepted() {
    let repository = TestRepository::new();
    let default = repository.commit("root", &[], None);
    let parents = [
        repository.commit("parent one", &[default], Some("Gone")),
        repository.commit("parent two", &[default], Some("Gone")),
        repository.commit("parent three", &[default], Some("Gone")),
    ];
    let children = [
        repository.commit("child one", &[parents[0]], Some("Gtwo")),
        repository.commit("child two", &[parents[1]], Some("Gtwo")),
        repository.commit("child three", &[parents[2]], Some("Gtwo")),
    ];

    for observed_head in children {
        for observed_base in parents {
            let stack = LocalStack::for_test_with_content(
                default,
                [
                    (id("Gone"), parents[2], "Title".to_owned(), String::new()),
                    (id("Gtwo"), children[2], "Title".to_owned(), String::new()),
                ],
            )
            .unwrap();
            let remote_heads =
                heads(default, &[("Gone", parents[2], default), ("Gtwo", children[2], parents[2])]);
            let history = versions(&[
                ("Gone", 1, parents[0]),
                ("Gone", 2, parents[1]),
                ("Gone", 3, parents[2]),
                ("Gtwo", 1, children[0]),
                ("Gtwo", 2, children[1]),
                ("Gtwo", 3, children[2]),
            ]);
            let response = open_response(
                default,
                vec![
                    open_node(7, "Gone", "main", default, parents[0], false),
                    open_node(8, "Gtwo", "gherrit-bases/Gtwo", observed_base, observed_head, false),
                ],
            );
            let (correlated, active) =
                correlate_and_activate(remote_heads, &stack, response, &history);
            let context = BodyLinkContext::from_destination(active.destination(), None).unwrap();
            let plan = plan_publication(
                context,
                stack,
                correlated,
                empty_terminal_histories(&active, &[]),
                active,
                &repository.graph(children),
            )
            .unwrap();

            assert!(requests_for_test(&plan).is_empty());
        }
    }
}

#[test]
fn mixed_existing_create_projection_renders_final_navigation_for_both() {
    let repository = TestRepository::new();
    let default = repository.commit("root", &[], None);
    let parent = repository.commit("parent", &[default], Some("Gone"));
    let child = repository.commit("child", &[parent], Some("Gtwo"));
    let stack = LocalStack::for_test_with_content(
        default,
        [
            (id("Gone"), parent, "Title".to_owned(), String::new()),
            (id("Gtwo"), child, "Title".to_owned(), String::new()),
        ],
    )
    .unwrap();
    let remote_heads = heads(default, &[("Gone", parent, default)]);
    let history = versions(&[("Gone", 1, parent)]);
    let response =
        open_response(default, vec![open_node(7, "Gone", "main", default, parent, false)]);
    let (correlated, active) = correlate_and_activate(remote_heads, &stack, response, &history);
    let context = BodyLinkContext::from_destination(active.destination(), None).unwrap();
    let plan = plan_publication(
        context,
        stack,
        correlated,
        empty_terminal_histories(&active, &[id("Gtwo")]),
        active,
        &repository.graph([child]),
    )
    .unwrap();
    assert_eq!(requests_for_test(&plan).len(), 1);
    let ReadyProjection::Creates { creates, projection } = into_projection_for_test(plan) else {
        panic!("the mixed stack must create its missing child");
    };
    let receipts = creates
        .complete_for_test(vec![(
            id("Gtwo"),
            PullRequestIdentity::new(42, "PR_42".to_owned()).unwrap(),
        )])
        .unwrap();
    let updates = projection.complete(receipts).unwrap();
    assert_eq!(updates.operation_count(), 2);
    let request = updates.request_text();
    assert!(request.contains("#7"));
    assert!(request.contains("#42"));
}

#[test]
fn an_open_pr_without_published_history_is_not_a_create_recovery() {
    let repository = TestRepository::new();
    let default = repository.commit("root", &[], None);
    let proposed = repository.commit("proposed", &[default], Some("Gone"));
    let stack = LocalStack::for_test_with_content(
        default,
        [(id("Gone"), proposed, "Title".to_owned(), String::new())],
    )
    .unwrap();
    let remote_heads = heads(default, &[]);
    let mut node = open_node(7, "Gone", "main", default, proposed, false);
    node["body"] = json!("<!-- gherrit-meta: {\"id\":\"Gone\",\"parent\":null,\"child\":null} -->");
    let (correlated, active) =
        correlate_and_activate(remote_heads, &stack, open_response(default, vec![node]), "");
    let context = BodyLinkContext::from_destination(active.destination(), None).unwrap();
    let error = match plan_publication(
        context,
        stack,
        correlated,
        empty_terminal_histories(&active, &[]),
        active,
        &repository.graph([proposed]),
    ) {
        Ok(_) => panic!("OPEN without published history must be rejected"),
        Err(error) => error,
    };

    assert!(error.to_string().contains("OPEN pull request but no published history"));
}

#[test]
fn landing_automation_is_rejected_for_an_owned_base() {
    let repository = TestRepository::new();
    let default = repository.commit("root", &[], None);
    let parent = repository.commit("parent", &[default], Some("Gone"));
    let child = repository.commit("child", &[parent], Some("Gtwo"));
    let stack = LocalStack::for_test_with_content(
        default,
        [
            (id("Gone"), parent, "Title".to_owned(), String::new()),
            (id("Gtwo"), child, "Title".to_owned(), String::new()),
        ],
    )
    .unwrap();
    let remote_heads = heads(default, &[("Gone", parent, default), ("Gtwo", child, parent)]);
    let history = versions(&[("Gone", 1, parent), ("Gtwo", 1, child)]);
    let response = open_response(
        default,
        vec![
            open_node(7, "Gone", "main", default, parent, false),
            open_node(8, "Gtwo", "gherrit-bases/Gtwo", parent, child, true),
        ],
    );
    let (correlated, active) = correlate_and_activate(remote_heads, &stack, response, &history);
    let context = BodyLinkContext::from_destination(active.destination(), None).unwrap();
    let error = match plan_publication(
        context,
        stack,
        correlated,
        empty_terminal_histories(&active, &[]),
        active,
        &repository.graph([child]),
    ) {
        Ok(_) => panic!("landing automation on an owned base must be rejected"),
        Err(error) => error,
    };

    assert!(error.to_string().contains("landing automation with an owned base"));
}

#[test]
fn observed_desired_base_and_landing_automation_truth_table_is_exact() {
    for desired_owned in [false, true] {
        for observed_owned in [false, true] {
            for landing in 0..3 {
                let repository = TestRepository::new();
                let default = repository.commit("root", &[], None);
                let parent = repository.commit("parent", &[default], Some("Gone"));
                let child = repository.commit("child", &[parent], Some("Gtwo"));
                let (stack, managed, history, mut nodes, target_id, graph_roots) = if desired_owned
                {
                    (
                        LocalStack::for_test_with_content(
                            default,
                            [
                                (id("Gone"), parent, "Title".to_owned(), String::new()),
                                (id("Gtwo"), child, "Title".to_owned(), String::new()),
                            ],
                        )
                        .unwrap(),
                        vec![("Gone", parent, default), ("Gtwo", child, parent)],
                        versions(&[("Gone", 1, parent), ("Gtwo", 1, child)]),
                        vec![open_node(7, "Gone", "main", default, parent, false)],
                        "Gtwo",
                        vec![child],
                    )
                } else {
                    (
                        LocalStack::for_test_with_content(
                            default,
                            [(id("Gone"), parent, "Title".to_owned(), String::new())],
                        )
                        .unwrap(),
                        vec![("Gone", parent, default)],
                        versions(&[("Gone", 1, parent)]),
                        Vec::new(),
                        "Gone",
                        vec![parent],
                    )
                };
                let target_head = if desired_owned { child } else { parent };
                let owned_base = if desired_owned { parent } else { default };
                let mut target = open_node(
                    8,
                    target_id,
                    if observed_owned {
                        if desired_owned { "gherrit-bases/Gtwo" } else { "gherrit-bases/Gone" }
                    } else {
                        "main"
                    },
                    if observed_owned { owned_base } else { default },
                    target_head,
                    landing == 1,
                );
                target["isInMergeQueue"] = json!(landing == 2);
                nodes.push(target);
                let remote_heads = heads(default, &managed);
                let response = open_response(default, nodes);
                let (correlated, active) =
                    correlate_and_activate(remote_heads, &stack, response, &history);
                let context =
                    BodyLinkContext::from_destination(active.destination(), None).unwrap();
                let result = plan_publication(
                    context,
                    stack,
                    correlated,
                    empty_terminal_histories(&active, &[]),
                    active,
                    &repository.graph(graph_roots),
                );
                let must_reject = landing != 0 && (observed_owned || desired_owned);
                assert_eq!(
                    result.is_err(),
                    must_reject,
                    "desired_owned={desired_owned}, observed_owned={observed_owned}, landing={landing}"
                );
            }
        }
    }
}

#[test]
fn nonlocal_rows_are_validation_only_and_fail_closed() {
    for mode in 0..4 {
        let repository = TestRepository::new();
        let default = repository.commit("root", &[], None);
        let local = repository.commit("local", &[default], Some("Gone"));
        let nonlocal = repository.commit("nonlocal", &[default], Some("Gextra"));
        let stack = LocalStack::for_test_with_content(
            default,
            [(id("Gone"), local, "Title".to_owned(), String::new())],
        )
        .unwrap();
        let absent_history = mode == 3;
        let observed_owned = mode == 1 || mode == 2;
        let landing = mode == 2;
        let mut managed = vec![("Gone", local, default)];
        if !absent_history {
            managed.push(("Gextra", nonlocal, default));
        }
        let history = if absent_history {
            versions(&[("Gone", 1, local)])
        } else {
            versions(&[("Gone", 1, local), ("Gextra", 1, nonlocal)])
        };
        let mut nonlocal_node = open_node(
            8,
            "Gextra",
            if observed_owned { "gherrit-bases/Gextra" } else { "main" },
            default,
            nonlocal,
            landing,
        );
        if absent_history {
            nonlocal_node["body"] =
                json!("<!-- gherrit-meta: {\"id\":\"Gextra\",\"parent\":null,\"child\":null} -->");
        }
        let response = open_response(
            default,
            vec![open_node(7, "Gone", "main", default, local, false), nonlocal_node],
        );
        let remote_heads = heads(default, &managed);
        let (correlated, active) = correlate_and_activate_with_nonlocal(
            remote_heads,
            &stack,
            &[id("Gextra")],
            response,
            &history,
        );
        let context = BodyLinkContext::from_destination(active.destination(), None).unwrap();
        let result = plan_publication(
            context,
            stack,
            correlated,
            empty_terminal_histories(&active, &[]),
            active,
            &repository.graph([local, nonlocal]),
        );

        assert_eq!(result.is_err(), landing || absent_history, "mode={mode}");
        if let Ok(plan) = result {
            assert!(requests_for_test(&plan).is_empty(), "nonlocal state cannot emit Git work");
        }
    }
}

#[test]
fn empty_local_stack_is_outside_the_planner_contract() {
    let repository = TestRepository::new();
    let default = repository.commit("root", &[], None);
    let stack = LocalStack::for_test(default, std::iter::empty());
    let remote_heads = heads(default, &[]);
    let (correlated, active) =
        correlate_and_activate(remote_heads, &stack, open_response(default, Vec::new()), "");
    let context = BodyLinkContext::from_destination(active.destination(), None).unwrap();
    let error = match plan_publication(
        context,
        stack,
        correlated,
        empty_terminal_histories(&active, &[]),
        active,
        &repository.graph([default]),
    ) {
        Ok(_) => panic!("empty planning should be unreachable"),
        Err(error) => error,
    };

    assert!(error.to_string().contains("nonempty local stack"));
}

#[test]
fn body_equality_normalizes_only_crlf_pairs() {
    let cases = [
        ("empty", "", "", true),
        ("exact text", "body", "body", true),
        ("exact LF", "a\nb", "a\nb", true),
        ("observed CRLF", "a\r\nb", "a\nb", true),
        ("desired CRLF", "a\nb", "a\r\nb", true),
        ("both CRLF", "a\r\nb", "a\r\nb", true),
        ("mixed spellings", "a\r\nb\nc\r\n", "a\nb\r\nc\n", true),
        ("terminal line-ending spelling", "body\r\n", "body\n", true),
        ("identical outer spaces", " body ", " body ", true),
        ("leading ASCII space added", " body", "body", false),
        ("trailing ASCII space added", "body ", "body", false),
        ("leading tab added", "\tbody", "body", false),
        ("trailing tab added", "body\t", "body", false),
        ("leading blank line added", "\nbody", "body", false),
        ("terminal LF added", "body\n", "body", false),
        ("second terminal blank line added", "body\n\n", "body\n", false),
        ("nonbreaking space added", "\u{00a0}body", "body", false),
        ("em space added", "body\u{2003}", "body", false),
        ("lone CR changed to LF", "a\rb", "a\nb", false),
        ("CR before a CRLF pair", "a\r\r\nb", "a\nb", false),
        ("internal blank line removed", "a\n\nb", "a\nb", false),
        ("content changed", "a\nx", "a\ny", false),
    ];

    for (case, observed, desired, expected) in cases {
        assert_eq!(bodies_equal(observed, desired), expected, "{case}");
    }
}

#[test]
fn existing_projection_emits_each_exact_title_body_base_difference_mask() {
    for mask in 0_u8..8 {
        let projection = ExistingProjection {
            id: id("Gone"),
            identity: PullRequestIdentity::new(7, "PR_7".to_owned()).unwrap(),
            observed_body: if mask & 2 == 0 { "new body" } else { "old body" }.into(),
            title_update: (mask & 1 != 0).then(|| "new title".to_owned()),
            base_update: (mask & 4 != 0).then(|| "gherrit-bases/Gone".to_owned()),
        };
        let update = projection.into_update(GeneratedBody::for_test("new body")).unwrap();
        if mask == 0 {
            assert!(update.is_none());
            continue;
        }
        let request = PreparedUpdates::new(vec![update.unwrap()]).unwrap().request_text();
        assert_eq!(request.contains("title:"), mask & 1 != 0, "mask={mask:03b}");
        assert_eq!(request.contains("body:"), mask & 2 != 0, "mask={mask:03b}");
        assert_eq!(request.contains("baseRefName:"), mask & 4 != 0, "mask={mask:03b}");
    }
}

#[test]
fn an_open_pr_head_cannot_be_just_the_unpublished_proposal() {
    let repository = TestRepository::new();
    let default = repository.commit("root", &[], None);
    let published = repository.commit("published", &[default], Some("Gone"));
    let proposed = repository.commit("proposed", &[default], Some("Gone"));
    let stack = LocalStack::for_test_with_content(
        default,
        [(id("Gone"), proposed, "Title".to_owned(), String::new())],
    )
    .unwrap();
    let remote_heads = heads(default, &[("Gone", published, default)]);
    let history = versions(&[("Gone", 1, published)]);
    let response =
        open_response(default, vec![open_node(7, "Gone", "main", default, proposed, false)]);
    let (correlated, active) = correlate_and_activate(remote_heads, &stack, response, &history);
    let context = BodyLinkContext::from_destination(active.destination(), None).unwrap();
    let error = match plan_publication(
        context,
        stack,
        correlated,
        empty_terminal_histories(&active, &[]),
        active,
        &repository.graph([proposed, published]),
    ) {
        Ok(_) => panic!("proposal-only GitHub head must be rejected"),
        Err(error) => error,
    };

    assert!(error.to_string().contains("head not present in published history"));
}

#[test]
fn initial_pr_oid_evidence_accepts_only_published_or_exact_default_values() {
    let repository = TestRepository::new();
    let default = repository.commit("root", &[], None);
    let parent_published = repository.commit("parent published", &[default], Some("Gone"));
    let parent_proposed = repository.commit("parent proposed", &[default], Some("Gone"));
    let child_published = repository.commit("child published", &[parent_published], Some("Gtwo"));
    let child_proposed = repository.commit("child proposed", &[parent_proposed], Some("Gtwo"));
    let unrelated = repository.commit("unrelated", &[default], Some("Gother"));
    let graph = repository.graph([child_proposed, child_published, unrelated]);

    for (label, head, base_name, base_oid, should_succeed) in [
        ("published-owned", child_published, "gherrit-bases/Gtwo", parent_published, true),
        ("exact-default", child_published, "main", default, true),
        ("wrong-default-tip", child_published, "main", parent_published, false),
        ("proposal-head", child_proposed, "gherrit-bases/Gtwo", parent_published, false),
        ("unrelated-head", unrelated, "gherrit-bases/Gtwo", parent_published, false),
        ("wrong-change-head", parent_published, "gherrit-bases/Gtwo", parent_published, false),
        ("proposal-base", child_published, "gherrit-bases/Gtwo", parent_proposed, false),
        ("unrelated-base", child_published, "gherrit-bases/Gtwo", unrelated, false),
        ("wrong-change-base", child_published, "gherrit-bases/Gtwo", child_published, false),
    ] {
        let stack = LocalStack::for_test_with_content(
            default,
            [
                (id("Gone"), parent_proposed, "Title".to_owned(), String::new()),
                (id("Gtwo"), child_proposed, "Title".to_owned(), String::new()),
            ],
        )
        .unwrap();
        let remote_heads = heads(
            default,
            &[("Gone", parent_published, default), ("Gtwo", child_published, parent_published)],
        );
        let history = versions(&[("Gone", 1, parent_published), ("Gtwo", 1, child_published)]);
        let response = open_response(
            default,
            vec![
                open_node(7, "Gone", "main", default, parent_published, false),
                open_node(8, "Gtwo", base_name, base_oid, head, false),
            ],
        );
        let (correlated, active) = correlate_and_activate(remote_heads, &stack, response, &history);
        let context = BodyLinkContext::from_destination(active.destination(), None).unwrap();
        let result = plan_publication(
            context,
            stack,
            correlated,
            empty_terminal_histories(&active, &[]),
            active,
            &graph,
        );

        assert_eq!(result.is_ok(), should_succeed, "case={label}");
    }
}

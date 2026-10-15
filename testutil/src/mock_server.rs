use std::{
    collections::{HashMap, HashSet, VecDeque},
    future::IntoFuture,
    path::PathBuf,
    sync::{mpsc::Sender, Arc, LazyLock, RwLock},
};

use apollo_compiler::{ast, executable, validation::Valid, ExecutableDocument, Name, Node};
use axum::{
    body::{Body, Bytes},
    extract::State,
    http::{
        header::{CONTENT_TYPE, LOCATION},
        HeaderMap, HeaderValue, StatusCode,
    },
    response::{IntoResponse, Response},
    routing::{any, post},
    Json, Router,
};
use tokio::net::TcpListener;

use crate::{
    git_interceptor, FailureKind, GraphQlOperation, MalformedJson, MockSchedules, PullRequestState,
    RedirectStatus, RetryableHttpStatus, TestEnvironment,
};

mod document;
use document::*;

mod http;
use http::*;

mod failures;
use failures::*;

mod query;
use query::*;

mod mutation;
use mutation::*;

const LOCAL_PULL_REQUEST_PAGE_SIZE: usize = 1;

static GITHUB_SCHEMA: LazyLock<Valid<apollo_compiler::Schema>> = LazyLock::new(|| {
    apollo_compiler::Schema::parse_and_validate(
        include_str!("../data/github_schema.graphql"),
        "github_schema.graphql",
    )
    .expect("Failed to parse and validate embedded GitHub schema")
});

#[derive(Debug, Clone, Default)]
pub struct MockState {
    pub prs: Vec<PrEntry>,
    pub(super) cross_repository_prs: HashSet<usize>,
    pub(super) git: git_interceptor::State,
    pub(super) graphql_requests: Vec<GraphQlRequestRecord>,
    pub(super) graphql_arrivals: usize,
    pub(super) protocol_violations: Vec<String>,
    pub graphql_redirect_trap_requests: usize,
    pub max_graphql_query_operations_per_request: Option<usize>,
    pub repo_owner: String,
    pub repo_name: String,
    pub github_default_branch: Option<(String, String)>,
    pub faults: VecDeque<FailureKind>,
}

#[derive(Debug, Clone)]
pub(super) struct GraphQlRequestRecord {
    pub(super) operations: Vec<GraphQlOperation>,
    pub(super) document: String,
}

impl MockState {
    pub fn new(owner: String, name: String) -> Self {
        Self { repo_owner: owner, repo_name: name, ..Default::default() }
    }

    pub fn add_pr(&mut self, pr: PrEntry) {
        self.prs.push(pr);
    }
}

#[derive(Debug, Clone)]
pub struct PrEntry {
    pub number: usize,
    pub node_id: String,
    pub state: PullRequestState,
    pub title: String,
    pub body: String,
    pub head: BranchState,
    pub base: BranchState,
    pub is_draft: bool,
    pub auto_merge: bool,
    pub in_merge_queue: bool,
}

pub struct MockPrArgs {
    pub number: usize,
    pub title: String,
    pub body: String,
    pub head: String,
    pub base: String,
}

impl PrEntry {
    pub fn mock(args: MockPrArgs) -> Self {
        let MockPrArgs { number, title, body, head, base } = args;
        Self {
            number,
            node_id: format!("PR_{number}"),
            state: PullRequestState::Open,
            title,
            body,
            head: BranchState { name: head, oid: String::new() },
            base: BranchState { name: base, oid: String::new() },
            is_draft: false,
            auto_merge: false,
            in_merge_queue: false,
        }
    }
}

#[derive(Debug, Clone)]
pub struct BranchState {
    pub name: String,
    pub oid: String,
}

#[derive(Clone)]
struct AppState {
    state: Arc<RwLock<MockState>>,
    remote_path: PathBuf,
    system_git: PathBuf,
    test_environment: TestEnvironment,
    schedules: MockSchedules,
}

/// Runs a mock GitHub API server until `shutdown_rx` is signaled.
pub(super) async fn run_mock_server(
    state: Arc<RwLock<MockState>>,
    remote_path: PathBuf,
    system_git: PathBuf,
    test_environment: TestEnvironment,
    schedules: MockSchedules,
    ready_tx: Sender<String>,
    shutdown_rx: tokio::sync::oneshot::Receiver<()>,
) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let url = format!("http://{}", addr);

    let git_routes = git_interceptor::routes(
        state.clone(),
        remote_path.clone(),
        system_git.clone(),
        test_environment.clone(),
        schedules.clone(),
    );
    let app_state = AppState { state, remote_path, system_git, test_environment, schedules };

    let app = Router::new()
        .route("/graphql", post(graphql).fallback(graphql_wrong_method))
        .route("/graphql-redirect-trap", any(graphql_redirect_trap))
        .with_state(app_state)
        .merge(git_routes);

    ready_tx.send(url).expect("Failed to send mock server URL");

    let server = axum::serve(listener, app).into_future();
    tokio::pin!(server);
    tokio::select! {
        result = &mut server => result.unwrap(),
        _ = shutdown_rx => {}
    }
}

fn check_and_apply_graphql_failure(
    mock_state: &mut MockState,
    operations: &[GraphQlOperation],
) -> Option<FailureKind> {
    use FailureKind::*;

    let fail_action = mock_state.faults.front()?;
    let matches = match fail_action {
        GraphQl => true,
        QueryTransport | QueryHttp(_) => {
            !operations.is_empty()
                && operations.iter().all(|operation| *operation == GraphQlOperation::Query)
        }
        CreatePr
        | CreatePrMismatchedContent
        | CreatePrMalformedJson(_)
        | CreatePrHttp(_)
        | CreatePrRedirect(_) => operations.contains(&GraphQlOperation::CreatePr),
        CreatePrApplyThenDisconnect => {
            !operations.is_empty()
                && operations.iter().all(|operation| *operation == GraphQlOperation::CreatePr)
        }
        ClosePr | ClosePrApplyThenDisconnect => operations.contains(&GraphQlOperation::ClosePr),
        UpdatePr
        | UpdatePrApplyThenDisconnect
        | UpdatePrMalformedJson(_)
        | UpdatePrConcurrentClose => operations.contains(&GraphQlOperation::UpdatePr),
        LosePublicationPushReceipt(_) | Git(_) => false,
    };

    if !matches {
        return None;
    }

    mock_state.faults.pop_front()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn existing_branch(_branch: &str) -> Result<Option<String>, String> {
        Ok(Some("1".repeat(40)))
    }

    fn parse_document(query: &str) -> ExecutableDocument {
        ExecutableDocument::parse_and_validate(&GITHUB_SCHEMA, query, "test.graphql")
            .unwrap()
            .into_inner()
    }

    fn root_field(document: &ExecutableDocument) -> &executable::Field {
        let operation = document.operations.iter().next().unwrap();
        let executable::Selection::Field(field) = &operation.selection_set.selections[0] else {
            panic!("expected a root field");
        };
        field
    }

    fn exact_local_document(after: Option<&str>, include_facts: bool) -> ExecutableDocument {
        let facts =
            if include_facts { "id, defaultBranchRef { name, target { oid } }, " } else { "" };
        local_document(after, facts)
    }

    fn local_document(after: Option<&str>, facts: &str) -> ExecutableDocument {
        let after = after.map(|cursor| format!(", after: {cursor:?}")).unwrap_or_default();
        parse_document(&format!(
            "query {{ repository(owner: \"owner\", name: \"repo\") {{ {facts}\
             op0: pullRequests(headRefName: \"Ghead\", first: 1{after}, \
             states: [OPEN]) {{ nodes {{ number, id, title, body, \
             baseRefName, baseRefOid, headRefName, headRefOid, state, isCrossRepository, \
             autoMergeRequest {{ enabledAt }}, isInMergeQueue, isDraft }} \
             pageInfo {{ hasNextPage, endCursor }} }} }} }}"
        ))
    }

    fn apply_failure(
        state: &mut MockState,
        operations: &[GraphQlOperation],
    ) -> Option<FailureKind> {
        check_and_apply_graphql_failure(state, operations)
    }

    #[test]
    fn operation_failure_only_matches_the_requested_operation() {
        let mut state = MockState {
            faults: VecDeque::from([FailureKind::UpdatePr, FailureKind::CreatePr]),
            ..Default::default()
        };

        assert_eq!(apply_failure(&mut state, &[GraphQlOperation::Query]), None);
        assert_eq!(state.faults, VecDeque::from([FailureKind::UpdatePr, FailureKind::CreatePr]));

        assert_eq!(apply_failure(&mut state, &[GraphQlOperation::CreatePr]), None);

        assert_eq!(
            apply_failure(&mut state, &[GraphQlOperation::UpdatePr]),
            Some(FailureKind::UpdatePr)
        );
        assert_eq!(state.faults, VecDeque::from([FailureKind::CreatePr]));
    }

    #[test]
    fn close_faults_route_only_to_close_operations() {
        let mut state = MockState {
            faults: VecDeque::from([FailureKind::ClosePrApplyThenDisconnect, FailureKind::ClosePr]),
            ..Default::default()
        };

        for operation in
            [GraphQlOperation::Query, GraphQlOperation::CreatePr, GraphQlOperation::UpdatePr]
        {
            assert_eq!(apply_failure(&mut state, &[operation]), None);
        }
        assert_eq!(
            apply_failure(&mut state, &[GraphQlOperation::ClosePr]),
            Some(FailureKind::ClosePrApplyThenDisconnect)
        );
        assert_eq!(
            apply_failure(&mut state, &[GraphQlOperation::ClosePr]),
            Some(FailureKind::ClosePr)
        );
        assert!(state.faults.is_empty());
    }

    #[test]
    fn generic_graphql_failure_matches_any_operation() {
        let mut state =
            MockState { faults: VecDeque::from([FailureKind::GraphQl]), ..Default::default() };

        assert_eq!(
            apply_failure(&mut state, &[GraphQlOperation::Query]),
            Some(FailureKind::GraphQl)
        );
        assert!(state.faults.is_empty());
    }

    #[test]
    fn accepts_the_production_request_shapes() {
        let variables = Some(serde_json::Map::from_iter([
            ("owner".to_string(), serde_json::json!("owner")),
            ("name".to_string(), serde_json::json!("repo")),
        ]));
        let query_with_variables = parse_document(
            "query ProductionPullRequests($owner: String!, $name: String!) { \
             repository(owner: $owner, name: $name) { id, \
             defaultBranchRef { name, target { oid } }, \
             op0: pullRequests(headRefName: \"Ghead\", first: 1, \
             states: [OPEN]) { \
             nodes { number, id, title, body, baseRefName, baseRefOid, headRefName, \
             headRefOid, state, isCrossRepository, autoMergeRequest { enabledAt }, \
             isInMergeQueue, isDraft } pageInfo { hasNextPage, endCursor } } } }",
        );
        validate_supported_document(&query_with_variables, &variables).unwrap();
        validate_supported_document(&exact_local_document(None, false), &None).unwrap();

        let create = parse_document(
            "mutation { op0: createPullRequest(input: { repositoryId: \
             \"REPO_NODE_ID\", headRepositoryId: \"REPO_NODE_ID\", \
             baseRefName: \"main\", headRefName: \"Ghead\", \
             title: \"Title\", body: \"Body\", draft: true, clientMutationId: \
             \"gherrit:create:Ghead\" }) { clientMutationId, pullRequest { \
             number, id, state, title, body, isDraft, headRefName, headRefOid, headRepository { id }, \
             baseRefName, baseRefOid, baseRepository { id } } } }",
        );
        validate_supported_document(&create, &None).unwrap();

        let update = parse_document(
            "mutation { op0: updatePullRequest(input: { pullRequestId: \"PR_1\", \
             title: \"Updated\", clientMutationId: \"gherrit:update:PR_1\" }) { \
             clientMutationId, pullRequest { number, id, state } } }",
        );
        validate_supported_document(&update, &None).unwrap();

        let close = parse_document(
            "mutation { op0: closePullRequest(input: { pullRequestId: \"PR_1\", \
             clientMutationId: \"gherrit:close:PR_1\" }) { clientMutationId, \
             pullRequest { number, id, state } } }",
        );
        validate_supported_document(&close, &None).unwrap();

        let mixed = parse_document(
            "mutation { close: closePullRequest(input: { pullRequestId: \"PR_1\", \
             clientMutationId: \"gherrit:close:PR_1\" }) { clientMutationId, \
             pullRequest { number, id, state } } update: updatePullRequest(input: \
             { pullRequestId: \"PR_2\", title: \"Updated\", \
             clientMutationId: \"gherrit:update:PR_2\" }) { clientMutationId, \
             pullRequest { number, id, state } } }",
        );
        validate_supported_document(&mixed, &None).unwrap();
        assert_eq!(
            graphql_operations(&mixed),
            [GraphQlOperation::ClosePr, GraphQlOperation::UpdatePr]
        );
    }

    #[test]
    fn rejects_create_requests_without_an_exact_draft_true_flag() {
        let create = |draft: &str| {
            parse_document(&format!(
                "mutation {{ createPullRequest(input: {{ repositoryId: \
                 \"REPO_NODE_ID\", headRepositoryId: \"REPO_NODE_ID\", \
                 baseRefName: \"main\", headRefName: \"Ghead\", title: \"Title\", \
                 body: \"Body\", {draft} clientMutationId: \"create\" }}) {{ \
                 clientMutationId, pullRequest {{ number, id, state, isDraft, \
                 headRefName, headRefOid, headRepository {{ id }}, baseRefName, \
                 baseRefOid, baseRepository {{ id }} }} }} }}"
            ))
        };

        for draft in ["", "draft: false,"] {
            let error = validate_supported_document(&create(draft), &None).unwrap_err();
            assert!(error.contains("draft: true"), "unexpected error: {error}");
        }

        let error = validate_supported_document(&create("draft: null,"), &None).unwrap_err();
        assert!(error.contains("inline boolean"), "unexpected error: {error}");
    }

    #[test]
    fn rejects_valid_but_unsupported_graphql() {
        let viewer = parse_document("query { viewer { login } }");
        assert!(validate_supported_document(&viewer, &None)
            .unwrap_err()
            .contains("root field `viewer`"));

        let repository_name =
            parse_document("query { repository(owner: \"owner\", name: \"repo\") { name } }");
        assert!(validate_supported_document(&repository_name, &None)
            .unwrap_err()
            .contains("production pull request query"));

        let repository_id =
            parse_document("query { repository(owner: \"owner\", name: \"repo\") { id } }");
        assert!(validate_supported_document(&repository_id, &None)
            .unwrap_err()
            .contains("production pull request query"));

        let fragment = parse_document(
            "query { repository(owner: \"owner\", name: \"repo\") { ...Fields } } \
             fragment Fields on Repository { id }",
        );
        assert!(validate_supported_document(&fragment, &None).unwrap_err().contains("fragments"));

        let multiple_operations = parse_document(
            "query One { repository(owner: \"owner\", name: \"repo\") { id } } \
             query Two { repository(owner: \"owner\", name: \"repo\") { id } }",
        );
        assert!(validate_supported_document(&multiple_operations, &None)
            .unwrap_err()
            .contains("exactly one"));

        let multiple_repositories = parse_document(
            "query { first: repository(owner: \"owner\", name: \"repo\") { id } \
             second: repository(owner: \"owner\", name: \"repo\") { id } }",
        );
        assert!(validate_supported_document(&multiple_repositories, &None)
            .unwrap_err()
            .contains("exactly one repository root field"));

        let create_and_projection = parse_document(
            "mutation { create: createPullRequest(input: { repositoryId: \
             \"REPO_NODE_ID\", baseRefName: \"main\", headRefName: \"Ghead\", \
             title: \"Title\", clientMutationId: \"create\" }) { clientMutationId } \
             update: updatePullRequest(input: { pullRequestId: \"PR_1\", \
             title: \"Updated\", clientMutationId: \"update\" }) { \
             clientMutationId } }",
        );
        assert!(validate_supported_document(&create_and_projection, &None)
            .unwrap_err()
            .contains("does not mix pull request creation with final projection"));

        let duplicate_mutation = parse_document(
            "mutation { createPullRequest(input: { repositoryId: \"REPO_NODE_ID\", \
             baseRefName: \"main\", headRefName: \"Ghead\", title: \"Title\", \
             clientMutationId: \"first\" }) { \
             pullRequest { number } } createPullRequest(input: { repositoryId: \
             \"REPO_NODE_ID\", baseRefName: \"main\", headRefName: \"Ghead\", \
             title: \"Title\", clientMutationId: \"first\" }) { \
             pullRequest { number } } }",
        );
        assert!(validate_supported_document(&duplicate_mutation, &None)
            .unwrap_err()
            .contains("duplicate response key `createPullRequest`"));

        for states in
            ["[CLOSED, MERGED]", "[OPEN, OPEN]", "[MERGED, CLOSED, OPEN]", "[CLOSED]", "[MERGED]"]
        {
            let unsupported_states = parse_document(&format!(
                "query {{ repository(owner: \"owner\", name: \"repo\") {{ \
                 op0: pullRequests(headRefName: \"Ghead\", first: 1, \
                 states: {states}) {{ nodes {{ number, id, headRefName, state, \
                 isCrossRepository }} pageInfo {{ hasNextPage, endCursor }} }} }} }}"
            ));
            assert!(validate_supported_document(&unsupported_states, &None)
                .unwrap_err()
                .contains("states: [OPEN]"));
        }

        let incomplete_node_fields = parse_document(
            "query { repository(owner: \"owner\", name: \"repo\") { \
             op0: pullRequests(headRefName: \"Ghead\", first: 1, \
             states: [OPEN]) { \
             nodes { number, id, headRefName, state, isCrossRepository } \
             pageInfo { hasNextPage, endCursor } } } }",
        );
        assert!(validate_supported_document(&incomplete_node_fields, &None)
            .unwrap_err()
            .contains("repository.pullRequests.nodes"));

        for facts in [
            "repository_id: id, branch: defaultBranchRef { name, target { oid } }, ",
            "id, duplicate_id: id, defaultBranchRef { name, target { oid } }, ",
        ] {
            let inexact_facts = local_document(None, facts);
            assert!(validate_supported_document(&inexact_facts, &None)
                .unwrap_err()
                .contains("exactly one unaliased repository fact pair"));
        }

        let update_without_state = parse_document(
            "mutation { updatePullRequest(input: { pullRequestId: \"PR_1\", \
             title: \"Updated\", clientMutationId: \"update\" }) { \
             clientMutationId, pullRequest { number, id } } }",
        );
        assert!(validate_supported_document(&update_without_state, &None)
            .unwrap_err()
            .contains("number, id, state"));

        let close_without_client_id = parse_document(
            "mutation { closePullRequest(input: { pullRequestId: \"PR_1\" }) { \
             clientMutationId, pullRequest { number, id, state } } }",
        );
        assert!(validate_supported_document(&close_without_client_id, &None)
            .unwrap_err()
            .contains("clientMutationId"));

        let close_without_state = parse_document(
            "mutation { closePullRequest(input: { pullRequestId: \"PR_1\", \
             clientMutationId: \"close\" }) { clientMutationId, \
             pullRequest { number, id } } }",
        );
        assert!(validate_supported_document(&close_without_state, &None)
            .unwrap_err()
            .contains("number, id, state"));
    }

    #[test]
    fn rejects_non_object_variables_payload() {
        let payload = serde_json::json!({ "variables": null });
        let error = graphql_variables(&payload).unwrap_err();
        assert_eq!(error, "Invalid GraphQL payload: `variables` must be an object");
    }

    #[test]
    fn rejects_missing_runtime_variables() {
        let document = parse_document(
            "query ProductionPullRequests($owner: String!, $name: String!) { \
             repository(owner: $owner, name: $name) { \
             op0: pullRequests(headRefName: \"Ghead\", first: 1, \
             states: [OPEN]) { \
             nodes { number, id, title, body, baseRefName, baseRefOid, headRefName, \
             headRefOid, state, isCrossRepository, autoMergeRequest { enabledAt }, \
             isInMergeQueue, isDraft } pageInfo { hasNextPage, endCursor } } } }",
        );
        let error = validate_supported_document(&document, &None).unwrap_err();
        assert!(error.contains("variable `$owner`"), "unexpected error: {error}");
    }

    #[test]
    fn repository_response_projects_the_exact_open_contract() {
        let document = parse_document(
            "query { repository(owner: \"owner\", name: \"repo\") { \
             id, defaultBranchRef { name, target { oid } } \
             op0: pullRequests(headRefName: \"Ghead\", first: 1, \
             states: [OPEN]) { nodes { number, id, title, body, \
             baseRefName, baseRefOid, headRefName, headRefOid, state, \
             isCrossRepository, autoMergeRequest { enabledAt }, isInMergeQueue, isDraft } \
             pageInfo { hasNextPage, endCursor } } } }",
        );
        validate_supported_document(&document, &None).unwrap();

        let mut state = MockState::new("owner".to_string(), "repo".to_string());
        let mut pull_request = PrEntry::mock(MockPrArgs {
            number: 1,
            title: "Title".to_string(),
            body: "Body".to_string(),
            head: "Ghead".to_string(),
            base: "main".to_string(),
        });
        pull_request.head.oid = "2".repeat(40);
        pull_request.base.oid = "3".repeat(40);
        pull_request.auto_merge = true;
        pull_request.in_merge_queue = true;
        state.add_pr(pull_request);
        state.cross_repository_prs.insert(1);
        let response = handle_repository_query(
            &state,
            root_field(&document),
            &None,
            &|| Ok(("main".to_owned(), "1".repeat(40))),
            &existing_branch,
        )
        .unwrap();
        assert_eq!(
            response,
            serde_json::json!({
                "id": "REPO_NODE_ID",
                "defaultBranchRef": {
                    "name": "main",
                    "target": { "oid": "1111111111111111111111111111111111111111" }
                },
                "op0": {
                    "nodes": [{
                        "number": 1,
                        "id": "PR_1",
                        "title": "Title",
                        "body": "Body",
                        "baseRefName": "main",
                        "baseRefOid": "3333333333333333333333333333333333333333",
                        "headRefName": "Ghead",
                        "headRefOid": "2222222222222222222222222222222222222222",
                        "state": "OPEN",
                        "isCrossRepository": true,
                        "autoMergeRequest": { "enabledAt": "2026-01-01T00:00:00Z" },
                        "isInMergeQueue": true,
                        "isDraft": false
                    }],
                    "pageInfo": { "hasNextPage": false, "endCursor": null }
                }
            })
        );
    }

    #[test]
    fn mixed_projection_applies_close_before_update_in_request_order() {
        let document = parse_document(
            "mutation { close: closePullRequest(input: { pullRequestId: \"PR_1\", \
             clientMutationId: \"gherrit:close:PR_1\" }) { clientMutationId, \
             pullRequest { number, id, state } } update: updatePullRequest(input: \
             { pullRequestId: \"PR_2\", title: \"Updated\", \
             clientMutationId: \"gherrit:update:PR_2\" }) { clientMutationId, \
             pullRequest { number, id, state } } }",
        );
        validate_supported_document(&document, &None).unwrap();
        let fields =
            selected_fields(&document.operations.iter().next().unwrap().selection_set, "operation")
                .unwrap();
        let mut state = MockState::new("owner".to_string(), "repo".to_string());
        for number in [1, 2] {
            state.add_pr(PrEntry::mock(MockPrArgs {
                number,
                title: "Old".to_string(),
                body: String::new(),
                head: format!("Ghead{number}"),
                base: "main".to_string(),
            }));
        }

        handle_close_pr(&mut state, fields[0]).unwrap();
        handle_update_pr(&mut state, fields[1], &existing_branch, false).unwrap();

        assert_eq!(state.prs[0].state, PullRequestState::Closed);
        assert_eq!(state.prs[1].title, "Updated");
        assert_eq!(state.prs[1].state, PullRequestState::Open);
    }

    #[test]
    fn close_handler_changes_exact_open_identity_and_returns_closed_receipt() {
        let document = parse_document(
            "mutation { op0: closePullRequest(input: { pullRequestId: \"PR_7\", \
             clientMutationId: \"gherrit:close:PR_7\" }) { clientMutationId, \
             pullRequest { number, id, state } } }",
        );
        validate_supported_document(&document, &None).unwrap();
        let mut state = MockState::new("owner".to_string(), "repo".to_string());
        state.add_pr(PrEntry::mock(MockPrArgs {
            number: 7,
            title: "Title".to_string(),
            body: "Body".to_string(),
            head: "Ghead".to_string(),
            base: "main".to_string(),
        }));

        let response = handle_close_pr(&mut state, root_field(&document)).unwrap();
        assert_eq!(
            response,
            serde_json::json!({
                "clientMutationId": "gherrit:close:PR_7",
                "pullRequest": { "number": 7, "id": "PR_7", "state": "CLOSED" }
            })
        );
        assert_eq!(state.prs[0].state, PullRequestState::Closed);

        let error = handle_close_pr(&mut state, root_field(&document)).unwrap_err();
        assert!(error.contains("is not OPEN"), "unexpected error: {error}");
    }

    #[test]
    fn draft_handler_makes_the_open_pull_request_safe_and_returns_exact_identity() {
        let document = parse_document(
            "mutation { op0: convertPullRequestToDraft(input: { pullRequestId: \"PR_7\", \
             clientMutationId: \"gherrit:draft:PR_7\" }) { clientMutationId, \
             pullRequest { number, id, state, isDraft, headRefName, headRefOid, \
             baseRefName, baseRefOid } } }",
        );
        validate_supported_document(&document, &None).unwrap();
        let mut state = MockState::new("owner".to_string(), "repo".to_string());
        state.add_pr(PrEntry::mock(MockPrArgs {
            number: 7,
            title: "Title".to_string(),
            body: "Body".to_string(),
            head: "Ghead".to_string(),
            base: "main".to_string(),
        }));

        let response =
            handle_draft_pr(&mut state, root_field(&document), &existing_branch).unwrap();
        assert_eq!(
            response,
            serde_json::json!({
                "clientMutationId": "gherrit:draft:PR_7",
                "pullRequest": {
                    "number": 7,
                    "id": "PR_7",
                    "state": "OPEN",
                    "isDraft": true,
                    "headRefName": "Ghead",
                    "headRefOid": "1111111111111111111111111111111111111111",
                    "baseRefName": "main",
                    "baseRefOid": "1111111111111111111111111111111111111111"
                }
            })
        );
        assert!(state.prs[0].is_draft);
    }

    #[test]
    fn exact_local_connections_paginate_and_use_live_same_repository_oids() {
        let mut state = MockState::new("owner".to_string(), "repo".to_string());
        for id in [1, 2] {
            let mut pull_request = PrEntry::mock(MockPrArgs {
                number: id,
                title: format!("Title {id}"),
                body: format!("Body {id}"),
                head: "Ghead".to_string(),
                base: "main".to_string(),
            });
            pull_request.head.oid = "8".repeat(40);
            pull_request.base.oid = "9".repeat(40);
            state.add_pr(pull_request);
        }
        state.add_pr(PrEntry {
            state: PullRequestState::Closed,
            ..PrEntry::mock(MockPrArgs {
                number: 3,
                title: "Closed".to_string(),
                body: "Not projected".to_string(),
                head: "Ghead".to_string(),
                base: "main".to_string(),
            })
        });
        state.add_pr(PrEntry {
            state: PullRequestState::Merged,
            ..PrEntry::mock(MockPrArgs {
                number: 4,
                title: "Merged".to_string(),
                body: "Not projected".to_string(),
                head: "Ghead".to_string(),
                base: "main".to_string(),
            })
        });
        let branch_oid = |branch: &str| {
            Ok(Some(match branch {
                "Ghead" => "2".repeat(40),
                "main" => "3".repeat(40),
                _ => return Ok(None),
            }))
        };

        let first = exact_local_document(None, true);
        validate_supported_document(&first, &None).unwrap();
        let first = handle_repository_query(
            &state,
            root_field(&first),
            &None,
            &|| Ok(("main".to_owned(), "1".repeat(40))),
            &branch_oid,
        )
        .unwrap();
        assert_eq!(first["op0"]["nodes"][0]["number"], 1);
        assert_eq!(first["op0"]["nodes"][0]["headRefOid"], "2".repeat(40));
        assert_eq!(first["op0"]["nodes"][0]["baseRefOid"], "3".repeat(40));
        assert_eq!(first["op0"]["pageInfo"]["hasNextPage"], true);
        assert_eq!(first["op0"]["pageInfo"]["endCursor"], "cursor:Ghead:1");

        let second = exact_local_document(Some("cursor:Ghead:1"), false);
        validate_supported_document(&second, &None).unwrap();
        let second = handle_repository_query(
            &state,
            root_field(&second),
            &None,
            &|| panic!("later pages must not request repository facts"),
            &branch_oid,
        )
        .unwrap();
        assert_eq!(second["op0"]["nodes"][0]["number"], 2);
        assert_eq!(second["op0"]["pageInfo"]["hasNextPage"], false);
        assert!(second["op0"]["pageInfo"]["endCursor"].is_null());

        let wrong_head = exact_local_document(Some("cursor:Gother:1"), false);
        let error = handle_repository_query(
            &state,
            root_field(&wrong_head),
            &None,
            &|| panic!("later pages must not request repository facts"),
            &branch_oid,
        )
        .unwrap_err();
        assert!(error.contains("belongs to another local head"), "error={error}");

        let malformed_offset = exact_local_document(Some("cursor:Ghead:not-a-number"), false);
        let error = handle_repository_query(
            &state,
            root_field(&malformed_offset),
            &None,
            &|| panic!("later pages must not request repository facts"),
            &branch_oid,
        )
        .unwrap_err();
        assert!(error.contains("Invalid pull request cursor"), "error={error}");
    }

    #[test]
    fn repository_response_rejects_another_repository() {
        let state = MockState::new("owner".to_string(), "repo".to_string());

        for query in [
            "query { repository(owner: \"other\", name: \"repo\") { \
             op0: pullRequests(headRefName: \"Ghead\", first: 1, \
             states: [OPEN]) { nodes { number, id, title, body, \
             baseRefName, baseRefOid, headRefName, headRefOid, state, isCrossRepository, \
             autoMergeRequest { enabledAt }, isInMergeQueue, isDraft } \
             pageInfo { hasNextPage, endCursor } } } }",
            "query { repository(owner: \"owner\", name: \"other\") { \
             op0: pullRequests(headRefName: \"Ghead\", first: 1, \
             states: [OPEN]) { nodes { number, id, title, body, \
             baseRefName, baseRefOid, headRefName, headRefOid, state, isCrossRepository, \
             autoMergeRequest { enabledAt }, isInMergeQueue, isDraft } \
             pageInfo { hasNextPage, endCursor } } } }",
        ] {
            let document = parse_document(query);
            validate_supported_document(&document, &None).unwrap();

            assert_eq!(
                handle_repository_query(
                    &state,
                    root_field(&document),
                    &None,
                    &|| { Ok(("main".to_owned(), "1".repeat(40))) },
                    &existing_branch
                )
                .unwrap(),
                serde_json::Value::Null,
                "query: {query}"
            );
        }
    }

    #[test]
    fn create_allows_duplicate_open_requests_and_preserves_numbering() {
        let document = parse_document(
            "mutation { createPullRequest(input: { repositoryId: \"REPO_NODE_ID\", \
             headRepositoryId: \"REPO_NODE_ID\", baseRefName: \"main\", \
             headRefName: \"Gnew\", title: \"Title\", body: \"Body\", \
             draft: true, clientMutationId: \"create\" }) { \
             pullRequest { number } } }",
        );
        let mut state = MockState::new("owner".to_string(), "repo".to_string());
        state.add_pr(PrEntry::mock(MockPrArgs {
            number: 7,
            title: "Old".to_string(),
            body: String::new(),
            head: "Gnew".to_string(),
            base: "main".to_string(),
        }));
        let response =
            handle_create_pr(&mut state, root_field(&document), &existing_branch).unwrap();
        assert_eq!(response.pointer("/pullRequest/number"), Some(&serde_json::json!(8)));
        assert_eq!(state.prs.len(), 2);

        let response =
            handle_create_pr(&mut state, root_field(&document), &existing_branch).unwrap();
        assert_eq!(response.pointer("/pullRequest/number"), Some(&serde_json::json!(9)));
        assert_eq!(state.prs.len(), 3);
        assert!(state.prs.iter().all(|pr| pr.state == PullRequestState::Open));
        assert!(state.prs.iter().all(|pr| pr.head.name == "Gnew"));
        assert_eq!(
            state.prs.iter().map(|pr| pr.node_id.as_str()).collect::<Vec<_>>(),
            ["PR_7", "PR_8", "PR_9"]
        );

        let mut state = MockState::new("owner".to_string(), "repo".to_string());
        let error = handle_create_pr(&mut state, root_field(&document), &|branch| {
            Ok((branch == "main").then(|| "1".repeat(40)))
        })
        .unwrap_err();
        assert!(error.contains("Head branch `Gnew` does not exist"));
        assert!(state.prs.is_empty());
    }

    #[test]
    fn mutation_fields_are_not_modeled_as_an_atomic_transaction() {
        let document = parse_document(
            "mutation { first: createPullRequest(input: { repositoryId: \
             \"REPO_NODE_ID\", headRepositoryId: \"REPO_NODE_ID\", \
             baseRefName: \"main\", headRefName: \"Gnew\", \
             title: \"First\", body: \"First body\", draft: true, clientMutationId: \"first\" }) { \
             clientMutationId } second: createPullRequest(input: { \
             repositoryId: \"REPO_NODE_ID\", \
             headRepositoryId: \"REPO_NODE_ID\", baseRefName: \"main\", \
             headRefName: \"Gnew\", title: \"Second\", body: \"Second body\", draft: true, clientMutationId: \
             \"second\" }) { clientMutationId } }",
        );
        let operation = document.operations.iter().next().unwrap();
        let fields = selected_fields(&operation.selection_set, "operation").unwrap();
        let mut state = MockState::new("owner".to_string(), "repo".to_string());

        handle_create_pr(&mut state, fields[0], &existing_branch).unwrap();
        handle_create_pr(&mut state, fields[1], &existing_branch).unwrap();

        assert_eq!(state.prs.len(), 2, "each acknowledged field remains applied");
        assert_eq!(state.prs[0].title, "First");
        assert_eq!(state.prs[1].title, "Second");
    }

    #[test]
    fn mutations_reject_unknown_repository_and_pull_request_ids() {
        let create = parse_document(
            "mutation { createPullRequest(input: { repositoryId: \"WRONG\", \
             headRepositoryId: \"WRONG\", baseRefName: \"main\", \
             headRefName: \"Ghead\", title: \"Title\", body: \"Body\", \
             draft: true, clientMutationId: \"create\" }) { \
             pullRequest { number } } }",
        );
        let mut state = MockState::new("owner".to_string(), "repo".to_string());
        let error =
            handle_create_pr(&mut state, root_field(&create), &existing_branch).unwrap_err();
        assert!(error.contains("Repository node `WRONG` does not exist"));
        assert!(state.prs.is_empty());

        let update = parse_document(
            "mutation { updatePullRequest(input: { pullRequestId: \"PR_missing\", \
             title: \"Updated\", clientMutationId: \"update\" }) { \
             clientMutationId } }",
        );
        let error =
            handle_update_pr(&mut state, root_field(&update), &existing_branch, false).unwrap_err();
        assert!(error.contains("Pull request node `PR_missing` does not exist"));

        state.add_pr(PrEntry::mock(MockPrArgs {
            number: 1,
            title: "Title".to_string(),
            body: String::new(),
            head: "Ghead".to_string(),
            base: "main".to_string(),
        }));
        let update = parse_document(
            "mutation { updatePullRequest(input: { pullRequestId: \"PR_1\", \
             baseRefName: \"Ghead\", clientMutationId: \"update\" }) { \
             clientMutationId } }",
        );
        let error =
            handle_update_pr(&mut state, root_field(&update), &existing_branch, false).unwrap_err();
        assert!(error.contains("head and base branches must differ"));
    }

    #[test]
    fn update_handler_returns_closed_state_for_a_closed_pull_request() {
        let document = parse_document(
            "mutation { op0: updatePullRequest(input: { pullRequestId: \"PR_3\", \
             title: \"Updated\", clientMutationId: \"gherrit:update:PR_3\" }) { \
             clientMutationId, pullRequest { number, id, state } } }",
        );
        validate_supported_document(&document, &None).unwrap();
        let mut state = MockState::new("owner".to_string(), "repo".to_string());
        state.add_pr(PrEntry {
            state: PullRequestState::Closed,
            ..PrEntry::mock(MockPrArgs {
                number: 3,
                title: "Old".to_string(),
                body: "Body".to_string(),
                head: "Ghead".to_string(),
                base: "main".to_string(),
            })
        });

        let response =
            handle_update_pr(&mut state, root_field(&document), &existing_branch, false).unwrap();
        assert_eq!(
            response,
            serde_json::json!({
                "clientMutationId": "gherrit:update:PR_3",
                "pullRequest": { "number": 3, "id": "PR_3", "state": "CLOSED" }
            })
        );
        assert_eq!(state.prs[0].title, "Updated");
        assert_eq!(state.prs[0].state, PullRequestState::Closed);
    }
}

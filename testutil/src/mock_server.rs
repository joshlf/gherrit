use std::{
    collections::{HashSet, VecDeque},
    future::IntoFuture,
    path::PathBuf,
    sync::{mpsc::Sender, Arc, LazyLock, RwLock},
};

use apollo_compiler::{executable, validation::Valid, ExecutableDocument};
use axum::{extract::State, http::StatusCode, routing::post, Json, Router};
use serde::{Deserialize, Serialize};
use tokio::net::TcpListener;

use crate::{git_interceptor, FailureKind, GraphQlOperation, TestEnvironment};

mod codec;
mod mutations;
mod queries;
use codec::{graphql_operations, graphql_variables, response_key, validate_supported_document};
use mutations::{handle_create_pr, handle_update_pr};
use queries::handle_repository_query;

const MAX_PULL_REQUEST_CANDIDATES: usize = 100;

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
    pub graphql_requests: Vec<Vec<GraphQlOperation>>,
    pub max_graphql_operations_per_request: Option<usize>,
    pub repo_owner: String,
    pub repo_name: String,
    pub faults: VecDeque<FailureKind>,
}

impl MockState {
    pub fn new(owner: String, name: String) -> Self {
        Self { repo_owner: owner, repo_name: name, ..Default::default() }
    }

    pub fn add_pr(&mut self, pr: PrEntry) {
        self.prs.push(pr);
    }
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct PrEntry {
    pub id: u64,
    pub number: usize,
    #[serde(rename = "html_url")]
    pub html_url: String,
    #[serde(rename = "url")]
    pub api_url: String,
    #[serde(rename = "node_id")]
    pub node_id: String,
    pub state: String,
    pub user: User,
    #[serde(rename = "title")]
    pub title: Option<String>,
    #[serde(rename = "body")]
    pub body: Option<String>,
    #[serde(rename = "head")]
    pub head: RefInfo,
    #[serde(rename = "base")]
    pub base: RefInfo,
    pub created_at: String,
    pub updated_at: String,
}

pub struct MockPrArgs<'a> {
    pub id: u64,
    pub title: String,
    pub body: String,
    pub head: String,
    pub base: String,
    pub repo_owner: &'a str,
    pub repo_name: &'a str,
}

impl PrEntry {
    pub fn mock(args: MockPrArgs) -> Self {
        let MockPrArgs { id, title, body, head, base, repo_owner, repo_name } = args;
        let node_id = format!("PR_{}", id);
        let url_path = format!("repos/{}/{}/pulls/{}", repo_owner, repo_name, id);
        let html_url = format!("http://github.com/{url_path}");
        Self {
            id,
            number: id as usize,
            html_url,
            api_url: format!("http://api.github.com/{url_path}"),
            node_id,
            state: "OPEN".to_string(),
            user: User {
                login: "test-user".to_string(),
                id: 123,
                node_id: "MDQ6VXNlcjE=".to_string(),
                avatar_url: "https://example.com/avatar".to_string(),
                gravatar_id: "".to_string(),
                url: "https://api.github.com/users/test-user".to_string(),
                html_url: "https://github.com/test-user".to_string(),
                followers_url: "https://api.github.com/users/test-user/followers".to_string(),
                following_url: "https://api.github.com/users/test-user/following{/other_user}"
                    .to_string(),
                gists_url: "https://api.github.com/users/test-user/gists{/gist_id}".to_string(),
                starred_url: "https://api.github.com/users/test-user/starred{/owner}{/repo}"
                    .to_string(),
                subscriptions_url: "https://api.github.com/users/test-user/subscriptions"
                    .to_string(),
                organizations_url: "https://api.github.com/users/test-user/orgs".to_string(),
                repos_url: "https://api.github.com/users/test-user/repos".to_string(),
                events_url: "https://api.github.com/users/test-user/events{/privacy}".to_string(),
                received_events_url: "https://api.github.com/users/test-user/received_events"
                    .to_string(),
                type_field: "User".to_string(),
                site_admin: false,
            },
            title: Some(title),
            body: Some(body),
            head: RefInfo { ref_field: head, sha: "".to_string() },
            base: RefInfo { ref_field: base, sha: "".to_string() },
            created_at: "2023-01-01T00:00:00Z".to_string(),
            updated_at: "2023-01-01T00:00:00Z".to_string(),
        }
    }
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct User {
    pub login: String,
    pub id: u64,
    pub node_id: String,
    pub avatar_url: String,
    pub gravatar_id: String,
    pub url: String,
    pub html_url: String,
    pub followers_url: String,
    pub following_url: String,
    pub gists_url: String,
    pub starred_url: String,
    pub subscriptions_url: String,
    pub organizations_url: String,
    pub repos_url: String,
    pub events_url: String,
    pub received_events_url: String,
    #[serde(rename = "type")]
    pub type_field: String,
    pub site_admin: bool,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct RefInfo {
    #[serde(rename = "ref")]
    pub ref_field: String,
    pub sha: String,
}

#[derive(Clone)]
struct AppState {
    state: Arc<RwLock<MockState>>,
    remote_path: PathBuf,
    system_git: PathBuf,
    test_environment: TestEnvironment,
}

/// Runs a mock GitHub API server until `shutdown_rx` is signaled.
pub(super) async fn run_mock_server(
    state: Arc<RwLock<MockState>>,
    remote_path: PathBuf,
    system_git: PathBuf,
    test_environment: TestEnvironment,
    ready_tx: Sender<String>,
    shutdown_rx: tokio::sync::oneshot::Receiver<()>,
) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let url = format!("http://{}", addr);

    let git_routes = git_interceptor::routes(state.clone());
    let app_state = AppState { state, remote_path, system_git, test_environment };

    let app =
        Router::new().route("/graphql", post(graphql)).with_state(app_state).merge(git_routes);

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
        CreatePr => operations.contains(&GraphQlOperation::CreatePr),
        UpdatePr => operations.contains(&GraphQlOperation::UpdatePr),
        Git(_) => false,
    };

    if !matches {
        return None;
    }

    mock_state.faults.pop_front()
}

async fn graphql(
    State(app_state): State<AppState>,
    Json(payload): Json<serde_json::Value>,
) -> (StatusCode, Json<serde_json::Value>) {
    let Some(query) = payload.get("query").and_then(|value| value.as_str()) else {
        return graphql_http_error("Invalid GraphQL payload: missing string field `query`");
    };
    let variables = match graphql_variables(&payload) {
        Ok(variables) => variables,
        Err(message) => return graphql_http_error(&message),
    };

    let document =
        match ExecutableDocument::parse_and_validate(&GITHUB_SCHEMA, query, "query.graphql") {
            Ok(doc) => doc,
            Err(e) => {
                eprintln!("DEBUG: GraphQL validation errors: {:?}", e.errors);
                return graphql_http_error("GraphQL request failed schema validation");
            }
        };
    if let Err(message) = validate_supported_document(&document, &variables) {
        return graphql_http_error(&message);
    }

    let operations = graphql_operations(&document);
    let mut mock_state = app_state.state.write().unwrap();
    mock_state.graphql_requests.push(operations.clone());
    if mock_state.max_graphql_operations_per_request.is_some_and(|limit| operations.len() > limit) {
        return (
            StatusCode::OK,
            Json(serde_json::json!({
                "errors": [{
                    "type": "RESOURCE_LIMITS_EXCEEDED",
                    "message": "Request exceeds the mock GraphQL operation limit",
                }]
            })),
        );
    }
    if let Some(failure) = check_and_apply_graphql_failure(&mut mock_state, &operations) {
        return (
            StatusCode::OK,
            Json(serde_json::json!({
                "errors": [
                    { "message": format!("Injected {failure:?} failure") }
                ]
            })),
        );
    }

    let mut response_data = serde_json::Map::new();

    let mut errors = Vec::new();

    for operation in document.operations.iter() {
        for selection in operation.selection_set.selections.iter() {
            if let executable::Selection::Field(field) = selection {
                let alias = response_key(field);

                let result = match field.name.as_str() {
                    "updatePullRequest" => handle_update_pr(&mut mock_state, field, &|branch| {
                        remote_branch_exists(&app_state, branch)
                    }),
                    "createPullRequest" => handle_create_pr(&mut mock_state, field, &|branch| {
                        remote_branch_exists(&app_state, branch)
                    }),
                    "repository" => handle_repository_query(&mock_state, field, &variables),
                    _ => unreachable!("request was checked by validate_supported_document"),
                };
                match result {
                    Ok(value) => {
                        response_data.insert(alias, value);
                    }
                    Err(message) => {
                        response_data.insert(alias.clone(), serde_json::Value::Null);
                        errors.push(serde_json::json!({
                            "message": message,
                            "path": [alias],
                        }));
                    }
                }
            }
        }
    }

    let mut response_json = serde_json::Map::new();
    response_json.insert("data".to_string(), serde_json::Value::Object(response_data));
    if !errors.is_empty() {
        response_json.insert("errors".to_string(), serde_json::Value::Array(errors));
    }

    (StatusCode::OK, Json(serde_json::Value::Object(response_json)))
}

fn graphql_http_error(message: &str) -> (StatusCode, Json<serde_json::Value>) {
    (
        StatusCode::BAD_REQUEST,
        Json(serde_json::json!({
            "errors": [{ "message": message }],
        })),
    )
}

fn remote_branch_exists(app_state: &AppState, branch: &str) -> Result<bool, String> {
    let reference = format!("refs/heads/{branch}");
    let output = app_state
        .test_environment
        .command(&app_state.system_git)
        .arg("--git-dir")
        .arg(&app_state.remote_path)
        .args(["show-ref", "--verify", "--quiet", &reference])
        .output()
        .map_err(|error| format!("Failed to inspect remote Git ref `{reference}`: {error}"))?;
    match output.status.code() {
        Some(0) => Ok(true),
        Some(1) => Ok(false),
        code => Err(format!("Inspecting remote Git ref `{reference}` exited with {code:?}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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

    #[test]
    fn operation_failure_only_matches_the_requested_operation() {
        let mut state = MockState {
            faults: VecDeque::from([FailureKind::UpdatePr, FailureKind::CreatePr]),
            ..Default::default()
        };

        assert_eq!(check_and_apply_graphql_failure(&mut state, &[GraphQlOperation::Query]), None);
        assert_eq!(state.faults, VecDeque::from([FailureKind::UpdatePr, FailureKind::CreatePr]));

        assert_eq!(
            check_and_apply_graphql_failure(&mut state, &[GraphQlOperation::CreatePr]),
            None
        );

        assert_eq!(
            check_and_apply_graphql_failure(&mut state, &[GraphQlOperation::UpdatePr]),
            Some(FailureKind::UpdatePr)
        );
        assert_eq!(state.faults, VecDeque::from([FailureKind::CreatePr]));
    }

    #[test]
    fn generic_graphql_failure_matches_any_operation() {
        let mut state =
            MockState { faults: VecDeque::from([FailureKind::GraphQl]), ..Default::default() };

        assert_eq!(
            check_and_apply_graphql_failure(&mut state, &[GraphQlOperation::Query]),
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
        let repository_id = parse_document(
            "query RepositoryID($owner: String!, $name: String!) { \
             repository(owner: $owner, name: $name) { id } }",
        );
        validate_supported_document(&repository_id, &variables).unwrap();

        let lookup = parse_document(
            "query { op0: repository(owner: \"owner\", name: \"repo\") { \
             open: pullRequests(headRefName: \"Ghead\", first: 100, \
             states: [OPEN]) { nodes { number, id, title, body, baseRefName, \
             state, isCrossRepository } pageInfo { hasNextPage } } \
             historical: pullRequests(headRefName: \"Ghead\", first: 100, \
             states: [CLOSED, MERGED]) { nodes { number, id, title, body, \
             baseRefName, state, isCrossRepository } pageInfo { hasNextPage } } } }",
        );
        validate_supported_document(&lookup, &None).unwrap();

        let create = parse_document(
            "mutation { op0: createPullRequest(input: { repositoryId: \
             \"REPO_NODE_ID\", baseRefName: \"main\", headRefName: \"Ghead\", \
             title: \"Title\", body: \"Body\" }) { pullRequest { number, url, id } } }",
        );
        validate_supported_document(&create, &None).unwrap();

        let update = parse_document(
            "mutation { op0: updatePullRequest(input: { pullRequestId: \"PR_1\", \
             title: \"Updated\" }) { clientMutationId } }",
        );
        validate_supported_document(&update, &None).unwrap();
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
            .contains("field `repository.name`"));

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

        let duplicate_mutation = parse_document(
            "mutation { createPullRequest(input: { repositoryId: \"REPO_NODE_ID\", \
             baseRefName: \"main\", headRefName: \"Ghead\", title: \"Title\" }) { \
             pullRequest { number } } createPullRequest(input: { repositoryId: \
             \"REPO_NODE_ID\", baseRefName: \"main\", headRefName: \"Ghead\", \
             title: \"Title\" }) { pullRequest { number } } }",
        );
        assert!(validate_supported_document(&duplicate_mutation, &None)
            .unwrap_err()
            .contains("duplicate response key `createPullRequest`"));

        let duplicate_states = parse_document(
            "query { repository(owner: \"owner\", name: \"repo\") { \
             pullRequests(headRefName: \"Ghead\", first: 100, \
             states: [OPEN, CLOSED, MERGED, OPEN]) { nodes { number } } } }",
        );
        assert!(validate_supported_document(&duplicate_states, &None)
            .unwrap_err()
            .contains("only supports `repository.pullRequests(states:"));
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
            "query RepositoryID($owner: String!, $name: String!) { \
             repository(owner: $owner, name: $name) { id } }",
        );
        let error = validate_supported_document(&document, &None).unwrap_err();
        assert!(error.contains("variable `$owner`"), "unexpected error: {error}");
    }

    #[test]
    fn repository_response_contains_only_selected_fields() {
        let document = parse_document(
            "query { repository(owner: \"owner\", name: \"repo\") { \
             open: pullRequests(headRefName: \"Ghead\", first: 100, \
             states: [OPEN]) { nodes { number, isCrossRepository } \
             pageInfo { hasNextPage } } } }",
        );
        validate_supported_document(&document, &None).unwrap();

        let mut state = MockState::new("owner".to_string(), "repo".to_string());
        state.add_pr(PrEntry::mock(MockPrArgs {
            id: 1,
            title: "Title".to_string(),
            body: "Body".to_string(),
            head: "Ghead".to_string(),
            base: "main".to_string(),
            repo_owner: "owner",
            repo_name: "repo",
        }));
        state.cross_repository_prs.insert(1);
        let response = handle_repository_query(&state, root_field(&document), &None).unwrap();
        assert_eq!(
            response,
            serde_json::json!({
                "open": {
                    "nodes": [{ "number": 1, "isCrossRepository": true }],
                    "pageInfo": { "hasNextPage": false }
                }
            })
        );
    }

    #[test]
    fn repository_response_rejects_another_repository() {
        let state = MockState::new("owner".to_string(), "repo".to_string());

        for query in [
            "query { repository(owner: \"other\", name: \"repo\") { id } }",
            "query { repository(owner: \"owner\", name: \"other\") { id } }",
        ] {
            let document = parse_document(query);
            validate_supported_document(&document, &None).unwrap();

            assert_eq!(
                handle_repository_query(&state, root_field(&document), &None).unwrap(),
                serde_json::Value::Null,
                "query: {query}"
            );
        }
    }

    #[test]
    fn create_validates_same_repository_refs_uniqueness_and_numbering() {
        let document = parse_document(
            "mutation { createPullRequest(input: { repositoryId: \"REPO_NODE_ID\", \
             baseRefName: \"main\", headRefName: \"Gnew\", title: \"Title\" }) { \
             pullRequest { number } } }",
        );
        let mut state = MockState::new("owner".to_string(), "repo".to_string());
        state.add_pr(PrEntry::mock(MockPrArgs {
            id: 7,
            title: "Old".to_string(),
            body: String::new(),
            head: "Gnew".to_string(),
            base: "main".to_string(),
            repo_owner: "owner",
            repo_name: "repo",
        }));
        state.cross_repository_prs.insert(7);

        let response = handle_create_pr(&mut state, root_field(&document), &|_| Ok(true)).unwrap();
        assert_eq!(response.pointer("/pullRequest/number"), Some(&serde_json::json!(8)));
        assert_eq!(state.prs.len(), 2);

        let error = handle_create_pr(&mut state, root_field(&document), &|_| Ok(true)).unwrap_err();
        assert!(error.contains("already exists"));
        assert_eq!(state.prs.len(), 2);

        let mut state = MockState::new("owner".to_string(), "repo".to_string());
        let error =
            handle_create_pr(&mut state, root_field(&document), &|branch| Ok(branch == "main"))
                .unwrap_err();
        assert!(error.contains("Head branch `Gnew` does not exist"));
        assert!(state.prs.is_empty());
    }

    #[test]
    fn mutations_reject_unknown_repository_and_pull_request_ids() {
        let create = parse_document(
            "mutation { createPullRequest(input: { repositoryId: \"WRONG\", \
             baseRefName: \"main\", headRefName: \"Ghead\", title: \"Title\" }) { \
             pullRequest { number } } }",
        );
        let mut state = MockState::new("owner".to_string(), "repo".to_string());
        let error = handle_create_pr(&mut state, root_field(&create), &|_| Ok(true)).unwrap_err();
        assert!(error.contains("Repository node `WRONG` does not exist"));
        assert!(state.prs.is_empty());

        let update = parse_document(
            "mutation { updatePullRequest(input: { pullRequestId: \"PR_missing\", \
             title: \"Updated\" }) { clientMutationId } }",
        );
        let error = handle_update_pr(&mut state, root_field(&update), &|_| Ok(true)).unwrap_err();
        assert!(error.contains("Pull request node `PR_missing` does not exist"));

        state.add_pr(PrEntry::mock(MockPrArgs {
            id: 1,
            title: "Title".to_string(),
            body: String::new(),
            head: "Ghead".to_string(),
            base: "main".to_string(),
            repo_owner: "owner",
            repo_name: "repo",
        }));
        let update = parse_document(
            "mutation { updatePullRequest(input: { pullRequestId: \"PR_1\", \
             baseRefName: \"Ghead\" }) { clientMutationId } }",
        );
        let error = handle_update_pr(&mut state, root_field(&update), &|_| Ok(true)).unwrap_err();
        assert!(error.contains("head and base branches must differ"));
    }
}

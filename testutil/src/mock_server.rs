use std::{
    collections::{HashSet, VecDeque},
    future::IntoFuture,
    path::PathBuf,
    sync::{mpsc::Sender, Arc, LazyLock, RwLock},
};

use apollo_compiler::{ast, executable, validation::Valid, ExecutableDocument, Name, Node};
use axum::{
    body::{Body, Bytes},
    extract::State,
    http::{header::LOCATION, HeaderMap, HeaderValue, StatusCode},
    response::{IntoResponse, Response},
    routing::{any, post},
    Json, Router,
};
use serde::{Deserialize, Serialize};
use tokio::net::TcpListener;

use crate::{
    git_interceptor, FailureKind, GraphQlOperation, RedirectStatus, RetryableHttpStatus,
    TestEnvironment,
};

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
    pub graphql_redirect_trap_requests: usize,
    pub max_graphql_query_operations_per_request: Option<usize>,
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

    let app = Router::new()
        .route("/graphql", post(graphql))
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
    is_repository_id_query: bool,
    create_request_number: usize,
) -> Option<FailureKind> {
    use FailureKind::*;

    let fail_action = mock_state.faults.front()?;
    let matches = match fail_action {
        GraphQl => true,
        QueryTransport | QueryHttp(_) => {
            !operations.is_empty()
                && operations.iter().all(|operation| *operation == GraphQlOperation::Query)
        }
        RepositoryIdHttp(_) => is_repository_id_query,
        CreatePr | CreatePrHttp(_) | CreatePrRedirect(_) => {
            operations.contains(&GraphQlOperation::CreatePr)
        }
        SecondCreatePrHttp(_) => {
            create_request_number == 2 && operations.contains(&GraphQlOperation::CreatePr)
        }
        UpdatePr => operations.contains(&GraphQlOperation::UpdatePr),
        Git(_) => false,
    };

    if !matches {
        return None;
    }

    mock_state.faults.pop_front()
}

fn graphql_operations(document: &ExecutableDocument) -> Vec<GraphQlOperation> {
    document
        .operations
        .iter()
        .flat_map(|operation| operation.selection_set.selections.iter())
        .filter_map(|selection| {
            let executable::Selection::Field(field) = selection else { return None };
            match field.name.as_str() {
                "repository" => Some(GraphQlOperation::Query),
                "createPullRequest" => Some(GraphQlOperation::CreatePr),
                "updatePullRequest" => Some(GraphQlOperation::UpdatePr),
                _ => None,
            }
        })
        .collect()
}

type GraphQlVariables = Option<serde_json::Map<String, serde_json::Value>>;

fn graphql_variables(payload: &serde_json::Value) -> Result<GraphQlVariables, String> {
    match payload.get("variables") {
        None => Ok(None),
        Some(serde_json::Value::Object(variables)) => Ok(Some(variables.clone())),
        Some(_) => Err("Invalid GraphQL payload: `variables` must be an object".to_string()),
    }
}

fn response_key(field: &executable::Field) -> String {
    field
        .alias
        .as_ref()
        .map(|alias| alias.as_str())
        .unwrap_or_else(|| field.name.as_str())
        .to_string()
}

fn selected_fields<'a>(
    selection_set: &'a executable::SelectionSet,
    path: &str,
) -> Result<Vec<&'a executable::Field>, String> {
    let fields: Vec<_> = selection_set
        .selections
        .iter()
        .map(|selection| {
            let executable::Selection::Field(field) = selection else {
                return Err(format!("The mock GitHub API does not support fragments at `{path}`"));
            };
            if !field.directives.is_empty() {
                return Err(format!(
                    "The mock GitHub API does not support directives at `{path}.{}`",
                    field.name
                ));
            }
            Ok(field.as_ref())
        })
        .collect::<Result<_, _>>()?;
    let mut response_keys = HashSet::new();
    for field in &fields {
        let key = response_key(field);
        if !response_keys.insert(key.clone()) {
            return Err(format!(
                "The mock GitHub API does not support duplicate response key `{key}` at `{path}`"
            ));
        }
    }
    Ok(fields)
}

fn validate_argument_names(
    field: &executable::Field,
    path: &str,
    allowed: &[&str],
) -> Result<(), String> {
    for argument in &field.arguments {
        if !allowed.contains(&argument.name.as_str()) {
            return Err(format!(
                "The mock GitHub API does not support argument `{path}({}: ...)`",
                argument.name
            ));
        }
    }
    Ok(())
}

fn validate_scalar_fields(
    selection_set: &executable::SelectionSet,
    path: &str,
    allowed: &[&str],
) -> Result<(), String> {
    for field in selected_fields(selection_set, path)? {
        if !allowed.contains(&field.name.as_str()) {
            return Err(format!(
                "The mock GitHub API does not support field `{path}.{}`",
                field.name
            ));
        }
    }
    Ok(())
}

fn validate_exact_fields<'a>(
    selection_set: &'a executable::SelectionSet,
    path: &str,
    required: &[&str],
) -> Result<Vec<&'a executable::Field>, String> {
    let fields = selected_fields(selection_set, path)?;
    let names = fields.iter().map(|field| field.name.as_str()).collect::<HashSet<_>>();
    let required_names = required.iter().copied().collect::<HashSet<_>>();
    if fields.len() != required.len()
        || names != required_names
        || fields.iter().any(|field| field.alias.is_some())
    {
        return Err(format!(
            "The mock GitHub API requires exactly unaliased fields {} at `{path}`",
            required.join(", ")
        ));
    }
    Ok(fields)
}

fn input_object<'a>(
    field: &'a executable::Field,
    path: &str,
) -> Result<&'a [(Name, Node<ast::Value>)], String> {
    extract_input_field(field, "input")
        .map(Vec::as_slice)
        .ok_or_else(|| format!("The mock GitHub API requires an inline object at `{path}(input:)`"))
}

fn validate_input_fields(
    input: &[(Name, Node<ast::Value>)],
    path: &str,
    allowed: &[&str],
) -> Result<(), String> {
    for (name, value) in input {
        if !allowed.contains(&name.as_str()) {
            return Err(format!(
                "The mock GitHub API does not support input field `{path}.input.{name}`"
            ));
        }
        if !matches!(&**value, ast::Value::String(_)) {
            return Err(format!(
                "The mock GitHub API only supports inline string values at `{path}.input.{name}`"
            ));
        }
    }
    Ok(())
}

fn argument<'a>(field: &'a executable::Field, name: &str) -> Option<&'a ast::Value> {
    field.arguments.iter().find(|argument| argument.name == name).map(|argument| &*argument.value)
}

fn resolve_string_argument(
    field: &executable::Field,
    name: &str,
    path: &str,
    variables: &GraphQlVariables,
) -> Result<String, String> {
    let value = argument(field, name)
        .ok_or_else(|| format!("Missing GraphQL argument `{path}({name}: ...)`"))?;
    match value {
        ast::Value::String(value) => Ok(value.clone()),
        ast::Value::Variable(variable) => variables
            .as_ref()
            .and_then(|variables| variables.get(variable.as_str()))
            .and_then(serde_json::Value::as_str)
            .map(ToString::to_string)
            .ok_or_else(|| {
                format!("Missing or non-string GraphQL variable `${variable}` for `{path}.{name}`")
            }),
        _ => Err(format!("The mock GitHub API requires a string at `{path}({name}: ...)`")),
    }
}

fn validate_pull_requests_field(field: &executable::Field) -> Result<(), String> {
    const PATH: &str = "repository.pullRequests";
    validate_argument_names(field, PATH, &["headRefName", "first", "states"])?;

    let first = argument(field, "first")
        .and_then(|value| match value {
            ast::Value::Int(value) => Some(value.as_str()),
            _ => None,
        })
        .ok_or_else(|| {
            format!("The mock GitHub API requires `{PATH}(first: {MAX_PULL_REQUEST_CANDIDATES})`")
        })?;
    if first != MAX_PULL_REQUEST_CANDIDATES.to_string() {
        return Err(format!(
            "The mock GitHub API only supports `{PATH}(first: {MAX_PULL_REQUEST_CANDIDATES})`"
        ));
    }

    let states = argument(field, "states")
        .and_then(|value| match value {
            ast::Value::List(values) => Some(values),
            _ => None,
        })
        .ok_or_else(|| format!("The mock GitHub API requires `{PATH}(states: ...)`"))?;
    let state_count = states.len();
    let states: HashSet<_> = states
        .iter()
        .filter_map(|value| match &**value {
            ast::Value::Enum(value) => Some(value.as_str()),
            _ => None,
        })
        .collect();
    let is_open = state_count == 1 && states == HashSet::from(["OPEN"]);
    let is_historical = state_count == 2 && states == HashSet::from(["CLOSED", "MERGED"]);
    if !is_open && !is_historical {
        return Err(format!(
            "The mock GitHub API only supports `{PATH}(states: [OPEN])` or `{PATH}(states: [CLOSED, MERGED])`"
        ));
    }

    for field in selected_fields(&field.selection_set, PATH)? {
        match field.name.as_str() {
            "nodes" => validate_scalar_fields(
                &field.selection_set,
                "repository.pullRequests.nodes",
                &["number", "id", "title", "body", "baseRefName", "state", "isCrossRepository"],
            )?,
            "pageInfo" => validate_scalar_fields(
                &field.selection_set,
                "repository.pullRequests.pageInfo",
                &["hasNextPage"],
            )?,
            _ => {
                return Err(format!(
                    "The mock GitHub API does not support field `{PATH}.{}`",
                    field.name
                ));
            }
        }
    }
    Ok(())
}

fn validate_repository_field(
    field: &executable::Field,
    variables: &GraphQlVariables,
) -> Result<(), String> {
    const PATH: &str = "repository";
    validate_argument_names(field, PATH, &["owner", "name"])?;
    resolve_string_argument(field, "owner", PATH, variables)?;
    resolve_string_argument(field, "name", PATH, variables)?;

    for field in selected_fields(&field.selection_set, PATH)? {
        match field.name.as_str() {
            "id" => {}
            "pullRequests" => {
                validate_pull_requests_field(field)?;
                resolve_string_argument(
                    field,
                    "headRefName",
                    "repository.pullRequests",
                    variables,
                )?;
            }
            _ => {
                return Err(format!(
                    "The mock GitHub API does not support field `{PATH}.{}`",
                    field.name
                ));
            }
        }
    }
    Ok(())
}

fn validate_create_field(field: &executable::Field) -> Result<(), String> {
    const PATH: &str = "createPullRequest";
    validate_argument_names(field, PATH, &["input"])?;
    let input = input_object(field, PATH)?;
    validate_input_fields(
        input,
        PATH,
        &[
            "repositoryId",
            "headRepositoryId",
            "baseRefName",
            "headRefName",
            "title",
            "body",
            "clientMutationId",
        ],
    )?;
    for required in [
        "repositoryId",
        "headRepositoryId",
        "baseRefName",
        "headRefName",
        "title",
        "clientMutationId",
    ] {
        required_string_field(input, required, PATH)?;
    }

    for field in selected_fields(&field.selection_set, PATH)? {
        match field.name.as_str() {
            "clientMutationId" => {}
            "pullRequest" => {
                for selected in validate_exact_fields(
                    &field.selection_set,
                    "createPullRequest.pullRequest",
                    &[
                        "number",
                        "id",
                        "state",
                        "headRefName",
                        "headRefOid",
                        "headRepository",
                        "baseRefName",
                        "baseRefOid",
                        "baseRepository",
                    ],
                )? {
                    match selected.name.as_str() {
                        "headRepository" | "baseRepository" => validate_scalar_fields(
                            &selected.selection_set,
                            "createPullRequest.pullRequest.repository",
                            &["id"],
                        )?,
                        _ => validate_scalar_fields(
                            &selected.selection_set,
                            "createPullRequest.pullRequest.scalar",
                            &[],
                        )?,
                    }
                }
            }
            _ => {
                return Err(format!(
                    "The mock GitHub API does not support field `{PATH}.{}`",
                    field.name
                ));
            }
        }
    }
    Ok(())
}

fn validate_update_field(field: &executable::Field) -> Result<(), String> {
    const PATH: &str = "updatePullRequest";
    validate_argument_names(field, PATH, &["input"])?;
    let input = input_object(field, PATH)?;
    validate_input_fields(
        input,
        PATH,
        &["pullRequestId", "baseRefName", "title", "body", "clientMutationId"],
    )?;
    required_string_field(input, "pullRequestId", PATH)?;
    required_string_field(input, "clientMutationId", PATH)?;
    if !["baseRefName", "title", "body"].iter().any(|name| get_string_field(input, name).is_some())
    {
        return Err("The mock GitHub API requires at least one pull request update".to_string());
    }
    for field in selected_fields(&field.selection_set, PATH)? {
        match field.name.as_str() {
            "clientMutationId" => {}
            "pullRequest" => validate_scalar_fields(
                &field.selection_set,
                "updatePullRequest.pullRequest",
                &["number", "id"],
            )?,
            _ => {
                return Err(format!(
                    "The mock GitHub API does not support field `{PATH}.{}`",
                    field.name
                ));
            }
        }
    }
    Ok(())
}

fn validate_supported_document(
    document: &ExecutableDocument,
    variables: &GraphQlVariables,
) -> Result<(), String> {
    if document.operations.len() != 1 {
        return Err("The mock GitHub API supports exactly one GraphQL operation".to_string());
    }
    if !document.fragments.is_empty() {
        return Err("The mock GitHub API does not support GraphQL fragments".to_string());
    }

    let operation = document.operations.iter().next().unwrap();
    if !operation.directives.is_empty() {
        return Err("The mock GitHub API does not support operation directives".to_string());
    }
    for field in selected_fields(&operation.selection_set, "operation")? {
        match field.name.as_str() {
            "repository" => validate_repository_field(field, variables)?,
            "createPullRequest" => validate_create_field(field)?,
            "updatePullRequest" => validate_update_field(field)?,
            _ => {
                return Err(format!(
                    "The mock GitHub API does not support root field `{}`",
                    field.name
                ));
            }
        }
    }
    Ok(())
}

async fn graphql(
    State(app_state): State<AppState>,
    Json(payload): Json<serde_json::Value>,
) -> Response {
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
    let create_request_number = mock_state
        .graphql_requests
        .iter()
        .filter(|request| request.contains(&GraphQlOperation::CreatePr))
        .count();
    if operations.iter().all(|operation| *operation == GraphQlOperation::Query)
        && mock_state
            .max_graphql_query_operations_per_request
            .is_some_and(|limit| operations.len() > limit)
    {
        return graphql_response(
            StatusCode::OK,
            serde_json::json!({
                "errors": [{
                    "type": "RESOURCE_LIMITS_EXCEEDED",
                    "message": "Request exceeds the mock GraphQL operation limit",
                }]
            }),
        );
    }
    if let Some(failure) = check_and_apply_graphql_failure(
        &mut mock_state,
        &operations,
        query.trim_start().starts_with("query RepositoryID("),
        create_request_number,
    ) {
        return graphql_failure_response(failure);
    }

    let mut response_data = serde_json::Map::new();

    let mut errors = Vec::new();

    for operation in document.operations.iter() {
        for selection in operation.selection_set.selections.iter() {
            if let executable::Selection::Field(field) = selection {
                let alias = response_key(field);

                let result = match field.name.as_str() {
                    "updatePullRequest" => handle_update_pr(&mut mock_state, field, &|branch| {
                        remote_branch_oid(&app_state, branch)
                    }),
                    "createPullRequest" => handle_create_pr(&mut mock_state, field, &|branch| {
                        remote_branch_oid(&app_state, branch)
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

    graphql_response(StatusCode::OK, serde_json::Value::Object(response_json))
}

async fn graphql_redirect_trap(State(app_state): State<AppState>) -> Response {
    app_state.state.write().unwrap().graphql_redirect_trap_requests += 1;
    graphql_response(
        StatusCode::INTERNAL_SERVER_ERROR,
        serde_json::json!({ "message": "Mutation redirect trap was reached" }),
    )
}

fn retryable_status(status: RetryableHttpStatus) -> StatusCode {
    match status {
        RetryableHttpStatus::TooManyRequests => StatusCode::TOO_MANY_REQUESTS,
        RetryableHttpStatus::ServiceUnavailable => StatusCode::SERVICE_UNAVAILABLE,
    }
}

fn redirect_status(status: RedirectStatus) -> StatusCode {
    match status {
        RedirectStatus::Temporary => StatusCode::TEMPORARY_REDIRECT,
        RedirectStatus::Permanent => StatusCode::PERMANENT_REDIRECT,
    }
}

fn graphql_failure_response(failure: FailureKind) -> Response {
    use FailureKind::*;

    if matches!(failure, QueryTransport) {
        let body = Body::from_stream(futures_util::stream::once(async {
            Err::<Bytes, _>(std::io::Error::other("Injected GraphQL response transport failure"))
        }));
        return Response::new(body);
    }

    let status = match failure {
        QueryHttp(status)
        | RepositoryIdHttp(status)
        | CreatePrHttp(status)
        | SecondCreatePrHttp(status) => retryable_status(status),
        CreatePrRedirect(status) => redirect_status(status),
        GraphQl | CreatePr | UpdatePr => StatusCode::OK,
        QueryTransport => unreachable!("handled above"),
        Git(_) => unreachable!("Git failures are not handled by the GraphQL endpoint"),
    };
    let mut headers = HeaderMap::new();
    if matches!(failure, CreatePrRedirect(_)) {
        headers.insert(LOCATION, HeaderValue::from_static("/graphql-redirect-trap"));
    }
    let message = format!("Injected {failure:?} failure");
    (
        status,
        headers,
        Json(serde_json::json!({
            "message": message,
            "errors": [{ "message": message }],
        })),
    )
        .into_response()
}

fn graphql_response(status: StatusCode, value: serde_json::Value) -> Response {
    (status, Json(value)).into_response()
}

fn graphql_http_error(message: &str) -> Response {
    graphql_response(
        StatusCode::BAD_REQUEST,
        serde_json::json!({
            "message": message,
            "errors": [{ "message": message }],
        }),
    )
}

fn remote_branch_oid(app_state: &AppState, branch: &str) -> Result<Option<String>, String> {
    let reference = format!("refs/heads/{branch}");
    let run = |arguments: &[&str]| {
        app_state
            .test_environment
            .command(&app_state.system_git)
            .arg("--git-dir")
            .arg(&app_state.remote_path)
            .args(arguments)
            .output()
            .map_err(|error| format!("Failed to inspect remote Git ref `{reference}`: {error}"))
    };
    match run(&["show-ref", "--verify", "--quiet", &reference])?.status.code() {
        Some(1) => return Ok(None),
        Some(0) => {}
        code => {
            return Err(format!("Inspecting remote Git ref `{reference}` exited with {code:?}"));
        }
    }
    let output = run(&["show-ref", "--hash", "--verify", &reference])?;
    if !output.status.success() {
        return Err(format!("Remote Git ref `{reference}` disappeared while it was inspected"));
    }
    let oid = std::str::from_utf8(&output.stdout)
        .map_err(|_| format!("Remote Git ref `{reference}` is not UTF-8"))?
        .trim_end_matches(['\r', '\n']);
    if oid.is_empty() {
        return Err(format!("Remote Git ref `{reference}` has an empty object ID"));
    }
    Ok(Some(oid.to_string()))
}

fn extract_input_field<'a>(
    field: &'a executable::Field,
    arg_name: &str,
) -> Option<&'a Vec<(Name, Node<ast::Value>)>> {
    field.arguments.iter().find(|arg| arg.name == arg_name).and_then(|arg| {
        if let ast::Value::Object(obj) = &*arg.value {
            Some(obj)
        } else {
            None
        }
    })
}

fn get_string_field(obj: &[(Name, Node<ast::Value>)], key: &str) -> Option<String> {
    obj.iter().find(|(k, _)| k == key).and_then(|(_, v)| {
        if let ast::Value::String(s) = &**v {
            Some(s.to_string())
        } else {
            None
        }
    })
}

fn required_string_field(
    input: &[(Name, Node<ast::Value>)],
    key: &str,
    path: &str,
) -> Result<String, String> {
    get_string_field(input, key)
        .ok_or_else(|| format!("The mock GitHub API requires string field `{path}.input.{key}`"))
}

fn handle_update_pr(
    mock_state: &mut MockState,
    field: &executable::Field,
    branch_oid: &dyn Fn(&str) -> Result<Option<String>, String>,
) -> Result<serde_json::Value, String> {
    const PATH: &str = "updatePullRequest";
    let input = input_object(field, PATH)?;
    let node_id = required_string_field(input, "pullRequestId", PATH)?;
    let client_mutation_id = required_string_field(input, "clientMutationId", PATH)?;
    let title = get_string_field(input, "title");
    let body = get_string_field(input, "body");
    let base = get_string_field(input, "baseRefName")
        .map(|base| -> Result<_, String> {
            let oid =
                branch_oid(&base)?.ok_or_else(|| format!("Base branch `{base}` does not exist"))?;
            Ok((base, oid))
        })
        .transpose()?;

    let Some(pr) = mock_state.prs.iter_mut().find(|pr| pr.node_id == node_id) else {
        return Err(format!("Pull request node `{node_id}` does not exist"));
    };
    if base.as_ref().map(|(name, _)| name.as_str()) == Some(pr.head.ref_field.as_str()) {
        return Err("Pull request head and base branches must differ".to_string());
    }
    if let Some(title) = title {
        pr.title = Some(title);
    }
    if let Some(body) = &body {
        pr.body = Some(body.clone());
    }
    if let Some((name, oid)) = base {
        pr.base = RefInfo { ref_field: name, sha: oid };
    }

    let mut response = serde_json::Map::new();
    for field in selected_fields(&field.selection_set, PATH)? {
        match field.name.as_str() {
            "clientMutationId" => {
                response.insert(response_key(field), serde_json::json!(client_mutation_id));
            }
            "pullRequest" => {
                let mut pull_request = serde_json::Map::new();
                for field in selected_fields(&field.selection_set, "updatePullRequest.pullRequest")?
                {
                    match field.name.as_str() {
                        "number" => {
                            pull_request.insert(response_key(field), serde_json::json!(pr.number));
                        }
                        "id" => {
                            pull_request.insert(response_key(field), serde_json::json!(node_id));
                        }
                        _ => unreachable!("request was checked by validate_update_field"),
                    }
                }
                response.insert(response_key(field), serde_json::Value::Object(pull_request));
            }
            _ => unreachable!("request was checked by validate_update_field"),
        }
    }
    Ok(serde_json::Value::Object(response))
}

fn handle_create_pr(
    mock_state: &mut MockState,
    field: &executable::Field,
    branch_oid: &dyn Fn(&str) -> Result<Option<String>, String>,
) -> Result<serde_json::Value, String> {
    const PATH: &str = "createPullRequest";
    let input = input_object(field, PATH)?;
    let repository_id = required_string_field(input, "repositoryId", PATH)?;
    let head_repository_id = required_string_field(input, "headRepositoryId", PATH)?;
    let base = required_string_field(input, "baseRefName", PATH)?;
    let head = required_string_field(input, "headRefName", PATH)?;
    let title = required_string_field(input, "title", PATH)?;
    let body = get_string_field(input, "body").unwrap_or_default();
    let client_mutation_id = required_string_field(input, "clientMutationId", PATH)?;

    if repository_id != "REPO_NODE_ID" {
        return Err(format!("Repository node `{repository_id}` does not exist"));
    }
    if head_repository_id != repository_id {
        return Err(format!("Head repository node `{head_repository_id}` does not exist"));
    }
    if base == head {
        return Err("Pull request head and base branches must differ".to_string());
    }
    let base_oid =
        branch_oid(&base)?.ok_or_else(|| format!("Base branch `{base}` does not exist"))?;
    let head_oid =
        branch_oid(&head)?.ok_or_else(|| format!("Head branch `{head}` does not exist"))?;
    if mock_state.prs.iter().any(|pr| {
        pr.state == "OPEN"
            && pr.head.ref_field == head
            && !mock_state.cross_repository_prs.contains(&pr.number)
    }) {
        return Err(format!("An open pull request already exists for head branch `{head}`"));
    }

    let number = mock_state.prs.iter().map(|pr| pr.number as u64).max().unwrap_or(0) + 1;
    let owner = mock_state.repo_owner.clone();
    let repo = mock_state.repo_name.clone();
    let mut entry = PrEntry::mock(MockPrArgs {
        id: number,
        title,
        body,
        head: head.clone(),
        base: base.clone(),
        repo_owner: &owner,
        repo_name: &repo,
    });
    entry.head.sha = head_oid.clone();
    entry.base.sha = base_oid.clone();
    let node_id = entry.node_id.clone();
    mock_state.prs.push(entry);

    let mut response = serde_json::Map::new();
    for field in selected_fields(&field.selection_set, PATH)? {
        match field.name.as_str() {
            "clientMutationId" => {
                response.insert(response_key(field), serde_json::json!(client_mutation_id));
            }
            "pullRequest" => {
                let mut pull_request = serde_json::Map::new();
                for field in selected_fields(&field.selection_set, "createPullRequest.pullRequest")?
                {
                    let value = match field.name.as_str() {
                        "number" => serde_json::json!(number),
                        "id" => serde_json::json!(node_id),
                        "state" => serde_json::json!("OPEN"),
                        "headRefName" => serde_json::json!(head),
                        "headRefOid" => serde_json::json!(head_oid),
                        "headRepository" | "baseRepository" => {
                            serde_json::json!({ "id": repository_id })
                        }
                        "baseRefName" => serde_json::json!(base),
                        "baseRefOid" => serde_json::json!(base_oid),
                        _ => unreachable!("request was checked by validate_create_field"),
                    };
                    pull_request.insert(response_key(field), value);
                }
                response.insert(response_key(field), serde_json::Value::Object(pull_request));
            }
            _ => unreachable!("request was checked by validate_create_field"),
        }
    }
    Ok(serde_json::Value::Object(response))
}

fn handle_repository_query(
    mock_state: &MockState,
    field: &executable::Field,
    variables: &GraphQlVariables,
) -> Result<serde_json::Value, String> {
    const PATH: &str = "repository";
    let owner = resolve_string_argument(field, "owner", PATH, variables)?;
    let name = resolve_string_argument(field, "name", PATH, variables)?;

    if owner != mock_state.repo_owner || name != mock_state.repo_name {
        return Ok(serde_json::Value::Null);
    }

    let mut repo_data = serde_json::Map::new();

    for field in selected_fields(&field.selection_set, PATH)? {
        match field.name.as_str() {
            "pullRequests" => {
                let head = resolve_string_argument(
                    field,
                    "headRefName",
                    "repository.pullRequests",
                    variables,
                )?;
                let states = argument(field, "states")
                    .and_then(|value| match value {
                        ast::Value::List(values) => Some(values),
                        _ => None,
                    })
                    .expect("request was checked by validate_pull_requests_field");
                let states = states
                    .iter()
                    .map(|value| match &**value {
                        ast::Value::Enum(value) => value.as_str(),
                        _ => unreachable!("request was checked by validate_pull_requests_field"),
                    })
                    .collect::<HashSet<_>>();
                let matching_prs = mock_state
                    .prs
                    .iter()
                    .filter(|pr| pr.head.ref_field == head && states.contains(pr.state.as_str()))
                    .collect::<Vec<_>>();
                let has_next_page = matching_prs.len() > MAX_PULL_REQUEST_CANDIDATES;

                let mut connection = serde_json::Map::new();
                for field in selected_fields(&field.selection_set, "repository.pullRequests")? {
                    match field.name.as_str() {
                        "nodes" => {
                            let nodes = matching_prs
                                .iter()
                                .take(MAX_PULL_REQUEST_CANDIDATES)
                                .map(|pr| {
                                    project_pr_node(
                                        pr,
                                        mock_state.cross_repository_prs.contains(&pr.number),
                                        &field.selection_set,
                                    )
                                })
                                .collect::<Result<Vec<_>, _>>()?;
                            connection.insert(response_key(field), serde_json::json!(nodes));
                        }
                        "pageInfo" => {
                            let mut page_info = serde_json::Map::new();
                            for field in selected_fields(
                                &field.selection_set,
                                "repository.pullRequests.pageInfo",
                            )? {
                                match field.name.as_str() {
                                    "hasNextPage" => {
                                        page_info.insert(
                                            response_key(field),
                                            serde_json::json!(has_next_page),
                                        );
                                    }
                                    _ => unreachable!(
                                        "request was checked by validate_pull_requests_field"
                                    ),
                                }
                            }
                            connection
                                .insert(response_key(field), serde_json::Value::Object(page_info));
                        }
                        _ => unreachable!("request was checked by validate_pull_requests_field"),
                    }
                }
                repo_data.insert(response_key(field), serde_json::Value::Object(connection));
            }
            "id" => {
                repo_data.insert(
                    response_key(field),
                    serde_json::Value::String("REPO_NODE_ID".to_string()),
                );
            }
            _ => unreachable!("request was checked by validate_repository_field"),
        }
    }

    Ok(serde_json::Value::Object(repo_data))
}

fn project_pr_node(
    pr: &PrEntry,
    is_cross_repository: bool,
    selection_set: &executable::SelectionSet,
) -> Result<serde_json::Value, String> {
    let mut node = serde_json::Map::new();
    for field in selected_fields(selection_set, "repository.pullRequests.nodes")? {
        let value = match field.name.as_str() {
            "number" => serde_json::json!(pr.number),
            "id" => serde_json::json!(pr.node_id),
            "title" => serde_json::json!(pr.title),
            "body" => serde_json::json!(pr.body),
            "baseRefName" => serde_json::json!(pr.base.ref_field),
            "state" => serde_json::json!(pr.state),
            "isCrossRepository" => serde_json::json!(is_cross_repository),
            _ => unreachable!("request was checked by validate_pull_requests_field"),
        };
        node.insert(response_key(field), value);
    }
    Ok(serde_json::Value::Object(node))
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

    fn apply_failure(
        state: &mut MockState,
        operations: &[GraphQlOperation],
    ) -> Option<FailureKind> {
        check_and_apply_graphql_failure(state, operations, false, 0)
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
             \"REPO_NODE_ID\", headRepositoryId: \"REPO_NODE_ID\", \
             baseRefName: \"main\", headRefName: \"Ghead\", \
             title: \"Title\", body: \"Body\", clientMutationId: \
             \"gherrit:create:Ghead\" }) { clientMutationId, pullRequest { \
             number, id, state, headRefName, headRefOid, headRepository { id }, \
             baseRefName, baseRefOid, baseRepository { id } } } }",
        );
        validate_supported_document(&create, &None).unwrap();

        let update = parse_document(
            "mutation { op0: updatePullRequest(input: { pullRequestId: \"PR_1\", \
             title: \"Updated\", clientMutationId: \"gherrit:update:PR_1\" }) { \
             clientMutationId, pullRequest { number, id } } }",
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
             headRepositoryId: \"REPO_NODE_ID\", baseRefName: \"main\", \
             headRefName: \"Gnew\", title: \"Title\", \
             clientMutationId: \"create\" }) { \
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

        let response =
            handle_create_pr(&mut state, root_field(&document), &existing_branch).unwrap();
        assert_eq!(response.pointer("/pullRequest/number"), Some(&serde_json::json!(8)));
        assert_eq!(state.prs.len(), 2);

        let error =
            handle_create_pr(&mut state, root_field(&document), &existing_branch).unwrap_err();
        assert!(error.contains("already exists"));
        assert_eq!(state.prs.len(), 2);

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
             title: \"First\", clientMutationId: \"first\" }) { \
             clientMutationId } second: createPullRequest(input: { \
             repositoryId: \"REPO_NODE_ID\", \
             headRepositoryId: \"REPO_NODE_ID\", baseRefName: \"main\", \
             headRefName: \"Gnew\", title: \"Second\", clientMutationId: \
             \"second\" }) { clientMutationId } }",
        );
        let operation = document.operations.iter().next().unwrap();
        let fields = selected_fields(&operation.selection_set, "operation").unwrap();
        let mut state = MockState::new("owner".to_string(), "repo".to_string());

        handle_create_pr(&mut state, fields[0], &existing_branch).unwrap();
        let error = handle_create_pr(&mut state, fields[1], &existing_branch).unwrap_err();

        assert!(error.contains("already exists"));
        assert_eq!(state.prs.len(), 1, "the acknowledged first field remains applied");
        assert_eq!(state.prs[0].title.as_deref(), Some("First"));
    }

    #[test]
    fn mutations_reject_unknown_repository_and_pull_request_ids() {
        let create = parse_document(
            "mutation { createPullRequest(input: { repositoryId: \"WRONG\", \
             headRepositoryId: \"WRONG\", baseRefName: \"main\", \
             headRefName: \"Ghead\", title: \"Title\", \
             clientMutationId: \"create\" }) { \
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
            handle_update_pr(&mut state, root_field(&update), &existing_branch).unwrap_err();
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
             baseRefName: \"Ghead\", clientMutationId: \"update\" }) { \
             clientMutationId } }",
        );
        let error =
            handle_update_pr(&mut state, root_field(&update), &existing_branch).unwrap_err();
        assert!(error.contains("head and base branches must differ"));
    }
}

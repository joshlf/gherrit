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
    git_interceptor, FailureKind, GraphQlOperation, MalformedJson, PullRequestState,
    RedirectStatus, RetryableHttpStatus, TestEnvironment,
};

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
    pub graphql_requests: Vec<Vec<GraphQlOperation>>,
    pub graphql_redirect_trap_requests: usize,
    pub max_graphql_query_operations_per_request: Option<usize>,
    pub repo_owner: String,
    pub repo_name: String,
    pub github_default_branch: Option<(String, String)>,
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

    let git_routes = git_interceptor::routes(
        state.clone(),
        remote_path.clone(),
        system_git.clone(),
        test_environment.clone(),
    );
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
) -> Option<FailureKind> {
    use FailureKind::*;

    let fail_action = mock_state.faults.front()?;
    let matches = match fail_action {
        GraphQl => true,
        QueryTransport | QueryHttp(_) => {
            !operations.is_empty()
                && operations.iter().all(|operation| *operation == GraphQlOperation::Query)
        }
        CreatePr | CreatePrMalformedJson(_) | CreatePrHttp(_) | CreatePrRedirect(_) => {
            operations.contains(&GraphQlOperation::CreatePr)
        }
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
                "convertPullRequestToDraft" => Some(GraphQlOperation::DraftPr),
                "closePullRequest" => Some(GraphQlOperation::ClosePr),
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
    string_fields: &[&str],
    boolean_fields: &[&str],
) -> Result<(), String> {
    let mut names = HashSet::new();
    for (name, value) in input {
        if !names.insert(name.as_str()) {
            return Err(format!(
                "The mock GitHub API does not support duplicate input field `{path}.input.{name}`"
            ));
        }
        let expected_type = if string_fields.contains(&name.as_str()) {
            "string"
        } else if boolean_fields.contains(&name.as_str()) {
            "boolean"
        } else {
            return Err(format!(
                "The mock GitHub API does not support input field `{path}.input.{name}`"
            ));
        };
        let has_expected_type = match expected_type {
            "string" => matches!(&**value, ast::Value::String(_)),
            "boolean" => matches!(&**value, ast::Value::Boolean(_)),
            _ => unreachable!("the expected input type is known"),
        };
        if !has_expected_type {
            return Err(format!(
                "The mock GitHub API requires an inline {expected_type} value at \
                 `{path}.input.{name}`"
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

fn validate_pull_request_states(field: &executable::Field) -> Result<(), String> {
    const PATH: &str = "repository.pullRequests";
    let states = argument(field, "states")
        .and_then(|value| match value {
            ast::Value::List(values) => Some(values),
            _ => None,
        })
        .ok_or_else(|| format!("The mock GitHub API requires `{PATH}(states: ...)`"))?;
    let states = states
        .iter()
        .map(|value| match &**value {
            ast::Value::Enum(value) => Some(value.as_str()),
            _ => None,
        })
        .collect::<Option<Vec<_>>>()
        .ok_or_else(|| {
            "The mock GitHub API requires an exact production pull request lifecycle query"
                .to_string()
        })?;
    if states != ["OPEN"] {
        return Err("The mock GitHub API requires states: [OPEN]".to_string());
    }
    Ok(())
}

fn validate_pull_requests_field(
    field: &executable::Field,
    variables: &GraphQlVariables,
) -> Result<(), String> {
    const PATH: &str = "repository.pullRequests";
    validate_argument_names(field, PATH, &["headRefName", "first", "after", "states"])?;

    let first = argument(field, "first")
        .and_then(|value| match value {
            ast::Value::Int(value) => Some(value.as_str()),
            _ => None,
        })
        .ok_or_else(|| format!("The mock GitHub API requires `{PATH}(first: 1)`"))?;
    if first != LOCAL_PULL_REQUEST_PAGE_SIZE.to_string() {
        return Err(format!(
            "The mock GitHub API requires `{PATH}(first: {LOCAL_PULL_REQUEST_PAGE_SIZE})`"
        ));
    }

    validate_pull_request_states(field)?;

    if field.alias.is_none() {
        return Err(format!(
            "The mock GitHub API requires every exact `{PATH}` connection to be aliased"
        ));
    }
    if resolve_string_argument(field, "headRefName", PATH, variables)?.is_empty() {
        return Err("The mock GitHub API requires a nonempty local headRefName".to_string());
    }
    if argument(field, "after").is_some()
        && resolve_string_argument(field, "after", PATH, variables)?.is_empty()
    {
        return Err("The mock GitHub API requires a nonempty pagination cursor".to_string());
    }

    for field in validate_exact_fields(&field.selection_set, PATH, &["nodes", "pageInfo"])? {
        match field.name.as_str() {
            "nodes" => {
                let fields = validate_exact_fields(
                    &field.selection_set,
                    "repository.pullRequests.nodes",
                    &[
                        "number",
                        "id",
                        "title",
                        "body",
                        "baseRefName",
                        "baseRefOid",
                        "headRefName",
                        "headRefOid",
                        "state",
                        "isCrossRepository",
                        "autoMergeRequest",
                        "isInMergeQueue",
                        "isDraft",
                    ],
                )?;
                let auto_merge = fields
                    .into_iter()
                    .find(|field| field.name == "autoMergeRequest")
                    .expect("the exact pull request field set contains autoMergeRequest");
                validate_exact_fields(
                    &auto_merge.selection_set,
                    "repository.pullRequests.nodes.autoMergeRequest",
                    &["enabledAt"],
                )?;
            }
            "pageInfo" => {
                validate_exact_fields(
                    &field.selection_set,
                    "repository.pullRequests.pageInfo",
                    &["hasNextPage", "endCursor"],
                )?;
            }
            _ => unreachable!("the exact connection field set was checked"),
        }
    }
    Ok(())
}

fn validate_default_branch_ref_field(field: &executable::Field) -> Result<(), String> {
    const PATH: &str = "repository.defaultBranchRef";
    for field in validate_exact_fields(&field.selection_set, PATH, &["name", "target"])? {
        match field.name.as_str() {
            "name" => {}
            "target" => validate_scalar_fields(
                &field.selection_set,
                "repository.defaultBranchRef.target",
                &["oid"],
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

    let fields = selected_fields(&field.selection_set, PATH)?;
    let pull_requests =
        fields.iter().filter(|field| field.name == "pullRequests").copied().collect::<Vec<_>>();
    if pull_requests.is_empty() {
        return Err("The mock GitHub API requires a production pull request query".to_string());
    }
    let repository_facts = fields
        .iter()
        .filter(|field| matches!(field.name.as_str(), "id" | "defaultBranchRef"))
        .copied()
        .collect::<Vec<_>>();
    match repository_facts.len() {
        0 => {}
        2 if ["id", "defaultBranchRef"].iter().all(|name| {
            repository_facts.iter().any(|field| field.name == *name && field.alias.is_none())
        }) => {}
        _ => {
            return Err("The mock GitHub API requires exactly one unaliased repository fact pair"
                .to_string());
        }
    }

    for field in fields {
        match field.name.as_str() {
            "id" => {}
            "defaultBranchRef" => validate_default_branch_ref_field(field)?,
            "pullRequests" => {
                validate_pull_requests_field(field, variables)?;
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
        &["draft"],
    )?;
    for required in [
        "repositoryId",
        "headRepositoryId",
        "baseRefName",
        "headRefName",
        "title",
        "body",
        "clientMutationId",
    ] {
        required_string_field(input, required, PATH)?;
    }
    if get_bool_field(input, "draft") != Some(true) {
        return Err(
            "The mock GitHub API requires `createPullRequest.input.draft: true`".to_string()
        );
    }

    for field in
        validate_exact_fields(&field.selection_set, PATH, &["clientMutationId", "pullRequest"])?
    {
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
                        "isDraft",
                        "headRefName",
                        "headRefOid",
                        "headRepository",
                        "baseRefName",
                        "baseRefOid",
                        "baseRepository",
                    ],
                )? {
                    match selected.name.as_str() {
                        "headRepository" | "baseRepository" => validate_exact_fields(
                            &selected.selection_set,
                            "createPullRequest.pullRequest.repository",
                            &["id"],
                        )
                        .map(|_| ())?,
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
        &[],
    )?;
    required_string_field(input, "pullRequestId", PATH)?;
    required_string_field(input, "clientMutationId", PATH)?;
    if !["baseRefName", "title", "body"].iter().any(|name| get_string_field(input, name).is_some())
    {
        return Err("The mock GitHub API requires at least one pull request update".to_string());
    }
    for field in
        validate_exact_fields(&field.selection_set, PATH, &["clientMutationId", "pullRequest"])?
    {
        match field.name.as_str() {
            "clientMutationId" => {}
            "pullRequest" => validate_exact_fields(
                &field.selection_set,
                "updatePullRequest.pullRequest",
                &["number", "id", "state"],
            )
            .and_then(|fields| {
                fields
                    .into_iter()
                    .find(|field| field.name == "state")
                    .map(|field| {
                        validate_scalar_fields(
                            &field.selection_set,
                            "updatePullRequest.pullRequest.state",
                            &[],
                        )
                    })
                    .unwrap_or(Ok(()))
            })?,
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

fn validate_close_field(field: &executable::Field) -> Result<(), String> {
    const PATH: &str = "closePullRequest";
    validate_argument_names(field, PATH, &["input"])?;
    let input = input_object(field, PATH)?;
    validate_input_fields(input, PATH, &["pullRequestId", "clientMutationId"], &[])?;
    required_string_field(input, "pullRequestId", PATH)?;
    required_string_field(input, "clientMutationId", PATH)?;

    for field in
        validate_exact_fields(&field.selection_set, PATH, &["clientMutationId", "pullRequest"])?
    {
        match field.name.as_str() {
            "clientMutationId" => {}
            "pullRequest" => {
                for field in validate_exact_fields(
                    &field.selection_set,
                    "closePullRequest.pullRequest",
                    &["number", "id", "state"],
                )? {
                    validate_scalar_fields(
                        &field.selection_set,
                        "closePullRequest.pullRequest.scalar",
                        &[],
                    )?;
                }
            }
            _ => unreachable!("the exact close field set was checked"),
        }
    }
    Ok(())
}

fn validate_draft_field(field: &executable::Field) -> Result<(), String> {
    const PATH: &str = "convertPullRequestToDraft";
    validate_argument_names(field, PATH, &["input"])?;
    let input = input_object(field, PATH)?;
    validate_input_fields(input, PATH, &["pullRequestId", "clientMutationId"], &[])?;
    required_string_field(input, "pullRequestId", PATH)?;
    required_string_field(input, "clientMutationId", PATH)?;
    for field in
        validate_exact_fields(&field.selection_set, PATH, &["clientMutationId", "pullRequest"])?
    {
        match field.name.as_str() {
            "clientMutationId" => {}
            "pullRequest" => {
                validate_exact_fields(
                    &field.selection_set,
                    "convertPullRequestToDraft.pullRequest",
                    &[
                        "number",
                        "id",
                        "state",
                        "isDraft",
                        "headRefName",
                        "headRefOid",
                        "baseRefName",
                        "baseRefOid",
                    ],
                )?;
            }
            _ => unreachable!("the exact draft field set was checked"),
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
    let fields = selected_fields(&operation.selection_set, "operation")?;
    let repository_count = fields.iter().filter(|field| field.name == "repository").count();
    if repository_count != 0 && (repository_count != 1 || fields.len() != 1) {
        return Err(
            "The mock GitHub API requires exactly one repository root field per query".to_string()
        );
    }
    let has_create = fields.iter().any(|field| field.name == "createPullRequest");
    let has_projection = fields
        .iter()
        .any(|field| matches!(field.name.as_str(), "closePullRequest" | "updatePullRequest"));
    if has_create && has_projection {
        return Err("The mock GitHub API does not mix pull request creation with final projection"
            .to_string());
    }
    for field in fields {
        match field.name.as_str() {
            "repository" => validate_repository_field(field, variables)?,
            "createPullRequest" => validate_create_field(field)?,
            "convertPullRequestToDraft" => validate_draft_field(field)?,
            "closePullRequest" => validate_close_field(field)?,
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

fn local_pull_request_connection_count(document: &ExecutableDocument) -> usize {
    document
        .operations
        .iter()
        .flat_map(|operation| &operation.selection_set.selections)
        .filter_map(|selection| {
            let executable::Selection::Field(field) = selection else { return None };
            (field.name == "repository").then_some(&field.selection_set.selections)
        })
        .flatten()
        .filter(|selection| {
            matches!(selection, executable::Selection::Field(field) if field.name == "pullRequests")
        })
        .count()
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
    if operations.iter().all(|operation| *operation == GraphQlOperation::Query)
        && mock_state
            .max_graphql_query_operations_per_request
            .is_some_and(|limit| local_pull_request_connection_count(&document) > limit)
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
    let failure = check_and_apply_graphql_failure(&mut mock_state, &operations);
    let malformed_json = match failure {
        Some(FailureKind::CreatePrMalformedJson(kind))
        | Some(FailureKind::UpdatePrMalformedJson(kind)) => Some(kind),
        _ => None,
    };
    let concurrent_close = matches!(failure, Some(FailureKind::UpdatePrConcurrentClose));
    if let Some(failure) = failure.filter(|_| malformed_json.is_none() && !concurrent_close) {
        if matches!(
            failure,
            FailureKind::CreatePrApplyThenDisconnect
                | FailureKind::ClosePrApplyThenDisconnect
                | FailureKind::UpdatePrApplyThenDisconnect
        ) {
            let branch_oids = match remote_branch_oids(&app_state) {
                Ok(branches) => branches,
                Err(message) => return graphql_http_error(&message),
            };
            let branch_oid = |branch: &str| Ok(branch_oids.get(branch).cloned());
            for operation in document.operations.iter() {
                for selection in &operation.selection_set.selections {
                    let executable::Selection::Field(field) = selection else { continue };
                    let result = match field.name.as_str() {
                        "createPullRequest" => {
                            handle_create_pr(&mut mock_state, field, &branch_oid)
                        }
                        "convertPullRequestToDraft" => {
                            handle_draft_pr(&mut mock_state, field, &branch_oid)
                        }
                        "closePullRequest" => handle_close_pr(&mut mock_state, field),
                        "updatePullRequest" => {
                            handle_update_pr(&mut mock_state, field, &branch_oid, false)
                        }
                        _ => unreachable!("request was checked by validate_supported_document"),
                    };
                    if let Err(message) = result {
                        return graphql_http_error(&message);
                    }
                }
            }
            return graphql_disconnect_response();
        }
        return graphql_failure_response(failure);
    }

    let mut response_data = serde_json::Map::new();

    let mut errors = Vec::new();
    let branch_oids = match remote_branch_oids(&app_state) {
        Ok(branches) => branches,
        Err(message) => return graphql_http_error(&message),
    };
    let branch_oid = |branch: &str| Ok(branch_oids.get(branch).cloned());

    for operation in document.operations.iter() {
        for selection in operation.selection_set.selections.iter() {
            if let executable::Selection::Field(field) = selection {
                let alias = response_key(field);

                let result = match field.name.as_str() {
                    "createPullRequest" => handle_create_pr(&mut mock_state, field, &branch_oid),
                    "convertPullRequestToDraft" => {
                        handle_draft_pr(&mut mock_state, field, &branch_oid)
                    }
                    "closePullRequest" => handle_close_pr(&mut mock_state, field),
                    "updatePullRequest" => {
                        handle_update_pr(&mut mock_state, field, &branch_oid, concurrent_close)
                    }
                    "repository" => handle_repository_query(
                        &mock_state,
                        field,
                        &variables,
                        &|| repository_default_branch(&app_state, &mock_state),
                        &branch_oid,
                    ),
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

    let response_json = serde_json::Value::Object(response_json);
    malformed_json
        .map(|kind| malformed_json_response(response_json.clone(), kind))
        .unwrap_or_else(|| graphql_response(StatusCode::OK, response_json))
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
        return graphql_disconnect_response();
    }

    let status = match failure {
        QueryHttp(status) | CreatePrHttp(status) => retryable_status(status),
        CreatePrRedirect(status) => redirect_status(status),
        GraphQl | CreatePr | ClosePr | UpdatePr => StatusCode::OK,
        QueryTransport => unreachable!("handled above"),
        CreatePrApplyThenDisconnect
        | ClosePrApplyThenDisconnect
        | UpdatePrApplyThenDisconnect
        | UpdatePrConcurrentClose => {
            unreachable!("handled before response generation")
        }
        CreatePrMalformedJson(_) | UpdatePrMalformedJson(_) => {
            unreachable!("handled after applying the mutation")
        }
        LosePublicationPushReceipt(_) | Git(_) => {
            unreachable!("Git publication failures are not handled by the GraphQL endpoint")
        }
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
            "data": null,
            "errors": [{ "message": message }],
        })),
    )
        .into_response()
}

fn graphql_disconnect_response() -> Response {
    let body = Body::from_stream(futures_util::stream::once(async {
        Err::<Bytes, _>(std::io::Error::other("Injected GraphQL response transport failure"))
    }));
    Response::new(body)
}

fn graphql_response(status: StatusCode, value: serde_json::Value) -> Response {
    (status, Json(value)).into_response()
}

fn malformed_json_response(value: serde_json::Value, kind: MalformedJson) -> Response {
    let body = match kind {
        MalformedJson::DuplicateObjectMember => {
            let data = serde_json::to_string(&value["data"]).unwrap();
            format!(r#"{{"data":{data},"data":{data}}}"#)
        }
        MalformedJson::TrailingValue => format!("{} null", serde_json::to_string(&value).unwrap()),
    };
    Response::builder()
        .status(StatusCode::OK)
        .header(CONTENT_TYPE, "application/json")
        .body(Body::from(body))
        .unwrap()
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

fn remote_branch_oids(app_state: &AppState) -> Result<HashMap<String, String>, String> {
    let output = app_state
        .test_environment
        .command(&app_state.system_git)
        .arg("--git-dir")
        .arg(&app_state.remote_path)
        .args(["for-each-ref", "--format=%(refname)%00%(objectname)", "refs/heads"])
        .output()
        .map_err(|error| format!("Failed to inspect remote Git branches: {error}"))?;
    if !output.status.success() {
        return Err(format!(
            "Inspecting remote Git branches exited with {:?}",
            output.status.code()
        ));
    }

    let mut branches = HashMap::new();
    for record in output.stdout.split(|byte| *byte == b'\n').filter(|record| !record.is_empty()) {
        let record = record.strip_suffix(b"\r").unwrap_or(record);
        let Some(separator) = record.iter().position(|byte| *byte == 0) else {
            return Err("Remote Git branch observation is missing its object ID".to_owned());
        };
        let reference = std::str::from_utf8(&record[..separator])
            .map_err(|_| "Remote Git branch name is not UTF-8".to_owned())?;
        let branch = reference
            .strip_prefix("refs/heads/")
            .ok_or_else(|| format!("Remote Git reported non-branch ref `{reference}`"))?;
        let oid = std::str::from_utf8(&record[separator + 1..])
            .map_err(|_| format!("Remote Git branch `{branch}` has a non-UTF-8 object ID"))?;
        if oid.len() != 40 || !oid.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            return Err(format!("Remote Git branch `{branch}` has an invalid object ID"));
        }
        if branches.insert(branch.to_owned(), oid.to_owned()).is_some() {
            return Err(format!("Remote Git reported branch `{branch}` more than once"));
        }
    }
    Ok(branches)
}

fn repository_default_branch(
    app_state: &AppState,
    mock_state: &MockState,
) -> Result<(String, String), String> {
    if let Some(default_branch) = &mock_state.github_default_branch {
        return Ok(default_branch.clone());
    }

    let run = |arguments: &[&str]| {
        app_state
            .test_environment
            .command(&app_state.system_git)
            .arg("--git-dir")
            .arg(&app_state.remote_path)
            .args(arguments)
            .output()
            .map_err(|error| format!("Failed to inspect remote default branch: {error}"))
    };
    let symbolic = run(&["symbolic-ref", "HEAD"])?;
    let tip = run(&["rev-parse", "HEAD"])?;
    if !symbolic.status.success() || !tip.status.success() {
        return Err("The mock repository has no default branch".to_owned());
    }
    let symbolic = std::str::from_utf8(&symbolic.stdout)
        .map_err(|_| "The mock repository default branch is not UTF-8".to_owned())?
        .trim_end_matches(['\r', '\n']);
    let name = symbolic
        .strip_prefix("refs/heads/")
        .ok_or_else(|| "The mock repository HEAD is not a local branch".to_owned())?;
    let tip = std::str::from_utf8(&tip.stdout)
        .map_err(|_| "The mock repository default branch tip is not UTF-8".to_owned())?
        .trim_end_matches(['\r', '\n']);
    Ok((name.to_owned(), tip.to_owned()))
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

fn get_bool_field(obj: &[(Name, Node<ast::Value>)], key: &str) -> Option<bool> {
    obj.iter().find(|(name, _)| name == key).and_then(|(_, value)| match &**value {
        ast::Value::Boolean(value) => Some(*value),
        _ => None,
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

fn handle_close_pr(
    mock_state: &mut MockState,
    field: &executable::Field,
) -> Result<serde_json::Value, String> {
    const PATH: &str = "closePullRequest";
    let input = input_object(field, PATH)?;
    let node_id = required_string_field(input, "pullRequestId", PATH)?;
    let client_mutation_id = required_string_field(input, "clientMutationId", PATH)?;

    let Some(pr) = mock_state.prs.iter_mut().find(|pr| pr.node_id == node_id) else {
        return Err(format!("Pull request node `{node_id}` does not exist"));
    };
    if pr.state != PullRequestState::Open {
        return Err(format!("Pull request node `{node_id}` is not OPEN and cannot be closed"));
    }
    pr.state = PullRequestState::Closed;

    let mut response = serde_json::Map::new();
    for field in selected_fields(&field.selection_set, PATH)? {
        match field.name.as_str() {
            "clientMutationId" => {
                response.insert(response_key(field), serde_json::json!(client_mutation_id));
            }
            "pullRequest" => {
                let mut pull_request = serde_json::Map::new();
                for field in selected_fields(&field.selection_set, "closePullRequest.pullRequest")?
                {
                    let value = match field.name.as_str() {
                        "number" => serde_json::json!(pr.number),
                        "id" => serde_json::json!(pr.node_id),
                        "state" => serde_json::json!(pr.state),
                        _ => unreachable!("request was checked by validate_close_field"),
                    };
                    pull_request.insert(response_key(field), value);
                }
                response.insert(response_key(field), serde_json::Value::Object(pull_request));
            }
            _ => unreachable!("request was checked by validate_close_field"),
        }
    }
    Ok(serde_json::Value::Object(response))
}

fn handle_draft_pr(
    mock_state: &mut MockState,
    field: &executable::Field,
    branch_oid: &dyn Fn(&str) -> Result<Option<String>, String>,
) -> Result<serde_json::Value, String> {
    const PATH: &str = "convertPullRequestToDraft";
    let input = input_object(field, PATH)?;
    let node_id = required_string_field(input, "pullRequestId", PATH)?;
    let client_mutation_id = required_string_field(input, "clientMutationId", PATH)?;
    let Some(pr) = mock_state.prs.iter_mut().find(|pr| pr.node_id == node_id) else {
        return Err(format!("Pull request node `{node_id}` does not exist"));
    };
    if pr.state != PullRequestState::Open {
        return Err(format!("Pull request node `{node_id}` is not OPEN and cannot become a draft"));
    }
    let head_oid = branch_oid(&pr.head.name)?.unwrap_or_else(|| pr.head.oid.clone());
    let base_oid = branch_oid(&pr.base.name)?.unwrap_or_else(|| pr.base.oid.clone());
    pr.is_draft = true;

    let mut response = serde_json::Map::new();
    for field in selected_fields(&field.selection_set, PATH)? {
        match field.name.as_str() {
            "clientMutationId" => {
                response.insert(response_key(field), serde_json::json!(client_mutation_id));
            }
            "pullRequest" => {
                let mut pull_request = serde_json::Map::new();
                for field in
                    selected_fields(&field.selection_set, "convertPullRequestToDraft.pullRequest")?
                {
                    let value = match field.name.as_str() {
                        "number" => serde_json::json!(pr.number),
                        "id" => serde_json::json!(pr.node_id),
                        "state" => serde_json::json!(pr.state),
                        "isDraft" => serde_json::json!(pr.is_draft),
                        "headRefName" => serde_json::json!(pr.head.name),
                        "headRefOid" => serde_json::json!(head_oid),
                        "baseRefName" => serde_json::json!(pr.base.name),
                        "baseRefOid" => serde_json::json!(base_oid),
                        _ => unreachable!("request was checked by validate_draft_field"),
                    };
                    pull_request.insert(response_key(field), value);
                }
                response.insert(response_key(field), serde_json::Value::Object(pull_request));
            }
            _ => unreachable!("request was checked by validate_draft_field"),
        }
    }
    Ok(serde_json::Value::Object(response))
}

fn handle_update_pr(
    mock_state: &mut MockState,
    field: &executable::Field,
    branch_oid: &dyn Fn(&str) -> Result<Option<String>, String>,
    concurrent_close: bool,
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
    if base.as_ref().map(|(name, _)| name.as_str()) == Some(pr.head.name.as_str()) {
        return Err("Pull request head and base branches must differ".to_string());
    }
    if let Some(title) = title {
        pr.title = title;
    }
    if let Some(body) = &body {
        pr.body = body.clone();
    }
    if let Some((name, oid)) = base {
        pr.base = BranchState { name, oid };
    }
    if concurrent_close {
        pr.state = PullRequestState::Closed;
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
                        "state" => {
                            pull_request.insert(response_key(field), serde_json::json!(pr.state));
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
    let body = required_string_field(input, "body", PATH)?;
    if get_bool_field(input, "draft") != Some(true) {
        return Err("The mock GitHub API requires draft pull request creation".to_string());
    }
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
    let number = mock_state.prs.iter().map(|pr| pr.number).max().unwrap_or(0) + 1;
    let mut entry =
        PrEntry::mock(MockPrArgs { number, title, body, head: head.clone(), base: base.clone() });
    entry.is_draft = true;
    entry.head.oid = head_oid.clone();
    entry.base.oid = base_oid.clone();
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
                        "isDraft" => serde_json::json!(true),
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
    default_branch: &dyn Fn() -> Result<(String, String), String>,
    branch_oid: &dyn Fn(&str) -> Result<Option<String>, String>,
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
            "defaultBranchRef" => {
                let (name, oid) = default_branch()?;
                let mut branch = serde_json::Map::new();
                for field in selected_fields(&field.selection_set, "repository.defaultBranchRef")? {
                    let value = match field.name.as_str() {
                        "name" => serde_json::json!(name),
                        "target" => {
                            let mut target = serde_json::Map::new();
                            for field in selected_fields(
                                &field.selection_set,
                                "repository.defaultBranchRef.target",
                            )? {
                                match field.name.as_str() {
                                    "oid" => {
                                        target.insert(response_key(field), serde_json::json!(oid));
                                    }
                                    _ => unreachable!(
                                        "request was checked by validate_default_branch_ref_field"
                                    ),
                                }
                            }
                            serde_json::Value::Object(target)
                        }
                        _ => {
                            unreachable!("request was checked by validate_default_branch_ref_field")
                        }
                    };
                    branch.insert(response_key(field), value);
                }
                repo_data.insert(response_key(field), serde_json::Value::Object(branch));
            }
            "pullRequests" => {
                let head = resolve_string_argument(
                    field,
                    "headRefName",
                    "repository.pullRequests",
                    variables,
                )?;
                let after = argument(field, "after")
                    .map(|_| {
                        resolve_string_argument(
                            field,
                            "after",
                            "repository.pullRequests",
                            variables,
                        )
                    })
                    .transpose()?;
                let matching_prs = mock_state
                    .prs
                    .iter()
                    .filter(|pr| pr.head.name == head && pr.state == PullRequestState::Open)
                    .collect::<Vec<_>>();
                let offset = after
                    .as_deref()
                    .map(|raw_cursor| {
                        let (cursor_head, offset) = raw_cursor
                            .strip_prefix("cursor:")
                            .and_then(|cursor| cursor.rsplit_once(':'))
                            .ok_or_else(|| format!("Invalid pull request cursor `{raw_cursor}`"))?;
                        if cursor_head != head {
                            return Err(format!(
                                "Pull request cursor `{raw_cursor}` belongs to another local head"
                            ));
                        }
                        offset
                            .parse::<usize>()
                            .map_err(|_| format!("Invalid pull request cursor `{raw_cursor}`"))
                    })
                    .transpose()?
                    .unwrap_or(0);
                let page = matching_prs
                    .iter()
                    .skip(offset)
                    .take(LOCAL_PULL_REQUEST_PAGE_SIZE)
                    .collect::<Vec<_>>();
                let has_next_page = matching_prs.len() > offset + page.len();

                let mut connection = serde_json::Map::new();
                for field in selected_fields(&field.selection_set, "repository.pullRequests")? {
                    match field.name.as_str() {
                        "nodes" => {
                            let nodes = page
                                .iter()
                                .map(|pr| {
                                    let is_cross_repository =
                                        mock_state.cross_repository_prs.contains(&pr.number);
                                    let (head_oid, base_oid) = if is_cross_repository {
                                        (pr.head.oid.clone(), pr.base.oid.clone())
                                    } else {
                                        (
                                            branch_oid(&pr.head.name)?
                                                .unwrap_or_else(|| pr.head.oid.clone()),
                                            branch_oid(&pr.base.name)?
                                                .unwrap_or_else(|| pr.base.oid.clone()),
                                        )
                                    };
                                    project_pr_node(
                                        pr,
                                        is_cross_repository,
                                        &head_oid,
                                        &base_oid,
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
                                    "endCursor" => {
                                        let cursor = has_next_page.then(|| {
                                            format!("cursor:{head}:{}", offset + page.len())
                                        });
                                        page_info
                                            .insert(response_key(field), serde_json::json!(cursor));
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
    head_oid: &str,
    base_oid: &str,
    selection_set: &executable::SelectionSet,
) -> Result<serde_json::Value, String> {
    let mut node = serde_json::Map::new();
    for field in selected_fields(selection_set, "repository.pullRequests.nodes")? {
        let value = match field.name.as_str() {
            "number" => serde_json::json!(pr.number),
            "id" => serde_json::json!(pr.node_id),
            "title" => serde_json::json!(pr.title),
            "body" => serde_json::json!(pr.body),
            "baseRefName" => serde_json::json!(pr.base.name),
            "baseRefOid" => serde_json::json!(base_oid),
            "headRefName" => serde_json::json!(pr.head.name),
            "headRefOid" => serde_json::json!(head_oid),
            "state" => serde_json::json!(pr.state),
            "isCrossRepository" => serde_json::json!(is_cross_repository),
            "autoMergeRequest" => {
                if pr.auto_merge {
                    serde_json::json!({ "enabledAt": "2026-01-01T00:00:00Z" })
                } else {
                    serde_json::Value::Null
                }
            }
            "isInMergeQueue" => serde_json::json!(pr.in_merge_queue),
            "isDraft" => serde_json::json!(pr.is_draft),
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
             number, id, state, isDraft, headRefName, headRefOid, headRepository { id }, \
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

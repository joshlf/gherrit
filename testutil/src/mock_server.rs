use std::{
    collections::{HashMap, HashSet},
    future::IntoFuture,
    path::PathBuf,
    sync::{mpsc::Sender, Arc, LazyLock, RwLock},
};

use apollo_compiler::{ast, executable, validation::Valid, ExecutableDocument, Name, Node};
use axum::{extract::State, http::StatusCode, routing::post, Json, Router};
use serde::{Deserialize, Serialize};
use tokio::net::TcpListener;

use crate::{FailureKind, TestEnvironment};

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
    pub pushes: Vec<GitPush>,
    pub graphql_requests: Vec<Vec<GraphQlOperation>>,
    pub max_graphql_operations_per_request: Option<usize>,
    pub repo_owner: String,
    pub repo_name: String,
    pub fail_next_request: Option<FailureKind>,
    pub merge_queue: HashSet<u64>,
    pub auto_merge: HashSet<u64>,
    pub native_stacks: HashSet<u64>,
    pub base_updates: Vec<BaseUpdate>,
    pub merge_queue_after_base_update: Option<u64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GraphQlOperation {
    Query,
    CreatePr,
    UpdatePr,
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
    #[serde(skip)]
    pub head_repository: Option<MockRepositoryIdentity>,
    #[serde(skip)]
    pub base_repository: Option<MockRepositoryIdentity>,
    #[serde(skip)]
    pub is_cross_repository: bool,
    pub created_at: String,
    pub updated_at: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MockRepositoryIdentity {
    pub id: String,
    pub name_with_owner: String,
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
            head_repository: Some(MockRepositoryIdentity {
                id: "REPO_NODE_ID".to_string(),
                name_with_owner: format!("{repo_owner}/{repo_name}"),
            }),
            base_repository: Some(MockRepositoryIdentity {
                id: "REPO_NODE_ID".to_string(),
                name_with_owner: format!("{repo_owner}/{repo_name}"),
            }),
            is_cross_repository: false,
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

#[derive(Serialize, Deserialize, Debug)]
pub struct GitRequest {
    pub args: Vec<String>,
    pub cwd: String,
    pub env: HashMap<String, String>,
}

#[derive(Serialize, Deserialize, Debug)]
pub struct GitResponse {
    pub stdout: String,
    pub stderr: String,
    pub exit_code: i32,
    pub passthrough: bool,
    pub report_exit_status: bool,
    pub override_exit_code: Option<i32>,
}

#[derive(Serialize, Deserialize, Debug)]
pub struct GitCompletion {
    pub args: Vec<String>,
    pub exit_code: i32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GitPush {
    pub args: Vec<String>,
    pub refspecs: Vec<String>,
    pub exit_code: i32,
}

impl GitPush {
    pub fn succeeded(&self) -> bool {
        self.exit_code == 0
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BaseUpdate {
    pub pr_id: u64,
    pub old_base: String,
    pub new_base: String,
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

    let app_state = AppState { state, remote_path, system_git, test_environment };

    let app = Router::new()
        .route("/graphql", post(graphql))
        .route("/_internal/git", post(handle_git))
        .route("/_internal/git/complete", post(complete_git))
        .with_state(app_state);

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

    let fail_action = mock_state.fail_next_request.as_ref()?;
    let matches = match fail_action {
        GraphQl => true,
        GraphQlAfterApply | UpdatePrAfterApply => false,
        CreatePr => operations.contains(&GraphQlOperation::CreatePr),
        UpdatePr => operations.contains(&GraphQlOperation::UpdatePr),
    };

    if !matches {
        return None;
    }

    mock_state.fail_next_request.take()
}

fn check_and_apply_graphql_after_failure(
    mock_state: &mut MockState,
    operations: &[GraphQlOperation],
) -> Option<FailureKind> {
    use FailureKind::*;

    let fail_action = mock_state.fail_next_request.as_ref()?;
    let matches = match fail_action {
        GraphQlAfterApply => true,
        UpdatePrAfterApply => operations.contains(&GraphQlOperation::UpdatePr),
        GraphQl | CreatePr | UpdatePr => false,
    };
    matches.then(|| mock_state.fail_next_request.take().unwrap())
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct GitCommand<'a> {
    name: &'a str,
    args: &'a [String],
}

/// Parses the command portion of a Git invocation.
///
/// Git accepts global options between the executable and subcommand. Keep this
/// list in sync with the public options documented by `git(1)`, and fail
/// closed for unknown or incomplete options rather than mistaking one of their
/// arguments for a subcommand.
fn parse_git_command(args: &[String]) -> Option<GitCommand<'_>> {
    let mut remaining = args.get(1..)?;

    loop {
        let (arg, rest) = remaining.split_first()?;
        match arg.as_str() {
            // These options consume the following argument.
            "-C" | "-c" | "--git-dir" | "--work-tree" | "--namespace" | "--config-env"
            | "--attr-source" => {
                remaining = rest.get(1..)?;
            }

            // These options affect how Git dispatches the eventual command.
            "-p"
            | "--paginate"
            | "-P"
            | "--no-pager"
            | "--bare"
            | "--no-replace-objects"
            | "--no-lazy-fetch"
            | "--no-optional-locks"
            | "--no-advice"
            | "--literal-pathspecs"
            | "--glob-pathspecs"
            | "--noglob-pathspecs"
            | "--icase-pathspecs" => {
                remaining = rest;
            }

            // These forms carry their value in the same argument.
            _ if [
                "--exec-path=",
                "--git-dir=",
                "--work-tree=",
                "--namespace=",
                "--config-env=",
                "--attr-source=",
            ]
            .iter()
            .any(|prefix| arg.starts_with(prefix)) =>
            {
                remaining = rest;
            }

            // These are complete Git requests, not options preceding a
            // subcommand. Any following argument is data for the request.
            "-v" | "--version" | "-h" | "--help" | "--exec-path" | "--html-path" | "--man-path"
            | "--info-path" => return None,
            _ if arg.starts_with("--list-cmds=") => return None,

            // Git does not support `--` before its subcommand. Unknown global
            // options are likewise not safe to parse through.
            _ if arg.starts_with('-') => return None,
            name => return Some(GitCommand { name, args: rest }),
        }
    }
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

fn resolve_optional_string_argument(
    field: &executable::Field,
    name: &str,
    path: &str,
    variables: &GraphQlVariables,
) -> Result<Option<String>, String> {
    argument(field, name).map(|_| resolve_string_argument(field, name, path, variables)).transpose()
}

fn enum_list_argument(
    field: &executable::Field,
    name: &str,
    path: &str,
) -> Result<HashSet<String>, String> {
    let values = argument(field, name)
        .and_then(|value| match value {
            ast::Value::List(values) => Some(values),
            _ => None,
        })
        .ok_or_else(|| format!("The mock GitHub API requires `{path}({name}: [...])`"))?;
    values
        .iter()
        .map(|value| match &**value {
            ast::Value::Enum(value) => Ok(value.to_string()),
            _ => {
                Err(format!("The mock GitHub API requires enum values at `{path}({name}: [...])`"))
            }
        })
        .collect()
}

fn validate_pull_requests_field(field: &executable::Field) -> Result<(), String> {
    const PATH: &str = "repository.pullRequests";
    validate_argument_names(field, PATH, &["headRefName", "baseRefName", "first", "states"])?;

    let has_head = argument(field, "headRefName").is_some();
    let has_base = argument(field, "baseRefName").is_some();
    if has_head == has_base {
        return Err(format!(
            "The mock GitHub API requires exactly one of `{PATH}(headRefName:)` or `{PATH}(baseRefName:)`"
        ));
    }

    let first = argument(field, "first")
        .and_then(|value| match value {
            ast::Value::Int(value) => Some(value.as_str()),
            _ => None,
        })
        .ok_or_else(|| format!("The mock GitHub API requires `{PATH}(first: 100)`"))?;
    if first != "100" {
        return Err(format!("The mock GitHub API only supports `{PATH}(first: 100)`"));
    }

    let state_count = argument(field, "states")
        .and_then(|value| match value {
            ast::Value::List(values) => Some(values.len()),
            _ => None,
        })
        .expect("enum_list_argument already validated the states list");
    let states = enum_list_argument(field, "states", PATH)?;
    let all_states =
        HashSet::from(["OPEN".to_string(), "CLOSED".to_string(), "MERGED".to_string()]);
    let open_only = HashSet::from(["OPEN".to_string()]);
    if state_count != states.len() || (states != all_states && states != open_only) {
        return Err(format!(
            "The mock GitHub API only supports `{PATH}(states: [OPEN])` or `{PATH}(states: [OPEN, CLOSED, MERGED])`"
        ));
    }

    for field in selected_fields(&field.selection_set, PATH)? {
        match field.name.as_str() {
            "totalCount" => {}
            "nodes" => validate_pr_node_fields(&field.selection_set)?,
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

fn validate_pr_node_fields(selection_set: &executable::SelectionSet) -> Result<(), String> {
    const PATH: &str = "repository.pullRequests.nodes";
    for field in selected_fields(selection_set, PATH)? {
        match field.name.as_str() {
            "number" | "id" | "title" | "body" | "baseRefName" | "baseRefOid" | "headRefName"
            | "headRefOid" | "state" | "isInMergeQueue" | "isCrossRepository" => {}
            "headRepository" | "baseRepository" => {
                validate_scalar_fields(
                    &field.selection_set,
                    if field.name == "headRepository" {
                        "PullRequest.headRepository"
                    } else {
                        "PullRequest.baseRepository"
                    },
                    &["id", "nameWithOwner"],
                )?;
            }
            "autoMergeRequest" => {
                validate_scalar_fields(
                    &field.selection_set,
                    "PullRequest.autoMergeRequest",
                    &["enabledAt"],
                )?;
            }
            "stackEntry" => {
                validate_scalar_fields(&field.selection_set, "PullRequest.stackEntry", &["id"])?;
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
            "id" | "nameWithOwner" => {}
            "pullRequests" => {
                validate_pull_requests_field(field)?;
                if argument(field, "headRefName").is_some() {
                    resolve_string_argument(
                        field,
                        "headRefName",
                        "repository.pullRequests",
                        variables,
                    )?;
                } else {
                    resolve_string_argument(
                        field,
                        "baseRefName",
                        "repository.pullRequests",
                        variables,
                    )?;
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

fn validate_create_field(field: &executable::Field) -> Result<(), String> {
    const PATH: &str = "createPullRequest";
    validate_argument_names(field, PATH, &["input"])?;
    let input = input_object(field, PATH)?;
    validate_input_fields(
        input,
        PATH,
        &["repositoryId", "baseRefName", "headRefName", "title", "body"],
    )?;
    for required in ["repositoryId", "baseRefName", "headRefName", "title"] {
        required_string_field(input, required, PATH)?;
    }

    for field in selected_fields(&field.selection_set, PATH)? {
        match field.name.as_str() {
            "clientMutationId" => {}
            "pullRequest" => validate_scalar_fields(
                &field.selection_set,
                "createPullRequest.pullRequest",
                &["number", "url", "id"],
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

fn validate_update_field(field: &executable::Field) -> Result<(), String> {
    const PATH: &str = "updatePullRequest";
    validate_argument_names(field, PATH, &["input"])?;
    let input = input_object(field, PATH)?;
    validate_input_fields(input, PATH, &["pullRequestId", "baseRefName", "title", "body"])?;
    required_string_field(input, "pullRequestId", PATH)?;
    if !["baseRefName", "title", "body"].iter().any(|name| get_string_field(input, name).is_some())
    {
        return Err("The mock GitHub API requires at least one pull request update".to_string());
    }
    validate_scalar_fields(&field.selection_set, PATH, &["clientMutationId"])
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

async fn handle_git(
    State(app_state): State<AppState>,
    Json(req): Json<GitRequest>,
) -> Json<GitResponse> {
    let command = parse_git_command(&req.args);
    let is_push = command.is_some_and(|command| command.name == "push");

    // Check for simulated failure
    if let Some(command) = command {
        if req
            .env
            .get("MOCK_BIN_FAIL_CMD")
            .is_some_and(|fail_cmd| fail_cmd == &format!("git:{}", command.name))
        {
            if is_push {
                record_push(&app_state, req.args.clone(), 1);
            }
            return Json(GitResponse {
                stdout: "".to_string(),
                stderr: format!("Simulated failure for git {}", command.name),
                exit_code: 1,
                passthrough: false,
                report_exit_status: false,
                override_exit_code: None,
            });
        }
    }

    // Spy on "push" logic
    if is_push {
        let state = app_state.state.read().unwrap();
        let repo_owner = state.repo_owner.clone();
        let repo_name = state.repo_name.clone();

        // We want to verify the output in tests, so we print the expected GitHub msg
        let stderr = format!(
            "remote: \nremote: Create a pull request for 'feature' on GitHub by visiting:\nremote:      https://github.com/{}/{}/pull/new/feature\nremote: \n",
            repo_owner, repo_name
        );

        // For now, we still want to passthrough to real git to actually move refs in the local repo
        let override_exit_code = req
            .env
            .get("MOCK_BIN_FAIL_AFTER_CMD")
            .is_some_and(|fail_cmd| fail_cmd == "git:push")
            .then_some(1);
        return Json(GitResponse {
            stdout: "".to_string(),
            stderr,
            exit_code: 0,
            passthrough: true,
            report_exit_status: true,
            override_exit_code,
        });
    }

    // Default: strict passthrough
    Json(GitResponse {
        stdout: "".to_string(),
        stderr: "".to_string(),
        exit_code: 0,
        passthrough: true,
        report_exit_status: false,
        override_exit_code: None,
    })
}

async fn complete_git(
    State(app_state): State<AppState>,
    Json(completion): Json<GitCompletion>,
) -> StatusCode {
    if parse_git_command(&completion.args).is_none_or(|command| command.name != "push") {
        return StatusCode::BAD_REQUEST;
    }

    let exit_code = completion.exit_code;
    record_push(&app_state, completion.args, exit_code);
    if exit_code == 0 {
        if let Err(message) = apply_indirect_merges(&app_state) {
            eprintln!("mock GitHub indirect-merge evaluation failed: {message}");
            return StatusCode::INTERNAL_SERVER_ERROR;
        }
    }
    StatusCode::NO_CONTENT
}

fn record_push(app_state: &AppState, args: Vec<String>, exit_code: i32) {
    let command = parse_git_command(&args)
        .filter(|command| command.name == "push")
        .expect("record_push requires a parsed Git push");
    let refspecs = command
        .args
        .iter()
        .filter(|arg| !arg.starts_with('-'))
        .filter(|arg| {
            let refspec = arg.trim_start_matches('+');
            refspec.starts_with("refs/")
                || refspec
                    .split_once(':')
                    .is_some_and(|(_, destination)| destination.starts_with("refs/"))
        })
        .cloned()
        .collect();
    app_state.state.write().unwrap().pushes.push(GitPush { args, refspecs, exit_code });
}

fn apply_indirect_merges(app_state: &AppState) -> Result<(), String> {
    let candidates = {
        let state = app_state.state.read().unwrap();
        state
            .prs
            .iter()
            .filter(|pr| pr.state == "OPEN")
            .map(|pr| (pr.id, pr.head.ref_field.clone(), pr.base.ref_field.clone()))
            .collect::<Vec<_>>()
    };

    let mut merged = HashSet::new();
    for (id, head, base) in candidates {
        let Some(head_oid) = remote_branch_oid(app_state, &head)? else {
            continue;
        };
        let Some(base_oid) = remote_branch_oid(app_state, &base)? else {
            continue;
        };
        let output = app_state
            .test_environment
            .command(&app_state.system_git)
            .arg("--git-dir")
            .arg(&app_state.remote_path)
            .args(["merge-base", "--is-ancestor", &head_oid, &base_oid])
            .output()
            .map_err(|error| {
                format!(
                    "Failed to evaluate whether PR head {head}@{head_oid} is reachable from {base}@{base_oid}: {error}"
                )
            })?;
        match output.status.code() {
            Some(0) => {
                merged.insert(id);
            }
            Some(1) => {}
            code => {
                return Err(format!("Reachability check for PR {id} exited with {code:?}"));
            }
        }
    }

    if !merged.is_empty() {
        let mut state = app_state.state.write().unwrap();
        for pr in &mut state.prs {
            if pr.state == "OPEN" && merged.contains(&pr.id) {
                pr.state = "MERGED".to_string();
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod git_tests {
    use super::*;

    fn args(values: &[&str]) -> Vec<String> {
        values.iter().map(|value| (*value).to_string()).collect()
    }

    fn app_state() -> AppState {
        AppState {
            state: Arc::new(RwLock::new(MockState::new("owner".to_string(), "repo".to_string()))),
            remote_path: PathBuf::new(),
            system_git: PathBuf::from("git"),
            test_environment: TestEnvironment { variables: Vec::new() },
        }
    }

    #[test]
    fn parses_subcommand_after_documented_git_global_options() {
        let invocation = args(&[
            "git",
            "-C",
            "repo",
            "-c",
            "color.ui=false",
            "--git-dir=.git",
            "--work-tree",
            ".",
            "--namespace=tests",
            "--config-env",
            "user.name=TEST_USER",
            "--exec-path=/git-core",
            "--no-pager",
            "--literal-pathspecs",
            "--attr-source=HEAD",
            "push",
            "origin",
            "HEAD:refs/heads/main",
        ]);

        let command = parse_git_command(&invocation).unwrap();
        assert_eq!(command.name, "push");
        assert_eq!(command.args, args(&["origin", "HEAD:refs/heads/main"]));
    }

    #[test]
    fn does_not_find_push_outside_the_subcommand_position() {
        for invocation in [
            args(&["git", "show", "push"]),
            args(&["git", "--help", "push"]),
            args(&["git", "--list-cmds=main", "push"]),
            args(&["git", "--unknown", "push"]),
            args(&["git", "-c", "push"]),
        ] {
            assert_ne!(parse_git_command(&invocation).map(|command| command.name), Some("push"));
        }
    }

    #[tokio::test]
    async fn records_completed_push_after_git_global_options() {
        let app_state = app_state();
        let invocation = args(&[
            "git",
            "-c",
            "remote.origin.push=refs/fake:refs/heads/fake",
            "push",
            "origin",
            "+HEAD:refs/heads/main",
        ]);
        let request =
            GitRequest { args: invocation.clone(), cwd: "/repo".to_string(), env: HashMap::new() };

        let Json(response) = handle_git(State(app_state.clone()), Json(request)).await;
        assert!(response.passthrough);
        assert!(response.report_exit_status);
        assert!(app_state.state.read().unwrap().pushes.is_empty());

        let status = complete_git(
            State(app_state.clone()),
            Json(GitCompletion { args: invocation.clone(), exit_code: 0 }),
        )
        .await;
        assert_eq!(status, StatusCode::NO_CONTENT);

        let state = app_state.state.read().unwrap();
        assert_eq!(
            state.pushes,
            [GitPush {
                args: invocation,
                refspecs: vec!["+HEAD:refs/heads/main".to_string()],
                exit_code: 0,
            }]
        );
    }

    #[tokio::test]
    async fn failure_injection_uses_parsed_git_subcommand() {
        let app_state = app_state();
        let invocation = args(&["git", "-c", "color.ui=false", "push", "origin", "main"]);
        let request = GitRequest {
            args: invocation.clone(),
            cwd: "/repo".to_string(),
            env: HashMap::from([("MOCK_BIN_FAIL_CMD".to_string(), "git:push".to_string())]),
        };

        let Json(response) = handle_git(State(app_state.clone()), Json(request)).await;
        assert!(!response.passthrough);
        assert!(!response.report_exit_status);
        assert_eq!(response.exit_code, 1);
        assert_eq!(
            app_state.state.read().unwrap().pushes,
            [GitPush { args: invocation, refspecs: Vec::new(), exit_code: 1 }]
        );
    }
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
                    "repository" => {
                        handle_repository_query(&mock_state, field, &variables, &|branch| {
                            remote_branch_oid(&app_state, branch)
                        })
                    }
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

    let after_failure = check_and_apply_graphql_after_failure(&mut mock_state, &operations);
    let mutates_prs = operations.iter().any(|operation| {
        matches!(operation, GraphQlOperation::CreatePr | GraphQlOperation::UpdatePr)
    });
    drop(mock_state);
    if mutates_prs {
        if let Err(message) = apply_indirect_merges(&app_state) {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({ "errors": [{ "message": message }] })),
            );
        }
    }

    if let Some(failure) = after_failure {
        return (
            StatusCode::OK,
            Json(serde_json::json!({
                "errors": [
                    { "message": format!("Injected {failure:?} failure after application") }
                ]
            })),
        );
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

fn remote_branch_oid(app_state: &AppState, branch: &str) -> Result<Option<String>, String> {
    let reference = format!("refs/heads/{branch}");
    let output = app_state
        .test_environment
        .command(&app_state.system_git)
        .arg("--git-dir")
        .arg(&app_state.remote_path)
        .args(["rev-parse", "--verify", "--quiet", &reference])
        .output()
        .map_err(|error| format!("Failed to inspect remote Git ref `{reference}`: {error}"))?;
    match output.status.code() {
        Some(0) => Ok(Some(String::from_utf8_lossy(&output.stdout).trim().to_string())),
        Some(1) => Ok(None),
        code => Err(format!("Inspecting remote Git ref `{reference}` exited with {code:?}")),
    }
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
    branch_exists: &dyn Fn(&str) -> Result<bool, String>,
) -> Result<serde_json::Value, String> {
    const PATH: &str = "updatePullRequest";
    let input = input_object(field, PATH)?;
    let node_id = required_string_field(input, "pullRequestId", PATH)?;
    let title = get_string_field(input, "title");
    let body = get_string_field(input, "body");
    let base = get_string_field(input, "baseRefName");

    if let Some(base) = &base {
        if !branch_exists(base)? {
            return Err(format!("Base branch `{base}` does not exist"));
        }
    }

    let Some(pr) = mock_state.prs.iter_mut().find(|pr| pr.node_id == node_id) else {
        return Err(format!("Pull request node `{node_id}` does not exist"));
    };
    if base.as_deref() == Some(pr.head.ref_field.as_str()) {
        return Err("Pull request head and base branches must differ".to_string());
    }
    if mock_state.merge_queue.contains(&pr.id) && base.is_some() {
        return Err("Pull request is in a merge queue and cannot be updated".to_string());
    }

    if let Some(title) = title {
        pr.title = Some(title);
    }
    if let Some(body) = &body {
        pr.body = Some(body.clone());
    }
    let base_update = base.and_then(|base| {
        (pr.base.ref_field != base).then(|| BaseUpdate {
            pr_id: pr.id,
            old_base: std::mem::replace(&mut pr.base.ref_field, base.clone()),
            new_base: base,
        })
    });

    if body.as_deref().is_some_and(|body| body.contains("TRIGGER_GRAPHQL_NULL")) {
        if let Some(update) = base_update {
            mock_state.base_updates.push(update);
        }
        return Ok(serde_json::Value::Null);
    }

    if let Some(update) = base_update {
        mock_state.base_updates.push(update);
        if let Some(pr_id) = mock_state.merge_queue_after_base_update.take() {
            mock_state.merge_queue.insert(pr_id);
        }
    }

    let mut response = serde_json::Map::new();
    for field in selected_fields(&field.selection_set, PATH)? {
        match field.name.as_str() {
            "clientMutationId" => {
                response.insert(response_key(field), serde_json::Value::Null);
            }
            _ => unreachable!("request was checked by validate_update_field"),
        }
    }
    Ok(serde_json::Value::Object(response))
}

fn handle_create_pr(
    mock_state: &mut MockState,
    field: &executable::Field,
    branch_exists: &dyn Fn(&str) -> Result<bool, String>,
) -> Result<serde_json::Value, String> {
    const PATH: &str = "createPullRequest";
    let input = input_object(field, PATH)?;
    let repository_id = required_string_field(input, "repositoryId", PATH)?;
    let base = required_string_field(input, "baseRefName", PATH)?;
    let head = required_string_field(input, "headRefName", PATH)?;
    let title = required_string_field(input, "title", PATH)?;
    let body = get_string_field(input, "body").unwrap_or_default();

    if repository_id != "REPO_NODE_ID" {
        return Err(format!("Repository node `{repository_id}` does not exist"));
    }
    if base == head {
        return Err("Pull request head and base branches must differ".to_string());
    }
    if !branch_exists(&base)? {
        return Err(format!("Base branch `{base}` does not exist"));
    }
    if !branch_exists(&head)? {
        return Err(format!("Head branch `{head}` does not exist"));
    }
    if mock_state.prs.iter().any(|pr| pr.state == "OPEN" && pr.head.ref_field == head) {
        return Err(format!("An open pull request already exists for head branch `{head}`"));
    }

    let number = mock_state.prs.iter().map(|pr| pr.number as u64).max().unwrap_or(0) + 1;
    let owner = mock_state.repo_owner.clone();
    let repo = mock_state.repo_name.clone();
    let entry = PrEntry::mock(MockPrArgs {
        id: number,
        title,
        body,
        head,
        base,
        repo_owner: &owner,
        repo_name: &repo,
    });
    let node_id = entry.node_id.clone();
    let html_url = entry.html_url.clone();
    mock_state.prs.push(entry);

    let mut response = serde_json::Map::new();
    for field in selected_fields(&field.selection_set, PATH)? {
        match field.name.as_str() {
            "clientMutationId" => {
                response.insert(response_key(field), serde_json::Value::Null);
            }
            "pullRequest" => {
                let mut pull_request = serde_json::Map::new();
                for field in selected_fields(&field.selection_set, "createPullRequest.pullRequest")?
                {
                    let value = match field.name.as_str() {
                        "number" => serde_json::json!(number),
                        "url" => serde_json::json!(html_url),
                        "id" => serde_json::json!(node_id),
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
    branch_oid: &dyn Fn(&str) -> Result<Option<String>, String>,
) -> Result<serde_json::Value, String> {
    const PATH: &str = "repository";
    let owner = resolve_string_argument(field, "owner", PATH, variables)?;
    let name = resolve_string_argument(field, "name", PATH, variables)?;

    if !owner.eq_ignore_ascii_case(&mock_state.repo_owner)
        || !name.eq_ignore_ascii_case(&mock_state.repo_name)
    {
        return Ok(serde_json::Value::Null);
    }

    let mut repo_data = serde_json::Map::new();

    for field in selected_fields(&field.selection_set, PATH)? {
        match field.name.as_str() {
            "pullRequests" => {
                let head = resolve_optional_string_argument(
                    field,
                    "headRefName",
                    "repository.pullRequests",
                    variables,
                )?;
                let base = resolve_optional_string_argument(
                    field,
                    "baseRefName",
                    "repository.pullRequests",
                    variables,
                )?;
                let states = enum_list_argument(field, "states", "repository.pullRequests")?;
                let matching_prs = mock_state
                    .prs
                    .iter()
                    .filter(|pr| states.contains(&pr.state))
                    .filter(|pr| head.as_ref().is_none_or(|head| pr.head.ref_field == *head))
                    .filter(|pr| base.as_ref().is_none_or(|base| pr.base.ref_field == *base))
                    .collect::<Vec<_>>();

                let mut connection = serde_json::Map::new();
                for field in selected_fields(&field.selection_set, "repository.pullRequests")? {
                    match field.name.as_str() {
                        "totalCount" => {
                            connection
                                .insert(response_key(field), serde_json::json!(matching_prs.len()));
                        }
                        "nodes" => {
                            let nodes = matching_prs
                                .iter()
                                .map(|pr| {
                                    project_pr_node(
                                        mock_state,
                                        pr,
                                        &field.selection_set,
                                        branch_oid,
                                    )
                                })
                                .collect::<Result<Vec<_>, _>>()?;
                            connection.insert(response_key(field), serde_json::json!(nodes));
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
            "nameWithOwner" => {
                repo_data.insert(
                    response_key(field),
                    serde_json::Value::String(format!(
                        "{}/{}",
                        mock_state.repo_owner, mock_state.repo_name
                    )),
                );
            }
            _ => unreachable!("request was checked by validate_repository_field"),
        }
    }

    Ok(serde_json::Value::Object(repo_data))
}

fn project_pr_node(
    mock_state: &MockState,
    pr: &PrEntry,
    selection_set: &executable::SelectionSet,
    branch_oid: &dyn Fn(&str) -> Result<Option<String>, String>,
) -> Result<serde_json::Value, String> {
    let mut node = serde_json::Map::new();
    for field in selected_fields(selection_set, "repository.pullRequests.nodes")? {
        let value = match field.name.as_str() {
            "number" => serde_json::json!(pr.number),
            "id" => serde_json::json!(pr.node_id),
            "title" => serde_json::json!(pr.title),
            "body" => serde_json::json!(pr.body),
            "baseRefName" => serde_json::json!(pr.base.ref_field),
            "baseRefOid" => serde_json::json!(branch_oid(&pr.base.ref_field)?
                .ok_or_else(|| format!("Base branch `{}` does not exist", pr.base.ref_field))?),
            "headRefName" => serde_json::json!(pr.head.ref_field),
            "headRefOid" => serde_json::json!(branch_oid(&pr.head.ref_field)?
                .ok_or_else(|| format!("Head branch `{}` does not exist", pr.head.ref_field))?),
            "headRepository" => match &pr.head_repository {
                Some(repository) => project_nested_object(
                    field,
                    "PullRequest.headRepository",
                    &[
                        ("id", serde_json::json!(repository.id)),
                        ("nameWithOwner", serde_json::json!(repository.name_with_owner)),
                    ],
                )?,
                None => serde_json::Value::Null,
            },
            "baseRepository" => match &pr.base_repository {
                Some(repository) => project_nested_object(
                    field,
                    "PullRequest.baseRepository",
                    &[
                        ("id", serde_json::json!(repository.id)),
                        ("nameWithOwner", serde_json::json!(repository.name_with_owner)),
                    ],
                )?,
                None => serde_json::Value::Null,
            },
            "isCrossRepository" => serde_json::json!(pr.is_cross_repository),
            "state" => serde_json::json!(pr.state),
            "isInMergeQueue" => serde_json::json!(mock_state.merge_queue.contains(&pr.id)),
            "autoMergeRequest" => {
                if mock_state.auto_merge.contains(&pr.id) {
                    project_nested_object(
                        field,
                        "PullRequest.autoMergeRequest",
                        &[("enabledAt", serde_json::json!("2026-01-01T00:00:00Z"))],
                    )?
                } else {
                    serde_json::Value::Null
                }
            }
            "stackEntry" => {
                if mock_state.native_stacks.contains(&pr.id) {
                    project_nested_object(
                        field,
                        "PullRequest.stackEntry",
                        &[("id", serde_json::json!(format!("STACK_ENTRY_{}", pr.id)))],
                    )?
                } else {
                    serde_json::Value::Null
                }
            }
            _ => unreachable!("request was checked by validate_pull_requests_field"),
        };
        node.insert(response_key(field), value);
    }
    Ok(serde_json::Value::Object(node))
}

fn project_nested_object(
    field: &executable::Field,
    path: &str,
    values: &[(&str, serde_json::Value)],
) -> Result<serde_json::Value, String> {
    let values = values.iter().cloned().collect::<HashMap<_, _>>();
    let mut object = serde_json::Map::new();
    for selected in selected_fields(&field.selection_set, path)? {
        object.insert(
            response_key(selected),
            values
                .get(selected.name.as_str())
                .cloned()
                .ok_or_else(|| format!("No mock value for `{path}.{}`", selected.name))?,
        );
    }
    Ok(serde_json::Value::Object(object))
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
        let mut state =
            MockState { fail_next_request: Some(FailureKind::UpdatePr), ..Default::default() };

        assert_eq!(check_and_apply_graphql_failure(&mut state, &[GraphQlOperation::Query]), None);
        assert_eq!(state.fail_next_request, Some(FailureKind::UpdatePr));

        assert_eq!(
            check_and_apply_graphql_failure(&mut state, &[GraphQlOperation::CreatePr]),
            None
        );

        assert_eq!(
            check_and_apply_graphql_failure(&mut state, &[GraphQlOperation::UpdatePr]),
            Some(FailureKind::UpdatePr)
        );
        assert_eq!(state.fail_next_request, None);
    }

    #[test]
    fn generic_graphql_failure_matches_any_operation() {
        let mut state =
            MockState { fail_next_request: Some(FailureKind::GraphQl), ..Default::default() };

        assert_eq!(
            check_and_apply_graphql_failure(&mut state, &[GraphQlOperation::Query]),
            Some(FailureKind::GraphQl)
        );
        assert_eq!(state.fail_next_request, None);
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
             pullRequests(headRefName: \"Ghead\", first: 100, \
             states: [OPEN, CLOSED, MERGED]) { nodes { number, id, title, body, \
             baseRefName, state } } } }",
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
             pullRequests(headRefName: \"Ghead\", first: 100, \
             states: [OPEN, CLOSED, MERGED]) { nodes { number } } } }",
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
        let response = handle_repository_query(&state, root_field(&document), &None, &|branch| {
            Ok(Some(format!("{branch}-oid")))
        })
        .unwrap();
        assert_eq!(
            response,
            serde_json::json!({
                "pullRequests": {
                    "nodes": [{ "number": 1 }]
                }
            })
        );
    }

    #[test]
    fn create_validates_refs_uniqueness_and_numbering() {
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
            head: "Gold".to_string(),
            base: "main".to_string(),
            repo_owner: "owner",
            repo_name: "repo",
        }));

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

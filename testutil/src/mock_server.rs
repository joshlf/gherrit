use std::{
    collections::{HashMap, HashSet, VecDeque},
    future::IntoFuture,
    path::PathBuf,
    sync::{mpsc::Sender, Arc, LazyLock, RwLock},
};

use apollo_compiler::{ast, executable, validation::Valid, ExecutableDocument, Name, Node};
use axum::{extract::State, http::StatusCode, routing::post, Json, Router};
use serde::{Deserialize, Serialize};
use tokio::net::TcpListener;

use crate::{FailureKind, GraphQlOperation, TestEnvironment};

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
    pub faults: VecDeque<FailureKind>,
    pub merge_queue: HashSet<u64>,
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
    pub delete: bool,
    pub exit_code: i32,
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

    let fail_action = mock_state.faults.front()?;
    let matches = match fail_action {
        GraphQl => true,
        CreatePr => operations.contains(&GraphQlOperation::CreatePr),
        UpdatePr | UpdatePrNull => operations.contains(&GraphQlOperation::UpdatePr),
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

fn validate_pull_requests_field(field: &executable::Field) -> Result<(), String> {
    const PATH: &str = "repository.pullRequests";
    validate_argument_names(field, PATH, &["headRefName", "first", "states"])?;

    let first = argument(field, "first")
        .and_then(|value| match value {
            ast::Value::Int(value) => Some(value.as_str()),
            _ => None,
        })
        .ok_or_else(|| format!("The mock GitHub API requires `{PATH}(first: 1)`"))?;
    if first != "1" {
        return Err(format!("The mock GitHub API only supports `{PATH}(first: 1)`"));
    }

    let states = argument(field, "states")
        .and_then(|value| match value {
            ast::Value::List(values) => Some(values),
            _ => None,
        })
        .ok_or_else(|| {
            format!("The mock GitHub API requires `{PATH}(states: [OPEN, CLOSED, MERGED])`")
        })?;
    let state_count = states.len();
    let states: HashSet<_> = states
        .iter()
        .filter_map(|value| match &**value {
            ast::Value::Enum(value) => Some(value.as_str()),
            _ => None,
        })
        .collect();
    if state_count != 3 || states != HashSet::from(["OPEN", "CLOSED", "MERGED"]) {
        return Err(format!(
            "The mock GitHub API only supports `{PATH}(states: [OPEN, CLOSED, MERGED])`"
        ));
    }

    for field in selected_fields(&field.selection_set, PATH)? {
        if field.name != "nodes" {
            return Err(format!(
                "The mock GitHub API does not support field `{PATH}.{}`",
                field.name
            ));
        }
        validate_scalar_fields(
            &field.selection_set,
            "repository.pullRequests.nodes",
            &["number", "id", "title", "body", "baseRefName", "state"],
        )?;
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
        return Json(GitResponse {
            stdout: "".to_string(),
            stderr,
            exit_code: 0,
            passthrough: true,
            report_exit_status: true,
        });
    }

    // Default: strict passthrough
    Json(GitResponse {
        stdout: "".to_string(),
        stderr: "".to_string(),
        exit_code: 0,
        passthrough: true,
        report_exit_status: false,
    })
}

async fn complete_git(
    State(app_state): State<AppState>,
    Json(completion): Json<GitCompletion>,
) -> StatusCode {
    if parse_git_command(&completion.args).is_none_or(|command| command.name != "push") {
        return StatusCode::BAD_REQUEST;
    }

    record_push(&app_state, completion.args, completion.exit_code);
    StatusCode::NO_CONTENT
}

fn record_push(app_state: &AppState, args: Vec<String>, exit_code: i32) {
    let command = parse_git_command(&args)
        .filter(|command| command.name == "push")
        .expect("record_push requires a parsed Git push");
    let delete = push_deletes_refspecs(command.args);
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
    app_state.state.write().unwrap().pushes.push(GitPush { args, refspecs, delete, exit_code });
}

fn push_deletes_refspecs(arguments: &[String]) -> bool {
    let mut delete = false;
    let mut consume_value = false;

    for argument in arguments {
        if consume_value {
            consume_value = false;
            continue;
        }
        if argument == "--" {
            break;
        }

        match argument.as_str() {
            "--delete" => delete = true,
            "--no-delete" => delete = false,
            // These documented `git push` options consume the next argument.
            // An option value is opaque even when it resembles another flag.
            "--repo" | "--receive-pack" | "--exec" | "--recurse-submodules" | "--push-option" => {
                consume_value = true
            }
            argument if !argument.starts_with("--") => {
                let Some(options) = argument.strip_prefix('-') else {
                    continue;
                };
                for (index, option) in options.char_indices() {
                    match option {
                        'd' => delete = true,
                        // `-o` consumes the remainder of its bundle, or the
                        // next argument when no inline value remains.
                        'o' => {
                            consume_value = index + option.len_utf8() == options.len();
                            break;
                        }
                        _ => {}
                    }
                }
            }
            _ => {}
        }
    }

    delete
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
                delete: false,
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
            [GitPush { args: invocation, refspecs: Vec::new(), delete: false, exit_code: 1 }]
        );
    }

    #[tokio::test]
    async fn records_long_and_short_delete_options() {
        for option in ["--delete", "-d"] {
            let app_state = app_state();
            let invocation = args(&["git", "push", option, "origin", "refs/heads/old"]);

            record_push(&app_state, invocation.clone(), 0);

            assert_eq!(
                app_state.state.read().unwrap().pushes,
                [GitPush {
                    args: invocation,
                    refspecs: vec!["refs/heads/old".to_string()],
                    delete: true,
                    exit_code: 0,
                }]
            );
        }
    }

    #[test]
    fn records_push_option_values_without_reinterpreting_them() {
        let app_state = app_state();
        let invocation = args(&["git", "push", "-o", "-d", "origin", "HEAD:refs/heads/main"]);

        record_push(&app_state, invocation.clone(), 0);

        assert_eq!(
            app_state.state.read().unwrap().pushes,
            [GitPush {
                args: invocation,
                refspecs: vec!["HEAD:refs/heads/main".to_string()],
                delete: false,
                exit_code: 0,
            }]
        );
    }

    #[test]
    fn parses_effective_delete_option_state() {
        for (arguments, expected) in [
            (args(&["origin", "refs/heads/old"]), false),
            (args(&["--delete", "origin", "refs/heads/old"]), true),
            (args(&["-qd", "origin", "refs/heads/old"]), true),
            (args(&["-dq", "origin", "refs/heads/old"]), true),
            (args(&["--delete", "--no-delete", "origin"]), false),
            (args(&["--no-delete", "-qd", "origin"]), true),
            (args(&["-qd", "--no-delete", "origin"]), false),
            (args(&["-odelete", "origin", "refs/heads/old"]), false),
            (args(&["-o", "-d", "origin", "refs/heads/old"]), false),
            (args(&["-qo", "-d", "origin", "refs/heads/old"]), false),
            (args(&["-do", "value", "origin", "refs/heads/old"]), true),
            (args(&["--push-option", "--delete", "origin"]), false),
            (args(&["--repo", "-d", "refs/heads/old"]), false),
            (args(&["--receive-pack", "-d", "origin"]), false),
            (args(&["--exec", "-d", "origin"]), false),
            (args(&["--recurse-submodules", "-d", "origin"]), false),
            (args(&["--", "--delete", "refs/heads/old"]), false),
        ] {
            assert_eq!(push_deletes_refspecs(&arguments), expected, "arguments: {arguments:?}");
        }
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
    let failure = check_and_apply_graphql_failure(&mut mock_state, &operations);
    if let Some(failure) = failure.as_ref().filter(|failure| **failure != FailureKind::UpdatePrNull)
    {
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
                    "updatePullRequest" => {
                        let result = handle_update_pr(&mut mock_state, field, &|branch| {
                            remote_branch_exists(&app_state, branch)
                        });
                        if failure == Some(FailureKind::UpdatePrNull) && result.is_ok() {
                            Ok(serde_json::Value::Null)
                        } else {
                            result
                        }
                    }
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
    if let Some(base) = base {
        pr.base.ref_field = base;
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
                let matching_prs: Vec<_> =
                    mock_state.prs.iter().filter(|pr| pr.head.ref_field == head).take(1).collect();

                let mut connection = serde_json::Map::new();
                for field in selected_fields(&field.selection_set, "repository.pullRequests")? {
                    match field.name.as_str() {
                        "nodes" => {
                            let nodes = matching_prs
                                .iter()
                                .map(|pr| project_pr_node(pr, &field.selection_set))
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
            _ => unreachable!("request was checked by validate_repository_field"),
        }
    }

    Ok(serde_json::Value::Object(repo_data))
}

fn project_pr_node(
    pr: &PrEntry,
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
            _ => unreachable!("request was checked by validate_pull_requests_field"),
        };
        node.insert(response_key(field), value);
    }
    Ok(serde_json::Value::Object(node))
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
             pullRequests(headRefName: \"Ghead\", first: 1, \
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
             pullRequests(headRefName: \"Ghead\", first: 1, \
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
             pullRequests(headRefName: \"Ghead\", first: 1, \
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
        let response = handle_repository_query(&state, root_field(&document), &None).unwrap();
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

use std::{
    collections::HashMap,
    sync::{Arc, RwLock},
};

use apollo_compiler::{ast, executable, validation::Valid, ExecutableDocument, Name, Node};
use axum::{
    extract::{Path, Query, State},
    http::{HeaderMap, StatusCode},
    response::IntoResponse,
    routing::{get, post},
    Json, Router,
};
use serde::{Deserialize, Serialize};
use tokio::net::TcpListener;

use crate::FailureKind;

#[derive(Debug, Clone, Default)]
pub struct MockState {
    pub prs: Vec<PrEntry>,
    pub pushed_refs: Vec<String>,
    pub push_count: usize,
    pub repo_owner: String,
    pub repo_name: String,
    pub fail_next_request: Option<FailureKind>,
    pub fail_remaining: usize,
    pub schema: Option<Valid<apollo_compiler::Schema>>,
}

impl MockState {
    pub fn new(owner: String, name: String) -> Self {
        let schema_src = include_str!("../data/github_schema.graphql");
        let schema =
            apollo_compiler::Schema::parse_and_validate(schema_src, "github_schema.graphql")
                .expect("Failed to parse and validate embedded GitHub schema");

        Self { repo_owner: owner, repo_name: name, schema: Some(schema), ..Default::default() }
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
}

#[derive(Clone)]
struct AppState {
    state: Arc<RwLock<MockState>>,
    base_url: String,
}

/// Starts a mock GitHub API server on a random local port.
/// Returns the address of the running server (e.g., `http://127.0.0.1:12345`).
pub async fn start_mock_server(state: Arc<RwLock<MockState>>) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let url = format!("http://{}", addr);

    let app_state = AppState { state, base_url: url.clone() };

    let app = Router::new()
        .route("/repos/{owner}/{repo}/pulls", get(list_prs))
        .route("/graphql", post(graphql))
        .route("/_internal/git", post(handle_git))
        .with_state(app_state);

    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });

    url
}

fn check_and_apply_failure(mock_state: &mut MockState, action: FailureKind) -> bool {
    use FailureKind::*;

    let Some(fail_action) = &mock_state.fail_next_request else { return false };
    let matches = match (fail_action, action) {
        (GraphQl, GraphQl | CreatePr | UpdatePr) => true,
        (f, a) => f == &a,
    };

    if !matches {
        return false;
    }

    if mock_state.fail_remaining > 0 {
        mock_state.fail_remaining -= 1;
    }
    if mock_state.fail_remaining == 0 {
        mock_state.fail_next_request = None;
    }

    true
}

async fn handle_git(
    State(app_state): State<AppState>,
    Json(req): Json<GitRequest>,
) -> Json<GitResponse> {
    // Check for simulated failure
    if let Some(subcommand) = req.args.get(1) {
        if req
            .env
            .get("MOCK_BIN_FAIL_CMD")
            .is_some_and(|fail_cmd| fail_cmd == &format!("git:{}", subcommand))
        {
            return Json(GitResponse {
                stdout: "".to_string(),
                stderr: format!("Simulated failure for git {}", subcommand),
                exit_code: 1,
                passthrough: false,
            });
        }
    }

    // Spy on "push" logic
    if req.args.contains(&"push".to_string()) {
        let mut state = app_state.state.write().unwrap();
        let refspecs: Vec<String> = req
            .args
            .iter()
            .skip(1)
            .filter(|arg| arg.starts_with("refs/") || arg.contains(":"))
            .cloned()
            .collect();

        state.pushed_refs.extend(refspecs);
        state.push_count += 1;
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
        });
    }

    // Default: strict passthrough
    Json(GitResponse {
        stdout: "".to_string(),
        stderr: "".to_string(),
        exit_code: 0,
        passthrough: true,
    })
}

async fn list_prs(
    State(state): State<AppState>,
    Path((owner, repo)): Path<(String, String)>,
    Query(params): Query<HashMap<String, String>>,
) -> Result<impl IntoResponse, StatusCode> {
    let mut mock_state = state.state.write().unwrap();
    if check_and_apply_failure(&mut mock_state, FailureKind::GitCmd("list_prs".to_string())) {
        return Err(StatusCode::INTERNAL_SERVER_ERROR);
    }

    let page = params.get("page").and_then(|p| p.parse::<usize>().ok()).unwrap_or(1);
    let per_page = params.get("per_page").and_then(|p| p.parse::<usize>().ok()).unwrap_or(30);

    let start = (page - 1) * per_page;
    let total = mock_state.prs.len();
    let end = start + per_page;

    let items = if start >= total {
        Vec::new()
    } else {
        mock_state.prs[start..std::cmp::min(end, total)].to_vec()
    };

    let mut headers = HeaderMap::new();
    if end < total {
        let next_page = page + 1;
        let last_page = total.div_ceil(per_page);
        let next_url = format!(
            "{}/repos/{}/{}/pulls?page={}&per_page={}",
            state.base_url, owner, repo, next_page, per_page
        );
        let last_url = format!(
            "{}/repos/{}/{}/pulls?page={}&per_page={}",
            state.base_url, owner, repo, last_page, per_page
        );
        let link = format!(r#"<{}>; rel="next", <{}>; rel="last""#, next_url, last_url);
        headers.insert("Link", link.parse().unwrap());
    }

    Ok((headers, Json(items)))
}

async fn graphql(
    State(state): State<AppState>,
    Json(payload): Json<serde_json::Value>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    let query = payload.get("query").and_then(|v| v.as_str()).ok_or_else(|| {
        eprintln!("DEBUG: Invalid GraphQL payload (missing 'query'): {}", payload);
        StatusCode::BAD_REQUEST
    })?;
    let variables = payload.get("variables").and_then(|v| v.as_object()).cloned();

    let mut mock_state = state.state.write().unwrap();

    if check_and_apply_failure(&mut mock_state, FailureKind::UpdatePr)
        || check_and_apply_failure(&mut mock_state, FailureKind::CreatePr)
        || check_and_apply_failure(&mut mock_state, FailureKind::GraphQl)
    {
        return Ok(Json(serde_json::json!({
            "errors": [
                { "message": "Injected failure" }
            ]
        })));
    }

    let schema = mock_state.schema.as_ref().expect("Schema not initialized");
    let document = match ExecutableDocument::parse_and_validate(schema, query, "query.graphql") {
        Ok(doc) => doc,
        Err(e) => {
            eprintln!("DEBUG: GraphQL validation errors: {:?}", e.errors);
            return Err(StatusCode::BAD_REQUEST);
        }
    };

    let mut response_data = serde_json::Map::new();

    for operation in document.operations.iter() {
        for selection in operation.selection_set.selections.iter() {
            if let executable::Selection::Field(field) = selection {
                let alias = field
                    .alias
                    .as_ref()
                    .map(|a| a.as_str())
                    .unwrap_or_else(|| field.name.as_str())
                    .to_string();

                match field.name.as_str() {
                    "updatePullRequest" => {
                        handle_update_pr(&mut mock_state, field, &alias, &mut response_data);
                    }
                    "createPullRequest" => {
                        handle_create_pr(&mut mock_state, field, &alias, &mut response_data);
                    }
                    "repository" => {
                        handle_repository_query(
                            &mock_state,
                            field,
                            &alias,
                            &variables,
                            &mut response_data,
                        );
                    }
                    _ => {}
                }
            }
        }
    }

    Ok(Json(serde_json::json!({
        "data": response_data
    })))
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

fn handle_update_pr(
    mock_state: &mut MockState,
    field: &executable::Field,
    alias: &str,
    response_data: &mut serde_json::Map<String, serde_json::Value>,
) {
    if let Some(input) = extract_input_field(field, "input") {
        let node_id = get_string_field(input, "pullRequestId").unwrap();
        let title = get_string_field(input, "title");
        let body = get_string_field(input, "body");
        let base = get_string_field(input, "baseRefName");

        if let Some(pr) = mock_state.prs.iter_mut().find(|p| p.node_id == node_id) {
            if let Some(t) = title {
                pr.title = Some(t);
            }
            if let Some(b) = &body {
                pr.body = Some(b.clone());
            }
            if let Some(base_ref) = base {
                pr.base.ref_field = base_ref;
            }
        }

        if body.as_deref().map(|b| b.contains("TRIGGER_GRAPHQL_NULL")).unwrap_or(false) {
            response_data.insert(alias.to_string(), serde_json::Value::Null);
        } else {
            response_data.insert(
                alias.to_string(),
                serde_json::json!({
                    "clientMutationId": null
                }),
            );
        }
    }
}

fn handle_create_pr(
    mock_state: &mut MockState,
    field: &executable::Field,
    alias: &str,
    response_data: &mut serde_json::Map<String, serde_json::Value>,
) {
    if let Some(input) = extract_input_field(field, "input") {
        let base = get_string_field(input, "baseRefName").unwrap();
        let head = get_string_field(input, "headRefName").unwrap();
        let title_val = get_string_field(input, "title").unwrap();
        let body_val = get_string_field(input, "body").unwrap();

        let num = mock_state.prs.len() as u64 + 1;
        let owner = mock_state.repo_owner.clone();
        let repo = mock_state.repo_name.clone();

        let entry = PrEntry::mock(MockPrArgs {
            id: num,
            title: title_val,
            body: body_val,
            head,
            base,
            repo_owner: &owner,
            repo_name: &repo,
        });
        let node_id = entry.node_id.clone();
        let html_url = entry.html_url.clone();

        mock_state.prs.push(entry);

        response_data.insert(
            alias.to_string(),
            serde_json::json!({
                "clientMutationId": null,
                "pullRequest": {
                    "id": node_id,
                    "number": num,
                    "url": html_url,
                }
            }),
        );
    }
}

fn handle_repository_query(
    mock_state: &MockState,
    field: &executable::Field,
    alias: &str,
    variables: &Option<serde_json::Map<String, serde_json::Value>>,
    response_data: &mut serde_json::Map<String, serde_json::Value>,
) {
    let get_string_arg = |name| -> Option<String> {
        let arg = field.arguments.iter().find(|arg| arg.name == name)?;
        match &*arg.value {
            ast::Value::String(s) => Some(s.to_string()),
            ast::Value::Variable(var_name) => {
                variables.as_ref()?.get(var_name.as_str())?.as_str().map(|s| s.to_string())
            }
            _ => None,
        }
    };

    let owner = get_string_arg("owner");
    let name = get_string_arg("name");

    if owner.as_deref() != Some(mock_state.repo_owner.as_str())
        || name.as_deref() != Some(mock_state.repo_name.as_str())
    {
        response_data.insert(alias.to_string(), serde_json::Value::Null);
        return;
    }

    let mut repo_data = serde_json::Map::new();

    for selection in field.selection_set.selections.iter() {
        if let executable::Selection::Field(sub_field) = selection {
            match sub_field.name.as_str() {
                "pullRequests" => {
                    let head = sub_field.arguments.iter().find_map(|arg| {
                        match (&*arg.name, &*arg.value) {
                            ("headRefName", ast::Value::String(s)) => Some(s.to_string()),
                            ("headRefName", ast::Value::Variable(var_name)) => variables
                                .as_ref()?
                                .get(var_name.as_str())?
                                .as_str()
                                .map(|s| s.to_string()),
                            _ => None,
                        }
                    });

                    // Filter PRs
                    let prs: Vec<_> = mock_state
                        .prs
                        .iter()
                        .filter(|pr| match &head {
                            Some(h) => pr.head.ref_field == *h,
                            None => true,
                        })
                        .collect();

                    let nodes: Vec<_> = prs
                        .into_iter()
                        .map(|pr| {
                            serde_json::json!({
                                "number": pr.number,
                                "id": pr.node_id,
                                "title": pr.title,
                                "body": pr.body,
                                "baseRefName": pr.base.ref_field,
                                "state": pr.state,
                                "url": pr.html_url,
                                "headRefName": pr.head.ref_field,
                            })
                        })
                        .collect();

                    repo_data.insert(
                        "pullRequests".to_string(),
                        serde_json::json!({
                            "nodes": nodes
                        }),
                    );
                }
                "id" => {
                    repo_data.insert(
                        "id".to_string(),
                        serde_json::Value::String("REPO_NODE_ID".to_string()),
                    );
                }
                _ => {}
            }
        }
    }

    if !repo_data.is_empty() {
        response_data.insert(alias.to_string(), serde_json::Value::Object(repo_data));
    }
}

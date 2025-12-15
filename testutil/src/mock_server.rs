use axum::{
    extract::{Path, Query, State},
    http::{HeaderMap, StatusCode},
    response::IntoResponse,
    routing::get,
    Json, Router,
};

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;
use tokio::net::TcpListener;

#[macro_export]
macro_rules! re {
    ($re:literal) => {{
        static RE: std::sync::LazyLock<regex::Regex> =
            std::sync::LazyLock::new(|| regex::Regex::new($re).unwrap());
        &*RE
    }};
}

#[derive(Serialize, Deserialize, Debug, Clone, Default)]
pub struct MockState {
    #[serde(default)]
    pub prs: Vec<PrEntry>,
    #[serde(default)]
    pub pushed_refs: Vec<String>,
    #[serde(default)]
    pub push_count: usize,
    #[serde(default = "default_owner")]
    pub repo_owner: String,
    #[serde(default = "default_repo")]
    pub repo_name: String,
    #[serde(default)]
    pub fail_next_request: Option<String>,
    #[serde(default)]
    pub fail_remaining: usize,
}

fn default_owner() -> String {
    "owner".to_string()
}

fn default_repo() -> String {
    "repo".to_string()
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
    state_path: PathBuf,
    base_url: String,
}

/// Starts a mock GitHub API server on a random local port.
///
/// Returns the address of the running server (e.g., `http://127.0.0.1:12345`).
pub async fn start_mock_server(state_path: PathBuf) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let url = format!("http://{}", addr);

    let app_state = AppState {
        state_path,
        base_url: url.clone(),
    };

    let app = Router::new()
        .route("/repos/{owner}/{repo}/pulls", get(list_prs))
        .route("/graphql", axum::routing::post(graphql))
        .with_state(app_state);

    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });

    url
}

fn check_and_apply_failure(mock_state: &mut MockState, action_name: &str) -> bool {
    if let Some(action) = &mock_state.fail_next_request {
        if action == action_name
            || (action == "graphql" && (action_name == "update_pr" || action_name == "create_pr"))
        {
            if mock_state.fail_remaining > 0 {
                mock_state.fail_remaining -= 1;
            }
            if mock_state.fail_remaining == 0 {
                mock_state.fail_next_request = None;
            }
            return true;
        }
    }
    false
}

/// Handler for `GET /repos/:owner/:repo/pulls`.
/// Returns a list of PRs from the mock state.
async fn list_prs(
    State(state): State<AppState>,
    Path((owner, repo)): Path<(String, String)>,
    Query(params): Query<HashMap<String, String>>,
) -> Result<impl IntoResponse, StatusCode> {
    let mut mock_state = read_state(&state.state_path);
    if check_and_apply_failure(&mut mock_state, "list_prs") {
        write_state(&state.state_path, &mock_state);
        return Err(StatusCode::INTERNAL_SERVER_ERROR);
    }

    let page = params
        .get("page")
        .and_then(|p| p.parse::<usize>().ok())
        .unwrap_or(1);
    let per_page = params
        .get("per_page")
        .and_then(|p| p.parse::<usize>().ok())
        .unwrap_or(30);

    let start = (page - 1) * per_page;
    let end = start + per_page;

    let total = mock_state.prs.len();
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

/// Handler for `POST /graphql`.
///
/// This mock implementation does *not* use a real GraphQL parser. Instead, it
/// relies on simple regex matching to identify supported mutations
/// (`updatePullRequest` and `createPullRequest`) and extract their input
/// fields.
///
/// This strategy is sufficient for the specific, predictable queries generated
/// by the client's batching logic (aliased mutations).
async fn graphql(
    State(state): State<AppState>,
    Json(payload): Json<serde_json::Value>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    let query = payload
        .get("query")
        .and_then(|v| v.as_str())
        .ok_or_else(|| {
            eprintln!(
                "DEBUG: Invalid GraphQL payload (missing 'query'): {}",
                payload
            );
            StatusCode::BAD_REQUEST
        })?;

    let mut mock_state = read_state(&state.state_path);

    // Support failure injection.
    if check_and_apply_failure(&mut mock_state, "update_pr")
        || check_and_apply_failure(&mut mock_state, "create_pr")
        || check_and_apply_failure(&mut mock_state, "graphql")
    {
        write_state(&state.state_path, &mock_state);
        return Ok(Json(serde_json::json!({
            "errors": [
                { "message": "Injected failure" }
            ]
        })));
    }

    let mut response_data = serde_json::Map::new();

    // Regex definitions
    // These regexes capture the full JSON string for complex fields like title and body.
    let update_re = re!(
        r#"op(?P<idx>\d+): updatePullRequest\(input: \{pullRequestId: "(?P<id>[^"]+)", baseRefName: "(?P<base>[^"]+)", title: (?P<title>"(?:\\.|[^"\\])*"), body: (?P<body>"(?:\\.|[^"\\])*")\}\)"#
    );
    let create_re = re!(
        r#"op(?P<idx>\d+): createPullRequest\(input: \{ repositoryId: "[^"]+", baseRefName: "(?P<base>[^"]+)", headRefName: "(?P<head>[^"]+)", title: (?P<title>"(?:\\.|[^"\\])*"), body: (?P<body>"(?:\\.|[^"\\])*") \}\)"#
    );
    // Modified regex for pullRequests to handle potential spacing and quotes correctly
    // op0: repository... pullRequests(headRefName: "head"
    // We skip matching 'repository' explicitly to be safer against spacing/formatting variations.
    let pr_query_re = re!(r#"op(?P<idx>\d+):.*?pullRequests\(headRefName:\s*"(?P<head>[^"]+)""#);

    // Process Updates
    for caps in update_re.captures_iter(query) {
        let idx = caps.name("idx").expect("Missing idx").as_str();
        let node_id = caps.name("id").expect("Missing id").as_str();
        let title = caps.name("title").expect("Missing title").as_str();
        let body = caps.name("body").expect("Missing body").as_str();
        let base = caps.name("base").expect("Missing base").as_str();

        if let Some(pr) = mock_state.prs.iter_mut().find(|p| p.node_id == node_id) {
            pr.title = Some(serde_json::from_str::<String>(title).unwrap());
            pr.body = Some(serde_json::from_str::<String>(body).unwrap());
            pr.base.ref_field = base.to_string();
        }

        let alias = format!("op{}", idx);

        // Regression test helper: inject failure if body contains specific trigger
        if body.contains("TRIGGER_GRAPHQL_NULL") {
            response_data.insert(alias, serde_json::Value::Null);
        } else {
            response_data.insert(
                alias,
                serde_json::json!({
                    "clientMutationId": null
                }),
            );
        }
    }

    // Process Creations
    for caps in create_re.captures_iter(query) {
        let idx = caps.name("idx").expect("Missing idx").as_str();
        let base = caps.name("base").expect("Missing base").as_str();
        let head = caps.name("head").expect("Missing head").as_str();
        let title = caps.name("title").expect("Missing title").as_str();
        let body = caps.name("body").expect("Missing body").as_str();

        let num = mock_state.prs.len() as u64 + 1;
        let node_id = format!("PR_{}", num);
        let html_url = format!("http://github.com/owner/repo/pull/{}", num);

        let title_val = serde_json::from_str::<String>(title)
            .expect("Failed to parse title in mock server; client may have sent invalid JSON");
        let body_val = serde_json::from_str::<String>(body)
            .expect("Failed to parse body in mock server; client may have sent invalid JSON");

        let entry = PrEntry {
            id: num,
            number: num as usize,
            html_url: html_url.clone(),
            api_url: format!("http://api.github.com/repos/owner/repo/pulls/{}", num),
            node_id: node_id.clone(),
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
            title: Some(title_val),
            body: Some(body_val),
            head: RefInfo {
                ref_field: head.to_string(),
                sha: "".to_string(),
            },
            base: RefInfo {
                ref_field: base.to_string(),
                sha: "".to_string(),
            },
            created_at: "2023-01-01T00:00:00Z".to_string(),
            updated_at: "2023-01-01T00:00:00Z".to_string(),
        };

        mock_state.prs.push(entry);

        let alias = format!("op{}", idx);
        response_data.insert(
            alias,
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

    // Process batched pullRequests query
    let mut has_pr_query = false;
    for caps in pr_query_re.captures_iter(query) {
        has_pr_query = true;
        let idx = caps.name("idx").expect("Missing idx").as_str();
        let head = caps.name("head").expect("Missing head").as_str();

        let alias = format!("op{}", idx);

        // Find matching PR
        let nodes = if let Some(pr) = mock_state.prs.iter().find(|p| p.head.ref_field == head) {
            vec![serde_json::json!({
                "number": pr.number,
                "id": pr.node_id,
                "title": pr.title,
                "body": pr.body,
                "baseRefName": pr.base.ref_field,
                "state": pr.state,
            })]
        } else {
            vec![]
        };

        response_data.insert(
            alias,
            serde_json::json!({
                "pullRequests": {
                    "nodes": nodes
                }
            }),
        );
    }

    // Process Repo ID Fetch (simple query fallback)
    if !has_pr_query
        && response_data.is_empty()
        && (query.contains("query { repository(owner:")
            || query.contains("repository(owner: $owner, name: $name)"))
    {
        return Ok(Json(serde_json::json!({
            "data": {
                "repository": {
                    "id": "REPO_NODE_ID"
                }
            }
        })));
    }

    write_state(&state.state_path, &mock_state);

    Ok(Json(serde_json::json!({
        "data": response_data
    })))
}

fn read_state(path: &PathBuf) -> MockState {
    if let Ok(content) = fs::read_to_string(path) {
        serde_json::from_str(&content).unwrap_or_default()
    } else {
        MockState::default()
    }
}

pub fn write_state(path: &PathBuf, state: &MockState) {
    let content = serde_json::to_string(state).unwrap();
    fs::write(path, content).unwrap();
}

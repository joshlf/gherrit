use axum::{
    extract::{Path, State},
    http::StatusCode,
    routing::{get, patch},
    Json, Router,
};
use regex::Regex;
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::PathBuf;
use tokio::net::TcpListener;

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
}

/// Starts a mock GitHub API server on a random local port.
///
/// Returns the address of the running server (e.g., `http://127.0.0.1:12345`).
pub async fn start_mock_server(state_path: PathBuf) -> String {
    let app_state = AppState { state_path };

    let app = Router::new()
        .route("/repos/{owner}/{repo}/pulls", get(list_prs).post(create_pr))
        .route("/repos/{owner}/{repo}/pulls/{number}", patch(update_pr))
        .route("/graphql", axum::routing::post(graphql))
        .with_state(app_state);

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let url = format!("http://{}", addr);

    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });

    url
}

/// Handler for `GET /repos/:owner/:repo/pulls`.
/// Returns a list of PRs from the mock state.
async fn list_prs(State(state): State<AppState>) -> Result<Json<Vec<PrEntry>>, StatusCode> {
    let mut mock_state = read_state(&state.state_path);
    if let Some(action) = &mock_state.fail_next_request {
        if action == "list_prs" {
            if mock_state.fail_remaining > 0 {
                mock_state.fail_remaining -= 1;
            }
            if mock_state.fail_remaining == 0 {
                mock_state.fail_next_request = None;
            }
            write_state(&state.state_path, &mock_state);
            return Err(StatusCode::INTERNAL_SERVER_ERROR);
        }
    }
    Ok(Json(mock_state.prs))
}

#[derive(Deserialize)]
struct CreatePrPayload {
    title: String,
    head: String,
    base: String,
    body: Option<String>,
}

/// Handler for `POST /repos/:owner/:repo/pulls`.
/// Creates a new PR in the mock state.
async fn create_pr(
    State(state): State<AppState>,
    Path((owner, repo)): Path<(String, String)>,
    Json(payload): Json<CreatePrPayload>,
) -> Result<Json<PrEntry>, StatusCode> {
    let mut mock_state = read_state(&state.state_path);

    if let Some(action) = &mock_state.fail_next_request {
        if action == "create_pr" {
            if mock_state.fail_remaining > 0 {
                mock_state.fail_remaining -= 1;
            }
            if mock_state.fail_remaining == 0 {
                mock_state.fail_next_request = None;
            }
            write_state(&state.state_path, &mock_state);
            return Err(StatusCode::INTERNAL_SERVER_ERROR);
        }
    }

    let max_id = mock_state.prs.iter().map(|p| p.number).max().unwrap_or(0);
    let num = max_id + 1;
    let html_url = format!("https://github.com/{}/{}/pull/{}", owner, repo, num);
    let api_url = format!(
        "https://api.github.com/repos/{}/{}/pulls/{}",
        owner, repo, num
    );

    let entry = PrEntry {
        id: num as u64,
        number: num,
        html_url: html_url.clone(),
        api_url: api_url.clone(),
        node_id: format!("PR_NODE_{}", num),
        state: "open".to_string(),
        user: User {
            login: "test-user".to_string(),
            id: 1,
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
            subscriptions_url: "https://api.github.com/users/test-user/subscriptions".to_string(),
            organizations_url: "https://api.github.com/users/test-user/orgs".to_string(),
            repos_url: "https://api.github.com/users/test-user/repos".to_string(),
            events_url: "https://api.github.com/users/test-user/events{/privacy}".to_string(),
            received_events_url: "https://api.github.com/users/test-user/received_events"
                .to_string(),
            type_field: "User".to_string(),
            site_admin: false,
        },
        title: Some(payload.title),
        body: payload.body,
        head: RefInfo {
            ref_field: payload.head,
            sha: "0000000000000000000000000000000000000000".to_string(),
        },
        base: RefInfo {
            ref_field: payload.base,
            sha: "0000000000000000000000000000000000000000".to_string(),
        },
        created_at: "2023-01-01T00:00:00Z".to_string(),
        updated_at: "2023-01-01T00:00:00Z".to_string(),
    };

    mock_state.prs.push(entry.clone());
    write_state(&state.state_path, &mock_state);

    Ok(Json(entry))
}

#[derive(Deserialize)]
struct UpdatePrPayload {
    title: Option<String>,
    body: Option<String>,
    base: Option<String>,
}

/// Handler for `PATCH /repos/:owner/:repo/pulls/:number`.
/// Updates an existing PR in the mock state.
async fn update_pr(
    State(state): State<AppState>,
    Path((_owner, _repo, number)): Path<(String, String, usize)>,
    Json(payload): Json<UpdatePrPayload>,
) -> Result<Json<PrEntry>, StatusCode> {
    let mut mock_state = read_state(&state.state_path);

    if let Some(action) = &mock_state.fail_next_request {
        if action == "update_pr" {
            if mock_state.fail_remaining > 0 {
                mock_state.fail_remaining -= 1;
            }
            if mock_state.fail_remaining == 0 {
                mock_state.fail_next_request = None;
            }
            write_state(&state.state_path, &mock_state);
            return Err(StatusCode::INTERNAL_SERVER_ERROR);
        }
    }

    if let Some(pr) = mock_state.prs.iter_mut().find(|p| p.number == number) {
        if let Some(t) = payload.title {
            pr.title = Some(t);
        }
        if let Some(b) = payload.body {
            pr.body = Some(b);
        }
        if let Some(base) = payload.base {
            pr.base.ref_field = base;
        }
        let entry = pr.clone();
        write_state(&state.state_path, &mock_state);
        Ok(Json(entry))
    } else {
        Err(StatusCode::NOT_FOUND)
    }
}

/// Handler for `POST /graphql`.
///
/// Handles batched `updatePullRequest` mutations used by `gherrit` for efficiency.
/// It uses regex to parse aliased mutations (e.g., `update0`, `update1`) from the query string
/// and updates the mock state accordingly.
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

    // Support failure injection for updates (mapped to "update_pr" for compatibility)
    if let Some(action) = &mock_state.fail_next_request {
        if action == "update_pr" || action == "graphql" {
            if mock_state.fail_remaining > 0 {
                mock_state.fail_remaining -= 1;
            }
            if mock_state.fail_remaining == 0 {
                mock_state.fail_next_request = None;
            }
            write_state(&state.state_path, &mock_state);
            return Ok(Json(serde_json::json!({
                "errors": [
                    { "message": "Injected failure" }
                ]
            })));
        }
    }

    // Simple regex to find batched mutations
    // Matches `updateN: updatePullRequest(input: { ... }) {`
    // We use `.+?` non-greedy match for fields until the closing `}) {`.
    let re =
        Regex::new(r#"update(?P<idx>\d+): updatePullRequest\(input: \{(?P<fields>.+?)\}\) \{"#)
            .expect("Regex compilation failed");
    // ... regexes ...
    let title_re = Regex::new(r#"title: (?P<val>"(?:\\.|[^"\\])*")"#).expect("Title regex failed");
    let body_re = Regex::new(r#"body: (?P<val>"(?:\\.|[^"\\])*")"#).expect("Body regex failed");
    let id_re = Regex::new(r#"pullRequestId: "(?P<val>[^"]+)""#).expect("ID regex failed");
    let base_re = Regex::new(r#"baseRefName: "(?P<val>[^"]+)""#).expect("Base regex failed");

    let mut response_data = serde_json::Map::new();

    let matches: Vec<_> = re.captures_iter(query).collect();

    for caps in matches {
        let idx = caps.name("idx").expect("Missing idx capture").as_str();
        let fields = caps
            .name("fields")
            .expect("Missing fields capture")
            .as_str();

        // Extract ID
        let node_id = if let Some(c) = id_re.captures(fields) {
            c.name("val").unwrap().as_str().to_string()
        } else {
            continue;
        };

        // Find PR
        if let Some(pr) = mock_state.prs.iter_mut().find(|p| p.node_id == node_id) {
            if let Some(c) = title_re.captures(fields) {
                // Needs unescaping? serde_json::from_str handles quotes and escapes
                let val_str = c.name("val").unwrap().as_str();
                if let Ok(val) = serde_json::from_str::<String>(val_str) {
                    pr.title = Some(val);
                }
            }
            if let Some(c) = body_re.captures(fields) {
                let val_str = c.name("val").unwrap().as_str();
                if let Ok(val) = serde_json::from_str::<String>(val_str) {
                    pr.body = Some(val);
                }
            }
            if let Some(c) = base_re.captures(fields) {
                let val = c.name("val").unwrap().as_str();
                pr.base.ref_field = val.to_string();
            }

            // Add success response for this alias
            let alias = format!("update{}", idx);
            response_data.insert(
                alias,
                serde_json::json!({
                    "clientMutationId": null,
                    "pullRequest": {
                        "id": pr.node_id
                    }
                }),
            );
        }
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

fn write_state(path: &PathBuf, state: &MockState) {
    let content = serde_json::to_string(state).unwrap();
    fs::write(path, content).unwrap();
}

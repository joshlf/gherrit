use axum::{
    extract::{Path, State},
    http::StatusCode,
    routing::{get, patch},
    Json, Router,
};
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

pub async fn start_mock_server(state_path: PathBuf) -> String {
    let app_state = AppState { state_path };

    let app = Router::new()
        .route("/repos/{owner}/{repo}/pulls", get(list_prs).post(create_pr))
        .route("/repos/{owner}/{repo}/pulls/{number}", patch(update_pr))
        .with_state(app_state);

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let url = format!("http://{}", addr);

    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });

    url
}

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
        node_id: "MDExOlB1bGxSZXF1ZXN0MQ==".to_string(),
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

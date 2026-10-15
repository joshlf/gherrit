//! Literal HTTP exchanges, independent of the stateful GitHub simulator.
use std::{
    collections::VecDeque,
    sync::{mpsc, Arc, Mutex},
    thread,
    time::Duration,
};

use axum::{body::to_bytes, extract::State, http::Request, routing::any, Router};

const STARTUP_TIMEOUT: Duration = Duration::from_secs(10);
const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(10);
const REQUEST_BODY_LIMIT: usize = 1_048_576;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HttpRequest {
    pub method: String,
    pub path: String,
    pub body: Vec<u8>,
}

pub struct Exchange {
    pub request: HttpRequest,
    pub status: u16,
    pub response: Vec<u8>,
}

#[derive(Default)]
struct Script {
    remaining: VecDeque<Exchange>,
    arrivals: Vec<HttpRequest>,
    problems: Vec<String>,
}

pub struct ScriptedHttp {
    pub url: String,
    state: Arc<Mutex<Script>>,
    stop: Option<tokio::sync::oneshot::Sender<()>>,
    done: mpsc::Receiver<()>,
    worker: Option<thread::JoinHandle<()>>,
}

impl ScriptedHttp {
    pub fn start(exchanges: impl IntoIterator<Item = Exchange>) -> Self {
        let state = Arc::new(Mutex::new(Script {
            remaining: exchanges.into_iter().collect(),
            ..Script::default()
        }));
        let (ready_tx, ready_rx) = mpsc::channel();
        let (done_tx, done) = mpsc::channel();
        let (stop, stopped) = tokio::sync::oneshot::channel();
        let server_state = state.clone();
        let worker = thread::spawn(move || {
            tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap().block_on(
                async move {
                    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
                    ready_tx.send(format!("http://{}", listener.local_addr().unwrap())).unwrap();
                    let router = Router::new()
                        .route("/", any(exchange))
                        .route("/{*path}", any(exchange))
                        .with_state(server_state);
                    axum::serve(listener, router)
                        .with_graceful_shutdown(async {
                            let _ = stopped.await;
                        })
                        .await
                        .unwrap();
                },
            );
            let _ = done_tx.send(());
        });
        let url = ready_rx.recv_timeout(STARTUP_TIMEOUT).expect("scripted HTTP startup failed");
        Self { url, state, stop: Some(stop), done, worker: Some(worker) }
    }

    pub fn arrivals(&self) -> Vec<HttpRequest> {
        self.state.lock().unwrap().arrivals.clone()
    }
}

impl Drop for ScriptedHttp {
    fn drop(&mut self) {
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
        let completed = self.done.recv_timeout(SHUTDOWN_TIMEOUT).is_ok();
        let joined = completed && self.worker.take().unwrap().join().is_ok();
        let state = self.state.lock().unwrap();
        if !thread::panicking() {
            assert!(joined, "scripted HTTP server did not complete normally");
            assert!(state.problems.is_empty(), "HTTP contract violations: {:?}", state.problems);
            assert!(
                state.remaining.is_empty(),
                "{} HTTP exchanges were not consumed",
                state.remaining.len()
            );
        }
    }
}

async fn exchange(
    State(state): State<Arc<Mutex<Script>>>,
    request: Request<axum::body::Body>,
) -> (axum::http::StatusCode, Vec<u8>) {
    let (parts, body) = request.into_parts();
    let bytes = to_bytes(body, REQUEST_BODY_LIMIT).await;
    let mut state = state.lock().unwrap();
    let request = HttpRequest {
        method: parts.method.to_string(),
        path: parts.uri.path_and_query().map_or("/", |path| path.as_str()).to_string(),
        body: bytes.as_ref().map_or_else(|_| Vec::new(), |bytes| bytes.to_vec()),
    };
    // Record the arrival even when the body cannot be acquired. No JSON or
    // application/schema decoder stands in front of this recorder.
    state.arrivals.push(request.clone());
    if let Err(error) = bytes {
        state.problems.push(format!("request body could not be read: {error}"));
        return (axum::http::StatusCode::BAD_REQUEST, Vec::new());
    }
    let Some(expected) = state.remaining.pop_front() else {
        state.problems.push(format!("unexpected HTTP request: {request:?}"));
        return (axum::http::StatusCode::BAD_REQUEST, Vec::new());
    };
    if request != expected.request {
        state.problems.push(format!("expected {:?}, received {request:?}", expected.request));
        return (axum::http::StatusCode::BAD_REQUEST, Vec::new());
    }
    (axum::http::StatusCode::from_u16(expected.status).unwrap(), expected.response)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[should_panic(expected = "HTTP exchanges were not consumed")]
    fn unused_scripted_responses_fail_the_contract() {
        let _server = ScriptedHttp::start([Exchange {
            request: HttpRequest {
                method: "POST".to_string(),
                path: "/graphql".to_string(),
                body: vec![],
            },
            status: 200,
            response: b"{}".to_vec(),
        }]);
    }
}

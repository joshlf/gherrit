use std::sync::{Arc, RwLock};

use axum::{extract::State as AxumState, http::StatusCode, routing::post, Json, Router};
use serde::{Deserialize, Serialize};

use crate::{mock_server::MockState, FailureKind, GitOperation};

#[derive(Debug, Clone, Default)]
pub(super) struct State {
    pushes: Vec<Push>,
}

impl State {
    pub(super) fn pushes(&self) -> &[Push] {
        &self.pushes
    }

    fn record_push(&mut self, args: Vec<String>, exit_code: i32) {
        self.pushes.push(Push { args, exit_code });
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Push {
    args: Vec<String>,
    exit_code: i32,
}

impl Push {
    pub(super) fn arguments(&self) -> &[String] {
        &self.args
    }

    pub(super) fn exit_code(&self) -> i32 {
        self.exit_code
    }
}

#[derive(Clone)]
struct HandlerState {
    shared: Arc<RwLock<MockState>>,
}

#[derive(Deserialize)]
struct GitRequest {
    args: Vec<String>,
}

#[derive(Serialize)]
struct GitResponse {
    stdout: String,
    stderr: String,
    exit_code: i32,
    passthrough: bool,
    report_exit_status: bool,
}

#[derive(Deserialize)]
struct GitCompletion {
    args: Vec<String>,
    exit_code: i32,
}

pub(super) fn routes(shared: Arc<RwLock<MockState>>) -> Router {
    Router::new()
        .route("/_internal/git", post(handle_git))
        .route("/_internal/git/complete", post(complete_git))
        .with_state(HandlerState { shared })
}

impl GitOperation {
    fn from_subcommand(subcommand: &str) -> Option<Self> {
        match subcommand {
            "var" => Some(Self::Var),
            "interpret-trailers" => Some(Self::InterpretTrailers),
            "ls-remote" => Some(Self::LsRemote),
            _ => None,
        }
    }

    fn subcommand(self) -> &'static str {
        match self {
            Self::Var => "var",
            Self::InterpretTrailers => "interpret-trailers",
            Self::LsRemote => "ls-remote",
        }
    }
}

fn check_and_apply_failure(
    mock_state: &mut MockState,
    operation: GitOperation,
) -> Option<GitOperation> {
    let FailureKind::Git { operation: expected, matching_calls_before_failure } =
        mock_state.faults.front_mut()?
    else {
        return None;
    };
    if *expected != operation {
        return None;
    }
    if *matching_calls_before_failure > 0 {
        *matching_calls_before_failure -= 1;
        return None;
    }

    let FailureKind::Git { operation: consumed, .. } = mock_state.faults.pop_front().unwrap()
    else {
        unreachable!("the front fault was just matched as a Git fault")
    };
    Some(consumed)
}

/// Returns the subcommand from the command shape emitted by GHerrit.
///
/// The harness intentionally does not emulate Git's global-option grammar.
/// Production calls put the subcommand immediately after the executable and
/// the interceptor treats every subsequent argument as opaque.
fn subcommand(args: &[String]) -> Option<&str> {
    args.get(1).filter(|argument| !argument.starts_with('-')).map(String::as_str)
}

async fn handle_git(
    AxumState(handler): AxumState<HandlerState>,
    Json(request): Json<GitRequest>,
) -> Json<GitResponse> {
    let subcommand = subcommand(&request.args);
    let is_push = subcommand == Some("push");

    let failure = subcommand.and_then(GitOperation::from_subcommand).and_then(|operation| {
        check_and_apply_failure(&mut handler.shared.write().unwrap(), operation)
    });
    if let Some(operation) = failure {
        return Json(GitResponse {
            stdout: String::new(),
            stderr: format!("Simulated failure for git {}", operation.subcommand()),
            exit_code: 1,
            passthrough: false,
            report_exit_status: false,
        });
    }

    if is_push {
        let state = handler.shared.read().unwrap();
        let stderr = format!(
            "remote: \nremote: Create a pull request for 'feature' on GitHub by visiting:\nremote:      https://github.com/{}/{}/pull/new/feature\nremote: \n",
            state.repo_owner, state.repo_name
        );
        return Json(GitResponse {
            stdout: String::new(),
            stderr,
            exit_code: 0,
            passthrough: true,
            report_exit_status: true,
        });
    }

    Json(GitResponse {
        stdout: String::new(),
        stderr: String::new(),
        exit_code: 0,
        passthrough: true,
        report_exit_status: false,
    })
}

async fn complete_git(
    AxumState(handler): AxumState<HandlerState>,
    Json(completion): Json<GitCompletion>,
) -> StatusCode {
    if subcommand(&completion.args) != Some("push") {
        return StatusCode::BAD_REQUEST;
    }

    record_push(&handler.shared, completion.args, completion.exit_code);
    StatusCode::NO_CONTENT
}

fn record_push(shared: &RwLock<MockState>, args: Vec<String>, exit_code: i32) {
    assert_eq!(subcommand(&args), Some("push"), "record_push requires a Git push");
    shared.write().unwrap().git.record_push(args, exit_code);
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;

    use super::*;

    fn args(values: &[&str]) -> Vec<String> {
        values.iter().map(|value| (*value).to_string()).collect()
    }

    fn handler_state() -> HandlerState {
        HandlerState {
            shared: Arc::new(RwLock::new(MockState::new("owner".to_string(), "repo".to_string()))),
        }
    }

    #[test]
    fn recognizes_only_the_product_command_shape() {
        assert_eq!(subcommand(&args(&["git", "push", "origin"])), Some("push"));
        assert_eq!(subcommand(&args(&["git", "ls-remote", "origin"])), Some("ls-remote"));
        assert_eq!(subcommand(&args(&["git"])), None);
        assert_eq!(subcommand(&args(&["git", "--version"])), None);
        assert_eq!(subcommand(&args(&["git", "-c", "push", "origin"])), None);
    }

    #[test]
    fn does_not_find_push_in_opaque_arguments() {
        assert_eq!(subcommand(&args(&["git", "show", "push"])), Some("show"));
    }

    #[test]
    fn recognizes_typed_git_operations() {
        for (subcommand, expected) in [
            ("var", Some(GitOperation::Var)),
            ("interpret-trailers", Some(GitOperation::InterpretTrailers)),
            ("ls-remote", Some(GitOperation::LsRemote)),
            ("push", None),
            ("status", None),
        ] {
            assert_eq!(GitOperation::from_subcommand(subcommand), expected);
        }
    }

    #[test]
    fn git_faults_match_in_script_order() {
        let expected = VecDeque::from([
            FailureKind::Git { operation: GitOperation::Var, matching_calls_before_failure: 0 },
            FailureKind::Git {
                operation: GitOperation::LsRemote,
                matching_calls_before_failure: 0,
            },
        ]);
        let mut state = MockState { faults: expected.clone(), ..Default::default() };

        assert_eq!(check_and_apply_failure(&mut state, GitOperation::LsRemote), None);
        assert_eq!(state.faults, expected);
        assert_eq!(check_and_apply_failure(&mut state, GitOperation::Var), Some(GitOperation::Var));
        assert_eq!(
            check_and_apply_failure(&mut state, GitOperation::LsRemote),
            Some(GitOperation::LsRemote)
        );
        assert!(state.faults.is_empty());

        let expected = VecDeque::from([
            FailureKind::CreatePr,
            FailureKind::Git {
                operation: GitOperation::InterpretTrailers,
                matching_calls_before_failure: 0,
            },
        ]);
        let mut state = MockState { faults: expected.clone(), ..Default::default() };
        assert_eq!(check_and_apply_failure(&mut state, GitOperation::InterpretTrailers), None);
        assert_eq!(state.faults, expected);
    }

    #[test]
    fn git_faults_can_skip_matching_calls() {
        let mut state = MockState {
            faults: VecDeque::from([FailureKind::Git {
                operation: GitOperation::LsRemote,
                matching_calls_before_failure: 1,
            }]),
            ..Default::default()
        };

        assert_eq!(check_and_apply_failure(&mut state, GitOperation::LsRemote), None);
        assert_eq!(
            check_and_apply_failure(&mut state, GitOperation::LsRemote),
            Some(GitOperation::LsRemote)
        );
        assert!(state.faults.is_empty());
    }

    #[tokio::test]
    async fn records_completed_push() {
        let handler = handler_state();
        let invocation = args(&["git", "push", "origin", "+HEAD:refs/heads/main"]);
        let request = GitRequest { args: invocation.clone() };

        let Json(response) = handle_git(AxumState(handler.clone()), Json(request)).await;
        assert!(response.passthrough);
        assert!(response.report_exit_status);
        assert!(handler.shared.read().unwrap().git.pushes.is_empty());

        let status = complete_git(
            AxumState(handler.clone()),
            Json(GitCompletion { args: invocation.clone(), exit_code: 0 }),
        )
        .await;
        assert_eq!(status, StatusCode::NO_CONTENT);
        assert_eq!(
            handler.shared.read().unwrap().git.pushes,
            [Push { args: invocation, exit_code: 0 }]
        );
    }

    #[tokio::test]
    async fn failure_injection_uses_product_subcommand() {
        let handler = handler_state();
        handler.shared.write().unwrap().faults.push_back(FailureKind::Git {
            operation: GitOperation::Var,
            matching_calls_before_failure: 0,
        });
        let request = GitRequest { args: args(&["git", "var", "GIT_COMMITTER_IDENT"]) };

        let Json(response) = handle_git(AxumState(handler.clone()), Json(request)).await;
        assert!(!response.passthrough);
        assert!(!response.report_exit_status);
        assert_eq!(response.exit_code, 1);
        assert_eq!(response.stderr, "Simulated failure for git var");
        assert!(handler.shared.read().unwrap().faults.is_empty());
        assert!(handler.shared.read().unwrap().git.pushes.is_empty());
    }

    #[test]
    fn records_pushes_without_interpreting_git_syntax() {
        for tail in [
            &["origin", ":"][..],
            &["origin", "tag", "v1"][..],
            &["-o", "refs/heads/not-a-refspec", "origin", "HEAD:refs/heads/main"][..],
        ] {
            let handler = handler_state();
            let invocation = std::iter::once("git")
                .chain(std::iter::once("push"))
                .chain(tail.iter().copied())
                .map(ToString::to_string)
                .collect::<Vec<_>>();

            record_push(&handler.shared, invocation.clone(), 0);

            assert_eq!(
                handler.shared.read().unwrap().git.pushes,
                [Push { args: invocation, exit_code: 0 }]
            );
        }
    }
}

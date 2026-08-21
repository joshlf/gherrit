use std::{
    collections::VecDeque,
    path::PathBuf,
    sync::{Arc, RwLock},
};

use axum::{extract::State as AxumState, http::StatusCode, routing::post, Json, Router};
use serde::{Deserialize, Serialize};

use crate::{mock_server::MockState, FailureKind, GitOperation, TestEnvironment};

#[derive(Debug, Clone, Default)]
pub(super) struct State {
    pushes: Vec<Push>,
    invocations: Vec<(GitOperation, Vec<String>)>,
    before_push_updates: VecDeque<RemoteRefUpdate>,
}

impl State {
    pub(super) fn pushes(&self) -> &[Push] {
        &self.pushes
    }

    fn record_push(&mut self, args: Vec<String>, exit_code: i32) {
        self.pushes.push(Push { args, exit_code });
    }

    pub(super) fn invocations(&self, operation: GitOperation) -> Vec<Vec<String>> {
        self.invocations
            .iter()
            .filter(|(candidate, _)| *candidate == operation)
            .map(|(_, arguments)| arguments.clone())
            .collect()
    }

    fn record_invocation(&mut self, operation: GitOperation, args: Vec<String>) {
        self.invocations.push((operation, args));
    }

    pub(super) fn schedule_remote_ref_update(&mut self, update: RemoteRefUpdate) {
        self.before_push_updates.push_back(update);
    }

    pub(super) fn pending_remote_ref_updates(&self) -> &VecDeque<RemoteRefUpdate> {
        &self.before_push_updates
    }

    fn take_remote_ref_update(&mut self) -> Option<RemoteRefUpdate> {
        self.before_push_updates.pop_front()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct RemoteRefUpdate {
    pub(super) ref_name: String,
    pub(super) target: String,
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
    remote: Option<RemoteRepository>,
}

#[derive(Clone)]
struct RemoteRepository {
    path: PathBuf,
    system_git: PathBuf,
    environment: TestEnvironment,
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

pub(super) fn routes(
    shared: Arc<RwLock<MockState>>,
    remote_path: PathBuf,
    system_git: PathBuf,
    environment: TestEnvironment,
) -> Router {
    Router::new()
        .route("/_internal/git", post(handle_git))
        .route("/_internal/git/complete", post(complete_git))
        .with_state(HandlerState {
            shared,
            remote: Some(RemoteRepository { path: remote_path, system_git, environment }),
        })
}

impl GitOperation {
    fn from_subcommand(subcommand: &str) -> Option<Self> {
        match subcommand {
            "var" => Some(Self::Var),
            "interpret-trailers" => Some(Self::InterpretTrailers),
            _ => None,
        }
    }

    fn from_args(args: &[String]) -> Option<Self> {
        match subcommand(args)? {
            "config" if args.iter().any(|argument| argument == "--get-urlmatch") => {
                Some(Self::HttpRedirectPolicy)
            }
            "ls-remote" if args.iter().any(|argument| argument == "--get-url") => {
                Some(Self::LsRemoteUrl)
            }
            "ls-remote" if args.iter().any(|argument| argument == "--symref") => {
                Some(Self::LsRemoteDefaultBranch)
            }
            "ls-remote"
                if args.iter().any(|argument| argument.starts_with("refs/tags/gherrit/")) =>
            {
                Some(Self::LsRemoteActiveVersions)
            }
            "ls-remote" => Some(Self::LsRemoteManagedBranches),
            subcommand => Self::from_subcommand(subcommand),
        }
    }

    fn subcommand(self) -> &'static str {
        match self {
            Self::Var => "var",
            Self::InterpretTrailers => "interpret-trailers",
            Self::HttpRedirectPolicy => "config --get-urlmatch",
            Self::LsRemoteUrl
            | Self::LsRemoteDefaultBranch
            | Self::LsRemoteManagedBranches
            | Self::LsRemoteActiveVersions => "ls-remote",
        }
    }
}

fn check_and_apply_failure(
    mock_state: &mut MockState,
    operation: GitOperation,
) -> Option<FailureKind> {
    let matches = match mock_state.faults.front()? {
        FailureKind::Git(expected) => *expected == operation,
        FailureKind::GitOutput { operation: expected, .. } => *expected == operation,
        _ => false,
    };
    if !matches {
        return None;
    }
    mock_state.faults.pop_front()
}

/// Returns the subcommand from the command shape emitted by GHerrit.
///
/// The harness recognizes the global options required by production, but does
/// not emulate Git's general global-option grammar. Direct fixture commands
/// still use the ordinary `git <subcommand>` shape.
fn subcommand(args: &[String]) -> Option<&str> {
    let mut arguments = args.iter().skip(1).map(String::as_str);
    if arguments.next()? != "--no-replace-objects" {
        return args.get(1).map(String::as_str).filter(|argument| !argument.starts_with('-'));
    }

    loop {
        match arguments.next()? {
            "-c" => {
                arguments.next()?;
            }
            argument if argument.starts_with("--config-env=") => {}
            subcommand if !subcommand.starts_with('-') => return Some(subcommand),
            _ => return None,
        }
    }
}

async fn handle_git(
    AxumState(handler): AxumState<HandlerState>,
    Json(request): Json<GitRequest>,
) -> Json<GitResponse> {
    let subcommand = subcommand(&request.args);
    let is_push = subcommand == Some("push");

    let operation = GitOperation::from_args(&request.args);
    if let Some(operation) = operation {
        handler.shared.write().unwrap().git.record_invocation(operation, request.args.clone());
    }
    let failure = operation.and_then(|operation| {
        check_and_apply_failure(&mut handler.shared.write().unwrap(), operation)
    });
    match failure {
        Some(FailureKind::Git(operation)) => {
            return Json(GitResponse {
                stdout: String::new(),
                stderr: format!("Simulated failure for git {}", operation.subcommand()),
                exit_code: 1,
                passthrough: false,
                report_exit_status: false,
            });
        }
        Some(FailureKind::GitOutput { stdout, .. }) => {
            return Json(GitResponse {
                stdout: stdout.to_owned(),
                stderr: String::new(),
                exit_code: 0,
                passthrough: false,
                report_exit_status: false,
            });
        }
        Some(_) => unreachable!("only Git failures are matched by the Git interceptor"),
        None => {}
    }

    if is_push {
        let update = handler.shared.write().unwrap().git.take_remote_ref_update();
        if let Some(update) = update {
            let remote = handler
                .remote
                .as_ref()
                .expect("scheduled remote update requires a remote repository");
            remote
                .environment
                .command(&remote.system_git)
                .current_dir(&remote.path)
                .arg("update-ref")
                .arg(update.ref_name)
                .arg(update.target)
                .assert()
                .success();
        }
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
            remote: None,
        }
    }

    #[test]
    fn recognizes_product_and_direct_fixture_command_shapes() {
        assert_eq!(subcommand(&args(&["git", "push", "origin"])), Some("push"));
        assert_eq!(
            subcommand(&args(&["git", "--no-replace-objects", "push", "origin"])),
            Some("push")
        );
        assert_eq!(
            subcommand(&args(&[
                "git",
                "--no-replace-objects",
                "--config-env=remote.gherrit-publication.url=GHERRIT_PRIVATE_PUSH_DESTINATION",
                "--config-env=remote.gherrit-publication.pushurl=GHERRIT_PRIVATE_PUSH_DESTINATION",
                "-c",
                "http.followRedirects=false",
                "push",
                "gherrit-publication",
            ])),
            Some("push")
        );
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
            ("ls-remote", None),
            ("push", None),
            ("status", None),
        ] {
            assert_eq!(GitOperation::from_subcommand(subcommand), expected);
        }

        for (arguments, expected) in [
            (
                &[
                    "git",
                    "--no-replace-objects",
                    "-c",
                    "http.followRedirects=false",
                    "config",
                    "--bool",
                    "--get-urlmatch",
                ][..],
                Some(GitOperation::HttpRedirectPolicy),
            ),
            (
                &["git", "--no-replace-objects", "ls-remote", "--get-url"][..],
                Some(GitOperation::LsRemoteUrl),
            ),
            (
                &["git", "--no-replace-objects", "ls-remote", "--symref"][..],
                Some(GitOperation::LsRemoteDefaultBranch),
            ),
            (
                &["git", "--no-replace-objects", "ls-remote", "refs/heads/Gone"][..],
                Some(GitOperation::LsRemoteManagedBranches),
            ),
            (
                &["git", "--no-replace-objects", "ls-remote", "refs/tags/gherrit/Gone/*"][..],
                Some(GitOperation::LsRemoteActiveVersions),
            ),
        ] {
            assert_eq!(GitOperation::from_args(&args(arguments)), expected);
        }
    }

    #[test]
    fn git_faults_match_in_script_order() {
        let expected = VecDeque::from([
            FailureKind::Git(GitOperation::Var),
            FailureKind::Git(GitOperation::LsRemoteDefaultBranch),
        ]);
        let mut state = MockState { faults: expected.clone(), ..Default::default() };

        assert_eq!(check_and_apply_failure(&mut state, GitOperation::LsRemoteDefaultBranch), None);
        assert_eq!(state.faults, expected);
        assert_eq!(
            check_and_apply_failure(&mut state, GitOperation::Var),
            Some(FailureKind::Git(GitOperation::Var))
        );
        assert_eq!(
            check_and_apply_failure(&mut state, GitOperation::LsRemoteDefaultBranch),
            Some(FailureKind::Git(GitOperation::LsRemoteDefaultBranch))
        );
        assert!(state.faults.is_empty());

        let expected = VecDeque::from([
            FailureKind::CreatePr,
            FailureKind::Git(GitOperation::InterpretTrailers),
        ]);
        let mut state = MockState { faults: expected.clone(), ..Default::default() };
        assert_eq!(check_and_apply_failure(&mut state, GitOperation::InterpretTrailers), None);
        assert_eq!(state.faults, expected);
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
        handler.shared.write().unwrap().faults.push_back(FailureKind::Git(GitOperation::Var));
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

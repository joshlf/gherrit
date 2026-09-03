use std::{
    collections::VecDeque,
    path::PathBuf,
    sync::{Arc, RwLock},
};

use axum::{extract::State as AxumState, http::StatusCode, routing::post, Json, Router};
use serde::{Deserialize, Serialize};

use crate::{
    mock_server::MockState, FailureKind, GitOperation, PublicationOverlapSchedule,
    PublicationPushStage, TestEnvironment,
};

#[derive(Debug, Clone, Default)]
pub(super) struct State {
    pushes: Vec<Push>,
    operations: Vec<GitOperation>,
    remote_ref_updates_before_push: VecDeque<RemoteRefUpdate>,
}

impl State {
    pub(super) fn pushes(&self) -> &[Push] {
        &self.pushes
    }

    pub(super) fn operations(&self) -> &[GitOperation] {
        &self.operations
    }

    fn record_operation(&mut self, operation: GitOperation) {
        self.operations.push(operation);
    }

    fn record_push(&mut self, args: Vec<String>, exit_code: i32) {
        self.pushes.push(Push { args, exit_code });
    }

    pub(super) fn update_remote_ref_before_push(&mut self, ref_name: String, target: String) {
        self.remote_ref_updates_before_push.push_back(RemoteRefUpdate { ref_name, target });
    }

    fn take_remote_ref_update_before_push(&mut self) -> Option<RemoteRefUpdate> {
        self.remote_ref_updates_before_push.pop_front()
    }

    pub(super) fn pending_remote_ref_updates(&self) -> usize {
        self.remote_ref_updates_before_push.len()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct RemoteRefUpdate {
    ref_name: String,
    target: String,
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
    remote_path: PathBuf,
    system_git: PathBuf,
    test_environment: TestEnvironment,
    publication_overlap: Option<Arc<PublicationOverlapSchedule>>,
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
    suppress_stdout: bool,
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
    test_environment: TestEnvironment,
    publication_overlap: Option<Arc<PublicationOverlapSchedule>>,
) -> Router {
    Router::new()
        .route("/_internal/git", post(handle_git))
        .route("/_internal/git/complete", post(complete_git))
        .with_state(HandlerState {
            shared,
            remote_path,
            system_git,
            test_environment,
            publication_overlap,
        })
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

fn fallible_operation(args: &[String]) -> Option<GitOperation> {
    let operation = subcommand(args).and_then(GitOperation::from_subcommand)?;
    // `ls-remote --get-url` only asks Git to resolve configuration; it does
    // not contact the remote. A simulated remote-operation failure therefore
    // belongs to the first network-bearing `ls-remote`, not this local probe.
    (operation != GitOperation::LsRemote || !args.iter().any(|argument| argument == "--get-url"))
        .then_some(operation)
}

fn check_and_apply_failure(
    mock_state: &mut MockState,
    operation: GitOperation,
) -> Option<GitOperation> {
    let FailureKind::Git(expected) = mock_state.faults.front()? else {
        return None;
    };
    if *expected != operation {
        return None;
    }

    let FailureKind::Git(consumed) = mock_state.faults.pop_front().unwrap() else {
        unreachable!("the front fault was just matched as a Git fault")
    };
    Some(consumed)
}

/// Returns the subcommand from the command shape emitted by GHerrit.
///
/// The harness recognizes the small global-option grammar emitted by
/// production, but intentionally does not emulate Git's general option
/// grammar. Direct fixture commands still use the ordinary `git <subcommand>`
/// shape.
fn subcommand_index(args: &[String]) -> Option<usize> {
    let first = args.get(1)?;
    if first != "--no-replace-objects" {
        return (!first.starts_with('-')).then_some(1);
    }

    let mut index = 2;
    loop {
        match args.get(index)?.as_str() {
            "-c" => {
                args.get(index + 1)?;
                index += 2;
            }
            argument if argument.starts_with("--config-env=") => index += 1,
            subcommand if !subcommand.starts_with('-') => return Some(index),
            _ => return None,
        }
    }
}

fn subcommand(args: &[String]) -> Option<&str> {
    subcommand_index(args).map(|index| args[index].as_str())
}

/// Recognizes the destination-bound push shape emitted by GHerrit.
///
/// Faults scheduled at the publication boundary must not be consumed by a
/// fixture push or by the user's enclosing push through an installed hook.
fn publication_push_stage(args: &[String]) -> Option<PublicationPushStage> {
    let index = subcommand_index(args).filter(|index| args[*index] == "push")?;
    let tail = &args[index + 1..];
    let separator = tail.iter().position(|argument| argument == "--")?;
    let remote = tail.get(separator + 1)?;
    let suffix = remote.strip_prefix("gherrit-publication")?;
    if !suffix.is_empty()
        && !suffix.strip_prefix('-').is_some_and(|digits| {
            !digits.is_empty() && digits.bytes().all(|byte| byte.is_ascii_digit())
        })
    {
        return None;
    }

    let url = format!("--config-env=remote.{remote}.url=");
    let pushurl = format!("--config-env=remote.{remote}.pushurl=");
    if !args[..index].iter().any(|argument| argument.starts_with(&url))
        || !args[..index].iter().any(|argument| argument.starts_with(&pushurl))
    {
        return None;
    }

    let creates_marker = tail[separator + 2..].iter().any(|refspec| {
        let refspec = refspec.strip_prefix('+').unwrap_or(refspec);
        let Some((source, destination)) = refspec.split_once(':') else { return false };
        !source.is_empty()
            && destination
                .strip_prefix("refs/tags/gherrit/")
                .and_then(|path| path.strip_suffix("/pr"))
                .is_some_and(|id| !id.is_empty())
    });
    Some(if creates_marker { PublicationPushStage::Marker } else { PublicationPushStage::Initial })
}

fn take_publication_receipt_fault(mock_state: &mut MockState, stage: PublicationPushStage) -> bool {
    let Some(FailureKind::LosePublicationPushReceipt(expected)) = mock_state.faults.front() else {
        return false;
    };
    if *expected != stage {
        return false;
    }
    let Some(FailureKind::LosePublicationPushReceipt(consumed)) = mock_state.faults.pop_front()
    else {
        unreachable!("the front fault was just matched as a publication-receipt fault")
    };
    debug_assert_eq!(consumed, stage);
    true
}

async fn handle_git(
    AxumState(handler): AxumState<HandlerState>,
    Json(request): Json<GitRequest>,
) -> Json<GitResponse> {
    let subcommand = subcommand(&request.args);
    let is_push = subcommand == Some("push");
    let publication_stage = publication_push_stage(&request.args);
    let overlap_marker = publication_stage == Some(PublicationPushStage::Marker)
        && handler.publication_overlap.as_ref().is_some_and(|overlap| overlap.claim_marker());
    if overlap_marker {
        let overlap = handler.publication_overlap.as_ref().unwrap();
        // The passthrough Git child has not started yet. Releasing both
        // publishers from this external gate makes the absence-lease race the
        // only nondeterministic winner, without holding mock state.
        if !overlap.before_marker_push().await {
            return Json(GitResponse {
                stdout: String::new(),
                stderr: "Publication overlap schedule was cancelled".to_string(),
                exit_code: 1,
                passthrough: false,
                report_exit_status: false,
                suppress_stdout: false,
            });
        }
    }
    let suppress_stdout = publication_stage.is_some_and(|stage| {
        take_publication_receipt_fault(&mut handler.shared.write().unwrap(), stage)
    });

    let failure = fallible_operation(&request.args).and_then(|operation| {
        let mut state = handler.shared.write().unwrap();
        state.git.record_operation(operation);
        check_and_apply_failure(&mut state, operation)
    });
    if let Some(operation) = failure {
        return Json(GitResponse {
            stdout: String::new(),
            stderr: format!("Simulated failure for git {}", operation.subcommand()),
            exit_code: 1,
            passthrough: false,
            report_exit_status: false,
            suppress_stdout: false,
        });
    }

    if is_push {
        let update = publication_stage
            .is_some()
            .then(|| handler.shared.write().unwrap().git.take_remote_ref_update_before_push())
            .flatten();
        if let Some(RemoteRefUpdate { ref_name, target }) = update {
            let output = handler
                .test_environment
                .command(&handler.system_git)
                .arg("--git-dir")
                .arg(&handler.remote_path)
                .args(["update-ref", &ref_name, &target])
                .output()
                .expect("failed to apply scheduled remote ref update");
            assert!(
                output.status.success(),
                "scheduled remote ref update failed: {}",
                String::from_utf8_lossy(&output.stderr)
            );
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
            suppress_stdout,
        });
    }

    Json(GitResponse {
        stdout: String::new(),
        stderr: String::new(),
        exit_code: 0,
        passthrough: true,
        report_exit_status: false,
        suppress_stdout: false,
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
        let temporary = tempfile::tempdir().unwrap();
        let system_git = PathBuf::from("git");
        HandlerState {
            shared: Arc::new(RwLock::new(MockState::new("owner".to_string(), "repo".to_string()))),
            remote_path: temporary.path().to_path_buf(),
            test_environment: TestEnvironment::new(temporary.path(), &system_git),
            system_git,
            publication_overlap: None,
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
                "--config-env=remote.gherrit-publication.url=GHERRIT_REMOTE_URL",
                "--config-env=remote.gherrit-publication.pushurl=GHERRIT_REMOTE_URL",
                "-c",
                "http.followRedirects=false",
                "-c",
                "push.followTags=false",
                "-c",
                "push.recurseSubmodules=no",
                "-c",
                "push.pushOption=",
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
    fn classifies_only_destination_bound_internal_publication_pushes() {
        let publication = args(&[
            "git",
            "--no-replace-objects",
            "--config-env=remote.gherrit-publication-2.url=DESTINATION",
            "--config-env=remote.gherrit-publication-2.pushurl=DESTINATION",
            "-c",
            "push.followTags=false",
            "push",
            "--atomic",
            "--",
            "gherrit-publication-2",
            "HEAD:refs/heads/Gone",
        ]);
        assert_eq!(publication_push_stage(&publication), Some(PublicationPushStage::Initial));

        let mut marker = publication.clone();
        *marker.last_mut().unwrap() = "MARKER:refs/tags/gherrit/Gone/pr".to_string();
        assert_eq!(publication_push_stage(&marker), Some(PublicationPushStage::Marker));

        for not_marker in [
            "MARKER:refs/tags/gherrit/Gone/pr/extra",
            ":refs/tags/gherrit/Gone/pr",
            "MARKER:refs/tags/gherrit//pr",
        ] {
            let mut invocation = publication.clone();
            *invocation.last_mut().unwrap() = not_marker.to_string();
            assert_eq!(
                publication_push_stage(&invocation),
                Some(PublicationPushStage::Initial),
                "refspec: {not_marker}"
            );
        }

        for ordinary in [
            args(&["git", "push", "origin", "HEAD:refs/heads/feature"]),
            args(&[
                "git",
                "--no-replace-objects",
                "--config-env=remote.gherrit-publication-9-extra.url=DESTINATION",
                "--config-env=remote.gherrit-publication-9-extra.pushurl=DESTINATION",
                "push",
                "--",
                "gherrit-publication-9-extra",
            ]),
            args(&[
                "git",
                "--no-replace-objects",
                "--config-env=remote.gherrit-publication.url=DESTINATION",
                "push",
                "--",
                "gherrit-publication",
            ]),
        ] {
            assert_eq!(publication_push_stage(&ordinary), None, "ordinary push: {ordinary:?}");
        }
    }

    #[test]
    fn publication_receipt_faults_match_stage_and_queue_order() {
        let expected = VecDeque::from([
            FailureKind::LosePublicationPushReceipt(PublicationPushStage::Marker),
            FailureKind::LosePublicationPushReceipt(PublicationPushStage::Initial),
        ]);
        let mut state = MockState { faults: expected.clone(), ..Default::default() };

        assert!(!take_publication_receipt_fault(&mut state, PublicationPushStage::Initial));
        assert_eq!(state.faults, expected);
        assert!(take_publication_receipt_fault(&mut state, PublicationPushStage::Marker));
        assert!(take_publication_receipt_fault(&mut state, PublicationPushStage::Initial));
        assert!(state.faults.is_empty());

        let expected = VecDeque::from([
            FailureKind::CreatePr,
            FailureKind::LosePublicationPushReceipt(PublicationPushStage::Initial),
        ]);
        let mut state = MockState { faults: expected.clone(), ..Default::default() };
        assert!(!take_publication_receipt_fault(&mut state, PublicationPushStage::Initial));
        assert_eq!(state.faults, expected);
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

        assert_eq!(
            fallible_operation(&args(&[
                "git",
                "--no-replace-objects",
                "ls-remote",
                "--get-url",
                "--",
                "gherrit-publication",
            ])),
            None
        );
        assert_eq!(
            fallible_operation(&args(&[
                "git",
                "--no-replace-objects",
                "ls-remote",
                "--symref",
                "--",
                "gherrit-publication",
                "HEAD",
            ])),
            Some(GitOperation::LsRemote)
        );
    }

    #[test]
    fn git_faults_match_in_script_order() {
        let expected = VecDeque::from([
            FailureKind::Git(GitOperation::Var),
            FailureKind::Git(GitOperation::LsRemote),
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
        assert!(!response.suppress_stdout);
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
    async fn receipt_loss_runs_and_records_the_matching_publication_push() {
        let handler = handler_state();
        handler
            .shared
            .write()
            .unwrap()
            .faults
            .push_back(FailureKind::LosePublicationPushReceipt(PublicationPushStage::Marker));
        let invocation = args(&[
            "git",
            "--no-replace-objects",
            "--config-env=remote.gherrit-publication.url=DESTINATION",
            "--config-env=remote.gherrit-publication.pushurl=DESTINATION",
            "push",
            "--",
            "gherrit-publication",
            "MARKER:refs/tags/gherrit/Gone/pr",
        ]);

        let Json(response) =
            handle_git(AxumState(handler.clone()), Json(GitRequest { args: invocation.clone() }))
                .await;
        assert!(response.passthrough);
        assert!(response.report_exit_status);
        assert!(response.suppress_stdout);
        assert!(handler.shared.read().unwrap().faults.is_empty());

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
        assert!(!response.suppress_stdout);
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

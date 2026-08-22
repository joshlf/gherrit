use std::{
    collections::{HashSet, VecDeque},
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
    remote_ref_transactions: VecDeque<ScheduledRemoteRefTransaction>,
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

    pub(super) fn schedule_remote_ref_transaction(
        &mut self,
        transaction: ScheduledRemoteRefTransaction,
    ) {
        self.remote_ref_transactions.push_back(transaction);
    }

    pub(super) fn pending_remote_ref_transactions(
        &self,
    ) -> &VecDeque<ScheduledRemoteRefTransaction> {
        &self.remote_ref_transactions
    }

    fn take_remote_ref_transaction(
        &mut self,
        trigger: RemoteRefTransactionTrigger,
    ) -> Option<ScheduledRemoteRefTransaction> {
        (self.remote_ref_transactions.front()?.trigger == trigger)
            .then(|| self.remote_ref_transactions.pop_front().expect("front transaction exists"))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum RemoteRefTransactionTrigger {
    BeforePush,
    BeforeActiveVersionObservation,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct ScheduledRemoteRefTransaction {
    pub(super) trigger: RemoteRefTransactionTrigger,
    pub(super) updates: Vec<RemoteRefUpdate>,
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
        let (subcommand, _) = command(args)?;
        match subcommand {
            "config" if args.iter().any(|argument| argument == "--get-urlmatch") => {
                Some(Self::HttpRedirectPolicy)
            }
            "ls-remote" if args.iter().any(|argument| argument == "--get-url") => {
                Some(Self::LsRemoteUrl)
            }
            "ls-remote" => {
                let Some((remote, arguments)) = private_remote_ls_remote(args) else {
                    return Some(Self::LsRemoteOther);
                };
                if is_global_head_query(remote, arguments) {
                    Some(Self::LsRemoteHeads)
                } else if is_active_version_query(remote, arguments) {
                    Some(Self::LsRemoteActiveVersions)
                } else {
                    Some(Self::LsRemoteOther)
                }
            }
            subcommand => Self::from_subcommand(subcommand),
        }
    }

    fn subcommand(self) -> &'static str {
        match self {
            Self::Var => "var",
            Self::InterpretTrailers => "interpret-trailers",
            Self::HttpRedirectPolicy => "config --get-urlmatch",
            Self::LsRemoteUrl
            | Self::LsRemoteHeads
            | Self::LsRemoteActiveVersions
            | Self::LsRemoteOther => "ls-remote",
        }
    }
}

/// Recognizes only the suffix emitted by `PushDestination::remote_command`.
///
/// This deliberately does not implement Git's general `ls-remote` grammar.
fn is_global_head_query(remote: &str, arguments: &[String]) -> bool {
    let [quiet, symref, separator, operand, head, heads, version_root] = arguments else {
        return false;
    };

    quiet == "--quiet"
        && symref == "--symref"
        && separator == "--"
        && operand == remote
        && head == "HEAD"
        && heads == "refs/heads/*"
        && version_root == "refs/tags/gherrit"
}

fn is_active_version_query(remote: &str, arguments: &[String]) -> bool {
    let [quiet, separator, operand, patterns @ ..] = arguments else {
        return false;
    };
    if quiet != "--quiet"
        || separator != "--"
        || operand != remote
        || patterns.is_empty()
        || !patterns.len().is_multiple_of(2)
    {
        return false;
    }

    let mut ids = HashSet::with_capacity(patterns.len() / 2);
    patterns.chunks_exact(2).all(|pair| {
        let [root, wildcard] = pair else {
            unreachable!("chunks_exact(2) always yields pairs");
        };
        let Some(id) = root.strip_prefix("refs/tags/gherrit/") else {
            return false;
        };
        !id.is_empty()
            && id.bytes().all(|byte| byte.is_ascii_alphanumeric())
            && wildcard.strip_suffix("/*") == Some(root.as_str())
            && ids.insert(id)
    })
}

/// Extracts the one exact private-remote adapter invocation production emits.
fn private_remote_ls_remote(args: &[String]) -> Option<(&str, &[String])> {
    const DESTINATION: &str = "GHERRIT_PRIVATE_PUSH_DESTINATION";

    let [program, no_replace_objects, url, pushurl, config, redirect, subcommand, arguments @ ..] =
        args
    else {
        return None;
    };
    if program != "git"
        || no_replace_objects != "--no-replace-objects"
        || config != "-c"
        || redirect != "http.followRedirects=false"
        || subcommand != "ls-remote"
    {
        return None;
    }

    let url = url.strip_prefix("--config-env=remote.")?;
    let remote = url.strip_suffix(&format!(".url={DESTINATION}"))?;
    let pushurl = pushurl.strip_prefix("--config-env=remote.")?;
    let push_remote = pushurl.strip_suffix(&format!(".pushurl={DESTINATION}"))?;
    (remote == push_remote && is_internal_remote(remote)).then_some((remote, arguments))
}

fn is_internal_remote(remote: &str) -> bool {
    const STEM: &str = "gherrit-publication";

    let Some(suffix) = remote.strip_prefix(STEM) else {
        return false;
    };
    if suffix.is_empty() {
        return true;
    }
    let Some(index) = suffix.strip_prefix('-') else {
        return false;
    };
    index.parse::<usize>().is_ok_and(|parsed| parsed != 0 && parsed.to_string() == index)
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
    command(args).map(|(subcommand, _)| subcommand)
}

fn command(args: &[String]) -> Option<(&str, &[String])> {
    let first = args.get(1)?;
    if first != "--no-replace-objects" {
        return (!first.starts_with('-')).then_some((first, &args[2..]));
    }

    let mut index = 2;
    loop {
        match args.get(index)?.as_str() {
            "-c" => {
                args.get(index + 1)?;
                index += 2;
            }
            argument if argument.starts_with("--config-env=") => index += 1,
            subcommand if !subcommand.starts_with('-') => {
                return Some((subcommand, &args[index + 1..]));
            }
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

    if operation == Some(GitOperation::LsRemoteActiveVersions) {
        apply_remote_ref_transaction(
            &handler,
            RemoteRefTransactionTrigger::BeforeActiveVersionObservation,
        );
    }

    if is_push {
        apply_remote_ref_transaction(&handler, RemoteRefTransactionTrigger::BeforePush);
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

fn apply_remote_ref_transaction(handler: &HandlerState, trigger: RemoteRefTransactionTrigger) {
    let transaction = handler.shared.write().unwrap().git.take_remote_ref_transaction(trigger);
    let Some(transaction) = transaction else { return };
    let remote =
        handler.remote.as_ref().expect("scheduled remote update requires a remote repository");

    // `git update-ref --stdin` applies every command in one ref transaction.
    // This lets a process test place a coherent publication tuple exactly
    // between two observations without exposing an impossible intermediate
    // state to either observation.
    let input = std::iter::once("start\n".to_owned())
        .chain(
            transaction
                .updates
                .into_iter()
                .map(|update| format!("update {} {}\n", update.ref_name, update.target)),
        )
        .chain(["prepare\n".to_owned(), "commit\n".to_owned()])
        .collect::<String>();
    remote
        .environment
        .command(&remote.system_git)
        .current_dir(&remote.path)
        .args(["update-ref", "--stdin"])
        .input(input)
        .assert()
        .success();
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
                &[
                    "git",
                    "--no-replace-objects",
                    "ls-remote",
                    "--quiet",
                    "--symref",
                    "--",
                    "gherrit-publication",
                    "HEAD",
                    "refs/heads/*",
                    "refs/tags/gherrit",
                ][..],
                Some(GitOperation::LsRemoteOther),
            ),
            (
                &["git", "--no-replace-objects", "ls-remote", "--symref", "HEAD"][..],
                Some(GitOperation::LsRemoteOther),
            ),
            (
                &["git", "--no-replace-objects", "ls-remote", "refs/heads/Gone"][..],
                Some(GitOperation::LsRemoteOther),
            ),
            (
                &[
                    "git",
                    "--no-replace-objects",
                    "ls-remote",
                    "--quiet",
                    "--",
                    "gherrit-publication",
                    "refs/tags/gherrit/Gone",
                    "refs/tags/gherrit/Gone/*",
                ][..],
                Some(GitOperation::LsRemoteOther),
            ),
        ] {
            assert_eq!(GitOperation::from_args(&args(arguments)), expected);
        }
    }

    fn production_ls_remote(remote: &str, arguments: &[&str]) -> Vec<String> {
        [
            "git".to_owned(),
            "--no-replace-objects".to_owned(),
            format!("--config-env=remote.{remote}.url=GHERRIT_PRIVATE_PUSH_DESTINATION"),
            format!("--config-env=remote.{remote}.pushurl=GHERRIT_PRIVATE_PUSH_DESTINATION"),
            "-c".to_owned(),
            "http.followRedirects=false".to_owned(),
            "ls-remote".to_owned(),
        ]
        .into_iter()
        .chain(arguments.iter().map(|argument| (*argument).to_owned()))
        .collect()
    }

    fn assert_ls_remote_other(remote: &str, arguments: &[&str]) {
        assert_eq!(
            GitOperation::from_args(&production_ls_remote(remote, arguments)),
            Some(GitOperation::LsRemoteOther),
            "remote={remote:?}, arguments={arguments:?}"
        );
    }

    fn assert_not_known_remote_observation(arguments: &[String]) {
        assert!(
            !matches!(
                GitOperation::from_args(arguments),
                Some(GitOperation::LsRemoteHeads | GitOperation::LsRemoteActiveVersions)
            ),
            "arguments={arguments:?}"
        );
    }

    #[test]
    fn remote_observation_requires_the_exact_private_adapter_prefix() {
        let tail = [
            "--quiet",
            "--symref",
            "--",
            "gherrit-publication-2",
            "HEAD",
            "refs/heads/*",
            "refs/tags/gherrit",
        ];
        let canonical = production_ls_remote("gherrit-publication-2", &tail);
        assert_eq!(GitOperation::from_args(&canonical), Some(GitOperation::LsRemoteHeads));

        for index in 0..7 {
            let mut missing = canonical.clone();
            missing.remove(index);
            assert_not_known_remote_observation(&missing);
        }
        for index in 0..6 {
            let mut reordered = canonical.clone();
            reordered.swap(index, index + 1);
            assert_not_known_remote_observation(&reordered);
        }

        for (index, replacement) in [
            (0, "not-git"),
            (1, "--replace-objects"),
            (
                2,
                "--config-env=remote.gherrit-publication.url=GHERRIT_PRIVATE_PUSH_DESTINATION",
            ),
            (
                3,
                "--config-env=remote.gherrit-publication-3.pushurl=GHERRIT_PRIVATE_PUSH_DESTINATION",
            ),
            (
                3,
                "--config-env=remote.gherrit-publication-2.pushurl=OTHER_DESTINATION",
            ),
            (4, "--config"),
            (5, "http.followRedirects=true"),
            (6, "fetch"),
        ] {
            let mut invalid = canonical.clone();
            invalid[index] = replacement.to_owned();
            assert_not_known_remote_observation(&invalid);
        }
    }

    #[test]
    fn global_head_operation_requires_the_complete_ordered_production_tail() {
        let canonical = [
            "--quiet",
            "--symref",
            "--",
            "gherrit-publication-2",
            "HEAD",
            "refs/heads/*",
            "refs/tags/gherrit",
        ];
        assert_eq!(
            GitOperation::from_args(&production_ls_remote("gherrit-publication-2", &canonical)),
            Some(GitOperation::LsRemoteHeads)
        );

        for index in 0..canonical.len() {
            let mut missing = canonical.to_vec();
            missing.remove(index);
            assert_ls_remote_other("gherrit-publication-2", &missing);

            let mut duplicated = canonical.to_vec();
            duplicated.insert(index, canonical[index]);
            assert_ls_remote_other("gherrit-publication-2", &duplicated);
        }
        for index in 0..canonical.len() - 1 {
            let mut reordered = canonical;
            reordered.swap(index, index + 1);
            assert_ls_remote_other("gherrit-publication-2", &reordered);
        }
        for index in 0..=canonical.len() {
            let mut extra = canonical.to_vec();
            extra.insert(index, "unexpected");
            assert_ls_remote_other("gherrit-publication-2", &extra);
        }

        for remote in ["gherrit-publication", "gherrit-publication-1", "gherrit-publication-42"] {
            let mut query = canonical;
            query[3] = remote;
            assert_eq!(
                GitOperation::from_args(&production_ls_remote(remote, &query)),
                Some(GitOperation::LsRemoteHeads)
            );
        }
        for remote in [
            "",
            "origin",
            "gherrit-publication-0",
            "gherrit-publication-01",
            "gherrit-publication--1",
            "gherrit-publication-9999999999999999999999999999999999999999",
        ] {
            let mut query = canonical;
            query[3] = remote;
            assert_ls_remote_other(remote, &query);
        }

        assert_ls_remote_other("gherrit-publication", &["--symref", "HEAD"]);
        assert_ls_remote_other(
            "gherrit-publication",
            &[
                "--quiet",
                "--symref",
                "--",
                "gherrit-publication",
                "HEAD",
                "refs/heads/*",
                "refs/tags/gherrit/Gone",
                "refs/tags/gherrit/Gone/*",
            ],
        );
    }

    #[test]
    fn active_version_operation_requires_complete_unique_root_wildcard_pairs() {
        let one = [
            "--quiet",
            "--",
            "gherrit-publication",
            "refs/tags/gherrit/Gone",
            "refs/tags/gherrit/Gone/*",
        ];
        let two = [
            "--quiet",
            "--",
            "gherrit-publication-12",
            "refs/tags/gherrit/Gone",
            "refs/tags/gherrit/Gone/*",
            "refs/tags/gherrit/G2",
            "refs/tags/gherrit/G2/*",
        ];
        assert_eq!(
            GitOperation::from_args(&production_ls_remote("gherrit-publication", &one)),
            Some(GitOperation::LsRemoteActiveVersions)
        );
        assert_eq!(
            GitOperation::from_args(&production_ls_remote("gherrit-publication-12", &two)),
            Some(GitOperation::LsRemoteActiveVersions)
        );

        for index in 0..two.len() {
            let mut missing = two.to_vec();
            missing.remove(index);
            assert_ls_remote_other("gherrit-publication-12", &missing);

            let mut duplicated = two.to_vec();
            duplicated.insert(index, two[index]);
            assert_ls_remote_other("gherrit-publication-12", &duplicated);
        }
        for index in 0..two.len() - 1 {
            let mut reordered = two;
            reordered.swap(index, index + 1);
            assert_ls_remote_other("gherrit-publication-12", &reordered);
        }
        for index in 0..=two.len() {
            let mut extra = two.to_vec();
            extra.insert(index, "unexpected");
            assert_ls_remote_other("gherrit-publication-12", &extra);
        }

        for malformed in [
            &[
                "--quiet",
                "--",
                "gherrit-publication",
                "refs/tags/gherrit/Gone",
                "refs/tags/gherrit/Gone/*",
                "refs/tags/gherrit/Gone",
                "refs/tags/gherrit/Gone/*",
            ][..],
            &["--quiet", "--", "gherrit-publication", "refs/tags/gherrit/Gone"][..],
            &[
                "--quiet",
                "--",
                "gherrit-publication",
                "refs/tags/gherrit/Gone/*",
                "refs/tags/gherrit/Gone",
            ][..],
            &[
                "--quiet",
                "--",
                "gherrit-publication",
                "refs/tags/gherrit/Gone",
                "refs/tags/gherrit/Gtwo/*",
            ][..],
            &[
                "--quiet",
                "--",
                "gherrit-publication",
                "refs/tags/gherrit/G-one",
                "refs/tags/gherrit/G-one/*",
            ][..],
            &["--quiet", "--", "gherrit-publication", "refs/tags/gherrit/", "refs/tags/gherrit//*"]
                [..],
            &[
                "--quiet",
                "--",
                "gherrit-publication",
                "refs/tags/gherrit/G/one",
                "refs/tags/gherrit/G/one/*",
            ][..],
            &[
                "--quiet",
                "--",
                "gherrit-publication",
                "refs/tags/gherrit/G雪",
                "refs/tags/gherrit/G雪/*",
            ][..],
            &[
                "--quiet",
                "--symref",
                "--",
                "gherrit-publication",
                "refs/tags/gherrit/Gone",
                "refs/tags/gherrit/Gone/*",
            ][..],
            &["--quiet", "--", "gherrit-publication", "HEAD", "refs/heads/*"][..],
        ] {
            assert_ls_remote_other("gherrit-publication", malformed);
        }
    }

    #[test]
    fn git_faults_match_in_script_order() {
        let expected = VecDeque::from([
            FailureKind::Git(GitOperation::Var),
            FailureKind::Git(GitOperation::LsRemoteHeads),
        ]);
        let mut state = MockState { faults: expected.clone(), ..Default::default() };

        assert_eq!(check_and_apply_failure(&mut state, GitOperation::LsRemoteHeads), None);
        assert_eq!(state.faults, expected);
        assert_eq!(
            check_and_apply_failure(&mut state, GitOperation::Var),
            Some(FailureKind::Git(GitOperation::Var))
        );
        assert_eq!(
            check_and_apply_failure(&mut state, GitOperation::LsRemoteHeads),
            Some(FailureKind::Git(GitOperation::LsRemoteHeads))
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

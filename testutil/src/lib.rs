use std::{
    fs,
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc, LazyLock, RwLock,
    },
    thread,
};

use regex::Regex;
use tempfile::TempDir;

mod environment;
mod git_fixture;
mod github;
mod normalization;
mod server;
use environment::{TestEnvironment, SYSTEM_GIT};
use git_fixture::{
    deterministic_gherrit_id, init_git_bare_repo, init_git_repo, install_gherrit_binary,
    install_git_interceptor,
};
pub use github::MockGithub;
#[cfg(test)]
use normalization::normalize_git_diagnostic_separators;
use normalization::{
    apply_literal_redactions, expand_literal_redactions, normalize_command_output,
    redact_identities,
};
#[cfg(any(test, windows))]
use normalization::{normalize_windows_redacted_paths, normalize_windows_stderr};
pub use server::MockServerInfo;
#[cfg(test)]
use server::{wait_for_server_completion, MockServerStopError};

mod command;
mod git_interceptor;
mod mock_server;
pub mod scripted_http;

pub use command::TestCommand;

pub const DEFAULT_OWNER: &str = "owner";
pub const DEFAULT_REPO: &str = "repo";
pub const MANAGED_PRIVATE: &str = "managedPrivate";
pub const MANAGED_PUBLIC: &str = "managedPublic";

const FIRST_GIT_TIMESTAMP: u64 = 946_684_800;

#[macro_export]
macro_rules! test_context {
    () => {
        $crate::TestContextBuilder::new(assert_cmd::cargo::cargo_bin!("gherrit-test-driver"))
    };
}

pub struct TestContextBuilder {
    owner: String,
    name: String,
    remote: bool,
    installed_hooks: bool,
    initial_commit: bool,
    mock_github: bool,
    git_interceptor: bool,
    gherrit_bin: PathBuf,
}

impl TestContextBuilder {
    pub fn new(gherrit_bin: impl Into<PathBuf>) -> Self {
        Self {
            owner: DEFAULT_OWNER.to_string(),
            name: DEFAULT_REPO.to_string(),
            remote: false,
            installed_hooks: false,
            initial_commit: false,
            mock_github: false,
            git_interceptor: false,
            gherrit_bin: gherrit_bin.into(),
        }
    }

    #[must_use]
    pub fn repository(mut self, owner: &str, name: &str) -> Self {
        self.owner = owner.to_string();
        self.name = name.to_string();
        self
    }

    #[must_use]
    pub fn with_remote(mut self) -> Self {
        self.remote = true;
        self
    }

    #[must_use]
    pub fn with_installed_hooks(mut self) -> Self {
        self.installed_hooks = true;
        self
    }

    #[must_use]
    pub fn with_initial_commit(mut self) -> Self {
        self.initial_commit = true;
        self
    }

    #[must_use]
    pub fn with_mock_github(mut self) -> Self {
        self.mock_github = true;
        self
    }

    #[must_use]
    pub fn with_git_interceptor(mut self) -> Self {
        self.git_interceptor = true;
        self
    }

    pub fn build(self) -> TestContext {
        let dir = Arc::new(TempDir::new().unwrap());
        let system_git = SYSTEM_GIT.clone();
        let test_environment = TestEnvironment::new(dir.path(), &system_git);
        let repo_path = dir.path().join("local");
        fs::create_dir(&repo_path).unwrap();

        let remote_parent = dir.path().join(&self.owner);
        let remote_path = remote_parent.join(format!("{}.git", self.name));
        if self.remote {
            fs::create_dir_all(&remote_parent).unwrap();
            init_git_bare_repo(&test_environment, &system_git, &remote_path);
        }

        init_git_repo(
            &test_environment,
            &system_git,
            &repo_path,
            self.remote.then_some(remote_path.as_path()),
        );

        if self.installed_hooks {
            install_gherrit_binary(dir.path(), &self.gherrit_bin);
        }
        if self.git_interceptor {
            install_git_interceptor(dir.path(), &self.gherrit_bin);
        }

        let mut mock_server_state = None;

        let mock_server = (self.mock_github || self.git_interceptor).then(|| {
            let state = mock_server::MockState::new(self.owner.clone(), self.name.clone());
            let state = Arc::new(RwLock::new(state));
            mock_server_state = Some(state.clone());

            MockServerInfo::start(
                state,
                remote_path.clone(),
                system_git.clone(),
                test_environment.clone(),
                dir.clone(),
            )
        });

        let ctx = TestContext {
            dir,
            repo_path,
            remote_path,
            has_remote: self.remote,
            has_mock_github: self.mock_github,
            has_git_interceptor: self.git_interceptor,
            system_git: system_git.clone(),
            gherrit_bin_path: self.gherrit_bin,
            test_environment,
            next_git_timestamp: AtomicU64::new(FIRST_GIT_TIMESTAMP),
            next_gherrit_id: AtomicU64::new(1),
            mock_server,
            mock_server_state,
        };

        if self.installed_hooks {
            ctx.gherrit_cmd().arg("install").assert().success();
        }

        if self.initial_commit {
            ctx.commit("Initial commit");
            if self.remote {
                ctx.seed_remote_main();
            }
        }

        ctx
    }
}

pub struct TestContext {
    pub dir: Arc<TempDir>,
    pub repo_path: PathBuf,
    pub system_git: PathBuf,
    pub gherrit_bin_path: PathBuf,
    remote_path: PathBuf,
    has_remote: bool,
    has_mock_github: bool,
    has_git_interceptor: bool,
    test_environment: TestEnvironment,
    next_git_timestamp: AtomicU64,
    next_gherrit_id: AtomicU64,
    mock_server: Option<MockServerInfo>,
    mock_server_state: Option<Arc<RwLock<mock_server::MockState>>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GitOperation {
    Var,
    InterpretTrailers,
    LsRemote,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FailureKind {
    GraphQl,
    CreatePr,
    UpdatePr,
    Git(GitOperation),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GraphQlOperation {
    Query,
    CreatePr,
    UpdatePr,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum PullRequestState {
    Open,
    Closed,
    Merged,
}

impl PullRequestState {
    fn as_str(self) -> &'static str {
        match self {
            PullRequestState::Open => "OPEN",
            PullRequestState::Closed => "CLOSED",
            PullRequestState::Merged => "MERGED",
        }
    }

    fn from_str(state: &str) -> Self {
        match state {
            "OPEN" => PullRequestState::Open,
            "CLOSED" => PullRequestState::Closed,
            "MERGED" => PullRequestState::Merged,
            state => panic!("mock pull request has invalid state {state:?}"),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PullRequestSeed {
    pub number: usize,
    pub title: String,
    pub body: String,
    pub head: String,
    pub base: String,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct PullRequestSnapshot {
    pub number: usize,
    pub node_id: String,
    pub state: PullRequestState,
    pub title: Option<String>,
    pub body: Option<String>,
    pub head: String,
    pub base: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PushRecord {
    arguments: Vec<String>,
    pub exit_code: i32,
}

impl PushRecord {
    pub fn arguments(&self) -> &[String] {
        &self.arguments
    }

    pub fn succeeded(&self) -> bool {
        self.exit_code == 0
    }
}

impl From<&mock_server::PrEntry> for PullRequestSnapshot {
    fn from(pr: &mock_server::PrEntry) -> Self {
        Self {
            number: pr.number,
            node_id: pr.node_id.clone(),
            state: PullRequestState::from_str(&pr.state),
            title: pr.title.clone(),
            body: pr.body.clone(),
            head: pr.head.clone(),
            base: pr.base.clone(),
        }
    }
}

impl Drop for TestContext {
    fn drop(&mut self) {
        // Stop the server before fixture directories and state are released.
        drop(self.mock_server.take());

        let (state_poisoned, pending_faults) = self
            .mock_server_state
            .as_ref()
            .map(|state| match state.read() {
                Ok(state) => (false, state.faults.clone()),
                Err(poisoned) => (true, poisoned.into_inner().faults.clone()),
            })
            .unwrap_or_default();
        if state_poisoned {
            let message = format!(
                "Test fixture mock state was poisoned; unconsumed faults: {pending_faults:?}"
            );
            if thread::panicking() {
                eprintln!("{message}");
            } else {
                panic!("{message}");
            }
            return;
        }
        if !pending_faults.is_empty() {
            if thread::panicking() {
                eprintln!("Test fixture also has unconsumed faults: {pending_faults:?}");
            } else {
                panic!("Test fixture has unconsumed faults: {pending_faults:?}");
            }
        }
    }
}

impl TestContext {
    fn configure_test_env(&self, cmd: &mut TestCommand) {
        self.test_environment.apply_to_command(cmd);

        // Give each command a deterministic, unique timestamp. In particular,
        // this ensures an otherwise-empty amend creates a distinct Git object.
        let timestamp = self.next_git_timestamp.fetch_add(1, Ordering::Relaxed);
        let git_date = format!("@{timestamp} +0000");
        cmd.env("GIT_AUTHOR_DATE", &git_date);
        cmd.env("GIT_COMMITTER_DATE", &git_date);

        if self.has_git_interceptor {
            cmd.env("SYSTEM_GIT_PATH", &self.system_git);
            cmd.env("GHERRIT_TEST_DRIVER", &self.gherrit_bin_path);

            if let Some(server) = &self.mock_server {
                cmd.env("GHERRIT_MOCK_SERVER_URL", &server.url);
            }
        }

        // These variables belong on the outer Git command too: installed
        // hooks inherit its environment when Git invokes GHerrit.
        if self.has_mock_github {
            let server = self.mock_server.as_ref().expect("mock GitHub server not available");
            cmd.env("GHERRIT_GITHUB_API_URL", &server.url);
            cmd.env("GITHUB_TOKEN", "mock-token");
        }
    }

    #[must_use = "command builders do nothing until executed"]
    pub fn gherrit_cmd(&self) -> TestCommand {
        // Use injected binary path
        let mut cmd = TestCommand::new(&self.gherrit_bin_path);
        cmd.current_dir(&self.repo_path);

        self.configure_test_env(&mut cmd);

        cmd
    }

    #[must_use = "command builders do nothing until executed"]
    pub fn remote_git_cmd(&self) -> TestCommand {
        assert!(self.has_remote, "missing test capability: .with_remote()");
        let mut cmd = TestCommand::new(&self.system_git);
        cmd.current_dir(&self.remote_path);
        self.configure_test_env(&mut cmd);
        cmd
    }

    pub fn run_git(&self, args: &[&str]) {
        self.git_cmd().args(args).assert().success();
    }

    #[must_use = "command builders do nothing until executed"]
    pub fn git_cmd(&self) -> TestCommand {
        let mut cmd = TestCommand::new("git");
        cmd.current_dir(&self.repo_path);
        self.configure_test_env(&mut cmd);
        cmd
    }

    fn mock_state(&self) -> &Arc<RwLock<mock_server::MockState>> {
        self.mock_server_state
            .as_ref()
            .expect("missing test capability: .with_mock_github() or .with_git_interceptor()")
    }

    pub fn mock_server_url(&self) -> &str {
        assert!(self.has_mock_github, "missing test capability: .with_mock_github()");
        &self.mock_server.as_ref().expect("mock GitHub server not available").url
    }

    pub fn commit(&self, msg: &str) {
        self.run_git(&["commit", "--allow-empty", "-m", msg]);
    }

    /// Creates a commit with a deterministic, syntactically valid GHerrit ID.
    ///
    /// This is fixture setup, not a commit-hook boundary. `--no-verify` makes
    /// that distinction explicit and prevents an installed hook from changing
    /// the supplied identity.
    pub fn commit_with_gherrit_id(&self, message: &str) -> String {
        let sequence = self.next_gherrit_id.fetch_add(1, Ordering::Relaxed);
        let id = deterministic_gherrit_id(sequence);
        self.commit_with_explicit_gherrit_id(message, &id);
        id
    }

    /// Creates a commit with a caller-supplied legacy or scenario identity.
    pub fn commit_with_explicit_gherrit_id(&self, message: &str, id: &str) {
        assert!(
            !message.lines().any(|line| line.starts_with("gherrit-pr-id: ")),
            "the explicit GHerrit ID must not also appear in the message"
        );
        assert!(
            !id.is_empty() && id.bytes().all(|byte| byte.is_ascii_alphanumeric()),
            "GHerrit IDs contain only ASCII letters and numbers"
        );

        let message = format!("{message}\n\ngherrit-pr-id: {id}");
        self.run_git(&["commit", "--allow-empty", "--no-verify", "-m", &message]);
    }

    pub fn amend(&self) {
        self.amend_inner(&["--no-edit"]);
    }

    pub fn amend_with_message(&self, message: &str) {
        let previous_id = self.gherrit_id("HEAD").ok();
        let message = if message.lines().any(|line| line.starts_with("gherrit-pr-id: ")) {
            message.to_string()
        } else if let Some(id) = &previous_id {
            format!("{message}\n\ngherrit-pr-id: {id}")
        } else {
            message.to_string()
        };

        self.amend_inner(&["-m", &message]);
    }

    fn amend_inner(&self, args: &[&str]) {
        let previous_oid = self.head_oid();
        let previous_id = self.gherrit_id("HEAD").ok();

        self.git_cmd()
            .arg("commit")
            .arg("--amend")
            .args(args)
            .arg("--allow-empty")
            .assert()
            .success();

        let amended_oid = self.head_oid();
        assert_ne!(previous_oid, amended_oid, "Amend must create a distinct commit object");
        if let Some(previous_id) = previous_id {
            assert_eq!(
                self.gherrit_id("HEAD").unwrap(),
                previous_id,
                "Amend must preserve the GHerrit ID"
            );
        }
    }

    pub fn head_oid(&self) -> String {
        let assert = self.git_cmd().args(["rev-parse", "HEAD"]).assert().success();
        String::from_utf8(assert.get_output().stdout.clone()).unwrap().trim().to_string()
    }

    fn seed_remote_main(&self) {
        self.test_environment
            .command(&self.system_git)
            .current_dir(&self.repo_path)
            .args(["push", "--quiet", "--no-verify", "origin", "refs/heads/main:refs/heads/main"])
            .assert()
            .success();
        self.test_environment
            .command(&self.system_git)
            .current_dir(&self.remote_path)
            .args(["symbolic-ref", "HEAD", "refs/heads/main"])
            .assert()
            .success();
    }

    pub fn checkout_new(&self, branch_name: &str) {
        self.run_git(&["checkout", "-b", branch_name]);
    }

    pub fn checkout_managed_private(&self, branch_name: &str) {
        self.checkout_new(branch_name);
        self.configure_managed_private(branch_name);
    }

    pub fn checkout_managed_public(&self, branch_name: &str) {
        self.checkout_new(branch_name);
        self.configure_managed_public(branch_name);
    }

    pub fn configure_managed_private(&self, branch_name: &str) {
        self.configure_managed(branch_name, MANAGED_PRIVATE, ".");
    }

    pub fn configure_managed_public(&self, branch_name: &str) {
        self.configure_managed(branch_name, MANAGED_PUBLIC, "origin");
    }

    fn configure_managed(&self, branch_name: &str, state: &str, push_remote: &str) {
        let key = |suffix: &str| format!("branch.{branch_name}.{suffix}");
        self.set_config(&key("gherritManaged"), Some(state));
        self.set_config(&key("pushRemote"), Some(push_remote));
        self.set_config(&key("remote"), Some("."));
        self.set_config(&key("merge"), Some(&format!("refs/heads/{branch_name}")));
    }

    pub fn inject_failure(&self, kind: FailureKind) {
        match kind {
            FailureKind::Git(_) => assert!(
                self.has_git_interceptor,
                "missing test capability: .with_git_interceptor()"
            ),
            _ => assert!(self.has_mock_github, "missing test capability: .with_mock_github()"),
        }
        self.enqueue_failure(kind);
    }

    pub fn expect_git_failure(&self, operation: GitOperation) {
        self.inject_failure(FailureKind::Git(operation));
    }

    fn enqueue_failure(&self, kind: FailureKind) {
        self.mock_state().write().unwrap().faults.push_back(kind);
    }

    pub fn limit_graphql_operations_per_request(&self, limit: usize) {
        assert!(self.has_mock_github, "missing test capability: .with_mock_github()");
        assert_ne!(limit, 0, "GraphQL operation limit must be nonzero");
        self.mock_state().write().unwrap().max_graphql_operations_per_request = Some(limit);
    }

    pub fn assert_failure_consumed(&self) {
        self.inspect_mock_state(|state| {
            assert!(
                state.faults.is_empty(),
                "Expected injected failures to be consumed, but {:?} remain",
                state.faults
            );
        });
    }

    fn inspect_mock_state<T>(&self, f: impl FnOnce(&mock_server::MockState) -> T) -> T {
        let state = self.mock_state().read().unwrap();
        f(&state)
    }

    fn mutate_mock_state<T>(&self, f: impl FnOnce(&mut mock_server::MockState) -> T) -> T {
        let mut state = self.mock_state().write().unwrap();
        f(&mut state)
    }

    pub fn github(&self) -> MockGithub<'_> {
        assert!(self.has_mock_github, "missing test capability: .with_mock_github()");
        MockGithub { context: self }
    }

    pub fn recorded_pushes(&self) -> Vec<PushRecord> {
        assert!(self.has_git_interceptor, "missing test capability: .with_git_interceptor()");
        self.inspect_mock_state(|state| {
            state
                .git
                .pushes()
                .iter()
                .map(|push| PushRecord {
                    arguments: push.arguments().to_vec(),
                    exit_code: push.exit_code(),
                })
                .collect()
        })
    }

    pub fn formatted_github_state(&self) -> String {
        let state = self.github().pull_requests();
        let json = serde_json::to_string_pretty(&state).expect("Failed to serialize PRs");
        self.sanitize(&json)
    }

    pub fn remote_ref_oid(&self, ref_name: &str) -> Option<String> {
        let output = self
            .remote_git_cmd()
            .args(["rev-parse", "--verify", "--quiet", ref_name])
            .output()
            .expect("Failed to inspect remote ref");
        match output.status.code() {
            Some(0) => Some(String::from_utf8(output.stdout).unwrap().trim().to_string()),
            Some(1) => None,
            code => panic!("git rev-parse failed with exit code {code:?}"),
        }
    }

    pub fn remote_refs(&self, prefix: &str) -> Vec<String> {
        let assert = self
            .remote_git_cmd()
            .args(["for-each-ref", "--format=%(refname)", prefix])
            .assert()
            .success();
        String::from_utf8(assert.get_output().stdout.clone())
            .unwrap()
            .lines()
            .map(ToString::to_string)
            .collect()
    }

    pub fn init_bare_repo(&self, path: &Path) {
        init_git_bare_repo(&self.test_environment, &self.system_git, path);
    }

    pub fn set_config(&self, key: &str, value: Option<&str>) {
        if let Some(val) = value {
            self.git_cmd().args(["config", key, val]).assert().success();
        } else {
            let output = self
                .git_cmd()
                .args(["config", "--get-all", key])
                .output()
                .expect("Failed to inspect Git config");
            match output.status.code() {
                Some(0) => {
                    self.git_cmd().args(["config", "--unset-all", key]).assert().success();
                }
                Some(1) => {}
                code => panic!(
                    "Failed to inspect Git config {key:?} with exit code {code:?}: {}",
                    String::from_utf8_lossy(&output.stderr)
                ),
            }
        }
    }

    pub fn assert_config(&self, key: &str, expected_value: Option<&str>) {
        if let Some(val) = expected_value {
            self.git_cmd().args(["config", key]).assert().success().stdout(format!("{}\n", val));
        } else {
            self.git_cmd().args(["config", key]).assert().code(1).stdout("");
        }
    }

    #[must_use = "command builders do nothing until executed"]
    pub fn hook_cmd(&self, name: &str) -> TestCommand {
        let mut cmd = self.gherrit_cmd();
        cmd.args(["hook", name]);
        cmd
    }

    #[must_use = "command builders do nothing until executed"]
    pub fn manage_cmd(&self) -> TestCommand {
        let mut cmd = self.gherrit_cmd();
        cmd.arg("manage");
        cmd
    }

    #[must_use = "command builders do nothing until executed"]
    pub fn unmanage_cmd(&self) -> TestCommand {
        let mut cmd = self.gherrit_cmd();
        cmd.arg("unmanage");
        cmd
    }

    pub fn gherrit_id(&self, ref_name: &str) -> Result<String, Box<dyn std::error::Error>> {
        let assert = self.git_cmd().args(["log", "-1", "--format=%B", ref_name]).assert().success();
        let stdout = String::from_utf8(assert.get_output().stdout.clone())?;

        stdout
            .lines()
            .find_map(|line| line.strip_prefix("gherrit-pr-id: "))
            .map(|id| id.trim().to_string())
            .ok_or_else(|| format!("Commit {} is missing 'gherrit-pr-id' trailer", ref_name).into())
    }

    pub fn sanitize(&self, output: &str) -> String {
        self.sanitize_with_redactions(output, &[])
    }

    pub fn sanitize_with_redactions(&self, output: &str, redactions: &[(&str, &str)]) -> String {
        let repo_path = self.repo_path.to_str().unwrap();
        let remote_path = self.remote_path.to_str().unwrap();
        let redactions = expand_literal_redactions(
            redactions
                .iter()
                .copied()
                .chain([(repo_path, "[REPO_PATH]"), (remote_path, "[REMOTE_PATH]")]),
        );

        let output = apply_literal_redactions(
            output,
            redactions.iter().map(|(target, replacement)| (target.as_str(), replacement.as_str())),
        );
        static SHA_REGEX: LazyLock<Regex> =
            LazyLock::new(|| Regex::new(r"\b[0-9a-f]{40}\b").expect("Invalid regex"));

        static MOCK_URL_REGEX: LazyLock<Regex> =
            LazyLock::new(|| Regex::new(r"http://127\.0\.0\.1:\d+").expect("Invalid regex"));

        static GHERRIT_ID_REGEX: LazyLock<Regex> =
            LazyLock::new(|| Regex::new(r"\bG[a-zA-Z0-9]{16,}\b").expect("Invalid regex"));

        let output = redact_identities(&output, &SHA_REGEX, "SHA");
        let output = redact_identities(&output, &GHERRIT_ID_REGEX, "GHERRIT_ID");
        MOCK_URL_REGEX.replace_all(&output, "[MOCK_SERVER_URL]").to_string()
    }
}

pub trait IntoCommandRef {
    fn as_command_mut(&mut self) -> &mut TestCommand;
}

impl IntoCommandRef for TestCommand {
    fn as_command_mut(&mut self) -> &mut TestCommand {
        self
    }
}

impl IntoCommandRef for &mut TestCommand {
    fn as_command_mut(&mut self) -> &mut TestCommand {
        self
    }
}

impl TestContext {
    pub fn execute_and_format(
        &self,
        mut cmd: impl IntoCommandRef,
        redactions: &[(&str, &str)],
        expected_exit: ExpectedExit,
    ) -> String {
        let cmd = cmd.as_command_mut();
        let output = cmd.output().expect("Failed to execute command");

        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        let stderr = normalize_command_output(&stderr);
        #[cfg(windows)]
        let stderr = normalize_windows_stderr(&stderr);
        let succeeded = output.status.success();
        let exit_code = output.status.code().unwrap_or(-1);

        // This output will be stored verbatim in the filesystem.
        let output = format!(
            "EXIT_CODE: {}\n\nSTDOUT:\n{}\n\nSTDERR:\n{}\n",
            exit_code,
            if stdout.is_empty() { "(empty)" } else { &stdout },
            if stderr.is_empty() { "(empty)" } else { &stderr }
        );
        let output = self.sanitize_with_redactions(&output, redactions);
        #[cfg(windows)]
        let output = normalize_windows_redacted_paths(&output);
        match expected_exit {
            ExpectedExit::Success => {
                assert!(succeeded, "Expected command to succeed:\n{output}");
            }
            ExpectedExit::Failure => {
                assert!(!succeeded, "Expected command to fail:\n{output}");
            }
        }
        output
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExpectedExit {
    Success,
    Failure,
}

#[macro_export]
macro_rules! assert_success_snapshot {
    ($ctx:expr, $cmd:expr, $name:expr $(,)?) => {
        $crate::assert_success_snapshot!($ctx, $cmd, $name, &[])
    };
    ($ctx:expr, $cmd:expr, $name:expr, $redactions:expr $(,)?) => {
        let content = $ctx.execute_and_format($cmd, $redactions, $crate::ExpectedExit::Success);
        insta::assert_snapshot!($name, content);
    };
}

#[macro_export]
macro_rules! assert_failure_snapshot {
    ($ctx:expr, $cmd:expr, $name:expr $(,)?) => {
        $crate::assert_failure_snapshot!($ctx, $cmd, $name, &[])
    };
    ($ctx:expr, $cmd:expr, $name:expr, $redactions:expr $(,)?) => {
        let content = $ctx.execute_and_format($cmd, $redactions, $crate::ExpectedExit::Failure);
        insta::assert_snapshot!($name, content);
    };
}

#[macro_export]
macro_rules! assert_pr_snapshot {
    ($ctx:expr, $name:expr $(,)?) => {
        insta::assert_snapshot!($name, $ctx.formatted_github_state());
    };
}

#[cfg(test)]
mod tests {
    use std::{
        panic,
        sync::mpsc::{self, Receiver},
        time::{Duration, Instant},
    };

    use super::*;

    #[test]
    fn poisoned_mock_state_reports_unconsumed_faults() {
        let driver_dir = TempDir::new().unwrap();
        let driver = driver_dir.path().join("gherrit-test-driver");
        fs::write(&driver, "test driver placeholder").unwrap();

        let ctx = TestContextBuilder::new(&driver).with_git_interceptor().build();
        ctx.expect_git_failure(GitOperation::Var);
        let state = ctx.mock_server_state.as_ref().unwrap().clone();
        let poison = panic::catch_unwind(move || {
            let _state = state.write().unwrap();
            panic!("simulated request-handler panic");
        });
        assert!(poison.is_err());

        let panic = panic::catch_unwind(panic::AssertUnwindSafe(|| drop(ctx)))
            .expect_err("dropping the fixture must reject poisoned mock state");
        let message = panic
            .downcast_ref::<String>()
            .map(String::as_str)
            .or_else(|| panic.downcast_ref::<&str>().copied())
            .expect("fixture panic must have a string message");
        assert!(message.contains("was poisoned"), "unexpected teardown panic: {message}");
        assert!(message.contains("Git(Var)"), "unexpected teardown panic: {message}");
    }

    #[test]
    fn deterministic_ids_use_the_production_alphabet_and_width() {
        assert_eq!(deterministic_gherrit_id(0), format!("G{}", "a".repeat(32)));
        assert_eq!(deterministic_gherrit_id(1), format!("G{}b", "a".repeat(31)));
        assert_eq!(deterministic_gherrit_id(31), format!("G{}7", "a".repeat(31)));
        assert_eq!(deterministic_gherrit_id(32), format!("G{}ba", "a".repeat(30)));
    }

    #[test]
    fn explicit_management_helpers_write_complete_branch_state() {
        let ctx = TestContextBuilder::new("unused").with_initial_commit().build();
        let assert_state = |branch: &str, state: &str, push_remote: &str| {
            ctx.assert_config(&format!("branch.{branch}.gherritManaged"), Some(state));
            ctx.assert_config(&format!("branch.{branch}.pushRemote"), Some(push_remote));
            ctx.assert_config(&format!("branch.{branch}.remote"), Some("."));
            ctx.assert_config(
                &format!("branch.{branch}.merge"),
                Some(&format!("refs/heads/{branch}")),
            );
        };

        ctx.checkout_managed_private("private-stack");
        assert_state("private-stack", MANAGED_PRIVATE, ".");

        ctx.run_git(&["checkout", "main"]);
        ctx.checkout_managed_public("public-stack");
        assert_state("public-stack", MANAGED_PUBLIC, "origin");
    }

    #[test]
    fn interceptor_only_context_rejects_unconsumed_git_faults() {
        let driver_dir = TempDir::new().unwrap();
        let driver = driver_dir.path().join("gherrit-test-driver");
        fs::write(&driver, "test driver placeholder").unwrap();

        let panic = panic::catch_unwind(|| {
            let ctx = TestContextBuilder::new(&driver).with_git_interceptor().build();
            ctx.expect_git_failure(GitOperation::Var);
        })
        .expect_err("dropping the fixture must reject an unconsumed Git fault");
        let message = panic
            .downcast_ref::<String>()
            .map(String::as_str)
            .or_else(|| panic.downcast_ref::<&str>().copied())
            .expect("fixture panic must have a string message");
        assert!(message.contains("Git(Var)"), "unexpected teardown panic: {message}");
    }

    #[test]
    fn identity_redaction_preserves_equality_and_order() {
        let regex = Regex::new(r"G[a-z0-9]+").unwrap();
        let output = redact_identities("Galpha Gbeta Galpha Ggamma Gbeta", &regex, "ID");

        assert_eq!(output, "[ID_1] [ID_2] [ID_1] [ID_3] [ID_2]");
    }

    #[test]
    fn identity_namespaces_are_independent() {
        let sha_regex = Regex::new(r"[0-9a-f]{40}").unwrap();
        let id_regex = Regex::new(r"G[a-z0-9]+").unwrap();
        let first_sha = "1111111111111111111111111111111111111111";
        let second_sha = "2222222222222222222222222222222222222222";
        let output = format!("{first_sha} Gone {second_sha} Gtwo {first_sha} Gtwo");
        let output = redact_identities(&output, &sha_regex, "SHA");
        let output = redact_identities(&output, &id_regex, "GHERRIT_ID");

        assert_eq!(output, "[SHA_1] [GHERRIT_ID_1] [SHA_2] [GHERRIT_ID_2] [SHA_1] [GHERRIT_ID_2]");
    }

    #[test]
    fn literal_redactions_take_precedence_over_identity_redaction() {
        let regex = Regex::new(r"[0-9a-f]{40}").unwrap();
        let base_sha = "1111111111111111111111111111111111111111";
        let other_sha = "2222222222222222222222222222222222222222";
        let output = format!("{base_sha} {other_sha} {base_sha}");
        let output = apply_literal_redactions(&output, [(base_sha, "[BASE_SHA]")]);
        let output = redact_identities(&output, &regex, "SHA");

        assert_eq!(output, "[BASE_SHA] [SHA_1] [BASE_SHA]");
    }

    #[test]
    fn normalizes_windows_stderr_without_rewriting_payloads() {
        let stderr = concat!(
            "Usage: gherrit.exe manage\r\n",
            "[gherrit] [WARN] Failed: Command \"git\" failed with status: exit code: 128\r\n",
            "bash.exe: warning: could not find /tmp, please create!\r\n",
            "[gherrit] [WARN] Nested command failed. Stderr: bash.exe: warning: could not find /tmp, please create!\r\n",
            "fatal: repository missing\r\n",
            "payload: Usage: gherrit.exe manage\r\n",
            "payload: Command failed with status: exit code: 2\r\n",
            "payload: bash.exe: warning: could not find /tmp, please create!\r\n",
            "payload Stderr: bash.exe: warning: could not find /tmp, please create!\r\n",
            r#"payload: {"title":"Usage: gherrit.exe","body":"line\n{\"key\":\"value\"}"}"#,
        );

        assert_eq!(
            normalize_windows_stderr(stderr),
            concat!(
                "Usage: gherrit manage\r\n",
                "[gherrit] [WARN] Failed: Command \"git\" failed with status: exit status: 128\r\n",
                "[gherrit] [WARN] Nested command failed. Stderr: fatal: repository missing\r\n",
                "payload: Usage: gherrit.exe manage\r\n",
                "payload: Command failed with status: exit code: 2\r\n",
                "payload: bash.exe: warning: could not find /tmp, please create!\r\n",
                "payload Stderr: bash.exe: warning: could not find /tmp, please create!\r\n",
                r#"payload: {"title":"Usage: gherrit.exe","body":"line\n{\"key\":\"value\"}"}"#,
            )
        );
    }

    #[test]
    fn normalizes_only_redacted_windows_paths() {
        let output = concat!(
            "[REPO_PATH]\\.git\\hooks\\pre-push\n",
            "[REMOTE_PATH]\\\\objects\\\\pack\n",
            r#"{"body":"line\n{\"path\":\"C:\\unredacted\"}"}"#,
        );

        assert_eq!(
            normalize_windows_redacted_paths(output),
            concat!(
                "[REPO_PATH]/.git/hooks/pre-push\n",
                "[REMOTE_PATH]/objects/pack\n",
                r#"{"body":"line\n{\"path\":\"C:\\unredacted\"}"}"#,
            )
        );
    }

    #[test]
    fn expands_path_redaction_spellings() {
        let root = TempDir::new().unwrap();
        let path = root.path().to_str().unwrap();
        let canonical = fs::canonicalize(path).unwrap();
        let debug = format!("{canonical:?}");
        let debug = debug.trim_matches('"');
        let redactions = expand_literal_redactions([(path, "[ROOT]")]);

        let redact = |output| {
            apply_literal_redactions(
                output,
                redactions
                    .iter()
                    .map(|(target, replacement)| (target.as_str(), replacement.as_str())),
            )
        };
        assert_eq!(redact(canonical.to_str().unwrap()), "[ROOT]");
        assert_eq!(redact(debug), "[ROOT]");

        let windows_path = r"C:\root";
        let debug = format!("{windows_path:?}");
        let debug = debug.trim_matches('"');
        let redactions = expand_literal_redactions([(windows_path, "[ROOT]")]);
        let redact = |output| {
            apply_literal_redactions(
                output,
                redactions
                    .iter()
                    .map(|(target, replacement)| (target.as_str(), replacement.as_str())),
            )
        };
        assert_eq!(redact(debug), "[ROOT]");
        assert_eq!(redact("C:/root/child"), "[ROOT]/child");
    }

    #[test]
    fn applies_longest_literal_redactions_first() {
        let root = TempDir::new().unwrap();
        let child = root.path().join("child");
        fs::create_dir(&child).unwrap();
        let root = root.path().to_str().unwrap();
        let child = child.to_str().unwrap();
        let redactions = expand_literal_redactions([(root, "[ROOT]"), (child, "[CHILD]")]);

        assert_eq!(
            apply_literal_redactions(
                child,
                redactions
                    .iter()
                    .map(|(target, replacement)| (target.as_str(), replacement.as_str())),
            ),
            "[CHILD]"
        );
    }

    #[test]
    #[cfg(unix)]
    fn test_environment_clears_inherited_values() {
        let root = TempDir::new().unwrap();
        let environment = TestEnvironment::new(root.path(), SYSTEM_GIT.as_path());
        let mut command = TestCommand::new("/usr/bin/env");
        command.env("SHOULD_BE_CLEARED", "yes");
        environment.apply_to_command(&mut command);

        let assert = command.assert().success();
        let output = String::from_utf8(assert.get_output().stdout.clone()).unwrap();
        assert!(!output.contains("SHOULD_BE_CLEARED="));
        assert!(output.lines().any(|line| line == "RUST_LOG=info"));
        assert!(output.lines().any(|line| line.starts_with("HOME=")));
    }

    #[test]
    fn server_completion_wait_is_bounded() {
        let (_completion_tx, completion_rx) = mpsc::channel();
        let timeout = Duration::from_millis(20);
        let started = Instant::now();

        let error = wait_for_server_completion(&completion_rx, timeout).unwrap_err();

        assert!(matches!(error, MockServerStopError::TimedOut(value) if value == timeout));
        assert!(started.elapsed() < Duration::from_secs(1));
    }

    #[test]
    fn server_completion_propagates_panics() {
        let (completion_tx, completion_rx): (_, Receiver<thread::Result<()>>) = mpsc::channel();
        assert!(completion_tx.send(Err(Box::new("server panic"))).is_ok());

        let error = wait_for_server_completion(&completion_rx, Duration::from_secs(1)).unwrap_err();

        let MockServerStopError::Panicked(payload) = error else {
            panic!("expected server panic result");
        };
        assert_eq!(payload.downcast_ref::<&str>(), Some(&"server panic"));
    }

    #[test]
    fn dropping_mock_server_waits_for_state_release() {
        let state = Arc::new(RwLock::new(mock_server::MockState::default()));
        let weak_state = Arc::downgrade(&state);
        let test_dir = Arc::new(TempDir::new().unwrap());
        let system_git = SYSTEM_GIT.clone();
        let test_environment = TestEnvironment::new(test_dir.path(), &system_git);
        let server = MockServerInfo::start(
            state,
            test_dir.path().join("remote.git"),
            system_git,
            test_environment,
            test_dir,
        );

        assert!(weak_state.upgrade().is_some());
        drop(server);
        assert!(weak_state.upgrade().is_none(), "mock server retained state after teardown");
    }

    #[test]
    fn normalizes_only_atomic_push_separator_lines() {
        let canonical = concat!(
            "before\n",
            "error: atomic push failed for ref refs/tags/v2. status: 7\n",
            "To remote\n",
            "after\n",
        );
        let with_separator = concat!(
            "before\n",
            "error: atomic push failed for ref refs/tags/v2. status: 7\n",
            "\n",
            "To remote\n",
            "after\n",
        );
        assert_eq!(normalize_git_diagnostic_separators(canonical), canonical);
        assert_eq!(normalize_git_diagnostic_separators(with_separator), canonical);

        let unrelated = "error: another failure\n\nTo remote\n";
        assert_eq!(normalize_git_diagnostic_separators(unrelated), unrelated);
    }
}

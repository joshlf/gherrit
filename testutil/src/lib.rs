use std::{
    env, fs,
    path::{Path, PathBuf},
    process::Command,
    sync::{Arc, LazyLock, RwLock},
};

use regex::Regex;
use tempfile::TempDir;

pub mod mock_server;

pub const DEFAULT_OWNER: &str = "owner";
pub const DEFAULT_REPO: &str = "repo";
pub const MANAGED_PRIVATE: &str = "managedPrivate";
pub const MANAGED_PUBLIC: &str = "managedPublic";

static SHA_REGEX: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\b[0-9a-f]{40}\b").expect("Invalid regex"));

static MOCK_URL_REGEX: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"http://127\.0\.0\.1:\d+").expect("Invalid regex"));

static GHERRIT_ID_REGEX: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\bG[0-9a-fA-F]{16,}\b").expect("Invalid regex"));

#[macro_export]
macro_rules! test_context {
    () => {
        $crate::TestContextBuilder::new()
            .binaries(assert_cmd::cargo::cargo_bin!("gherrit"), $crate::build_mock_bin())
    };
}

#[doc(hidden)]
pub fn build_mock_bin() -> PathBuf {
    static MOCK_BIN: LazyLock<PathBuf> = LazyLock::new(|| {
        let manifest_dir = Path::new(env!("CARGO_MANIFEST_DIR"));
        let target_dir = manifest_dir.parent().unwrap().join("target").join("mock_bin_build");

        eprintln!("Building mock_bin at {:?}", manifest_dir);
        let status = Command::new("cargo")
            .args(["build", "--bin", "mock_bin"])
            .arg("--manifest-path")
            .arg(manifest_dir.join("Cargo.toml"))
            .arg("--target-dir")
            .arg(&target_dir)
            .status()
            .expect("Failed to execute cargo build for mock_bin");

        if !status.success() {
            panic!("Failed to build mock_bin. See stdout/stderr for details.");
        }

        target_dir.join("debug").join(if cfg!(windows) { "mock_bin.exe" } else { "mock_bin" })
    });
    MOCK_BIN.clone()
}

#[macro_export]
macro_rules! test_context_minimal {
    () => {
        $crate::TestContextBuilder::new_minimal()
            .binaries(assert_cmd::cargo::cargo_bin!("gherrit"), $crate::build_mock_bin())
    };
}

pub struct TestContextBuilder {
    owner: String,
    name: String,
    install_hooks: bool,
    initial_commit: bool,
    gherrit_bin: Option<PathBuf>,
    mock_bin: Option<PathBuf>,
}

impl Default for TestContextBuilder {
    fn default() -> Self {
        Self::new()
    }
}

impl TestContextBuilder {
    pub fn new() -> Self {
        let mut slf = Self::new_minimal();
        slf.install_hooks(true).initial_commit(true);
        slf
    }

    pub fn new_minimal() -> Self {
        Self {
            owner: DEFAULT_OWNER.to_string(),
            name: DEFAULT_REPO.to_string(),
            install_hooks: false,
            initial_commit: false,
            gherrit_bin: None,
            mock_bin: None,
        }
    }

    pub fn binaries(&mut self, gherrit: impl Into<PathBuf>, mock: impl Into<PathBuf>) -> &mut Self {
        self.gherrit_bin = Some(gherrit.into());
        self.mock_bin = Some(mock.into());
        self
    }

    pub fn owner(&mut self, owner: &str) -> &mut Self {
        self.owner = owner.to_string();
        self
    }

    pub fn name(&mut self, name: &str) -> &mut Self {
        self.name = name.to_string();
        self
    }

    pub fn install_hooks(&mut self, install_hooks: bool) -> &mut Self {
        self.install_hooks = install_hooks;
        self
    }

    pub fn initial_commit(&mut self, initial_commit: bool) -> &mut Self {
        self.initial_commit = initial_commit;
        self
    }

    pub fn build(&self) -> TestContext {
        if std::env::var("GHERRIT_TEST_BUILD").is_err() {
            eprintln!("\n\x1b[31mERROR: You must run these tests with GHERRIT_TEST_BUILD=1\x1b[0m");
            eprintln!("This ensures the binary is compiled with the necessary test hooks.\n");
            panic!("Missing GHERRIT_TEST_BUILD environment variable");
        }

        let dir = TempDir::new().unwrap();
        let repo_path = dir.path().join("local");
        fs::create_dir(&repo_path).unwrap();

        let remote_parent = dir.path().join(&self.owner);
        fs::create_dir_all(&remote_parent).unwrap();
        let remote_path = remote_parent.join(format!("{}.git", self.name));
        init_git_bare_repo(&remote_path);

        let is_live = env::var("GHERRIT_LIVE_TEST").is_ok();

        // Resolve system git before we mess with PATH.
        let system_git = SYSTEM_GIT.clone();

        init_git_repo(&repo_path, &remote_path);

        let gherrit_bin = self.gherrit_bin.clone().expect("gherrit binary path must be set");
        let mock_bin = self.mock_bin.clone().expect("mock binary path must be set");

        let mut mock_server_state = None;

        let mock_server = (!is_live).then(|| {
            install_mock_binaries(dir.path(), &mock_bin, &gherrit_bin);

            let state = mock_server::MockState::new(self.owner.clone(), self.name.clone());

            let state = Arc::new(RwLock::new(state));
            mock_server_state = Some(state.clone());

            // Spawn the server on a separate thread to avoid blocking the main
            // test thread. This ensures the runtime persists for the duration
            // of the test context.

            let (tx, rx) = std::sync::mpsc::channel();
            let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel::<()>();

            let state_for_server = state.clone();
            std::thread::spawn(move || {
                let rt = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .expect("Failed to build runtime");

                rt.block_on(async {
                    let url = mock_server::start_mock_server(state_for_server).await;
                    tx.send(url).expect("Failed to send mock server URL");
                    let _ = shutdown_rx.await;
                });
            });

            MockServerInfo { url: rx.recv().unwrap(), shutdown_tx }
        });

        let ctx = TestContext {
            dir,
            repo_path,
            remote_path: remote_path.clone(),
            is_live,
            system_git: system_git.clone(),
            gherrit_bin_path: gherrit_bin.clone(),
            mock_server,
            mock_server_state,
        };

        if self.install_hooks {
            ctx.install_hooks();
        }

        if self.initial_commit {
            ctx.commit("Initial commit");
        }

        ctx
    }
}

pub struct TestContext {
    pub dir: TempDir,
    pub repo_path: PathBuf,
    pub remote_path: PathBuf,
    pub is_live: bool,
    pub system_git: PathBuf,
    pub gherrit_bin_path: PathBuf,
    pub mock_server: Option<MockServerInfo>,
    pub mock_server_state: Option<Arc<RwLock<mock_server::MockState>>>,
}

pub struct MockServerInfo {
    pub url: String,
    pub shutdown_tx: tokio::sync::oneshot::Sender<()>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum FailureKind {
    GraphQl,
    CreatePr,
    UpdatePr,
    Named(String),
}

impl Drop for TestContext {
    fn drop(&mut self) {
        if let Some(server) = self.mock_server.take() {
            let _ = server.shutdown_tx.send(());
        }
    }
}

impl TestContext {
    fn configure_mock_env(&self, cmd: &mut assert_cmd::Command) {
        if !self.is_live {
            // Prepend temp dir to PATH so 'gh' and 'git' resolve to our mock
            let mut paths = vec![self.dir.path().to_path_buf()];
            paths.extend(env::split_paths(&env::var_os("PATH").unwrap()));

            let new_path_str = env::join_paths(paths).unwrap();
            cmd.env("PATH", new_path_str);
            cmd.env("SYSTEM_GIT_PATH", &self.system_git);

            if let Some(server) = &self.mock_server {
                cmd.env("GHERRIT_MOCK_SERVER_URL", &server.url);
            }
        }
    }

    pub fn gherrit(&self) -> assert_cmd::Command {
        // Use injected binary path
        let mut cmd = assert_cmd::Command::new(&self.gherrit_bin_path);
        cmd.current_dir(&self.repo_path);

        self.configure_mock_env(&mut cmd);

        if !self.is_live {
            if let Some(server) = &self.mock_server {
                cmd.env("GHERRIT_GITHUB_API_URL", &server.url);
                cmd.env("GITHUB_TOKEN", "mock-token");
            }
        }

        cmd
    }

    pub fn remote_git(&self) -> assert_cmd::Command {
        let mut cmd = assert_cmd::Command::new("git");
        cmd.current_dir(&self.remote_path);
        self.configure_mock_env(&mut cmd);
        cmd
    }

    pub fn run_git(&self, args: &[&str]) {
        self.git().args(args).assert().success();
    }

    pub fn git(&self) -> assert_cmd::Command {
        let mut cmd = assert_cmd::Command::new("git");
        cmd.current_dir(&self.repo_path);
        self.configure_mock_env(&mut cmd);
        cmd
    }

    pub fn read_mock_state(&self) -> mock_server::MockState {
        self.mock_server_state.as_ref().expect("Mock state not available").read().unwrap().clone()
    }

    pub fn install_hooks(&self) {
        // Use the new install command
        self.gherrit().args(["install"]).assert().success();
    }

    pub fn commit(&self, msg: &str) {
        self.run_git(&["commit", "--allow-empty", "-m", msg]);
    }

    pub fn checkout_new(&self, branch_name: &str) {
        self.run_git(&["checkout", "-b", branch_name]);
    }

    pub fn inject_failure(&self, kind: FailureKind, remaining: usize) {
        let mut state =
            self.mock_server_state.as_ref().expect("Mock state not available").write().unwrap();

        state.fail_next_request = Some(kind);
        state.fail_remaining = remaining;
    }

    pub fn maybe_inspect_mock_state(&self, f: impl FnOnce(&mock_server::MockState)) {
        if !self.is_live {
            let state = self.read_mock_state();
            f(&state);
        }
    }

    pub fn maybe_mutate_mock_state(&self, f: impl FnOnce(&mut mock_server::MockState)) {
        if !self.is_live {
            let mut state =
                self.mock_server_state.as_ref().expect("Mock state not available").write().unwrap();
            f(&mut state);
        }
    }

    pub fn assert_pushed(&self, ref_name: &str) {
        self.maybe_inspect_mock_state(|state| {
            let found = state.pushed_refs.iter().any(|r| r == ref_name);
            assert!(
                found,
                "Expected ref '{}' to be pushed. Refs: {:?}",
                ref_name, state.pushed_refs
            );
        });
    }

    pub fn count_pushed_containing(&self, substring: &str) -> usize {
        let mut count = 0;
        self.maybe_inspect_mock_state(|state| {
            count = state
                .pushed_refs
                .iter()
                .filter(|r| !r.starts_with("--"))
                .filter(|r| r.contains(substring))
                .count();
        });
        count
    }

    pub fn set_config(&self, key: &str, value: Option<&str>) {
        if let Some(val) = value {
            self.git().args(["config", key, val]).assert().success();
        } else {
            // Ignore error if key doesn't exist when unsetting
            let _ = self.git().args(["config", "--unset", key]).output();
        }
    }

    pub fn assert_config(&self, key: &str, expected_value: Option<&str>) {
        if let Some(val) = expected_value {
            self.git().args(["config", key]).assert().success().stdout(format!("{}\n", val));
        } else {
            self.git().args(["config", key]).assert().failure();
        }
    }

    pub fn hook(&self, name: &str) -> assert_cmd::Command {
        let mut cmd = self.gherrit();
        cmd.args(["hook", name]);
        cmd
    }

    pub fn manage(&self) -> assert_cmd::Command {
        let mut cmd = self.gherrit();
        cmd.arg("manage");
        cmd
    }

    pub fn unmanage(&self) -> assert_cmd::Command {
        let mut cmd = self.gherrit();
        cmd.arg("unmanage");
        cmd
    }

    pub fn sanitize(&self, output: &str) -> String {
        self.sanitize_with_redactions(output, &[])
    }

    pub fn sanitize_with_redactions(&self, output: &str, redactions: &[(&str, &str)]) -> String {
        let mut output = output.replace(self.repo_path.to_str().unwrap(), "[REPO_PATH]");
        output = output.replace(self.remote_path.to_str().unwrap(), "[REMOTE_PATH]");

        for (target, replacement) in redactions {
            output = output.replace(target, replacement);
        }

        let output = SHA_REGEX.replace_all(&output, "[SHA]");
        let output = GHERRIT_ID_REGEX.replace_all(&output, "[GHERRIT_ID]");
        MOCK_URL_REGEX.replace_all(&output, "[MOCK_SERVER_URL]").to_string()
    }
}

pub trait IntoCommandRef {
    fn as_command_mut(&mut self) -> &mut assert_cmd::Command;
}

impl IntoCommandRef for assert_cmd::Command {
    fn as_command_mut(&mut self) -> &mut assert_cmd::Command {
        self
    }
}

impl<'a> IntoCommandRef for &'a mut assert_cmd::Command {
    fn as_command_mut(&mut self) -> &mut assert_cmd::Command {
        *self
    }
}

impl TestContext {
    pub fn execute_and_format(
        &self,
        mut cmd: impl IntoCommandRef,
        redactions: &[(&str, &str)],
    ) -> String {
        let cmd = cmd.as_command_mut();
        let output = cmd.output().expect("Failed to execute command");

        let stdout =
            self.sanitize_with_redactions(&String::from_utf8_lossy(&output.stdout), redactions);
        let stderr =
            self.sanitize_with_redactions(&String::from_utf8_lossy(&output.stderr), redactions);
        let exit_code = output.status.code().unwrap_or(-1);

        // This output will be stored verbatim in the filesystem.
        format!(
            "EXIT_CODE: {}\n\nSTDOUT:\n{}\n\nSTDERR:\n{}\n",
            exit_code,
            if stdout.is_empty() { "(empty)" } else { &stdout },
            if stderr.is_empty() { "(empty)" } else { &stderr }
        )
    }
}

#[macro_export]
macro_rules! assert_snapshot {
    ($ctx:expr, $cmd:expr, $name:expr $(,)?) => {
        $crate::assert_snapshot!($ctx, $cmd, $name, &[])
    };
    ($ctx:expr, $cmd:expr, $name:expr, $redactions:expr $(,)?) => {
        let content = $ctx.execute_and_format($cmd, $redactions);
        insta::assert_snapshot!($name, content);
    };
}

fn run_git_cmd(path: &Path, args: &[&str]) {
    assert_cmd::Command::new("git").current_dir(path).args(args).assert().success();
}

pub fn install_mock_binaries(path: &Path, mock_bin: &Path, gherrit_bin: &Path) {
    let git_dst = path.join(if cfg!(windows) { "git.exe" } else { "git" });
    let gherrit_dst = path.join(if cfg!(windows) { "gherrit.exe" } else { "gherrit" });

    fs::copy(mock_bin, &git_dst).unwrap();
    fs::copy(gherrit_bin, &gherrit_dst).unwrap();
}

pub fn init_git_bare_repo(path: &Path) {
    fs::create_dir(path).unwrap();
    run_git_cmd(path, &["init", "--bare"]);
}

fn init_git_repo(path: &Path, remote_path: &Path) {
    run_git_cmd(path, &["init"]);
    run_git_cmd(path, &["config", "core.hooksPath", ".git/hooks"]);
    // Must config user identity for commits to work
    run_git_cmd(path, &["config", "user.email", "test@example.com"]);
    run_git_cmd(path, &["config", "user.name", "Test User"]);
    // Ensure default branch is main
    run_git_cmd(path, &["symbolic-ref", "HEAD", "refs/heads/main"]);
    // Explicitly unmanage main to satisfy strict config checks
    run_git_cmd(path, &["config", "branch.main.gherritManaged", "false"]);
    // Add origin remote
    run_git_cmd(path, &["remote", "add", "origin", remote_path.to_str().unwrap()]);
}

static SYSTEM_GIT: LazyLock<PathBuf> = LazyLock::new(|| -> PathBuf {
    let output = if cfg!(windows) {
        Command::new("where").arg("git").output()
    } else {
        Command::new("which").arg("git").output()
    };
    let output = output.expect("Failed to find system git");
    if !output.status.success() {
        panic!("Failed to find git using 'which/where': {:?}", output);
    }
    let stdout = String::from_utf8(output.stdout).expect("Invalid utf8 from which git");
    let path = stdout.lines().next().expect("No git path found").trim();
    PathBuf::from(path)
});

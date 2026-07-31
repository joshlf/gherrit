use std::{
    collections::HashMap,
    env,
    ffi::OsString,
    fs,
    path::{Path, PathBuf},
    process::Command,
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc, LazyLock, RwLock,
    },
};

use regex::Regex;
use tempfile::TempDir;

pub mod mock_server;

pub const DEFAULT_OWNER: &str = "owner";
pub const DEFAULT_REPO: &str = "repo";
pub const MANAGED_PRIVATE: &str = "managedPrivate";
pub const MANAGED_PUBLIC: &str = "managedPublic";

const FIRST_GIT_TIMESTAMP: u64 = 946_684_800;

#[macro_export]
macro_rules! test_context {
    () => {
        $crate::TestContextBuilder::new()
            .binaries(assert_cmd::cargo::cargo_bin!("gherrit"), $crate::build_mock_bin())
    };
}

#[macro_export]
macro_rules! test_context_minimal {
    () => {
        $crate::TestContextBuilder::new_minimal()
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
        let system_git = SYSTEM_GIT.clone();
        let test_environment = TestEnvironment::new(dir.path(), &system_git);
        let repo_path = dir.path().join("local");
        fs::create_dir(&repo_path).unwrap();

        let remote_parent = dir.path().join(&self.owner);
        fs::create_dir_all(&remote_parent).unwrap();
        let remote_path = remote_parent.join(format!("{}.git", self.name));
        init_git_bare_repo(&test_environment, &system_git, &remote_path);

        let is_live = env::var("GHERRIT_LIVE_TEST").is_ok();
        let live_github_token = is_live.then(resolve_live_github_token);

        init_git_repo(&test_environment, &system_git, &repo_path, &remote_path);

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
            live_github_token,
            system_git: system_git.clone(),
            gherrit_bin_path: gherrit_bin.clone(),
            test_environment,
            next_git_timestamp: AtomicU64::new(FIRST_GIT_TIMESTAMP),
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
    live_github_token: Option<String>,
    pub system_git: PathBuf,
    pub gherrit_bin_path: PathBuf,
    test_environment: TestEnvironment,
    next_git_timestamp: AtomicU64,
    pub mock_server: Option<MockServerInfo>,
    pub mock_server_state: Option<Arc<RwLock<mock_server::MockState>>>,
}

#[derive(Clone)]
struct TestEnvironment {
    variables: Vec<(OsString, OsString)>,
}

impl TestEnvironment {
    fn new(root: &Path, system_git: &Path) -> Self {
        let home = root.join("home");
        let temporary_directory = root.join("tmp");
        fs::create_dir_all(&home).unwrap();
        fs::create_dir_all(&temporary_directory).unwrap();

        let git_config = home.join("gitconfig");
        fs::write(
            &git_config,
            "[color]\n\tui = false\n[commit]\n\tgpgSign = false\n[tag]\n\tgpgSign = false\n",
        )
        .unwrap();

        let mut paths = vec![root.to_path_buf()];
        if let Some(parent) = system_git.parent() {
            push_unique_path(&mut paths, parent);
        }

        #[cfg(unix)]
        for path in ["/usr/local/bin", "/usr/bin", "/bin", "/usr/sbin", "/sbin"] {
            push_unique_path(&mut paths, Path::new(path));
        }

        let mut variables = vec![
            (OsString::from("HOME"), home.clone().into_os_string()),
            (OsString::from("XDG_CONFIG_HOME"), home.join(".config").into_os_string()),
            (OsString::from("TMPDIR"), temporary_directory.clone().into_os_string()),
            (OsString::from("TMP"), temporary_directory.clone().into_os_string()),
            (OsString::from("TEMP"), temporary_directory.into_os_string()),
            (OsString::from("GIT_CONFIG_NOSYSTEM"), OsString::from("1")),
            (OsString::from("GIT_CONFIG_GLOBAL"), git_config.into_os_string()),
            (OsString::from("GIT_ATTR_NOSYSTEM"), OsString::from("1")),
            (OsString::from("GIT_TERMINAL_PROMPT"), OsString::from("0")),
            (OsString::from("GCM_INTERACTIVE"), OsString::from("never")),
            (OsString::from("GIT_PAGER"), OsString::from("cat")),
            (OsString::from("PAGER"), OsString::from("cat")),
            (OsString::from("GIT_EDITOR"), OsString::from("true")),
            (OsString::from("GIT_SEQUENCE_EDITOR"), OsString::from("true")),
            (OsString::from("LANG"), OsString::from("C")),
            (OsString::from("LC_ALL"), OsString::from("C")),
            (OsString::from("TZ"), OsString::from("UTC")),
            (OsString::from("TERM"), OsString::from("dumb")),
            (OsString::from("NO_COLOR"), OsString::from("1")),
            (OsString::from("RUST_LOG"), OsString::from("info")),
            (OsString::from("RUST_BACKTRACE"), OsString::from("0")),
            (OsString::from("NO_PROXY"), OsString::from("127.0.0.1,localhost")),
            (OsString::from("no_proxy"), OsString::from("127.0.0.1,localhost")),
        ];

        // Preserve only process-instrumentation settings needed to run the
        // already-built test binaries. These do not provide developer or CI
        // configuration to GHerrit, but dropping them would make coverage and
        // sanitizer runs silently incomplete or unable to start.
        for name in [
            "LLVM_PROFILE_FILE",
            "ASAN_OPTIONS",
            "LSAN_OPTIONS",
            "MSAN_OPTIONS",
            "TSAN_OPTIONS",
            "UBSAN_OPTIONS",
        ] {
            if let Some(value) = env::var_os(name) {
                variables.push((OsString::from(name), value));
            }
        }

        #[cfg(target_os = "linux")]
        if let Some(value) = env::var_os("LD_LIBRARY_PATH") {
            variables.push((OsString::from("LD_LIBRARY_PATH"), value));
        }

        #[cfg(target_os = "macos")]
        for name in ["DYLD_LIBRARY_PATH", "DYLD_FALLBACK_LIBRARY_PATH"] {
            if let Some(value) = env::var_os(name) {
                variables.push((OsString::from(name), value));
            }
        }

        #[cfg(windows)]
        {
            if let Some(system_root) = env::var_os("SystemRoot") {
                let system_root = PathBuf::from(system_root);
                push_unique_path(&mut paths, &system_root.join("System32"));
                variables
                    .push((OsString::from("SystemRoot"), system_root.clone().into_os_string()));
                variables.push((OsString::from("WINDIR"), system_root.clone().into_os_string()));
                let command_processor = env::var_os("COMSPEC")
                    .unwrap_or_else(|| system_root.join("System32/cmd.exe").into_os_string());
                variables.push((OsString::from("COMSPEC"), command_processor));
            }

            if let Some(git_root) = system_git.parent().and_then(Path::parent) {
                push_unique_path(&mut paths, &git_root.join("mingw64/bin"));
                push_unique_path(&mut paths, &git_root.join("usr/bin"));
            }
            variables.push((OsString::from("PATHEXT"), OsString::from(".COM;.EXE;.BAT;.CMD")));
        }

        variables.push((OsString::from("PATH"), env::join_paths(paths).unwrap()));
        Self { variables }
    }

    fn apply_to_assert_cmd(&self, cmd: &mut assert_cmd::Command) {
        cmd.env_clear();
        cmd.envs(self.variables.iter().cloned());
    }

    fn command(&self, program: &Path) -> assert_cmd::Command {
        let mut cmd = assert_cmd::Command::new(program);
        self.apply_to_assert_cmd(&mut cmd);
        cmd
    }
}

fn push_unique_path(paths: &mut Vec<PathBuf>, path: &Path) {
    if !paths.iter().any(|existing| existing == path) {
        paths.push(path.to_path_buf());
    }
}

fn resolve_live_github_token() -> String {
    let mut gh_auth_token = Command::new("gh");
    gh_auth_token.args(["auth", "token"]);
    resolve_live_github_token_with(env::var_os("GITHUB_TOKEN"), &mut gh_auth_token)
        .unwrap_or_else(|message| panic!("Live GitHub authentication failed: {message}"))
}

fn resolve_live_github_token_with(
    github_token: Option<OsString>,
    gh_auth_token: &mut Command,
) -> Result<String, String> {
    if let Some(token) = github_token.and_then(|token| token.into_string().ok()) {
        if !token.is_empty() {
            return Ok(token);
        }
    }

    let output = gh_auth_token
        .output()
        .map_err(|error| format!("failed to run `gh auth token`: {error}"))?;
    if !output.status.success() {
        return Err(
            "`gh auth token` did not return a token; set GITHUB_TOKEN or run `gh auth login`"
                .to_string(),
        );
    }

    let token = String::from_utf8(output.stdout)
        .map_err(|_| "`gh auth token` returned non-UTF-8 output".to_string())?;
    let token = token.trim().to_string();
    if token.is_empty() {
        return Err("`gh auth token` returned an empty token".to_string());
    }
    Ok(token)
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
    fn configure_test_env(&self, cmd: &mut assert_cmd::Command) {
        self.test_environment.apply_to_assert_cmd(cmd);

        // Give each command a deterministic, unique timestamp. In particular,
        // this ensures an otherwise-empty amend creates a distinct Git object.
        let timestamp = self.next_git_timestamp.fetch_add(1, Ordering::Relaxed);
        let git_date = format!("@{timestamp} +0000");
        cmd.env("GIT_AUTHOR_DATE", &git_date);
        cmd.env("GIT_COMMITTER_DATE", &git_date);

        if !self.is_live {
            cmd.env("SYSTEM_GIT_PATH", &self.system_git);

            if let Some(server) = &self.mock_server {
                cmd.env("GHERRIT_MOCK_SERVER_URL", &server.url);
            }
        } else {
            cmd.env(
                "GITHUB_TOKEN",
                self.live_github_token.as_ref().expect("live GitHub token was not resolved"),
            );
        }
    }

    #[must_use = "command builders do nothing until executed"]
    pub fn gherrit_cmd(&self) -> assert_cmd::Command {
        // Use injected binary path
        let mut cmd = assert_cmd::Command::new(&self.gherrit_bin_path);
        cmd.current_dir(&self.repo_path);

        self.configure_test_env(&mut cmd);

        if !self.is_live {
            if let Some(server) = &self.mock_server {
                cmd.env("GHERRIT_GITHUB_API_URL", &server.url);
                cmd.env("GITHUB_TOKEN", "mock-token");
            }
        }

        cmd
    }

    #[must_use = "command builders do nothing until executed"]
    pub fn remote_git_cmd(&self) -> assert_cmd::Command {
        let mut cmd = assert_cmd::Command::new(&self.system_git);
        cmd.current_dir(&self.remote_path);
        self.configure_test_env(&mut cmd);
        cmd
    }

    pub fn run_git(&self, args: &[&str]) {
        self.git_cmd().args(args).assert().success();
    }

    #[must_use = "command builders do nothing until executed"]
    pub fn git_cmd(&self) -> assert_cmd::Command {
        let mut cmd = assert_cmd::Command::new("git");
        cmd.current_dir(&self.repo_path);
        self.configure_test_env(&mut cmd);
        cmd
    }

    pub fn read_mock_state(&self) -> mock_server::MockState {
        self.mock_server_state.as_ref().expect("Mock state not available").read().unwrap().clone()
    }

    pub fn install_hooks(&self) {
        // Use the new install command
        self.gherrit_cmd().args(["install"]).assert().success();
    }

    pub fn commit(&self, msg: &str) {
        self.run_git(&["commit", "--allow-empty", "-m", msg]);
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

    pub fn checkout_new(&self, branch_name: &str) {
        self.run_git(&["checkout", "-b", branch_name]);
    }

    pub fn inject_failure(&self, kind: FailureKind) {
        let mut state =
            self.mock_server_state.as_ref().expect("Mock state not available").write().unwrap();

        state.fail_next_request = Some(kind);
    }

    pub fn assert_failure_consumed(&self) {
        self.maybe_inspect_mock_state(|state| {
            assert!(
                state.fail_next_request.is_none(),
                "Expected injected failure to be consumed, but {:?} remains",
                state.fail_next_request
            );
        });
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

    pub fn formatted_mock_pr_state(&self) -> String {
        let mut content = String::new();
        self.maybe_inspect_mock_state(|state| {
            let json = serde_json::to_string_pretty(&state.prs).expect("Failed to serialize PRs");
            content = self.sanitize(&json);
        });
        content
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

    pub fn count_successfully_pushed_containing(&self, substring: &str) -> usize {
        let mut count = 0;
        self.maybe_inspect_mock_state(|state| {
            count = state
                .pushes
                .iter()
                .filter(|push| push.succeeded())
                .flat_map(|push| &push.refspecs)
                .filter(|refspec| refspec.contains(substring))
                .count();
        });
        count
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
    pub fn hook_cmd(&self, name: &str) -> assert_cmd::Command {
        let mut cmd = self.gherrit_cmd();
        cmd.args(["hook", name]);
        cmd
    }

    #[must_use = "command builders do nothing until executed"]
    pub fn manage_cmd(&self) -> assert_cmd::Command {
        let mut cmd = self.gherrit_cmd();
        cmd.arg("manage");
        cmd
    }

    #[must_use = "command builders do nothing until executed"]
    pub fn unmanage_cmd(&self) -> assert_cmd::Command {
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
        let redactions = redactions.iter().cloned().chain([
            (repo_path, "[REPO_PATH]"),
            (remote_path, "[REMOTE_PATH]"),
            // On macOS, the system may report paths starting with /private/var,
            // while the test harness sees /var. After the redaction above, we
            // get "/private[REPO_PATH]". This line strips that prefix. On
            // Linux, this string won't exist, so it does nothing.
            ("/private[", "["),
            // This git error message only appears on some platforms/git
            // versions.
            ("fatal: the remote end hung up unexpectedly\n", ""),
        ]);

        let output = apply_literal_redactions(output, redactions);
        let output = normalize_git_diagnostic_separators(&output);

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

fn normalize_git_diagnostic_separators(output: &str) -> String {
    static ATOMIC_PUSH_SEPARATOR_REGEX: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r"(?m)^(error: atomic push failed[^\r\n]*\r?\n)(?:\r?\n)+(To )")
            .expect("Invalid regex")
    });

    ATOMIC_PUSH_SEPARATOR_REGEX.replace_all(output, "${1}${2}").into_owned()
}

pub trait IntoCommandRef {
    fn as_command_mut(&mut self) -> &mut assert_cmd::Command;
}

impl IntoCommandRef for assert_cmd::Command {
    fn as_command_mut(&mut self) -> &mut assert_cmd::Command {
        self
    }
}

impl IntoCommandRef for &mut assert_cmd::Command {
    fn as_command_mut(&mut self) -> &mut assert_cmd::Command {
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

fn redact_identities(output: &str, regex: &Regex, namespace: &str) -> String {
    let mut identities = HashMap::new();
    regex
        .replace_all(output, |captures: &regex::Captures<'_>| {
            let next_index = identities.len() + 1;
            let index = identities.entry(captures[0].to_string()).or_insert(next_index);
            format!("[{namespace}_{index}]")
        })
        .into_owned()
}

fn apply_literal_redactions<'a>(
    output: &str,
    redactions: impl IntoIterator<Item = (&'a str, &'a str)>,
) -> String {
    redactions.into_iter().fold(output.to_string(), |output, (target, replacement)| {
        output.replace(target, replacement)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

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
    fn live_github_token_prefers_environment() {
        let mut unusable_fallback = Command::new("gherrit-command-that-must-not-run");

        let token = resolve_live_github_token_with(
            Some(OsString::from("environment-token")),
            &mut unusable_fallback,
        )
        .unwrap();

        assert_eq!(token, "environment-token");
    }

    #[test]
    #[cfg(unix)]
    fn live_github_token_falls_back_to_gh_login() {
        let mut gh_auth_token = Command::new("sh");
        gh_auth_token.args(["-c", "printf 'login-token\\n'"]);

        let token = resolve_live_github_token_with(None, &mut gh_auth_token).unwrap();

        assert_eq!(token, "login-token");
    }

    #[test]
    #[cfg(unix)]
    fn live_environment_injects_captured_token_without_restoring_home() {
        let dir = TempDir::new().unwrap();
        let repo_path = dir.path().join("local");
        fs::create_dir(&repo_path).unwrap();
        let isolated_home = dir.path().join("home");
        let test_environment = TestEnvironment::new(dir.path(), SYSTEM_GIT.as_path());
        let ctx = TestContext {
            remote_path: dir.path().join("remote.git"),
            dir,
            repo_path,
            is_live: true,
            live_github_token: Some("captured-token".to_string()),
            system_git: SYSTEM_GIT.clone(),
            gherrit_bin_path: PathBuf::from("/usr/bin/env"),
            test_environment,
            next_git_timestamp: AtomicU64::new(FIRST_GIT_TIMESTAMP),
            mock_server: None,
            mock_server_state: None,
        };

        let assert = ctx.gherrit_cmd().assert().success();
        let output = String::from_utf8(assert.get_output().stdout.clone()).unwrap();

        assert!(output.lines().any(|line| line == "GITHUB_TOKEN=captured-token"));
        assert!(
            output.lines().any(|line| line == format!("HOME={}", isolated_home.display())),
            "live command did not retain the fixture-owned HOME: {output}"
        );
    }

    #[test]
    #[cfg(unix)]
    fn test_environment_clears_inherited_values() {
        let root = TempDir::new().unwrap();
        let environment = TestEnvironment::new(root.path(), SYSTEM_GIT.as_path());
        let mut command = assert_cmd::Command::new("/usr/bin/env");
        command.env("SHOULD_BE_CLEARED", "yes");
        environment.apply_to_assert_cmd(&mut command);

        let assert = command.assert().success();
        let output = String::from_utf8(assert.get_output().stdout.clone()).unwrap();
        assert!(!output.contains("SHOULD_BE_CLEARED="));
        assert!(output.lines().any(|line| line == "RUST_LOG=info"));
        assert!(output.lines().any(|line| line.starts_with("HOME=")));
    }
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
        insta::assert_snapshot!($name, $ctx.formatted_mock_pr_state());
    };
}

fn run_git_cmd(environment: &TestEnvironment, system_git: &Path, path: &Path, args: &[&str]) {
    environment.command(system_git).current_dir(path).args(args).assert().success();
}

pub fn install_mock_binaries(path: &Path, mock_bin: &Path, gherrit_bin: &Path) {
    let git_dst = path.join(if cfg!(windows) { "git.exe" } else { "git" });
    let gherrit_dst = path.join(if cfg!(windows) { "gherrit.exe" } else { "gherrit" });

    fs::copy(mock_bin, &git_dst).unwrap();
    fs::copy(gherrit_bin, &gherrit_dst).unwrap();
}

fn init_git_bare_repo(environment: &TestEnvironment, system_git: &Path, path: &Path) {
    fs::create_dir(path).unwrap();
    run_git_cmd(environment, system_git, path, &["init", "--bare"]);
}

fn init_git_repo(
    environment: &TestEnvironment,
    system_git: &Path,
    path: &Path,
    remote_path: &Path,
) {
    let run = |args| run_git_cmd(environment, system_git, path, args);
    run(&["init"]);
    run(&["config", "core.hooksPath", ".git/hooks"]);
    // Must config user identity for commits to work
    run(&["config", "user.email", "test@example.com"]);
    run(&["config", "user.name", "Test User"]);
    // Ensure default branch is main
    run(&["symbolic-ref", "HEAD", "refs/heads/main"]);
    // Explicitly unmanage main to satisfy strict config checks
    run(&["config", "branch.main.gherritManaged", "false"]);
    // Add origin remote
    run(&["remote", "add", "origin", remote_path.to_str().unwrap()]);
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

#[cfg(test)]
mod git_diagnostic_tests {
    use super::normalize_git_diagnostic_separators;

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

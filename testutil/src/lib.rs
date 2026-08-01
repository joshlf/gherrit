use std::{
    any::Any,
    collections::HashMap,
    env,
    ffi::OsString,
    fmt, fs, panic,
    path::{Path, PathBuf},
    process::Command,
    sync::{
        atomic::{AtomicU64, Ordering},
        mpsc::{self, Receiver, RecvTimeoutError},
        Arc, LazyLock, RwLock,
    },
    thread,
    time::Duration,
};

use regex::Regex;
use tempfile::TempDir;

mod command;
pub mod mock_server;

pub use command::TestCommand;

pub const DEFAULT_OWNER: &str = "owner";
pub const DEFAULT_REPO: &str = "repo";
pub const MANAGED_PRIVATE: &str = "managedPrivate";
pub const MANAGED_PUBLIC: &str = "managedPublic";

const FIRST_GIT_TIMESTAMP: u64 = 946_684_800;
const MOCK_SERVER_STARTUP_TIMEOUT: Duration = Duration::from_secs(10);
const MOCK_SERVER_SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(10);

#[cfg(unix)]
const GIT_INTERCEPTOR_NAME: &str = "git";
#[cfg(windows)]
const GIT_INTERCEPTOR_NAME: &str = "git.cmd";

#[macro_export]
macro_rules! test_context {
    () => {
        $crate::TestContextBuilder::new(assert_cmd::cargo::cargo_bin!("gherrit"))
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
        if std::env::var("GHERRIT_TEST_BUILD").is_err() {
            eprintln!("\n\x1b[31mERROR: You must run these tests with GHERRIT_TEST_BUILD=1\x1b[0m");
            eprintln!("This ensures the binary is compiled with the necessary test hooks.\n");
            panic!("Missing GHERRIT_TEST_BUILD environment variable");
        }

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
            install_git_interceptor(dir.path());
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
    mock_server: Option<MockServerInfo>,
    mock_server_state: Option<Arc<RwLock<mock_server::MockState>>>,
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

    fn apply_to_command(&self, cmd: &mut TestCommand) {
        cmd.env_clear();
        cmd.envs(self.variables.iter().cloned());
    }

    fn command(&self, program: &Path) -> TestCommand {
        let mut cmd = TestCommand::new(program);
        self.apply_to_command(&mut cmd);
        cmd
    }
}

fn push_unique_path(paths: &mut Vec<PathBuf>, path: &Path) {
    if !paths.iter().any(|existing| existing == path) {
        paths.push(path.to_path_buf());
    }
}

pub struct MockServerInfo {
    pub url: String,
    shutdown_tx: Option<tokio::sync::oneshot::Sender<()>>,
    completion_rx: Option<Receiver<thread::Result<()>>>,
}

enum MockServerStopError {
    Panicked(Box<dyn Any + Send + 'static>),
    TimedOut(Duration),
    Disconnected,
}

impl fmt::Display for MockServerStopError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Panicked(_) => formatter.write_str("mock server thread panicked"),
            Self::TimedOut(timeout) => {
                write!(formatter, "mock server did not stop within {timeout:?}")
            }
            Self::Disconnected => {
                formatter.write_str("mock server thread exited without reporting its result")
            }
        }
    }
}

impl MockServerInfo {
    fn start(
        state: Arc<RwLock<mock_server::MockState>>,
        remote_path: PathBuf,
        system_git: PathBuf,
        test_environment: TestEnvironment,
        fixture_dir: Arc<TempDir>,
    ) -> Self {
        let (ready_tx, ready_rx) = mpsc::channel();
        let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel();
        let (completion_tx, completion_rx) = mpsc::channel();

        thread::Builder::new()
            .name("gherrit-mock-server".to_string())
            .spawn(move || {
                let result = panic::catch_unwind(panic::AssertUnwindSafe(move || {
                    // Keep the fixture directory alive if this thread must be
                    // detached after a pathological shutdown timeout.
                    let _fixture_dir = fixture_dir;
                    let runtime = tokio::runtime::Builder::new_current_thread()
                        .enable_all()
                        .build()
                        .expect("Failed to build mock server runtime");
                    runtime.block_on(mock_server::run_mock_server(
                        state,
                        remote_path,
                        system_git,
                        test_environment,
                        ready_tx,
                        shutdown_rx,
                    ));
                }));
                let _ = completion_tx.send(result);
            })
            .expect("Failed to spawn mock server thread");

        let url = match ready_rx.recv_timeout(MOCK_SERVER_STARTUP_TIMEOUT) {
            Ok(url) => url,
            Err(startup_error) => match Self::stop(Some(shutdown_tx), &completion_rx) {
                Ok(()) => panic!(
                    "Mock server did not become ready within \
                     {MOCK_SERVER_STARTUP_TIMEOUT:?}: {startup_error}"
                ),
                Err(MockServerStopError::Panicked(server_panic)) => {
                    panic::resume_unwind(server_panic)
                }
                Err(stop_error) => panic!(
                    "Mock server did not become ready within \
                     {MOCK_SERVER_STARTUP_TIMEOUT:?}: {startup_error}; {stop_error}"
                ),
            },
        };

        Self { url, shutdown_tx: Some(shutdown_tx), completion_rx: Some(completion_rx) }
    }

    fn stop(
        shutdown_tx: Option<tokio::sync::oneshot::Sender<()>>,
        completion_rx: &Receiver<thread::Result<()>>,
    ) -> Result<(), MockServerStopError> {
        if let Some(shutdown_tx) = shutdown_tx {
            let _ = shutdown_tx.send(());
        }

        wait_for_server_completion(completion_rx, MOCK_SERVER_SHUTDOWN_TIMEOUT)
    }
}

fn wait_for_server_completion(
    completion_rx: &Receiver<thread::Result<()>>,
    timeout: Duration,
) -> Result<(), MockServerStopError> {
    match completion_rx.recv_timeout(timeout) {
        Ok(Ok(())) => Ok(()),
        Ok(Err(server_panic)) => Err(MockServerStopError::Panicked(server_panic)),
        Err(RecvTimeoutError::Timeout) => Err(MockServerStopError::TimedOut(timeout)),
        Err(RecvTimeoutError::Disconnected) => Err(MockServerStopError::Disconnected),
    }
}

impl Drop for MockServerInfo {
    fn drop(&mut self) {
        let completion_rx = self.completion_rx.take().expect("mock server completion receiver");
        if let Err(error) = Self::stop(self.shutdown_tx.take(), &completion_rx) {
            if thread::panicking() {
                eprintln!("Mock server teardown also failed: {error}");
            } else if let MockServerStopError::Panicked(server_panic) = error {
                panic::resume_unwind(server_panic);
            } else {
                panic!("Mock server teardown failed: {error}");
            }
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum FailureKind {
    GraphQl,
    CreatePr,
    UpdatePr,
}

impl Drop for TestContext {
    fn drop(&mut self) {
        // Stop the server before fixture directories and state are released.
        drop(self.mock_server.take());
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
            cmd.env("GHERRIT_TEST_BINARY", &self.gherrit_bin_path);

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

    pub fn inject_failure(&self, kind: FailureKind) {
        assert!(self.has_mock_github, "missing test capability: .with_mock_github()");
        let mut state = self.mock_state().write().unwrap();

        state.fail_next_request = Some(kind);
    }

    pub fn assert_failure_consumed(&self) {
        self.inspect_mock_state(|state| {
            assert!(
                state.fail_next_request.is_none(),
                "Expected injected failure to be consumed, but {:?} remains",
                state.fail_next_request
            );
        });
    }

    pub fn inspect_mock_state(&self, f: impl FnOnce(&mock_server::MockState)) {
        let state = self.mock_state().read().unwrap();
        f(&state);
    }

    pub fn mutate_mock_state(&self, f: impl FnOnce(&mut mock_server::MockState)) {
        let mut state = self.mock_state().write().unwrap();
        f(&mut state);
    }

    pub fn formatted_mock_pr_state(&self) -> String {
        assert!(self.has_mock_github, "missing test capability: .with_mock_github()");
        let mut content = String::new();
        self.inspect_mock_state(|state| {
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
        assert!(self.has_git_interceptor, "missing test capability: .with_git_interceptor()");
        let mut count = 0;
        self.inspect_mock_state(|state| {
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

fn install_git_interceptor(path: &Path) {
    let git = path.join(GIT_INTERCEPTOR_NAME);

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;

        fs::write(&git, "#!/bin/sh\nexec \"$GHERRIT_TEST_BINARY\" __test-git \"$@\"\n").unwrap();
        fs::set_permissions(&git, fs::Permissions::from_mode(0o755)).unwrap();
    }

    #[cfg(windows)]
    fs::write(git, "@echo off\r\n\"%GHERRIT_TEST_BINARY%\" __test-git %*\r\n").unwrap();
}

fn install_gherrit_binary(path: &Path, gherrit_bin: &Path) {
    let gherrit_dst = path.join(if cfg!(windows) { "gherrit.exe" } else { "gherrit" });
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
    remote_path: Option<&Path>,
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
    if let Some(remote_path) = remote_path {
        run(&["remote", "add", "origin", remote_path.to_str().unwrap()]);
    }
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
mod tests {
    use std::time::Instant;

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

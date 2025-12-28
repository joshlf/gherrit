use std::{
    env, fs,
    path::{Path, PathBuf},
    process::Command,
    sync::LazyLock,
};

use tempfile::TempDir;

pub mod mock_server;

#[macro_export]
macro_rules! find_mock_bin {
    () => {{
        let gherrit = assert_cmd::cargo::cargo_bin!("gherrit");
        let exe = if cfg!(windows) { "mock_bin.exe" } else { "mock_bin" };
        gherrit.parent().unwrap().join(exe)
    }};
}

#[macro_export]
macro_rules! test_context {
    () => {
        $crate::TestContextBuilder::new()
            .binaries(assert_cmd::cargo::cargo_bin!("gherrit"), $crate::find_mock_bin!())
    };
}

#[macro_export]
macro_rules! test_context_minimal {
    () => {
        $crate::TestContextBuilder::new_minimal()
            .binaries(assert_cmd::cargo::cargo_bin!("gherrit"), $crate::find_mock_bin!())
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
        Self {
            owner: "owner".to_string(),
            name: "repo".to_string(),
            install_hooks: true,
            initial_commit: true,
            gherrit_bin: None,
            mock_bin: None,
        }
    }

    pub fn new_minimal() -> Self {
        Self {
            owner: "owner".to_string(),
            name: "repo".to_string(),
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

        let remote_path = dir.path().join("remote.git");
        init_git_bare_repo(&remote_path);

        let is_live = env::var("GHERRIT_LIVE_TEST").is_ok();

        // Resolve system git before we mess with PATH.
        let system_git = SYSTEM_GIT.clone();

        init_git_repo(&repo_path, &remote_path);

        let gherrit_bin = self.gherrit_bin.clone().expect("gherrit binary path must be set");
        let mock_bin = self.mock_bin.clone().expect("mock binary path must be set");

        let mock_server = (!is_live).then(|| {
            install_mock_binaries(dir.path(), &mock_bin, &gherrit_bin);
            let state = mock_server::MockState {
                repo_owner: self.owner.clone(),
                repo_name: self.name.clone(),
                ..Default::default()
            };
            let state_json = serde_json::to_string(&state).unwrap();
            let state_path = repo_path.join("mock_state.json");
            fs::write(&state_path, state_json).unwrap();

            // Spawn the server on a separate thread to avoid blocking the main
            // test thread. This ensures the runtime persists for the duration
            // of the test context.

            let (tx, rx) = std::sync::mpsc::channel();
            let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel::<()>();

            std::thread::spawn(move || {
                let rt = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .expect("Failed to build runtime");

                rt.block_on(async {
                    let url = mock_server::start_mock_server(state_path).await;
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
}

pub struct MockServerInfo {
    pub url: String,
    pub shutdown_tx: tokio::sync::oneshot::Sender<()>,
}

impl Drop for TestContext {
    fn drop(&mut self) {
        if let Some(server) = self.mock_server.take() {
            let _ = server.shutdown_tx.send(());
        }
    }
}

impl TestContext {
    pub fn gherrit(&self) -> assert_cmd::Command {
        // Use injected binary path
        let mut cmd = assert_cmd::Command::new(&self.gherrit_bin_path);
        cmd.current_dir(&self.repo_path);

        if !self.is_live {
            // Prepend temp dir to PATH so 'gh' and 'git' resolve to our mock
            let mut paths = vec![self.dir.path().to_path_buf()];
            paths.extend(env::split_paths(&env::var_os("PATH").unwrap()));

            let new_path_str = env::join_paths(paths).unwrap();
            cmd.env("PATH", new_path_str);
            cmd.env("SYSTEM_GIT_PATH", &self.system_git);

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

        if !self.is_live {
            let mut paths = vec![self.dir.path().to_path_buf()];
            paths.extend(env::split_paths(&env::var_os("PATH").unwrap()));
            let new_path_str = env::join_paths(paths).unwrap();
            cmd.env("PATH", new_path_str);
            cmd.env("SYSTEM_GIT_PATH", &self.system_git);
        }

        cmd
    }

    pub fn run_git(&self, args: &[&str]) {
        self.git().args(args).assert().success();
    }

    pub fn git(&self) -> assert_cmd::Command {
        let mut cmd = assert_cmd::Command::new("git");
        cmd.current_dir(&self.repo_path);

        if !self.is_live {
            let mut paths = vec![self.dir.path().to_path_buf()];
            paths.extend(env::split_paths(&env::var_os("PATH").unwrap()));
            let new_path_str = env::join_paths(paths).unwrap();
            cmd.env("PATH", new_path_str);
            cmd.env("SYSTEM_GIT_PATH", &self.system_git);
        }

        cmd
    }

    pub fn read_mock_state(&self) -> mock_server::MockState {
        let content = fs::read_to_string(self.repo_path.join("mock_state.json"))
            .expect("Failed to read mock_state.json");
        serde_json::from_str(&content).expect("Failed to parse mock state")
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

    pub fn inject_failure(&self, request_type: &str, remaining: usize) {
        let mut state = self.read_mock_state();
        state.fail_next_request = Some(request_type.to_string());
        state.fail_remaining = remaining;
        mock_server::write_state(&self.repo_path.join("mock_state.json"), &state);
    }
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

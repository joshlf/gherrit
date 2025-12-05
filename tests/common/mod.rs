use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::LazyLock;
use tempfile::TempDir;

pub struct TestContext {
    pub dir: TempDir,
    pub repo_path: PathBuf,
    pub is_live: bool,
    pub system_git: PathBuf,
}

impl TestContext {
    /// Allocates a new temporary directory and initializes a git repository in it.
    pub fn init() -> Self {
        let dir = TempDir::new().unwrap();
        let repo_path = dir.path().to_path_buf();
        let is_live = env::var("GHERRIT_LIVE_TEST").is_ok();

        // Resolve system git before we mess with PATH.
        let system_git = SYSTEM_GIT.clone();

        init_git_repo(&repo_path);
        if !is_live {
            install_mock_binaries(dir.path());
        }

        Self {
            dir,
            repo_path,
            is_live,
            system_git,
        }
    }

    pub fn gherrit(&self) -> assert_cmd::Command {
        let bin_path = env!("CARGO_BIN_EXE_gherrit");
        let mut cmd = assert_cmd::Command::new(bin_path);
        cmd.current_dir(&self.repo_path);

        if !self.is_live {
            // Prepend temp dir to PATH so 'gh' and 'git' resolve to our mock
            let mut paths = vec![self.dir.path().to_path_buf()];
            paths.extend(env::split_paths(&env::var_os("PATH").unwrap()));

            let new_path_str = env::join_paths(paths).unwrap();
            cmd.env("PATH", new_path_str);
            cmd.env("SYSTEM_GIT_PATH", &self.system_git);
        }

        cmd
    }

    pub fn run_git(&self, args: &[&str]) {
        let mut cmd = assert_cmd::Command::new("git");
        cmd.current_dir(&self.repo_path);
        cmd.args(args);

        if !self.is_live {
            let mut paths = vec![self.dir.path().to_path_buf()];
            paths.extend(env::split_paths(&env::var_os("PATH").unwrap()));
            let new_path_str = env::join_paths(paths).unwrap();
            cmd.env("PATH", new_path_str);
            cmd.env("SYSTEM_GIT_PATH", &self.system_git);
        }

        cmd.assert().success();
    }

    pub fn read_mock_state(&self) -> MockState {
        let content = fs::read_to_string(self.repo_path.join("mock_state.json"))
            .expect("Failed to read mock_state.json");
        serde_json::from_str(&content).expect("Failed to parse mock state")
    }

    pub fn install_hooks(&self) {
        let hooks_dir = self.repo_path.join(".git").join("hooks");
        fs::create_dir_all(&hooks_dir).expect("Failed to create hooks dir");

        self.install_hook(&hooks_dir, "commit-msg");
        self.install_hook(&hooks_dir, "pre-push");
        self.install_hook(&hooks_dir, "post-checkout");
    }

    fn install_hook(&self, dir: &Path, name: &str) {
        let file_path = dir.join(name);
        let bin_path = env!("CARGO_BIN_EXE_gherrit");
        let script = format!("#!/bin/sh\n{:?} hook {} \"$@\"\n", bin_path, name);

        fs::write(&file_path, script).expect("Failed to write hook script");

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perms = fs::metadata(&file_path).unwrap().permissions();
            perms.set_mode(0o755);
            fs::set_permissions(&file_path, perms).expect("Failed to set hook permissions");
        }
    }
}

fn run_git_cmd(path: &Path, args: &[&str]) {
    assert_cmd::Command::new("git")
        .current_dir(path)
        .args(args)
        .assert()
        .success();
}

#[derive(serde::Deserialize, Debug)]
pub struct MockState {
    pub prs: Vec<PrEntry>,
    pub pushed_refs: Vec<String>,
}

#[derive(serde::Deserialize, Debug)]
#[expect(dead_code)]
pub struct PrEntry {
    pub number: usize,
    pub title: String,
    pub body: String,
}

fn install_mock_binaries(path: &Path) {
    let mock_bin = PathBuf::from(env!("CARGO_BIN_EXE_mock_bin"));
    let gherrit_bin = PathBuf::from(env!("CARGO_BIN_EXE_gherrit"));

    let git_dst = path.join(if cfg!(windows) { "git.exe" } else { "git" });
    let gh_dst = path.join(if cfg!(windows) { "gh.exe" } else { "gh" });
    let gherrit_dst = path.join(if cfg!(windows) {
        "gherrit.exe"
    } else {
        "gherrit"
    });

    fs::copy(&mock_bin, &git_dst).unwrap();
    fs::copy(&mock_bin, &gh_dst).unwrap();
    fs::copy(&gherrit_bin, &gherrit_dst).unwrap();
}

fn init_git_repo(path: &Path) {
    run_git_cmd(path, &["init"]);
    // Must config user identity for commits to work
    run_git_cmd(path, &["config", "user.email", "test@example.com"]);
    run_git_cmd(path, &["config", "user.name", "Test User"]);
    // Ensure default branch is main
    run_git_cmd(path, &["symbolic-ref", "HEAD", "refs/heads/main"]);
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

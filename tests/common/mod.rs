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
    pub namespace: String,
}

impl TestContext {
    /// Allocates a new temporary directory and initializes a git repository in it.
    pub fn init() -> Self {
        let dir = TempDir::new().unwrap();
        let repo_path = dir.path().to_path_buf();
        let is_live = env::var("GHERRIT_TEST_REMOTE_URL").is_ok();

        // Resolve system git before we mess with PATH.
        let system_git = SYSTEM_GIT.clone();

        // Generate unique namespace
        let id: u64 = rand::random();
        let namespace = format!("test-{}", id);
        let root_branch = format!("{}/main", namespace);

        init_git_repo(&repo_path);

        // Configure namespaced trunk
        run_git_cmd(&repo_path, &["branch", "-m", &root_branch]);
        run_git_cmd(&repo_path, &["config", "init.defaultBranch", &root_branch]);

        if is_live {
            let remote_url = env::var("GHERRIT_TEST_REMOTE_URL").unwrap();
            run_git_cmd(&repo_path, &["remote", "add", "origin", &remote_url]);
            // We need to push the root branch to seed the remote
            run_git_cmd(&repo_path, &["push", "-u", "origin", &root_branch]);
        } else {
            install_mock_binaries(dir.path());
        }

        Self {
            dir,
            repo_path,
            is_live,
            system_git,
            namespace,
        }
    }

    pub fn main_branch_name(&self) -> String {
        format!("{}/main", self.namespace)
    }

    pub fn init_and_install_hooks() -> Self {
        let ctx = Self::init();
        ctx.install_hooks();
        ctx
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

    pub fn read_mock_state(&self) -> MockState {
        let content = fs::read_to_string(self.repo_path.join("mock_state.json"))
            .expect("Failed to read mock_state.json");
        serde_json::from_str(&content).expect("Failed to parse mock state")
    }

    pub fn install_hooks(&self) {
        // Use the new install command
        self.gherrit().args(["install"]).assert().success();
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
    #[serde(default)]
    pub push_count: usize,
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

/// The state of a stack of commits, using `git log` as the source of truth.
pub struct StackVerifier<'a> {
    ctx: &'a TestContext,
    commits: Vec<CommitInfo>,
}

#[derive(Debug)]
struct CommitInfo {
    oid: String,
    title: String,
    g_id: String,
}

impl<'a> StackVerifier<'a> {
    /// Uses `git log` to construct a verifier.
    pub fn from_git_log(ctx: &'a TestContext) -> Self {
        // Fetch history in reverse order (oldest -> newest)
        // Format: OID|Title|Trailer
        let output = ctx
            .git()
            .args([
                "log",
                "--format=%H|%s|%(trailers:key=gherrit-pr-id,valueonly,separator=)",
                "--reverse",
            ])
            .output()
            .expect("Failed to get git log");

        let stdout = String::from_utf8(output.stdout).expect("Invalid UTF-8");
        let commits: Vec<CommitInfo> = stdout
            .lines()
            .filter_map(|line| {
                let parts: Vec<&str> = line.split('|').collect();
                let [oid, title, g_id] = parts.as_slice() else {
                    return None;
                };
                if g_id.is_empty() {
                    return None;
                }
                let oid = oid.to_string();
                let title = title.to_string();
                let g_id = g_id.trim().to_string();

                Some(CommitInfo { oid, title, g_id })
            })
            .collect();

        Self { ctx, commits }
    }

    pub fn verify_pushed_refs(&self, version: usize) {
        let state = self.ctx.read_mock_state();
        for commit in &self.commits {
            // Check for expected phantom branch ref
            let expected_branch_ref = format!("{}:refs/heads/{}", commit.oid, commit.g_id);
            assert!(
                state.pushed_refs.contains(&expected_branch_ref),
                "Missing expected branch ref: {}",
                expected_branch_ref
            );

            // Check for expected version tag ref
            let expected_tag_ref = format!(
                "{}:refs/tags/gherrit/{}/v{}",
                commit.oid, commit.g_id, version
            );
            assert!(
                state.pushed_refs.contains(&expected_tag_ref),
                "Missing expected tag ref: {}",
                expected_tag_ref
            );
        }
    }

    pub fn verify_pr_bodies(&self) {
        let state = self.ctx.read_mock_state();
        for (i, commit) in self.commits.iter().enumerate() {
            // Find PR by title
            let pr = state
                .prs
                .iter()
                .find(|pr| pr.title == commit.title)
                .unwrap_or_else(|| panic!("PR not found for commit: {}", commit.title));

            // Construct expected JSON
            let parent = if i > 0 {
                format!("\"{}\"", self.commits[i - 1].g_id)
            } else {
                "null".to_string()
            };

            let child = if i < self.commits.len() - 1 {
                format!("\"{}\"", self.commits[i + 1].g_id)
            } else {
                "null".to_string()
            };

            // Use strict JSON formatting
            // Note: In verify logic, we want to ensure the metadata block is present and correct.
            let expected_json = format!(
                r#"{{"id": "{}", "parent": {}, "child": {}}}"#,
                commit.g_id, parent, child
            );

            assert!(
                pr.body.contains(&expected_json),
                "PR body for '{}' (ID: {}) missing expected JSON metadata.\nExpected: {}\nActual Body:\n{}",
                commit.title,
                commit.g_id,
                expected_json,
                pr.body
            );
        }
    }
}

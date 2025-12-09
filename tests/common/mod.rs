use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::LazyLock;
use tempfile::TempDir;

pub struct TestContext {
    pub dir: TempDir,
    pub repo_path: PathBuf,
    pub remote_path: PathBuf,
    pub is_live: bool,
    pub system_git: PathBuf,
    pub namespace: String,
}

#[allow(dead_code)]
impl TestContext {
    /// Allocates a new temporary directory and initializes a git repository in it.
    pub fn init() -> Self {
        Self::init_with_repo("owner", "repo")
    }

    pub fn init_with_repo(owner: &str, name: &str) -> Self {
        let dir = TempDir::new().unwrap();
        let repo_path = dir.path().join("local");
        fs::create_dir(&repo_path).unwrap();

        let remote_path = dir.path().join("remote.git");
        init_git_bare_repo(&remote_path);

        let is_live = env::var("GHERRIT_TEST_REMOTE_URL").is_ok();

        // Resolve system git before we mess with PATH.
        let system_git = SYSTEM_GIT.clone();

        // Generate unique namespace
        let id: u64 = rand::random();
        let namespace = format!("test-{}", id);
        let root_branch = format!("{}/main", namespace);

        init_git_repo(&repo_path, &remote_path);

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
            // Create initial mock state with custom repo
            let state = MockState {
                repo_owner: owner.to_string(),
                repo_name: name.to_string(),
                ..Default::default()
            };
            let state_json = serde_json::to_string(&state).unwrap();
            fs::write(repo_path.join("mock_state.json"), state_json).unwrap();
        }

        Self {
            dir,
            repo_path,
            remote_path,
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

#[derive(serde::Serialize, serde::Deserialize, Debug, Default)]
pub struct MockState {
    #[serde(default)]
    pub prs: Vec<PrEntry>,
    #[serde(default)]
    pub pushed_refs: Vec<String>,
    #[serde(default)]
    pub push_count: usize,
    #[serde(default)]
    pub repo_owner: String,
    #[serde(default)]
    pub repo_name: String,
}

#[derive(serde::Serialize, serde::Deserialize, Debug, Clone, Default)]
#[allow(dead_code)]
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

fn init_git_bare_repo(path: &Path) {
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
    // Add origin remote
    run_git_cmd(
        path,
        &["remote", "add", "origin", remote_path.to_str().unwrap()],
    );
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
    body: String,
    g_id: String,
}

impl<'a> StackVerifier<'a> {
    /// Uses `git log` to construct a verifier.
    pub fn from_git_log(ctx: &'a TestContext) -> Self {
        // Fetch history in reverse order (oldest -> newest). Format:
        //
        //   OID\0Title\0Trailer\0Body\0
        //
        // We use null byte delimiters to handle newlines safeley.
        let output = ctx
            .git()
            .args([
                "log",
                "--format=%H%x00%s%x00%(trailers:key=gherrit-pr-id,valueonly,separator=)%x00%b%x00",
                "--reverse",
            ])
            .output()
            .expect("Failed to get git log");

        let stdout = String::from_utf8(output.stdout).expect("Invalid UTF-8");

        // Split by null byte.
        //
        // Each record has 4 fields + 1 record separator (effectively).
        // The last split element will be empty if string ends with \0.
        let parts: Vec<&str> = stdout.split_terminator('\0').collect();

        let commits: Vec<CommitInfo> = parts
            .chunks_exact(4)
            .filter_map(|chunk| {
                let [oid, title, g_id, body] = chunk else {
                    return None;
                };

                let g_id = g_id.trim().to_string();
                if g_id.is_empty() {
                    return None;
                }

                Some(CommitInfo {
                    oid: oid.trim().to_string(),
                    title: title.to_string(),
                    body: body.to_string(),
                    g_id,
                })
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

            // Verify body content
            //
            // `git log --format=%b` includes trailers, but `gherrit` explicitly strips the
            // internal `gherrit-pr-id` trailer before generating the PR body. We match that
            // behavior here to ensure strict equality for the rest of the content.
            let expected_body = commit
                .body
                .lines()
                .filter(|l| !l.starts_with("gherrit-pr-id:"))
                .collect::<Vec<_>>()
                .join("\n");
            let expected_body = expected_body.trim();

            assert!(
                pr.body.contains(expected_body),
                "PR body for '{}' (ID: {}) missing commit message body.\nExpected content: {}\nActual Body:\n{}",
                commit.title,
                commit.g_id,
                expected_body,
                pr.body
            );

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

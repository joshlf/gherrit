# Task: Add Optimistic Locking Test with Real Git Remote

The goal is to enhance integration tests to verify `gherrit`'s optimistic locking mechanism (using `git push --atomic --force-with-lease`) by simulating a race condition against a real local git remote.

## 1. Update `tests/common/mod.rs`

We need to expose the `remote_path` in `TestContext` and add a `remote_git()` helper to interact with the bare remote repository directly.

**Action:** Replace `tests/common/mod.rs` with the following content:

```rust
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
}

impl TestContext {
    /// Allocates a new temporary directory and initializes a git repository in it.
    pub fn init() -> Self {
        let dir = TempDir::new().unwrap();
        let repo_path = dir.path().join("local");
        fs::create_dir(&repo_path).unwrap();

        let remote_path = dir.path().join("remote.git");
        init_git_bare_repo(&remote_path);

        let is_live = env::var("GHERRIT_LIVE_TEST").is_ok();

        // Resolve system git before we mess with PATH.
        let system_git = SYSTEM_GIT.clone();

        init_git_repo(&repo_path, &remote_path);
        if !is_live {
            install_mock_binaries(dir.path());
        }

        Self {
            dir,
            repo_path,
            remote_path,
            is_live,
            system_git,
        }
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
```

## 2. Update `tests/integration_tests.rs`

We need to:
1.  Modify `test_version_increment` to confirm refs exist on the remote.
2.  Add `test_optimistic_locking_conflict`.

**Important Implementation Detail:** In `test_optimistic_locking_conflict`, we must use `git commit --amend -m ...` to change the commit SHA (creating a conflict). However, `git commit -m` destroys existing trailers (like `gherrit-pr-id`). If this trailer is lost, `gherrit` treats it as a new stack and generates a new ID, avoiding the conflict. We **must manually preserve the `gherrit-pr-id` trailer** in the amended message.

**Action:** Apply the following changes to `tests/integration_tests.rs` (using `replace_with_git_merge_diff`):

```rust
<<<<<<< SEARCH
    // Push 2 (v2)
    ctx.gherrit().args(["hook", "pre-push"]).assert().success();

    if !ctx.is_live {
        let state = ctx.read_mock_state();
        let has_v2 = state.pushed_refs.iter().any(|r| r.contains("/v2"));
        assert!(
            has_v2,
            "Expected v2 tag to be pushed. Refs: {:?}",
            state.pushed_refs
        );

        // We check the *new* pushes only.
        let new_pushes = &state.pushed_refs[pushed_count_v1..];
        let v1_repush = new_pushes.iter().any(|r| r.contains("/v1"));
        assert!(
            !v1_repush,
            "v1 tag should NOT be pushed again in the second push. New pushes: {:?}",
            new_pushes
        );
    }
}

#[test]
fn test_pr_body_generation() {
=======
    // Push 2 (v2)
    ctx.gherrit().args(["hook", "pre-push"]).assert().success();

    if !ctx.is_live {
        let state = ctx.read_mock_state();
        let has_v2 = state.pushed_refs.iter().any(|r| r.contains("/v2"));
        assert!(
            has_v2,
            "Expected v2 tag to be pushed. Refs: {:?}",
            state.pushed_refs
        );

        // We check the *new* pushes only.
        let new_pushes = &state.pushed_refs[pushed_count_v1..];
        let v1_repush = new_pushes.iter().any(|r| r.contains("/v1"));
        assert!(
            !v1_repush,
            "v1 tag should NOT be pushed again in the second push. New pushes: {:?}",
            new_pushes
        );

        // Verify that tags actually exist on the remote
        let output = ctx.remote_git().args(["tag", "-l"]).output().unwrap();
        let tags = std::str::from_utf8(&output.stdout).unwrap();
        assert!(tags.contains("/v1"), "Remote should contain v1 tag");
        assert!(tags.contains("/v2"), "Remote should contain v2 tag");
    }
}

#[test]
fn test_optimistic_locking_conflict() {
    let ctx = TestContext::init_and_install_hooks();

    // 1. Initial setup
    ctx.run_git(&["commit", "--allow-empty", "-m", "Initial Commit"]);
    ctx.run_git(&["checkout", "-b", "feature-conflict"]);
    ctx.run_git(&["commit", "--allow-empty", "-m", "Commit V1"]);

    ctx.gherrit().args(["manage"]).assert().success();

    // 2. Push V1
    ctx.gherrit().args(["hook", "pre-push"]).assert().success();

    // Retrieve the gherrit_id from local refs
    let output = ctx
        .git()
        .args(["for-each-ref", "--format=%(refname:short)", "refs/gherrit/"])
        .output()
        .unwrap();
    let stdout = std::str::from_utf8(&output.stdout).unwrap();
    let gherrit_id = stdout
        .lines()
        .next()
        .expect("No gherrit ref found")
        .strip_prefix("gherrit/")
        .expect("Invalid ref format");

    // 3. Simulate race condition: Create v2 tag on REMOTE manually
    // The next version should be v2 (since v1 exists).
    // Note: In bare repo, we can create refs directly.
    let tag_name = format!("gherrit/{}/v2", gherrit_id);

    // Create tag pointing to the branch we just pushed
    ctx.remote_git()
        .args(["tag", &tag_name, &format!("refs/heads/{}", gherrit_id)])
        .assert()
        .success();

    // 4. Create local commit for V2 (modify to ensure new hash)
    // Note: We change the message to guarantee a different SHA even if running quickly.
    // We MUST preserve the Change-ID to simulate an update to the SAME stack.
    let new_msg = format!("Commit V1 (Amended)\n\ngherrit-pr-id: {}", gherrit_id);
    ctx.run_git(&[
        "commit",
        "--amend",
        "--allow-empty",
        "-m",
        &new_msg,
    ]);

    // 5. Attempt push - should fail due to atomic lock
    let output = ctx.gherrit().args(["hook", "pre-push"]).assert().failure();

    let stderr = std::str::from_utf8(&output.get_output().stderr).unwrap();
    assert!(
        stderr.contains("`git push` failed"),
        "Expected push failure due to lock, got: {}",
        stderr
    );
}

#[test]
fn test_pr_body_generation() {
>>>>>>> REPLACE
```

## 3. Verify

Run `cargo test` to ensure all tests pass, especially `test_optimistic_locking_conflict`.

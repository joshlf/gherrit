mod common;
use common::TestContext;

#[test]
fn test_commit_msg_hook() {
    let ctx = TestContext::init();
    let msg_file = ctx.repo_path.join("COMMIT_EDITMSG");
    std::fs::write(&msg_file, "feat: my cool feature").unwrap();

    // Must manage the branch first so the hook runs
    ctx.gherrit().args(["manage"]).assert().success();

    // Run hook
    ctx.gherrit()
        .args(["hook", "commit-msg", msg_file.to_str().unwrap()])
        .assert()
        .success();

    // Verify trailer was added
    let content = std::fs::read_to_string(msg_file).unwrap();
    assert!(content.contains("\ngherrit-pr-id: G"));
}

#[test]
fn test_full_stack_lifecycle_mocked() {
    let ctx = TestContext::init();
    ctx.install_hooks();

    // Setup: Create 'main' and a feature branch
    ctx.run_git(&["commit", "--allow-empty", "-m", "Initial Commit"]);
    ctx.run_git(&["checkout", "-b", "feature-stack"]);

    // Manage the branch properly before making commits so hooks are installed
    ctx.gherrit().args(["manage"]).assert().success();

    ctx.run_git(&["commit", "--allow-empty", "-m", "Commit A"]);

    ctx.run_git(&["commit", "--allow-empty", "-m", "Commit B"]);

    // Trigger Pre-Push Hook (Simulate 'git push'). We call the hook directly
    // because simulating a real 'git push' that calls the hook recursively is
    // complex in a test env.
    ctx.gherrit().args(["hook", "pre-push"]).assert().success();

    // Verify Side Effects (Mock Only)
    if !ctx.is_live {
        let state = ctx.read_mock_state();
        assert_eq!(state.prs.len(), 2, "Expected 2 PRs created");
        // Verify we pushed phantom branches or tags. The mock intercepts 'git
        // push origin <refspec>...'. GHerrit pushes refspecs like
        // 'refs/heads/G...:refs/heads/G...' or tags.
        assert!(
            !state.pushed_refs.is_empty(),
            "Expected some refs to be pushed"
        );
    }
}

#[test]
fn test_branch_management() {
    let ctx = TestContext::init();
    ctx.install_hooks();

    // Create a branch to manage
    ctx.run_git(&["checkout", "-b", "feature-A"]);

    // Scenario A: Custom Push Remote Preservation
    ctx.run_git(&["config", "branch.feature-A.pushRemote", "origin"]);

    ctx.gherrit().args(["manage"]).assert().success();

    // Assert managed
    ctx.git()
        .args(["config", "branch.feature-A.gherritManaged"])
        .assert()
        .success()
        .stdout("true\n");

    // Assert pushRemote preserved
    ctx.git()
        .args(["config", "branch.feature-A.pushRemote"])
        .assert()
        .success()
        .stdout("origin\n");

    // Assert other keys set
    ctx.git()
        .args(["config", "branch.feature-A.remote"])
        .assert()
        .success()
        .stdout(".\n");
    ctx.git()
        .args(["config", "branch.feature-A.merge"])
        .assert()
        .success()
        .stdout("refs/heads/feature-A\n");

    // Scenario B: Unmanage Cleanup
    ctx.gherrit().args(["unmanage"]).assert().success();

    // Assert unmanaged (key exists but is false)
    ctx.git()
        .args(["config", "branch.feature-A.gherritManaged"])
        .assert()
        .success()
        .stdout("false\n");

    // Assert cleanup (keys should be unset)
    ctx.git()
        .args(["config", "branch.feature-A.remote"])
        .assert()
        .failure();
    ctx.git()
        .args(["config", "branch.feature-A.merge"])
        .assert()
        .failure();

    // Assert pushRemote preserved
    ctx.git()
        .args(["config", "branch.feature-A.pushRemote"])
        .assert()
        .success()
        .stdout("origin\n");
}

#[test]
fn test_post_checkout_hook() {
    let ctx = TestContext::init();
    ctx.install_hooks();

    // Scenario A: New Feature Branch (Stack Mode)
    // -------------------------------------------
    // Must have a commit so HEAD is valid
    ctx.run_git(&["commit", "--allow-empty", "-m", "Initial Commit"]);

    ctx.run_git(&["checkout", "-b", "feature-stack"]);

    // Manually invoke the hook (Simulation of git calling it)
    // args: prev_sha new_sha flag(1=branch checkout)
    ctx.gherrit()
        .args(["hook", "post-checkout", "HEAD", "HEAD", "1"])
        .assert()
        .success();

    // Assert managed = true
    ctx.git()
        .args(["config", "branch.feature-stack.gherritManaged"])
        .assert()
        .success()
        .stdout("true\n");

    // Scenario B: Existing Branch (Collaboration Mode)
    // ------------------------------------------------
    // Setup a fake remote tracking branch
    // We switch back to main first to create a fresh branch from
    ctx.run_git(&["checkout", "main"]);

    // Create the remote ref 'refs/remotes/origin/collab-feature' pointing to HEAD
    ctx.run_git(&["update-ref", "refs/remotes/origin/collab-feature", "HEAD"]);

    // Define 'origin' remote so --track works
    ctx.run_git(&["remote", "add", "origin", "."]);

    // Checkout tracking branch atomically so config is set when hook runs
    ctx.run_git(&[
        "checkout",
        "-b",
        "collab-feature",
        "--track",
        "origin/collab-feature",
    ]);

    // Manually invoke hook
    ctx.gherrit()
        .args(["hook", "post-checkout", "HEAD", "HEAD", "1"])
        .assert()
        .success();

    // Assert managed = false
    ctx.git()
        .args(["config", "branch.collab-feature.gherritManaged"])
        .assert()
        .success()
        .stdout("false\n");
}

#[test]
fn test_commit_msg_edge_cases() {
    let ctx = TestContext::init();
    ctx.install_hooks();
    ctx.run_git(&["commit", "--allow-empty", "-m", "Initial"]);
    // Ensure we are managed so the hook is active
    ctx.gherrit().args(["manage"]).assert().success();

    // Scenario A: Squash Commit
    let squash_msg_file = ctx.repo_path.join("SQUASH_MSG");
    let squash_content = "squash! some other commit";
    std::fs::write(&squash_msg_file, squash_content).unwrap();

    ctx.gherrit()
        .args(["hook", "commit-msg", squash_msg_file.to_str().unwrap()])
        .assert()
        .success();

    let content_after = std::fs::read_to_string(&squash_msg_file).unwrap();
    assert_eq!(
        content_after, squash_content,
        "Commit-msg hook should ignore squash commits"
    );

    // Scenario B: Detached HEAD
    ctx.run_git(&["checkout", "--detach"]);
    let detached_msg_file = ctx.repo_path.join("DETACHED_MSG");
    let detached_content = "feat: detached work";
    std::fs::write(&detached_msg_file, detached_content).unwrap();

    ctx.gherrit()
        .args(["hook", "commit-msg", detached_msg_file.to_str().unwrap()])
        .assert()
        .success();

    let content_after = std::fs::read_to_string(&detached_msg_file).unwrap();
    assert_eq!(
        content_after, detached_content,
        "Commit-msg hook should ignore detached HEAD"
    );
}

#[test]
fn test_pre_push_ancestry_check() {
    let ctx = TestContext::init();
    ctx.install_hooks();

    // Setup: Create a normal history first (common init)
    ctx.run_git(&["commit", "--allow-empty", "-m", "Initial Root"]);

    // Create an orphan branch
    ctx.run_git(&["checkout", "--orphan", "lonely-branch"]);
    // ctx.run_git(&["rm", "--cached", "-r", "."]); // Index is already empty from empty commit
    ctx.run_git(&["commit", "--allow-empty", "-m", "Lonely Commit"]);

    // Manage it (this might succeed or fail depending on implementation,
    // but we care about push failure)
    ctx.gherrit().args(["manage"]).assert().success();

    // Trigger pre-push hook
    // It should fail because it can't find the merge base with 'main'
    let output = ctx.gherrit().args(["hook", "pre-push"]).assert().failure();

    let output = output.get_output();
    let stderr = std::str::from_utf8(&output.stderr).unwrap();

    assert!(
        stderr.contains("not based on") || stderr.contains("share history"),
        "Expected ancestry error, got: {}",
        stderr
    );
}

#[test]
fn test_version_increment() {
    let ctx = TestContext::init();
    ctx.install_hooks();

    // Setup: Initial commit
    ctx.run_git(&["commit", "--allow-empty", "-m", "Initial Commit"]);
    // Create feature branch
    ctx.run_git(&["checkout", "-b", "feat-versioning"]);
    ctx.run_git(&["commit", "--allow-empty", "-m", "Feature Commit"]);

    // Manage
    ctx.gherrit().args(["manage"]).assert().success();

    // Push 1 (v1)
    ctx.gherrit().args(["hook", "pre-push"]).assert().success();

    let mut pushed_count_v1 = 0;
    if !ctx.is_live {
        let state = ctx.read_mock_state();
        let has_v1 = state.pushed_refs.iter().any(|r| r.contains("/v1"));
        assert!(
            has_v1,
            "Expected v1 tag to be pushed. Refs: {:?}",
            state.pushed_refs
        );
        pushed_count_v1 = state.pushed_refs.len();
    }

    // Amend commit (modifies SHA, keeps Change-ID)
    ctx.run_git(&["commit", "--amend", "--allow-empty", "--no-edit"]);

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
    let ctx = TestContext::init();
    ctx.install_hooks();

    // Setup: Initial commit on main to establish the branch
    ctx.run_git(&["commit", "--allow-empty", "-m", "Initial Commit"]);

    // Setup: Stack of 3 commits: A -> B -> C
    // Must be on a feature branch (not main) for gherrit to sync them
    ctx.run_git(&["checkout", "-b", "feature-stack"]);
    ctx.run_git(&["commit", "--allow-empty", "-m", "Commit A"]);
    ctx.run_git(&["commit", "--allow-empty", "-m", "Commit B"]);
    ctx.run_git(&["commit", "--allow-empty", "-m", "Commit C"]);

    // Ensure we capture the Change-IDs (Gherrit-IDs).
    // We can verify this implicitly by checking the PR bodies later.

    // Manage
    ctx.gherrit().args(["manage"]).assert().success();

    // Sync
    ctx.gherrit().args(["hook", "pre-push"]).assert().success();

    // Verify
    if !ctx.is_live {
        let state = ctx.read_mock_state();

        // Find PR for Commit B (should be the middle one, index 1)
        // mock_bin typically creates PRs in order [A, B, C] or based on commit list.
        // We know we created 3 PRs.
        assert_eq!(state.prs.len(), 3, "Expected 3 PRs");

        // The mock bin stores PRs. We need to find the one corresponding to Commit B.
        // Commit B is the parent of C and child of A.
        // Let's filter by title
        let pr_b = state
            .prs
            .iter()
            .find(|pr| pr.title == "Commit B")
            .expect("PR for Commit B not found");

        let body = &pr_b.body;

        // 1. Verify Metadata JSON
        // Should contain <!-- gherrit-meta: { ... } -->
        assert!(
            body.contains("<!-- gherrit-meta: {"),
            "Body missing gherrit-meta block"
        );

        // Verify parent/child keys exist (basic check, since IDs are dynamic)
        assert!(
            body.contains(r#""parent": "G"#),
            "Body missing valid parent field"
        );
        assert!(
            body.contains(r#""child": "G"#),
            "Body missing valid child field"
        );

        // 2. Verify Table
        // For v1, the history table is NOT generated.
        assert!(
            !body.contains("| Version |"),
            "Table should NOT be present for v1"
        );
    }
}

#[test]
fn test_large_stack_batching() {
    let ctx = TestContext::init();
    ctx.install_hooks();

    // Setup: Initial commit on main
    ctx.run_git(&["commit", "--allow-empty", "-m", "Initial Commit"]);

    // Create feature branch
    ctx.run_git(&["checkout", "-b", "large-stack"]);

    // Create 85 commits (exceeds batch limit of 80)
    for i in 1..=85 {
        ctx.run_git(&["commit", "--allow-empty", "-m", &format!("Commit {}", i)]);
    }

    // Manage
    ctx.gherrit().args(["manage"]).assert().success();

    // Sync - should succeed without error
    // Using simple pre-push hook invocation.
    ctx.gherrit().args(["hook", "pre-push"]).assert().success();

    if !ctx.is_live {
        let state = ctx.read_mock_state();

        // Assert: 85 PRs created
        assert_eq!(state.prs.len(), 85, "Expected 85 PRs created");

        // Assert: 85 refs pushed (actually more, since tags are also pushed)
        // Check refs/gherrit/<ID>/v1 count.
        let v1_refs = state
            .pushed_refs
            .iter()
            .filter(|r| !r.starts_with("--"))
            .filter(|r| r.contains("/v1"))
            .count();
        assert_eq!(
            v1_refs,
            85,
            "Expected 85 v1 specific refs pushed. Total pushed: {:?}",
            state.pushed_refs.len()
        );
    }
}

#[test]
fn test_rebase_detection() {
    let ctx = TestContext::init();
    ctx.install_hooks();

    // Setup: Main and feature
    ctx.run_git(&["commit", "--allow-empty", "-m", "Initial"]);
    ctx.run_git(&["checkout", "-b", "feature-rebase"]);
    ctx.run_git(&["commit", "--allow-empty", "-m", "Feature Work"]);

    // Detach HEAD to simulate rebase state
    ctx.run_git(&["checkout", "--detach"]);

    // Create rebase-merge state manually
    let rebase_dir = ctx.repo_path.join(".git/rebase-merge");
    std::fs::create_dir_all(&rebase_dir).unwrap();
    std::fs::write(rebase_dir.join("head-name"), "refs/heads/feature-rebase").unwrap();

    // Run manage - should succeed by detecting 'feature-rebase'
    ctx.gherrit().args(["manage"]).assert().success();

    // Verify config was applied to 'feature-rebase'
    ctx.git()
        .args(["config", "branch.feature-rebase.gherritManaged"])
        .assert()
        .success()
        .stdout("true\n");
}

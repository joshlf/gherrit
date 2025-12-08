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
    let ctx = TestContext::init_and_install_hooks();

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
    let ctx = TestContext::init_and_install_hooks();

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
    let ctx = TestContext::init_and_install_hooks();

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
    let ctx = TestContext::init_and_install_hooks();
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
    let ctx = TestContext::init_and_install_hooks();

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
    let ctx = TestContext::init_and_install_hooks();

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
    let ctx = TestContext::init_and_install_hooks();

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

    // 4. Update to v2 to verify the Patch History Table appears
    ctx.run_git(&["checkout", "feature-stack"]); // Ensure we are on the branch

    // Amend "Commit B" (via tip Commit C) to create v2
    ctx.run_git(&["commit", "--amend", "--allow-empty", "--no-edit"]);

    // Sync again
    ctx.gherrit().args(["hook", "pre-push"]).assert().success();

    if !ctx.is_live {
        let state = ctx.read_mock_state();
        // Find PR for Commit C (the tip)
        let pr_c = state
            .prs
            .iter()
            .find(|pr| pr.title == "Commit C")
            .expect("PR for Commit C not found");

        let body = &pr_c.body;

        // Assert table exists now
        assert!(
            body.contains("| Version |"),
            "Patch History Table should appear for v2"
        );
        assert!(body.contains("v1 |"), "Table should reference v1");
    }
}

#[test]
fn test_large_stack_batching() {
    let ctx = TestContext::init_and_install_hooks();

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

        // Assert: Push was split into 2 batches (80 + 5)
        assert_eq!(
            state.push_count, 2,
            "Expected 2 push invocations for 85 commits (batch size 80)"
        );
    }
}

#[test]
fn test_rebase_detection() {
    let ctx = TestContext::init_and_install_hooks();

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

#[test]
fn test_public_stack_links() {
    let ctx = TestContext::init_and_install_hooks();

    ctx.run_git(&["commit", "--allow-empty", "-m", "Init"]);
    ctx.run_git(&["checkout", "-b", "public-feature"]);
    ctx.run_git(&["commit", "--allow-empty", "-m", "Public Commit"]);

    // 1. Private Mode (Default)
    ctx.gherrit().args(["manage"]).assert().success();
    ctx.gherrit().args(["hook", "pre-push"]).assert().success();

    if !ctx.is_live {
        let state = ctx.read_mock_state();
        let body = &state.prs[0].body;
        assert!(
            !body.contains("This PR is on branch"),
            "Private stack should NOT link to local branch"
        );
    }

    // 2. Public Mode
    // Manually set pushRemote to origin (simulating a public stack)
    ctx.run_git(&["config", "branch.public-feature.pushRemote", "origin"]);

    // Force an update so the body regenerates (amend commit)
    ctx.run_git(&["commit", "--amend", "--allow-empty", "--no-edit"]);
    ctx.gherrit().args(["hook", "pre-push"]).assert().success();

    if !ctx.is_live {
        let state = ctx.read_mock_state();
        let body = &state.prs[0].body; // Get the updated body
        assert!(
            body.contains("This PR is on branch"),
            "Public stack SHOULD link to local branch"
        );
        assert!(
            body.contains("[public-feature]"),
            "Link should mention the branch name"
        );
    }
}

#[test]
fn test_install_command_edge_cases() {
    let ctx = TestContext::init();

    let hooks_dir = ctx.repo_path.join(".git/hooks");
    std::fs::create_dir_all(&hooks_dir).unwrap();
    let pre_push = hooks_dir.join("pre-push");

    // Scenario A: Conflict
    std::fs::write(&pre_push, "foo").unwrap();

    ctx.gherrit()
        .args(["install"])
        .assert()
        .failure() // Should fail
        .stderr(predicates::str::contains("Refusing to overwrite"));

    assert_eq!(std::fs::read_to_string(&pre_push).unwrap(), "foo");

    // Scenario B: Force Overwrite
    ctx.gherrit()
        .args(["install", "--force"])
        .assert()
        .success();

    let content = std::fs::read_to_string(&pre_push).unwrap();
    assert!(content.contains("# gherrit-installer: managed"));

    // Scenario C: Idempotency (Safe to run again)
    ctx.gherrit().args(["install"]).assert().success();

    // Scenario D: Safe Update (Modify but keep sentinel)
    let modified = content + "\n# Some custom comment";
    std::fs::write(&pre_push, modified).unwrap();

    ctx.gherrit()
        .args(["install"]) // Should detect sentinel and update gracefully
        .assert()
        .success();

    // Content should be reset to standard shim (losing custom comment, which is expected behavior for managed hooks)
    let reset_content = std::fs::read_to_string(&pre_push).unwrap();
    assert!(reset_content.contains("# gherrit-installer: managed"));
    assert!(!reset_content.contains("# Some custom comment"));
}

#[test]
fn test_install_configuration_and_security() {
    let ctx = TestContext::init();

    // Scenario A: Automatic Directory Creation (Default Path)
    // -------------------------------------------------------
    // Ensure .git/hooks does not exist (git init might create it depending on version/templates)
    let default_hooks = ctx.repo_path.join(".git/hooks");
    if default_hooks.exists() {
        std::fs::remove_dir_all(&default_hooks).unwrap();
    }

    ctx.gherrit().args(["install"]).assert().success();
    assert!(
        default_hooks.join("pre-push").exists(),
        "Should create directory and install hook"
    );

    // Scenario B: Custom core.hooksPath (Internal)
    // --------------------------------------------
    let custom_internal = ctx.repo_path.join(".githooks");
    ctx.run_git(&["config", "core.hooksPath", ".githooks"]);

    ctx.gherrit().args(["install"]).assert().success();
    assert!(
        custom_internal.join("pre-push").exists(),
        "Should respect core.hooksPath within repo"
    );

    // Scenario C: Custom core.hooksPath (External/Global) - Security Block
    // --------------------------------------------------------------------
    let external_dir = tempfile::TempDir::new().unwrap();
    let ext_path = external_dir.path().to_str().unwrap();

    // We must use absolute path for git config to ensure gherrit sees it as external
    ctx.run_git(&["config", "core.hooksPath", ext_path]);

    ctx.gherrit()
        .args(["install"])
        .assert()
        .failure()
        .stderr(predicates::str::contains("external/global hooks path"));

    assert!(
        !external_dir.path().join("pre-push").exists(),
        "Should NOT install to external path without flag"
    );

    // Scenario D: Custom core.hooksPath (External) - Allow Global
    // -----------------------------------------------------------
    ctx.gherrit()
        .args(["install", "--allow-global"])
        .assert()
        .success();

    assert!(
        external_dir.path().join("pre-push").exists(),
        "Should install to external path with --allow-global"
    );
}

#[test]
fn test_manage_detached_head() {
    let ctx = TestContext::init();
    ctx.run_git(&["commit", "--allow-empty", "-m", "Init"]);

    // Enter detached HEAD state
    ctx.run_git(&["checkout", "--detach"]);

    // Attempt to manage; should fail with specific error
    ctx.gherrit()
        .args(["manage"])
        .assert()
        .failure()
        .stderr(predicates::str::contains(
            "Cannot set state for detached HEAD",
        ));
}

#[test]
fn test_unmanage_cleanup_logic() {
    let ctx = TestContext::init();
    ctx.run_git(&["commit", "--allow-empty", "-m", "Init"]);
    ctx.run_git(&["checkout", "-b", "feature-cleanup"]);

    // Manually configure the state to exact values that trigger the deep cleanup logic
    ctx.run_git(&["config", "branch.feature-cleanup.gherritManaged", "true"]);
    ctx.run_git(&["config", "branch.feature-cleanup.pushRemote", "."]);
    ctx.run_git(&["config", "branch.feature-cleanup.remote", "."]);
    ctx.run_git(&[
        "config",
        "branch.feature-cleanup.merge",
        "refs/heads/feature-cleanup",
    ]);

    // Run unmanage
    ctx.gherrit().args(["unmanage"]).assert().success();

    // Verify cleanup: remote and merge keys should be removed
    ctx.git()
        .args(["config", "branch.feature-cleanup.remote"])
        .assert()
        .failure();
    ctx.git()
        .args(["config", "branch.feature-cleanup.merge"])
        .assert()
        .failure();
    // gherritManaged should be false
    ctx.git()
        .args(["config", "branch.feature-cleanup.gherritManaged"])
        .assert()
        .success()
        .stdout("false\n");
}

#[test]
fn test_pre_push_failure() {
    let ctx = TestContext::init_and_install_hooks();
    ctx.run_git(&["commit", "--allow-empty", "-m", "Init"]);

    ctx.run_git(&["checkout", "-b", "feature-fail"]);
    ctx.run_git(&["commit", "--allow-empty", "-m", "Work to push"]);

    ctx.gherrit().args(["manage"]).assert().success();

    // Configure an invalid remote to trigger `git push` failure
    ctx.run_git(&["remote", "add", "broken-remote", "/path/to/nowhere"]);
    ctx.run_git(&["config", "gherrit.remote", "broken-remote"]);

    ctx.gherrit()
        .args(["hook", "pre-push"])
        .assert()
        .failure()
        .stderr(predicates::str::contains("`git push` failed"));
}

#[test]
#[cfg(unix)]
fn test_install_read_only_fs() {
    use std::os::unix::fs::PermissionsExt;
    let ctx = TestContext::init();
    let hooks_dir = ctx.repo_path.join(".git/hooks");
    std::fs::create_dir_all(&hooks_dir).unwrap();

    // Mark hooks directory read-only
    let mut perms = std::fs::metadata(&hooks_dir).unwrap().permissions();
    perms.set_mode(0o555); // Read/Execute only (no write)
    std::fs::set_permissions(&hooks_dir, perms).unwrap();

    // Attempt installation, verifying failure due to permission denied
    ctx.gherrit().args(["install"]).assert().failure();

    // Cleanup: Restore permissions so TempDir cleanup doesn't panic
    let mut perms = std::fs::metadata(&hooks_dir).unwrap().permissions();
    perms.set_mode(0o755);
    std::fs::set_permissions(&hooks_dir, perms).unwrap();
}

#[test]
fn test_recursive_base_detection() {
    let ctx = TestContext::init_and_install_hooks();
    ctx.run_git(&["commit", "--allow-empty", "-m", "Init"]);

    // 1. Setup: Checkout feature-A from main
    //    Git < 2.3XX or mostly defaults don't set upstream on matching local branch names automatically
    //    without configuration. Ensure it's tracked so GHerrit treats it as valid.
    ctx.run_git(&["checkout", "-b", "feature-A"]);
    ctx.run_git(&["config", "branch.feature-A.remote", "."]);
    ctx.run_git(&["config", "branch.feature-A.merge", "refs/heads/main"]);

    // We must trigger post-checkout logic manually because we configured it AFTER the checkout hook ran.
    // Or just run "gherrit manage" explicitly.
    ctx.gherrit().args(["manage"]).assert().success();

    // Verify feature-A automatically picked up "main" as base
    ctx.git()
        .args(["config", "branch.feature-A.gherritBase"])
        .assert()
        .success()
        .stdout(predicates::str::contains("main"));

    // 2. Recursion: Checkout feature-B from feature-A
    ctx.run_git(&["checkout", "-b", "feature-B"]);
    ctx.run_git(&["config", "branch.feature-B.remote", "."]);
    ctx.run_git(&["config", "branch.feature-B.merge", "refs/heads/feature-A"]);
    ctx.gherrit().args(["manage"]).assert().success();

    //    Should inherit "main" from feature-A
    ctx.git()
        .args(["config", "branch.feature-B.gherritBase"])
        .assert()
        .success()
        .stdout(predicates::str::contains("main"));

    // 3. Manual Override:
    ctx.run_git(&["checkout", "main"]); // Switch back to clear state
    ctx.run_git(&["checkout", "-b", "hotfix-1"]);
    ctx.run_git(&["config", "branch.hotfix-1.remote", "."]);
    ctx.run_git(&["config", "branch.hotfix-1.merge", "refs/heads/main"]);
    ctx.gherrit().args(["manage"]).assert().success(); // Initialize

    // Manual override
    ctx.run_git(&["config", "branch.hotfix-1.gherritBase", "production"]);

    //    Checkout hotfix-1-patch from hotfix-1
    ctx.run_git(&["checkout", "-b", "hotfix-1-patch"]);
    ctx.run_git(&["config", "branch.hotfix-1-patch.remote", "."]);
    ctx.run_git(&[
        "config",
        "branch.hotfix-1-patch.merge",
        "refs/heads/hotfix-1",
    ]);
    ctx.gherrit().args(["manage"]).assert().success();

    //    Should inherit "production"
    ctx.git()
        .args(["config", "branch.hotfix-1-patch.gherritBase"])
        .assert()
        .success()
        .stdout(predicates::str::contains("production"));

    // 4. Remote Edge Case
    ctx.run_git(&["checkout", "-b", "feature-dev"]);
    // Simulate what `git checkout -b feature-dev origin/develop` does config-wise:
    ctx.run_git(&["config", "branch.feature-dev.remote", "origin"]);
    ctx.run_git(&["config", "branch.feature-dev.merge", "refs/heads/develop"]);

    // Manually trigger manage logic
    ctx.gherrit().args(["manage"]).assert().success();

    // Now it should detect "develop" as base.
    ctx.git()
        .args(["config", "branch.feature-dev.gherritBase"])
        .assert()
        .success()
        .stdout(predicates::str::contains("develop"));
}

#[test]
fn test_base_detection_with_slashes() {
    let ctx = TestContext::init_and_install_hooks();
    ctx.run_git(&["commit", "--allow-empty", "-m", "Init"]);

    // 1. Level 1 (Slash Branch)
    // Checkout group/feature from main.
    // Ensure tracking is setup so GHerrit detects upstream.
    ctx.run_git(&["checkout", "-b", "group/feature"]);
    ctx.run_git(&["config", "branch.group/feature.remote", "."]);
    ctx.run_git(&["config", "branch.group/feature.merge", "refs/heads/main"]);

    // Manage
    ctx.gherrit().args(["manage"]).assert().success();

    // Assert base is main
    ctx.git()
        .args(["config", "branch.group/feature.gherritBase"])
        .assert()
        .success()
        .stdout(predicates::str::contains("main"));

    // 2. Level 2 (Deep Nesting)
    // Checkout group/sub-feature from group/feature.
    ctx.run_git(&["checkout", "-b", "group/sub-feature"]);
    ctx.run_git(&["config", "branch.group/sub-feature.remote", "."]);
    ctx.run_git(&[
        "config",
        "branch.group/sub-feature.merge",
        "refs/heads/group/feature",
    ]);

    // Manage
    ctx.gherrit().args(["manage"]).assert().success();

    // Assert base is main (inherited from group/feature -> main)
    ctx.git()
        .args(["config", "branch.group/sub-feature.gherritBase"])
        .assert()
        .success()
        .stdout(predicates::str::contains("main"));

    // 3. Remote Edge Case
    // Mock remote structure
    ctx.run_git(&[
        "config",
        "remote.my-remote.url",
        "https://example.com/repo.git",
    ]);
    ctx.run_git(&[
        "config",
        "remote.my-remote.fetch",
        "+refs/heads/*:refs/remotes/my-remote/*",
    ]);

    // Create a mock remote ref to track
    ctx.run_git(&["update-ref", "refs/remotes/my-remote/release/2.0", "HEAD"]);

    // Checkout hotfix-2 tracking my-remote/release/2.0
    ctx.run_git(&["checkout", "-b", "hotfix-2"]);
    ctx.run_git(&["config", "branch.hotfix-2.remote", "my-remote"]);
    ctx.run_git(&["config", "branch.hotfix-2.merge", "refs/heads/release/2.0"]);
    // (Note: refs/heads/release/2.0 is the name ON REMOTE, which maps to refs/remotes/my-remote/release/2.0 locally)

    // Manage
    ctx.gherrit().args(["manage"]).assert().success();

    // Assert base is release/2.0
    // Logic: Remote upstream -> Use stripped name "release/2.0".
    ctx.git()
        .args(["config", "branch.hotfix-2.gherritBase"])
        .assert()
        .success()
        .stdout(predicates::str::contains("release/2.0"));
}

#[test]
fn test_regression_custom_default_branch() {
    // An earlier version of the code introduced in the commit which introduces
    // this test hardcoded "main" as the default base branch if not configured
    // (even while the base branch was correctly tracked elsewhere in the
    // GHerrit codebase).
    let ctx = TestContext::init_and_install_hooks();

    // Override harness to use "trunk" explicitly
    ctx.run_git(&["branch", "-m", "trunk"]);
    ctx.run_git(&["config", "init.defaultBranch", "trunk"]);

    ctx.run_git(&["commit", "--allow-empty", "-m", "Initial Commit"]);
    ctx.run_git(&["checkout", "-b", "feature-trunk"]);
    ctx.run_git(&["commit", "--allow-empty", "-m", "Feature Commit"]);

    ctx.gherrit().args(["manage"]).assert().success();

    // Trigger pre-push. If logic is buggy, it looks for "main" default and
    // fails/panics. If fixed, it detects "trunk" from config.
    ctx.gherrit().args(["hook", "pre-push"]).assert().success();
}

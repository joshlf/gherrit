#[test]
fn test_commit_msg_hook() {
    let ctx = testutil::test_context_minimal!().build();
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
    let ctx = testutil::test_context!().build();

    // Setup: Create 'main' and a feature branch
    ctx.checkout_new("feature-stack");

    ctx.commit("Commit A");

    ctx.commit("Commit B");

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
    let ctx = testutil::test_context_minimal!()
        .install_hooks(true)
        .build();

    // Create a branch to manage
    ctx.checkout_new("feature-A");

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
    let ctx = testutil::test_context!().build();

    // Scenario A: New Feature Branch

    ctx.checkout_new("feature-stack");

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

    // Scenario B: Existing Branch
    // ------------------------------------------------
    // Setup a fake remote tracking branch. We switch back to main first to
    // create a fresh branch from.
    ctx.run_git(&["checkout", "main"]);

    // Create the remote ref 'refs/remotes/origin/collab-feature' pointing to HEAD
    ctx.run_git(&["update-ref", "refs/remotes/origin/collab-feature", "HEAD"]);

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
    let ctx = testutil::test_context!().build();
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
    let ctx = testutil::test_context_minimal!()
        .install_hooks(true)
        .build();

    // Setup: Create a normal history first (common init)
    ctx.commit("Initial Root");

    // Create an orphan branch
    ctx.run_git(&["checkout", "--orphan", "lonely-branch"]);
    ctx.commit("Lonely Commit");

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
    let ctx = testutil::test_context!().build();

    // Create feature branch
    ctx.checkout_new("feat-versioning");
    ctx.commit("Feature Commit");

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

        // Verify that tags actually exist on the remote
        let output = ctx.remote_git().args(["tag", "-l"]).output().unwrap();
        let tags = std::str::from_utf8(&output.stdout).unwrap();
        assert!(tags.contains("/v1"), "Remote should contain v1 tag");
        assert!(tags.contains("/v2"), "Remote should contain v2 tag");
    }
}

#[test]
fn test_optimistic_locking_conflict() {
    let ctx = testutil::test_context!().build();

    // Initial setup
    ctx.checkout_new("feature-conflict");
    ctx.commit("Commit V1");

    // Push V1
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

    // Simulate race condition: Create v2 tag on REMOTE manually. The next
    // version should be v2 (since v1 exists). Note that in a bare repo, we can
    // create refs directly.
    let tag_name = format!("gherrit/{}/v2", gherrit_id);

    // Create tag pointing to the branch we just pushed
    ctx.remote_git()
        .args(["tag", &tag_name, &format!("refs/heads/{}", gherrit_id)])
        .assert()
        .success();

    // Create local commit for V2 (modify to ensure new hash).
    // Note: We change the message to guarantee a different SHA even if running
    // quickly. We MUST preserve the Change-ID to simulate an update to the SAME
    // stack.
    let new_msg = format!("Commit V1 (Amended)\n\ngherrit-pr-id: {}", gherrit_id);
    ctx.run_git(&["commit", "--amend", "--allow-empty", "-m", &new_msg]);

    // Attempt push - should fail due to atomic lock
    let output = ctx.gherrit().args(["hook", "pre-push"]).assert().failure();

    let stderr = std::str::from_utf8(&output.get_output().stderr).unwrap();
    assert!(
        stderr.contains("`git push` failed"),
        "Expected push failure due to lock, got: {}",
        stderr
    );
    assert!(
        stderr.contains("stale info") || stderr.contains("atomic push failed"),
        "Expected atomic push failure (stale info), got: {}",
        stderr
    );
}

#[test]
fn test_pr_body_generation() {
    let ctx = testutil::test_context!().build();

    // Setup: Stack of 3 commits: A -> B -> C
    // Must be on a feature branch (not main) for gherrit to sync them
    ctx.checkout_new("feature-stack");
    ctx.commit("Commit A");
    ctx.commit("Commit B");
    ctx.commit("Commit C");

    // Ensure we capture the Change-IDs (Gherrit-IDs).
    // We can verify this implicitly by checking the PR bodies later.

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
    let ctx = testutil::test_context!().build();

    // Create feature branch
    ctx.checkout_new("large-stack");

    // Create 85 commits (exceeds batch limit of 80)
    for i in 1..=85 {
        ctx.commit(&format!("Commit {}", i));
    }

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
    let ctx = testutil::test_context!().build();

    ctx.checkout_new("feature-rebase");
    ctx.commit("Feature Work");

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
    let ctx = testutil::test_context_minimal!()
        .install_hooks(true)
        .build();

    ctx.commit("Init");
    // 1. Private Mode (Default)
    ctx.checkout_new("public-feature");
    ctx.commit("Public Commit");

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
    let ctx = testutil::test_context_minimal!().build();

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
    let ctx = testutil::test_context_minimal!().build();

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
    let ctx = testutil::test_context_minimal!().build();
    ctx.commit("Init");

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
    let ctx = testutil::test_context_minimal!().build();
    ctx.commit("Init");
    ctx.checkout_new("feature-cleanup");

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
    let ctx = testutil::test_context_minimal!()
        .install_hooks(true)
        .build();
    ctx.commit("Init");

    ctx.checkout_new("feature-fail");
    ctx.commit("Work to push");

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
    use std::os::unix::fs::PermissionsExt as _;

    // Skip if running as root, as root ignores permissions. This can arise in
    // practice when developing inside a container.
    if unsafe { libc::geteuid() } == 0 {
        return;
    }

    let ctx = testutil::test_context_minimal!().build();
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

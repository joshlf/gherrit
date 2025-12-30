#[test]
fn test_commit_msg_hook() {
    let ctx = testutil::test_context_minimal!().build();
    let msg_file = ctx.repo_path.join("COMMIT_EDITMSG");
    std::fs::write(&msg_file, "feat: my cool feature").unwrap();

    // Must manage the branch first so the hook runs
    ctx.assert_snapshot(&mut ctx.manage(), "commit_msg_hook_manage");

    // Run hook
    ctx.assert_snapshot(
        ctx.gherrit().args(["hook", "commit-msg", msg_file.to_str().unwrap()]),
        "commit_msg_hook_run",
    );

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
    // Verify valid sync
    // Trigge Pre-Push Hook (Simulate 'git push'). We call the hook directly
    // because simulating a real 'git push' that calls the hook recursively is
    // complex in a test env.
    // Verify valid sync
    ctx.assert_snapshot(ctx.gherrit().args(["hook", "pre-push"]), "full_stack_lifecycle_push");

    // Verify Side Effects (Mock Only)
    ctx.maybe_inspect_mock_state(|state| {
        assert_eq!(state.prs.len(), 2, "Expected 2 PRs created");
        // Verify we pushed phantom branches or tags. The mock intercepts 'git
        // push origin <refspec>...'. GHerrit pushes refspecs like
        // 'refs/heads/G...:refs/heads/G...' or tags.
        assert!(!state.pushed_refs.is_empty(), "Expected some refs to be pushed");
    });
}

#[test]
fn test_branch_management() {
    let ctx = testutil::test_context_minimal!().install_hooks(true).build();

    // Create a branch to manage
    ctx.checkout_new("feature-A");

    // Scenario A: Custom Push Remote Preservation
    ctx.run_git(&["config", "branch.feature-A.pushRemote", "origin"]);

    // Attempt manage - should fail (drift)
    ctx.assert_snapshot(&mut ctx.manage(), "branch_management_drift_warning"); // Logs warning, no change
    // Assert still unmanaged (missing key)
    ctx.git().args(["config", "branch.feature-A.gherritManaged"]).assert().failure();

    ctx.assert_snapshot(
        ctx.gherrit().args(["manage", "--force"]),
        "branch_management_force_manage",
    );

    // Assert managed
    ctx.assert_config("branch.feature-A.gherritManaged", Some(testutil::MANAGED_PRIVATE));

    // Assert pushRemote updated to loopback (Private default)
    ctx.assert_config("branch.feature-A.pushRemote", Some("."));

    // Assert other keys set
    ctx.assert_config("branch.feature-A.remote", Some("."));
    ctx.assert_config("branch.feature-A.merge", Some("refs/heads/feature-A"));

    // Scenario B: Unmanage Cleanup
    ctx.assert_snapshot(&mut ctx.unmanage(), "branch_management_unmanage");

    // Assert unmanaged (key exists but is false)
    ctx.assert_config("branch.feature-A.gherritManaged", Some("false"));

    // Assert cleanup (keys should be unset)
    ctx.git().args(["config", "branch.feature-A.remote"]).assert().failure();
    ctx.git().args(["config", "branch.feature-A.merge"]).assert().failure();

    // Assert pushRemote unset
    ctx.git().args(["config", "branch.feature-A.pushRemote"]).assert().failure();
}

#[test]
fn test_post_checkout_hook() {
    let ctx = testutil::test_context!().build();

    // Scenario A: New Feature Branch

    ctx.checkout_new("feature-stack");

    ctx.assert_config("branch.feature-stack.gherritManaged", Some(testutil::MANAGED_PRIVATE));

    // Scenario B: Existing Branch
    // ------------------------------------------------
    // Setup a fake remote tracking branch. We switch back to main first to
    // create a fresh branch from.
    ctx.run_git(&["checkout", "main"]);

    // Create the remote ref 'refs/remotes/origin/collab-feature' pointing to HEAD
    ctx.run_git(&["update-ref", "refs/remotes/origin/collab-feature", "HEAD"]);

    // Checkout tracking branch atomically so config is set when hook runs
    // This implicitly runs post-checkout hook.
    ctx.run_git(&["checkout", "-b", "collab-feature", "--track", "origin/collab-feature"]);

    // Assert managed = false
    ctx.assert_config("branch.collab-feature.gherritManaged", Some("false"));
}

#[test]
fn test_commit_msg_edge_cases() {
    let ctx = testutil::test_context!().build();
    // Ensure we are managed so the hook is active
    ctx.assert_snapshot(&mut ctx.manage(), "commit_msg_edge_manage");

    // Scenario A: Squash Commit
    let squash_msg_file = ctx.repo_path.join("SQUASH_MSG");
    let squash_content = "squash! some other commit";
    std::fs::write(&squash_msg_file, squash_content).unwrap();

    ctx.assert_snapshot(
        ctx.gherrit().args(["hook", "commit-msg", squash_msg_file.to_str().unwrap()]),
        "commit_msg_squash",
    );

    let content_after = std::fs::read_to_string(&squash_msg_file).unwrap();
    assert_eq!(content_after, squash_content, "Commit-msg hook should ignore squash commits");

    // Scenario B: Detached HEAD
    ctx.run_git(&["checkout", "--detach"]);
    let detached_msg_file = ctx.repo_path.join("DETACHED_MSG");
    let detached_content = "feat: detached work";
    std::fs::write(&detached_msg_file, detached_content).unwrap();

    ctx.assert_snapshot(
        ctx.gherrit().args(["hook", "commit-msg", detached_msg_file.to_str().unwrap()]),
        "commit_msg_detached",
    );

    let content_after = std::fs::read_to_string(&detached_msg_file).unwrap();
    assert_eq!(content_after, detached_content, "Commit-msg hook should ignore detached HEAD");
}

#[test]
fn test_pre_push_ancestry_check() {
    // use predicates::prelude::*; // Unused

    let ctx = testutil::test_context_minimal!().install_hooks(true).build();

    // Setup: Create a normal history first (common init)
    ctx.commit("Initial Root");

    // Create an orphan branch
    ctx.run_git(&["checkout", "--orphan", "lonely-branch"]);
    ctx.commit("Lonely Commit");

    // Trigger pre-push hook; it should fail because it can't find the merge
    // base with 'main'
    // Trigger pre-push hook; it should fail because it can't find the merge
    // base with 'main'
    ctx.assert_snapshot(&mut ctx.hook("pre-push"), "pre_push_ancestry_failure");
}

#[test]
fn test_version_increment() {
    let ctx = testutil::test_context!().build();

    // Create feature branch
    ctx.checkout_new("feat-versioning");
    ctx.commit("Feature Commit");

    // Push 1 (v1)
    ctx.assert_snapshot(&mut ctx.hook("pre-push"), "version_increment_v1");

    // Verify v1 pushed
    let v1_count = ctx.count_pushed_containing("/v1");
    assert!(v1_count > 0, "Expected v1 tag to be pushed");

    // Amend commit (modifies SHA, keeps Change-ID)
    ctx.run_git(&["commit", "--amend", "--allow-empty", "--no-edit"]);

    // Push 2 (v2)
    ctx.assert_snapshot(&mut ctx.hook("pre-push"), "version_increment_v2");

    // Verify v2 pushed
    let v2_count = ctx.count_pushed_containing("/v2");
    assert!(v2_count > 0, "Expected v2 tag to be pushed");

    // Verify v1 NOT pushed AGAIN.
    let v1_count_final = ctx.count_pushed_containing("/v1");
    assert_eq!(v1_count_final, v1_count, "v1 tag should NOT be pushed again in the second push.");

    if !ctx.is_live {
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
    ctx.assert_snapshot(&mut ctx.hook("pre-push"), "optimistic_locking_v1");

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
    ctx.assert_snapshot(&mut ctx.hook("pre-push"), "optimistic_locking_v2_fail");
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
    ctx.assert_snapshot(&mut ctx.hook("pre-push"), "pr_body_generation_v1");

    // Verify
    ctx.maybe_inspect_mock_state(|state| {
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
            .find(|pr| pr.title.as_deref() == Some("Commit B"))
            .expect("PR for Commit B not found");

        let body = pr_b.body.as_ref().unwrap();

        // 1. Verify Metadata JSON
        // Should contain <!-- gherrit-meta: { ... } -->
        assert!(body.contains("<!-- gherrit-meta: {"), "Body missing gherrit-meta block");

        // Verify parent/child keys exist (basic check, since IDs are dynamic)
        assert!(body.contains(r#""parent": "G"#), "Body missing valid parent field");
        assert!(body.contains(r#""child": "G"#), "Body missing valid child field");

        // 2. Verify Table
        // For v1, the history table is NOT generated.
        assert!(!body.contains("| Version |"), "Table should NOT be present for v1");
    });

    // 4. Update to v2 to verify the Patch History Table appears
    ctx.run_git(&["checkout", "feature-stack"]); // Ensure we are on the branch

    // Amend "Commit B" (via tip Commit C) to create v2
    ctx.run_git(&["commit", "--amend", "--allow-empty", "--no-edit"]);

    // Sync again
    ctx.assert_snapshot(&mut ctx.hook("pre-push"), "pr_body_generation_v2");

    ctx.maybe_inspect_mock_state(|state| {
        // Find PR for Commit C (the tip)
        let pr_c = state
            .prs
            .iter()
            .find(|pr| pr.title.as_deref() == Some("Commit C"))
            .expect("PR for Commit C not found");

        let body = pr_c.body.as_ref().unwrap();

        // Assert table exists now
        assert!(body.contains("|Version|"), "Patch History Table should appear for v2");
        assert!(body.contains("v1|"), "Table should reference v1");
    });
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
    ctx.assert_snapshot(&mut ctx.hook("pre-push"), "large_stack_batching");

    ctx.maybe_inspect_mock_state(|state| {
        // Assert: 85 PRs created
        assert_eq!(state.prs.len(), 85, "Expected 85 PRs created");

        // Assert: Push was split into 2 batches (80 + 5)
        assert_eq!(
            state.push_count, 2,
            "Expected 2 push invocations for 85 commits (batch size 80)"
        );
    });

    // Assert: 85 refs pushed (actually more, since tags are also pushed)
    // Check refs/gherrit/<ID>/v1 count.
    let v1_refs = ctx.count_pushed_containing("/v1");
    assert_eq!(v1_refs, 85, "Expected 85 v1 specific refs pushed");
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
    ctx.assert_snapshot(&mut ctx.manage(), "rebase_detection_manage");

    // Verify config was applied to 'feature-rebase'
    ctx.assert_config("branch.feature-rebase.gherritManaged", Some(testutil::MANAGED_PRIVATE));
}

#[test]
fn test_public_stack_links() {
    let ctx = testutil::test_context_minimal!().install_hooks(true).build();

    ctx.commit("Init");
    // 1. Private Mode (Default)
    ctx.checkout_new("public-feature");
    ctx.commit("Public Commit");

    ctx.assert_snapshot(&mut ctx.hook("pre-push"), "public_stack_links_private");

    ctx.maybe_inspect_mock_state(|state| {
        let body = state.prs[0].body.as_ref().unwrap();
        assert!(
            !body.contains("This PR is on branch"),
            "Private stack should NOT link to local branch"
        );
    });

    // 2. Public Mode
    // Manually set pushRemote to origin (simulating a public stack)
    ctx.run_git(&["config", "branch.public-feature.pushRemote", "origin"]);

    // Force an update so the body regenerates (amend commit)
    ctx.run_git(&["commit", "--amend", "--allow-empty", "--no-edit"]);
    ctx.assert_snapshot(&mut ctx.hook("pre-push"), "public_stack_links_public");

    ctx.maybe_inspect_mock_state(|state| {
        let body = state.prs[0].body.as_ref().unwrap(); // Get the updated body
        assert!(body.contains("This PR is on branch"), "Public stack SHOULD link to local branch");
        assert!(body.contains("[public-feature]"), "Link should mention the branch name");
    });
}

#[test]
fn test_install_command_edge_cases() {
    let ctx = testutil::test_context_minimal!().build();

    let hooks_dir = ctx.repo_path.join(".git/hooks");
    std::fs::create_dir_all(&hooks_dir).unwrap();
    let pre_push = hooks_dir.join("pre-push");

    // Scenario A: Conflict
    std::fs::write(&pre_push, "foo").unwrap();

    ctx.assert_snapshot(ctx.gherrit().args(["install"]), "install_edge_cases_conflict");

    assert_eq!(std::fs::read_to_string(&pre_push).unwrap(), "foo");

    // Scenario B: Force Overwrite
    ctx.assert_snapshot(ctx.gherrit().args(["install", "--force"]), "install_edge_cases_force");

    let content = std::fs::read_to_string(&pre_push).unwrap();
    assert!(content.contains("# gherrit-installer: managed"));

    // Scenario C: Idempotency (Safe to run again)
    ctx.assert_snapshot(ctx.gherrit().args(["install"]), "install_edge_cases_idempotent");

    // Scenario D: Safe Update (Modify but keep sentinel)
    let modified = content + "\n# Some custom comment";
    std::fs::write(&pre_push, modified).unwrap();

    ctx.assert_snapshot(ctx.gherrit().args(["install"]), "install_edge_cases_upgrade");

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

    ctx.assert_snapshot(ctx.gherrit().args(["install"]), "install_security_default");
    assert!(default_hooks.join("pre-push").exists(), "Should create directory and install hook");

    // Scenario B: Custom core.hooksPath (Internal)
    // --------------------------------------------
    let custom_internal = ctx.repo_path.join(".githooks");
    ctx.run_git(&["config", "core.hooksPath", ".githooks"]);

    ctx.assert_snapshot(ctx.gherrit().args(["install"]), "install_security_custom_internal");
    assert!(custom_internal.join("pre-push").exists(), "Should respect core.hooksPath within repo");

    // Scenario C: Custom core.hooksPath (External/Global) - Security Block
    // --------------------------------------------------------------------
    let external_dir = tempfile::TempDir::new().unwrap();
    let ext_path = external_dir.path().to_str().unwrap();

    // We must use absolute path for git config to ensure gherrit sees it as external
    ctx.run_git(&["config", "core.hooksPath", ext_path]);

    ctx.assert_snapshot_with_redactions(
        ctx.gherrit().args(["install"]), // Should fail
        "install_security_custom_external_block",
        &[(ext_path, "[EXTERNAL_HOOKS_PATH]")],
    );

    assert!(
        !external_dir.path().join("pre-push").exists(),
        "Should NOT install to external path without flag"
    );

    // Scenario D: Custom core.hooksPath (External) - Allow Global
    // -----------------------------------------------------------
    ctx.assert_snapshot_with_redactions(
        ctx.gherrit().args(["install", "--allow-global"]),
        "install_security_custom_external_allow",
        &[(ext_path, "[EXTERNAL_HOOKS_PATH]")],
    );

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

    let test = |args: &[_], name| {
        ctx.assert_snapshot(ctx.gherrit().args(args), name);
    };

    test(&["manage"], "manage_detached_head");
    test(&["manage", "--public"], "manage_public_detached_head");
    test(&["manage", "--private"], "manage_private_detached_head");
    test(&["unmanage"], "unmanage_detached_head");
}

#[test]
fn test_unmanage_cleanup_logic() {
    let ctx = testutil::test_context_minimal!().build();
    ctx.commit("Init");
    ctx.checkout_new("feature-cleanup");

    // Manually configure the state to exact values that trigger the deep cleanup logic
    ctx.run_git(&["config", "branch.feature-cleanup.gherritManaged", testutil::MANAGED_PRIVATE]);
    ctx.run_git(&["config", "branch.feature-cleanup.pushRemote", "."]);
    ctx.run_git(&["config", "branch.feature-cleanup.remote", "."]);
    ctx.run_git(&["config", "branch.feature-cleanup.merge", "refs/heads/feature-cleanup"]);

    // Run unmanage
    ctx.assert_snapshot(&mut ctx.unmanage(), "unmanage_cleanup");

    // Verify cleanup: remote and merge keys should be removed
    ctx.git().args(["config", "branch.feature-cleanup.remote"]).assert().failure();
    ctx.git().args(["config", "branch.feature-cleanup.merge"]).assert().failure();
    // gherritManaged should be false
    ctx.assert_config("branch.feature-cleanup.gherritManaged", Some("false"));
}

#[test]
fn test_pre_push_failure() {
    let ctx = testutil::test_context_minimal!().install_hooks(true).build();
    ctx.commit("Init");

    ctx.checkout_new("feature-fail");
    ctx.commit("Work to push");

    // Configure an invalid remote to trigger `git push` failure
    ctx.run_git(&["remote", "add", "broken-remote", "/path/to/nowhere"]);
    ctx.run_git(&["config", "gherrit.remote", "broken-remote"]);

    ctx.assert_snapshot(&mut ctx.hook("pre-push"), "pre_push_failure_broken_remote");
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
    ctx.assert_snapshot(ctx.gherrit().args(["install"]), "install_read_only_fs");

    // Cleanup: Restore permissions so TempDir cleanup doesn't panic
    let mut perms = std::fs::metadata(&hooks_dir).unwrap().permissions();
    perms.set_mode(0o755);
    std::fs::set_permissions(&hooks_dir, perms).unwrap();
}

#[test]
fn test_manage_drift_detection() {
    let ctx = testutil::test_context_minimal!().build();
    ctx.checkout_new("drift-feature");

    // 1. Initialize managed private branch
    ctx.assert_snapshot(ctx.gherrit().args(["manage", "--private"]), "manage_drift_init");

    // 2. Manually sabotage
    ctx.run_git(&["config", "branch.drift-feature.pushRemote", "origin"]);

    // 3. Attempt Switch to Public (without force)
    // The command should exit with 0 but log a warning and NOT apply changes.
    ctx.assert_snapshot(ctx.gherrit().args(["manage", "--public"]), "manage_drift_attempt_switch");

    // Assert state matches OLD state (Private)
    ctx.assert_config("branch.drift-feature.gherritManaged", Some(testutil::MANAGED_PRIVATE));

    // 4. Force Switch
    ctx.assert_snapshot(
        ctx.gherrit().args(["manage", "--public", "--force"]),
        "manage_drift_force_switch",
    );

    // Assert Success
    ctx.assert_config("branch.drift-feature.gherritManaged", Some(testutil::MANAGED_PUBLIC));

    // Check pushRemote is now origin
    ctx.assert_config("branch.drift-feature.pushRemote", Some("origin"));
}

#[test]
fn test_manage_toggle_visibility() {
    let ctx = testutil::test_context_minimal!().build();
    ctx.checkout_new("visibility-feature");

    // 1. Private
    ctx.assert_snapshot(ctx.gherrit().args(["manage", "--private"]), "manage_toggle_init_private");
    ctx.assert_config("branch.visibility-feature.pushRemote", Some("."));

    // 2. Public
    ctx.assert_snapshot(ctx.gherrit().args(["manage", "--public"]), "manage_toggle_switch_public");
    ctx.assert_config("branch.visibility-feature.pushRemote", Some("origin"));

    // 3. Private again
    ctx.assert_snapshot(
        ctx.gherrit().args(["manage", "--private"]),
        "manage_toggle_switch_private",
    );
    ctx.assert_config("branch.visibility-feature.pushRemote", Some("."));
}

#[test]
fn test_manage_mutually_exclusive_flags() {
    let ctx = testutil::test_context_minimal!().build();
    ctx.checkout_new("conflict-feature");

    // Attempt to set both flags
    ctx.assert_snapshot(
        ctx.gherrit().args(["manage", "--public", "--private"]),
        "manage_mutually_exclusive",
    );
}

#[test]
fn test_manage_invalid_config() {
    let ctx = testutil::test_context_minimal!().build();
    ctx.checkout_new("invalid-config-feature");

    // Manually set invalid config
    ctx.run_git(&["config", "branch.invalid-config-feature.gherritManaged", "bad-value"]);

    // Attempt to manage; should fail
    ctx.assert_snapshot(&mut ctx.manage(), "manage_invalid_config");
}

#[test]
fn test_post_checkout_drift_detection() {
    let ctx = testutil::test_context!().build();

    // Condition A: Shared Branch Drift (Unmanaged vs Upstream Config)
    ctx.run_git(&["checkout", "main"]);
    ctx.run_git(&["update-ref", "refs/remotes/origin/drift-shared", "HEAD"]);

    // Switch to new tracking branch - this triggers post-checkout
    ctx.assert_snapshot(
        ctx.git().args(["checkout", "-b", "drift-shared", "--track", "origin/drift-shared"]),
        "post_checkout_drift_shared",
    );

    // Condition B: New Stack Drift (Private vs Pre-existing Config)
    ctx.run_git(&["checkout", "main"]);
    ctx.run_git(&["branch", "drift-stack"]);
    // Sabotage: Set remote=origin for what SHOULD be a private stack
    ctx.run_git(&["config", "branch.drift-stack.remote", "origin"]);

    // Switch to it
    ctx.assert_snapshot(ctx.git().args(["checkout", "drift-stack"]), "post_checkout_drift_stack");
}

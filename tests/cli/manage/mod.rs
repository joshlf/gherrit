#[test]
fn test_branch_management() {
    let ctx = testutil::test_context!().build();

    // Create a branch to manage
    ctx.checkout_new("feature-A");

    // Scenario A: Custom Push Remote Preservation
    ctx.run_git(&["config", "branch.feature-A.pushRemote", "origin"]);

    // Attempt manage - should fail (drift)
    testutil::assert_success_snapshot!(ctx, ctx.manage_cmd(), "branch_management_drift_warning"); // Logs warning, no change
    // Assert still unmanaged (missing key)
    ctx.assert_config("branch.feature-A.gherritManaged", None);

    testutil::assert_success_snapshot!(
        ctx,
        ctx.gherrit_cmd().args(["manage", "--force"]),
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
    testutil::assert_success_snapshot!(ctx, ctx.unmanage_cmd(), "branch_management_unmanage");

    // Assert unmanaged (key exists but is false)
    ctx.assert_config("branch.feature-A.gherritManaged", Some("false"));

    // Assert cleanup (keys should be unset)
    ctx.assert_config("branch.feature-A.remote", None);
    ctx.assert_config("branch.feature-A.merge", None);

    // Assert pushRemote unset
    ctx.assert_config("branch.feature-A.pushRemote", None);
}

#[test]
fn test_rebase_detection() {
    let ctx = testutil::test_context!().with_initial_commit().build();

    ctx.checkout_new("feature-rebase");
    ctx.commit("Feature Work");

    // Detach HEAD to simulate rebase state
    ctx.run_git(&["checkout", "--detach"]);

    // Create rebase-merge state manually
    let rebase_dir = ctx.repo_path.join(".git/rebase-merge");
    std::fs::create_dir_all(&rebase_dir).unwrap();
    std::fs::write(rebase_dir.join("head-name"), "refs/heads/feature-rebase").unwrap();

    // Run manage - should succeed by detecting 'feature-rebase'
    testutil::assert_success_snapshot!(ctx, ctx.manage_cmd(), "rebase_detection_manage");

    // Verify config was applied to 'feature-rebase'
    ctx.assert_config("branch.feature-rebase.gherritManaged", Some(testutil::MANAGED_PRIVATE));
}

#[test]
fn test_manage_detached_head() {
    let ctx = testutil::test_context!().with_initial_commit().build();

    // Enter detached HEAD state
    ctx.run_git(&["checkout", "--detach"]);

    let test = |args: &[_], name| {
        testutil::assert_failure_snapshot!(ctx, ctx.gherrit_cmd().args(args), name);
    };

    test(&["manage"], "manage_detached_head");
    test(&["manage", "--public"], "manage_public_detached_head");
    test(&["manage", "--private"], "manage_private_detached_head");
    test(&["unmanage"], "unmanage_detached_head");
}

#[test]
fn test_unmanage_force_cleanup() {
    let ctx = testutil::test_context!().with_initial_commit().build();
    ctx.checkout_new("feature-cleanup");

    // Configure a private branch whose push remote has drifted. Without
    // `--force`, GHerrit must preserve this state.
    ctx.run_git(&["config", "branch.feature-cleanup.gherritManaged", testutil::MANAGED_PRIVATE]);
    ctx.run_git(&["config", "branch.feature-cleanup.pushRemote", "drifted-remote"]);
    ctx.run_git(&["config", "branch.feature-cleanup.remote", "."]);
    ctx.run_git(&["config", "branch.feature-cleanup.merge", "refs/heads/feature-cleanup"]);

    let mut command = ctx.unmanage_cmd();
    command.arg("--force");
    testutil::assert_success_snapshot!(ctx, command, "unmanage_force_cleanup");

    ctx.assert_config("branch.feature-cleanup.pushRemote", None);
    ctx.assert_config("branch.feature-cleanup.remote", None);
    ctx.assert_config("branch.feature-cleanup.merge", None);
    ctx.assert_config("branch.feature-cleanup.gherritManaged", Some("false"));
}

#[test]
fn test_manage_drift_detection() {
    let ctx = testutil::test_context!().build();
    ctx.checkout_new("drift-feature");

    // 1. Initialize managed private branch
    testutil::assert_success_snapshot!(
        ctx,
        ctx.gherrit_cmd().args(["manage", "--private"]),
        "manage_drift_init"
    );

    // 2. Manually sabotage
    ctx.run_git(&["config", "branch.drift-feature.pushRemote", "origin"]);

    // 3. Attempt Switch to Public (without force)
    // The command should exit with 0 but log a warning and NOT apply changes.
    testutil::assert_success_snapshot!(
        ctx,
        ctx.gherrit_cmd().args(["manage", "--public"]),
        "manage_drift_attempt_switch"
    );

    // Assert state matches OLD state (Private)
    ctx.assert_config("branch.drift-feature.gherritManaged", Some(testutil::MANAGED_PRIVATE));

    // 4. Force Switch
    testutil::assert_success_snapshot!(
        ctx,
        ctx.gherrit_cmd().args(["manage", "--public", "--force"]),
        "manage_drift_force_switch",
    );

    // Assert Success
    ctx.assert_config("branch.drift-feature.gherritManaged", Some(testutil::MANAGED_PUBLIC));

    // Check pushRemote is now origin
    ctx.assert_config("branch.drift-feature.pushRemote", Some("origin"));
}

#[test]
fn test_manage_toggle_visibility() {
    let ctx = testutil::test_context!().build();
    ctx.checkout_new("visibility-feature");

    // 1. Private
    testutil::assert_success_snapshot!(
        ctx,
        ctx.gherrit_cmd().args(["manage", "--private"]),
        "manage_toggle_init_private"
    );
    ctx.assert_config("branch.visibility-feature.pushRemote", Some("."));

    // 2. Public
    testutil::assert_success_snapshot!(
        ctx,
        ctx.gherrit_cmd().args(["manage", "--public"]),
        "manage_toggle_switch_public"
    );
    ctx.assert_config("branch.visibility-feature.pushRemote", Some("origin"));

    // 3. Private again
    testutil::assert_success_snapshot!(
        ctx,
        ctx.gherrit_cmd().args(["manage", "--private"]),
        "manage_toggle_switch_private",
    );
    ctx.assert_config("branch.visibility-feature.pushRemote", Some("."));
}

#[test]
fn test_manage_mutually_exclusive_flags() {
    let ctx = testutil::test_context!().build();
    ctx.checkout_new("conflict-feature");

    // Attempt to set both flags
    testutil::assert_failure_snapshot!(
        ctx,
        ctx.gherrit_cmd().args(["manage", "--public", "--private"]),
        "manage_mutually_exclusive",
    );
}

#[test]
fn test_manage_invalid_config() {
    let ctx = testutil::test_context!().build();
    ctx.checkout_new("invalid-config-feature");

    // Manually set invalid config
    ctx.run_git(&["config", "branch.invalid-config-feature.gherritManaged", "bad-value"]);

    // Attempt to manage; should fail
    testutil::assert_failure_snapshot!(ctx, ctx.manage_cmd(), "manage_invalid_config");
}

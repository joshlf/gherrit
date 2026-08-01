#[test]
fn test_post_checkout_hook() {
    let ctx = testutil::test_context!()
        .with_remote()
        .with_installed_hooks()
        .with_initial_commit()
        .build();

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
fn test_post_checkout_drift_detection() {
    let ctx = testutil::test_context!()
        .with_remote()
        .with_installed_hooks()
        .with_initial_commit()
        .build();

    // Condition A: Shared Branch Drift (Unmanaged vs Upstream Config)
    ctx.run_git(&["checkout", "main"]);
    ctx.run_git(&["update-ref", "refs/remotes/origin/drift-shared", "HEAD"]);

    // Switch to new tracking branch - this triggers post-checkout
    testutil::assert_success_snapshot!(
        ctx,
        ctx.git_cmd().args(["checkout", "-b", "drift-shared", "--track", "origin/drift-shared"]),
        "post_checkout_drift_shared",
    );

    // Condition B: New Stack Drift (Private vs Pre-existing Config)
    ctx.run_git(&["checkout", "main"]);
    ctx.run_git(&["branch", "drift-stack"]);
    // Sabotage: Set remote=origin for what SHOULD be a private stack
    ctx.run_git(&["config", "branch.drift-stack.remote", "origin"]);

    // Switch to it
    testutil::assert_success_snapshot!(
        ctx,
        ctx.git_cmd().args(["checkout", "drift-stack"]),
        "post_checkout_drift_stack"
    );
}

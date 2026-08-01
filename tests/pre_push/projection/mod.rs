mod base;
mod pagination;

#[test]
fn test_pr_body_generation() {
    let ctx = testutil::test_context!()
        .with_remote()
        .with_installed_hooks()
        .with_initial_commit()
        .with_mock_github()
        .build();

    // Setup: Stack of 3 commits: A -> B -> C
    // Must be on a feature branch (not main) for gherrit to sync them
    ctx.checkout_new("feature-stack");
    ctx.commit("Commit A");
    ctx.commit("Commit B");
    ctx.commit("Commit C");

    // Ensure we capture the Change-IDs (Gherrit-IDs).
    // We can verify this implicitly by checking the PR bodies later.

    // Sync
    testutil::assert_success_snapshot!(ctx, ctx.hook_cmd("pre-push"), "pr_body_generation_v1");

    // Verify
    testutil::assert_pr_snapshot!(ctx, "pr_body_generation_v1_state");

    // 4. Update to v2 to verify the Patch History Table appears
    ctx.run_git(&["checkout", "feature-stack"]); // Ensure we are on the branch

    // Amend "Commit B" (via tip Commit C) to create v2
    ctx.amend();

    // Sync again
    testutil::assert_success_snapshot!(ctx, ctx.hook_cmd("pre-push"), "pr_body_generation_v2");

    testutil::assert_pr_snapshot!(ctx, "pr_body_generation_v2_state");
}

#[test]
fn test_public_stack_links() {
    let ctx = testutil::test_context!()
        .with_remote()
        .with_installed_hooks()
        .with_initial_commit()
        .with_mock_github()
        .build();

    // 1. Private Mode (Default)
    ctx.checkout_new("public-feature");
    ctx.commit("Public Commit");

    testutil::assert_success_snapshot!(ctx, ctx.hook_cmd("pre-push"), "public_stack_links_private");

    testutil::assert_pr_snapshot!(ctx, "public_stack_links_private_state");

    // 2. Public Mode
    // Manually set pushRemote to origin (simulating a public stack)
    ctx.run_git(&["config", "branch.public-feature.pushRemote", "origin"]);

    // Force an update so the body regenerates (amend commit)
    ctx.amend();
    testutil::assert_success_snapshot!(ctx, ctx.hook_cmd("pre-push"), "public_stack_links_public");

    testutil::assert_pr_snapshot!(ctx, "public_stack_links_public_state");
}

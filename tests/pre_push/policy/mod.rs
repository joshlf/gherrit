mod pr_state;
mod unmanaged;

#[test]
fn test_pre_push_ancestry_check() {
    let ctx = testutil::test_context!().with_installed_hooks().build();

    // Setup: Create a normal history first (common init)
    ctx.commit("Initial Root");

    // Create an orphan branch
    ctx.run_git(&["checkout", "--orphan", "lonely-branch"]);
    ctx.commit("Lonely Commit");

    // Trigger pre-push hook; it should fail because it can't find the merge
    // base with 'main'
    testutil::assert_failure_snapshot!(ctx, ctx.hook_cmd("pre-push"), "pre_push_ancestry_failure");
}

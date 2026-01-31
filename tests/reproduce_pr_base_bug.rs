#[test]
fn test_reproduce_pr_base_branch_bug() {
    // Regression test for "Base ref must be a branch" error. Checks that PRs
    // are always created with the repository's default branch (e.g., "main") as
    // the base, rather than using the local feature branch name.

    let ctx = testutil::test_context!().build();

    ctx.checkout_new("feature-branch");
    ctx.commit("Feature Work");

    ctx.gherrit().args(["hook", "pre-push"]).assert().success();

    testutil::assert_pr_snapshot!(ctx, "reproduce_pr_base_bug_state");
}

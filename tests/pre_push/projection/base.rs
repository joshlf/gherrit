#[test]
fn test_reproduce_pr_base_branch_bug() {
    // Regression test for "Base ref must be a branch" error. Checks that PRs
    // are always created with the repository's default branch (e.g., "main") as
    // the base, rather than using the local feature branch name.

    let ctx =
        testutil::test_context!().with_remote().with_initial_commit().with_mock_github().build();

    ctx.checkout_managed_private("feature-branch");
    ctx.commit_with_gherrit_id("Feature Work");

    ctx.gherrit_cmd().args(["hook", "pre-push"]).assert().success();

    testutil::assert_pr_snapshot!(ctx, "reproduce_pr_base_bug_state");
}

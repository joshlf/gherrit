#[test]
fn test_reproduce_pr_base_branch_bug() {
    let ctx = testutil::test_context!().build();
    ctx.checkout_new("feature-branch");

    // Initial commit (C1)
    ctx.commit("Feature Work");

    // We expect the PR creation to target "main" (the default branch), NOT "feature-branch".
    // Inspect the debug log or mock server state to verify this.
    // For now, let's rely on the mock server failing if we can strictly mock inputs,
    // or we can read the `mock_state` to see what requests were processed.
    // BUT, the current mock server might accept "feature-branch" as valid input if not configured strictly,
    // so we should look for the *absence* of a PR targeting "main", or explicit failure.

    // Actually, let's use the error message assertion if we can trigger the "Base ref must be a branch" error?
    // In our testutil context, we might not trigger the exact same GitHub error unless we mock that error.
    // However, we CAN assert that `createPullRequest` was called with `baseRefName: main`.

    // Run hook
    ctx.gherrit().args(["hook", "pre-push"]).assert().success();

    if !ctx.is_live {
        // Read the mock state to verify the keys of the created PRs
        let state = ctx.read_mock_state();
        let pr = state.prs.last().expect("PR should have been created");

        // THIS IS THE ASSERTION THAT WILL FAIL
        assert_eq!(
            pr.base.ref_field, "main",
            "PR should be based on main, not feature-branch"
        );
    }
}

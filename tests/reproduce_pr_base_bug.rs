#[test]
fn test_reproduce_pr_base_branch_bug() {
    // Regression test for "Base ref must be a branch" error. Checks that PRs
    // are always created with the repository's default branch (e.g., "main") as
    // the base, rather than using the local feature branch name.

    let ctx = testutil::test_context!().build();

    ctx.checkout_new("feature-branch");
    ctx.commit("Feature Work");

    ctx.gherrit().args(["hook", "pre-push"]).assert().success();

    ctx.maybe_inspect_mock_state(|state| {
        let pr = &state.prs[0];
        assert_eq!(
            pr.base.ref_field, "main",
            "PR should target main, not the parent feature branch"
        );
    });
}

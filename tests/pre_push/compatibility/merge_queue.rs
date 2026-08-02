#[test]
fn test_reproduce_merge_queue_failure() {
    // Regression test for "Merge Queue" state where base branch updates are
    // rejected (#271). The test sets up a scenario where the PR is "locked" in
    // the mock server, simulating a merge queue environment. 'gherrit' should
    // avoid updating the base branch if it hasn't changed, preventing failure.

    let ctx = testutil::test_context!()
        .with_remote()
        .with_installed_hooks()
        .with_initial_commit()
        .with_mock_github()
        .build();

    // 1. Create a PR
    ctx.checkout_new("feature-branch");
    ctx.commit("Initial Feature Work");
    ctx.gherrit_cmd().args(["hook", "pre-push"]).assert().success();

    // Get the PR ID
    let pr_number = ctx.github().pull_requests()[0].number;

    // 2. Add the PR to the merge queue
    ctx.github().add_to_merge_queue(pr_number);

    // 3. Amend the commit (update title/body) but NOT the base
    ctx.commit("Initial Feature Work (Amended)");

    // 4. Push again
    ctx.gherrit_cmd().args(["hook", "pre-push"]).assert().success();
}

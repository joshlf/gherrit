#[test]
fn test_reproduce_merge_queue_failure() {
    // Regression test for "Merge Queue" state where base branch updates are
    // rejected (#271). The test sets up a scenario where the PR is "locked" in
    // the mock server, simulating a merge queue environment. 'gherrit' should
    // avoid updating the base branch if it hasn't changed, preventing failure.

    let ctx = testutil::test_context!().build();

    // 1. Create a PR
    ctx.checkout_new("feature-branch");
    ctx.commit("Initial Feature Work");
    ctx.gherrit().args(["hook", "pre-push"]).assert().success();

    // Get the PR ID
    let mut pr_id = 0;
    ctx.maybe_inspect_mock_state(|state| {
        pr_id = state.prs[0].id;
    });

    // 2. Add the PR to the merge queue
    ctx.maybe_mutate_mock_state(move |state| {
        state.merge_queue.insert(pr_id);
    });

    // 3. Amend the commit (update title/body) but NOT the base
    ctx.commit("Initial Feature Work (Amended)");

    // 4. Push again
    ctx.gherrit().args(["hook", "pre-push"]).assert().success();
}

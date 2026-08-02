use predicates::prelude::*;

#[test]
fn test_regression_batch_update_silent_failure() {
    let ctx =
        testutil::test_context!().with_remote().with_initial_commit().with_mock_github().build();
    ctx.checkout_managed_private("feature-update-fail");

    // 1. Initial Push (creates PR)
    ctx.commit_with_gherrit_id("Initial Work");
    ctx.gherrit_cmd().args(["hook", "pre-push"]).assert().success();

    // 2. Modify the commit and make the next update response explicitly null.
    ctx.amend_with_message("Initial Work (Updated)");
    ctx.inject_failure(testutil::FailureKind::UpdatePrNull);

    // 3. Push again - Expect Failure due to null response
    let assert = ctx.gherrit_cmd().args(["hook", "pre-push"]).assert().failure();

    assert.stderr(predicate::str::contains("The batched GraphQL mutation failed to update PR"));
    ctx.assert_failure_consumed();
}

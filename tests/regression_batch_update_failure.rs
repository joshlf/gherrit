use predicates::prelude::*;

#[test]
fn test_regression_batch_update_silent_failure() {
    let ctx = testutil::test_context!().build();
    ctx.checkout_new("feature-update-fail");

    // 1. Initial Push (creates PR)
    ctx.commit("Initial Work");
    ctx.gherrit().args(["hook", "pre-push"]).assert().success();

    // 2. Modify commit to trigger failure in mock server
    //
    // Simulate failure by appending special token "TRIGGER_GRAPHQL_NULL" to the
    // body.
    ctx.run_git(&[
        "commit",
        "--amend",
        "--allow-empty",
        "-m",
        "Initial Work\n\nTRIGGER_GRAPHQL_NULL",
    ]);

    // 3. Push again - Expect Failure due to null response
    let assert = ctx.gherrit().args(["hook", "pre-push"]).assert().failure();

    assert.stderr(predicate::str::contains("The batched GraphQL mutation failed to update PR"));
}

use predicates::prelude::*;

#[test]
fn test_regression_batch_update_silent_failure() {
    let ctx = testutil::test_context!()
        .with_remote()
        .with_installed_hooks()
        .with_initial_commit()
        .with_mock_github()
        .build();
    ctx.checkout_new("feature-update-fail");

    // 1. Initial push creates two PRs.
    ctx.commit("First");
    ctx.commit("Second");
    let second_id = ctx.gherrit_id("HEAD").unwrap();
    ctx.gherrit_cmd().args(["hook", "pre-push"]).assert().success();

    // 2. Rewrite both commits, then make only the second update response null.
    ctx.run_git(&["checkout", "HEAD^"]);
    ctx.amend_with_message("First (Updated)");
    ctx.run_git(&[
        "commit",
        "--allow-empty",
        "--no-verify",
        "-m",
        &format!("Second (Updated)\n\ngherrit-pr-id: {second_id}"),
    ]);
    ctx.run_git(&["checkout", "-B", "feature-update-fail"]);
    ctx.inject_failure(testutil::FailureKind::UpdatePrNull { number: 2 });

    // 3. Push again - Expect Failure due to null response
    let assert = ctx.gherrit_cmd().args(["hook", "pre-push"]).assert().failure();

    assert.stderr(predicate::str::contains(
        "The batched GraphQL mutation failed to update PR with node ID 'PR_2'",
    ));
    ctx.assert_failure_consumed();
}

#[test]
fn test_pagination_bug() {
    let ctx =
        testutil::test_context!().with_remote().with_initial_commit().with_mock_github().build();

    // 1. Checkout a managed feature branch.
    ctx.checkout_managed_private("feature");

    // 2. Create a commit with a known Change-Id
    let change_id = "I0000000000000000000000000000000000000105";
    ctx.commit_with_explicit_gherrit_id("Commit 105", change_id);

    // 3. Generate 110 PRs in the mock server state
    let github = ctx.github();
    for i in 1..=110 {
        let is_target = i == 105;
        let head = if is_target { change_id.to_string() } else { format!("other-change-{i}") };

        github.seed_pull_request(testutil::PullRequestSeed {
            number: i,
            title: format!("PR {i}"),
            body: "body".to_string(),
            head,
            base: "main".to_string(),
        });
    }

    // 4. Run gherrit hook pre-push
    let assert =
        ctx.gherrit_cmd().args(["hook", "pre-push"]).env("RUST_LOG", "debug").assert().success();

    let output = assert.get_output();
    let stderr = String::from_utf8_lossy(&output.stderr);

    assert!(
        stderr.contains("Found existing PR #105"),
        "Regression: Failed to find PR #105 (likely pagination bug). Logs:\n{stderr}"
    );
}

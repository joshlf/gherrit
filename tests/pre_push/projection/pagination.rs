#[test]
fn test_pagination_bug() {
    let ctx = testutil::test_context!()
        .with_remote()
        .with_installed_hooks()
        .with_initial_commit()
        .with_mock_github()
        .build();

    // 1. Checkout feature branch and manage it
    ctx.checkout_new("feature");
    ctx.gherrit_cmd().args(["manage", "--force"]).assert().success();

    // 2. Create a commit with a known Change-Id
    let change_id = "I0000000000000000000000000000000000000105";
    let msg = format!("Commit 105\n\ngherrit-pr-id: {}", change_id);
    ctx.commit(&msg);

    // 3. Generate 110 PRs in the mock server state
    ctx.mutate_mock_state(|state| {
        for i in 1..=110 {
            let is_target = i == 105;
            let head_ref =
                if is_target { change_id.to_string() } else { format!("other-change-{}", i) };

            let pr = testutil::mock_server::PrEntry::mock(testutil::mock_server::MockPrArgs {
                id: i as u64,
                title: format!("PR {i}"),
                body: "body".to_string(),
                head: head_ref,
                base: "main".to_string(),
                repo_owner: "owner",
                repo_name: "repo",
            });
            state.add_pr(pr);
        }
    });

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

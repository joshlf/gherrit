use predicates::prelude::*;

#[test]
fn test_post_push_checks_closed_pr() {
    let ctx = testutil::test_context!().build();
    ctx.checkout_new("feature-closed");

    // 1. Initial Push (Creates PR)
    ctx.commit("Initial Work");
    ctx.gherrit().args(["hook", "pre-push"]).assert().success();

    // 2. Simulate PR Closed on GitHub
    if !ctx.is_live {
        let mut state = ctx.read_mock_state();
        let pr = state.prs.last_mut().expect("PR not found");

        pr.state = "CLOSED".to_string();
        testutil::mock_server::write_state(&ctx.repo_path.join("mock_state.json"), &state);
    }

    // 3. Amend and Push (Should Fail)
    ctx.run_git(&["commit", "--amend", "--allow-empty", "--no-edit"]);
    let assert = ctx.gherrit().args(["hook", "pre-push"]).assert().failure();

    assert.stderr(predicate::str::contains("Cannot push to closed PR"));

    // 4. Verify no new push happened
    if !ctx.is_live {
        let state = ctx.read_mock_state();
        assert_eq!(state.push_count, 1, "Should not have pushed to closed PR");
    }
}

#[test]
fn test_post_push_checks_merged_pr() {
    let ctx = testutil::test_context!().build();
    ctx.checkout_new("feature-merged");

    // 1. Initial Push
    ctx.commit("Initial Work");
    ctx.gherrit().args(["hook", "pre-push"]).assert().success();

    // 2. Simulate PR Merged
    if !ctx.is_live {
        let mut state = ctx.read_mock_state();
        let pr = state.prs.last_mut().expect("PR not found");
        pr.state = "MERGED".to_string();
        testutil::mock_server::write_state(&ctx.repo_path.join("mock_state.json"), &state);
    }

    // 3. Amend and Push (Should Fail)
    ctx.run_git(&["commit", "--amend", "--allow-empty", "--no-edit"]);
    let assert = ctx.gherrit().args(["hook", "pre-push"]).assert().failure();

    assert.stderr(predicate::str::contains("Cannot push to merged PR"));
}

#[test]
fn test_push_to_open_pr_succeeds() {
    let ctx = testutil::test_context!().build();
    ctx.checkout_new("feature-open");
    ctx.commit("Work");

    // 1. First Push
    ctx.gherrit().args(["hook", "pre-push"]).assert().success();

    // 2. Amend
    ctx.run_git(&["commit", "--amend", "--allow-empty", "--no-edit"]);

    // 3. Second Push (Should Succeed)
    ctx.gherrit().args(["hook", "pre-push"]).assert().success();
}

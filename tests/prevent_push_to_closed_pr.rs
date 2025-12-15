use predicates::prelude::*;

fn verify_push_to_non_open_fail(state_arg: &str, expected_msg_part: &str) {
    let ctx = testutil::test_context!().build();
    ctx.checkout_new(&format!("feature-{}", state_arg.to_lowercase()));

    // 1. Initial Push (Creates PR)
    ctx.commit("Initial Work");
    ctx.gherrit().args(["hook", "pre-push"]).assert().success();

    // 2. Simulate PR State Change on GitHub
    if !ctx.is_live {
        let mut state = ctx.read_mock_state();
        let pr = state.prs.last_mut().expect("PR not found");

        pr.state = state_arg.to_string();
        testutil::mock_server::write_state(&ctx.repo_path.join("mock_state.json"), &state);
    }

    // 3. Amend and Push (Should Fail)
    ctx.run_git(&["commit", "--amend", "--allow-empty", "--no-edit"]);
    let assert = ctx.gherrit().args(["hook", "pre-push"]).assert().failure();

    assert.stderr(predicate::str::contains(expected_msg_part));

    // 4. Verify no new push happened
    if !ctx.is_live {
        let state = ctx.read_mock_state();
        assert_eq!(
            state.push_count, 1,
            "Should not have pushed to {} PR",
            state_arg
        );
    }
}

#[test]
fn test_post_push_checks_closed_pr() {
    verify_push_to_non_open_fail("CLOSED", "Cannot push to closed PR");
}

#[test]
fn test_post_push_checks_merged_pr() {
    verify_push_to_non_open_fail("MERGED", "Cannot push to merged PR");
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

use predicates::prelude::*;

fn verify_push_to_non_open_fail(state_arg: &str, expected_msg_part: &str) {
    let ctx = testutil::test_context!().build();
    ctx.checkout_new(&format!("feature-{}", state_arg.to_lowercase()));

    // 1. Initial Push (Creates PR)
    ctx.commit("Initial Work");
    ctx.gherrit().args(["hook", "pre-push"]).assert().success();

    // 2. Simulate PR State Change on GitHub
    ctx.maybe_mutate_mock_state(|state| {
        let pr = state.prs.last_mut().unwrap();
        pr.state = state_arg.to_string();
    });

    // 3. Amend and Push (Should Fail)
    ctx.run_git(&["commit", "--amend", "--allow-empty", "--no-edit"]);
    ctx.hook("pre-push").assert().failure().stderr(predicate::str::contains(expected_msg_part));

    // 4. Verify no new push happened
    let name = format!("prevent_push_to_{}_pr_state", state_arg.to_lowercase());
    testutil::assert_pr_snapshot!(ctx, name.as_str());
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

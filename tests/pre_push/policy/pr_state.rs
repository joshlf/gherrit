use predicates::prelude::*;

fn verify_push_to_non_open_fail(
    state: testutil::PullRequestState,
    state_name: &str,
    expected_msg_part: &str,
) {
    let ctx = testutil::test_context!()
        .with_remote()
        .with_installed_hooks()
        .with_initial_commit()
        .with_mock_github()
        .build();
    ctx.checkout_new(&format!("feature-{}", state_name.to_lowercase()));

    // 1. Initial Push (Creates PR)
    ctx.commit("Initial Work");
    ctx.gherrit_cmd().args(["hook", "pre-push"]).assert().success();
    let refs_before_rejected_push = ctx.remote_refs("refs");

    // 2. Simulate PR State Change on GitHub
    let pr = ctx.github().pull_requests().pop().expect("created pull request");
    ctx.github().set_pull_request_state(pr.number, state);

    // 3. Amend and Push (Should Fail)
    ctx.amend();
    ctx.hook_cmd("pre-push").assert().failure().stderr(predicate::str::contains(expected_msg_part));

    // 4. Verify no new push happened
    let name = format!("prevent_push_to_{}_pr_state", state_name.to_lowercase());
    testutil::assert_pr_snapshot!(ctx, name.as_str());

    assert_eq!(ctx.remote_refs("refs"), refs_before_rejected_push);
}

#[test]
fn test_post_push_checks_closed_pr() {
    verify_push_to_non_open_fail(
        testutil::PullRequestState::Closed,
        "CLOSED",
        "Cannot push to closed PR",
    );
}

#[test]
fn test_post_push_checks_merged_pr() {
    verify_push_to_non_open_fail(
        testutil::PullRequestState::Merged,
        "MERGED",
        "Cannot push to merged PR",
    );
}

#[test]
fn test_push_to_open_pr_succeeds() {
    let ctx = testutil::test_context!()
        .with_remote()
        .with_installed_hooks()
        .with_initial_commit()
        .with_mock_github()
        .build();
    ctx.checkout_new("feature-open");
    ctx.commit("Work");

    // 1. First Push
    ctx.gherrit_cmd().args(["hook", "pre-push"]).assert().success();

    // 2. Amend
    ctx.amend();

    // 3. Second Push (Should Succeed)
    ctx.gherrit_cmd().args(["hook", "pre-push"]).assert().success();
}

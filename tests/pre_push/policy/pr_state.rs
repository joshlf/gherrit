#[test]
fn non_open_pr_blocks_all_mutations() {
    let ctx = testutil::test_context!()
        .with_remote()
        .with_initial_commit()
        .with_mock_github()
        .with_git_interceptor()
        .build();
    ctx.checkout_managed_private("feature-non-open");

    ctx.commit_with_gherrit_id("Initial Work");
    ctx.gherrit_cmd().args(["hook", "pre-push"]).assert().success();

    let pr = ctx.github().pull_requests().pop().expect("created pull request");
    ctx.github().set_pull_request_state(pr.number, testutil::PullRequestState::Merged);

    let refs_before = ctx.remote_refs("refs");
    let pushes_before = ctx.recorded_pushes();
    let requests_before = ctx.github().requests();
    let pull_requests_before = ctx.github().pull_requests();

    ctx.amend();
    testutil::assert_failure_snapshot!(
        ctx,
        ctx.hook_cmd("pre-push"),
        "non_open_pr_blocks_mutations",
    );

    assert_eq!(ctx.remote_refs("refs"), refs_before);
    assert_eq!(ctx.recorded_pushes(), pushes_before);
    assert_eq!(ctx.github().pull_requests(), pull_requests_before);

    let requests_after = ctx.github().requests();
    assert_eq!(
        &requests_after[..requests_before.len()],
        requests_before,
        "rejected push changed the existing request trace"
    );
    assert_eq!(
        &requests_after[requests_before.len()..],
        &[vec![testutil::GraphQlOperation::Query]],
        "rejected push must only observe GitHub state"
    );
}

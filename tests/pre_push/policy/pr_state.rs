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

#[test]
fn terminal_history_is_fully_paginated_before_rejection() {
    let ctx = testutil::test_context!()
        .with_remote()
        .with_initial_commit()
        .with_mock_github()
        .with_git_interceptor()
        .build();
    ctx.checkout_managed_private("paginated-terminal-history");
    let id = ctx.commit_with_gherrit_id("Observe every pull request page");

    for (number, state) in
        [(11, testutil::PullRequestState::Closed), (12, testutil::PullRequestState::Merged)]
    {
        ctx.github().seed_pull_request(testutil::PullRequestSeed {
            number,
            title: format!("Historical {number}"),
            body: String::new(),
            head: id.clone(),
            base: "main".to_owned(),
        });
        ctx.github().set_pull_request_state(number, state);
    }

    ctx.hook_cmd("pre-push")
        .assert()
        .failure()
        .stderr(predicates::str::contains("closed and merged pull request history"));

    assert_eq!(
        ctx.github().requests(),
        vec![vec![testutil::GraphQlOperation::Query]; 2],
        "a one-row connection must be exhausted before lifecycle policy runs"
    );
    assert!(ctx.recorded_pushes().is_empty());
}

#[test]
fn landing_automation_on_an_owned_base_blocks_every_new_effect() {
    for (auto_merge, in_merge_queue) in [(true, false), (false, true)] {
        let ctx = testutil::test_context!()
            .with_remote()
            .with_initial_commit()
            .with_mock_github()
            .with_git_interceptor()
            .build();
        ctx.checkout_managed_private("landing-automation");
        ctx.commit_with_gherrit_id("Do not rewrite an automated pull request");
        ctx.inject_failure(testutil::FailureKind::GitPushOutput {
            preceding_pushes: 1,
            stdout: "",
        });
        ctx.hook_cmd("pre-push").assert().failure();
        ctx.assert_failure_consumed();
        let pull_request = ctx.github().pull_requests().pop().unwrap();
        assert!(pull_request.base.starts_with("gherrit-bases/"));
        let pushes_before = ctx.recorded_pushes();
        ctx.github().set_pull_request_landing_automation(
            pull_request.number,
            auto_merge,
            in_merge_queue,
        );
        let refs_before = ctx.remote_refs("refs");
        let pull_requests_before = ctx.github().pull_requests();
        let requests_before = ctx.github().requests().len();

        ctx.hook_cmd("pre-push")
            .assert()
            .failure()
            .stderr(predicates::str::contains("cannot use landing automation"));

        assert_eq!(ctx.recorded_pushes(), pushes_before);
        assert_eq!(ctx.remote_refs("refs"), refs_before);
        assert_eq!(ctx.github().pull_requests(), pull_requests_before);
        assert_eq!(
            &ctx.github().requests()[requests_before..],
            &[vec![testutil::GraphQlOperation::Query]],
            "landing-automation rejection must perform observation only"
        );
    }
}

fn open_query(head: &str) -> testutil::GraphQlOperation {
    testutil::GraphQlOperation::open_query([head], true)
}

fn terminal_query(head: &str) -> testutil::GraphQlOperation {
    testutil::GraphQlOperation::terminal_query([head])
}

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
        &[vec![open_query(&pr.head)], vec![terminal_query(&pr.head)],],
        "rejected push must only observe GitHub state"
    );
}

#[test]
fn the_first_same_repository_terminal_row_rejects_observation() {
    let ctx = testutil::test_context!()
        .with_remote()
        .with_initial_commit()
        .with_mock_github()
        .with_git_interceptor()
        .build();
    ctx.checkout_managed_private("paginated-terminal-history");
    let id = ctx.commit_with_gherrit_id("Observe every pull request page");

    for (number, state) in
        [(9, testutil::PullRequestState::Closed), (10, testutil::PullRequestState::Merged)]
    {
        ctx.github().seed_cross_repository_pull_request(
            testutil::PullRequestSeed {
                number,
                title: format!("Foreign historical {number}"),
                body: String::new(),
                head: id.clone(),
                base: "main".to_owned(),
            },
            &"1".repeat(40),
            &"2".repeat(40),
        );
        ctx.github().set_pull_request_state(number, state);
    }
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
        .stderr(predicates::str::contains("PR #11 is closed"));

    assert_eq!(
        ctx.github().requests(),
        vec![
            vec![open_query(&id)],
            vec![terminal_query(&id)],
            vec![testutil::GraphQlOperation::TerminalQuery {
                connections: vec![testutil::PullRequestConnectionQuery::after(
                    id.clone(),
                    format!("cursor:terminal:{id}:1"),
                )],
            }],
            vec![testutil::GraphQlOperation::TerminalQuery {
                connections: vec![testutil::PullRequestConnectionQuery::after(
                    id.clone(),
                    format!("cursor:terminal:{id}:2"),
                )],
            }],
        ],
        "terminal forks must be paginated past before the first local row rejects"
    );
    assert!(ctx.recorded_pushes().is_empty());
}

#[test]
fn two_same_repository_open_rows_reject_before_any_push_or_mutation() {
    let ctx = testutil::test_context!()
        .with_remote()
        .with_initial_commit()
        .with_mock_github()
        .with_git_interceptor()
        .build();
    ctx.checkout_managed_private("duplicate-open-observation");
    let id = ctx.commit_with_gherrit_id("Observe every OPEN page before effects");
    ctx.hook_cmd("pre-push").assert().success();
    let pushes_before = ctx.recorded_pushes();
    let requests_before = ctx.github().requests().len();
    let pull_requests_before = ctx.github().pull_requests();
    ctx.github().seed_pull_request(testutil::PullRequestSeed {
        number: 10,
        title: "Late duplicate".to_owned(),
        body: String::new(),
        head: id.clone(),
        base: "main".to_owned(),
    });
    ctx.amend_with_message("Observe the second OPEN row");

    ctx.hook_cmd("pre-push")
        .assert()
        .failure()
        .stderr(predicates::str::contains("more than one OPEN pull request"));

    assert_eq!(ctx.recorded_pushes(), pushes_before);
    assert_eq!(ctx.github().pull_requests().len(), pull_requests_before.len() + 1);
    assert_eq!(
        &ctx.github().requests()[requests_before..],
        vec![
            vec![open_query(&id)],
            vec![testutil::GraphQlOperation::OpenQuery {
                connections: vec![testutil::PullRequestConnectionQuery::after(
                    id.clone(),
                    format!("cursor:open:{id}:1"),
                )],
                include_repository_facts: false,
            }],
        ],
        "a second local OPEN row must be found before the effect barrier"
    );
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
            &[vec![open_query(&pull_request.head)]],
            "landing-automation rejection must perform observation only"
        );
    }
}

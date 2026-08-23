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
        &[vec![testutil::GraphQlOperation::Query], vec![testutil::GraphQlOperation::Query],],
        "rejected push must complete the open scan and terminal history"
    );
}

#[test]
fn multiple_terminal_prs_for_one_change_fail_before_mutation() {
    let ctx = testutil::test_context!()
        .with_remote()
        .with_initial_commit()
        .with_mock_github()
        .with_git_interceptor()
        .build();
    ctx.checkout_managed_private("feature-ambiguous-history");
    ctx.commit_with_gherrit_id("Work");
    let id = ctx.gherrit_id("HEAD").unwrap();
    let head_oid = ctx.head_oid();
    let base_oid = ctx.remote_ref_oid("refs/heads/main").unwrap();

    for (number, state) in
        [(7, testutil::PullRequestState::Closed), (9, testutil::PullRequestState::Merged)]
    {
        ctx.github().seed_pull_request(testutil::PullRequestSeed {
            number,
            title: format!("Historical {number}"),
            body: String::new(),
            head: id.clone(),
            head_oid: head_oid.clone(),
            base: "main".to_string(),
            base_oid: base_oid.clone(),
            auto_merge: false,
            in_merge_queue: false,
        });
        ctx.github().set_pull_request_state(number, state);
    }

    ctx.hook_cmd("pre-push").assert().failure().stderr(predicates::str::contains(
        "Found multiple historical pull requests for GHerrit ID",
    ));

    assert_eq!(
        ctx.github().requests(),
        [vec![testutil::GraphQlOperation::Query], vec![testutil::GraphQlOperation::Query]]
    );
    assert!(ctx.recorded_pushes().is_empty());
}

#[test]
fn owned_base_landing_automation_is_rejected_before_every_write() {
    for automation in ["auto-merge", "merge-queue"] {
        let ctx = testutil::test_context!()
            .with_remote()
            .with_initial_commit()
            .with_mock_github()
            .with_git_interceptor()
            .build();
        ctx.checkout_managed_private(&format!("reject-{automation}"));
        ctx.commit_with_explicit_gherrit_id("Root", "Groot");
        ctx.commit_with_explicit_gherrit_id("Owned child", "Gchild");
        ctx.hook_cmd("pre-push").assert().success();

        let child = ctx
            .github()
            .pull_requests()
            .into_iter()
            .find(|pull_request| pull_request.head == "Gchild")
            .expect("published child pull request");
        assert_eq!(child.base, "gherrit-bases/Gchild");
        match automation {
            "auto-merge" => ctx.github().set_pull_request_auto_merge(child.number, true),
            "merge-queue" => {
                ctx.github().set_pull_request_in_merge_queue(child.number, true);
            }
            _ => unreachable!(),
        }
        ctx.amend();
        let refs_before = ctx.remote_refs("refs");
        let pushes_before = ctx.recorded_pushes();
        let pull_requests_before = ctx.github().pull_requests();

        ctx.hook_cmd("pre-push")
            .assert()
            .failure()
            .stderr(predicates::str::contains("landing automation with an owned base"));

        assert_eq!(ctx.remote_refs("refs"), refs_before, "automation={automation}");
        assert_eq!(ctx.recorded_pushes(), pushes_before, "automation={automation}");
        assert_eq!(ctx.github().pull_requests(), pull_requests_before, "automation={automation}");
    }
}

#[test]
fn automated_root_remains_allowed_while_both_bases_are_default() {
    let ctx = testutil::test_context!()
        .with_remote()
        .with_initial_commit()
        .with_mock_github()
        .with_git_interceptor()
        .build();
    ctx.checkout_managed_private("automated-root");
    ctx.commit_with_explicit_gherrit_id("Root", "Groot");
    ctx.hook_cmd("pre-push").assert().success();
    let root = ctx.github().pull_requests().pop().expect("published root pull request");
    assert_eq!(root.base, "main");
    ctx.github().set_pull_request_auto_merge(root.number, true);

    ctx.amend();
    ctx.hook_cmd("pre-push").assert().success();

    let root = ctx.github().pull_requests().pop().expect("updated root pull request");
    assert_eq!(root.base, "main");
    assert!(root.auto_merge);
    assert!(!root.in_merge_queue);

    ctx.run_git(&["checkout", "-b", "inserted-root", "main"]);
    ctx.commit_with_explicit_gherrit_id("Inserted root", "Ginserted");
    let inserted_head = ctx.head_oid();
    ctx.run_git(&["checkout", "automated-root"]);
    ctx.run_git(&["rebase", "--keep-empty", "--onto", &inserted_head, "main"]);
    let refs_before = ctx.remote_refs("refs");
    let pushes_before = ctx.recorded_pushes();
    let pull_requests_before = ctx.github().pull_requests();

    ctx.hook_cmd("pre-push")
        .assert()
        .failure()
        .stderr(predicates::str::contains("landing automation with an owned base"));

    assert_eq!(ctx.remote_refs("refs"), refs_before);
    assert_eq!(ctx.recorded_pushes(), pushes_before);
    assert_eq!(ctx.github().pull_requests(), pull_requests_before);
}

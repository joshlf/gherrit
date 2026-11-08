#[test]
fn test_full_stack_lifecycle_mocked() {
    let ctx = testutil::test_context!()
        .with_remote()
        .with_initial_commit()
        .with_mock_github()
        .with_git_interceptor()
        .build();

    // Setup: Create 'main' and a feature branch
    ctx.checkout_managed_private("feature-stack");

    ctx.commit_with_gherrit_id("Commit A");
    let commit_a_id = ctx.gherrit_id("HEAD").unwrap();
    let commit_a_oid = ctx.head_oid();

    ctx.commit_with_gherrit_id("Commit B");
    let commit_b_id = ctx.gherrit_id("HEAD").unwrap();
    let commit_b_oid = ctx.head_oid();

    // Trigger Pre-Push Hook (Simulate 'git push'). We call the hook directly
    // because simulating a real 'git push' that calls the hook recursively is
    // complex in a test env.
    testutil::assert_success_snapshot!(
        ctx,
        ctx.gherrit_cmd().args(["hook", "pre-push"]),
        "full_stack_lifecycle_push"
    );

    // Verify Side Effects (Mock Only)
    testutil::assert_pr_snapshot!(ctx, "full_stack_lifecycle_state");

    assert!(
        ctx.recorded_pushes().iter().any(|push| push.succeeded()),
        "Expected a successful push"
    );
    assert_eq!(
        ctx.remote_ref_oid(&format!("refs/heads/{commit_a_id}")).as_deref(),
        Some(commit_a_oid.as_str())
    );
    assert_eq!(
        ctx.remote_ref_oid(&format!("refs/heads/{commit_b_id}")).as_deref(),
        Some(commit_b_oid.as_str())
    );
}

#[test]
fn test_stack_id_comes_only_from_the_trailer_block() {
    let ctx = testutil::test_context!()
        .with_remote()
        .with_initial_commit()
        .with_mock_github()
        .with_git_interceptor()
        .build();
    ctx.checkout_managed_private("trailer-block");
    ctx.run_git(&[
        "commit",
        "--allow-empty",
        "--no-verify",
        "--cleanup=verbatim",
        "-m",
        "Document an example\n\ngherrit-pr-id: Gexample\n\nExplanation.\n\ngherrit-pr-id: Greal",
    ]);

    ctx.hook_cmd("pre-push").assert().success();

    assert!(ctx.remote_ref_oid("refs/heads/Gexample").is_none());
    assert_eq!(ctx.remote_ref_oid("refs/heads/Greal").as_deref(), Some(ctx.head_oid().as_str()));
    let pull_requests = ctx.github().pull_requests();
    assert_eq!(pull_requests.len(), 1);
    let body = pull_requests[0].body.as_deref().expect("created PR body");
    assert!(body.contains("gherrit-pr-id: Gexample"));
    assert!(!body.contains("\ngherrit-pr-id: Greal\n"));
}

#[test]
fn test_unrelated_continued_trailer_does_not_hide_stack_id() {
    let ctx = testutil::test_context!()
        .with_remote()
        .with_initial_commit()
        .with_mock_github()
        .with_git_interceptor()
        .build();
    ctx.checkout_managed_private("continued-trailer");
    ctx.run_git(&[
        "commit",
        "--allow-empty",
        "--no-verify",
        "--cleanup=verbatim",
        "-m",
        "Work\n\nReviewed-by: First\n continuation\ngherrit-pr-id: Gone",
    ]);

    ctx.hook_cmd("pre-push").assert().success();
    assert!(ctx.remote_ref_oid("refs/heads/Gone").is_some());
}

#[test]
fn test_version_increment() {
    let ctx = testutil::test_context!()
        .with_remote()
        .with_initial_commit()
        .with_mock_github()
        .with_git_interceptor()
        .build();

    // Create feature branch
    ctx.checkout_managed_private("feat-versioning");
    ctx.commit_with_gherrit_id("Feature Commit");
    let gherrit_id = ctx.gherrit_id("HEAD").unwrap();
    let v1_oid = ctx.head_oid();
    let managed_ref = format!("refs/heads/{gherrit_id}");
    let v1_ref = format!("refs/tags/gherrit/{gherrit_id}/v1");
    let v2_ref = format!("refs/tags/gherrit/{gherrit_id}/v2");

    // Push 1 (v1)
    testutil::assert_success_snapshot!(ctx, ctx.hook_cmd("pre-push"), "version_increment_v1");

    assert_eq!(ctx.remote_ref_oid(&managed_ref).as_deref(), Some(v1_oid.as_str()));
    assert_eq!(ctx.remote_ref_oid(&v1_ref).as_deref(), Some(v1_oid.as_str()));

    // Amend commit (modifies SHA, keeps Change-ID)
    ctx.amend();
    let v2_oid = ctx.head_oid();
    assert_ne!(v2_oid, v1_oid);

    // Push 2 (v2)
    testutil::assert_success_snapshot!(ctx, ctx.hook_cmd("pre-push"), "version_increment_v2");

    assert_eq!(ctx.remote_ref_oid(&managed_ref).as_deref(), Some(v2_oid.as_str()));
    assert_eq!(ctx.remote_ref_oid(&v1_ref).as_deref(), Some(v1_oid.as_str()));
    assert_eq!(ctx.remote_ref_oid(&v2_ref).as_deref(), Some(v2_oid.as_str()));

    let pushes = ctx.recorded_pushes();
    assert_eq!(pushes.len(), 2, "Expected one push per published version");
    assert!(
        pushes[1].arguments().iter().all(|argument| !argument.contains(&v1_ref)),
        "The second push must not attempt to republish the immutable v1 tag: {:?}",
        pushes[1].arguments()
    );
}

#[test]
fn test_optimistic_locking_conflict() {
    let ctx = testutil::test_context!()
        .with_remote()
        .with_initial_commit()
        .with_mock_github()
        .with_git_interceptor()
        .build();

    // Initial setup
    ctx.checkout_managed_private("feature-conflict");
    ctx.commit_with_gherrit_id("Commit V1");

    // Push V1
    testutil::assert_success_snapshot!(ctx, ctx.hook_cmd("pre-push"), "optimistic_locking_v1");

    let gherrit_id = ctx.gherrit_id("HEAD").unwrap();
    let managed_ref = format!("refs/heads/{gherrit_id}");
    let pushed_oid = ctx.remote_ref_oid(&managed_ref).expect("Managed ref was not pushed");

    // Simulate race condition: Create v2 tag on REMOTE manually. The next
    // version should be v2 (since v1 exists). Note that in a bare repo, we can
    // create refs directly.
    let tag_name = format!("gherrit/{}/v2", gherrit_id);

    // Create tag pointing to the branch we just pushed
    ctx.remote_git_cmd()
        .args(["tag", &tag_name, &format!("refs/heads/{}", gherrit_id)])
        .assert()
        .success();

    // Create local commit for V2 (modify to ensure new hash).
    // Note: We change the message to guarantee a different SHA even if running
    // quickly. We MUST preserve the Change-ID to simulate an update to the SAME
    // stack.
    let new_msg = format!("Commit V1 (Amended)\n\ngherrit-pr-id: {}", gherrit_id);
    ctx.amend_with_message(&new_msg);

    // Attempt push - should fail due to atomic lock
    testutil::assert_failure_snapshot!(ctx, ctx.hook_cmd("pre-push"), "optimistic_locking_v2_fail");

    let pushes = ctx.recorded_pushes();
    assert_eq!(pushes.len(), 2, "Expected one successful and one failed push");
    assert!(pushes[0].succeeded(), "Initial push should succeed");
    assert!(!pushes[1].succeeded(), "Conflicting push should fail");
    assert_eq!(
        ctx.remote_ref_oid(&managed_ref).as_deref(),
        Some(pushed_oid.as_str()),
        "Failed atomic push must not update the managed ref"
    );
}

#[test]
fn test_graphql_batch_backoff() {
    let ctx = testutil::test_context!()
        .with_remote()
        .with_initial_commit()
        .with_mock_github()
        .with_git_interceptor()
        .build();

    ctx.limit_graphql_operations_per_request(2);
    ctx.checkout_managed_private("batch-backoff");

    for i in 1..=4 {
        ctx.commit_with_gherrit_id(&format!("Commit {i}"));
    }

    testutil::assert_success_snapshot!(ctx, ctx.hook_cmd("pre-push"), "graphql_batch_backoff");

    assert_eq!(
        ctx.recorded_pushes().iter().filter(|push| push.succeeded()).count(),
        1,
        "GraphQL backoff must not alter the independent Git publication batch"
    );
    assert_eq!(ctx.github().pull_requests().len(), 4, "Expected every commit to have a PR");
    insta::assert_debug_snapshot!("graphql_batch_backoff_trace", ctx.github().requests());

    let v1_refs = ctx
        .remote_refs("refs/tags/gherrit")
        .into_iter()
        .filter(|ref_name| ref_name.ends_with("/v1"))
        .count();
    assert_eq!(v1_refs, 4, "Expected every v1 tag on the remote");
}

#[test]
fn stack_visibility_reaches_the_pr_body() {
    let ctx =
        testutil::test_context!().with_remote().with_initial_commit().with_mock_github().build();
    ctx.checkout_managed_private("visibility-topic");
    ctx.commit_with_explicit_gherrit_id("Visible work", "Gvisibility");
    for push_remote in [Some("."), Some("origin"), None] {
        match push_remote {
            Some(remote) => ctx.run_git(&["config", "branch.visibility-topic.pushRemote", remote]),
            None => ctx.run_git(&["config", "--unset", "branch.visibility-topic.pushRemote"]),
        }
        ctx.hook_cmd("pre-push").assert().success();
        let prs = ctx.github().pull_requests();
        assert_eq!(prs.len(), 1);
        assert_eq!(
            prs[0]
                .body
                .as_deref()
                .unwrap()
                .contains("This PR is on branch [visibility-topic](../tree/visibility-topic)."),
            push_remote != Some(".")
        );
    }
}

#[test]
fn each_selected_version_reaches_its_refs_and_pr_body() {
    let ctx = testutil::test_context!()
        .repository("owner", "repo")
        .with_remote()
        .with_initial_commit()
        .with_mock_github()
        .with_git_interceptor()
        .build();
    ctx.checkout_managed_private("different-versions");
    let mut expected = Vec::new();
    for (id, previous, version) in [("Gzeta", 3, 4), ("Galpha", 0, 1), ("Gmiddle", 7, 8)] {
        ctx.commit_with_explicit_gherrit_id(&format!("Work for {id}"), id);
        let oid = ctx.head_oid();
        if previous != 0 {
            ctx.run_git(&["update-ref", &format!("refs/tags/gherrit/{id}/v{previous}"), &oid]);
        }
        expected.push((id, oid, version));
    }
    ctx.hook_cmd("pre-push").assert().success();
    let prs = ctx.github().pull_requests();
    assert_eq!(prs.len(), 3);
    for (id, oid, version) in expected {
        let tag = format!("refs/tags/gherrit/{id}/v{version}");
        assert_eq!(ctx.remote_ref_oid(&tag).as_deref(), Some(oid.as_str()));
        assert_eq!(ctx.remote_ref_oid(&format!("refs/heads/{id}")).as_deref(), Some(oid.as_str()));
        ctx.git_cmd()
            .args(["show-ref", "--verify", "--hash", &tag])
            .assert()
            .success()
            .stdout(format!("{oid}\n"));
        let pr = prs.iter().find(|pr| pr.head == id).unwrap();
        let body = pr.body.as_deref().unwrap();
        if version == 1 {
            assert!(!body.contains("**Latest Update:**"));
        } else {
            let previous = version - 1;
            assert!(body.contains(&format!("**Latest Update:** v{version} — [Compare vs v{previous}](/owner/repo/compare/gherrit/{id}/v{previous}..gherrit/{id}/v{version})")), "{id}: {body}");
        }
    }
    assert_eq!(ctx.recorded_pushes().len(), 1);
}

#[test]
fn mixed_pr_slots_keep_created_metadata_after_update_failure_and_retry() {
    use predicates::prelude::*;
    let ctx = testutil::test_context!()
        .with_remote()
        .with_initial_commit()
        .with_mock_github()
        .with_git_interceptor()
        .build();
    ctx.checkout_managed_private("mixed-slots");
    for id in ["Gzeta", "Galpha", "Gmiddle"] {
        ctx.commit_with_explicit_gherrit_id(&format!("Work {id}\n\nBody {id}"), id);
    }
    ctx.github().seed_pull_request(testutil::PullRequestSeed {
        number: 7,
        title: "Previous title".to_owned(),
        body: "Retained body".to_owned(),
        head: "Galpha".to_owned(),
        base: "main".to_owned(),
    });
    let existing = ctx.github().pull_requests().pop().unwrap();
    ctx.inject_failure(testutil::FailureKind::UpdatePr);
    ctx.hook_cmd("pre-push")
        .assert()
        .failure()
        .stderr(predicate::str::contains("Injected UpdatePr failure"));
    ctx.assert_failure_consumed();
    let initial = ctx.github().pull_requests();
    assert_eq!(initial.len(), 3);
    assert_eq!(initial.iter().find(|pr| pr.head == "Galpha").unwrap(), &existing);
    for (id, base) in [("Gzeta", "main"), ("Gmiddle", "Galpha")] {
        let pr = initial.iter().find(|pr| pr.head == id).unwrap();
        assert_eq!(pr.title.as_deref(), Some(format!("Work {id}").as_str()));
        assert_eq!(pr.body.as_deref(), Some(format!("Body {id}\n\n\n").as_str()));
        assert_eq!(pr.base, base);
        assert_eq!(pr.state, testutil::PullRequestState::Open);
    }
    assert_eq!(
        ctx.github().requests().last(),
        Some(&vec![testutil::GraphQlOperation::UpdatePr; 3])
    );
    ctx.hook_cmd("pre-push").assert().success();
    let final_prs = ctx.github().pull_requests();
    assert_eq!(final_prs.len(), 3);
    for (id, base) in [("Gzeta", "main"), ("Galpha", "Gzeta"), ("Gmiddle", "Galpha")] {
        let pr = final_prs.iter().find(|pr| pr.head == id).unwrap();
        assert_eq!(pr.number, initial.iter().find(|pr| pr.head == id).unwrap().number);
        assert_eq!(pr.title.as_deref(), Some(format!("Work {id}").as_str()));
        assert_eq!(pr.base, base);
        assert!(pr.body.as_deref().unwrap().contains(&format!("Body {id}")));
    }
    assert_eq!(
        ctx.github()
            .requests()
            .into_iter()
            .flatten()
            .filter(|operation| *operation == testutil::GraphQlOperation::CreatePr)
            .count(),
        2
    );
}

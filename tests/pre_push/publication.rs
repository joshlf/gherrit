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
fn test_first_parent_stack_excludes_commits_reachable_only_through_a_merge() {
    let ctx = testutil::test_context!()
        .with_remote()
        .with_initial_commit()
        .with_mock_github()
        .with_git_interceptor()
        .build();
    ctx.checkout_managed_private("first-parent-merge");
    ctx.commit_with_gherrit_id("Stack change");
    let stack_id = ctx.gherrit_id("HEAD").unwrap();

    ctx.run_git(&["checkout", "-b", "side", "main"]);
    ctx.commit_with_gherrit_id("Side change");
    let side_id = ctx.gherrit_id("HEAD").unwrap();
    ctx.run_git(&["checkout", "first-parent-merge"]);
    ctx.run_git(&["merge", "--no-ff", "side", "-m", "Merge side\n\ngherrit-pr-id: Gmerge"]);

    ctx.hook_cmd("pre-push").assert().success();

    assert!(ctx.remote_ref_oid(&format!("refs/heads/{stack_id}")).is_some());
    assert!(ctx.remote_ref_oid("refs/heads/Gmerge").is_some());
    assert!(ctx.remote_ref_oid(&format!("refs/heads/{side_id}")).is_none());
    let pull_requests = ctx.github().pull_requests();
    assert_eq!(
        pull_requests.iter().map(|pr| (pr.head.as_str(), pr.base.as_str())).collect::<Vec<_>>(),
        [(stack_id.as_str(), "main"), ("Gmerge", stack_id.as_str())]
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
fn test_replacement_ref_is_ignored_even_with_gix_075_false_polarity() {
    let ctx = testutil::test_context!()
        .with_remote()
        .with_initial_commit()
        .with_mock_github()
        .with_git_interceptor()
        .build();
    ctx.checkout_managed_private("replacement-ref");
    let id = ctx.commit_with_gherrit_id("Literal commit");
    let original = ctx.head_oid();
    let tree = String::from_utf8(
        ctx.git_cmd()
            .args(["rev-parse", "HEAD^{tree}"])
            .assert()
            .success()
            .get_output()
            .stdout
            .clone(),
    )
    .unwrap();
    let parent = String::from_utf8(
        ctx.git_cmd().args(["rev-parse", "HEAD^"]).assert().success().get_output().stdout.clone(),
    )
    .unwrap();
    let replacement = String::from_utf8(
        ctx.git_cmd()
            .arg("commit-tree")
            .arg(tree.trim())
            .arg("-p")
            .arg(parent.trim())
            .args(["-m", "Replacement without a GHerrit ID"])
            .assert()
            .success()
            .get_output()
            .stdout
            .clone(),
    )
    .unwrap();
    ctx.git_cmd().arg("replace").arg(&original).arg(replacement.trim()).assert().success();
    ctx.run_git(&["config", "core.useReplaceRefs", "false"]);
    ctx.git_cmd()
        .args(["show-ref", "--verify"])
        .arg(format!("refs/replace/{original}"))
        .assert()
        .success();

    ctx.hook_cmd("pre-push").env("GIT_NO_REPLACE_OBJECTS", "0").assert().success();

    assert_eq!(ctx.remote_ref_oid(&format!("refs/heads/{id}")).as_deref(), Some(original.as_str()));
    let pull_requests = ctx.github().pull_requests();
    assert_eq!(pull_requests.len(), 1);
    assert_eq!(pull_requests[0].title.as_deref(), Some("Literal commit"));
    assert_eq!(&ctx.recorded_pushes()[0].arguments()[..3], ["git", "--no-replace-objects", "push"]);
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

    ctx.limit_graphql_query_operations_per_request(2);
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
    let requests = ctx.github().requests();
    insta::assert_debug_snapshot!("graphql_batch_backoff_trace", requests);
    assert!(
        ctx.github()
            .requests()
            .iter()
            .any(|request| { request == &vec![testutil::GraphQlOperation::CreatePr; 4] }),
        "query backoff must not impose its learned limit on mutation batches"
    );

    let v1_refs = ctx
        .remote_refs("refs/tags/gherrit")
        .into_iter()
        .filter(|ref_name| ref_name.ends_with("/v1"))
        .count();
    assert_eq!(v1_refs, 4, "Expected every v1 tag on the remote");
}

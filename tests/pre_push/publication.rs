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
fn test_ambiguous_mutation_batch_fails_closed() {
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

    ctx.hook_cmd("pre-push")
        .assert()
        .failure()
        .stderr(predicates::str::contains("cannot safely retry the ambiguous write"));

    assert_eq!(
        ctx.recorded_pushes().iter().filter(|push| push.succeeded()).count(),
        1,
        "GraphQL backoff must not alter the independent Git publication batch"
    );
    assert!(ctx.github().pull_requests().is_empty());
    let requests = ctx.github().requests();
    assert_eq!(
        requests
            .iter()
            .filter(|operations| {
                operations
                    .iter()
                    .all(|operation| *operation == testutil::GraphQlOperation::CreatePr)
            })
            .count(),
        1,
        "an ambiguously acknowledged mutation batch must not be replayed"
    );

    let v1_refs = ctx
        .remote_refs("refs/tags/gherrit")
        .into_iter()
        .filter(|ref_name| ref_name.ends_with("/v1"))
        .count();
    assert_eq!(v1_refs, 4, "Expected every v1 tag on the remote");
}

#[test]
fn test_full_stack_lifecycle_mocked() {
    let ctx = testutil::test_context!()
        .with_remote()
        .with_installed_hooks()
        .with_initial_commit()
        .with_mock_github()
        .with_git_interceptor()
        .build();

    // Setup: Create 'main' and a feature branch
    ctx.checkout_new("feature-stack");

    ctx.commit("Commit A");

    ctx.commit("Commit B");

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

    ctx.inspect_mock_state(|state| {
        assert!(state.pushes.iter().any(|push| push.succeeded()), "Expected a successful push");
    });
    assert!(!ctx.remote_refs("refs/heads").is_empty(), "Expected remote branches to be updated");
}

#[test]
fn test_version_increment() {
    let ctx = testutil::test_context!()
        .with_remote()
        .with_installed_hooks()
        .with_initial_commit()
        .with_mock_github()
        .with_git_interceptor()
        .build();

    // Create feature branch
    ctx.checkout_new("feat-versioning");
    ctx.commit("Feature Commit");

    // Push 1 (v1)
    testutil::assert_success_snapshot!(ctx, ctx.hook_cmd("pre-push"), "version_increment_v1");

    // Verify v1 pushed
    let v1_count = ctx.count_successfully_pushed_containing("/v1");
    assert!(v1_count > 0, "Expected v1 tag to be pushed");

    // Amend commit (modifies SHA, keeps Change-ID)
    ctx.amend();

    // Push 2 (v2)
    testutil::assert_success_snapshot!(ctx, ctx.hook_cmd("pre-push"), "version_increment_v2");

    // Verify v2 pushed
    let v2_count = ctx.count_successfully_pushed_containing("/v2");
    assert!(v2_count > 0, "Expected v2 tag to be pushed");

    // Verify v1 NOT pushed AGAIN.
    let v1_count_final = ctx.count_successfully_pushed_containing("/v1");
    assert_eq!(v1_count_final, v1_count, "v1 tag should NOT be pushed again in the second push.");

    // Verify that tags actually exist on the remote.
    let tags = ctx.remote_refs("refs/tags/gherrit");
    assert!(tags.iter().any(|tag| tag.ends_with("/v1")), "Remote should contain v1 tag");
    assert!(tags.iter().any(|tag| tag.ends_with("/v2")), "Remote should contain v2 tag");
}

#[test]
fn test_remote_version_allocation_skips_occupied_versions() {
    let ctx = testutil::test_context!()
        .with_remote()
        .with_installed_hooks()
        .with_initial_commit()
        .with_mock_github()
        .with_git_interceptor()
        .build();

    // Initial setup
    ctx.checkout_new("feature-conflict");
    ctx.commit("Commit V1");

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

    // Remote version allocation observes the occupied v2 and publishes v3.
    testutil::assert_success_snapshot!(
        ctx,
        ctx.hook_cmd("pre-push"),
        "remote_version_allocation_v3"
    );

    ctx.inspect_mock_state(|state| {
        assert_eq!(state.pushes.len(), 2, "Expected two successful publications");
        assert!(state.pushes.iter().all(testutil::mock_server::GitPush::succeeded));
    });
    assert_ne!(ctx.head_oid(), pushed_oid);
    assert_eq!(ctx.remote_ref_oid(&managed_ref).as_deref(), Some(ctx.head_oid().as_str()));
    assert!(ctx.count_successfully_pushed_containing("/v3") > 0);
    let tags = ctx.remote_refs(&format!("refs/tags/gherrit/{gherrit_id}"));
    assert!(tags.iter().any(|tag| tag.ends_with("/v1")));
    assert!(tags.iter().any(|tag| tag.ends_with("/v2")));
    assert!(tags.iter().any(|tag| tag.ends_with("/v3")));
}

#[test]
fn test_exact_noop_retry_is_projection_only() {
    let ctx = testutil::test_context!()
        .with_remote()
        .with_installed_hooks()
        .with_initial_commit()
        .with_mock_github()
        .with_git_interceptor()
        .build();

    ctx.checkout_new("projection-only");
    ctx.commit("Projection-only retry");
    ctx.hook_cmd("pre-push").assert().success();

    let pushes_after_first = {
        let mut count = 0;
        ctx.inspect_mock_state(|state| count = state.pushes.len());
        count
    };
    let tags_after_first = ctx.remote_refs("refs/tags/gherrit");

    ctx.hook_cmd("pre-push").assert().success();

    ctx.inspect_mock_state(|state| {
        assert_eq!(state.pushes.len(), pushes_after_first, "No-op retry must not push Git refs");
        assert_eq!(state.prs.len(), 1);
        assert_eq!(state.prs[0].state, "OPEN");
    });
    assert_eq!(ctx.remote_refs("refs/tags/gherrit"), tags_after_first);
}

#[test]
fn test_graphql_batch_backoff() {
    let ctx = testutil::test_context!()
        .with_remote()
        .with_installed_hooks()
        .with_initial_commit()
        .with_mock_github()
        .with_git_interceptor()
        .build();

    ctx.limit_graphql_operations_per_request(2);
    ctx.checkout_new("batch-backoff");

    for i in 1..=4 {
        ctx.commit(&format!("Commit {}", i));
    }

    testutil::assert_success_snapshot!(
        ctx,
        ctx.hook_cmd("pre-push").env("GHERRIT_TEST_PUSH_BATCH_LEN", "2"),
        "graphql_batch_backoff"
    );

    ctx.inspect_mock_state(|state| {
        assert_eq!(
            state.pushes.iter().filter(|push| push.succeeded()).count(),
            2,
            "Expected two pushes at the test batch size"
        );
        assert_eq!(state.prs.len(), 4, "Expected every commit to have a PR");
        insta::assert_debug_snapshot!("graphql_batch_backoff_trace", state.graphql_requests);
    });

    let v1_refs = ctx
        .remote_refs("refs/tags/gherrit")
        .into_iter()
        .filter(|ref_name| ref_name.ends_with("/v1"))
        .count();
    assert_eq!(v1_refs, 4, "Expected every v1 tag on the remote");
}

#[test]
fn projection_only_requires_latest_remote_tag_to_match_the_branch_head() {
    let ctx = testutil::test_context!()
        .with_remote()
        .with_installed_hooks()
        .with_initial_commit()
        .with_mock_github()
        .with_git_interceptor()
        .build();

    ctx.checkout_new("version-coherence");
    ctx.commit("Version coherence");
    ctx.hook_cmd("pre-push").assert().success();
    let id = ctx.gherrit_id("HEAD").unwrap();
    let remote_refs = ctx.remote_refs("refs");

    ctx.remote_git_cmd()
        .args(["tag", &format!("gherrit/{id}/v2"), "refs/heads/main"])
        .assert()
        .success();

    ctx.hook_cmd("pre-push")
        .assert()
        .failure()
        .stderr(predicates::str::contains("Refusing to extend this inconsistent remote history"));
    let expected =
        remote_refs.into_iter().chain([format!("refs/tags/gherrit/{id}/v2")]).collect::<Vec<_>>();
    assert_eq!(ctx.remote_refs("refs"), expected);
}

#[test]
fn existing_branch_without_an_authoritative_version_tag_is_rejected() {
    let ctx = testutil::test_context!()
        .with_remote()
        .with_installed_hooks()
        .with_initial_commit()
        .with_mock_github()
        .with_git_interceptor()
        .build();

    ctx.checkout_new("missing-version-repair");
    ctx.commit("Missing version repair");
    ctx.hook_cmd("pre-push").assert().success();
    let id = ctx.gherrit_id("HEAD").unwrap();
    let branch = format!("refs/heads/{id}");
    let oid = ctx.remote_ref_oid(&branch).unwrap();
    ctx.remote_git_cmd()
        .args(["update-ref", "-d", &format!("refs/tags/gherrit/{id}/v1")])
        .assert()
        .success();

    ctx.hook_cmd("pre-push")
        .assert()
        .failure()
        .stderr(predicates::str::contains("no authoritative GHerrit patch-version tag"));
    assert_eq!(ctx.remote_ref_oid(&branch).as_deref(), Some(oid.as_str()));
    assert!(ctx.remote_ref_oid(&format!("refs/tags/gherrit/{id}/v1")).is_none());
}

#[test]
fn update_requires_latest_remote_tag_to_match_the_observed_branch_head() {
    let ctx = testutil::test_context!()
        .with_remote()
        .with_installed_hooks()
        .with_initial_commit()
        .with_mock_github()
        .with_git_interceptor()
        .build();

    ctx.checkout_new("version-update-coherence");
    ctx.commit("Version update coherence");
    ctx.hook_cmd("pre-push").assert().success();
    let id = ctx.gherrit_id("HEAD").unwrap();
    let original_head = ctx.remote_ref_oid(&format!("refs/heads/{id}")).unwrap();
    ctx.remote_git_cmd()
        .args(["tag", &format!("gherrit/{id}/v2"), "refs/heads/main"])
        .assert()
        .success();
    ctx.amend();

    ctx.hook_cmd("pre-push")
        .assert()
        .failure()
        .stderr(predicates::str::contains("Refusing to extend this inconsistent remote history"));
    assert_eq!(
        ctx.remote_ref_oid(&format!("refs/heads/{id}")).as_deref(),
        Some(original_head.as_str())
    );
}

#[test]
fn rejects_noncanonical_remote_version_names() {
    for suffix in ["0", "01", "+1", "-1"] {
        let ctx = testutil::test_context!()
            .with_remote()
            .with_installed_hooks()
            .with_initial_commit()
            .with_mock_github()
            .with_git_interceptor()
            .build();

        ctx.checkout_new(&format!("bad-version-{suffix}"));
        ctx.commit("Bad remote version spelling");
        let id = ctx.gherrit_id("HEAD").unwrap();
        ctx.remote_git_cmd()
            .args(["tag", &format!("gherrit/{id}/v{suffix}"), "refs/heads/main"])
            .assert()
            .success();

        ctx.hook_cmd("pre-push")
            .assert()
            .failure()
            .stderr(predicates::str::contains("noncanonical version number"));
    }
}

#[test]
fn rejects_gaps_in_authoritative_remote_version_history() {
    let ctx = testutil::test_context!()
        .with_remote()
        .with_installed_hooks()
        .with_initial_commit()
        .with_mock_github()
        .with_git_interceptor()
        .build();

    ctx.checkout_new("missing-version-gap");
    ctx.commit("Missing version gap");
    ctx.hook_cmd("pre-push").assert().success();
    let id = ctx.gherrit_id("HEAD").unwrap();
    ctx.remote_git_cmd()
        .args(["tag", &format!("gherrit/{id}/v3"), &format!("refs/heads/{id}")])
        .assert()
        .success();

    ctx.hook_cmd("pre-push")
        .assert()
        .failure()
        .stderr(predicates::str::contains("missing authoritative version v2 before v3"));
}

#[test]
fn annotated_remote_version_uses_its_peeled_commit_target() {
    let ctx = testutil::test_context!()
        .with_remote()
        .with_installed_hooks()
        .with_initial_commit()
        .with_mock_github()
        .with_git_interceptor()
        .build();

    ctx.checkout_new("annotated-version");
    ctx.commit("Annotated version");
    ctx.hook_cmd("pre-push").assert().success();
    let id = ctx.gherrit_id("HEAD").unwrap();
    ctx.remote_git_cmd()
        .args([
            "-c",
            "user.name=GHerrit Test",
            "-c",
            "user.email=gherrit@example.com",
            "tag",
            "-a",
            "-m",
            "annotated",
            &format!("gherrit/{id}/v2"),
            &format!("refs/heads/{id}"),
        ])
        .assert()
        .success();
    let mut pushes = 0;
    ctx.inspect_mock_state(|state| pushes = state.pushes.len());

    ctx.hook_cmd("pre-push").assert().success();
    ctx.inspect_mock_state(|state| assert_eq!(state.pushes.len(), pushes));
}

#[test]
fn exact_local_sync_after_a_versioned_cascade_is_projection_only() {
    let ctx = testutil::test_context!()
        .with_remote()
        .with_installed_hooks()
        .with_initial_commit()
        .with_mock_github()
        .with_git_interceptor()
        .build();

    ctx.checkout_new("cascade-sync");
    ctx.commit("Root");
    let root_oid = ctx.head_oid();
    ctx.commit("Child");
    let child_id = ctx.gherrit_id("HEAD").unwrap();
    ctx.hook_cmd("pre-push").assert().success();

    ctx.run_git(&["checkout", "main"]);
    ctx.git_cmd()
        .args(["commit", "--allow-empty", "--no-verify", "-m", "Land root as squash"])
        .assert()
        .success();
    ctx.git_cmd().args(["push", "--quiet", "--no-verify", "origin", "main"]).assert().success();
    ctx.run_git(&["checkout", "cascade-sync"]);
    ctx.run_git(&["rebase", "--onto", "main", &root_oid]);
    let cascaded_oid = ctx.head_oid();
    ctx.git_cmd()
        .args([
            "push",
            "--quiet",
            "--no-verify",
            "--atomic",
            "--force",
            "origin",
            &format!("HEAD:refs/heads/{child_id}"),
            &format!("HEAD:refs/tags/gherrit/{child_id}/v2"),
        ])
        .assert()
        .success();
    ctx.mutate_mock_state(|state| {
        let child = state.prs.iter_mut().find(|pr| pr.head.ref_field == child_id).unwrap();
        child.base.ref_field = "main".to_string();
    });
    let mut pushes_before = 0;
    ctx.inspect_mock_state(|state| pushes_before = state.pushes.len());

    ctx.hook_cmd("pre-push").assert().success();
    assert_eq!(
        ctx.remote_ref_oid(&format!("refs/heads/{child_id}")).as_deref(),
        Some(cascaded_oid.as_str())
    );
    assert_eq!(
        ctx.remote_ref_oid(&format!("refs/tags/gherrit/{child_id}/v2")).as_deref(),
        Some(cascaded_oid.as_str())
    );
    ctx.inspect_mock_state(|state| {
        assert_eq!(state.pushes.len(), pushes_before, "exact cascade sync must not publish v3");
        let child = state.prs.iter().find(|pr| pr.head.ref_field == child_id).unwrap();
        assert_eq!(child.base.ref_field, "main");
        assert!(child.body.as_deref().unwrap().contains("\"parent\":null"));
    });
}

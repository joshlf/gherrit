use std::fs;

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
fn test_unchanged_publication_is_a_git_noop() {
    let ctx = testutil::test_context!()
        .with_remote()
        .with_initial_commit()
        .with_mock_github()
        .with_git_interceptor()
        .build();
    ctx.checkout_managed_private("unchanged-publication");
    ctx.commit_with_gherrit_id("Unchanged commit");

    let gherrit_id = ctx.gherrit_id("HEAD").unwrap();
    let v1_ref = format!("refs/tags/gherrit/{gherrit_id}/v1");
    let v2_ref = format!("refs/tags/gherrit/{gherrit_id}/v2");
    ctx.hook_cmd("pre-push").assert().success();
    let requests_after_first_run = ctx.github().requests().len();

    ctx.hook_cmd("pre-push").assert().success();

    assert_eq!(ctx.recorded_pushes().len(), 1, "An unchanged retry must not republish Git refs");
    assert!(ctx.remote_ref_oid(&v1_ref).is_some());
    assert!(ctx.remote_ref_oid(&v2_ref).is_none());
    assert!(
        ctx.github().requests().len() > requests_after_first_run,
        "A Git no-op must still reobserve and reconcile the PR projection"
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

    // The known-occupied next version is rejected before any remote write.
    testutil::assert_failure_snapshot!(ctx, ctx.hook_cmd("pre-push"), "optimistic_locking_v2_fail");

    let pushes = ctx.recorded_pushes();
    assert_eq!(pushes.len(), 1, "The conflicting version must be rejected before pushing");
    assert!(pushes[0].succeeded(), "Initial push should succeed");
    assert_eq!(
        ctx.remote_ref_oid(&managed_ref).as_deref(),
        Some(pushed_oid.as_str()),
        "Failed atomic push must not update the managed ref"
    );
}

#[test]
fn test_recovers_a_partially_persisted_publication_without_republishing() {
    let ctx = testutil::test_context!()
        .with_remote()
        .with_initial_commit()
        .with_mock_github()
        .with_git_interceptor()
        .build();
    ctx.checkout_managed_private("recover-version");
    let first_id = ctx.commit_with_gherrit_id("First recoverable commit");
    let first_oid = ctx.head_oid();
    let second_id = ctx.commit_with_gherrit_id("Second recoverable commit");
    let second_oid = ctx.head_oid();

    let first_branch = format!("refs/heads/{first_id}");
    let second_branch = format!("refs/heads/{second_id}");
    let first_version = format!("refs/tags/gherrit/{first_id}/v1");
    let second_version = format!("refs/tags/gherrit/{second_id}/v1");
    let lock_path = ctx.repo_path.join(".git/refs/tags/gherrit").join(&second_id).join("v1.lock");
    fs::create_dir_all(lock_path.parent().unwrap()).unwrap();
    fs::write(&lock_path, "held by test").unwrap();

    ctx.hook_cmd("pre-push")
        .assert()
        .failure()
        .stderr(predicates::str::contains("could not record the matching local checkpoint"));

    assert_eq!(ctx.remote_ref_oid(&first_branch).as_deref(), Some(first_oid.as_str()));
    assert_eq!(ctx.remote_ref_oid(&second_branch).as_deref(), Some(second_oid.as_str()));
    assert_eq!(ctx.remote_ref_oid(&first_version).as_deref(), Some(first_oid.as_str()));
    assert_eq!(ctx.remote_ref_oid(&second_version).as_deref(), Some(second_oid.as_str()));
    ctx.git_cmd().args(["rev-parse", "--verify", &first_version]).assert().success();
    ctx.git_cmd().args(["rev-parse", "--verify", "--quiet", &second_version]).assert().code(1);
    assert!(ctx.github().pull_requests().is_empty());
    assert_eq!(ctx.recorded_pushes().len(), 1);

    fs::remove_file(lock_path).unwrap();
    ctx.hook_cmd("pre-push").assert().success();

    ctx.git_cmd()
        .args(["rev-parse", "--verify", &second_version])
        .assert()
        .success()
        .stdout(format!("{second_oid}\n"));
    assert_eq!(ctx.recorded_pushes().len(), 1, "Recovery must not republish the remote refs");
    assert_eq!(ctx.github().pull_requests().len(), 2);
}

#[test]
fn test_rejects_incoherent_remote_publication_before_writing() {
    let ctx = testutil::test_context!()
        .with_remote()
        .with_initial_commit()
        .with_mock_github()
        .with_git_interceptor()
        .build();
    ctx.checkout_managed_private("incoherent-remote");
    ctx.commit_with_gherrit_id("Published commit");
    ctx.hook_cmd("pre-push").assert().success();

    let gherrit_id = ctx.gherrit_id("HEAD").unwrap();
    let managed_ref = format!("refs/heads/{gherrit_id}");
    let push_count = ctx.recorded_pushes().len();
    let mutation_count = ctx
        .github()
        .requests()
        .into_iter()
        .flatten()
        .filter(|operation| *operation != testutil::GraphQlOperation::Query)
        .count();

    ctx.remote_git_cmd().args(["update-ref", "-d", &managed_ref]).assert().success();
    ctx.hook_cmd("pre-push")
        .assert()
        .failure()
        .stderr(predicates::str::contains("has version tags but no managed branch"));

    let commit_oid = ctx.head_oid();
    ctx.remote_git_cmd().args(["update-ref", &managed_ref, &commit_oid]).assert().success();
    ctx.remote_git_cmd().args(["update-ref", &managed_ref, "refs/heads/main"]).assert().success();
    ctx.hook_cmd("pre-push")
        .assert()
        .failure()
        .stderr(predicates::str::contains("does not match latest version tag v1"));

    assert_eq!(ctx.recorded_pushes().len(), push_count);
    assert_eq!(
        ctx.github()
            .requests()
            .into_iter()
            .flatten()
            .filter(|operation| *operation != testutil::GraphQlOperation::Query)
            .count(),
        mutation_count,
        "Invalid remote state must not reach a PR mutation"
    );
}

#[test]
fn test_rejects_malformed_local_checkpoint_names() {
    let overflow = ((usize::MAX as u128) + 1).to_string();
    for suffix in ["0", "01", "1/child", &overflow] {
        let ctx = testutil::test_context!()
            .with_remote()
            .with_initial_commit()
            .with_mock_github()
            .with_git_interceptor()
            .build();
        ctx.checkout_managed_private("malformed-local-checkpoint");
        let gherrit_id = ctx.commit_with_gherrit_id("Unpublished commit");
        let malformed_ref = format!("refs/tags/gherrit/{gherrit_id}/v{suffix}");
        ctx.git_cmd().args(["update-ref", &malformed_ref, "HEAD"]).assert().success();

        ctx.hook_cmd("pre-push")
            .assert()
            .failure()
            .stderr(predicates::str::contains("Malformed local GHerrit version tag"));
        assert!(ctx.recorded_pushes().is_empty());
    }
}

fn drifted_update_stack(branch: &str, count: usize) -> (testutil::TestContext, Vec<String>) {
    let ctx = testutil::test_context!()
        .with_remote()
        .with_initial_commit()
        .with_mock_github()
        .with_git_interceptor()
        .build();

    ctx.checkout_managed_private(branch);

    let commits = (1..=count)
        .map(|index| {
            let title = format!("Commit {index}");
            let id = ctx.commit_with_gherrit_id(&title);
            ctx.github().seed_pull_request(testutil::PullRequestSeed {
                number: index,
                title,
                body: "stale".to_string(),
                head: id.clone(),
                base: "main".to_string(),
            });
            id
        })
        .collect::<Vec<_>>();

    (ctx, commits)
}

fn absent_stack(branch: &str, count: usize) -> (testutil::TestContext, Vec<String>) {
    let ctx = testutil::test_context!()
        .with_remote()
        .with_initial_commit()
        .with_mock_github()
        .with_git_interceptor()
        .build();

    ctx.checkout_managed_private(branch);
    let commits =
        (1..=count).map(|index| ctx.commit_with_gherrit_id(&format!("Commit {index}"))).collect();
    (ctx, commits)
}

fn operation_batch_lengths(
    requests: &[Vec<testutil::GraphQlOperation>],
    operation: testutil::GraphQlOperation,
) -> Vec<usize> {
    requests
        .iter()
        .filter(|operations| {
            !operations.is_empty() && operations.iter().all(|candidate| *candidate == operation)
        })
        .map(Vec::len)
        .collect()
}

fn ambiguous_batch_index(
    requests: &[Vec<testutil::GraphQlOperation>],
    operation: testutil::GraphQlOperation,
    attempted: usize,
) -> usize {
    requests
        .iter()
        .position(|operations| {
            operations.len() == attempted
                && operations.iter().all(|candidate| *candidate == operation)
        })
        .unwrap()
}

fn assert_reobserved_after(
    requests: &[Vec<testutil::GraphQlOperation>],
    operation: testutil::GraphQlOperation,
    attempted: usize,
    stack_len: usize,
) {
    let ambiguous = ambiguous_batch_index(requests, operation, attempted);
    assert!(
        requests[ambiguous + 1..].iter().any(|operations| {
            operations.len() == stack_len
                && operations
                    .iter()
                    .all(|candidate| *candidate == testutil::GraphQlOperation::Query)
        }),
        "GHerrit must reobserve the complete stack after an ambiguous {operation:?} batch: {requests:?}"
    );
}

fn assert_reobserved_before_retry(
    requests: &[Vec<testutil::GraphQlOperation>],
    operation: testutil::GraphQlOperation,
    attempted: usize,
    retry: usize,
    stack_len: usize,
) {
    let ambiguous = ambiguous_batch_index(requests, operation, attempted);
    let first_retry = requests
        .iter()
        .enumerate()
        .skip(ambiguous + 1)
        .find(|(_, operations)| {
            operations.len() == retry && operations.iter().all(|candidate| *candidate == operation)
        })
        .map(|(index, _)| index)
        .unwrap();
    assert!(
        requests[ambiguous + 1..first_retry].iter().any(|operations| {
            operations.len() == stack_len
                && operations
                    .iter()
                    .all(|candidate| *candidate == testutil::GraphQlOperation::Query)
        }),
        "GHerrit must reobserve the complete stack before retrying an ambiguous {operation:?} batch: {requests:?}"
    );
}

fn assert_stack_converged(ctx: &testutil::TestContext, commits: &[String]) {
    let pull_requests = ctx.github().pull_requests();
    assert_eq!(pull_requests.len(), commits.len());
    assert!(
        pull_requests.iter().all(|pr| pr.body != "stale"),
        "Expected every PR to be reconciled"
    );
    let expected_bases = std::iter::once("main".to_string())
        .chain(commits.iter().take(commits.len().saturating_sub(1)).cloned())
        .collect::<Vec<_>>();
    assert_eq!(pull_requests.iter().map(|pr| pr.base.clone()).collect::<Vec<_>>(), expected_bases);
}

#[test]
fn test_update_batch_reobserves_before_backoff_retry() {
    let (ctx, commits) = drifted_update_stack("batch-backoff", 4);
    ctx.limit_graphql_operations_per_request(2);

    testutil::assert_success_snapshot!(ctx, ctx.hook_cmd("pre-push"), "graphql_batch_backoff");

    assert_eq!(
        ctx.recorded_pushes().iter().filter(|push| push.succeeded()).count(),
        1,
        "GraphQL backoff must not alter the independent Git publication batch"
    );
    assert_stack_converged(&ctx, &commits);
    let requests = ctx.github().requests();
    insta::assert_debug_snapshot!("graphql_batch_backoff_trace", &requests);
    assert_eq!(
        operation_batch_lengths(&requests, testutil::GraphQlOperation::UpdatePr),
        [4, 2, 2],
        "the ambiguous batch must be retried at the reduced ceiling"
    );
    assert_reobserved_before_retry(&requests, testutil::GraphQlOperation::UpdatePr, 4, 2, 4);

    let v1_refs = ctx
        .remote_refs("refs/tags/gherrit")
        .into_iter()
        .filter(|ref_name| ref_name.ends_with("/v1"))
        .count();
    assert_eq!(v1_refs, 4, "Expected every v1 tag on the remote");
}

#[test]
fn test_partial_update_reobserves_before_retry() {
    let (ctx, commits) = drifted_update_stack("partial-update", 4);
    ctx.inject_failure(testutil::FailureKind::GraphQlResourceLimit {
        mutation: testutil::GraphQlMutation::UpdatePr,
        applied: 2,
    });

    ctx.hook_cmd("pre-push").assert().success();

    ctx.assert_failure_consumed();
    assert_stack_converged(&ctx, &commits);
    let requests = ctx.github().requests();
    assert_eq!(
        operation_batch_lengths(&requests, testutil::GraphQlOperation::UpdatePr),
        [4, 2],
        "the applied prefix must disappear when the update batch is replanned"
    );
    assert_reobserved_before_retry(&requests, testutil::GraphQlOperation::UpdatePr, 4, 2, 4);
}

#[test]
fn test_fully_applied_update_batch_is_not_replayed() {
    let (ctx, commits) = drifted_update_stack("fully-applied-update", 4);
    ctx.inject_failure(testutil::FailureKind::GraphQlResourceLimit {
        mutation: testutil::GraphQlMutation::UpdatePr,
        applied: 4,
    });

    ctx.hook_cmd("pre-push").assert().success();

    ctx.assert_failure_consumed();
    assert_stack_converged(&ctx, &commits);
    let requests = ctx.github().requests();
    assert_eq!(operation_batch_lengths(&requests, testutil::GraphQlOperation::UpdatePr), [4]);
    assert_reobserved_after(&requests, testutil::GraphQlOperation::UpdatePr, 4, 4);
}

#[test]
fn test_applied_singleton_update_is_not_replayed() {
    let (ctx, commits) = drifted_update_stack("applied-singleton-update", 1);
    ctx.inject_failure(testutil::FailureKind::GraphQlResourceLimit {
        mutation: testutil::GraphQlMutation::UpdatePr,
        applied: 1,
    });

    ctx.hook_cmd("pre-push").assert().success();

    ctx.assert_failure_consumed();
    assert_stack_converged(&ctx, &commits);
    assert_eq!(
        operation_batch_lengths(&ctx.github().requests(), testutil::GraphQlOperation::UpdatePr),
        [1]
    );
}

#[test]
fn test_unapplied_singleton_update_fails_without_replay() {
    let (ctx, _) = drifted_update_stack("unapplied-singleton-update", 1);
    ctx.inject_failure(testutil::FailureKind::GraphQlResourceLimit {
        mutation: testutil::GraphQlMutation::UpdatePr,
        applied: 0,
    });

    ctx.hook_cmd("pre-push")
        .assert()
        .failure()
        .stderr(predicates::str::contains("still unchanged after reobservation"));

    ctx.assert_failure_consumed();
    assert_eq!(ctx.github().pull_requests()[0].body, "stale");
    assert_eq!(
        operation_batch_lengths(&ctx.github().requests(), testutil::GraphQlOperation::UpdatePr),
        [1]
    );
}

#[test]
fn test_creation_batch_reobserves_before_backoff_retry() {
    let (ctx, commits) = absent_stack("create-batch-backoff", 4);
    ctx.limit_graphql_operations_per_request(2);

    ctx.hook_cmd("pre-push").assert().success();

    assert_stack_converged(&ctx, &commits);
    let requests = ctx.github().requests();
    assert_eq!(operation_batch_lengths(&requests, testutil::GraphQlOperation::CreatePr), [4, 2, 2]);
    assert_eq!(
        operation_batch_lengths(&requests, testutil::GraphQlOperation::UpdatePr),
        [2, 2],
        "the reduced mutation ceiling must carry into the update phase"
    );
    assert_reobserved_before_retry(&requests, testutil::GraphQlOperation::CreatePr, 4, 2, 4);
}

#[test]
fn test_partial_creation_reobserves_before_retry() {
    let (ctx, commits) = absent_stack("partial-create", 4);
    ctx.inject_failure(testutil::FailureKind::GraphQlResourceLimit {
        mutation: testutil::GraphQlMutation::CreatePr,
        applied: 2,
    });

    ctx.hook_cmd("pre-push").assert().success();

    ctx.assert_failure_consumed();
    assert_stack_converged(&ctx, &commits);
    let requests = ctx.github().requests();
    assert_eq!(operation_batch_lengths(&requests, testutil::GraphQlOperation::CreatePr), [4, 2]);
    assert_reobserved_before_retry(&requests, testutil::GraphQlOperation::CreatePr, 4, 2, 4);
}

#[test]
fn test_fully_applied_creation_batch_is_not_replayed() {
    let (ctx, commits) = absent_stack("fully-applied-create", 4);
    ctx.inject_failure(testutil::FailureKind::GraphQlResourceLimit {
        mutation: testutil::GraphQlMutation::CreatePr,
        applied: 4,
    });

    ctx.hook_cmd("pre-push").assert().success();

    ctx.assert_failure_consumed();
    assert_stack_converged(&ctx, &commits);
    let requests = ctx.github().requests();
    assert_eq!(operation_batch_lengths(&requests, testutil::GraphQlOperation::CreatePr), [4]);
    assert_reobserved_after(&requests, testutil::GraphQlOperation::CreatePr, 4, 4);
}

#[test]
fn test_applied_singleton_creation_is_not_replayed() {
    let (ctx, commits) = absent_stack("applied-singleton-create", 1);
    ctx.inject_failure(testutil::FailureKind::GraphQlResourceLimit {
        mutation: testutil::GraphQlMutation::CreatePr,
        applied: 1,
    });

    ctx.hook_cmd("pre-push").assert().success();

    ctx.assert_failure_consumed();
    assert_stack_converged(&ctx, &commits);
    assert_eq!(
        operation_batch_lengths(&ctx.github().requests(), testutil::GraphQlOperation::CreatePr),
        [1]
    );
}

#[test]
fn test_unapplied_singleton_creation_fails_without_replay() {
    let (ctx, _) = absent_stack("unapplied-singleton-create", 1);
    ctx.inject_failure(testutil::FailureKind::GraphQlResourceLimit {
        mutation: testutil::GraphQlMutation::CreatePr,
        applied: 0,
    });

    ctx.hook_cmd("pre-push")
        .assert()
        .failure()
        .stderr(predicates::str::contains("still absent after reobservation"));

    ctx.assert_failure_consumed();
    assert!(ctx.github().pull_requests().is_empty());
    assert_eq!(
        operation_batch_lengths(&ctx.github().requests(), testutil::GraphQlOperation::CreatePr),
        [1]
    );
}

use std::fs;

use predicates::prelude::*;

fn stack_with_raw_commit_message(message: &str) -> testutil::TestContext {
    let ctx = testutil::test_context!()
        .with_remote()
        .with_initial_commit()
        .with_mock_github()
        .with_git_interceptor()
        .build();
    ctx.checkout_managed_private("invalid-id");
    ctx.run_git(&["commit", "--allow-empty", "--no-verify", "--cleanup=verbatim", "-m", message]);
    ctx
}

fn unpublished_managed_commit(branch: &str) -> testutil::TestContext {
    let ctx = testutil::test_context!()
        .with_remote()
        .with_initial_commit()
        .with_mock_github()
        .with_git_interceptor()
        .build();
    ctx.checkout_managed_private(branch);
    ctx.commit_with_gherrit_id("Work");
    ctx
}

fn assert_identity_failure_before_github_or_writes(ctx: testutil::TestContext, diagnostic: &str) {
    ctx.hook_cmd("pre-push").assert().failure().stderr(predicate::str::contains(diagnostic));

    assert!(ctx.github().requests().is_empty());
    assert!(ctx.recorded_pushes().is_empty());
}

fn linked_managed_stack(ctx: &testutil::TestContext, branch: &str, id: &str) -> std::path::PathBuf {
    let linked = ctx.dir.path().join(branch);
    ctx.git_cmd()
        .args(["worktree", "add", "-b", branch])
        .arg(&linked)
        .arg("main")
        .assert()
        .success();
    for (suffix, value) in [
        ("gherritManaged", testutil::MANAGED_PRIVATE),
        ("pushRemote", "."),
        ("remote", "."),
        ("merge", &format!("refs/heads/{branch}")),
    ] {
        ctx.git_cmd()
            .current_dir(&linked)
            .arg("config")
            .arg(format!("branch.{branch}.{suffix}"))
            .arg(value)
            .assert()
            .success();
    }
    ctx.git_cmd()
        .current_dir(&linked)
        .args([
            "commit",
            "--allow-empty",
            "--no-verify",
            "-m",
            &format!("Linked work\n\ngherrit-pr-id: {id}"),
        ])
        .assert()
        .success();
    linked
}

#[test]
fn test_empty_stack_id_fails_before_github_or_writes() {
    let ctx = stack_with_raw_commit_message("Work\n\ngherrit-pr-id: ");

    assert_identity_failure_before_github_or_writes(ctx, "missing gherrit-pr-id trailer");
}

#[test]
fn test_multiple_stack_ids_fail_before_github_or_writes() {
    let ctx = stack_with_raw_commit_message("Work\n\ngherrit-pr-id: Gone\ngherrit-pr-id: Gtwo");

    assert_identity_failure_before_github_or_writes(ctx, "multiple gherrit-pr-id trailers");
}

#[test]
fn test_body_lookalike_is_not_a_stack_id() {
    let ctx = stack_with_raw_commit_message(
        "Work\n\ngherrit-pr-id: Gexample\n\nThis final paragraph is not a trailer.",
    );

    assert_identity_failure_before_github_or_writes(ctx, "missing gherrit-pr-id trailer");
}

#[test]
fn test_continued_stack_id_fails_before_github_or_writes() {
    let ctx = stack_with_raw_commit_message("Work\n\ngherrit-pr-id: Gone\n continuation");

    assert_identity_failure_before_github_or_writes(ctx, "invalid gherrit-pr-id trailer");
}

#[test]
fn test_empty_and_valid_stack_ids_are_multiple() {
    let ctx = stack_with_raw_commit_message("Work\n\ngherrit-pr-id: \ngherrit-pr-id: Gvalid");

    assert_identity_failure_before_github_or_writes(ctx, "multiple gherrit-pr-id trailers");
}

#[test]
fn test_duplicate_stack_ids_fail_before_github_or_writes() {
    let ctx = testutil::test_context!()
        .with_remote()
        .with_initial_commit()
        .with_mock_github()
        .with_git_interceptor()
        .build();
    ctx.checkout_managed_private("duplicate-ids");
    ctx.commit_with_explicit_gherrit_id("First", "Gduplicate");
    ctx.commit_with_explicit_gherrit_id("Second", "Gduplicate");

    ctx.hook_cmd("pre-push")
        .assert()
        .failure()
        .stderr(predicate::str::contains("multiple commits with gherrit-pr-id 'Gduplicate'"));

    assert!(ctx.github().requests().is_empty());
    assert!(ctx.recorded_pushes().is_empty());
}

#[test]
fn test_stack_id_duplicated_through_a_merge_fails_before_github_or_writes() {
    let ctx = testutil::test_context!()
        .with_remote()
        .with_initial_commit()
        .with_mock_github()
        .with_git_interceptor()
        .build();
    ctx.checkout_managed_private("duplicate-merged-id");
    ctx.commit_with_explicit_gherrit_id("Stack change", "Gduplicate");
    ctx.run_git(&["checkout", "-b", "side", "main"]);
    ctx.commit_with_explicit_gherrit_id("Side change", "Gduplicate");
    ctx.run_git(&["checkout", "duplicate-merged-id"]);
    ctx.run_git(&["merge", "--no-ff", "side", "-m", "Merge side\n\ngherrit-pr-id: Gmerge"]);

    ctx.hook_cmd("pre-push").assert().failure().stderr(predicate::str::contains(
        "HEAD ancestry contains multiple commits with gherrit-pr-id 'Gduplicate'",
    ));

    assert!(ctx.github().requests().is_empty());
    assert!(ctx.recorded_pushes().is_empty());
}

#[test]
fn test_stack_id_duplicated_in_default_history_fails_before_github_or_writes() {
    let ctx = testutil::test_context!()
        .with_remote()
        .with_initial_commit()
        .with_mock_github()
        .with_git_interceptor()
        .build();
    ctx.commit_with_explicit_gherrit_id("Default-branch history", "Gduplicate");
    ctx.run_git(&["push", "--quiet", "--no-verify", "origin", "refs/heads/main:refs/heads/main"]);
    let fixture_pushes = ctx.recorded_pushes();
    ctx.checkout_managed_private("duplicate-default-id");
    ctx.commit_with_explicit_gherrit_id("Stack change", "Gduplicate");

    ctx.hook_cmd("pre-push").assert().failure().stderr(predicate::str::contains(
        "HEAD ancestry contains multiple commits with gherrit-pr-id 'Gduplicate'",
    ));

    assert!(ctx.github().requests().is_empty());
    assert_eq!(ctx.recorded_pushes(), fixture_pushes);
}

#[test]
fn test_default_branch_must_be_on_the_first_parent_stack_path() {
    let ctx = testutil::test_context!()
        .with_remote()
        .with_initial_commit()
        .with_mock_github()
        .with_git_interceptor()
        .build();
    ctx.run_git(&["checkout", "--orphan", "first-parent"]);
    ctx.configure_managed_private("first-parent");
    ctx.commit_with_gherrit_id("Unrelated first parent");
    ctx.run_git(&[
        "merge",
        "--allow-unrelated-histories",
        "--no-ff",
        "main",
        "-m",
        "Reach the default branch only through the second parent",
    ]);

    ctx.hook_cmd("pre-push")
        .assert()
        .failure()
        .stderr(predicate::str::contains("does not descend from 'main' on its first-parent path"));

    assert!(ctx.github().requests().is_empty());
    assert!(ctx.recorded_pushes().is_empty());
}

#[test]
fn test_nonempty_common_grafts_file_in_linked_worktree_blocks_publication() {
    let ctx = testutil::test_context!().with_remote().with_initial_commit().build();
    let linked = linked_managed_stack(&ctx, "linked-feature", "Glinked");
    let linked_head = ctx
        .git_cmd()
        .current_dir(&linked)
        .args(["rev-parse", "HEAD"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();

    let grafts = ctx.repo_path.join(".git/info/grafts");
    fs::write(grafts, linked_head).unwrap();

    ctx.gherrit_cmd().current_dir(&linked).args(["hook", "pre-push"]).assert().failure().stderr(
        predicate::str::contains(
            "info/grafts file is nonempty because grafts rewrite commit ancestry",
        ),
    );
    assert!(ctx.remote_ref_oid("refs/heads/Glinked").is_none());
}

#[test]
fn test_common_shallow_file_is_checked_despite_gix_config_redirection() {
    let ctx = testutil::test_context!().with_remote().with_initial_commit().build();
    let linked = linked_managed_stack(&ctx, "shallow-feature", "Gshallow");
    ctx.git_cmd()
        .current_dir(&linked)
        .args(["config", "gitoxide.core.shallowFile", "redirected-shallow"])
        .assert()
        .success();
    let linked_head = ctx
        .git_cmd()
        .current_dir(&linked)
        .args(["rev-parse", "HEAD"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();

    fs::write(ctx.repo_path.join(".git/shallow"), linked_head).unwrap();
    ctx.git_cmd()
        .current_dir(&linked)
        .args(["rev-parse", "--is-shallow-repository"])
        .assert()
        .success()
        .stdout("true\n");

    ctx.gherrit_cmd().current_dir(&linked).args(["hook", "pre-push"]).assert().failure().stderr(
        predicate::str::contains(
            "common Git directory's shallow file is nonempty because shallow history omits",
        ),
    );
    assert!(ctx.remote_ref_oid("refs/heads/Gshallow").is_none());
}

#[test]
fn test_effective_shallow_file_from_gix_config_blocks_publication() {
    let ctx = testutil::test_context!().with_remote().with_initial_commit().build();
    ctx.checkout_managed_private("configured-shallow");
    let id = ctx.commit_with_gherrit_id("Configured shallow work");
    ctx.run_git(&["config", "gitoxide.core.shallowFile", "alternate-shallow"]);
    fs::write(ctx.repo_path.join(".git/alternate-shallow"), format!("{}\n", ctx.head_oid()))
        .unwrap();

    ctx.hook_cmd("pre-push").assert().failure().stderr(predicate::str::contains(
        "effective shallow file is nonempty because shallow history omits",
    ));
    assert!(ctx.remote_ref_oid(&format!("refs/heads/{id}")).is_none());
}

#[test]
fn test_unavailable_remote_observation_failure() {
    let ctx = testutil::test_context!().repository("missing", "repo").with_mock_github().build();
    ctx.commit("Init");

    ctx.checkout_managed_private("feature-fail");
    ctx.commit_with_gherrit_id("Work to push");

    // Exercise the production Git adapter against a real unavailable remote.
    ctx.run_git(&["remote", "add", "broken-remote", "missing/repo.git"]);
    ctx.run_git(&["config", "gherrit.remote", "broken-remote"]);

    testutil::assert_failure_snapshot!(
        ctx,
        ctx.hook_cmd("pre-push"),
        "pre_push_failure_broken_remote"
    );
}

#[test]
fn test_pre_push_edit_failure() {
    let ctx =
        testutil::test_context!().with_remote().with_initial_commit().with_mock_github().build();

    // Setup: Create PR first
    ctx.checkout_managed_private("feature-edit-fail");
    ctx.commit_with_gherrit_id("Initial Work");
    // Initial push creates PR
    ctx.gherrit_cmd().args(["hook", "pre-push"]).assert().success();

    // Amend commit to trigger update (edit)
    ctx.amend_with_message("Initial Work (Updated)");

    // Run hook with failure injection
    let requests_before = ctx.github().requests().len();
    ctx.inject_failure(testutil::FailureKind::UpdatePr);

    ctx.gherrit_cmd()
        .args(["hook", "pre-push"])
        .assert()
        .failure()
        .stderr(predicates::str::contains("Injected UpdatePr failure"));
    ctx.assert_failure_consumed();
    let requests = ctx.github().requests();
    assert_eq!(
        &requests[requests_before..],
        [vec![testutil::GraphQlOperation::Query], vec![testutil::GraphQlOperation::UpdatePr],],
        "an indeterminate update response must stop without replay or continuation"
    );
    let prs = ctx.github().pull_requests();
    assert_eq!(prs.len(), 1);
    assert_eq!(prs[0].title.as_deref(), Some("Initial Work"));

    let gherrit_id = ctx.gherrit_id("HEAD").unwrap();
    let remote_ref = format!("refs/heads/{gherrit_id}");
    assert_eq!(ctx.remote_ref_oid(&remote_ref).as_deref(), Some(ctx.head_oid().as_str()));
}

#[test]
fn test_pre_push_ls_remote_failure() {
    let ctx = testutil::test_context!()
        .with_remote()
        .with_initial_commit()
        .with_mock_github()
        .with_git_interceptor()
        .build();
    // Manage branch
    ctx.checkout_managed_private("feature-ls-remote-fail");
    ctx.commit_with_gherrit_id("Work");

    let refs_before = ctx.remote_refs("refs");
    ctx.expect_git_failure(testutil::GitOperation::LsRemoteHeads);
    testutil::assert_failure_snapshot!(
        ctx,
        ctx.hook_cmd("pre-push"),
        "ls_remote_observation_failure"
    );

    ctx.assert_failure_consumed();
    assert_eq!(ctx.remote_refs("refs"), refs_before);
    assert!(ctx.recorded_pushes().is_empty());
    assert!(ctx.github().pull_requests().is_empty());
    assert!(ctx.github().requests().is_empty());
}

#[test]
fn test_pre_push_active_history_observation_failure_precedes_writes() {
    let ctx = unpublished_managed_commit("active-history-failure");
    let refs_before = ctx.remote_refs("refs");
    ctx.expect_git_failure(testutil::GitOperation::LsRemoteActiveVersions);

    ctx.hook_cmd("pre-push")
        .assert()
        .failure()
        .stderr(predicate::str::contains("observing active version history"));

    ctx.assert_failure_consumed();
    assert_eq!(ctx.remote_refs("refs"), refs_before);
    assert!(ctx.recorded_pushes().is_empty());
    assert!(ctx.github().requests().is_empty());
}

#[test]
fn test_later_active_history_observation_failure_precedes_writes() {
    let ctx = unpublished_managed_commit("later-active-history-failure");
    for index in 0..40 {
        let id = format!("G{index:03}{}", "a".repeat(196));
        ctx.commit_with_explicit_gherrit_id(&format!("Work {index}"), &id);
    }
    let refs_before = ctx.remote_refs("refs");
    ctx.expect_git_failure_on_invocation(testutil::GitOperation::LsRemoteActiveVersions, 2);

    ctx.hook_cmd("pre-push")
        .assert()
        .failure()
        .stderr(predicate::str::contains("observing active version history"));

    ctx.assert_failure_consumed();
    assert_eq!(
        ctx.recorded_git_invocations(testutil::GitOperation::LsRemoteActiveVersions).len(),
        2
    );
    assert_eq!(ctx.remote_refs("refs"), refs_before);
    assert!(ctx.recorded_pushes().is_empty());
    assert!(ctx.github().requests().is_empty());
}

#[test]
fn test_pre_push_rejects_an_oversized_late_id_before_any_history_request() {
    let ctx = unpublished_managed_commit("oversized-active-id");
    let oversized = format!("G{}", "a".repeat(20_000));
    ctx.commit_with_explicit_gherrit_id("Oversized later change", &oversized);
    let refs_before = ctx.remote_refs("refs");

    ctx.hook_cmd("pre-push")
        .assert()
        .failure()
        .stderr(predicate::str::contains("too long to observe its remote version history"));

    assert_eq!(ctx.recorded_git_invocations(testutil::GitOperation::LsRemoteHeads).len(), 1);
    assert!(
        ctx.recorded_git_invocations(testutil::GitOperation::LsRemoteActiveVersions).is_empty()
    );
    assert_eq!(ctx.remote_refs("refs"), refs_before);
    assert!(ctx.recorded_pushes().is_empty());
    assert!(ctx.github().requests().is_empty());
}

#[test]
fn test_pre_push_rejects_an_oversized_late_push_tuple_before_any_action() {
    let ctx = unpublished_managed_commit("oversized-push-tuple");
    // This ID fits one active-history observation query but its four rendered
    // branch/tag lease and refspec arguments do not fit one push batch.
    let oversized = format!("G{}", "a".repeat(5_000));
    ctx.commit_with_explicit_gherrit_id("Oversized later push", &oversized);
    let refs_before = ctx.remote_refs("refs");

    ctx.hook_cmd("pre-push")
        .assert()
        .failure()
        .stderr(predicate::str::contains("variable push arguments"))
        .stderr(predicate::str::contains("5001-byte change ID"));

    assert_eq!(ctx.recorded_git_invocations(testutil::GitOperation::LsRemoteHeads).len(), 1);
    assert_eq!(
        ctx.recorded_git_invocations(testutil::GitOperation::LsRemoteActiveVersions).len(),
        1
    );
    assert_eq!(ctx.remote_refs("refs"), refs_before);
    assert!(ctx.recorded_pushes().is_empty());
    assert!(ctx.github().requests().is_empty());
}

#[test]
fn test_pre_push_stops_after_a_later_push_batch_fails() {
    let ctx = unpublished_managed_commit("later-push-batch-failure");
    // Moderately long ref components make three byte-budgeted push batches
    // while keeping the complete temporary-repository path below Windows'
    // traditional MAX_PATH limit.
    let mut last_id = String::new();
    for index in 0..70 {
        let id = format!("G{index:03}{}", "a".repeat(116));
        ctx.commit_with_explicit_gherrit_id(&format!("Work {index}"), &id);
        last_id = id;
    }
    ctx.expect_git_failure_on_invocation(testutil::GitOperation::Push, 2);

    ctx.hook_cmd("pre-push")
        .assert()
        .failure()
        .stderr(predicate::str::contains("`git push` failed"));

    ctx.assert_failure_consumed();
    assert_eq!(ctx.recorded_git_invocations(testutil::GitOperation::Push).len(), 2);
    assert_eq!(ctx.recorded_pushes().len(), 1);
    assert!(ctx.recorded_pushes()[0].succeeded());
    let published_versions = ctx
        .remote_refs("refs/tags/gherrit")
        .into_iter()
        .filter(|name| name.ends_with("/v1"))
        .count();
    assert!(published_versions > 0, "the first batch must have committed");
    assert!(published_versions < 71, "no batch after the failure may run");
    let requests = ctx.github().requests();
    assert!(!requests.is_empty(), "GitHub state must be observed before publication");
    assert!(
        requests.iter().flatten().all(|operation| *operation == testutil::GraphQlOperation::Query),
        "a failed Git batch must stop before any GitHub mutation"
    );
    let events = ctx.boundary_events();
    let observation = events
        .iter()
        .position(|event| {
            matches!(
                event,
                testutil::BoundaryEvent::GraphQl(operations)
                    if operations.iter().all(|operation| *operation == testutil::GraphQlOperation::Query)
            )
        })
        .expect("GitHub observation event");
    let first_push = events
        .iter()
        .position(|event| {
            matches!(event, testutil::BoundaryEvent::Git(testutil::GitOperation::Push))
        })
        .expect("first push event");
    assert!(observation < first_push, "GitHub observation must precede the first Git write");

    ctx.hook_cmd("pre-push").assert().success();

    let version_refs = ctx.remote_refs("refs/tags/gherrit");
    assert_eq!(version_refs.iter().filter(|name| name.ends_with("/v1")).count(), 71);
    assert_eq!(version_refs.iter().filter(|name| name.ends_with("/v2")).count(), 0);
    assert_eq!(ctx.github().pull_requests().len(), 71);
    assert_eq!(ctx.recorded_pushes().len(), 3, "retry publishes only the two missing batches");

    let history_queries =
        ctx.recorded_git_invocations(testutil::GitOperation::LsRemoteActiveVersions).len();
    ctx.amend();
    ctx.hook_cmd("pre-push").assert().success();

    assert!(
        ctx.recorded_git_invocations(testutil::GitOperation::LsRemoteActiveVersions).len()
            >= history_queries + 2,
        "nonempty histories from multiple active-version requests must form one observation"
    );
    assert!(ctx.remote_ref_oid(&format!("refs/tags/gherrit/{last_id}/v2")).is_some());
}

#[test]
fn test_pre_push_rejects_a_null_managed_branch_object_id() {
    let ctx = testutil::test_context!()
        .with_remote()
        .with_initial_commit()
        .with_mock_github()
        .with_git_interceptor()
        .build();
    ctx.checkout_managed_private("feature-null-remote-object");
    ctx.commit_with_explicit_gherrit_id("Work", "Gnull");
    let refs_before = ctx.remote_refs("refs");
    ctx.expect_git_output(
        testutil::GitOperation::LsRemoteHeads,
        "0000000000000000000000000000000000000000\trefs/heads/Gnull\n",
    );

    ctx.hook_cmd("pre-push")
        .assert()
        .failure()
        .stderr(predicate::str::contains("remote ref has a null object ID"));

    ctx.assert_failure_consumed();
    assert_eq!(ctx.remote_refs("refs"), refs_before);
    assert!(ctx.recorded_pushes().is_empty());
    assert!(ctx.github().pull_requests().is_empty());
}

#[test]
fn test_pre_push_rejects_an_owned_base_as_the_default_branch_before_writes() {
    let ctx = unpublished_managed_commit("owned-base-default");
    let id = ctx.gherrit_id("HEAD").unwrap();
    let owned_base = format!("refs/heads/gherrit-bases/{id}");
    ctx.remote_git_cmd().args(["update-ref", &owned_base, "refs/heads/main"]).assert().success();
    ctx.remote_git_cmd().args(["symbolic-ref", "HEAD", &owned_base]).assert().success();
    let refs_before = ctx.remote_refs("refs");

    ctx.hook_cmd("pre-push")
        .assert()
        .failure()
        .stderr(predicate::str::contains("reserved owned-base namespace"));

    assert_eq!(ctx.remote_refs("refs"), refs_before);
    assert!(ctx.recorded_pushes().is_empty());
    assert!(ctx.github().requests().is_empty());
}

#[test]
fn test_pre_push_pr_list_failure() {
    let ctx = testutil::test_context!()
        .with_remote()
        .with_initial_commit()
        .with_mock_github()
        .with_git_interceptor()
        .build();
    ctx.checkout_managed_private("feature-pr-list-fail");
    ctx.commit_with_gherrit_id("Work");

    // Trigger hook
    ctx.inject_failure(testutil::FailureKind::GraphQl);

    ctx.gherrit_cmd()
        .args(["hook", "pre-push"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("Injected GraphQl failure"));
    ctx.assert_failure_consumed();
    assert_eq!(ctx.github().requests(), vec![vec![testutil::GraphQlOperation::Query]]);
    assert!(ctx.github().pull_requests().is_empty());
    assert!(ctx.recorded_pushes().is_empty());
}

#[test]
fn test_pre_push_pr_list_retries_a_transient_http_failure() {
    let ctx = unpublished_managed_commit("feature-pr-list-transient");
    ctx.inject_failure(testutil::FailureKind::QueryHttp(
        testutil::RetryableHttpStatus::ServiceUnavailable,
    ));

    ctx.gherrit_cmd().args(["hook", "pre-push"]).assert().success();

    ctx.assert_failure_consumed();
    assert_eq!(
        ctx.github().requests(),
        [
            vec![testutil::GraphQlOperation::Query],
            vec![testutil::GraphQlOperation::Query],
            vec![testutil::GraphQlOperation::Query],
            vec![testutil::GraphQlOperation::CreatePr],
            vec![testutil::GraphQlOperation::UpdatePr],
        ]
    );
    assert_eq!(ctx.github().pull_requests().len(), 1);
}

#[test]
fn test_pre_push_pr_list_stops_after_three_transient_http_retries() {
    let ctx = unpublished_managed_commit("feature-pr-list-retries-exhausted");
    (0..=3).for_each(|_| {
        ctx.inject_failure(testutil::FailureKind::QueryHttp(
            testutil::RetryableHttpStatus::TooManyRequests,
        ));
    });

    ctx.gherrit_cmd()
        .args(["hook", "pre-push"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("Injected QueryHttp(TooManyRequests) failure"));

    ctx.assert_failure_consumed();
    assert_eq!(ctx.github().requests(), vec![vec![testutil::GraphQlOperation::Query]; 4]);
    assert!(ctx.github().pull_requests().is_empty());
    assert!(ctx.recorded_pushes().is_empty());
}

#[test]
fn test_pre_push_pr_list_retries_a_response_transport_failure() {
    let ctx = unpublished_managed_commit("feature-pr-list-transport");
    ctx.inject_failure(testutil::FailureKind::QueryTransport);

    ctx.gherrit_cmd().args(["hook", "pre-push"]).assert().success();

    ctx.assert_failure_consumed();
    assert_eq!(
        ctx.github().requests(),
        [
            vec![testutil::GraphQlOperation::Query],
            vec![testutil::GraphQlOperation::Query],
            vec![testutil::GraphQlOperation::Query],
            vec![testutil::GraphQlOperation::CreatePr],
            vec![testutil::GraphQlOperation::UpdatePr],
        ]
    );
}

#[test]
fn test_pre_push_pr_list_stops_after_three_response_transport_retries() {
    let ctx = unpublished_managed_commit("feature-pr-list-transport-retries-exhausted");
    (0..=3).for_each(|_| ctx.inject_failure(testutil::FailureKind::QueryTransport));

    ctx.gherrit_cmd().args(["hook", "pre-push"]).assert().failure();

    ctx.assert_failure_consumed();
    assert_eq!(ctx.github().requests(), vec![vec![testutil::GraphQlOperation::Query]; 4]);
    assert!(ctx.github().pull_requests().is_empty());
    assert!(ctx.recorded_pushes().is_empty());
}

#[test]
fn test_pre_push_pr_create_failure() {
    let ctx = testutil::test_context!()
        .with_remote()
        .with_initial_commit()
        .with_mock_github()
        .with_git_interceptor()
        .build();
    ctx.checkout_managed_private("feature-pr-create-fail");
    ctx.commit_with_gherrit_id("Work");

    // Trigger hook
    ctx.inject_failure(testutil::FailureKind::CreatePr);

    ctx.gherrit_cmd()
        .args(["hook", "pre-push"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("Injected CreatePr failure"));
    ctx.assert_failure_consumed();
    assert_eq!(
        ctx.github().requests(),
        [
            vec![testutil::GraphQlOperation::Query],
            vec![testutil::GraphQlOperation::Query],
            vec![testutil::GraphQlOperation::CreatePr],
        ],
        "an indeterminate create response must stop without replay or continuation"
    );
    assert!(ctx.github().pull_requests().is_empty());
    assert_eq!(ctx.recorded_pushes().iter().filter(|push| push.succeeded()).count(), 1);
}

#[test]
fn test_pre_push_pr_create_service_unavailable_is_not_replayed() {
    let ctx = testutil::test_context!()
        .with_remote()
        .with_initial_commit()
        .with_mock_github()
        .with_git_interceptor()
        .build();
    ctx.checkout_managed_private("feature-pr-create-service-unavailable");
    ctx.commit_with_gherrit_id("Work");

    ctx.inject_failure(testutil::FailureKind::CreatePrHttp(
        testutil::RetryableHttpStatus::ServiceUnavailable,
    ));

    ctx.gherrit_cmd()
        .args(["hook", "pre-push"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("indeterminate"));
    ctx.assert_failure_consumed();
    assert_eq!(
        ctx.github().requests(),
        [
            vec![testutil::GraphQlOperation::Query],
            vec![testutil::GraphQlOperation::Query],
            vec![testutil::GraphQlOperation::CreatePr],
        ],
        "a retryable HTTP response must not replay a mutation request"
    );
    assert!(ctx.github().pull_requests().is_empty());
    assert_eq!(ctx.recorded_pushes().iter().filter(|push| push.succeeded()).count(), 1);
}

#[test]
fn test_pre_push_pr_create_redirect_is_not_followed() {
    for redirect in [testutil::RedirectStatus::Temporary, testutil::RedirectStatus::Permanent] {
        let ctx = testutil::test_context!()
            .with_remote()
            .with_initial_commit()
            .with_mock_github()
            .with_git_interceptor()
            .build();
        ctx.checkout_managed_private("feature-pr-create-redirect");
        ctx.commit_with_gherrit_id("Work");
        ctx.inject_failure(testutil::FailureKind::CreatePrRedirect(redirect));

        ctx.gherrit_cmd()
            .args(["hook", "pre-push"])
            .assert()
            .failure()
            .stderr(predicate::str::contains("indeterminate"));

        ctx.assert_failure_consumed();
        assert_eq!(
            ctx.github()
                .requests()
                .iter()
                .filter(|request| request.contains(&testutil::GraphQlOperation::CreatePr))
                .count(),
            1,
            "the original mutation endpoint must receive exactly one request for {redirect:?}"
        );
        assert_eq!(
            ctx.github().redirect_trap_requests(),
            0,
            "the client must not follow {redirect:?} mutation redirects"
        );
        assert!(ctx.github().pull_requests().is_empty());
    }
}

#[test]
fn test_later_mutation_batch_failure_preserves_prior_effects_and_stops() {
    const COMMIT_COUNT: usize = 129;
    const MUTATION_BATCH_LEN: usize = 64;

    let ctx = testutil::test_context!()
        .with_remote()
        .with_initial_commit()
        .with_mock_github()
        .with_git_interceptor()
        .build();
    ctx.checkout_managed_private("feature-multi-batch-ambiguity");
    let ids = (0..COMMIT_COUNT)
        .map(|index| ctx.commit_with_gherrit_id(&format!("Work {index}")))
        .collect::<Vec<_>>();
    ctx.inject_failure(testutil::FailureKind::SecondCreatePrHttp(
        testutil::RetryableHttpStatus::ServiceUnavailable,
    ));

    ctx.gherrit_cmd()
        .args(["hook", "pre-push"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("indeterminate"));

    ctx.assert_failure_consumed();
    let requests = ctx.github().requests();
    let create_requests = requests
        .iter()
        .filter(|request| request.contains(&testutil::GraphQlOperation::CreatePr))
        .collect::<Vec<_>>();
    assert_eq!(
        create_requests.iter().map(|request| request.len()).collect::<Vec<_>>(),
        [MUTATION_BATCH_LEN, MUTATION_BATCH_LEN],
        "the third mutation batch must not be sent after an indeterminate acknowledgement"
    );

    let pull_requests = ctx.github().pull_requests();
    assert_eq!(pull_requests.len(), MUTATION_BATCH_LEN);
    assert_eq!(
        pull_requests.iter().map(|pr| pr.head.as_str()).collect::<Vec<_>>(),
        ids[..MUTATION_BATCH_LEN].iter().map(String::as_str).collect::<Vec<_>>(),
        "effects from the completely acknowledged first batch must persist"
    );
}

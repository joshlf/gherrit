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

fn assert_identity_failure_after_open_before_writes(ctx: testutil::TestContext, diagnostic: &str) {
    ctx.hook_cmd("pre-push").assert().failure().stderr(predicate::str::contains(diagnostic));

    assert_eq!(ctx.github().requests(), [vec![testutil::GraphQlOperation::Query]]);
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
fn test_empty_stack_id_fails_after_open_before_writes() {
    let ctx = stack_with_raw_commit_message("Work\n\ngherrit-pr-id: ");

    assert_identity_failure_after_open_before_writes(ctx, "missing gherrit-pr-id trailer");
}

#[test]
fn test_multiple_stack_ids_fail_after_open_before_writes() {
    let ctx = stack_with_raw_commit_message("Work\n\ngherrit-pr-id: Gone\ngherrit-pr-id: Gtwo");

    assert_identity_failure_after_open_before_writes(ctx, "multiple gherrit-pr-id trailers");
}

#[test]
fn test_body_lookalike_is_not_a_stack_id() {
    let ctx = stack_with_raw_commit_message(
        "Work\n\ngherrit-pr-id: Gexample\n\nThis final paragraph is not a trailer.",
    );

    assert_identity_failure_after_open_before_writes(ctx, "missing gherrit-pr-id trailer");
}

#[test]
fn test_continued_stack_id_fails_after_open_before_writes() {
    let ctx = stack_with_raw_commit_message("Work\n\ngherrit-pr-id: Gone\n continuation");

    assert_identity_failure_after_open_before_writes(ctx, "invalid gherrit-pr-id trailer");
}

#[test]
fn test_empty_and_valid_stack_ids_are_multiple() {
    let ctx = stack_with_raw_commit_message("Work\n\ngherrit-pr-id: \ngherrit-pr-id: Gvalid");

    assert_identity_failure_after_open_before_writes(ctx, "multiple gherrit-pr-id trailers");
}

#[test]
fn test_duplicate_stack_ids_fail_after_open_before_writes() {
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

    assert_eq!(ctx.github().requests(), [vec![testutil::GraphQlOperation::Query]]);
    assert!(ctx.recorded_pushes().is_empty());
}

#[test]
fn test_replacement_cannot_hide_a_stack_id_duplicated_through_a_merge() {
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
    let side = ctx.head_oid();
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
    ctx.git_cmd().arg("replace").arg(side).arg(replacement.trim()).assert().success();
    ctx.git_cmd()
        .args(["log", "-1", "--format=%B", "side"])
        .assert()
        .success()
        .stdout(predicate::str::contains("Replacement without a GHerrit ID"))
        .stdout(predicate::str::contains("gherrit-pr-id").not());
    ctx.git_cmd()
        .args(["--no-replace-objects", "log", "-1", "--format=%B", "side"])
        .assert()
        .success()
        .stdout(predicate::str::contains("gherrit-pr-id: Gduplicate"));
    ctx.run_git(&["checkout", "duplicate-merged-id"]);
    ctx.run_git(&["merge", "--no-ff", "side", "-m", "Merge side\n\ngherrit-pr-id: Gmerge"]);

    ctx.hook_cmd("pre-push").assert().failure().stderr(predicate::str::contains(
        "HEAD ancestry contains multiple commits with gherrit-pr-id 'Gduplicate'",
    ));

    assert_eq!(ctx.github().requests(), [vec![testutil::GraphQlOperation::Query]]);
    assert!(ctx.recorded_pushes().is_empty());
}

#[test]
fn test_stack_id_duplicated_in_default_history_fails_after_open_before_writes() {
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

    assert_eq!(ctx.github().requests(), [vec![testutil::GraphQlOperation::Query]]);
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

    assert_eq!(ctx.github().requests(), [vec![testutil::GraphQlOperation::Query]]);
    assert!(ctx.recorded_pushes().is_empty());
}

#[test]
fn test_nonempty_common_grafts_file_in_linked_worktree_blocks_publication() {
    let ctx = testutil::test_context!()
        .with_remote()
        .with_initial_commit()
        .with_mock_github()
        .with_git_interceptor()
        .build();
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
    let ctx = testutil::test_context!()
        .with_remote()
        .with_initial_commit()
        .with_mock_github()
        .with_git_interceptor()
        .build();
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
    let ctx = testutil::test_context!()
        .with_remote()
        .with_initial_commit()
        .with_mock_github()
        .with_git_interceptor()
        .build();
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
    assert!(
        ctx.recorded_git_invocations(testutil::GitOperation::LsRemoteActiveManagedTags).is_empty()
    );
    assert!(ctx.recorded_git_invocations(testutil::GitOperation::LsRemoteOther).is_empty());
    assert!(ctx.github().pull_requests().is_empty());
    assert_eq!(ctx.github().requests(), [vec![testutil::GraphQlOperation::Query]]);
}

#[test]
fn active_managed_tag_observation_failure_stops_before_writes() {
    let ctx = testutil::test_context!()
        .with_remote()
        .with_initial_commit()
        .with_mock_github()
        .with_git_interceptor()
        .build();
    ctx.checkout_managed_private("feature-managed-tag-observation-fail");
    ctx.commit_with_gherrit_id("Work");
    let refs_before = ctx.remote_refs("refs");
    ctx.expect_git_failure(testutil::GitOperation::LsRemoteActiveManagedTags);

    ctx.hook_cmd("pre-push")
        .assert()
        .failure()
        .stderr(predicate::str::contains("observing active managed tags"));

    ctx.assert_failure_consumed();
    assert_eq!(ctx.remote_refs("refs"), refs_before);
    assert!(ctx.recorded_pushes().is_empty());
    assert_eq!(ctx.github().requests(), [vec![testutil::GraphQlOperation::Query]]);
}

#[test]
fn noncanonical_remote_version_fails_before_writes() {
    let ctx = testutil::test_context!()
        .with_remote()
        .with_initial_commit()
        .with_mock_github()
        .with_git_interceptor()
        .build();
    ctx.checkout_managed_private("feature-invalid-version");
    ctx.commit_with_explicit_gherrit_id("Work", "Ginvalid");
    let refs_before = ctx.remote_refs("refs");
    ctx.expect_git_output(
        testutil::GitOperation::LsRemoteActiveManagedTags,
        "1111111111111111111111111111111111111111\trefs/tags/gherrit/Ginvalid/v0\n",
    );

    ctx.hook_cmd("pre-push")
        .assert()
        .failure()
        .stderr(predicate::str::contains("is not canonical"));

    ctx.assert_failure_consumed();
    assert_eq!(ctx.remote_refs("refs"), refs_before);
    assert!(ctx.recorded_pushes().is_empty());
    assert_eq!(ctx.github().requests(), [vec![testutil::GraphQlOperation::Query]]);
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

    ctx.hook_cmd("pre-push").assert().failure().stderr(predicate::str::contains("null object ID"));

    ctx.assert_failure_consumed();
    assert_eq!(ctx.remote_refs("refs"), refs_before);
    assert!(ctx.recorded_pushes().is_empty());
    assert!(
        ctx.recorded_git_invocations(testutil::GitOperation::LsRemoteActiveManagedTags).is_empty()
    );
    assert!(ctx.github().pull_requests().is_empty());
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
fn test_pre_push_pr_list_does_not_retry_a_fatal_http_failure() {
    let ctx = unpublished_managed_commit("feature-pr-list-bad-request");
    ctx.inject_failure(testutil::FailureKind::QueryBadRequest);

    ctx.gherrit_cmd()
        .args(["hook", "pre-push"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("Injected QueryBadRequest failure"));

    ctx.assert_failure_consumed();
    assert_eq!(ctx.github().requests(), [vec![testutil::GraphQlOperation::Query]]);
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
    let mut literal_parent = ctx.head_oid();
    let tuples = (0..COMMIT_COUNT)
        .map(|index| {
            let id = ctx.commit_with_gherrit_id(&format!("Work {index}"));
            let head = ctx.head_oid();
            testutil::OwnedBaseTuple {
                id,
                version: 1,
                head_oid: head.clone(),
                base_oid: std::mem::replace(&mut literal_parent, head),
                marker_oid: None,
            }
        })
        .collect::<Vec<_>>();
    let ids = tuples.iter().map(|tuple| tuple.id.as_str()).collect::<Vec<_>>();
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

    let events = ctx.external_events();
    let create_batches = events
        .iter()
        .filter_map(|event| match event {
            testutil::ExternalEvent::GraphQl(testutil::GraphQlExchange::Mutation {
                operations,
            }) if operations
                .iter()
                .all(|operation| operation.operation == testutil::GraphQlOperation::CreatePr) =>
            {
                Some(operations)
            }
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(create_batches.len(), 2);
    for (batch, expected) in create_batches
        .iter()
        .zip([&ids[..MUTATION_BATCH_LEN], &ids[MUTATION_BATCH_LEN..2 * MUTATION_BATCH_LEN]])
    {
        assert_eq!(
            batch
                .iter()
                .map(|operation| operation.input["headRefName"].as_str())
                .collect::<Vec<_>>(),
            expected
        );
    }
    assert!(create_batches.iter().flat_map(|batch| batch.as_slice()).all(|operation| {
        operation.operation == testutil::GraphQlOperation::CreatePr
            && operation.input["headRefName"] != ids[2 * MUTATION_BATCH_LEN]
    }));
    assert!(!events.iter().any(|event| matches!(
        event,
        testutil::ExternalEvent::GraphQl(testutil::GraphQlExchange::Mutation {
            operations,
        }) if operations
            .iter()
            .any(|operation| operation.operation == testutil::GraphQlOperation::UpdatePr)
    )));

    let writes = events
        .iter()
        .filter(|event| {
            matches!(
                event,
                testutil::ExternalEvent::GitPush(_)
                    | testutil::ExternalEvent::GraphQl(testutil::GraphQlExchange::Mutation { .. })
            )
        })
        .collect::<Vec<_>>();
    let first_create = writes
        .iter()
        .position(|event| matches!(event, testutil::ExternalEvent::GraphQl(_)))
        .expect("create mutation events exist");
    assert!(first_create > 0, "tuple publication precedes create mutations");
    assert!(
        writes[..first_create]
            .iter()
            .all(|event| matches!(event, testutil::ExternalEvent::GitPush(_)))
    );
    assert_eq!(writes.len(), first_create + 2);
    assert!(writes[first_create..].iter().all(|event| matches!(
        event,
        testutil::ExternalEvent::GraphQl(testutil::GraphQlExchange::Mutation {
            operations,
        }) if operations.len() == MUTATION_BATCH_LEN
            && operations
                .iter()
                .all(|operation| operation.operation == testutil::GraphQlOperation::CreatePr)
    )));
    assert!(matches!(
        events.last(),
        Some(testutil::ExternalEvent::GraphQl(testutil::GraphQlExchange::Mutation {
            operations,
        })) if operations.len() == MUTATION_BATCH_LEN
    ));

    let push_invocations = ctx.recorded_git_invocations(testutil::GitOperation::Push);
    assert!(push_invocations.iter().flatten().all(|argument| !argument.ends_with("/pr")));
    assert_eq!(
        push_invocations.len(),
        events.iter().filter(|event| matches!(event, testutil::ExternalEvent::GitPush(_))).count(),
        "no uncompleted marker push may escape after the failed create batch"
    );
    ctx.assert_owned_base_tuples(&tuples);

    let pull_requests = ctx.github().pull_requests();
    assert_eq!(pull_requests.len(), MUTATION_BATCH_LEN);
    assert_eq!(
        pull_requests.iter().map(|pr| pr.head.as_str()).collect::<Vec<_>>(),
        ids[..MUTATION_BATCH_LEN],
        "effects from the completely acknowledged first batch must persist"
    );
    for (pull_request, tuple) in pull_requests.iter().zip(&tuples[..MUTATION_BATCH_LEN]) {
        assert_eq!(pull_request.head, tuple.id);
        assert_eq!(pull_request.head_oid, tuple.head_oid);
        assert_eq!(pull_request.base, format!("gherrit-bases/{}", tuple.id));
        assert_eq!(pull_request.base_oid, tuple.base_oid);
        let body = pull_request.body.as_deref().expect("created pull request has a body");
        assert!(
            !body.lines().any(|line| line.trim_start().starts_with('-') && line.contains('#')),
            "retained body must remain provisional and unnumbered"
        );
    }
}

#[test]
fn duplicate_observed_pull_request_identities_stop_before_git_publication() {
    #[derive(Clone, Copy)]
    enum Collision {
        Number,
        NodeId,
    }

    for has_missing_pull_request in [false, true] {
        for collision in [Collision::Number, Collision::NodeId] {
            let ctx = testutil::test_context!()
                .with_remote()
                .with_initial_commit()
                .with_mock_github()
                .with_git_interceptor()
                .build();
            ctx.checkout_managed_private("duplicate-observed-pr-identity");
            let first = ctx.commit_with_gherrit_id("Existing one");
            let first_oid = ctx.head_oid();
            let second = ctx.commit_with_gherrit_id("Existing two");
            let second_oid = ctx.head_oid();
            let main_oid = ctx.remote_ref_oid("refs/heads/main").unwrap();
            if has_missing_pull_request {
                ctx.commit_with_gherrit_id("Missing pull request");
            }

            for (id, oid) in [(&first, &first_oid), (&second, &second_oid)] {
                ctx.remote_git_cmd()
                    .args(["fetch", "--quiet"])
                    .arg(&ctx.repo_path)
                    .args([
                        format!("{oid}:refs/heads/{id}"),
                        format!("{oid}:refs/tags/gherrit/{id}/v1"),
                    ])
                    .assert()
                    .success();
            }
            // Fetching objects directly into the bare fixture can detach its
            // HEAD. Restore the symbolic default branch which production
            // observation requires the server to advertise.
            ctx.remote_git_cmd()
                .args(["symbolic-ref", "HEAD", "refs/heads/main"])
                .assert()
                .success();

            ctx.github().seed_pull_request(testutil::PullRequestSeed {
                number: 1,
                title: "Existing one".to_owned(),
                body: String::new(),
                head: first.clone(),
                head_oid: first_oid.clone(),
                base: "main".to_owned(),
                base_oid: main_oid,
                auto_merge: false,
                in_merge_queue: false,
            });
            ctx.github().seed_pull_request(testutil::PullRequestSeed {
                number: 2,
                title: "Existing two".to_owned(),
                body: String::new(),
                head: second.clone(),
                head_oid: second_oid,
                base: first,
                base_oid: first_oid,
                auto_merge: false,
                in_merge_queue: false,
            });

            let diagnostic = match collision {
                Collision::Number => {
                    ctx.github().set_pull_request_identity(&second, 1, "PR_2");
                    "duplicate open pull request number 1"
                }
                Collision::NodeId => {
                    ctx.github().set_pull_request_identity(&second, 2, "PR_1");
                    "duplicate open pull request node ID 'PR_1'"
                }
            };
            let refs_before = ctx.remote_refs("refs");
            let pull_requests_before = ctx.github().pull_requests();

            ctx.hook_cmd("pre-push")
                .assert()
                .failure()
                .stderr(predicate::str::contains(diagnostic));

            assert_eq!(ctx.remote_refs("refs"), refs_before);
            assert_eq!(ctx.github().pull_requests(), pull_requests_before);
            assert!(ctx.recorded_pushes().is_empty());
            assert_eq!(
                ctx.github().requests(),
                [vec![testutil::GraphQlOperation::Query]],
                "the complete open scan must reject duplicate identity evidence before terminal history or writes"
            );
        }
    }
}

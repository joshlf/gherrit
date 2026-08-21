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

fn assert_identity_failure_before_external_io(ctx: testutil::TestContext, diagnostic: &str) {
    ctx.hook_cmd("pre-push").assert().failure().stderr(predicate::str::contains(diagnostic));

    assert!(ctx.github().requests().is_empty());
    assert!(ctx.recorded_pushes().is_empty());
}

#[test]
fn test_empty_stack_id_fails_before_external_io() {
    let ctx = stack_with_raw_commit_message("Work\n\ngherrit-pr-id: ");

    assert_identity_failure_before_external_io(ctx, "missing gherrit-pr-id trailer");
}

#[test]
fn test_multiple_stack_ids_fail_before_external_io() {
    let ctx = stack_with_raw_commit_message("Work\n\ngherrit-pr-id: Gone\ngherrit-pr-id: Gtwo");

    assert_identity_failure_before_external_io(ctx, "multiple gherrit-pr-id trailers");
}

#[test]
fn test_body_lookalike_is_not_a_stack_id() {
    let ctx = stack_with_raw_commit_message(
        "Work\n\ngherrit-pr-id: Gexample\n\nThis final paragraph is not a trailer.",
    );

    assert_identity_failure_before_external_io(ctx, "missing gherrit-pr-id trailer");
}

#[test]
fn test_continued_stack_id_fails_before_external_io() {
    let ctx = stack_with_raw_commit_message("Work\n\ngherrit-pr-id: Gone\n continuation");

    assert_identity_failure_before_external_io(ctx, "invalid gherrit-pr-id trailer");
}

#[test]
fn test_empty_and_valid_stack_ids_are_multiple() {
    let ctx = stack_with_raw_commit_message("Work\n\ngherrit-pr-id: \ngherrit-pr-id: Gvalid");

    assert_identity_failure_before_external_io(ctx, "multiple gherrit-pr-id trailers");
}

#[test]
fn test_duplicate_stack_ids_fail_before_external_io() {
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
    ctx.checkout_managed_private("first-parent");
    ctx.commit_with_gherrit_id("Stack change");

    ctx.run_git(&["checkout", "main"]);
    ctx.commit("Advance the default branch");
    ctx.run_git(&["checkout", "first-parent"]);
    ctx.run_git(&["merge", "--no-ff", "main", "-m", "Merge the default branch"]);

    ctx.hook_cmd("pre-push")
        .assert()
        .failure()
        .stderr(predicate::str::contains("does not descend from 'main' on its first-parent path"));

    assert!(ctx.github().requests().is_empty());
    assert!(ctx.recorded_pushes().is_empty());
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
    ctx.inject_failure(testutil::FailureKind::UpdatePr);

    ctx.gherrit_cmd()
        .args(["hook", "pre-push"])
        .assert()
        .failure()
        .stderr(predicates::str::contains("Injected UpdatePr failure"));
    ctx.assert_failure_consumed();
    let requests = ctx.github().requests();
    assert_eq!(requests.last(), Some(&vec![testutil::GraphQlOperation::UpdatePr]));
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
    ctx.expect_git_failure(testutil::GitOperation::LsRemote);
    testutil::assert_failure_snapshot!(
        ctx,
        ctx.hook_cmd("pre-push"),
        "ls_remote_observation_failure"
    );

    ctx.assert_failure_consumed();
    assert_eq!(ctx.remote_refs("refs"), refs_before);
    assert!(ctx.recorded_pushes().is_empty());
    assert!(ctx.github().pull_requests().is_empty());
    assert_eq!(ctx.github().requests(), vec![vec![testutil::GraphQlOperation::Query]]);
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
    let requests = ctx.github().requests();
    assert_eq!(requests.last(), Some(&vec![testutil::GraphQlOperation::CreatePr]));
    assert!(ctx.github().pull_requests().is_empty());
    assert_eq!(ctx.recorded_pushes().iter().filter(|push| push.succeeded()).count(), 1);
}

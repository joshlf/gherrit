mod batch_update;

use predicates::prelude::*;

#[test]
fn test_pre_push_failure() {
    let ctx = testutil::test_context!().with_installed_hooks().with_mock_github().build();
    ctx.commit("Init");

    ctx.checkout_new("feature-fail");
    ctx.commit("Work to push");

    // Configure an invalid remote to trigger `git push` failure
    ctx.run_git(&["remote", "add", "broken-remote", "/path/to/nowhere"]);
    ctx.run_git(&["config", "gherrit.remote", "broken-remote"]);

    testutil::assert_failure_snapshot!(
        ctx,
        ctx.hook_cmd("pre-push"),
        "pre_push_failure_broken_remote"
    );
}

#[test]
fn test_pre_push_edit_failure() {
    let ctx = testutil::test_context!()
        .with_remote()
        .with_installed_hooks()
        .with_initial_commit()
        .with_mock_github()
        .build();

    // Setup: Create PR first
    ctx.checkout_new("feature-edit-fail");
    ctx.commit("Initial Work");
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
    ctx.inspect_mock_state(|state| {
        assert_eq!(
            state.graphql_requests.last(),
            Some(&vec![testutil::mock_server::GraphQlOperation::UpdatePr])
        );
        assert_eq!(state.prs.len(), 1);
        assert_eq!(state.prs[0].title.as_deref(), Some("Initial Work"));
    });

    let gherrit_id = ctx.gherrit_id("HEAD").unwrap();
    let remote_ref = format!("refs/heads/{gherrit_id}");
    assert_eq!(ctx.remote_ref_oid(&remote_ref).as_deref(), Some(ctx.head_oid().as_str()));
}

#[test]
fn test_pre_push_ls_remote_failure() {
    let ctx = testutil::test_context!()
        .with_remote()
        .with_installed_hooks()
        .with_initial_commit()
        .with_mock_github()
        .with_git_interceptor()
        .build();
    // Manage branch
    ctx.checkout_new("feature-ls-remote-fail");
    ctx.commit("Work");

    // Hook should succeed but warn about ls-remote failure
    ctx.gherrit_cmd()
        .args(["hook", "pre-push"])
        .env("MOCK_BIN_FAIL_CMD", "git:ls-remote")
        .assert()
        .success()
        .stderr(predicate::str::contains("Failed to fetch remote branch states"));
}

#[test]
fn test_pre_push_pr_list_failure() {
    let ctx = testutil::test_context!()
        .with_remote()
        .with_installed_hooks()
        .with_initial_commit()
        .with_mock_github()
        .with_git_interceptor()
        .build();
    ctx.checkout_new("feature-pr-list-fail");
    ctx.commit("Work");

    // Trigger hook
    ctx.inject_failure(testutil::FailureKind::GraphQl);

    ctx.gherrit_cmd()
        .args(["hook", "pre-push"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("Injected GraphQl failure"));
    ctx.assert_failure_consumed();
    ctx.inspect_mock_state(|state| {
        assert_eq!(
            state.graphql_requests,
            vec![vec![testutil::mock_server::GraphQlOperation::Query]]
        );
        assert!(state.prs.is_empty());
        assert!(state.pushes.is_empty());
    });
}

#[test]
fn test_pre_push_pr_create_failure() {
    let ctx = testutil::test_context!()
        .with_remote()
        .with_installed_hooks()
        .with_initial_commit()
        .with_mock_github()
        .with_git_interceptor()
        .build();
    ctx.checkout_new("feature-pr-create-fail");
    ctx.commit("Work");

    // Trigger hook
    ctx.inject_failure(testutil::FailureKind::CreatePr);

    ctx.gherrit_cmd()
        .args(["hook", "pre-push"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("Injected CreatePr failure"));
    ctx.assert_failure_consumed();
    ctx.inspect_mock_state(|state| {
        assert_eq!(
            state.graphql_requests.last(),
            Some(&vec![testutil::mock_server::GraphQlOperation::CreatePr])
        );
        assert!(state.prs.is_empty());
        assert_eq!(state.pushes.iter().filter(|push| push.succeeded()).count(), 1);
    });
}

use testutil::FailureKind;

fn context() -> testutil::TestContext {
    testutil::test_context!()
        .with_remote()
        .with_installed_hooks()
        .with_initial_commit()
        .with_mock_github()
        .with_git_interceptor()
        .build()
}

#[test]
fn staging_response_lost_after_application_is_reobserved_and_completed() {
    let ctx = context();
    ctx.checkout_new("ambiguous-staging");
    ctx.commit("A");
    let a = ctx.gherrit_id("HEAD").unwrap();
    ctx.commit("B");
    let b = ctx.gherrit_id("HEAD").unwrap();
    ctx.hook_cmd("pre-push").assert().success();

    ctx.run_git(&["reset", "--hard", "main"]);
    ctx.commit(&format!("B reordered\n\ngherrit-pr-id: {b}"));
    ctx.commit(&format!("A reordered\n\ngherrit-pr-id: {a}"));
    ctx.inject_failure(FailureKind::UpdatePrAfterApply);

    ctx.hook_cmd("pre-push").assert().success();
    ctx.assert_failure_consumed();
    ctx.inspect_mock_state(|state| {
        let a_pr = state.prs.iter().find(|pr| pr.head.ref_field == a).unwrap();
        let b_pr = state.prs.iter().find(|pr| pr.head.ref_field == b).unwrap();
        assert_eq!(b_pr.base.ref_field, "main");
        assert_eq!(a_pr.base.ref_field, b);
        assert!(state.prs.iter().all(|pr| pr.state == "OPEN"));
    });
}

#[test]
fn accepted_atomic_push_with_lost_status_is_reobserved_and_reconciled() {
    let ctx = context();
    ctx.checkout_new("ambiguous-push");
    ctx.commit("Published despite lost status");
    let id = ctx.gherrit_id("HEAD").unwrap();
    let desired = ctx.head_oid();

    ctx.hook_cmd("pre-push").env("MOCK_BIN_FAIL_AFTER_CMD", "git:push").assert().success();

    assert_eq!(ctx.remote_ref_oid(&format!("refs/heads/{id}")).as_deref(), Some(desired.as_str()));
    assert_eq!(
        ctx.remote_ref_oid(&format!("refs/tags/gherrit/{id}/v1")).as_deref(),
        Some(desired.as_str())
    );
    ctx.inspect_mock_state(|state| {
        assert_eq!(state.prs.len(), 1);
        assert_eq!(state.prs[0].head.ref_field, id);
        assert_eq!(state.prs[0].state, "OPEN");
        assert_eq!(state.pushes.iter().filter(|push| push.succeeded()).count(), 1);
    });
}

#[test]
fn create_response_lost_after_application_is_reobserved_and_completed() {
    let ctx = context();
    ctx.checkout_new("ambiguous-create");
    ctx.commit("Created despite lost response");
    let id = ctx.gherrit_id("HEAD").unwrap();
    ctx.inject_failure(FailureKind::CreatePrAfterApply);

    ctx.hook_cmd("pre-push").assert().success();
    ctx.assert_failure_consumed();
    ctx.inspect_mock_state(|state| {
        let matching = state.prs.iter().filter(|pr| pr.head.ref_field == id).collect::<Vec<_>>();
        assert_eq!(matching.len(), 1);
        assert!(matching[0].body.as_deref().unwrap().contains("gherrit-meta"));
    });
}

#[test]
fn partial_multi_batch_creation_reobserves_and_retries_only_missing_prs() {
    let ctx = context();
    ctx.limit_graphql_operations_per_request(1);
    ctx.checkout_new("partial-create");
    ctx.commit("A");
    let a = ctx.gherrit_id("HEAD").unwrap();
    ctx.commit("B");
    let b = ctx.gherrit_id("HEAD").unwrap();
    ctx.inject_failure(FailureKind::CreatePrAfterApply);

    ctx.hook_cmd("pre-push").assert().success();
    ctx.assert_failure_consumed();
    ctx.inspect_mock_state(|state| {
        assert_eq!(state.prs.iter().filter(|pr| pr.head.ref_field == a).count(), 1);
        assert_eq!(state.prs.iter().filter(|pr| pr.head.ref_field == b).count(), 1);
    });
}

#[test]
fn failed_first_full_body_update_leaves_a_retryable_provisional_pr() {
    let ctx = context();
    ctx.checkout_new("provisional-create");
    ctx.commit("Provisional body survives interruption");
    let id = ctx.gherrit_id("HEAD").unwrap();
    ctx.inject_failure(FailureKind::UpdatePr);

    ctx.hook_cmd("pre-push").assert().failure();
    ctx.assert_failure_consumed();
    ctx.inspect_mock_state(|state| {
        let pr = state.prs.iter().find(|pr| pr.head.ref_field == id).unwrap();
        let body = pr.body.as_deref().unwrap();
        assert!(body.contains("GHerrit is completing the initial projection"));
        assert!(body.trim_end().ends_with(" -->"));
    });

    ctx.hook_cmd("pre-push").assert().success();
    ctx.inspect_mock_state(|state| {
        let pr = state.prs.iter().find(|pr| pr.head.ref_field == id).unwrap();
        let body = pr.body.as_deref().unwrap();
        assert!(!body.contains("GHerrit is completing the initial projection"));
        assert!(body.contains("Stacked PRs enabled by"));
    });
}

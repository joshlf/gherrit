use predicates::prelude::*;

#[test]
fn installed_pre_push_projects_the_feature_commit() {
    let ctx = testutil::test_context!()
        .with_remote()
        .with_installed_hooks()
        .with_initial_commit()
        .with_mock_github()
        .build();

    ctx.checkout_new("feature-boundary");
    ctx.assert_config("branch.feature-boundary.gherritManaged", Some(testutil::MANAGED_PRIVATE));
    ctx.commit("Feature through installed hook");

    let id = ctx.gherrit_id("HEAD").unwrap();
    let oid = ctx.head_oid();
    let message =
        ctx.git_cmd().args(["show", "-s", "--format=%B", "HEAD"]).output().unwrap().stdout;
    let message = String::from_utf8(message).unwrap();
    assert_eq!(message.lines().filter(|line| line.starts_with("gherrit-pr-id: ")).count(), 1);

    ctx.git_cmd().args(["push", "origin", "feature-boundary"]).assert().success();

    assert_eq!(ctx.remote_ref_oid(&format!("refs/heads/{id}")).as_deref(), Some(oid.as_str()));
    let prs = ctx.github().pull_requests();
    assert_eq!(prs.len(), 1);
    assert_eq!(prs[0].head, id);
    testutil::assert_pr_snapshot!(ctx, "installed_pre_push_pr_state");
}

#[test]
fn installed_pre_push_blocks_the_enclosing_push() {
    let ctx = testutil::test_context!()
        .with_remote()
        .with_installed_hooks()
        .with_initial_commit()
        .build();

    ctx.checkout_new("blocked-boundary");
    ctx.commit("Work in progress");
    let id = ctx.gherrit_id("HEAD").unwrap();
    ctx.commit("fixup! Work in progress");

    ctx.git_cmd()
        .args(["push", "origin", "blocked-boundary"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("Stack contains pending fixup/squash/amend commits"));

    assert_eq!(ctx.remote_ref_oid("refs/heads/blocked-boundary"), None);
    assert_eq!(ctx.remote_ref_oid(&format!("refs/heads/{id}")), None);
}

#[test]
fn test_driver_never_falls_back_to_live_github() {
    let ctx = testutil::test_context!()
        .with_remote()
        .with_installed_hooks()
        .with_initial_commit()
        .build();

    ctx.checkout_new("missing-github-boundary");
    ctx.commit("Managed work");
    let id = ctx.gherrit_id("HEAD").unwrap();

    ctx.hook_cmd("pre-push").assert().failure().stderr(predicate::str::contains(
        "test driver cannot sync PRs without a configured GitHub endpoint",
    ));

    assert_eq!(ctx.remote_ref_oid(&format!("refs/heads/{id}")), None);
}

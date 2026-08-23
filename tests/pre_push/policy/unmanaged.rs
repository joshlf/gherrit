#[cfg(unix)]
use std::{fs::OpenOptions, io::Write as _};

use testutil::test_context;

fn assert_management_intent_failure_before_external_io(
    ctx: &testutil::TestContext,
    diagnostic: &str,
) {
    let refs_before = ctx.remote_refs("refs");

    ctx.hook_cmd("pre-push").assert().failure().stderr(predicates::str::contains(diagnostic));

    assert_eq!(ctx.remote_refs("refs"), refs_before);
    assert!(ctx.github().requests().is_empty());
    assert!(ctx.github().pull_requests().is_empty());
    assert!(ctx.recorded_pushes().is_empty());
    assert!(ctx.recorded_git_invocations(testutil::GitOperation::LsRemoteDefault).is_empty());
    assert!(ctx.recorded_git_invocations(testutil::GitOperation::LsRemoteLocal).is_empty());
    assert!(ctx.recorded_git_invocations(testutil::GitOperation::LsRemoteOther).is_empty());
}

#[test]
fn test_reproduce_unmanaged_sync() {
    // Prior to #217 (G1819a33e08a05c90e7f5e7a6198cd8ad7ca7e76e), we didn't
    // consistently distinguish between a missing `gherritManaged` configuration
    // and `gherritManaged = unmanaged`. We also spuriously synced
    // unmanaged branches. This is a regression test for the latter bug.

    let ctx = test_context!().build();

    // Condition 1: Explicit Unmanaged
    ctx.checkout_new("explicit-unmanaged");
    ctx.set_config("branch.explicit-unmanaged.gherritManaged", Some("false"));
    ctx.commit("Explicit Commit");

    testutil::assert_success_snapshot!(
        ctx,
        ctx.hook_cmd("pre-push"),
        "reproduce_unmanaged_sync_explicit"
    );

    // Condition 2: Implicit Unmanaged
    ctx.checkout_new("implicit-unmanaged");
    ctx.set_config("branch.implicit-unmanaged.gherritManaged", None);
    ctx.commit("Implicit Commit");

    testutil::assert_failure_snapshot!(
        ctx,
        ctx.hook_cmd("pre-push"),
        "reproduce_unmanaged_sync_implicit"
    );
}

#[test]
fn invalid_management_intent_fails_before_external_io() {
    let ctx = test_context!()
        .with_remote()
        .with_initial_commit()
        .with_mock_github()
        .with_git_interceptor()
        .build();
    let branch = "invalid-management-intent";
    ctx.checkout_managed_private(branch);
    ctx.commit_with_gherrit_id("Reject invalid management intent");
    ctx.set_config(&format!("branch.{branch}.gherritManaged"), Some("invalid"));

    assert_management_intent_failure_before_external_io(&ctx, "Invalid gherritManaged value");
}

#[cfg(unix)]
#[test]
fn non_utf8_management_intent_fails_before_external_io() {
    let ctx = test_context!()
        .with_remote()
        .with_initial_commit()
        .with_mock_github()
        .with_git_interceptor()
        .build();
    let branch = "non-utf8-management-intent";
    ctx.checkout_managed_private(branch);
    ctx.commit_with_gherrit_id("Reject unreadable management intent");
    ctx.set_config(&format!("branch.{branch}.gherritManaged"), None);
    OpenOptions::new()
        .append(true)
        .open(ctx.repo_path.join(".git/config"))
        .unwrap()
        .write_all(b"\n[branch \"non-utf8-management-intent\"]\n\tgherritManaged = \xff\n")
        .unwrap();

    assert_management_intent_failure_before_external_io(&ctx, "invalid utf-8 sequence");
}

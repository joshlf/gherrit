use testutil::test_context;

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

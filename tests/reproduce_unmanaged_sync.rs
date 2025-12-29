use testutil::test_context_minimal;

#[test]
fn test_reproduce_unmanaged_sync() {
    // Prior to #217 (G1819a33e08a05c90e7f5e7a6198cd8ad7ca7e76e), we didn't
    // consistently distinguish between a missing `gherritManaged` configuration
    // and `gherritManaged = unmanaged`. We also spuriously synced
    // unmanaged branches. This is a regression test for the latter bug.

    let ctx = test_context_minimal!().install_hooks(true).build();

    // Condition 1: Explicit Unmanaged
    ctx.checkout_new("explicit-unmanaged");
    ctx.set_config("branch.explicit-unmanaged.gherritManaged", Some("false"));
    ctx.commit("Explicit Commit");

    ctx.hook("pre-push").assert().success();

    ctx.maybe_inspect_mock_state(|state| {
        assert!(
            state.prs.is_empty(),
            "Explicit unmanaged branch should NOT sync PRs. Found: {:?}",
            state.prs
        );
    });

    // Condition 2: Implicit Unmanaged
    ctx.checkout_new("implicit-unmanaged");
    ctx.set_config("branch.implicit-unmanaged.gherritManaged", None);
    ctx.run_git(&["commit", "--allow-empty", "-m", "Implicit Commit", "--no-verify"]);

    let _ = ctx.hook("pre-push").output().unwrap();

    ctx.maybe_inspect_mock_state(|state| {
        assert!(
            state.prs.is_empty(),
            "Implicit unmanaged branch should NOT sync PRs. Found: {:?}",
            state.prs
        );
    });
}

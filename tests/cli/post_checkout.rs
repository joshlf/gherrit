#[test]
fn installed_hook_classifies_new_and_shared_branches() {
    let ctx = testutil::test_context!()
        .with_remote()
        .with_installed_hooks()
        .with_initial_commit()
        .build();

    testutil::assert_success_snapshot!(
        ctx,
        ctx.git_cmd().args(["checkout", "-b", "feature-stack"]),
        "post_checkout_new_stack",
    );
    ctx.assert_config("branch.feature-stack.gherritManaged", Some(testutil::MANAGED_PRIVATE));

    ctx.run_git(&["checkout", "main"]);
    ctx.run_git(&["update-ref", "refs/remotes/origin/collab-feature", "HEAD"]);
    testutil::assert_success_snapshot!(
        ctx,
        ctx.git_cmd().args([
            "checkout",
            "-b",
            "collab-feature",
            "--track",
            "origin/collab-feature",
        ]),
        "post_checkout_shared_branch",
    );
    ctx.assert_config("branch.collab-feature.gherritManaged", Some("false"));
}

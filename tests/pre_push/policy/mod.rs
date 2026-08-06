mod autosquash;
mod operational_state;
mod ownership;
mod pr_state;
mod remote_authority;
mod topology;
mod unmanaged;

#[test]
fn test_pre_push_ancestry_check() {
    let ctx = testutil::test_context!()
        .with_remote()
        .with_installed_hooks()
        .with_initial_commit()
        .build();

    // Create an orphan branch
    ctx.run_git(&["checkout", "--orphan", "lonely-branch"]);
    ctx.commit("Lonely Commit");

    // Trigger pre-push hook; it should fail because it can't find the merge
    // base with 'main'
    testutil::assert_failure_snapshot!(ctx, ctx.hook_cmd("pre-push"), "pre_push_ancestry_failure");
}

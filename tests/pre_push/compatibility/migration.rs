#[test]
fn test_mixed_stack_backward_compatibility() {
    let ctx =
        testutil::test_context!().with_remote().with_initial_commit().with_mock_github().build();

    ctx.checkout_managed_private("mixed-stack");

    // 1. Create a commit with a manual Hex ID (legacy format)
    let legacy_id = "G0000000000000000000000000000000000000001";
    ctx.commit_with_explicit_gherrit_id("Legacy Commit", legacy_id);

    // 2. Create a normal commit (will get a Base32 ID)
    ctx.commit_with_gherrit_id("Modern Commit");

    // 3. Trigger pre-push hook
    // We expect this to succeed and identify 2 commits to sync.
    // The "snapshot" will serve as verification of the output containing both IDs if we were looking at it,
    // but here we mainly care that it doesn't crash and processes both.
    testutil::assert_success_snapshot!(
        ctx,
        ctx.hook_cmd("pre-push"),
        "mixed_stack_backward_compatibility"
    );

    let legacy_ref = format!("refs/heads/{legacy_id}");
    assert!(ctx.remote_ref_oid(&legacy_ref).is_some(), "Expected legacy ID to be pushed");
}

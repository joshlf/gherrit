#[test]
fn test_mixed_stack_backward_compatibility() {
    let ctx = testutil::test_context!().build();

    ctx.manage(); // Ensure hooks are active
    ctx.checkout_new("mixed-stack"); // Checkout new to enable management on this branch

    // 1. Create a commit with a manual Hex ID (legacy format)
    let legacy_id = "G0000000000000000000000000000000000000001";
    let msg = format!("Legacy Commit\n\ngherrit-pr-id: {}", legacy_id);
    ctx.run_git(&["commit", "--allow-empty", "-m", &msg]);

    // 2. Create a normal commit (will get a Base32 ID)
    ctx.commit("Modern Commit");

    // 3. Trigger pre-push hook
    // We expect this to succeed and identify 2 commits to sync.
    // The "snapshot" will serve as verification of the output containing both IDs if we were looking at it,
    // but here we mainly care that it doesn't crash and processes both.
    testutil::assert_snapshot!(ctx, ctx.hook("pre-push"), "mixed_stack_backward_compatibility");

    let legacy_ref = format!("refs/heads/{legacy_id}");
    assert!(ctx.remote_ref_oid(&legacy_ref).is_some(), "Expected legacy ID to be pushed");
}

#[test]
fn test_base32_format_compliance() {
    let ctx = testutil::test_context!().build();
    ctx.manage();
    ctx.checkout_new("base32-format");
    ctx.commit("Base32 Commit");

    // Read the commit message
    let output = ctx.git().args(["log", "-1", "--format=%B"]).output().unwrap();
    let msg = String::from_utf8(output.stdout).unwrap();

    // extract ID
    let id_line = msg.lines().find(|l| l.starts_with("gherrit-pr-id: ")).expect("ID not found");
    let id = id_line.trim().strip_prefix("gherrit-pr-id: ").unwrap();

    // Must be uppercase G + [a-z2-7]
    assert!(id.starts_with('G'), "ID must start with G");
    let content = &id[1..];
    assert!(
        content.chars().all(|c| matches!(c, 'a'..='z' | '2'..='7')),
        "ID content must be lowercase letters or digits 2-7"
    );

    assert_eq!(id.len(), 33, "ID length should be 33 (1 prefix + 32 hash)");
}

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

#[test]
fn invalid_management_intent_fails_before_external_io() {
    let ctx = test_context!()
        .with_remote()
        .with_initial_commit()
        .with_mock_github()
        .with_git_interceptor()
        .build();
    ctx.checkout_new("invalid-management");
    let hostile = format!("\u{1b}[31m{}\r\nsecret", "x".repeat(10_000));
    ctx.set_config("branch.invalid-management.gherritManaged", Some(&hostile));
    ctx.commit_with_gherrit_id("Do not infer management intent");

    let assert = ctx.hook_cmd("pre-push").assert().failure();
    let stderr = String::from_utf8(assert.get_output().stderr.clone()).unwrap();
    assert!(stderr.contains("Invalid gherritManaged value"));
    assert!(stderr.len() < 512, "diagnostic length={}", stderr.len());
    assert!(!stderr.contains('\u{1b}'));
    assert!(!stderr.contains("secret"));

    assert!(ctx.github().requests().is_empty());
    assert!(ctx.recorded_pushes().is_empty());
}

#[test]
fn non_utf8_management_intent_fails_before_external_io() {
    use std::io::Write as _;

    let ctx = test_context!()
        .with_remote()
        .with_initial_commit()
        .with_mock_github()
        .with_git_interceptor()
        .build();
    ctx.checkout_new("non-utf8-management");
    ctx.commit_with_gherrit_id("Reject undecodable management intent");
    let mut config =
        std::fs::OpenOptions::new().append(true).open(ctx.repo_path.join(".git/config")).unwrap();
    config.write_all(b"\n[branch \"non-utf8-management\"]\n\tgherritManaged = \xff\n").unwrap();
    drop(config);

    ctx.hook_cmd("pre-push").assert().failure();

    assert!(ctx.github().requests().is_empty());
    assert!(ctx.recorded_pushes().is_empty());
}

#[cfg(unix)]
#[test]
fn non_utf8_branch_name_fails_before_lossy_management_lookup_or_external_io() {
    let ctx = test_context!()
        .with_remote()
        .with_initial_commit()
        .with_mock_github()
        .with_git_interceptor()
        .build();
    // A lossy conversion would consult this different, valid UTF-8 key and
    // could publish a link for the wrong branch identity.
    ctx.set_config("branch.non-utf8-�.gherritManaged", Some(testutil::MANAGED_PUBLIC));

    // macOS filesystems reject non-UTF-8 loose-ref filenames, but Git's ref
    // and packed-ref formats are byte-oriented. Write the valid raw Git state
    // directly so this boundary is exercised on every Unix filesystem.
    let raw_ref = b"refs/heads/non-utf8-\xff";
    let mut packed_refs = format!("{} ", ctx.head_oid()).into_bytes();
    packed_refs.extend_from_slice(raw_ref);
    packed_refs.push(b'\n');
    std::fs::write(ctx.repo_path.join(".git/packed-refs"), packed_refs).unwrap();
    let mut head = b"ref: ".to_vec();
    head.extend_from_slice(raw_ref);
    head.push(b'\n');
    std::fs::write(ctx.repo_path.join(".git/HEAD"), head).unwrap();

    ctx.hook_cmd("pre-push")
        .assert()
        .failure()
        .stderr(predicates::str::contains("branch names to be valid UTF-8"));

    assert!(ctx.github().requests().is_empty());
    assert!(ctx.recorded_pushes().is_empty());
}

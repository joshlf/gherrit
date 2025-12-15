#[test]
#[cfg(unix)]
fn test_commit_msg_trailer_failure() {
    use std::os::unix::fs::PermissionsExt;

    let ctx = testutil::test_context_minimal!()
        .install_hooks(true)
        .build();

    // Manage branch to enable hook
    ctx.gherrit().args(["manage"]).assert().success();

    let msg_file = ctx.repo_path.join("COMMIT_EDITMSG");
    std::fs::write(&msg_file, "feat: broken trailers").unwrap();

    // Make file read-only to force 'git interpret-trailers --in-place' to fail
    let mut perms = std::fs::metadata(&msg_file).unwrap().permissions();
    perms.set_mode(0o444);
    std::fs::set_permissions(&msg_file, perms).unwrap();

    // Hook should fail if it can't write trailer
    ctx.gherrit()
        .args(["hook", "commit-msg", msg_file.to_str().unwrap()])
        .assert()
        .failure();
}

#[test]
fn test_pre_push_edit_failure() {
    let ctx = testutil::test_context!().build();

    // Setup: Create PR first
    ctx.checkout_new("feature-edit-fail");
    ctx.commit("Initial Work");
    // Initial push creates PR
    ctx.gherrit().args(["hook", "pre-push"]).assert().success();

    // Amend commit to trigger update (edit)
    ctx.run_git(&[
        "commit",
        "--amend",
        "--allow-empty",
        "-m",
        "Initial Work (Updated)",
    ]);

    // Run hook with failure injection
    ctx.inject_failure("update_pr", 5);

    ctx.gherrit().args(["hook", "pre-push"]).assert().failure();
}

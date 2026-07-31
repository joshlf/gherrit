#[test]
#[cfg(unix)]
fn test_commit_msg_trailer_failure() {
    use std::os::unix::fs::PermissionsExt;

    let ctx = testutil::test_context!().build();

    // Manage branch to enable hook
    ctx.gherrit_cmd().args(["manage"]).assert().success();

    let msg_file = ctx.repo_path.join("COMMIT_EDITMSG");
    std::fs::write(&msg_file, "feat: broken trailers").unwrap();

    // Make file read-only to force 'git interpret-trailers --in-place' to fail
    let mut perms = std::fs::metadata(&msg_file).unwrap().permissions();
    perms.set_mode(0o444);
    std::fs::set_permissions(&msg_file, perms).unwrap();

    // Hook should fail if it can't write trailer
    ctx.gherrit_cmd()
        .args(["hook", "commit-msg", msg_file.to_str().unwrap()])
        .assert()
        .failure()
        .stderr(predicates::str::contains("is not writable by user"));
    assert_eq!(std::fs::read_to_string(&msg_file).unwrap(), "feat: broken trailers");
}

#[test]
fn test_pre_push_edit_failure() {
    let ctx = testutil::test_context!()
        .with_remote()
        .with_installed_hooks()
        .with_initial_commit()
        .with_mock_github()
        .build();

    // Setup: Create PR first
    ctx.checkout_new("feature-edit-fail");
    ctx.commit("Initial Work");
    // Initial push creates PR
    ctx.gherrit_cmd().args(["hook", "pre-push"]).assert().success();

    // Amend commit to trigger update (edit)
    ctx.amend_with_message("Initial Work (Updated)");

    // Run hook with failure injection
    ctx.inject_failure(testutil::FailureKind::UpdatePr);

    ctx.gherrit_cmd()
        .args(["hook", "pre-push"])
        .assert()
        .failure()
        .stderr(predicates::str::contains("Injected UpdatePr failure"));
    ctx.assert_failure_consumed();
    ctx.inspect_mock_state(|state| {
        assert_eq!(
            state.graphql_requests.last(),
            Some(&vec![testutil::mock_server::GraphQlOperation::UpdatePr])
        );
        assert_eq!(state.prs.len(), 1);
        assert_eq!(state.prs[0].title.as_deref(), Some("Initial Work"));
    });

    let gherrit_id = ctx.gherrit_id("HEAD").unwrap();
    let remote_ref = format!("refs/heads/{gherrit_id}");
    assert_eq!(ctx.remote_ref_oid(&remote_ref).as_deref(), Some(ctx.head_oid().as_str()));
}

#[test]
fn test_commit_msg_hook() {
    let ctx = testutil::test_context!().build();
    let msg_file = ctx.repo_path.join("COMMIT_EDITMSG");
    std::fs::write(&msg_file, "feat: my cool feature").unwrap();

    // Must manage the branch first so the hook runs
    testutil::assert_success_snapshot!(ctx, ctx.manage_cmd(), "commit_msg_hook_manage");

    // Run hook
    testutil::assert_success_snapshot!(
        ctx,
        ctx.gherrit_cmd().args(["hook", "commit-msg", msg_file.to_str().unwrap()]),
        "commit_msg_hook_run"
    );

    // Verify trailer was added
    let content = std::fs::read_to_string(msg_file).unwrap();
    assert!(content.contains("\ngherrit-pr-id: G"));
}

#[test]
fn test_commit_msg_edge_cases() {
    let ctx = testutil::test_context!().with_initial_commit().build();
    // Ensure we are managed so the hook is active
    testutil::assert_success_snapshot!(ctx, ctx.manage_cmd(), "commit_msg_edge_manage");

    // Scenario A: Squash Commit
    let squash_msg_file = ctx.repo_path.join("SQUASH_MSG");
    let squash_content = "squash! some other commit";
    std::fs::write(&squash_msg_file, squash_content).unwrap();

    testutil::assert_success_snapshot!(
        ctx,
        ctx.gherrit_cmd().args(["hook", "commit-msg", squash_msg_file.to_str().unwrap()]),
        "commit_msg_squash",
    );

    let content_after = std::fs::read_to_string(&squash_msg_file).unwrap();
    assert_eq!(content_after, squash_content, "Commit-msg hook should ignore squash commits");

    // Scenario B: Detached HEAD
    ctx.run_git(&["checkout", "--detach"]);
    let detached_msg_file = ctx.repo_path.join("DETACHED_MSG");
    let detached_content = "feat: detached work";
    std::fs::write(&detached_msg_file, detached_content).unwrap();

    testutil::assert_success_snapshot!(
        ctx,
        ctx.gherrit_cmd().args(["hook", "commit-msg", detached_msg_file.to_str().unwrap()]),
        "commit_msg_detached"
    );

    let content_after = std::fs::read_to_string(&detached_msg_file).unwrap();
    assert_eq!(content_after, detached_content, "Commit-msg hook should ignore detached HEAD");
}

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
fn test_commit_msg_git_var_failure() {
    #[cfg(unix)]
    {
        use predicates::prelude::*;

        let ctx = testutil::test_context!().with_git_interceptor().build();
        ctx.gherrit_cmd().args(["manage"]).assert().success();

        let msg_file = ctx.repo_path.join("COMMIT_EDITMSG");
        std::fs::write(&msg_file, "feat: broken git var").unwrap();

        ctx.gherrit_cmd()
            .args(["hook", "commit-msg", msg_file.to_str().unwrap()])
            .env("MOCK_BIN_FAIL_CMD", "git:var")
            .assert()
            .failure()
            .stderr(predicate::str::contains("Simulated failure for git var"));
    }
}

#[test]
fn test_commit_msg_trailers_failure() {
    #[cfg(unix)]
    {
        use predicates::prelude::*;

        let ctx = testutil::test_context!().with_git_interceptor().build();
        ctx.gherrit_cmd().args(["manage"]).assert().success();

        let msg_file = ctx.repo_path.join("COMMIT_EDITMSG");
        std::fs::write(&msg_file, "feat: broken trailers parse").unwrap();

        ctx.gherrit_cmd()
            .args(["hook", "commit-msg", msg_file.to_str().unwrap()])
            .env("MOCK_BIN_FAIL_CMD", "git:interpret-trailers")
            .assert()
            .failure()
            .stderr(predicate::str::contains("Simulated failure for git interpret-trailers"));
    }
}

#[test]
fn commit_msg_ignores_matching_prose_but_rejects_an_invalid_real_trailer() {
    let ctx = testutil::test_context!().with_initial_commit().build();
    ctx.manage_cmd().assert().success();

    let prose_file = ctx.repo_path.join("PROSE_MSG");
    std::fs::write(
        &prose_file,
        "feat: prose\n\nA sentence containing gherrit-pr-id: main.\n\nNot-A-Trailer paragraph.",
    )
    .unwrap();
    ctx.gherrit_cmd()
        .args(["hook", "commit-msg", prose_file.to_str().unwrap()])
        .assert()
        .success();
    let prose = std::fs::read_to_string(&prose_file).unwrap();
    assert!(prose.contains("gherrit-pr-id: G"));
    assert!(!prose.ends_with("gherrit-pr-id: main\n"));

    let invalid_file = ctx.repo_path.join("INVALID_MSG");
    std::fs::write(&invalid_file, "feat: invalid\n\ngherrit-pr-id: main\n").unwrap();
    ctx.gherrit_cmd()
        .args(["hook", "commit-msg", invalid_file.to_str().unwrap()])
        .assert()
        .failure()
        .stderr(predicates::str::contains("Invalid gherrit-pr-id `main`"));
}

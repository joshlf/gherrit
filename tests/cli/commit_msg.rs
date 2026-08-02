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
    let id = content
        .lines()
        .find_map(|line| line.strip_prefix("gherrit-pr-id: "))
        .expect("hook must add a GHerrit ID");
    assert_eq!(id.len(), 33, "ID must contain `G` and 32 base32 digits");
    assert!(
        id.starts_with('G')
            && id[1..].bytes().all(|byte| matches!(byte, b'a'..=b'z' | b'2'..=b'7')),
        "ID must use the lowercase base32 alphabet"
    );
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

        ctx.expect_git_failure(testutil::GitOperation::Var);
        ctx.gherrit_cmd()
            .args(["hook", "commit-msg", msg_file.to_str().unwrap()])
            .assert()
            .failure()
            .stderr(predicate::str::contains("Simulated failure for git var"));
        ctx.assert_failure_consumed();
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

        ctx.expect_git_failure(testutil::GitOperation::InterpretTrailers);
        ctx.gherrit_cmd()
            .args(["hook", "commit-msg", msg_file.to_str().unwrap()])
            .assert()
            .failure()
            .stderr(predicate::str::contains("Simulated failure for git interpret-trailers"));
        ctx.assert_failure_consumed();
    }
}

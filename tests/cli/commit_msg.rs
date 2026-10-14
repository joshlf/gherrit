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

#[test]
// Darwin filesystems reject invalid UTF-8 names before the hook can run.
#[cfg(all(unix, not(target_os = "macos")))]
fn test_commit_msg_non_utf8_filename() {
    use std::{ffi::OsString, os::unix::ffi::OsStringExt as _};
    let ctx = testutil::test_context!().build();
    ctx.gherrit_cmd().arg("manage").assert().success();
    let path = ctx.repo_path.join(OsString::from_vec(b"message-\xff".to_vec()));
    std::fs::write(&path, "subject\n").unwrap();
    ctx.gherrit_cmd().args(["hook", "commit-msg"]).arg(&path).assert().success();
    let bytes = std::fs::read(&path).unwrap();
    assert!(bytes.starts_with(b"subject\n"));
    assert!(bytes.windows(b"gherrit-pr-id: G".len()).any(|window| window == b"gherrit-pr-id: G"));
}

#[test]
#[cfg(unix)]
fn test_existing_identity_avoids_committer_lookup() {
    let ctx = testutil::test_context!().with_git_interceptor().build();
    ctx.gherrit_cmd().arg("manage").assert().success();
    let path = ctx.repo_path.join("identified-message");
    let message = "subject\n\ngherrit-pr-id: Legacy123\n";
    std::fs::write(&path, message).unwrap();
    ctx.expect_git_failure(testutil::GitOperation::Var);
    ctx.gherrit_cmd().args(["hook", "commit-msg"]).arg(&path).assert().success();
    assert_eq!(std::fs::read_to_string(&path).unwrap(), message);
    // Consume the still-pending failure explicitly: the hook did not query it.
    ctx.git_cmd().args(["var", "GIT_COMMITTER_IDENT"]).assert().failure();
    ctx.assert_failure_consumed();
}

#[test]
#[cfg(unix)]
fn test_installed_hook_preserves_git_empty_message_decisions() {
    use std::os::unix::fs::PermissionsExt as _;
    #[derive(serde::Deserialize, Debug)]
    struct Case {
        character: String,
        mode: String,
        allow: bool,
        message: String,
        success: bool,
        stored: String,
    }
    // Independent receipts captured from unhooked Git 2.54.0; no production
    // cleanup helper constructs the expected acceptance or stored message.
    let cases: Vec<Case> =
        serde_json::from_str(include_str!("fixtures/message-cleanup.json")).unwrap();
    let ctx = testutil::test_context!().with_installed_hooks().with_initial_commit().build();
    ctx.gherrit_cmd().arg("manage").assert().success();
    let editor = ctx.dir.path().join("message-editor");
    std::fs::write(&editor, "#!/bin/sh\ncp \"$GHERRIT_MESSAGE_FILE\" \"$1\"\n").unwrap();
    std::fs::set_permissions(&editor, std::fs::Permissions::from_mode(0o755)).unwrap();
    let input = ctx.dir.path().join("editor-input");
    for case in cases {
        ctx.run_git(&["config", "core.commentChar", &case.character]);
        std::fs::write(&input, &case.message).unwrap();
        let before = ctx.head_oid();
        let mut command = ctx.git_cmd();
        command
            .args(["commit", "--amend", "--allow-empty"])
            .env("GIT_EDITOR", &editor)
            .env("GHERRIT_MESSAGE_FILE", &input);
        if case.mode != "default" {
            command.arg(format!("--cleanup={}", case.mode));
        }
        if case.allow {
            command.arg("--allow-empty-message");
        }
        let output = command.output().unwrap();
        assert_eq!(output.status.success(), case.success, "{case:?}: {output:?}");
        if case.success {
            let stored = ctx
                .git_cmd()
                .args(["show", "-s", "--format=%B", "HEAD"])
                .assert()
                .success()
                .get_output()
                .stdout
                .clone();
            assert_eq!(stored, case.stored.as_bytes(), "{case:?}");
        } else {
            assert_eq!(ctx.head_oid(), before, "aborted amend moved HEAD: {case:?}");
        }
    }
}

#[test]
fn configured_trailer_separators_cannot_corrupt_new_identities() {
    let ctx = testutil::test_context!().build();
    ctx.gherrit_cmd().arg("manage").assert().success();
    ctx.run_git(&["config", "trailer.separators", "="]);
    let path = ctx.repo_path.join("MESSAGE");
    std::fs::write(&path, "subject\n").unwrap();
    ctx.gherrit_cmd().args(["hook", "commit-msg"]).arg(&path).assert().success();
    let message = std::fs::read_to_string(path).unwrap();
    let value = message.lines().find_map(|line| line.strip_prefix("gherrit-pr-id: ")).unwrap();
    assert!(value.bytes().all(|b| b.is_ascii_alphanumeric()));
    assert_eq!(message.lines().filter(|line| line.starts_with("gherrit-pr-id")).count(), 1);
}

#[test]
#[cfg(unix)]
fn interactive_message_comments_do_not_hide_the_inserted_identity() {
    use std::os::unix::fs::PermissionsExt as _;
    for (key, prefix) in
        [("core.commentChar", "#"), ("core.commentChar", ";"), ("core.commentString", "//")]
    {
        for mode in ["default", "strip", "whitespace", "verbatim", "scissors"] {
            let ctx =
                testutil::test_context!().with_installed_hooks().with_initial_commit().build();
            ctx.gherrit_cmd().arg("manage").assert().success();
            ctx.run_git(&["config", key, prefix]);
            let editor = ctx.dir.path().join("message-editor");
            // Retain Git's own status/comment suffix, exactly as a normal
            // interactive editor does, and prepend the user's subject.
            std::fs::write(&editor, "#!/bin/sh\nset -e\ncp \"$1\" \"$1.saved\"\nprintf 'Interactive subject\\n\\n' > \"$1\"\ncat \"$1.saved\" >> \"$1\"\n").unwrap();
            std::fs::set_permissions(&editor, std::fs::Permissions::from_mode(0o755)).unwrap();
            ctx.git_cmd()
                .args(["commit", "--allow-empty", &format!("--cleanup={mode}")])
                .env("GIT_EDITOR", &editor)
                .assert()
                .success();
            let stored =
                ctx.git_cmd().args(["show", "-s", "--format=%B", "HEAD"]).output().unwrap();
            let message = String::from_utf8(stored.stdout).unwrap();
            assert!(message.starts_with("Interactive subject\n"), "{key}/{mode}: {message}");
            assert_eq!(
                message.lines().filter(|line| line.starts_with("gherrit-pr-id: G")).count(),
                1,
                "{key}/{mode}: {message}"
            );
        }
    }
}

#[test]
fn object_store_environment_is_refused_without_editing_the_message() {
    let ctx = testutil::test_context!().with_initial_commit().build();
    ctx.gherrit_cmd().arg("manage").assert().success();
    let path = ctx.repo_path.join("MESSAGE");
    let message = b"subject\n";
    for variable in ["GIT_OBJECT_DIRECTORY", "GIT_ALTERNATE_OBJECT_DIRECTORIES"] {
        std::fs::write(&path, message).unwrap();
        ctx.gherrit_cmd()
            .args(["hook", "commit-msg"])
            .arg(&path)
            .env(variable, ctx.repo_path.join(".git/objects"))
            .assert()
            .failure()
            .stderr(predicates::str::contains(format!("{variable} is unsupported")));
        assert_eq!(std::fs::read(&path).unwrap(), message);
    }
}

#[test]
#[cfg(unix)]
fn automatic_comments_preserve_native_git_decisions_for_ambiguous_hook_inputs() {
    use std::os::unix::fs::PermissionsExt as _;
    for installed in [false, true] {
        for from_file in [false, true] {
            let builder = testutil::test_context!().with_initial_commit();
            let ctx =
                if installed { builder.with_installed_hooks().build() } else { builder.build() };
            if installed {
                ctx.manage_cmd().arg("--private").assert().success();
            }
            ctx.run_git(&["config", "core.commentChar", "auto"]);
            let prepare = ctx.repo_path.join(".git/hooks/prepare-commit-msg");
            std::fs::create_dir_all(prepare.parent().unwrap()).unwrap();
            std::fs::write(&prepare, "#!/bin/sh\nset -e\nprintf '# subject\\n' > \"$1\"\n")
                .unwrap();
            std::fs::set_permissions(&prepare, std::fs::Permissions::from_mode(0o755)).unwrap();
            let input = ctx.dir.path().join("message");
            std::fs::write(&input, "# subject\n").unwrap();
            let before = ctx.head_oid();
            let mut command = ctx.git_cmd();
            command
                .args(["commit", "--allow-empty", "--no-status", "--cleanup=strip"])
                .env("GIT_EDITOR", ":");
            if from_file {
                command.arg("--file").arg(&input);
            }
            let output = command.output().unwrap();
            assert_eq!(
                output.status.success(),
                from_file,
                "installed={installed}, file={from_file}: {output:?}"
            );
            if from_file {
                let message = ctx
                    .git_cmd()
                    .args(["show", "-s", "--format=%B", "HEAD"])
                    .output()
                    .unwrap()
                    .stdout;
                assert!(message.starts_with(b"# subject\n"));
                assert!(!String::from_utf8_lossy(&message).contains("gherrit-pr-id:"));
                if installed {
                    assert!(
                        String::from_utf8_lossy(&output.stderr)
                            .contains("automatic comment character")
                    );
                }
            } else {
                assert_eq!(ctx.head_oid(), before);
            }
        }
    }
}

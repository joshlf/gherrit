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
    for (separators, message) in [
        (":", "subject\n\ngherrit-pr-id: Legacy123\n"),
        (":=", "subject\n\ngherrit-pr-id=Legacy123\n"),
    ] {
        let ctx = testutil::test_context!().with_git_interceptor().build();
        ctx.gherrit_cmd().arg("manage").assert().success();
        let path = ctx.repo_path.join("identified-message");
        ctx.set_config("trailer.separators", Some(separators));
        std::fs::write(&path, message).unwrap();
        ctx.expect_git_failure(testutil::GitOperation::Var);
        ctx.gherrit_cmd().args(["hook", "commit-msg"]).arg(&path).assert().success();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), message);
        // Consume the failure explicitly: the hook did not query it.
        ctx.git_cmd().args(["var", "GIT_COMMITTER_IDENT"]).assert().failure();
        ctx.assert_failure_consumed();
    }
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
            if case.message.contains(" ------------------------ >8 ------------------------")
                && !case.stored.trim().is_empty()
            {
                let stored = String::from_utf8(stored).unwrap();
                let identities =
                    stored.lines().filter(|line| line.starts_with("gherrit-pr-id: ")).count();
                assert_eq!(identities, 1, "{case:?}: {stored}");
                let without_identity = stored
                    .lines()
                    .filter(|line| !line.starts_with("gherrit-pr-id: "))
                    .collect::<Vec<_>>()
                    .join("\n");
                assert_eq!(without_identity.trim_end(), case.stored.trim_end(), "{case:?}");
            } else {
                assert_eq!(stored, case.stored.as_bytes(), "{case:?}");
            }
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

#[test]
fn patch_dividers_do_not_hide_new_identities() {
    let ctx = testutil::test_context!().with_initial_commit().with_installed_hooks().build();
    ctx.manage_cmd().arg("--private").assert().success();
    let message =
        "Patch subject\n\nDescription\n\n---\n file | 1 +\n\ndiff --git a/file b/file\n+content\n";
    ctx.git_cmd()
        .args(["commit", "--allow-empty", "--cleanup=verbatim", "-m", message])
        .assert()
        .success();
    let stored = ctx.git_cmd().args(["show", "-s", "--format=%B", "HEAD"]).output().unwrap().stdout;
    assert!(stored.starts_with(message.as_bytes()));
    assert!(ctx.gherrit_id("HEAD").is_ok());
    let before = ctx.head_oid();
    ctx.git_cmd().args(["commit", "--amend", "--allow-empty", "--no-edit"]).assert().success();
    assert_eq!(ctx.gherrit_id("HEAD").unwrap(), ctx.gherrit_id(&before).unwrap());
}

#[test]
fn existing_identity_uses_the_same_pinned_trailer_separators() {
    let ctx = testutil::test_context!().with_initial_commit().with_installed_hooks().build();
    ctx.manage_cmd().arg("--private").assert().success();
    ctx.git_cmd().args(["commit", "--allow-empty", "-m", "Keep identity"]).assert().success();
    let id = ctx.gherrit_id("HEAD").unwrap();
    ctx.set_config("trailer.separators", Some("="));
    ctx.git_cmd().args(["commit", "--amend", "--allow-empty", "--no-edit"]).assert().success();
    assert_eq!(ctx.gherrit_id("HEAD").unwrap(), id);
}

#[test]
fn retained_comments_precede_the_stored_trailer_block() {
    for prefix in ["#", "!"] {
        for mode in ["verbatim", "strip"] {
            let ctx =
                testutil::test_context!().with_initial_commit().with_installed_hooks().build();
            ctx.manage_cmd().arg("--private").assert().success();
            ctx.set_config("core.commentChar", Some(prefix));
            let message = format!(
                "Retain comments\n\nBody\n\nSigned-off-by: Example <example@invalid>\n{prefix} retained note\n"
            );
            ctx.git_cmd()
                .args(["commit", "--allow-empty", &format!("--cleanup={mode}"), "-m", &message])
                .assert()
                .success();
            let stored =
                ctx.git_cmd().args(["show", "-s", "--format=%B", "HEAD"]).output().unwrap().stdout;
            let footer =
                std::str::from_utf8(&stored).unwrap().trim_end().rsplit("\n\n").next().unwrap();
            assert!(footer.contains("gherrit-pr-id: "));
            assert!(footer.contains("Signed-off-by: Example <example@invalid>"));
            assert!(!footer.contains("retained note"));
            assert_eq!(
                String::from_utf8_lossy(&stored).contains("retained note"),
                mode == "verbatim"
            );
            let id = ctx.gherrit_id("HEAD").unwrap();
            ctx.git_cmd()
                .args(["commit", "--amend", "--allow-empty", "--no-edit"])
                .assert()
                .success();
            assert_eq!(ctx.gherrit_id("HEAD").unwrap(), id);
        }
    }
}

#[test]
fn identity_key_casing_retains_an_existing_value() {
    let ctx = testutil::test_context!().with_initial_commit().with_installed_hooks().build();
    ctx.manage_cmd().arg("--private").assert().success();
    ctx.git_cmd()
        .args([
            "commit",
            "--allow-empty",
            "--cleanup=verbatim",
            "-m",
            "Subject\n\nGherrit-pr-id: Gold",
        ])
        .assert()
        .success();
    let message =
        ctx.git_cmd().args(["show", "-s", "--format=%B", "HEAD"]).output().unwrap().stdout;
    assert!(String::from_utf8(message).unwrap().contains("Gherrit-pr-id: Gold"));
    ctx.git_cmd().args(["commit", "--amend", "--allow-empty", "--no-edit"]).assert().success();
    let message =
        ctx.git_cmd().args(["show", "-s", "--format=%B", "HEAD"]).output().unwrap().stdout;
    assert_eq!(String::from_utf8(message).unwrap().matches("Gherrit-pr-id: Gold").count(), 1);
}

#[test]
fn a_verbatim_scissors_suffix_does_not_reclassify_the_original_squash_input() {
    let ctx = testutil::test_context!().with_initial_commit().with_installed_hooks().build();
    ctx.manage_cmd().arg("--private").assert().success();
    let message = "# ------------------------ >8 ------------------------\nsquash! retained text\n";
    ctx.git_cmd()
        .args(["commit", "--allow-empty", "--cleanup=verbatim", "-m", message])
        .assert()
        .success();
    assert!(ctx.gherrit_id("HEAD").is_ok());
    let title = ctx.git_cmd().args(["show", "-s", "--format=%s", "HEAD"]).output().unwrap().stdout;
    assert!(title.starts_with(b"# ------------------------"));
}

#[test]
fn compatibility_recognition_does_not_reorder_interleaved_literal_body_text() {
    let ctx = testutil::test_context!().with_initial_commit().build();
    ctx.manage_cmd().arg("--private").assert().success();
    ctx.set_config("core.commentChar", Some("!"));
    let message = "Subject\n\nSigned-off-by: Person\n# Literal body text\ngherrit-pr-id: Gone\n! retained note\n";
    let path = ctx.repo_path.join("MESSAGE");
    std::fs::write(&path, message).unwrap();
    ctx.gherrit_cmd().args(["hook", "commit-msg"]).arg(&path).assert().success();
    assert_eq!(std::fs::read_to_string(path).unwrap(), message);
}

#[test]
fn configured_legacy_identity_separators_survive_native_amend() {
    for (separators, separator) in [(":=", "="), ("=:%", "%"), ("%=$", "$")] {
        let ctx = testutil::test_context!().with_initial_commit().with_installed_hooks().build();
        ctx.checkout_managed_private("legacy-separator");
        ctx.set_config("trailer.separators", Some(separators));
        let message = format!("Preserve owner\n\nGhErRiT-pR-Id{separator}Gone\n");
        ctx.git_cmd()
            .args(["commit", "--allow-empty", "--no-verify", "--cleanup=verbatim", "-m", &message])
            .assert()
            .success();
        let before = ctx
            .git_cmd()
            .args(["show", "-s", "--format=%B", "HEAD"])
            .assert()
            .success()
            .get_output()
            .stdout
            .clone();
        ctx.git_cmd()
            .args(["commit", "--amend", "--allow-empty", "--no-edit", "--cleanup=verbatim"])
            .assert()
            .success();
        ctx.git_cmd().args(["show", "-s", "--format=%B", "HEAD"]).assert().success().stdout(before);
    }
}

#[test]
fn native_amend_ignores_the_config_builtin_file_override() {
    let ctx = testutil::test_context!().with_initial_commit().with_installed_hooks().build();
    ctx.checkout_managed_private("config-file-override");
    ctx.set_config("trailer.separators", Some(":="));
    let message = "Preserve owner\n\nGhErRiT-pR-Id=Gone\n";
    ctx.git_cmd()
        .args(["commit", "--allow-empty", "--no-verify", "--cleanup=verbatim", "-m", message])
        .assert()
        .success();
    let redirect = ctx.repo_path.join("redirect-config");
    std::fs::write(&redirect, "[trailer]\nseparators = :\n").unwrap();
    ctx.git_cmd()
        .args(["commit", "--amend", "--allow-empty", "--no-edit", "--cleanup=verbatim"])
        .env("GIT_CONFIG", &redirect)
        .assert()
        .success();
    ctx.git_cmd()
        .args(["show", "-s", "--format=%B", "HEAD"])
        .assert()
        .success()
        .stdout(format!("{message}\n"));
}

#[test]
fn native_commit_accepts_a_message_larger_than_the_local_command_cap() {
    for trailer in [false, true] {
        let ctx = testutil::test_context!().with_initial_commit().with_installed_hooks().build();
        ctx.checkout_managed_private("large-message");
        let path = ctx.repo_path.join("large-message.txt");
        let prefix = if trailer { "Unrelated: " } else { "" };
        std::fs::write(
            &path,
            format!("Large message\n\n{prefix}{}\n", "x".repeat(5 * 1024 * 1024)),
        )
        .unwrap();
        ctx.git_cmd().args(["commit", "--allow-empty", "-F"]).arg(&path).assert().success();
        ctx.git_cmd()
            .args(["log", "-1", "--format=%s"])
            .assert()
            .success()
            .stdout("Large message\n");
        ctx.git_cmd()
            .args(["log", "-1", "--format=%(trailers:key=gherrit-pr-id,valueonly)"])
            .assert()
            .success()
            .stdout(predicates::str::contains("G"));
    }
}

fn assert_installed_hooks(directory: &std::path::Path) {
    // Keep the expected shim independent of the production template.
    for hook in ["pre-push", "commit-msg", "post-checkout"] {
        let expected = format!(
            "#!/bin/sh\n\
             # gherrit-installer: managed\n\
             # This hook is managed by GHerrit.\n\
             # Any manual changes to this file may be overwritten by 'gherrit install'.\n\n\
             gherrit hook {hook} \"$@\"\n"
        );
        let path = directory.join(hook);
        assert_eq!(std::fs::read(&path).unwrap(), expected.as_bytes(), "{hook}");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            assert_eq!(std::fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o755);
        }
    }
}

#[test]
fn test_install_command_edge_cases() {
    let ctx = testutil::test_context!().build();

    let hooks_dir = ctx.repo_path.join(".git/hooks");
    std::fs::create_dir_all(&hooks_dir).unwrap();
    let pre_push = hooks_dir.join("pre-push");

    // Scenario A: Conflict
    std::fs::write(&pre_push, "foo").unwrap();

    testutil::assert_failure_snapshot!(
        ctx,
        ctx.gherrit_cmd().args(["install"]),
        "install_edge_cases_conflict"
    );

    assert_eq!(std::fs::read_to_string(&pre_push).unwrap(), "foo");

    // Scenario B: Force Overwrite
    testutil::assert_success_snapshot!(
        ctx,
        ctx.gherrit_cmd().args(["install", "--force"]),
        "install_edge_cases_force"
    );

    let content = std::fs::read_to_string(&pre_push).unwrap();
    assert_installed_hooks(&hooks_dir);

    // Scenario C: Idempotency (Safe to run again)
    testutil::assert_success_snapshot!(
        ctx,
        ctx.gherrit_cmd().args(["install"]),
        "install_edge_cases_idempotent"
    );

    // Scenario D: Safe Update (Modify but keep sentinel)
    let modified = content + "\n# Some custom comment";
    std::fs::write(&pre_push, modified).unwrap();

    testutil::assert_success_snapshot!(
        ctx,
        ctx.gherrit_cmd().args(["install"]),
        "install_edge_cases_upgrade"
    );

    // An upgrade restores the complete managed shim for every hook.
    assert_installed_hooks(&hooks_dir);
}

#[test]
fn test_install_configuration_and_security() {
    let ctx = testutil::test_context!().build();

    // Scenario A: Automatic Directory Creation (Default Path)
    // -------------------------------------------------------
    // Ensure .git/hooks does not exist (git init might create it depending on version/templates)
    let default_hooks = ctx.repo_path.join(".git/hooks");
    if default_hooks.exists() {
        std::fs::remove_dir_all(&default_hooks).unwrap();
    }

    testutil::assert_success_snapshot!(
        ctx,
        ctx.gherrit_cmd().args(["install"]),
        "install_security_default"
    );
    assert_installed_hooks(&default_hooks);

    // Scenario B: Custom core.hooksPath (Internal)
    // --------------------------------------------
    let custom_internal = ctx.repo_path.join(".githooks");
    ctx.run_git(&["config", "core.hooksPath", ".githooks"]);

    testutil::assert_success_snapshot!(
        ctx,
        ctx.gherrit_cmd().args(["install"]),
        "install_security_custom_internal"
    );
    assert_installed_hooks(&custom_internal);

    // Scenario C: Custom core.hooksPath (External/Global) - Security Block
    // --------------------------------------------------------------------
    let external_dir = tempfile::TempDir::new().unwrap();
    let ext_path = external_dir.path().to_str().unwrap();

    // We must use absolute path for git config to ensure gherrit sees it as external
    ctx.run_git(&["config", "core.hooksPath", ext_path]);

    testutil::assert_failure_snapshot!(
        ctx,
        ctx.gherrit_cmd().args(["install"]), // Should fail
        "install_security_custom_external_block",
        &[(ext_path, "[EXTERNAL_HOOKS_PATH]")]
    );

    assert!(
        !external_dir.path().join("pre-push").exists(),
        "Should NOT install to external path without flag"
    );

    // Scenario D: Custom core.hooksPath (External) - Allow Global
    // -----------------------------------------------------------
    testutil::assert_success_snapshot!(
        ctx,
        ctx.gherrit_cmd().args(["install", "--allow-global"]),
        "install_security_custom_external_allow",
        &[(ext_path, "[EXTERNAL_HOOKS_PATH]")],
    );

    assert_installed_hooks(external_dir.path());
}

#[test]
#[cfg(unix)]
fn test_install_read_only_fs() {
    use std::os::unix::fs::PermissionsExt as _;

    // Skip if running as root, as root ignores permissions. This can arise in
    // practice when developing inside a container.
    if unsafe { libc::geteuid() } == 0 {
        return;
    }

    let ctx = testutil::test_context!().build();
    let hooks_dir = ctx.repo_path.join(".git/hooks");
    std::fs::create_dir_all(&hooks_dir).unwrap();

    // Mark hooks directory read-only
    let mut perms = std::fs::metadata(&hooks_dir).unwrap().permissions();
    perms.set_mode(0o555); // Read/Execute only (no write)
    std::fs::set_permissions(&hooks_dir, perms).unwrap();

    // Attempt installation, verifying failure due to permission denied
    testutil::assert_failure_snapshot!(
        ctx,
        ctx.gherrit_cmd().args(["install"]),
        "install_read_only_fs"
    );

    // Cleanup: Restore permissions so TempDir cleanup doesn't panic
    let mut perms = std::fs::metadata(&hooks_dir).unwrap().permissions();
    perms.set_mode(0o755);
    std::fs::set_permissions(&hooks_dir, perms).unwrap();
}

#[test]
fn test_install_linked_worktree_uses_git_hooks_path() {
    let ctx = testutil::test_context!().with_initial_commit().build();
    // The shared fixture sets an explicit .git/hooks path for ordinary repos.
    ctx.run_git(&["config", "--unset", "core.hooksPath"]);
    let linked = ctx.repo_path.parent().unwrap().join("linked");
    ctx.git_cmd().args(["worktree", "add", "-b", "linked"]).arg(&linked).assert().success();
    let output = ctx
        .git_cmd()
        .current_dir(&linked)
        .args(["rev-parse", "--path-format=absolute", "--git-path", "hooks"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let effective = std::path::PathBuf::from(std::str::from_utf8(&output).unwrap().trim_end());
    ctx.gherrit_cmd().current_dir(&linked).arg("install").assert().success();
    assert_installed_hooks(&effective);
    assert_eq!(effective, ctx.repo_path.join(".git/hooks").canonicalize().unwrap());
    assert!(!ctx.repo_path.join(".git/worktrees/linked/hooks").exists());

    ctx.run_git(&["config", "core.hooksPath", ".githooks"]);
    ctx.gherrit_cmd().current_dir(&linked).arg("install").assert().success();
    assert_installed_hooks(&linked.join(".githooks"));
    assert!(!ctx.repo_path.join(".githooks").exists());
}

#[test]
fn test_install_bare_repository() {
    let ctx = testutil::test_context!().with_initial_commit().build();
    let bare = ctx.repo_path.parent().unwrap().join("bare");
    ctx.git_cmd().args(["clone", "--bare"]).arg(&ctx.repo_path).arg(&bare).assert().success();
    ctx.gherrit_cmd().current_dir(&bare).arg("install").assert().success();
    assert_installed_hooks(&bare.join("hooks"));
    ctx.git_cmd()
        .current_dir(&bare)
        .args(["config", "core.hooksPath", "custom-hooks"])
        .assert()
        .success();
    ctx.gherrit_cmd().current_dir(&bare).arg("install").assert().success();
    assert_installed_hooks(&bare.join("custom-hooks"));
}

#[test]
fn test_install_binary_hooks_and_complete_conflict_inspection() {
    let ctx = testutil::test_context!().build();
    let hooks = ctx.repo_path.join(".git/hooks");
    std::fs::create_dir_all(&hooks).unwrap();
    let managed = b"# gherrit-installer: managed\noriginal managed bytes";
    let binary = b"\xff\x00unmanaged executable";
    std::fs::write(hooks.join("pre-push"), managed).unwrap();
    // A conflict at the last destination must prevent every replacement.
    std::fs::write(hooks.join("post-checkout"), binary).unwrap();
    ctx.gherrit_cmd()
        .arg("install")
        .assert()
        .failure()
        .stderr(predicates::str::contains("Refusing to overwrite unmanaged hooks"))
        .stderr(predicates::str::contains("Use --force"));
    assert_eq!(std::fs::read(hooks.join("pre-push")).unwrap(), managed);
    assert_eq!(std::fs::read(hooks.join("post-checkout")).unwrap(), binary);
    assert!(!hooks.join("commit-msg").exists());

    ctx.gherrit_cmd().args(["install", "--force"]).assert().success();
    assert_installed_hooks(&hooks);
    let binary_managed = b"\xff# gherrit-installer: managed\x00";
    std::fs::write(hooks.join("pre-push"), binary_managed).unwrap();
    ctx.gherrit_cmd().arg("install").assert().success();
    assert_installed_hooks(&hooks);
}

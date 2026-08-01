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
    assert!(content.contains("# gherrit-installer: managed"));

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

    // Content should be reset to standard shim (losing custom comment, which is expected behavior for managed hooks)
    let reset_content = std::fs::read_to_string(&pre_push).unwrap();
    assert!(reset_content.contains("# gherrit-installer: managed"));
    assert!(!reset_content.contains("# Some custom comment"));
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
    assert!(default_hooks.join("pre-push").exists(), "Should create directory and install hook");

    // Scenario B: Custom core.hooksPath (Internal)
    // --------------------------------------------
    let custom_internal = ctx.repo_path.join(".githooks");
    ctx.run_git(&["config", "core.hooksPath", ".githooks"]);

    testutil::assert_success_snapshot!(
        ctx,
        ctx.gherrit_cmd().args(["install"]),
        "install_security_custom_internal"
    );
    assert!(custom_internal.join("pre-push").exists(), "Should respect core.hooksPath within repo");

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

    assert!(
        external_dir.path().join("pre-push").exists(),
        "Should install to external path with --allow-global"
    );
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

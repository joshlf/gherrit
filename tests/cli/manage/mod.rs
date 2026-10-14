#[test]
fn test_branch_management() {
    let ctx = testutil::test_context!().build();

    // Create a branch to manage
    ctx.checkout_new("feature-A");

    // Scenario A: Custom Push Remote Preservation
    ctx.run_git(&["config", "branch.feature-A.pushRemote", "origin"]);

    // Attempt manage - should fail (drift)
    testutil::assert_failure_snapshot!(ctx, ctx.manage_cmd(), "branch_management_drift_warning"); // Logs warning, no change
    // Assert still unmanaged (missing key)
    ctx.assert_config("branch.feature-A.gherritManaged", None);

    testutil::assert_success_snapshot!(
        ctx,
        ctx.gherrit_cmd().args(["manage", "--force"]),
        "branch_management_force_manage",
    );

    // Assert managed
    ctx.assert_config("branch.feature-A.gherritManaged", Some(testutil::MANAGED_PRIVATE));

    // Assert pushRemote updated to loopback (Private default)
    ctx.assert_config("branch.feature-A.pushRemote", Some("."));

    // Assert other keys set
    ctx.assert_config("branch.feature-A.remote", Some("."));
    ctx.assert_config("branch.feature-A.merge", Some("refs/heads/feature-A"));

    // Scenario B: Unmanage Cleanup
    testutil::assert_success_snapshot!(ctx, ctx.unmanage_cmd(), "branch_management_unmanage");

    // Assert unmanaged (key exists but is false)
    ctx.assert_config("branch.feature-A.gherritManaged", Some("false"));

    // Assert cleanup (keys should be unset)
    ctx.assert_config("branch.feature-A.remote", None);
    ctx.assert_config("branch.feature-A.merge", None);

    // Assert pushRemote unset
    ctx.assert_config("branch.feature-A.pushRemote", None);
}

#[test]
fn test_rebase_detection() {
    let ctx = testutil::test_context!().with_initial_commit().build();

    ctx.checkout_new("feature-rebase");
    ctx.commit("Feature Work");

    // Detach HEAD to simulate rebase state
    ctx.run_git(&["checkout", "--detach"]);

    // Create rebase-merge state manually
    let rebase_dir = ctx.repo_path.join(".git/rebase-merge");
    std::fs::create_dir_all(&rebase_dir).unwrap();
    std::fs::write(rebase_dir.join("head-name"), "refs/heads/feature-rebase").unwrap();

    // Run manage - should succeed by detecting 'feature-rebase'
    testutil::assert_success_snapshot!(ctx, ctx.manage_cmd(), "rebase_detection_manage");

    // Verify config was applied to 'feature-rebase'
    ctx.assert_config("branch.feature-rebase.gherritManaged", Some(testutil::MANAGED_PRIVATE));
}

#[test]
fn test_manage_detached_head() {
    let ctx = testutil::test_context!().with_initial_commit().build();

    // Enter detached HEAD state
    ctx.run_git(&["checkout", "--detach"]);

    let test = |args: &[_], name| {
        testutil::assert_failure_snapshot!(ctx, ctx.gherrit_cmd().args(args), name);
    };

    test(&["manage"], "manage_detached_head");
    test(&["manage", "--public"], "manage_public_detached_head");
    test(&["manage", "--private"], "manage_private_detached_head");
    test(&["unmanage"], "unmanage_detached_head");
}

#[test]
fn test_unmanage_preserves_drift_unless_forced() {
    let ctx = testutil::test_context!().with_initial_commit().build();
    ctx.checkout_new("feature-cleanup");

    // Configure a private branch whose push remote has drifted. Without
    // `--force`, GHerrit must preserve this state.
    ctx.run_git(&["config", "branch.feature-cleanup.gherritManaged", testutil::MANAGED_PRIVATE]);
    ctx.run_git(&["config", "branch.feature-cleanup.pushRemote", "drifted-remote"]);
    ctx.run_git(&["config", "branch.feature-cleanup.remote", "."]);
    ctx.run_git(&["config", "branch.feature-cleanup.merge", "refs/heads/feature-cleanup"]);

    testutil::assert_failure_snapshot!(ctx, ctx.unmanage_cmd(), "unmanage_preserve_drift",);

    ctx.assert_config("branch.feature-cleanup.gherritManaged", Some(testutil::MANAGED_PRIVATE));
    ctx.assert_config("branch.feature-cleanup.pushRemote", Some("drifted-remote"));
    ctx.assert_config("branch.feature-cleanup.remote", Some("."));
    ctx.assert_config("branch.feature-cleanup.merge", Some("refs/heads/feature-cleanup"));

    let mut command = ctx.unmanage_cmd();
    command.arg("--force");
    testutil::assert_success_snapshot!(ctx, command, "unmanage_force_cleanup");

    ctx.assert_config("branch.feature-cleanup.pushRemote", None);
    ctx.assert_config("branch.feature-cleanup.remote", None);
    ctx.assert_config("branch.feature-cleanup.merge", None);
    ctx.assert_config("branch.feature-cleanup.gherritManaged", Some("false"));
}

#[test]
fn test_manage_drift_detection() {
    let ctx = testutil::test_context!().build();
    ctx.checkout_new("drift-feature");

    // 1. Initialize managed private branch
    testutil::assert_success_snapshot!(
        ctx,
        ctx.gherrit_cmd().args(["manage", "--private"]),
        "manage_drift_init"
    );

    // 2. Manually sabotage
    ctx.run_git(&["config", "branch.drift-feature.pushRemote", "origin"]);

    // 3. Attempt Switch to Public (without force)
    // The command should exit with 0 but log a warning and NOT apply changes.
    testutil::assert_failure_snapshot!(
        ctx,
        ctx.gherrit_cmd().args(["manage", "--public"]),
        "manage_drift_attempt_switch"
    );

    // Assert state matches OLD state (Private)
    ctx.assert_config("branch.drift-feature.gherritManaged", Some(testutil::MANAGED_PRIVATE));

    // 4. Force Switch
    testutil::assert_success_snapshot!(
        ctx,
        ctx.gherrit_cmd().args(["manage", "--public", "--force"]),
        "manage_drift_force_switch",
    );

    // Assert Success
    ctx.assert_config("branch.drift-feature.gherritManaged", Some(testutil::MANAGED_PUBLIC));

    // Check pushRemote is now origin
    ctx.assert_config("branch.drift-feature.pushRemote", Some("origin"));
}

#[test]
fn test_manage_toggle_visibility() {
    let ctx = testutil::test_context!().build();
    ctx.checkout_new("visibility-feature");

    // 1. Private
    testutil::assert_success_snapshot!(
        ctx,
        ctx.gherrit_cmd().args(["manage", "--private"]),
        "manage_toggle_init_private"
    );
    ctx.assert_config("branch.visibility-feature.pushRemote", Some("."));

    // 2. Public
    testutil::assert_success_snapshot!(
        ctx,
        ctx.gherrit_cmd().args(["manage", "--public"]),
        "manage_toggle_switch_public"
    );
    ctx.assert_config("branch.visibility-feature.pushRemote", Some("origin"));

    // 3. Private again
    testutil::assert_success_snapshot!(
        ctx,
        ctx.gherrit_cmd().args(["manage", "--private"]),
        "manage_toggle_switch_private",
    );
    ctx.assert_config("branch.visibility-feature.pushRemote", Some("."));
}

fn configure_branch(
    ctx: &testutil::TestContext,
    branch: &str,
    state: Option<&str>,
    push_remote: Option<&str>,
    remote: Option<&str>,
    merge: Option<&str>,
) {
    let key = |suffix: &str| format!("branch.{branch}.{suffix}");
    ctx.set_config(&key("gherritManaged"), state);
    ctx.set_config(&key("pushRemote"), push_remote);
    ctx.set_config(&key("remote"), remote);
    ctx.set_config(&key("merge"), merge);
}

fn assert_branch_config(
    ctx: &testutil::TestContext,
    branch: &str,
    state: Option<&str>,
    push_remote: Option<&str>,
    remote: Option<&str>,
    merge: Option<&str>,
) {
    let key = |suffix: &str| format!("branch.{branch}.{suffix}");
    ctx.assert_config(&key("gherritManaged"), state);
    ctx.assert_config(&key("pushRemote"), push_remote);
    ctx.assert_config(&key("remote"), remote);
    ctx.assert_config(&key("merge"), merge);
}

#[test]
fn private_management_does_not_read_invalid_remote_configuration() {
    ["empty", "repeated"].into_iter().for_each(|case| {
        let ctx = testutil::test_context!().build();
        let branch = format!("private-invalid-{case}-remote");
        let merge = format!("refs/heads/{branch}");
        ctx.checkout_new(&branch);
        match case {
            "empty" => ctx.set_config("gherrit.remote", Some("")),
            "repeated" => {
                ctx.set_config("gherrit.remote", Some("first"));
                ctx.run_git(&["config", "--add", "gherrit.remote", "second"]);
            }
            _ => unreachable!(),
        }

        ctx.manage_cmd().arg("--private").assert().success();
        assert_branch_config(
            &ctx,
            &branch,
            Some(testutil::MANAGED_PRIVATE),
            Some("."),
            Some("."),
            Some(&merge),
        );

        ctx.set_config(&format!("branch.{branch}.pushRemote"), Some("drifted"));
        ctx.manage_cmd().args(["--private", "--force"]).assert().success();
        assert_branch_config(
            &ctx,
            &branch,
            Some(testutil::MANAGED_PRIVATE),
            Some("."),
            Some("."),
            Some(&merge),
        );
    });
}

#[test]
fn current_private_cleanup_does_not_read_invalid_remote_configuration() {
    ["empty", "repeated"].into_iter().for_each(|case| {
        let ctx = testutil::test_context!().build();
        let branch = format!("private-invalid-{case}-remote");
        let merge = format!("refs/heads/{branch}");
        ctx.checkout_new(&branch);
        configure_branch(
            &ctx,
            &branch,
            Some(testutil::MANAGED_PRIVATE),
            Some("."),
            Some("."),
            Some(&merge),
        );
        match case {
            "empty" => ctx.set_config("gherrit.remote", Some("")),
            "repeated" => {
                ctx.set_config("gherrit.remote", Some("first"));
                ctx.run_git(&["config", "--add", "gherrit.remote", "second"]);
            }
            _ => unreachable!(),
        }

        ctx.unmanage_cmd().assert().success();
        assert_branch_config(&ctx, &branch, Some("false"), None, None, None);
    });
}

#[cfg(unix)]
#[test]
fn current_private_cleanup_does_not_read_non_utf8_remote_configuration() {
    use std::os::unix::ffi::OsStrExt as _;

    let ctx = testutil::test_context!().build();
    let branch = "private-non-utf8-remote";
    let merge = format!("refs/heads/{branch}");
    ctx.checkout_new(branch);
    configure_branch(
        &ctx,
        branch,
        Some(testutil::MANAGED_PRIVATE),
        Some("."),
        Some("."),
        Some(&merge),
    );
    ctx.git_cmd()
        .args(["config", "--add", "gherrit.remote"])
        .arg(std::ffi::OsStr::from_bytes(b"\xff"))
        .assert()
        .success();

    ctx.unmanage_cmd().assert().success();
    assert_branch_config(&ctx, branch, Some("false"), None, None, None);
}

#[test]
fn test_manage_mutually_exclusive_flags() {
    let ctx = testutil::test_context!().build();
    ctx.checkout_new("conflict-feature");

    // Attempt to set both flags
    testutil::assert_failure_snapshot!(
        ctx,
        ctx.gherrit_cmd().args(["manage", "--public", "--private"]),
        "manage_mutually_exclusive",
    );
}

#[test]
fn forced_public_cleanup_does_not_require_a_parseable_remote() {
    for requested in ["private", "unmanaged"] {
        let ctx = testutil::test_context!().build();
        let branch = format!("forced-public-{requested}");
        let merge = format!("refs/heads/{branch}");
        ctx.checkout_new(&branch);
        ctx.set_config(&format!("branch.{branch}.gherritManaged"), Some(testutil::MANAGED_PUBLIC));
        ctx.set_config(&format!("branch.{branch}.pushRemote"), Some("drifted"));
        ctx.set_config(&format!("branch.{branch}.remote"), Some("."));
        ctx.set_config(&format!("branch.{branch}.merge"), Some(&merge));
        ctx.set_config("gherrit.remote", Some("first"));
        ctx.run_git(&["config", "--add", "gherrit.remote", "second"]);

        let mut command =
            if requested == "private" { ctx.manage_cmd() } else { ctx.unmanage_cmd() };
        if requested == "private" {
            command.arg("--private");
        }
        command.arg("--force").assert().success();
        ctx.assert_config(
            &format!("branch.{branch}.gherritManaged"),
            Some(if requested == "private" { testutil::MANAGED_PRIVATE } else { "false" }),
        );
    }
}

#[test]
fn test_manage_invalid_config() {
    let ctx = testutil::test_context!().build();
    ctx.checkout_new("invalid-config-feature");

    // Manually set invalid config
    ctx.run_git(&["config", "branch.invalid-config-feature.gherritManaged", "bad-value"]);

    // Attempt to manage; should fail
    testutil::assert_failure_snapshot!(ctx, ctx.manage_cmd(), "manage_invalid_config");
}

#[test]
fn legacy_public_shape_is_not_adopted_for_other_ownership_states() {
    let cases = [
        ("manual", None),
        ("unmanaged", Some("false")),
        ("private", Some(testutil::MANAGED_PRIVATE)),
    ];

    cases.into_iter().for_each(|(name, state)| {
        let ctx = testutil::test_context!().build();
        let branch = format!("legacy-shaped-{name}");
        let merge = format!("refs/heads/{branch}");
        ctx.checkout_new(&branch);
        ctx.set_config("gherrit.remote", Some("publication"));
        configure_branch(&ctx, &branch, state, Some("publication"), Some("."), Some(&merge));

        ctx.manage_cmd().arg("--public").assert().failure();
        assert_branch_config(&ctx, &branch, state, Some("publication"), Some("."), Some(&merge));
    });
}

#[cfg(unix)]
#[test]
fn management_state_rejects_non_utf8_without_echoing_it() {
    use std::os::unix::ffi::OsStrExt as _;

    let ctx = testutil::test_context!().build();
    let branch = "state-non-utf8";
    let key = format!("branch.{branch}.gherritManaged");
    ctx.checkout_new(branch);
    ctx.git_cmd()
        .args(["config", "--local", "--add", &key])
        .arg(std::ffi::OsStr::from_bytes(b"managedPrivate\xff"))
        .assert()
        .success();

    ctx.manage_cmd()
        .assert()
        .failure()
        .stderr(predicates::str::contains("Invalid gherritManaged value"));
}

#[test]
fn branch_ownership_requires_one_exact_value_and_force_repairs_it() {
    let cases = [
        ("pushRemote", ".", " . "),
        ("remote", ".", ". "),
        ("merge", "refs/heads/exact-config", " refs/heads/exact-config"),
    ];

    for (suffix, expected, drifted) in cases {
        let ctx = testutil::test_context!().build();
        let branch = "exact-config";
        let key = format!("branch.{branch}.{suffix}");
        ctx.checkout_new(branch);
        ctx.manage_cmd().arg("--private").assert().success();

        ctx.git_cmd()
            .args(["config", "--local", "--replace-all", &key, drifted])
            .assert()
            .success();
        ctx.manage_cmd().arg("--private").assert().failure();
        ctx.git_cmd()
            .args(["config", "--local", "--get", &key])
            .assert()
            .stdout(format!("{drifted}\n"));

        ctx.git_cmd().args(["config", "--local", "--add", &key, expected]).assert().success();
        ctx.manage_cmd().args(["--private", "--force"]).assert().success();
        ctx.git_cmd()
            .args(["config", "--local", "--get-all", &key])
            .assert()
            .success()
            .stdout(format!("{expected}\n"));

        ctx.git_cmd().args(["config", "--local", "--add", &key, expected]).assert().success();
        ctx.manage_cmd().arg("--private").assert().failure();
        ctx.git_cmd()
            .args(["config", "--local", "--get-all", &key])
            .assert()
            .success()
            .stdout(format!("{expected}\n{expected}\n"));
        ctx.manage_cmd().args(["--private", "--force"]).assert().success();
        ctx.git_cmd()
            .args(["config", "--local", "--get-all", &key])
            .assert()
            .success()
            .stdout(format!("{expected}\n"));
    }
}

#[cfg(unix)]
#[test]
fn force_repairs_a_non_utf8_owned_branch_value() {
    use std::os::unix::ffi::OsStrExt as _;

    let ctx = testutil::test_context!().build();
    let branch = "non-utf8-owned-config";
    let key = format!("branch.{branch}.pushRemote");
    ctx.checkout_new(branch);
    ctx.manage_cmd().arg("--private").assert().success();
    ctx.set_config(&key, None);
    ctx.git_cmd()
        .args(["config", "--local", "--add", &key])
        .arg(std::ffi::OsStr::from_bytes(b"\xff"))
        .assert()
        .success();

    ctx.manage_cmd().args(["--private", "--force"]).assert().success();
    ctx.git_cmd().args(["config", "--local", "--get-all", &key]).assert().success().stdout(".\n");
}

#[test]
fn forced_cleanup_unsets_every_owned_branch_occurrence() {
    let ctx = testutil::test_context!().build();
    let branch = "repeated-cleanup";
    ctx.checkout_new(branch);
    ctx.manage_cmd().arg("--private").assert().success();
    for suffix in ["pushRemote", "remote", "merge"] {
        ctx.git_cmd()
            .args(["config", "--local", "--add", &format!("branch.{branch}.{suffix}"), "duplicate"])
            .assert()
            .success();
    }

    ctx.unmanage_cmd().arg("--force").assert().success();
    assert_branch_config(&ctx, branch, Some("false"), None, None, None);
}

#[test]
fn management_ownership_ignores_global_and_included_branch_values() {
    let ctx = testutil::test_context!().build();
    let branch = "local-ownership";
    let state_key = format!("branch.{branch}.gherritManaged");
    let push_key = format!("branch.{branch}.pushRemote");
    ctx.checkout_new(branch);
    ctx.git_cmd().args(["config", "--global", &state_key, "false"]).assert().success();
    ctx.git_cmd().args(["config", "--global", &push_key, "global-drift"]).assert().success();

    let included = ctx.dir.path().join("included-config");
    std::fs::write(&included, format!("[branch \"{branch}\"]\n\tpushRemote = included-drift\n"))
        .unwrap();
    ctx.git_cmd().args(["config", "--local", "include.path"]).arg(&included).assert().success();

    ctx.manage_cmd().arg("--private").assert().success();
    ctx.git_cmd()
        .args(["config", "--local", "--get-all", &state_key])
        .assert()
        .success()
        .stdout(format!("{}\n", testutil::MANAGED_PRIVATE));
    ctx.git_cmd()
        .args(["config", "--local", "--get-all", &push_key])
        .assert()
        .success()
        .stdout(".\n");
}

#[test]
fn management_refuses_an_effective_worktree_override_without_writes() {
    let ctx = testutil::test_context!().with_initial_commit().build();
    ctx.run_git(&["checkout", "-b", "feature-work"]);
    ctx.run_git(&["config", "extensions.worktreeConfig", "true"]);
    ctx.run_git(&["config", "--worktree", "branch.feature-work.pushRemote", "origin"]);
    let before = std::fs::read(ctx.repo_path.join(".git/config")).unwrap();
    ctx.gherrit_cmd()
        .args(["manage", "--force"])
        .assert()
        .failure()
        .stderr(predicates::str::contains("overridden in worktree scope"));
    assert_eq!(std::fs::read(ctx.repo_path.join(".git/config")).unwrap(), before);
}

#[test]
fn explicit_management_refusal_returns_failure() {
    let ctx = testutil::test_context!().with_initial_commit().build();
    ctx.run_git(&["checkout", "-b", "feature-work"]);
    ctx.run_git(&["config", "branch.feature-work.pushRemote", "custom"]);
    ctx.gherrit_cmd()
        .arg("manage")
        .assert()
        .failure()
        .stderr(predicates::str::contains("management was not changed"));
}

#[test]
fn late_local_include_cannot_override_planned_management() {
    for forced in [false, true] {
        let ctx = testutil::test_context!().with_initial_commit().build();
        ctx.checkout_managed_private("late-include");
        let included = ctx.repo_path.join(".git/included-config");
        std::fs::write(&included, "[branch \"late-include\"]\n\tpushRemote = origin\n").unwrap();
        // Git replaces the existing pushRemote in place, before this include.
        ctx.run_git(&["config", "include.path", "included-config"]);
        if forced {
            ctx.run_git(&["config", "branch.late-include.pushRemote", "manual-drift"]);
        }
        let before = std::fs::read(ctx.repo_path.join(".git/config")).unwrap();
        let mut command = ctx.manage_cmd();
        command.arg("--private");
        if forced {
            command.arg("--force");
        }
        command
            .assert()
            .failure()
            .stderr(predicates::str::contains("overridden by included local configuration"));
        assert_eq!(std::fs::read(ctx.repo_path.join(".git/config")).unwrap(), before);
    }
}

#[test]
fn management_preview_retains_global_url_conditions_for_local_includes() {
    let ctx = testutil::test_context!().with_initial_commit().build();
    ctx.checkout_managed_private("conditional-include");
    ctx.run_git(&["config", "--global", "remote.condition.url", "https://example.com/context"]);
    std::fs::write(
        ctx.repo_path.join(".git/conditional-config"),
        "[branch \"conditional-include\"]\n\tpushRemote = origin\n",
    )
    .unwrap();
    ctx.run_git(&[
        "config",
        "includeIf.hasconfig:remote.*.url:https://example.com/context.path",
        "conditional-config",
    ]);
    let before = std::fs::read(ctx.repo_path.join(".git/config")).unwrap();
    ctx.manage_cmd()
        .arg("--private")
        .assert()
        .failure()
        .stderr(predicates::str::contains("overridden by included local configuration"));
    assert_eq!(std::fs::read(ctx.repo_path.join(".git/config")).unwrap(), before);
}

#[test]
fn command_scope_cannot_hide_a_persistent_worktree_override() {
    let ctx = testutil::test_context!().with_initial_commit().build();
    ctx.run_git(&["checkout", "-b", "masked-worktree"]);
    ctx.run_git(&["config", "extensions.worktreeConfig", "true"]);
    ctx.run_git(&["config", "--worktree", "branch.masked-worktree.pushRemote", "origin"]);
    let before = std::fs::read(ctx.repo_path.join(".git/config")).unwrap();
    ctx.manage_cmd()
        .args(["--private", "--force"])
        .env("GIT_CONFIG_COUNT", "1")
        .env("GIT_CONFIG_KEY_0", "branch.masked-worktree.pushRemote")
        .env("GIT_CONFIG_VALUE_0", ".")
        .assert()
        .failure()
        .stderr(predicates::str::contains("overridden in worktree scope"));
    assert_eq!(std::fs::read(ctx.repo_path.join(".git/config")).unwrap(), before);
}

#[test]
fn earlier_include_merge_values_block_management_before_any_write() {
    let ctx = testutil::test_context!().with_remote().with_initial_commit().build();
    ctx.checkout_new("early-include");
    let config = ctx.repo_path.join(".git/config");
    let included = ctx.repo_path.join(".git/earlier.inc");
    std::fs::write(&included, "[branch \"early-include\"]\n\tmerge = refs/heads/wrong\n").unwrap();
    let original = std::fs::read_to_string(&config).unwrap();
    std::fs::write(&config, format!("[include]\n\tpath = earlier.inc\n{original}")).unwrap();
    let before = std::fs::read(&config).unwrap();
    ctx.manage_cmd()
        .arg("--private")
        .assert()
        .failure()
        .stderr(predicates::str::contains("overridden by included local configuration"));
    assert_eq!(std::fs::read(&config).unwrap(), before);
    ctx.assert_config("branch.early-include.gherritManaged", None);
    // Native Git selects the first merge value, even when --get reports
    // the last one which agrees with the proposed management configuration.
    ctx.run_git(&["config", "branch.early-include.remote", "origin"]);
    ctx.run_git(&["config", "branch.early-include.merge", "refs/heads/early-include"]);
    ctx.run_git(&["update-ref", "refs/remotes/origin/wrong", "HEAD"]);
    ctx.git_cmd()
        .args(["for-each-ref", "--format=%(upstream)", "refs/heads/early-include"])
        .assert()
        .success()
        .stdout("refs/remotes/origin/wrong\n");
}

#[test]
fn nonlocal_merge_occurrences_block_management_before_any_write() {
    for (scope, merge) in [
        ("global", "refs/heads/wrong"),
        ("global", "refs/heads/nonlocal-merge"),
        ("system", "refs/heads/wrong"),
    ] {
        let ctx = testutil::test_context!().with_remote().with_initial_commit().build();
        ctx.checkout_new("nonlocal-merge");
        let config = ctx.repo_path.join(".git/config");
        let external = ctx.repo_path.join("external-config");
        std::fs::write(&external, format!("[branch \"nonlocal-merge\"]\nmerge = {merge}\n"))
            .unwrap();
        let before = std::fs::read(&config).unwrap();
        let mut command = ctx.manage_cmd();
        command.args(["--private", "--force"]);
        if scope == "system" {
            command.env("GIT_CONFIG_NOSYSTEM", "0").env("GIT_CONFIG_SYSTEM", &external);
        } else {
            command.env("GIT_CONFIG_GLOBAL", &external);
        }
        command.assert().failure().stderr(predicates::str::contains("nonlocal merge targets"));
        assert_eq!(std::fs::read(&config).unwrap(), before);
        ctx.assert_config("branch.nonlocal-merge.gherritManaged", None);
    }
}

#[test]
fn public_cleanup_with_invalid_destination_preserves_drift_unless_forced() {
    for request in ["private", "unmanaged"] {
        for invalid in ["empty", "duplicate"] {
            let ctx = testutil::test_context!().with_initial_commit().build();
            let branch = "public-cleanup";
            ctx.checkout_new(branch);
            ctx.set_config("branch.public-cleanup.gherritManaged", Some(testutil::MANAGED_PUBLIC));
            ctx.set_config("branch.public-cleanup.pushRemote", Some("user-owned"));
            ctx.set_config("branch.public-cleanup.remote", Some("."));
            ctx.set_config("branch.public-cleanup.merge", Some("refs/heads/public-cleanup"));
            if invalid == "empty" {
                ctx.set_config("gherrit.remote", Some(""));
            } else {
                ctx.set_config("gherrit.remote", Some("origin"));
                ctx.run_git(&["config", "--add", "gherrit.remote", "other"]);
            }
            let before = std::fs::read(ctx.repo_path.join(".git/config")).unwrap();
            let mut command = ctx.gherrit_cmd();
            if request == "private" {
                command.args(["manage", "--private"]);
            } else {
                command.arg("unmanage");
            }
            command.assert().failure();
            assert_eq!(std::fs::read(ctx.repo_path.join(".git/config")).unwrap(), before);
            let mut command = ctx.gherrit_cmd();
            if request == "private" {
                command.args(["manage", "--private", "--force"]);
            } else {
                command.args(["unmanage", "--force"]);
            }
            command.assert().success();
        }
    }
}

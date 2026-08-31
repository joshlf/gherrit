#[test]
fn test_branch_management() {
    let ctx = testutil::test_context!().build();

    // Create a branch to manage
    ctx.checkout_new("feature-A");

    // Scenario A: Custom Push Remote Preservation
    ctx.run_git(&["config", "branch.feature-A.pushRemote", "origin"]);

    // Attempt manage - should fail (drift)
    testutil::assert_success_snapshot!(ctx, ctx.manage_cmd(), "branch_management_drift_warning"); // Logs warning, no change
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

    testutil::assert_success_snapshot!(ctx, ctx.unmanage_cmd(), "unmanage_preserve_drift",);

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
    testutil::assert_success_snapshot!(
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

    // The enclosing push remains a local no-op in both visibility modes.
    ctx.assert_config("branch.drift-feature.pushRemote", Some("."));
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
    ctx.assert_config("branch.visibility-feature.pushRemote", Some("."));

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
fn exact_legacy_public_configuration_migrates_once() {
    let ctx = testutil::test_context!().build();
    let branch = "legacy-public";
    let merge = format!("refs/heads/{branch}");
    ctx.checkout_new(branch);
    ctx.set_config("gherrit.remote", Some("publication"));
    configure_branch(
        &ctx,
        branch,
        Some(testutil::MANAGED_PUBLIC),
        Some("publication"),
        Some("."),
        Some(&merge),
    );

    ctx.manage_cmd().assert().success();
    assert_branch_config(
        &ctx,
        branch,
        Some(testutil::MANAGED_PUBLIC),
        Some("."),
        Some("."),
        Some(&merge),
    );

    // The migrated form is the ordinary current form, so repeating the command
    // neither needs the legacy remote nor changes any configuration.
    ctx.set_config("gherrit.remote", Some("different-publication"));
    ctx.run_git(&["config", "--add", "gherrit.remote", "duplicate-publication"]);
    ctx.manage_cmd().assert().success();
    assert_branch_config(
        &ctx,
        branch,
        Some(testutil::MANAGED_PUBLIC),
        Some("."),
        Some("."),
        Some(&merge),
    );
}

#[test]
fn static_legacy_public_near_misses_do_not_read_the_legacy_remote() {
    let cases = [
        ("missing-push-remote", None, Some("."), None),
        ("upstream-remote", Some("publication"), Some("publication"), None),
        ("merge-target", Some("publication"), Some("."), Some("refs/heads/other-branch")),
    ];

    cases.into_iter().for_each(|(name, push_remote, remote, merge_override)| {
        let ctx = testutil::test_context!().build();
        let branch = format!("legacy-{name}");
        let merge = format!("refs/heads/{branch}");
        ctx.checkout_new(&branch);
        // An empty remote is invalid. These static near misses must remain
        // ordinary drift instead of trying to decode unrelated legacy state.
        ctx.set_config("gherrit.remote", Some(""));
        configure_branch(
            &ctx,
            &branch,
            Some(testutil::MANAGED_PUBLIC),
            push_remote,
            remote,
            merge_override.or(Some(merge.as_str())),
        );

        ctx.manage_cmd().assert().success();
        assert_branch_config(
            &ctx,
            &branch,
            Some(testutil::MANAGED_PUBLIC),
            push_remote,
            remote,
            merge_override.or(Some(merge.as_str())),
        );

        ctx.manage_cmd().arg("--force").assert().success();
        assert_branch_config(
            &ctx,
            &branch,
            Some(testutil::MANAGED_PUBLIC),
            Some("."),
            Some("."),
            Some(&merge),
        );
    });
}

#[test]
fn unreadable_legacy_remote_makes_an_exact_candidate_drift() {
    ["empty", "duplicate"].into_iter().for_each(|case| {
        let ctx = testutil::test_context!().build();
        let branch = format!("legacy-{case}-remote");
        let merge = format!("refs/heads/{branch}");
        ctx.checkout_new(&branch);
        configure_branch(
            &ctx,
            &branch,
            Some(testutil::MANAGED_PUBLIC),
            Some("publication"),
            Some("."),
            Some(&merge),
        );
        match case {
            "empty" => ctx.set_config("gherrit.remote", Some("")),
            "duplicate" => {
                ctx.set_config("gherrit.remote", Some("publication"));
                ctx.run_git(&["config", "--add", "gherrit.remote", "other-publication"]);
            }
            _ => unreachable!(),
        }

        ctx.manage_cmd().assert().success();
        assert_branch_config(
            &ctx,
            &branch,
            Some(testutil::MANAGED_PUBLIC),
            Some("publication"),
            Some("."),
            Some(&merge),
        );
    });
}

#[test]
fn forced_public_drift_repair_does_not_require_a_parseable_legacy_remote() {
    let ctx = testutil::test_context!().build();
    let branch = "forced-public-drift";
    let merge = format!("refs/heads/{branch}");
    ctx.checkout_new(branch);
    configure_branch(
        &ctx,
        branch,
        Some(testutil::MANAGED_PUBLIC),
        Some("manually-drifted"),
        Some("."),
        Some(&merge),
    );
    ctx.set_config("gherrit.remote", Some("first"));
    ctx.run_git(&["config", "--add", "gherrit.remote", "second"]);

    ctx.manage_cmd().args(["--private", "--force"]).assert().success();

    assert_branch_config(
        &ctx,
        branch,
        Some(testutil::MANAGED_PRIVATE),
        Some("."),
        Some("."),
        Some(&merge),
    );
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

        ctx.manage_cmd().arg("--public").assert().success();
        assert_branch_config(&ctx, &branch, state, Some("publication"), Some("."), Some(&merge));
    });
}

#[test]
fn legacy_public_configuration_is_owned_during_visibility_and_cleanup_transitions() {
    let private = testutil::test_context!().build();
    let private_branch = "legacy-to-private";
    let private_merge = format!("refs/heads/{private_branch}");
    private.checkout_new(private_branch);
    private.set_config("gherrit.remote", Some("publication"));
    configure_branch(
        &private,
        private_branch,
        Some(testutil::MANAGED_PUBLIC),
        Some("publication"),
        Some("."),
        Some(&private_merge),
    );

    private.manage_cmd().arg("--private").assert().success();
    assert_branch_config(
        &private,
        private_branch,
        Some(testutil::MANAGED_PRIVATE),
        Some("."),
        Some("."),
        Some(&private_merge),
    );

    let unmanaged = testutil::test_context!().build();
    let unmanaged_branch = "legacy-to-unmanaged";
    let unmanaged_merge = format!("refs/heads/{unmanaged_branch}");
    unmanaged.checkout_new(unmanaged_branch);
    unmanaged.set_config("gherrit.remote", Some("publication"));
    configure_branch(
        &unmanaged,
        unmanaged_branch,
        Some(testutil::MANAGED_PUBLIC),
        Some("publication"),
        Some("."),
        Some(&unmanaged_merge),
    );

    unmanaged.unmanage_cmd().assert().success();
    assert_branch_config(&unmanaged, unmanaged_branch, Some("false"), None, None, None);
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
fn public_management_requires_a_ref_namespace_disjoint_from_change_ids() {
    for branch in ["feature", "feature/one", "Gchange", "Gchange/child", "gherrit-bases/Gchange"] {
        let ctx = testutil::test_context!().build();
        ctx.checkout_new(branch);

        ctx.gherrit_cmd().args(["manage", "--public"]).assert().failure();

        ctx.assert_config(&format!("branch.{branch}.gherritManaged"), None);
        ctx.assert_config(&format!("branch.{branch}.pushRemote"), None);
        ctx.assert_config(&format!("branch.{branch}.remote"), None);
        ctx.assert_config(&format!("branch.{branch}.merge"), None);
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

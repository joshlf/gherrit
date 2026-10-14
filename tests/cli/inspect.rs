use serde_json::{Value, json};

#[test]
fn status_is_local_read_only_and_reports_complete_identity_problems() {
    let context = testutil::test_context!().with_remote().with_initial_commit().build();
    context.checkout_managed_private("status-test");
    context.commit_with_gherrit_id("Identified");
    context.commit("Missing");
    let config = std::fs::read(context.repo_path.join(".git/config")).unwrap();
    let head = context.head_oid();
    let result = context
        .gherrit_cmd()
        .args(["status", "--json", "--base", "main"])
        .env("GH_TOKEN", "unusable-status-must-stay-local")
        .env("GITHUB_TOKEN", "unusable-status-must-stay-local")
        .assert()
        .success();
    let report: Value = serde_json::from_slice(&result.get_output().stdout).unwrap();
    assert_eq!(report["schema_version"], 1);
    assert_eq!(report["remote"], json!({"state": "not_requested"}));
    assert_eq!(report["management"]["value"]["intent"], "managedPrivate");
    assert_eq!(report["management"]["value"]["drift"], json!([]));
    assert_eq!(report["stack"]["commits"].as_array().unwrap().len(), 2);
    assert_eq!(report["stack"]["commits"][1]["problems"], json!(["missing identity"]));
    assert_eq!(context.head_oid(), head);
    assert_eq!(std::fs::read(context.repo_path.join(".git/config")).unwrap(), config);
    assert!(!report.to_string().contains("unusable-status"));
}

#[test]
fn inspection_drift_and_missing_base_stay_facts_without_repair() {
    let context = testutil::test_context!().with_initial_commit().build();
    context.checkout_managed_private("status-drift");
    context.set_config("branch.status-drift.merge", Some("refs/heads/wrong"));
    let config = std::fs::read(context.repo_path.join(".git/config")).unwrap();
    let result = context
        .gherrit_cmd()
        .args(["inspect", "--json", "--base", "missing-ref"])
        .assert()
        .success();
    let report: Value = serde_json::from_slice(&result.get_output().stdout).unwrap();
    assert_eq!(report["stack"]["state"], "unavailable");
    assert_eq!(report["management"]["value"]["drift"].as_array().unwrap().len(), 1);
    assert_eq!(report["management"]["value"]["drift"][0]["field"], "merge");
    assert_eq!(std::fs::read(context.repo_path.join(".git/config")).unwrap(), config);
}

#[test]
fn native_git_status_survives_an_unsupported_reftable_backend() {
    let context = testutil::test_context!().with_initial_commit().build();
    let fixture = tempfile::tempdir().unwrap();
    // A raw local Git fixture command has a finite test-runner timeout. Older
    // Git builds explicitly lack this capability and cannot supply its oracle.
    let result = context
        .git_cmd()
        .args(["init", "--bare", "--ref-format=reftable"])
        .arg(fixture.path())
        .assert();
    if !result.get_output().status.success() {
        eprintln!("Git cannot construct a reftable fixture; capability witness unavailable");
        return;
    }
    let result = context
        .gherrit_cmd()
        .current_dir(fixture.path())
        .args(["status", "--json"])
        .assert()
        .success();
    let report: Value = serde_json::from_slice(&result.get_output().stdout).unwrap();
    assert_eq!(report["capabilities"]["ref_backend"]["value"]["utf8"], "reftable");
    assert_eq!(report["capabilities"]["gix"]["state"], "unsupported");
    context
        .gherrit_cmd()
        .current_dir(fixture.path())
        .args(["manage"])
        .assert()
        .failure()
        .stderr(predicates::str::contains("uses reftable refs"));
}

#[test]
fn remote_inspection_is_explicit_and_unsupported_destinations_are_unavailable() {
    let context = testutil::test_context!().with_remote().with_initial_commit().build();
    context.checkout_managed_private("status-remote");
    context.commit_with_gherrit_id("Change");
    let result = context
        .gherrit_cmd()
        .args(["status", "--json", "--remote", "--base", "main"])
        .assert()
        .success();
    let report: Value = serde_json::from_slice(&result.get_output().stdout).unwrap();
    assert_eq!(report["mode"], "remote");
    assert_eq!(report["remote"]["state"], "unavailable");
    assert!(report["remote"]["reason"].as_str().unwrap().contains("github.com"));
    assert_eq!(report["stack"]["commits"].as_array().unwrap().len(), 1);
}

#[test]
fn user_configuration_does_not_select_the_repository_ref_backend() {
    let context = testutil::test_context!().with_initial_commit().build();
    let global = context.dir.path().join("global-ref-storage.config");
    std::fs::write(&global, "[extensions]\n\trefStorage = reftable\n").unwrap();
    context.gherrit_cmd().arg("manage").env("GIT_CONFIG_GLOBAL", &global).assert().success();
    context
        .gherrit_cmd()
        .arg("manage")
        .env("GIT_CONFIG_COUNT", "1")
        .env("GIT_CONFIG_KEY_0", "extensions.refStorage")
        .env("GIT_CONFIG_VALUE_0", "reftable")
        .assert()
        .success();
}

#[test]
fn annotated_local_base_is_peeled_to_its_commit() {
    let ctx = testutil::test_context!().with_initial_commit().build();
    let base = ctx.head_oid();
    ctx.run_git(&["tag", "-a", "inspection-base", "-m", "base"]);
    ctx.checkout_managed_private("annotated-base");
    ctx.commit_with_gherrit_id("Change");
    let output = ctx
        .gherrit_cmd()
        .args(["status", "--json", "--base", "inspection-base"])
        .assert()
        .success();
    let report: Value = serde_json::from_slice(&output.get_output().stdout).unwrap();
    assert_eq!(report["stack"]["state"], "observed");
    assert_eq!(report["stack"]["base_oid"], base);
    assert_eq!(report["stack"]["commits"].as_array().unwrap().len(), 1);
}

#[test]
// Darwin filesystems reject byte-valued ref names before GHerrit can run.
#[cfg(all(unix, not(target_os = "macos")))]
fn byte_branch_management_is_unavailable_instead_of_lossily_queried() {
    use std::{ffi::OsString, os::unix::ffi::OsStringExt as _};
    let ctx = testutil::test_context!().with_initial_commit().build();
    let branch = b"branch-\xff";
    ctx.git_cmd()
        .arg("checkout")
        .arg("-b")
        .arg(OsString::from_vec(branch.to_vec()))
        .assert()
        .success();
    let output = ctx.gherrit_cmd().args(["status", "--json", "--base", "main"]).assert().success();
    let report: Value = serde_json::from_slice(&output.get_output().stdout).unwrap();
    assert_eq!(report["head"]["branch"]["value"]["utf8"], Value::Null);
    assert_eq!(report["head"]["branch"]["value"]["hex"], "6272616e63682dff");
    assert_eq!(report["capabilities"]["gix"]["state"], "supported");
    assert_eq!(report["stack"]["state"], "observed");
    assert_eq!(report["stack"]["commits"], json!([]));
    assert_eq!(report["management"]["state"], "unavailable");
}

#[test]
fn format_zero_backend_matches_native_git_not_an_unconditional_extension() {
    let ctx = testutil::test_context!().with_initial_commit().build();
    ctx.run_git(&["config", "extensions.refStorage", "reftable"]);
    // Recent Git honors this extension even under format 0; older supported
    // Git ignores it. Assert against the native capability when available.
    let native = ctx.git_cmd().args(["rev-parse", "--show-ref-format"]).output().unwrap();
    let result = ctx.gherrit_cmd().arg("manage").output().unwrap();
    if native.status.success() && native.stdout == b"reftable\n" {
        assert!(!result.status.success());
        assert!(String::from_utf8_lossy(&result.stderr).contains("uses reftable refs"));
    } else if native.status.success() && native.stdout == b"--show-ref-format\n" {
        assert!(result.status.success(), "{}", String::from_utf8_lossy(&result.stderr));
    }
}

#[test]
fn stack_inspection_detects_ids_reused_from_the_selected_base() {
    let ctx = testutil::test_context!().with_initial_commit().build();
    let id = ctx.commit_with_gherrit_id("Base identity");
    ctx.run_git(&["tag", "selected-base"]);
    ctx.checkout_managed_private("reused-identity");
    ctx.commit(&format!("Repeated\n\ngherrit-pr-id: {id}"));
    let output =
        ctx.gherrit_cmd().args(["status", "--json", "--base", "selected-base"]).assert().success();
    let report: Value = serde_json::from_slice(&output.get_output().stdout).unwrap();
    assert_eq!(report["stack"]["identity_ancestry"]["state"], "observed");
    assert_eq!(report["stack"]["commits"][0]["problems"], json!(["repeated identity"]));
}

#[test]
fn unmanaged_upstream_settings_are_not_management_drift() {
    let context = testutil::test_context!().with_initial_commit().build();
    context.checkout_new("ordinary-upstream");
    for (suffix, value) in
        [("remote", "origin"), ("merge", "refs/heads/main"), ("pushRemote", "backup")]
    {
        context.set_config(&format!("branch.ordinary-upstream.{suffix}"), Some(value));
    }
    for recorded in [false, true] {
        if recorded {
            context.unmanage_cmd().assert().success();
        }
        let config = std::fs::read(context.repo_path.join(".git/config")).unwrap();
        let result =
            context.gherrit_cmd().args(["status", "--json", "--base", "main"]).assert().success();
        let report: Value = serde_json::from_slice(&result.get_output().stdout).unwrap();
        let expected_intent = if recorded { json!("false") } else { Value::Null };
        assert_eq!(report["management"]["value"]["intent"], expected_intent);
        assert_eq!(report["management"]["value"]["drift"], json!([]));
        assert_eq!(std::fs::read(context.repo_path.join(".git/config")).unwrap(), config);
        for (suffix, value) in
            [("remote", "origin"), ("merge", "refs/heads/main"), ("pushRemote", "backup")]
        {
            context
                .git_cmd()
                .args(["config", "--get", &format!("branch.ordinary-upstream.{suffix}")])
                .assert()
                .success()
                .stdout(format!("{value}\n"));
        }
    }
}

#[test]
fn inherited_repository_selectors_do_not_mix_inspection_repositories() {
    let ctx = testutil::test_context!().with_initial_commit().build();
    ctx.checkout_managed_private("inspected");
    let other = testutil::test_context!().with_initial_commit().build();
    other.checkout_new("redirected");
    let baseline = ctx
        .gherrit_cmd()
        .args(["status", "--json", "--base", "main"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let redirected = ctx
        .gherrit_cmd()
        .args(["status", "--json", "--base", "main"])
        .env("GIT_DIR", other.repo_path.join(".git"))
        .env("GIT_COMMON_DIR", other.repo_path.join(".git"))
        .env("GIT_WORK_TREE", &other.repo_path)
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    assert_eq!(
        serde_json::from_slice::<Value>(&baseline).unwrap(),
        serde_json::from_slice::<Value>(&redirected).unwrap()
    );
}

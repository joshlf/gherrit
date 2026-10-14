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

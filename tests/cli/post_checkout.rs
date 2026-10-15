#[test]
fn installed_hook_classifies_new_and_shared_branches() {
    let ctx = testutil::test_context!()
        .with_remote()
        .with_installed_hooks()
        .with_initial_commit()
        .build();

    testutil::assert_success_snapshot!(
        ctx,
        ctx.git_cmd().args(["checkout", "-b", "feature-stack"]),
        "post_checkout_new_stack",
    );
    ctx.assert_config("branch.feature-stack.gherritManaged", Some(testutil::MANAGED_PRIVATE));

    ctx.run_git(&["checkout", "main"]);
    ctx.run_git(&["update-ref", "refs/remotes/origin/collab-feature", "HEAD"]);
    testutil::assert_success_snapshot!(
        ctx,
        ctx.git_cmd().args([
            "checkout",
            "-b",
            "collab-feature",
            "--track",
            "origin/collab-feature",
        ]),
        "post_checkout_shared_branch",
    );
    ctx.assert_config("branch.collab-feature.gherritManaged", Some("false"));
}

#[test]
fn installed_checkout_hook_keeps_drift_refusal_nonfatal() {
    use predicates::prelude::*;
    let ctx = testutil::test_context!()
        .with_remote()
        .with_installed_hooks()
        .with_initial_commit()
        .build();
    ctx.run_git(&["update-ref", "refs/remotes/origin/main", "HEAD"]);
    ctx.git_cmd()
        .args(["checkout", "-b", "tracking-topic", "--track", "origin/main"])
        .assert()
        .success()
        .stderr(predicate::str::contains("Branch configuration was not changed"));
    ctx.git_cmd()
        .args(["symbolic-ref", "--short", "HEAD"])
        .assert()
        .success()
        .stdout("tracking-topic\n");
    ctx.assert_config("branch.tracking-topic.gherritManaged", None);
    ctx.assert_config("branch.tracking-topic.remote", Some("origin"));
    ctx.assert_config("branch.tracking-topic.merge", Some("refs/heads/main"));
    ctx.gherrit_cmd()
        .args(["manage", "--private"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("Branch configuration was not changed"));
    ctx.assert_config("branch.tracking-topic.gherritManaged", None);
    ctx.assert_config("branch.tracking-topic.remote", Some("origin"));
    ctx.assert_config("branch.tracking-topic.merge", Some("refs/heads/main"));
}

#[test]
fn failed_default_branch_observation_preserves_branch_settings() {
    use predicates::prelude::*;
    let ctx = testutil::test_context!().with_initial_commit().build();
    ctx.checkout_new("topic");
    ctx.run_git(&["config", "branch.topic.remote", "origin"]);
    ctx.run_git(&["config", "branch.topic.merge", "refs/heads/main"]);
    let path = ctx.repo_path.join(".git/refs/remotes/origin/HEAD");
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, "malformed ref\n").unwrap();
    let before = std::fs::read(ctx.repo_path.join(".git/config")).unwrap();
    ctx.hook_cmd("post-checkout")
        .args(["old", "new", "1"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("[ERROR]"));
    assert_eq!(std::fs::read(ctx.repo_path.join(".git/config")).unwrap(), before);
}

#[test]
fn failed_remote_setting_read_preserves_management_settings() {
    use std::io::Write as _;
    let ctx = testutil::test_context!().with_initial_commit().build();
    ctx.checkout_new("topic");
    let path = ctx.repo_path.join(".git/config");
    std::fs::OpenOptions::new()
        .append(true)
        .open(&path)
        .unwrap()
        .write_all(b"\n[gherrit]\n remote = \xff\n")
        .unwrap();
    let before = std::fs::read(&path).unwrap();
    ctx.gherrit_cmd().args(["manage", "--force", "--public"]).assert().failure();
    assert_eq!(std::fs::read(path).unwrap(), before);
}

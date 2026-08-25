use std::fs;

use predicates::prelude::*;

fn install_composite_pre_push(ctx: &testutil::TestContext, body: &str) {
    let hook = ctx.repo_path.join(".git/hooks/pre-push");
    fs::write(
        hook,
        format!(
            "#!/bin/sh\n\
             set -eu\n\
             printf 'enter:%s\\n' \"$1\" >> \"$GHERRIT_HOOK_LOG\"\n\
             gherrit hook pre-push \"$@\"\n\
             {body}\n"
        ),
    )
    .unwrap();
}

#[test]
fn installed_pre_push_projects_the_feature_commit() {
    let ctx = testutil::test_context!()
        .with_remote()
        .with_installed_hooks()
        .with_initial_commit()
        .with_mock_github()
        .build();

    ctx.checkout_new("feature-boundary");
    ctx.assert_config("branch.feature-boundary.gherritManaged", Some(testutil::MANAGED_PRIVATE));
    ctx.commit("Feature through installed hook");

    let id = ctx.gherrit_id("HEAD").unwrap();
    let oid = ctx.head_oid();
    let message =
        ctx.git_cmd().args(["show", "-s", "--format=%B", "HEAD"]).output().unwrap().stdout;
    let message = String::from_utf8(message).unwrap();
    assert_eq!(message.lines().filter(|line| line.starts_with("gherrit-pr-id: ")).count(), 1);

    ctx.git_cmd().args(["push", "origin", "feature-boundary"]).assert().success();

    assert_eq!(ctx.remote_ref_oid(&format!("refs/heads/{id}")).as_deref(), Some(oid.as_str()));
    let prs = ctx.github().pull_requests();
    assert_eq!(prs.len(), 1);
    assert_eq!(prs[0].head, id);
    testutil::assert_pr_snapshot!(ctx, "installed_pre_push_pr_state");
}

#[test]
fn internal_publication_preserves_composite_pre_push_checks() {
    let ctx = testutil::test_context!()
        .with_remote()
        .with_installed_hooks()
        .with_initial_commit()
        .with_mock_github()
        .build();
    let log = ctx.dir.path().join("pre-push.log");
    install_composite_pre_push(&ctx, "printf 'policy:%s\\n' \"$1\" >> \"$GHERRIT_HOOK_LOG\"");

    ctx.checkout_new("composite-boundary");
    ctx.commit("Feature through composite hook");

    ctx.git_cmd()
        .env("GHERRIT_HOOK_LOG", &log)
        .args(["push", "origin", "composite-boundary"])
        .assert()
        .success();

    assert_eq!(
        fs::read_to_string(log).unwrap(),
        concat!(
            "enter:origin\n",
            "enter:gherrit-publication\n",
            "policy:gherrit-publication\n",
            "policy:origin\n",
        )
    );
}

#[test]
fn independent_pre_push_check_can_reject_an_internal_publication() {
    let ctx = testutil::test_context!()
        .with_remote()
        .with_installed_hooks()
        .with_initial_commit()
        .with_mock_github()
        .build();
    let log = ctx.dir.path().join("pre-push.log");
    install_composite_pre_push(
        &ctx,
        "case \"$1\" in\n\
         gherrit-publication*) exit 73 ;;\n\
         esac",
    );

    ctx.checkout_new("rejected-composite-boundary");
    ctx.commit("Feature rejected by independent hook");
    let id = ctx.gherrit_id("HEAD").unwrap();

    ctx.git_cmd()
        .env("GHERRIT_HOOK_LOG", &log)
        .args(["push", "origin", "rejected-composite-boundary"])
        .assert()
        .failure();

    assert_eq!(fs::read_to_string(log).unwrap(), "enter:origin\nenter:gherrit-publication\n");
    assert_eq!(ctx.remote_ref_oid(&format!("refs/heads/{id}")), None);
    assert!(ctx.github().pull_requests().is_empty());
}

#[test]
fn installed_pre_push_blocks_the_enclosing_push() {
    let ctx = testutil::test_context!()
        .with_remote()
        .with_installed_hooks()
        .with_initial_commit()
        .build();

    ctx.checkout_new("blocked-boundary");
    ctx.commit("Work in progress");
    let id = ctx.gherrit_id("HEAD").unwrap();
    ctx.commit("fixup! Work in progress");

    ctx.git_cmd()
        .args(["push", "origin", "blocked-boundary"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("Stack contains pending fixup/squash/amend commits"));

    assert_eq!(ctx.remote_ref_oid("refs/heads/blocked-boundary"), None);
    assert_eq!(ctx.remote_ref_oid(&format!("refs/heads/{id}")), None);
}

#[test]
fn empty_managed_stack_returns_before_history_and_github_requirements() {
    let ctx = testutil::test_context!()
        .with_remote()
        .with_installed_hooks()
        .with_initial_commit()
        .build();
    ctx.gherrit_cmd().args(["manage", "--private"]).assert().success();
    fs::write(ctx.repo_path.join("custom-grafts"), format!("{}\n", ctx.head_oid())).unwrap();

    ctx.git_cmd()
        .env("GIT_GRAFT_FILE", "custom-grafts")
        .args(["push", "origin", "main"])
        .assert()
        .success();
}

#[test]
fn installed_pre_push_blocks_an_inherited_custom_graft_file() {
    let ctx = testutil::test_context!()
        .with_remote()
        .with_installed_hooks()
        .with_initial_commit()
        .build();

    ctx.checkout_new("custom-graft-boundary");
    ctx.commit("Feature with custom graft environment");
    let id = ctx.gherrit_id("HEAD").unwrap();
    fs::write(ctx.repo_path.join("custom-grafts"), format!("{}\n", ctx.head_oid())).unwrap();

    ctx.git_cmd()
        .env("GIT_GRAFT_FILE", "custom-grafts")
        .args(["push", "origin", "custom-graft-boundary"])
        .assert()
        .failure()
        .stderr(predicate::str::contains(
            "file named by GIT_GRAFT_FILE is nonempty because the enclosing Git push retains",
        ));

    assert_eq!(ctx.remote_ref_oid("refs/heads/custom-graft-boundary"), None);
    assert_eq!(ctx.remote_ref_oid(&format!("refs/heads/{id}")), None);
}

#[test]
fn installed_pre_push_blocks_an_inherited_custom_shallow_file() {
    let ctx = testutil::test_context!()
        .with_remote()
        .with_installed_hooks()
        .with_initial_commit()
        .build();

    ctx.checkout_new("custom-shallow-boundary");
    ctx.commit("Feature with custom shallow environment");
    let id = ctx.gherrit_id("HEAD").unwrap();
    fs::write(ctx.repo_path.join("custom-shallow"), format!("{}\n", ctx.head_oid())).unwrap();

    ctx.git_cmd()
        .env("GIT_SHALLOW_FILE", "custom-shallow")
        .args(["push", "origin", "custom-shallow-boundary"])
        .assert()
        .failure()
        .stderr(predicate::str::contains(
            "file named by GIT_SHALLOW_FILE is nonempty because the enclosing Git push retains",
        ));

    assert_eq!(ctx.remote_ref_oid("refs/heads/custom-shallow-boundary"), None);
    assert_eq!(ctx.remote_ref_oid(&format!("refs/heads/{id}")), None);
}

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

    ctx.git_cmd().arg("push").assert().success();

    assert_eq!(ctx.remote_ref_oid(&format!("refs/heads/{id}")).as_deref(), Some(oid.as_str()));
    assert_eq!(
        ctx.remote_ref_oid(&format!("refs/heads/gherrit-bases/{id}")),
        ctx.remote_ref_oid("refs/heads/main")
    );
    assert_eq!(
        ctx.remote_ref_oid(&format!("refs/tags/gherrit/{id}/v1")).as_deref(),
        Some(oid.as_str())
    );
    assert!(ctx.remote_ref_oid(&format!("refs/tags/gherrit/{id}/pr")).is_some());
    assert_eq!(ctx.remote_ref_oid("refs/heads/feature-boundary"), None);
    let prs = ctx.github().pull_requests();
    assert_eq!(prs.len(), 1);
    assert_eq!(prs[0].head, id);
    testutil::assert_pr_snapshot!(ctx, "installed_pre_push_pr_state");
}

#[test]
fn installed_pre_push_projects_a_public_branch_through_a_loopback_push() {
    let ctx = testutil::test_context!()
        .with_remote()
        .with_installed_hooks()
        .with_initial_commit()
        .with_mock_github()
        .build();

    ctx.checkout_new("public-boundary");
    ctx.gherrit_cmd().args(["manage", "--public"]).assert().success();
    ctx.assert_config("branch.public-boundary.pushRemote", Some("."));
    ctx.commit("Public feature through installed hook");
    let head = ctx.head_oid();

    ctx.git_cmd().arg("push").assert().success();

    assert_eq!(ctx.remote_ref_oid("refs/heads/public-boundary").as_deref(), Some(head.as_str()));
    let pull_requests = ctx.github().pull_requests();
    assert_eq!(pull_requests.len(), 1);
    assert!(
        pull_requests[0]
            .body
            .as_deref()
            .unwrap()
            .contains("[public\\-boundary](/owner/repo/tree/public-boundary)")
    );
}

#[test]
fn installed_pre_push_dry_run_still_performs_gherrit_publication() {
    let ctx = testutil::test_context!()
        .with_remote()
        .with_installed_hooks()
        .with_initial_commit()
        .with_mock_github()
        .build();

    ctx.checkout_new("dry-run-boundary");
    ctx.commit("Publish despite the enclosing dry run");
    let id = ctx.gherrit_id("HEAD").unwrap();
    let head = ctx.head_oid();

    ctx.git_cmd().args(["push", "--dry-run"]).assert().success();

    assert_eq!(ctx.remote_ref_oid(&format!("refs/heads/{id}")).as_deref(), Some(head.as_str()));
    assert_eq!(ctx.remote_ref_oid("refs/heads/dry-run-boundary"), None);
    assert_eq!(ctx.github().pull_requests().len(), 1);
}

#[test]
fn empty_public_stack_publishes_only_its_branch() {
    let ctx = testutil::test_context!()
        .with_remote()
        .with_installed_hooks()
        .with_initial_commit()
        .build();

    ctx.checkout_new("empty-public");
    ctx.gherrit_cmd().args(["manage", "--public"]).assert().success();
    let head = ctx.head_oid();

    ctx.git_cmd().arg("push").assert().success();

    assert_eq!(ctx.remote_ref_oid("refs/heads/empty-public").as_deref(), Some(head.as_str()));
    assert!(ctx.remote_refs("refs/tags/gherrit").is_empty());
}

#[test]
fn managed_push_rejects_an_explicit_external_destination_before_publication() {
    let ctx = testutil::test_context!()
        .with_remote()
        .with_installed_hooks()
        .with_initial_commit()
        .with_mock_github()
        .build();
    ctx.checkout_new("private-explicit-origin");
    ctx.commit("Must remain private");
    let id = ctx.gherrit_id("HEAD").unwrap();

    ctx.git_cmd()
        .args(["push", "origin", "private-explicit-origin"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("must use the local no-op destination"));

    assert!(ctx.remote_ref_oid("refs/heads/private-explicit-origin").is_none());
    assert!(ctx.remote_ref_oid(&format!("refs/heads/{id}")).is_none());
    assert!(ctx.github().requests().is_empty());
}

#[test]
fn installed_hook_forwards_wrong_remote_arguments_even_with_empty_stdin() {
    let ctx = testutil::test_context!()
        .with_remote()
        .with_installed_hooks()
        .with_initial_commit()
        .with_mock_github()
        .build();
    ctx.checkout_new("wrong-remote-empty-input");
    ctx.commit("Do not publish through the wrong remote");
    let id = ctx.gherrit_id("HEAD").unwrap();

    // origin/main is already current, so Git invokes the hook with no update
    // records. Only forwarding the hook argv distinguishes this from the
    // accepted direct hidden-command boundary.
    ctx.git_cmd()
        .args(["push", "origin", "main"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("must use the local no-op destination"));

    assert!(ctx.remote_ref_oid(&format!("refs/heads/{id}")).is_none());
    assert!(ctx.github().requests().is_empty());
}

#[test]
fn installed_hook_forwards_nonempty_ref_update_input() {
    let ctx = testutil::test_context!()
        .with_remote()
        .with_installed_hooks()
        .with_initial_commit()
        .with_mock_github()
        .build();
    ctx.checkout_new("loopback-refspec");
    ctx.commit("Do not execute an enclosing ref update");
    let id = ctx.gherrit_id("HEAD").unwrap();

    ctx.git_cmd()
        .args(["push", ".", "HEAD:refs/heads/alternate"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("cannot include an enclosing Git ref update"));

    ctx.git_cmd()
        .args(["rev-parse", "--verify", "--quiet", "refs/heads/alternate"])
        .assert()
        .code(1);
    assert!(ctx.remote_ref_oid(&format!("refs/heads/{id}")).is_none());
    assert!(ctx.github().requests().is_empty());
}

#[test]
fn unmanaged_installed_hook_preserves_an_ordinary_origin_push() {
    let ctx = testutil::test_context!()
        .with_remote()
        .with_installed_hooks()
        .with_initial_commit()
        .build();
    ctx.checkout_new("ordinary-unmanaged");
    ctx.gherrit_cmd().arg("unmanage").assert().success();
    ctx.commit("Ordinary unmanaged change");
    let head = ctx.head_oid();

    ctx.git_cmd().args(["push", "origin", "ordinary-unmanaged"]).assert().success();

    assert_eq!(ctx.remote_ref_oid("refs/heads/ordinary-unmanaged").as_deref(), Some(head.as_str()));
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
    install_composite_pre_push(
        &ctx,
        "count=0\n\
         while IFS= read -r line; do\n\
         count=$((count + 1))\n\
         done\n\
         printf 'policy:%s:%s\\n' \"$1\" \"$count\" >> \"$GHERRIT_HOOK_LOG\"",
    );

    ctx.checkout_new("composite-boundary");
    ctx.commit("Feature through composite hook");

    ctx.git_cmd().env("GHERRIT_HOOK_LOG", &log).arg("push").assert().success();

    assert_eq!(
        fs::read_to_string(log).unwrap(),
        concat!(
            "enter:.\n",
            "enter:gherrit-publication\n",
            "policy:gherrit-publication:3\n",
            "enter:gherrit-publication\n",
            "policy:gherrit-publication:1\n",
            "policy:.:0\n",
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

    ctx.git_cmd().env("GHERRIT_HOOK_LOG", &log).arg("push").assert().failure();

    assert_eq!(fs::read_to_string(&log).unwrap(), "enter:.\nenter:gherrit-publication\n");
    assert_eq!(ctx.remote_ref_oid(&format!("refs/heads/{id}")), None);
    assert_eq!(ctx.remote_ref_oid("refs/heads/rejected-composite-boundary"), None);
    assert!(ctx.github().pull_requests().is_empty());

    install_composite_pre_push(&ctx, "printf 'policy:%s\\n' \"$1\" >> \"$GHERRIT_HOOK_LOG\"");
    ctx.git_cmd().env("GHERRIT_HOOK_LOG", &log).arg("push").assert().success();
    assert!(ctx.remote_ref_oid(&format!("refs/tags/gherrit/{id}/pr")).is_some());
    assert!(ctx.remote_ref_oid("refs/heads/rejected-composite-boundary").is_none());
    assert_eq!(ctx.github().pull_requests().len(), 1);
}

#[test]
fn later_composite_check_can_reject_the_outer_push_after_publication() {
    let ctx = testutil::test_context!()
        .with_remote()
        .with_installed_hooks()
        .with_initial_commit()
        .with_mock_github()
        .build();
    let log = ctx.dir.path().join("pre-push.log");
    install_composite_pre_push(
        &ctx,
        "if [ \"$1\" = . ]; then\n\
         exit 73\n\
         fi",
    );
    ctx.checkout_new("outer-policy-boundary");
    ctx.commit("Publish before a later outer policy rejects");
    let id = ctx.gherrit_id("HEAD").unwrap();

    ctx.git_cmd().env("GHERRIT_HOOK_LOG", &log).arg("push").assert().failure().code(1);

    assert!(ctx.remote_ref_oid(&format!("refs/heads/{id}")).is_some());
    assert!(ctx.remote_ref_oid(&format!("refs/heads/gherrit-bases/{id}")).is_some());
    assert!(ctx.remote_ref_oid(&format!("refs/tags/gherrit/{id}/v1")).is_some());
    assert!(ctx.remote_ref_oid(&format!("refs/tags/gherrit/{id}/pr")).is_some());
    assert_eq!(ctx.remote_ref_oid("refs/heads/outer-policy-boundary"), None);
    assert_eq!(ctx.github().pull_requests().len(), 1);

    let refs = ctx.remote_refs("refs");
    install_composite_pre_push(&ctx, ":");
    ctx.git_cmd().env("GHERRIT_HOOK_LOG", &log).arg("push").assert().success();
    assert_eq!(ctx.remote_refs("refs"), refs);
    assert_eq!(ctx.github().pull_requests().len(), 1);
}

#[test]
fn second_internal_hook_rejection_leaves_a_safe_prefix_for_retry() {
    let ctx = testutil::test_context!()
        .with_remote()
        .with_installed_hooks()
        .with_initial_commit()
        .with_mock_github()
        .build();
    let log = ctx.dir.path().join("pre-push.log");
    install_composite_pre_push(
        &ctx,
        "if [ \"$1\" = gherrit-publication ]; then\n\
         count=$(grep -c '^enter:gherrit-publication$' \"$GHERRIT_HOOK_LOG\")\n\
         [ \"$count\" -ne 2 ] || exit 73\n\
         fi",
    );
    ctx.checkout_new("second-rejected-boundary");
    ctx.commit("Reject only the marker barrier once");
    let id = ctx.gherrit_id("HEAD").unwrap();

    ctx.git_cmd().env("GHERRIT_HOOK_LOG", &log).arg("push").assert().failure();

    assert!(ctx.remote_ref_oid(&format!("refs/heads/{id}")).is_some());
    assert!(ctx.remote_ref_oid(&format!("refs/heads/gherrit-bases/{id}")).is_some());
    assert!(ctx.remote_ref_oid(&format!("refs/tags/gherrit/{id}/v1")).is_some());
    assert!(ctx.remote_ref_oid(&format!("refs/tags/gherrit/{id}/pr")).is_none());
    assert_eq!(ctx.remote_ref_oid("refs/heads/second-rejected-boundary"), None);
    let pull_requests = ctx.github().pull_requests();
    assert_eq!(pull_requests.len(), 1);
    assert_eq!(pull_requests[0].base, format!("gherrit-bases/{id}"));

    ctx.git_cmd().env("GHERRIT_HOOK_LOG", &log).arg("push").assert().success();

    assert!(ctx.remote_ref_oid(&format!("refs/tags/gherrit/{id}/pr")).is_some());
    assert!(ctx.remote_ref_oid("refs/heads/second-rejected-boundary").is_none());
    assert_eq!(ctx.github().pull_requests()[0].base, "main");
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
        .arg("push")
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

    ctx.git_cmd().env("GIT_GRAFT_FILE", "custom-grafts").arg("push").assert().success();
    assert!(ctx.remote_ref_oid("refs/heads/main").is_some());
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

    ctx.git_cmd().env("GIT_GRAFT_FILE", "custom-grafts").arg("push").assert().failure().stderr(
        predicate::str::contains(
            "file named by GIT_GRAFT_FILE is nonempty because the enclosing Git push retains",
        ),
    );

    assert_eq!(ctx.remote_ref_oid("refs/heads/custom-graft-boundary"), None);
    assert_eq!(ctx.remote_ref_oid(&format!("refs/heads/{id}")), None);
}

#[test]
fn installed_pre_push_rejects_a_public_branch_that_could_be_a_change_id() {
    let ctx = testutil::test_context!()
        .with_remote()
        .with_installed_hooks()
        .with_initial_commit()
        .with_mock_github()
        .build();
    ctx.checkout_managed_public("Gfirst");
    ctx.commit_with_explicit_gherrit_id("First change", "Gfirst");
    ctx.commit_with_explicit_gherrit_id("Second change", "Gsecond");

    ctx.git_cmd()
        .arg("push")
        .assert()
        .failure()
        .stderr(predicate::str::contains("cannot collide with a change-owned head"));

    assert!(ctx.remote_ref_oid("refs/heads/Gfirst").is_none());
    assert!(ctx.remote_ref_oid("refs/heads/Gsecond").is_none());
    assert!(ctx.remote_refs("refs/tags/gherrit").is_empty());
    assert!(ctx.github().pull_requests().is_empty());
    assert!(ctx.github().requests().is_empty());
}

#[test]
fn installed_pre_push_rejects_a_public_branch_in_the_owned_base_namespace() {
    let ctx = testutil::test_context!()
        .with_remote()
        .with_installed_hooks()
        .with_initial_commit()
        .with_mock_github()
        .build();
    ctx.checkout_managed_public("gherrit-bases/Gfirst");
    ctx.commit_with_explicit_gherrit_id("First change", "Gfirst");

    ctx.git_cmd()
        .arg("push")
        .assert()
        .failure()
        .stderr(predicate::str::contains("reserved 'gherrit-bases' namespace"));

    assert!(ctx.remote_ref_oid("refs/heads/Gfirst").is_none());
    assert!(ctx.remote_ref_oid("refs/heads/gherrit-bases/Gfirst").is_none());
    assert!(ctx.remote_refs("refs/tags/gherrit").is_empty());
    assert!(ctx.github().pull_requests().is_empty());
    assert!(ctx.github().requests().is_empty());
}

#[cfg(unix)]
#[test]
fn installed_pre_push_accepts_an_unmanaged_non_utf8_remote_location() {
    use std::{ffi::OsString, os::unix::ffi::OsStringExt as _};

    let ctx = testutil::test_context!().with_installed_hooks().with_initial_commit().build();
    let location = OsString::from_vec(b"remote-\xff.git".to_vec());

    ctx.installed_hook_cmd("pre-push").arg("raw").arg(location).assert().success();
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

    ctx.git_cmd().env("GIT_SHALLOW_FILE", "custom-shallow").arg("push").assert().failure().stderr(
        predicate::str::contains(
            "file named by GIT_SHALLOW_FILE is nonempty because the enclosing Git push retains",
        ),
    );

    assert_eq!(ctx.remote_ref_oid("refs/heads/custom-shallow-boundary"), None);
    assert_eq!(ctx.remote_ref_oid(&format!("refs/heads/{id}")), None);
}

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

fn checkout_from_main(ctx: &testutil::TestContext, branch: &str) {
    ctx.run_git(&["checkout", "main"]);
    ctx.checkout_new(branch);
}

#[test]
fn installed_hook_publishes_private_public_empty_and_dry_run_intent() {
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

    checkout_from_main(&ctx, "public-boundary");
    ctx.gherrit_cmd().args(["manage", "--public"]).assert().success();
    ctx.assert_config("branch.public-boundary.pushRemote", Some("."));
    ctx.commit("Public feature through installed hook");
    let id = ctx.gherrit_id("HEAD").unwrap();
    let head = ctx.head_oid();

    ctx.git_cmd().arg("push").assert().success();

    assert_eq!(ctx.remote_ref_oid("refs/heads/public-boundary").as_deref(), Some(head.as_str()));
    let public = ctx
        .github()
        .pull_requests()
        .into_iter()
        .find(|pull_request| pull_request.head == id)
        .unwrap();
    assert!(public.body.contains("[public\\-boundary](/owner/repo/tree/public-boundary)"));

    checkout_from_main(&ctx, "empty-public");
    ctx.gherrit_cmd().args(["manage", "--public"]).assert().success();
    let head = ctx.head_oid();
    let refs_before = ctx.remote_refs("refs");

    ctx.git_cmd().arg("push").assert().success();

    assert_eq!(ctx.remote_ref_oid("refs/heads/empty-public").as_deref(), Some(head.as_str()));
    let mut expected_refs = refs_before;
    expected_refs.push("refs/heads/empty-public".to_owned());
    expected_refs.sort();
    assert_eq!(ctx.remote_refs("refs"), expected_refs);
    assert_eq!(ctx.github().pull_requests().len(), 2);

    checkout_from_main(&ctx, "dry-run-boundary");
    ctx.commit("Publish despite the enclosing dry run");
    let id = ctx.gherrit_id("HEAD").unwrap();
    let head = ctx.head_oid();

    ctx.git_cmd().args(["push", "--dry-run"]).assert().success();

    assert_eq!(ctx.remote_ref_oid(&format!("refs/heads/{id}")).as_deref(), Some(head.as_str()));
    assert_eq!(ctx.remote_ref_oid("refs/heads/dry-run-boundary"), None);
    assert_eq!(ctx.github().pull_requests().len(), 3);
}

#[test]
fn installed_hook_preserves_git_protocol_arguments_and_input() {
    let ctx = testutil::test_context!()
        .with_remote()
        .with_installed_hooks()
        .with_initial_commit()
        .with_mock_github()
        .build();
    ctx.checkout_new("hook-input-boundary");
    ctx.commit("Exercise the installed hook boundary");
    let id = ctx.gherrit_id("HEAD").unwrap();
    let head = ctx.head_oid();

    // The remote main branch is already current, so this invocation has no
    // ref-update records. The hook must still forward and reject its argv.
    ctx.git_cmd()
        .args(["push", "origin", "main"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("must use the local no-op destination"));

    ctx.git_cmd()
        .args(["push", "origin", "hook-input-boundary"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("must use the local no-op destination"));
    assert_eq!(ctx.remote_ref_oid("refs/heads/hook-input-boundary"), None);

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

    ctx.gherrit_cmd().arg("unmanage").assert().success();
    #[cfg(unix)]
    {
        use std::{ffi::OsString, os::unix::ffi::OsStringExt as _};

        let location = OsString::from_vec(b"remote-\xff.git".to_vec());
        ctx.installed_hook_cmd("pre-push").arg("raw").arg(location).assert().success();
    }

    let log = ctx.dir.path().join("pre-push.log");
    let input = ctx.dir.path().join("pre-push.input");
    install_composite_pre_push(&ctx, "tee \"$GHERRIT_HOOK_INPUT\" >/dev/null");
    ctx.git_cmd()
        .env("GHERRIT_HOOK_LOG", &log)
        .env("GHERRIT_HOOK_INPUT", &input)
        .args(["push", "origin", "hook-input-boundary"])
        .assert()
        .success();

    assert_eq!(
        ctx.remote_ref_oid("refs/heads/hook-input-boundary").as_deref(),
        Some(head.as_str())
    );
    assert_eq!(fs::read_to_string(log).unwrap(), "enter:origin\n");
    assert_eq!(
        fs::read_to_string(input).unwrap(),
        format!(
            "refs/heads/hook-input-boundary {head} refs/heads/hook-input-boundary {}\n",
            "0".repeat(head.len())
        )
    );
}

#[test]
fn composite_hook_observes_complete_internal_pushes_and_chatty_output() {
    let ctx = testutil::test_context!()
        .with_remote()
        .with_installed_hooks()
        .with_initial_commit()
        .with_mock_github()
        .build();
    let log = ctx.dir.path().join("pre-push.log");
    install_composite_pre_push(
        &ctx,
        "printf 'policy accepted for %s\\n' \"$1\"\n\
         while IFS= read -r line; do\n\
         printf 'record:%s:%s\\n' \"$1\" \"$line\" >> \"$GHERRIT_HOOK_LOG\"\n\
         done\n\
         printf 'policy:%s\\n' \"$1\" >> \"$GHERRIT_HOOK_LOG\"",
    );

    ctx.checkout_new("composite-boundary");
    ctx.commit("Feature through a chatty composite hook");
    let id = ctx.gherrit_id("HEAD").unwrap();
    let head = ctx.head_oid();
    let base = ctx.remote_ref_oid("refs/heads/main").unwrap();
    let null = "0".repeat(head.len());

    ctx.git_cmd().env("GHERRIT_HOOK_LOG", &log).arg("push").assert().success();

    assert!(ctx.remote_ref_oid(&format!("refs/heads/{id}")).is_some());
    assert!(ctx.remote_ref_oid(&format!("refs/tags/gherrit/{id}/pr")).is_some());
    assert_eq!(ctx.github().pull_requests().len(), 1);
    assert_eq!(
        fs::read_to_string(log).unwrap(),
        format!(
            "enter:.\n\
             enter:gherrit-publication\n\
             record:gherrit-publication:{head} {head} refs/heads/{id} {null}\n\
             record:gherrit-publication:{base} {base} refs/heads/gherrit-bases/{id} {null}\n\
             record:gherrit-publication:{head} {head} refs/tags/gherrit/{id}/v1 {null}\n\
             policy:gherrit-publication\n\
             enter:gherrit-publication\n\
             record:gherrit-publication:{head} {head} refs/tags/gherrit/{id}/pr {null}\n\
             policy:gherrit-publication\n\
             policy:.\n"
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
         gherrit-publication*)\n\
           printf 'independent policy denied this publication\\n' >&2\n\
           exit 73\n\
           ;;\n\
         esac",
    );

    ctx.checkout_new("rejected-composite-boundary");
    ctx.commit("Feature rejected by independent hook");
    let id = ctx.gherrit_id("HEAD").unwrap();

    let private_destination = ctx
        .git_cmd()
        .args(["remote", "get-url", "--push", "origin"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let private_destination = String::from_utf8(private_destination).unwrap();
    let stderr = ctx
        .git_cmd()
        .env("GHERRIT_HOOK_LOG", &log)
        .arg("push")
        .assert()
        .failure()
        .get_output()
        .stderr
        .clone();
    let stderr = String::from_utf8_lossy(&stderr);
    assert!(stderr.contains("independent policy denied this publication"), "{stderr}");
    assert!(!stderr.contains(private_destination.trim()), "{stderr}");

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
fn installed_hook_enforces_local_history_preconditions() {
    let ctx = testutil::test_context!()
        .with_remote()
        .with_installed_hooks()
        .with_initial_commit()
        .build();

    ctx.checkout_new("autosquash-boundary");
    ctx.commit("Work in progress");
    let autosquash_id = ctx.gherrit_id("HEAD").unwrap();
    ctx.commit("fixup! Work in progress");
    let refs_before = ctx.remote_refs("refs");

    ctx.git_cmd()
        .arg("push")
        .assert()
        .failure()
        .stderr(predicate::str::contains("Stack contains pending fixup/squash/amend commits"));

    assert_eq!(ctx.remote_refs("refs"), refs_before);
    assert_eq!(ctx.remote_ref_oid("refs/heads/autosquash-boundary"), None);
    assert_eq!(ctx.remote_ref_oid(&format!("refs/heads/{autosquash_id}")), None);

    ctx.run_git(&["checkout", "main"]);
    ctx.gherrit_cmd().args(["manage", "--private"]).assert().success();
    fs::write(ctx.repo_path.join("custom-grafts"), format!("{}\n", ctx.head_oid())).unwrap();
    ctx.git_cmd().env("GIT_GRAFT_FILE", "custom-grafts").arg("push").assert().success();
    assert_eq!(ctx.remote_refs("refs"), refs_before);

    checkout_from_main(&ctx, "custom-graft-boundary");
    ctx.commit("Feature with custom graft environment");
    let graft_id = ctx.gherrit_id("HEAD").unwrap();
    fs::write(ctx.repo_path.join("custom-grafts"), format!("{}\n", ctx.head_oid())).unwrap();
    ctx.git_cmd().env("GIT_GRAFT_FILE", "custom-grafts").arg("push").assert().failure().stderr(
        predicate::str::contains(
            "file named by GIT_GRAFT_FILE is nonempty because the enclosing Git push retains",
        ),
    );
    assert_eq!(ctx.remote_refs("refs"), refs_before);
    assert_eq!(ctx.remote_ref_oid(&format!("refs/heads/{graft_id}")), None);

    checkout_from_main(&ctx, "custom-shallow-boundary");
    ctx.commit("Feature with custom shallow environment");
    let shallow_id = ctx.gherrit_id("HEAD").unwrap();
    fs::write(ctx.repo_path.join("custom-shallow"), format!("{}\n", ctx.head_oid())).unwrap();
    ctx.git_cmd().env("GIT_SHALLOW_FILE", "custom-shallow").arg("push").assert().failure().stderr(
        predicate::str::contains(
            "file named by GIT_SHALLOW_FILE is nonempty because the enclosing Git push retains",
        ),
    );
    assert_eq!(ctx.remote_refs("refs"), refs_before);
    assert_eq!(ctx.remote_ref_oid(&format!("refs/heads/{shallow_id}")), None);
}

#[test]
fn installed_hook_rejects_public_names_that_overlap_owned_refs() {
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

    ctx.run_git(&["checkout", "main"]);
    ctx.checkout_managed_public("gherrit-bases/Gthird");
    ctx.commit_with_explicit_gherrit_id("Third change", "Gthird");

    ctx.git_cmd()
        .arg("push")
        .assert()
        .failure()
        .stderr(predicate::str::contains("reserved 'gherrit-bases' namespace"));

    assert!(ctx.remote_ref_oid("refs/heads/Gthird").is_none());
    assert!(ctx.remote_ref_oid("refs/heads/gherrit-bases/Gthird").is_none());
    assert!(ctx.remote_refs("refs/tags/gherrit").is_empty());
    assert!(ctx.github().pull_requests().is_empty());
    assert!(ctx.github().requests().is_empty());
}

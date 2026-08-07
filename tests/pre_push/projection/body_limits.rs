use std::{
    io::Write as _,
    process::{Command, Stdio},
};

use predicates::prelude::*;

fn context() -> testutil::TestContext {
    testutil::test_context!()
        .with_remote()
        .with_installed_hooks()
        .with_initial_commit()
        .with_mock_github()
        .with_git_interceptor()
        .build()
}

fn remote_path(ctx: &testutil::TestContext) -> std::path::PathBuf {
    ctx.dir.path().join(testutil::DEFAULT_OWNER).join(format!("{}.git", testutil::DEFAULT_REPO))
}

fn create_remote_version_range(
    ctx: &testutil::TestContext,
    gherrit_id: &str,
    first: usize,
    last: usize,
    oid: &str,
) {
    let mut child = Command::new(&ctx.system_git)
        .arg("--git-dir")
        .arg(remote_path(ctx))
        .args(["update-ref", "--stdin"])
        .stdin(Stdio::piped())
        .spawn()
        .expect("spawn git update-ref --stdin");
    {
        let stdin = child.stdin.as_mut().expect("update-ref stdin");
        for version in first..=last {
            writeln!(stdin, "create refs/tags/gherrit/{gherrit_id}/v{version} {oid}")
                .expect("write update-ref command");
        }
    }
    let status = child.wait().expect("wait for git update-ref");
    assert!(status.success(), "git update-ref --stdin failed: {status}");
}

#[test]
fn high_version_history_remains_bounded_and_projectable() {
    let ctx = context();
    ctx.checkout_new("bounded-history");
    ctx.commit("Long-lived change");
    ctx.hook_cmd("pre-push").assert().success();

    let id = ctx.gherrit_id("HEAD").unwrap();
    let old_head = ctx.remote_ref_oid(&format!("refs/heads/{id}")).unwrap();
    create_remote_version_range(&ctx, &id, 2, 499, &old_head);
    ctx.amend();

    ctx.hook_cmd("pre-push").assert().success();
    assert_eq!(
        ctx.remote_ref_oid(&format!("refs/tags/gherrit/{id}/v500")).as_deref(),
        Some(ctx.head_oid().as_str())
    );
    ctx.inspect_mock_state(|state| {
        let body = state.prs[0].body.as_deref().expect("projected body");
        assert!(body.len() < 60_000);
        assert!(body.contains("**Latest Update:** v500"));
        assert!(body.contains("Showing the latest 32 of 500 patch versions"));
        assert!(!body.contains("| v499 | v498 |"));
    });
}

#[test]
fn oversized_final_body_fails_before_remote_or_github_mutation() {
    let ctx = context();
    ctx.checkout_new("oversized-body");
    let message = ctx.dir.path().join("oversized-message.txt");
    std::fs::write(&message, format!("Oversized body\n\n{}", "x".repeat(60_100))).unwrap();
    ctx.git_cmd().args(["commit", "--allow-empty", "-F"]).arg(&message).assert().success();

    let refs = ctx.remote_refs("refs");
    ctx.hook_cmd("pre-push")
        .assert()
        .failure()
        .stderr(predicate::str::contains("conservative 60000-byte limit"));
    assert_eq!(ctx.remote_refs("refs"), refs);
    ctx.inspect_mock_state(|state| {
        assert!(state.prs.is_empty());
        assert!(state.pushes.is_empty());
    });
}

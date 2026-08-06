use std::path::PathBuf;

use predicates::prelude::*;
use testutil::mock_server::{MockPrArgs, MockRepositoryIdentity, PrEntry};

fn context() -> testutil::TestContext {
    testutil::test_context!()
        .with_remote()
        .with_installed_hooks()
        .with_initial_commit()
        .with_mock_github()
        .with_git_interceptor()
        .build()
}

fn create_change(ctx: &testutil::TestContext) -> String {
    ctx.checkout_new("ownership-feature");
    ctx.commit("Owned change");
    let id = ctx.gherrit_id("HEAD").unwrap();
    ctx.hook_cmd("pre-push").assert().success();
    id
}

#[test]
fn refuses_to_overwrite_an_unowned_id_shaped_branch() {
    let ctx = context();
    ctx.checkout_new("ownership-collision");
    ctx.commit("Local change");
    let id = ctx.gherrit_id("HEAD").unwrap();

    ctx.run_git(&["checkout", "-b", "unrelated", "main"]);
    ctx.git_cmd()
        .args(["commit", "--allow-empty", "--no-verify", "-m", "Unrelated branch"])
        .assert()
        .success();
    ctx.git_cmd()
        .arg("push")
        .args(["--quiet", "--no-verify", "origin"])
        .arg(format!("HEAD:refs/heads/{id}"))
        .assert()
        .success();
    let unowned_oid = ctx.remote_ref_oid(&format!("refs/heads/{id}")).unwrap();
    ctx.run_git(&["checkout", "ownership-collision"]);

    ctx.hook_cmd("pre-push")
        .assert()
        .failure()
        .stderr(predicate::str::contains("not provably GHerrit-owned"));
    assert_eq!(
        ctx.remote_ref_oid(&format!("refs/heads/{id}")).as_deref(),
        Some(unowned_oid.as_str())
    );
}

#[test]
fn refuses_a_canonical_pr_with_mismatched_terminal_metadata() {
    let ctx = context();
    let id = create_change(&ctx);
    let other = "Gabcdefghijklmnopqrstuvwxyz234567";
    assert_ne!(id, other);

    ctx.mutate_mock_state(|state| {
        let pr = state.prs.iter_mut().find(|pr| pr.head.ref_field == id).unwrap();
        let body = pr.body.as_mut().unwrap();
        *body = body.replace(&format!("\"id\":\"{id}\""), &format!("\"id\":\"{other}\""));
    });
    let remote_oid = ctx.remote_ref_oid(&format!("refs/heads/{id}")).unwrap();
    ctx.amend();

    ctx.hook_cmd("pre-push")
        .assert()
        .failure()
        .stderr(predicate::str::contains("disagrees with terminal GHerrit metadata ID"));
    assert_eq!(
        ctx.remote_ref_oid(&format!("refs/heads/{id}")).as_deref(),
        Some(remote_oid.as_str())
    );
}

#[test]
fn ignores_fork_and_deleted_repository_candidates_when_selecting_the_canonical_pr() {
    let ctx = context();
    let id = create_change(&ctx);

    ctx.mutate_mock_state(|state| {
        let mut fork = PrEntry::mock(MockPrArgs {
            id: 100,
            title: "Fork collision".to_string(),
            body: String::new(),
            head: id.clone(),
            base: "main".to_string(),
            repo_owner: "fork-owner",
            repo_name: testutil::DEFAULT_REPO,
        });
        fork.head_repository = Some(MockRepositoryIdentity {
            id: "FORK_REPO_NODE_ID".to_string(),
            name_with_owner: format!("fork-owner/{}", testutil::DEFAULT_REPO),
        });
        fork.is_cross_repository = true;
        state.add_pr(fork);

        let mut deleted = PrEntry::mock(MockPrArgs {
            id: 101,
            title: "Deleted fork history".to_string(),
            body: String::new(),
            head: id.clone(),
            base: "main".to_string(),
            repo_owner: "deleted-owner",
            repo_name: testutil::DEFAULT_REPO,
        });
        deleted.state = "CLOSED".to_string();
        deleted.head_repository = None;
        deleted.is_cross_repository = true;
        state.add_pr(deleted);
    });

    ctx.amend();
    ctx.hook_cmd("pre-push").assert().success();
    ctx.inspect_mock_state(|state| {
        assert_eq!(state.prs.iter().filter(|pr| pr.head.ref_field == id && pr.id < 100).count(), 1);
    });
}

#[test]
fn repository_lookup_and_canonical_selection_ignore_url_casing() {
    let ctx = context();
    let output = ctx.git_cmd().args(["remote", "get-url", "origin"]).output().unwrap();
    assert!(output.status.success());
    let original = PathBuf::from(String::from_utf8(output.stdout).unwrap().trim());
    let repo_parent = original.parent().unwrap();
    let root = repo_parent.parent().unwrap();
    let uppercase_parent = root.join(testutil::DEFAULT_OWNER.to_ascii_uppercase());
    std::fs::create_dir_all(&uppercase_parent).unwrap();
    let uppercase_remote =
        uppercase_parent.join(format!("{}.git", testutil::DEFAULT_REPO.to_ascii_uppercase()));
    #[cfg(unix)]
    std::os::unix::fs::symlink(&original, &uppercase_remote).unwrap();

    ctx.run_git(&["remote", "set-url", "origin", uppercase_remote.to_str().unwrap()]);
    ctx.checkout_new("case-insensitive-repository");
    ctx.commit("Case-insensitive repository identity");
    ctx.hook_cmd("pre-push").assert().success();
    ctx.amend();
    ctx.hook_cmd("pre-push").assert().success();
}

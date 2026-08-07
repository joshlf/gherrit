use predicates::prelude::*;

fn context() -> testutil::TestContext {
    testutil::test_context!()
        .with_remote()
        .with_installed_hooks()
        .with_initial_commit()
        .with_mock_github()
        .build()
}

#[test]
fn remote_default_oid_bounds_local_intent() {
    let ctx = context();

    // This local-only main commit must not silently become part of the root PR.
    ctx.commit("Local unreviewed default change");
    ctx.checkout_new("local-default-ahead");
    ctx.commit("Reviewed feature change");

    let refs = ctx.remote_refs("refs/heads/G");
    ctx.hook_cmd("pre-push")
        .assert()
        .failure()
        .stderr(predicate::str::contains("missing a non-empty gherrit-pr-id trailer"));
    assert_eq!(ctx.remote_refs("refs/heads/G"), refs);
}

#[test]
fn rejects_when_remote_default_is_not_an_ancestor() {
    let ctx = context();
    ctx.checkout_new("diverged-feature");
    ctx.commit("Feature change");

    ctx.run_git(&["checkout", "main"]);
    ctx.commit("Remote-only main advance");
    ctx.git_cmd()
        .args(["push", "--quiet", "--no-verify", "origin", "main:main"])
        .assert()
        .success();
    ctx.run_git(&["checkout", "diverged-feature"]);

    ctx.hook_cmd("pre-push")
        .assert()
        .failure()
        .stderr(predicate::str::contains("is not an ancestor"))
        .stderr(predicate::str::contains("refuses to substitute a local or stale"));
}

#[test]
fn follows_remote_head_across_rename_and_stale_local_origin_head() {
    let ctx = context();
    let main_oid = ctx.remote_ref_oid("refs/heads/main").unwrap();
    ctx.remote_git_cmd().args(["update-ref", "refs/heads/trunk", &main_oid]).assert().success();
    ctx.remote_git_cmd().args(["symbolic-ref", "HEAD", "refs/heads/trunk"]).assert().success();
    ctx.remote_git_cmd().args(["update-ref", "-d", "refs/heads/main"]).assert().success();

    // Deliberately make the local remote-HEAD hint stale and delete the local
    // branch that used to be the default. Neither is authoritative.
    ctx.git_cmd()
        .args(["symbolic-ref", "refs/remotes/origin/HEAD", "refs/remotes/origin/main"])
        .assert()
        .success();
    ctx.checkout_new("renamed-default");
    ctx.git_cmd().args(["branch", "-D", "main"]).assert().success();
    ctx.commit("Feature on renamed default");

    ctx.hook_cmd("pre-push").assert().success();
    ctx.inspect_mock_state(|state| {
        assert_eq!(state.prs.len(), 1);
        assert_eq!(state.prs[0].base.ref_field, "trunk");
    });
}

#[test]
fn rejects_shallow_replace_and_graft_dags_before_mutation() {
    for mode in ["shallow", "replace", "graft"] {
        let ctx = context();
        ctx.checkout_new(&format!("dag-{mode}"));
        ctx.commit("Feature change");

        match mode {
            "shallow" => {
                std::fs::write(ctx.repo_path.join(".git/shallow"), format!("{}\n", ctx.head_oid()))
                    .unwrap();
            }
            "replace" => {
                ctx.git_cmd().args(["replace", "HEAD", "HEAD^"]).assert().success();
            }
            "graft" => {
                std::fs::write(ctx.repo_path.join(".git/info/grafts"), "deadbeef\n").unwrap();
            }
            _ => unreachable!(),
        }

        let expected = match mode {
            "shallow" => "shallow repository",
            "replace" => "replace refs",
            "graft" => ".git/info/grafts",
            _ => unreachable!(),
        };
        ctx.hook_cmd("pre-push").assert().failure().stderr(predicate::str::contains(expected));
        assert!(ctx.remote_refs("refs/heads/G").is_empty());
    }
}

#[test]
fn rejects_common_grafts_from_a_linked_worktree_before_mutation() {
    let ctx = context();
    let worktree = ctx.dir.path().join("linked-worktree");
    let worktree_arg = worktree.to_str().unwrap();
    ctx.git_cmd()
        .args(["worktree", "add", "-b", "linked-graft", worktree_arg, "main"])
        .assert()
        .success();

    ctx.manage_cmd().current_dir(&worktree).assert().success();
    ctx.git_cmd()
        .current_dir(&worktree)
        .args(["commit", "--allow-empty", "-m", "Feature from linked worktree"])
        .assert()
        .success();
    std::fs::write(ctx.repo_path.join(".git/info/grafts"), "deadbeef\n").unwrap();

    ctx.hook_cmd("pre-push")
        .current_dir(&worktree)
        .assert()
        .failure()
        .stderr(predicate::str::contains(".git/info/grafts"));
    assert!(ctx.remote_refs("refs/heads/G").is_empty());
}

#[test]
fn rejects_a_pushurl_that_targets_a_different_repository_before_mutation() {
    let ctx = context();
    ctx.checkout_new("different-pushurl");
    ctx.commit("Feature change");

    let other = ctx.dir.path().join("other-owner").join("other-repository.git");
    std::fs::create_dir_all(other.parent().unwrap()).unwrap();
    ctx.init_bare_repo(&other);
    ctx.git_cmd()
        .arg("push")
        .args(["--quiet", "--no-verify"])
        .arg(&other)
        .arg("main:refs/heads/main")
        .assert()
        .success();
    ctx.git_cmd().args(["remote", "set-url", "--push", "origin"]).arg(&other).assert().success();

    let original_refs = ctx.remote_refs("refs");
    let other_refs = ctx
        .git_cmd()
        .arg("--git-dir")
        .arg(&other)
        .args(["for-each-ref", "--format=%(refname)", "refs"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();

    ctx.hook_cmd("pre-push")
        .assert()
        .failure()
        .stderr(predicate::str::contains("different Git authorities"));
    assert_eq!(ctx.remote_refs("refs"), original_refs);
    assert_eq!(
        ctx.git_cmd()
            .arg("--git-dir")
            .arg(&other)
            .args(["for-each-ref", "--format=%(refname)", "refs"])
            .assert()
            .success()
            .get_output()
            .stdout
            .clone(),
        other_refs
    );
}

#[test]
fn rejects_multiple_pushurls_before_mutation() {
    let ctx = context();
    ctx.checkout_new("multiple-pushurls");
    ctx.commit("Feature change");

    let first = ctx.dir.path().join("first.git");
    let second = ctx.dir.path().join("second.git");
    ctx.init_bare_repo(&first);
    ctx.init_bare_repo(&second);
    ctx.git_cmd().args(["remote", "set-url", "--push", "origin"]).arg(&first).assert().success();
    ctx.git_cmd()
        .args(["remote", "set-url", "--add", "--push", "origin"])
        .arg(&second)
        .assert()
        .success();

    let original_refs = ctx.remote_refs("refs");
    ctx.hook_cmd("pre-push")
        .assert()
        .failure()
        .stderr(predicate::str::contains("effective push URLs"))
        .stderr(predicate::str::contains("multi-destination"));
    assert_eq!(ctx.remote_refs("refs"), original_refs);
}

#[test]
fn accepts_equivalent_file_and_path_urls_for_one_repository() {
    let ctx = context();
    let remote = ctx
        .dir
        .path()
        .join(testutil::DEFAULT_OWNER)
        .join(format!("{}.git", testutil::DEFAULT_REPO));
    ctx.git_cmd()
        .args(["remote", "set-url", "origin"])
        .arg(format!("file://{}", remote.display()))
        .assert()
        .success();
    ctx.git_cmd().args(["remote", "set-url", "--push", "origin"]).arg(&remote).assert().success();

    ctx.checkout_new("equivalent-remote-urls");
    ctx.commit("Feature change");
    let id = ctx.gherrit_id("HEAD").unwrap();
    let head = ctx.head_oid();
    ctx.hook_cmd("pre-push").assert().success();
    assert_eq!(ctx.remote_ref_oid(&format!("refs/heads/{id}")).as_deref(), Some(head.as_str()));
}

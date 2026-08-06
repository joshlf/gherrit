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
    ctx.remote_git_cmd()
        .args(["update-ref", "refs/heads/trunk", &main_oid])
        .assert()
        .success();
    ctx.remote_git_cmd()
        .args(["symbolic-ref", "HEAD", "refs/heads/trunk"])
        .assert()
        .success();
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
        ctx.hook_cmd("pre-push")
            .assert()
            .failure()
            .stderr(predicate::str::contains(expected));
        assert!(ctx.remote_refs("refs/heads/G").is_empty());
    }
}

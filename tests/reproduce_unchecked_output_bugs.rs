#[test]
fn test_pre_push_ls_remote_failure() {
    let ctx = testutil::test_context!().build();
    // Manage branch
    ctx.checkout_new("feature-ls-remote-fail");
    ctx.commit("Work");

    // Hook should succeed but warn about ls-remote failure
    ctx.gherrit()
        .args(["hook", "pre-push"])
        .env("MOCK_BIN_FAIL_CMD", "git:ls-remote")
        .assert()
        .success()
        .stderr(predicates::str::contains("Failed to fetch remote branch states"));
}

#[test]
fn test_pre_push_pr_list_failure() {
    let ctx = testutil::test_context!().build();
    ctx.checkout_new("feature-pr-list-fail");
    ctx.commit("Work");

    // Trigger hook
    ctx.inject_failure("graphql", 5);

    ctx.gherrit().args(["hook", "pre-push"]).assert().failure();
}

#[test]
fn test_pre_push_pr_create_failure() {
    let ctx = testutil::test_context!().build();
    ctx.checkout_new("feature-pr-create-fail");
    ctx.commit("Work");

    // Trigger hook
    ctx.inject_failure("create_pr", 5);

    ctx.gherrit().args(["hook", "pre-push"]).assert().failure();
}

#[test]
fn test_commit_msg_git_var_failure() {
    #[cfg(unix)]
    {
        let ctx = testutil::test_context_minimal!().install_hooks(true).build();
        ctx.gherrit().args(["manage"]).assert().success();

        let msg_file = ctx.repo_path.join("COMMIT_EDITMSG");
        std::fs::write(&msg_file, "feat: broken git var").unwrap();

        ctx.gherrit()
            .args(["hook", "commit-msg", msg_file.to_str().unwrap()])
            .env("MOCK_BIN_FAIL_CMD", "git:var")
            .assert()
            .failure();
    }
}

#[test]
fn test_commit_msg_trailers_failure() {
    #[cfg(unix)]
    {
        let ctx = testutil::test_context_minimal!().install_hooks(true).build();
        ctx.gherrit().args(["manage"]).assert().success();

        let msg_file = ctx.repo_path.join("COMMIT_EDITMSG");
        std::fs::write(&msg_file, "feat: broken trailers parse").unwrap();

        ctx.gherrit()
            .args(["hook", "commit-msg", msg_file.to_str().unwrap()])
            .env("MOCK_BIN_FAIL_CMD", "git:interpret-trailers")
            .assert()
            .failure();
    }
}

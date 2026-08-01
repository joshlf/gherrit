#[test]
fn test_special_characters_in_repo_url() {
    // Regression test for #180
    let scenarios = vec![
        // 1. User with hyphen
        ("user-name", "repo-normal"),
        // 2. User with underscore (technically invalid on GitHub, but tests parser robustness)
        ("user_name", "repo-normal"),
        // 3. User with period (technically invalid on GitHub, but tests parser robustness)
        ("user.name", "repo-normal"),
        // 4. Repo with hyphen
        ("user", "repo-name"),
        // 5. Repo with underscore
        ("user", "repo_name"),
        // 6. Repo with period
        ("user", "repo.name"),
    ];

    for (user, repo) in scenarios {
        println!("Testing scenario: {user}/{repo}");
        let ctx = testutil::test_context!()
            .repository(user, repo)
            .with_remote()
            .with_installed_hooks()
            .with_initial_commit()
            .with_mock_github()
            .build();

        ctx.checkout_new("feature-stack");

        // Manage must happen before commit to ensure the commit-msg hook adds the trailer
        ctx.manage_cmd().assert().success();

        ctx.run_git(&["commit", "--allow-empty", "-m", "Commit A"]);

        // Run pre-push hook
        // This fails if the regex doesn't match the generated URL
        ctx.hook_cmd("pre-push").assert().success();
    }
}

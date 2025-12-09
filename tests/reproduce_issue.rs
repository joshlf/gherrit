mod common;
use common::TestContext;

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
        let ctx = TestContext::init_with_repo(user, repo);
        ctx.install_hooks();

        // Setup a commit
        ctx.run_git(&["commit", "--allow-empty", "-m", "Initial Commit"]);
        ctx.run_git(&["checkout", "-b", "feature-stack"]);

        // Manage must happen before commit to ensure the commit-msg hook adds the trailer
        ctx.gherrit().args(["manage"]).assert().success();

        ctx.run_git(&["commit", "--allow-empty", "-m", "Commit A"]);

        // Run pre-push hook
        // This fails if the regex doesn't match the generated URL
        ctx.gherrit().args(["hook", "pre-push"]).assert().success();
    }
}

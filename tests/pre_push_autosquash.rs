// TODO: Review this file.

mod common;
use common::TestContext;
use predicates::prelude::*;

// Helper to assert the specific error message format
fn assert_autosquash_error(
    output: assert_cmd::assert::Assert,
    remote: &str,
    branch: &str,
) -> assert_cmd::assert::Assert {
    output
        .failure()
        .stderr(predicate::str::contains(
            "Stack contains pending fixup/squash/amend commits",
        ))
        .stderr(predicate::str::contains(format!(
            "git rebase -i --autosquash {}/{}",
            remote, branch
        )))
}

#[test]
fn test_autosquash_prefixes() {
    let ctx = TestContext::init_and_install_hooks();
    ctx.run_git(&["commit", "--allow-empty", "-m", "Initial"]);

    // Manage once
    ctx.gherrit().args(["manage"]).assert().success();

    let prefixes = ["fixup!", "squash!", "amend!"];

    for prefix in prefixes {
        // Create a new branch for each prefix test to ensure clean state
        let branch_name = format!("feature-{}", prefix.replace("!", ""));
        ctx.run_git(&["checkout", "-b", &branch_name, "main"]);

        // Normal commit
        ctx.run_git(&["commit", "--allow-empty", "-m", "Work in progress"]);

        // Prefix commit
        ctx.run_git(&[
            "commit",
            "--allow-empty",
            "-m",
            &format!("{} Work in progress", prefix),
        ]);

        ctx.gherrit().args(["manage"]).assert().success();

        let output = ctx.gherrit().args(["hook", "pre-push"]).assert();
        assert_autosquash_error(output, "origin", "main");
    }
}

#[test]
fn test_buried_autosquash_commit() {
    let ctx = TestContext::init_and_install_hooks();
    ctx.run_git(&["commit", "--allow-empty", "-m", "Initial"]);

    ctx.run_git(&["checkout", "-b", "feature-buried"]);

    // 1. Commit A
    ctx.run_git(&["commit", "--allow-empty", "-m", "Commit A"]);

    // 2. Commit B (Dirty)
    ctx.run_git(&["commit", "--allow-empty", "-m", "fixup! Commit A"]);

    // 3. Commit C (Normal)
    ctx.run_git(&["commit", "--allow-empty", "-m", "Commit C"]);

    ctx.gherrit().args(["manage"]).assert().success();

    let output = ctx.gherrit().args(["hook", "pre-push"]).assert();
    assert_autosquash_error(output, "origin", "main");
}

#[test]
fn test_precedence_over_trailer_check() {
    let ctx = TestContext::init_and_install_hooks();
    ctx.run_git(&["commit", "--allow-empty", "-m", "Initial"]);
    ctx.run_git(&["checkout", "-b", "feature-precedence"]);

    // Create a fixup commit that ALSO has a valid trailer.
    // Normally, a trailer makes a commit valid for Gherrit.
    // But 'fixup!' should still block it because it's considered "work in progress/to be squashed".
    let msg = "fixup! Some feature\n\ngherrit-pr-id: G12345";
    ctx.run_git(&["commit", "--allow-empty", "-m", msg]);

    ctx.gherrit().args(["manage"]).assert().success();

    let output = ctx.gherrit().args(["hook", "pre-push"]).assert();

    // Must fail with autosquash error, NOT missing trailer error
    let output = assert_autosquash_error(output, "origin", "main");

    let stderr = std::str::from_utf8(&output.get_output().stderr).unwrap();
    assert!(
        !stderr.contains("missing gherrit-pr-id trailer"),
        "Error should not be about missing trailer. Got: {}",
        stderr
    );
}

#[test]
fn test_dynamic_remote_and_branch_suggestion() {
    let ctx = TestContext::init();

    // Configure default branch to be 'master' to test dynamic branch name detection
    // Note: init_with_repo usually sets main, we can rename it.
    ctx.run_git(&["branch", "-m", "main", "master"]);
    ctx.run_git(&["symbolic-ref", "HEAD", "refs/heads/master"]);

    // Add custom remote 'upstream'
    // We need to actually point it somewhere valid for `rev-parse` checks if they happen,
    // but the error message generation just calls `default_remote_name`.
    // However, `gherrit` manages `origin` by default. We need to tell it to use `upstream`.
    let upstream_path = ctx.dir.path().join("upstream.git");
    common::init_git_bare_repo(&upstream_path);
    ctx.run_git(&["remote", "add", "upstream", upstream_path.to_str().unwrap()]);

    // Configure gherrit to use upstream
    ctx.run_git(&["config", "gherrit.remote", "upstream"]);

    // Install hooks manually since we didn't use init_and_install_hooks
    ctx.install_hooks();

    ctx.run_git(&["commit", "--allow-empty", "-m", "Initial"]);
    ctx.run_git(&["checkout", "-b", "feature-dynamic"]);

    // Create fixup
    ctx.run_git(&["commit", "--allow-empty", "-m", "fixup! Work"]);

    ctx.gherrit().args(["manage"]).assert().success();

    // The previous implementation of `collect_commits` calls `find_default_branch_on_default_remote`.
    // We need to ensure that logic picks up `master` from `upstream`.
    // Pre-push hook usually checks `refs/remotes/<remote>/ HEAD`.
    // We might need to fetch or ensure logic knows about upstream/master.

    let output = ctx.gherrit().args(["hook", "pre-push"]).assert();

    assert_autosquash_error(output, "upstream", "master");
}

#[test]
fn test_valid_stack_passes() {
    let ctx = TestContext::init_and_install_hooks();
    ctx.run_git(&["commit", "--allow-empty", "-m", "Initial"]);
    ctx.run_git(&["checkout", "-b", "feature-valid"]);

    // Stack of 3 normal commits
    ctx.run_git(&["commit", "--allow-empty", "-m", "Commit A"]);
    ctx.run_git(&["commit", "--allow-empty", "-m", "Commit B"]);
    ctx.run_git(&["commit", "--allow-empty", "-m", "Commit C"]);

    ctx.gherrit().args(["manage"]).assert().success();

    ctx.gherrit().args(["hook", "pre-push"]).assert().success();
}

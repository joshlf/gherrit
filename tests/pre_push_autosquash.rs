// Tests for the detection and rejection of autosquash commits (fixup!, squash!,
// amend!) in the pre-push hook. These tests ensure that GHerrit prevents users
// from pushing unfinished work to the remote, maintaining a clean commit
// history.

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
            "git rebase -i --autosquash {remote}/{branch}",
        )))
}

#[test]
fn test_autosquash_prefixes() {
    // Verify that the pre-push hook detects and rejects commits with any of the
    // standard autosquash prefixes: "fixup!", "squash!", and "amend!". This
    // ensures that even if a user creates these commits manually or via other
    // tools, GHerrit will catch them before they reach the remote.

    let ctx = testutil::test_context!().build();

    let prefixes = ["fixup!", "squash!", "amend!"];
    for prefix in prefixes {
        // Create a new branch for each prefix test to ensure clean state
        let branch_name = format!("feature-{}", prefix.replace("!", ""));
        ctx.run_git(&["checkout", "-b", &branch_name, "main"]);

        // Normal commit
        ctx.commit("Work in progress");

        // Prefix commit
        ctx.commit(&format!("{} Work in progress", prefix));

        let output = ctx.gherrit().args(["hook", "pre-push"]).assert();
        assert_autosquash_error(output, "origin", "main");
    }
}

#[test]
fn test_buried_autosquash_commit() {
    // Test that the hook scans the entire stack of commits being pushed, not
    // just the check-out tip.
    //
    // In this scenario:
    // - Commit A (Base)
    // - Commit B (Dirty - fixup! Commit A) <--- Should trigger failure
    // - Commit C (Clean tip)
    //
    // Even though HEAD (Commit C) is clean, the push includes Commit B, so it
    // must fail.
    let ctx = testutil::test_context!().build();
    ctx.checkout_new("feature-buried");

    // 1. Commit A
    ctx.commit("Commit A");

    // 2. Commit B (Dirty)
    ctx.commit("fixup! Commit A");

    // 3. Commit C (Normal)
    ctx.commit("Commit C");

    let output = ctx.gherrit().args(["hook", "pre-push"]).assert();
    assert_autosquash_error(output, "origin", "main");
}

#[test]
fn test_precedence_over_trailer_check() {
    // Test the edge case where a commit implies two conflicting states:
    // 1. It has a valid `gherrit-pr-id` trailer (usually allowing the push).
    // 2. It has a `fixup!` prefix (absolutely forbidding the push).
    //
    // The autosquash check must take precedence. A "fixup" commit is by
    // definition temporary/incomplete, so it doesn't matter if it has a valid
    // ID or not.

    let ctx = testutil::test_context!().build();
    ctx.checkout_new("feature-precedence");

    // Create a fixup commit that ALSO has a valid trailer. Normally, a trailer
    // makes a commit valid for Gherrit. But 'fixup!' should still block it
    // because it's considered "work in progress/to be squashed".
    let msg = "fixup! Some feature\n\ngherrit-pr-id: G12345";
    ctx.commit(msg);

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
    // Test that the error message correctly suggests the command to run even
    // when the user has a non-standard configuration.
    // - Remote: "upstream" (instead of "origin")
    // - Branch: "master" (instead of "main")
    //
    // The error message should recommend:
    //
    //   git rebase -i --autosquash upstream/master

    let ctx = testutil::test_context_minimal!().build();

    // Configure default branch to be 'master' to test dynamic branch name
    // detection.
    //
    // Note: `init_with_repo` usually sets main; this renaming operation
    // ensures that it is `master` *and* should work even if `init_with_repo`
    // changes to a different default branch name in the future.
    //
    // We also explicitly set `init.defaultBranch` to `master` so that Gherrit's
    // heuristic (which reads this config) finds `master` even if the user has
    // a global config setting it to `main`.
    ctx.run_git(&["config", "init.defaultBranch", "master"]);
    ctx.run_git(&["branch", "-m", "master"]);

    // Add custom remote 'upstream'. We need to actually point it somewhere
    // valid for `rev-parse` checks if they happen, but the error message
    // generation just calls `default_remote_name`. However, `gherrit` manages
    // `origin` by default. We need to tell it to use `upstream`.
    let upstream_path = ctx.dir.path().join("upstream.git");
    testutil::init_git_bare_repo(&upstream_path);
    ctx.run_git(&["remote", "add", "upstream", upstream_path.to_str().unwrap()]);

    // Configure gherrit to use upstream
    ctx.run_git(&["config", "gherrit.remote", "upstream"]);

    // Install hooks manually since we didn't use init_and_install_hooks
    ctx.install_hooks();

    ctx.commit("Initial commit");
    ctx.checkout_new("feature-dynamic");

    // Create fixup
    ctx.commit("fixup! Work");

    let output = ctx.gherrit().args(["hook", "pre-push"]).assert();

    assert_autosquash_error(output, "upstream", "master");
}

#[test]
fn test_valid_stack_passes() {
    // Control group test: standard, valid commits should pass without issues.

    let ctx = testutil::test_context!().build();
    ctx.checkout_new("feature-valid");

    // Stack of 3 normal commits
    ctx.commit("Commit A");
    ctx.commit("Commit B");
    ctx.commit("Commit C");

    ctx.gherrit().args(["hook", "pre-push"]).assert().success();
}

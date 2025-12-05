mod common;
use common::TestContext;

#[test]
fn test_commit_msg_hook() {
    let ctx = TestContext::init();
    let msg_file = ctx.repo_path.join("COMMIT_EDITMSG");
    std::fs::write(&msg_file, "feat: my cool feature").unwrap();

    // Must manage the branch first so the hook runs
    ctx.gherrit().args(["manage"]).assert().success();

    // Run hook
    ctx.gherrit()
        .args(["hook", "commit-msg", msg_file.to_str().unwrap()])
        .assert()
        .success();

    // Verify trailer was added
    let content = std::fs::read_to_string(msg_file).unwrap();
    assert!(content.contains("\ngherrit-pr-id: G"));
}

#[test]
fn test_full_stack_lifecycle_mocked() {
    let ctx = TestContext::init();

    // Setup: Create 'main' and a feature branch
    ctx.run_git(&["commit", "--allow-empty", "-m", "Initial Commit"]);
    ctx.run_git(&["checkout", "-b", "feature-stack"]);
    ctx.run_git(&[
        "commit",
        "--allow-empty",
        "-m",
        "Commit A\n\ngherrit-pr-id: G1",
    ]);
    ctx.run_git(&[
        "commit",
        "--allow-empty",
        "-m",
        "Commit B\n\ngherrit-pr-id: G2",
    ]);

    ctx.gherrit().args(["manage"]).assert().success();

    // Trigger Pre-Push Hook (Simulate 'git push'). We call the hook directly
    // because simulating a real 'git push' that calls the hook recursively is
    // complex in a test env.
    ctx.gherrit().args(["hook", "pre-push"]).assert().success();

    // Verify Side Effects (Mock Only)
    if !ctx.is_live {
        let state = ctx.read_mock_state();
        assert_eq!(state.prs.len(), 2, "Expected 2 PRs created");
        // Verify we pushed phantom branches or tags. The mock intercepts 'git
        // push origin <refspec>...'. GHerrit pushes refspecs like
        // 'refs/heads/G...:refs/heads/G...' or tags.
        assert!(
            !state.pushed_refs.is_empty(),
            "Expected some refs to be pushed"
        );
    }
}

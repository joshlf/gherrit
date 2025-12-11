use testutil;
use testutil::TestContext;

#[derive(Debug, Clone, Copy, PartialEq)]
enum StartState {
    /// gherritManaged is false/unset, and all relevant git configs are unset/default.
    UnmanagedClean,
    /// gherritManaged is false/unset, but configs have been manually set (e.g., pushRemote=dirty-remote).
    UnmanagedDirty,
    /// gherritManaged is managedPrivate, and configs match the expected private state (pushRemote=.).
    PrivateClean,
    /// gherritManaged is managedPrivate, but configs do NOT match (e.g., pushRemote=drifted-remote).
    PrivateDrifted,
    /// gherritManaged is managedPublic, and configs match the expected public state (pushRemote=origin).
    PublicClean,
    /// gherritManaged is managedPublic, but configs do NOT match (e.g., pushRemote=drifted-remote).
    PublicDrifted,
}

#[derive(Debug, Clone, Copy)]
enum Command {
    ManageDefault,
    ManagePrivate,
    ManagePublic,
    Unmanage,
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum ExpectedResult {
    Success,
    Block,
}

struct TestCase {
    id: &'static str,
    start: StartState,
    cmd: Command,
    force: bool,
    expected_result: ExpectedResult,
    expected_final_state: StartState,
}

const BRANCH: &str = "feature";

/// Helper to force the repo into a specific state
fn setup_state(ctx: &TestContext, state: StartState) {
    // Always start with a clean branch
    let _ = ctx.git().args(["branch", "-D", BRANCH]).output();
    ctx.checkout_new(BRANCH);

    let set_config = |key: &str, val: &str| {
        ctx.run_git(&["config", key, val]);
    };
    let unset_config = |key: &str| {
        let _ = ctx.git().args(["config", "--unset", key]).ok();
    };

    let key_managed = format!("branch.{BRANCH}.gherritManaged");
    let key_push = format!("branch.{BRANCH}.pushRemote");
    let key_remote = format!("branch.{BRANCH}.remote");
    let key_merge = format!("branch.{BRANCH}.merge");

    match state {
        StartState::UnmanagedClean => {
            unset_config(&key_managed);
            unset_config(&key_push);
            unset_config(&key_remote);
            unset_config(&key_merge);
        }
        StartState::UnmanagedDirty => {
            unset_config(&key_managed);
            set_config(&key_push, "dirty-remote");
        }
        StartState::PrivateClean => {
            set_config(&key_managed, "managedPrivate");
            set_config(&key_push, ".");
            set_config(&key_remote, ".");
            set_config(&key_merge, &format!("refs/heads/{BRANCH}"));
        }
        StartState::PrivateDrifted => {
            set_config(&key_managed, "managedPrivate");
            set_config(&key_push, "drifted-remote");
            set_config(&key_remote, ".");
            set_config(&key_merge, &format!("refs/heads/{BRANCH}"));
        }
        StartState::PublicClean => {
            set_config(&key_managed, "managedPublic");
            set_config(&key_push, "origin");
            set_config(&key_remote, ".");
            set_config(&key_merge, &format!("refs/heads/{BRANCH}"));
        }
        StartState::PublicDrifted => {
            set_config(&key_managed, "managedPublic");
            set_config(&key_push, "drifted-remote");
            set_config(&key_remote, ".");
            set_config(&key_merge, &format!("refs/heads/{BRANCH}"));
        }
    }
}

fn verify_state(ctx: &TestContext, expected: StartState) {
    let get_config = |key: &str| -> Option<String> {
        let output = ctx.git().args(["config", key]).output().ok()?;
        if output.status.success() {
            Some(String::from_utf8_lossy(&output.stdout).trim().to_string())
        } else {
            None
        }
    };

    let val_managed = get_config(&format!("branch.{BRANCH}.gherritManaged"));
    let val_push = get_config(&format!("branch.{BRANCH}.pushRemote"));

    match expected {
        StartState::UnmanagedClean => {
            assert_eq!(val_managed.as_deref().unwrap_or("false"), "false");
            assert!(val_push.is_none(), "Expected pushRemote to be unset");
        }
        StartState::UnmanagedDirty => {
            assert_eq!(val_managed.as_deref().unwrap_or("false"), "false");
            assert_eq!(val_push.as_deref(), Some("dirty-remote"));
        }
        StartState::PrivateClean => {
            assert_eq!(val_managed.as_deref(), Some("managedPrivate"));
            assert_eq!(val_push.as_deref(), Some("."));
        }
        StartState::PrivateDrifted => {
            assert_eq!(val_managed.as_deref(), Some("managedPrivate"));
            assert_eq!(val_push.as_deref(), Some("drifted-remote"));
        }
        StartState::PublicClean => {
            assert_eq!(val_managed.as_deref(), Some("managedPublic"));
            assert_eq!(val_push.as_deref(), Some("origin"));
        }
        StartState::PublicDrifted => {
            assert_eq!(val_managed.as_deref(), Some("managedPublic"));
            assert_eq!(val_push.as_deref(), Some("drifted-remote"));
        }
    }
}

#[test]
fn test_branch_config_transitions() {
    #[rustfmt::skip]
    let cases = {
        use {Command::*, ExpectedResult::*, StartState::*};

        vec![
            // Unmanaged Clean -> ...
            TestCase { id: "T01", start: UnmanagedClean, cmd: ManageDefault, force: false, expected_result: Success, expected_final_state: PrivateClean },
            TestCase { id: "T02", start: UnmanagedClean, cmd: ManagePublic, force: false, expected_result: Success, expected_final_state: PublicClean },
            TestCase { id: "T03", start: UnmanagedClean, cmd: Unmanage, force: false, expected_result: Success, expected_final_state: UnmanagedClean },

            // Unmanaged Dirty -> ...
            TestCase { id: "T04", start: UnmanagedDirty, cmd: ManagePrivate, force: false, expected_result: Block, expected_final_state: UnmanagedDirty },
            TestCase { id: "T05", start: UnmanagedDirty, cmd: ManagePrivate, force: true, expected_result: Success, expected_final_state: PrivateClean },
            TestCase { id: "T06", start: UnmanagedDirty, cmd: ManagePublic, force: false, expected_result: Block, expected_final_state: UnmanagedDirty },
            TestCase { id: "T07", start: UnmanagedDirty, cmd: ManagePublic, force: true, expected_result: Success, expected_final_state: PublicClean },

            // Private Clean -> ...
            TestCase { id: "T08", start: PrivateClean, cmd: ManagePrivate, force: false, expected_result: Success, expected_final_state: PrivateClean },
            TestCase { id: "T09", start: PrivateClean, cmd: ManagePublic, force: false, expected_result: Success, expected_final_state: PublicClean },
            TestCase { id: "T10", start: PrivateClean, cmd: Unmanage, force: false, expected_result: Success, expected_final_state: UnmanagedClean },

            // Private Drifted -> ...
            TestCase { id: "T11", start: PrivateDrifted, cmd: ManagePrivate, force: false, expected_result: Block, expected_final_state: PrivateDrifted },
            TestCase { id: "T12", start: PrivateDrifted, cmd: ManagePrivate, force: true, expected_result: Success, expected_final_state: PrivateClean },
            TestCase { id: "T13", start: PrivateDrifted, cmd: ManagePublic, force: false, expected_result: Block, expected_final_state: PrivateDrifted },
            TestCase { id: "T14", start: PrivateDrifted, cmd: ManagePublic, force: true, expected_result: Success, expected_final_state: PublicClean },
            TestCase { id: "T15", start: PrivateDrifted, cmd: Unmanage, force: false, expected_result: Block, expected_final_state: PrivateDrifted },
            TestCase { id: "T16", start: PrivateDrifted, cmd: Unmanage, force: true, expected_result: Success, expected_final_state: UnmanagedClean },

            // Public Clean -> ...
            TestCase { id: "T17", start: PublicClean, cmd: ManagePrivate, force: false, expected_result: Success, expected_final_state: PrivateClean },
            TestCase { id: "T18", start: PublicClean, cmd: ManagePublic, force: false, expected_result: Success, expected_final_state: PublicClean },
            TestCase { id: "T19", start: PublicClean, cmd: Unmanage, force: false, expected_result: Success, expected_final_state: UnmanagedClean },

            // Public Drifted -> ...
            TestCase { id: "T20", start: PublicDrifted, cmd: ManagePrivate, force: false, expected_result: Block, expected_final_state: PublicDrifted },
            TestCase { id: "T21", start: PublicDrifted, cmd: ManagePrivate, force: true, expected_result: Success, expected_final_state: PrivateClean },
            TestCase { id: "T22", start: PublicDrifted, cmd: ManagePublic, force: false, expected_result: Block, expected_final_state: PublicDrifted },
            TestCase { id: "T23", start: PublicDrifted, cmd: ManagePublic, force: true, expected_result: Success, expected_final_state: PublicClean },
            TestCase { id: "T24", start: PublicDrifted, cmd: Unmanage, force: false, expected_result: Block, expected_final_state: PublicDrifted },
            TestCase { id: "T25", start: PublicDrifted, cmd: Unmanage, force: true, expected_result: Success, expected_final_state: UnmanagedClean },
        ]
    };

    let ctx = testutil::test_context_minimal!().build();

    for case in cases {
        println!("Running Case: {}", case.id);
        setup_state(&ctx, case.start);

        let mut cmd = ctx.gherrit();
        match case.cmd {
            Command::ManageDefault => {
                cmd.arg("manage");
            }
            Command::ManagePrivate => {
                cmd.args(["manage", "--private"]);
            }
            Command::ManagePublic => {
                cmd.args(["manage", "--public"]);
            }
            Command::Unmanage => {
                cmd.arg("unmanage");
            }
        }

        if case.force {
            cmd.arg("--force");
        }

        let assert = cmd.assert().success(); // All these commands should exit 0, even blocks (they just warn)
        // Check output for blocks
        if case.expected_result == ExpectedResult::Block {
            let output = assert.get_output();
            let stderr = String::from_utf8_lossy(&output.stderr);
            assert!(
                stderr.contains("drift detected") || stderr.contains("custom pushRemote"),
                "Case {}: Expected warning about drift/custom config, got: {}",
                case.id,
                stderr
            );
        }

        verify_state(&ctx, case.expected_final_state);
    }
}

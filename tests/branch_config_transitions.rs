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
    _expected_result: ExpectedResult,
    expected_final_state: StartState,
}

const BRANCH: &str = "feature";

/// Helper to force the repo into a specific state
fn setup_state(ctx: &TestContext, state: StartState) {
    // Always start with a clean branch
    let _ = ctx.git().args(["branch", "-D", BRANCH]).output();
    ctx.checkout_new(BRANCH);

    let key_managed = format!("branch.{BRANCH}.gherritManaged");
    let key_push = format!("branch.{BRANCH}.pushRemote");
    let key_remote = format!("branch.{BRANCH}.remote");
    let key_merge = format!("branch.{BRANCH}.merge");

    match state {
        StartState::UnmanagedClean => {
            ctx.set_config(&key_managed, Some("false"));
            ctx.set_config(&key_push, None);
            ctx.set_config(&key_remote, None);
            ctx.set_config(&key_merge, None);
        }
        StartState::UnmanagedDirty => {
            ctx.set_config(&key_managed, Some("false"));
            ctx.set_config(&key_push, Some("dirty-remote"));
        }
        StartState::PrivateClean => {
            ctx.set_config(&key_managed, Some(testutil::MANAGED_PRIVATE));
            ctx.set_config(&key_push, Some("."));
            ctx.set_config(&key_remote, Some("."));
            ctx.set_config(&key_merge, Some(&format!("refs/heads/{BRANCH}")));
        }
        StartState::PrivateDrifted => {
            ctx.set_config(&key_managed, Some(testutil::MANAGED_PRIVATE));
            ctx.set_config(&key_push, Some("drifted-remote"));
            ctx.set_config(&key_remote, Some("."));
            ctx.set_config(&key_merge, Some(&format!("refs/heads/{BRANCH}")));
        }
        StartState::PublicClean => {
            ctx.set_config(&key_managed, Some(testutil::MANAGED_PUBLIC));
            ctx.set_config(&key_push, Some("origin"));
            ctx.set_config(&key_remote, Some("."));
            ctx.set_config(&key_merge, Some(&format!("refs/heads/{BRANCH}")));
        }
        StartState::PublicDrifted => {
            ctx.set_config(&key_managed, Some(testutil::MANAGED_PUBLIC));
            ctx.set_config(&key_push, Some("drifted-remote"));
            ctx.set_config(&key_remote, Some("."));
            ctx.set_config(&key_merge, Some(&format!("refs/heads/{BRANCH}")));
        }
    }
}

fn verify_state(ctx: &TestContext, expected: StartState) {
    let key_managed = format!("branch.{BRANCH}.gherritManaged");
    let key_push = format!("branch.{BRANCH}.pushRemote");

    match expected {
        StartState::UnmanagedClean => {
            ctx.assert_config(&key_managed, Some("false"));
            ctx.assert_config(&key_push, None);
        }
        StartState::UnmanagedDirty => {
            ctx.assert_config(&key_managed, Some("false"));
            ctx.assert_config(&key_push, Some("dirty-remote"));
        }
        StartState::PrivateClean => {
            ctx.assert_config(&key_managed, Some(testutil::MANAGED_PRIVATE));
            ctx.assert_config(&key_push, Some("."));
        }
        StartState::PrivateDrifted => {
            ctx.assert_config(&key_managed, Some(testutil::MANAGED_PRIVATE));
            ctx.assert_config(&key_push, Some("drifted-remote"));
        }
        StartState::PublicClean => {
            ctx.assert_config(&key_managed, Some(testutil::MANAGED_PUBLIC));
            ctx.assert_config(&key_push, Some("origin"));
        }
        StartState::PublicDrifted => {
            ctx.assert_config(&key_managed, Some(testutil::MANAGED_PUBLIC));
            ctx.assert_config(&key_push, Some("drifted-remote"));
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
            TestCase { id: "T01", start: UnmanagedClean, cmd: ManageDefault, force: false, _expected_result: Success, expected_final_state: PrivateClean },
            TestCase { id: "T02", start: UnmanagedClean, cmd: ManagePublic, force: false, _expected_result: Success, expected_final_state: PublicClean },
            TestCase { id: "T03", start: UnmanagedClean, cmd: Unmanage, force: false, _expected_result: Success, expected_final_state: UnmanagedClean },

            // Unmanaged Dirty -> ...
            TestCase { id: "T04", start: UnmanagedDirty, cmd: ManagePrivate, force: false, _expected_result: Block, expected_final_state: UnmanagedDirty },
            TestCase { id: "T05", start: UnmanagedDirty, cmd: ManagePrivate, force: true, _expected_result: Success, expected_final_state: PrivateClean },
            TestCase { id: "T06", start: UnmanagedDirty, cmd: ManagePublic, force: false, _expected_result: Block, expected_final_state: UnmanagedDirty },
            TestCase { id: "T07", start: UnmanagedDirty, cmd: ManagePublic, force: true, _expected_result: Success, expected_final_state: PublicClean },

            // Private Clean -> ...
            TestCase { id: "T08", start: PrivateClean, cmd: ManagePrivate, force: false, _expected_result: Success, expected_final_state: PrivateClean },
            TestCase { id: "T09", start: PrivateClean, cmd: ManagePublic, force: false, _expected_result: Success, expected_final_state: PublicClean },
            TestCase { id: "T10", start: PrivateClean, cmd: Unmanage, force: false, _expected_result: Success, expected_final_state: UnmanagedClean },

            // Private Drifted -> ...
            TestCase { id: "T11", start: PrivateDrifted, cmd: ManagePrivate, force: false, _expected_result: Block, expected_final_state: PrivateDrifted },
            TestCase { id: "T12", start: PrivateDrifted, cmd: ManagePrivate, force: true, _expected_result: Success, expected_final_state: PrivateClean },
            TestCase { id: "T13", start: PrivateDrifted, cmd: ManagePublic, force: false, _expected_result: Block, expected_final_state: PrivateDrifted },
            TestCase { id: "T14", start: PrivateDrifted, cmd: ManagePublic, force: true, _expected_result: Success, expected_final_state: PublicClean },
            TestCase { id: "T15", start: PrivateDrifted, cmd: Unmanage, force: false, _expected_result: Block, expected_final_state: PrivateDrifted },
            TestCase { id: "T16", start: PrivateDrifted, cmd: Unmanage, force: true, _expected_result: Success, expected_final_state: UnmanagedClean },

            // Public Clean -> ...
            TestCase { id: "T17", start: PublicClean, cmd: ManagePrivate, force: false, _expected_result: Success, expected_final_state: PrivateClean },
            TestCase { id: "T18", start: PublicClean, cmd: ManagePublic, force: false, _expected_result: Success, expected_final_state: PublicClean },
            TestCase { id: "T19", start: PublicClean, cmd: Unmanage, force: false, _expected_result: Success, expected_final_state: UnmanagedClean },

            // Public Drifted -> ...
            TestCase { id: "T20", start: PublicDrifted, cmd: ManagePrivate, force: false, _expected_result: Block, expected_final_state: PublicDrifted },
            TestCase { id: "T21", start: PublicDrifted, cmd: ManagePrivate, force: true, _expected_result: Success, expected_final_state: PrivateClean },
            TestCase { id: "T22", start: PublicDrifted, cmd: ManagePublic, force: false, _expected_result: Block, expected_final_state: PublicDrifted },
            TestCase { id: "T23", start: PublicDrifted, cmd: ManagePublic, force: true, _expected_result: Success, expected_final_state: PublicClean },
            TestCase { id: "T24", start: PublicDrifted, cmd: Unmanage, force: false, _expected_result: Block, expected_final_state: PublicDrifted },
            TestCase { id: "T25", start: PublicDrifted, cmd: Unmanage, force: true, _expected_result: Success, expected_final_state: UnmanagedClean },
        ]
    };

    let ctx = testutil::test_context_minimal!().build();

    for case in cases {
        println!("Running Case: {}", case.id);
        setup_state(&ctx, case.start);

        let mut cmd;
        match case.cmd {
            Command::ManageDefault => {
                cmd = ctx.manage();
            }
            Command::ManagePrivate => {
                cmd = ctx.manage();
                cmd.arg("--private");
            }
            Command::ManagePublic => {
                cmd = ctx.manage();
                cmd.arg("--public");
            }
            Command::Unmanage => {
                cmd = ctx.unmanage();
            }
        }

        if case.force {
            cmd.arg("--force");
        }

        testutil::assert_snapshot!(ctx, cmd, format!("transition_case_{}", case.id));

        verify_state(&ctx, case.expected_final_state);
    }
}

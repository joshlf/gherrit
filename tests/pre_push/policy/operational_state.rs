use predicates::prelude::*;

fn context() -> testutil::TestContext {
    testutil::test_context!()
        .with_remote()
        .with_installed_hooks()
        .with_initial_commit()
        .with_mock_github()
        .with_git_interceptor()
        .build()
}

fn create_change(ctx: &testutil::TestContext) -> (String, u64) {
    ctx.checkout_new("operational-state");
    ctx.commit("Initial change");
    let id = ctx.gherrit_id("HEAD").unwrap();
    ctx.hook_cmd("pre-push").assert().success();
    let mut pr_id = 0;
    ctx.inspect_mock_state(|state| pr_id = state.prs[0].id);
    (id, pr_id)
}

#[test]
fn blockers_apply_to_same_topology_head_updates() {
    for (case, expected, set) in [
        ("merge-queue", "is in the merge queue", 0),
        ("auto-merge", "has auto-merge enabled", 1),
        ("native-stack", "belongs to a native GitHub stack", 2),
    ] {
        let ctx = context();
        let (id, pr_id) = create_change(&ctx);
        ctx.mutate_mock_state(|state| match set {
            0 => {
                state.merge_queue.insert(pr_id);
            }
            1 => {
                state.auto_merge.insert(pr_id);
            }
            2 => {
                state.native_stacks.insert(pr_id);
            }
            _ => unreachable!(),
        });
        let remote_oid = ctx.remote_ref_oid(&format!("refs/heads/{id}")).unwrap();
        ctx.amend_with_message(&format!("{case} content update"));

        ctx.hook_cmd("pre-push").assert().failure().stderr(predicate::str::contains(expected));
        assert_eq!(
            ctx.remote_ref_oid(&format!("refs/heads/{id}")).as_deref(),
            Some(remote_oid.as_str())
        );
    }
}

#[test]
fn blocker_appearing_after_staging_is_rechecked_before_git_publication() {
    let ctx = context();
    ctx.checkout_new("operational-race");
    ctx.commit("A");
    let a = ctx.gherrit_id("HEAD").unwrap();
    ctx.commit("B");
    let b = ctx.gherrit_id("HEAD").unwrap();
    ctx.hook_cmd("pre-push").assert().success();

    let mut b_pr = 0;
    ctx.inspect_mock_state(|state| {
        b_pr = state.prs.iter().find(|pr| pr.head.ref_field == b).unwrap().id;
    });
    ctx.mutate_mock_state(|state| {
        state.base_updates.clear();
        state.pushes.clear();
        state.merge_queue_after_base_update = Some(b_pr);
    });

    ctx.run_git(&["reset", "--hard", "main"]);
    ctx.commit(&format!("B reordered\n\ngherrit-pr-id: {b}"));
    ctx.commit(&format!("A reordered\n\ngherrit-pr-id: {a}"));
    let remote_refs = ctx.remote_refs("refs");

    ctx.hook_cmd("pre-push")
        .assert()
        .failure()
        .stderr(predicate::str::contains("is in the merge queue"));
    assert_eq!(ctx.remote_refs("refs"), remote_refs);
    ctx.inspect_mock_state(|state| {
        let b_pr = state.prs.iter().find(|pr| pr.head.ref_field == b).unwrap();
        assert_eq!(b_pr.base.ref_field, "main", "staging should remain safe after abort");
        assert!(state.pushes.iter().all(|push| !push.succeeded()));
    });
}

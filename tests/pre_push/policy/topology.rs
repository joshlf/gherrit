use predicates::prelude::*;

fn context() -> testutil::TestContext {
    testutil::test_context!().with_remote().with_installed_hooks().with_initial_commit().build()
}

#[test]
fn rejects_duplicate_gherrit_ids_before_remote_io() {
    let ctx = context();
    ctx.checkout_new("duplicate-id");
    ctx.commit("First change");
    let id = ctx.gherrit_id("HEAD").unwrap();
    ctx.commit(&format!("Second change\n\ngherrit-pr-id: {id}"));

    let refs = ctx.remote_refs("refs");
    ctx.hook_cmd("pre-push")
        .assert()
        .failure()
        .stderr(predicate::str::contains(format!("duplicate gherrit-pr-id `{id}`")));
    assert_eq!(ctx.remote_refs("refs"), refs);
}

#[test]
fn rejects_multiple_or_empty_gherrit_id_trailers() {
    for (branch, message, expected) in [
        (
            "multiple-ids",
            "Change\n\ngherrit-pr-id: Gone\ngherrit-pr-id: Gtwo",
            "multiple gherrit-pr-id trailers",
        ),
        ("empty-id", "Change\n\ngherrit-pr-id: ", "missing a non-empty gherrit-pr-id trailer"),
    ] {
        let ctx = context();
        ctx.checkout_new(branch);
        ctx.commit(message);
        ctx.hook_cmd("pre-push").assert().failure().stderr(predicate::str::contains(expected));
    }
}

#[test]
fn rejects_non_linear_history() {
    let ctx = context();
    ctx.checkout_new("nonlinear");
    ctx.commit("Base feature change");
    ctx.run_git(&["checkout", "-b", "side"]);
    ctx.commit("Side change");
    ctx.run_git(&["checkout", "nonlinear"]);
    ctx.commit("Mainline change");
    ctx.git_cmd().args(["merge", "--no-ff", "side", "-m", "Merge side"]).assert().success();

    ctx.hook_cmd("pre-push")
        .assert()
        .failure()
        .stderr(predicate::str::contains("linear first-parent stack"));
}

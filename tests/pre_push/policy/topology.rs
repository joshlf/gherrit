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
        ("empty-id", "Change\n\ngherrit-pr-id: ", "gherrit-pr-id trailer is empty"),
    ] {
        let ctx = context();
        ctx.checkout_new(branch);
        ctx.git_cmd()
            .args(["commit", "--allow-empty", "--no-verify", "-m", message])
            .assert()
            .success();
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

#[test]
fn rejects_invalid_or_reserved_id_spellings_before_remote_io() {
    for (index, id) in [
        "main",
        "master",
        "HEAD",
        "feature",
        "G12345",
        "Gabcdefghijklmnopqrstuvwxyz234567extra",
    ]
    .into_iter()
    .enumerate()
    {
        let ctx = context();
        ctx.checkout_new(&format!("invalid-id-{index}"));
        let message = format!("Change\n\ngherrit-pr-id: {id}");
        ctx.git_cmd()
            .args(["commit", "--allow-empty", "--no-verify", "-m", &message])
            .assert()
            .success();

        let refs = ctx.remote_refs("refs");
        ctx.hook_cmd("pre-push")
            .assert()
            .failure()
            .stderr(predicate::str::contains("Invalid gherrit-pr-id"));
        assert_eq!(ctx.remote_refs("refs"), refs);
    }
}

#[test]
fn matching_prose_outside_the_trailer_block_is_not_an_id() {
    let ctx = context();
    ctx.checkout_new("prose-id");
    let message = concat!(
        "Change\n\n",
        "This prose mentions gherrit-pr-id: Gabcdefghijklmnopqrstuvwxyz234567.\n\n",
        "Not-A-Trailer paragraph."
    );
    ctx.git_cmd()
        .args(["commit", "--allow-empty", "--no-verify", "-m", message])
        .assert()
        .success();

    ctx.hook_cmd("pre-push")
        .assert()
        .failure()
        .stderr(predicate::str::contains("missing a non-empty gherrit-pr-id trailer"));
}

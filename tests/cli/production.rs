use predicates::prelude::*;

#[test]
fn production_binary_rejects_the_test_driver_protocol() {
    let ctx = testutil::TestContextBuilder::new(assert_cmd::cargo::cargo_bin!("gherrit")).build();
    ctx.gherrit_cmd()
        .arg("__test-git")
        .assert()
        .failure()
        .stderr(predicate::str::contains("unrecognized subcommand '__test-git'"));
}

#[test]
fn test_driver_without_github_endpoint_fails_closed() {
    let ctx = testutil::test_context!().with_remote().with_initial_commit().build();
    ctx.checkout_managed_private("missing-github-endpoint");
    ctx.commit_with_gherrit_id("Managed work");
    let id = ctx.gherrit_id("HEAD").unwrap();

    ctx.gherrit_cmd().args(["hook", "pre-push"]).assert().failure().stderr(
        predicate::str::contains(
            "test driver cannot publish PRs without a configured GitHub endpoint",
        ),
    );

    assert_eq!(ctx.remote_ref_oid(&format!("refs/heads/{id}")), None);
}

#[test]
fn production_rejects_a_local_destination_before_empty_stack_observation() {
    let ctx = testutil::TestContextBuilder::new(assert_cmd::cargo::cargo_bin!("gherrit"))
        .with_remote()
        .with_initial_commit()
        .build();
    ctx.checkout_managed_private("empty-production-stack");

    ctx.gherrit_cmd().args(["hook", "pre-push"]).assert().failure().stderr(
        predicate::str::contains(
            "Production publication requires an HTTPS or SSH destination on github.com",
        ),
    );
}

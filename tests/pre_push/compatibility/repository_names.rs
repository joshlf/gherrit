#[test]
fn special_characters_cross_the_adapter_boundary() {
    // Parsing permutations are pure unit tests. Keep one complete flow to prove
    // that punctuation also crosses Git paths and GraphQL requests correctly.
    let ctx = testutil::test_context!()
        .repository("user.name", "repo-name")
        .with_remote()
        .with_installed_hooks()
        .with_initial_commit()
        .with_mock_github()
        .build();

    ctx.checkout_new("feature-stack");
    ctx.commit("Commit A");

    ctx.hook_cmd("pre-push").assert().success();
}

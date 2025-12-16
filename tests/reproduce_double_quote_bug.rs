#[test]
fn test_reproduce_double_quote_bug() {
    let ctx = testutil::test_context!().build();
    ctx.checkout_new("feature-double-quote");
    ctx.commit("Work");

    // The current bug causes the owner/repo to be sent as "\"owner\"" instead of "owner".
    // The mock server's regex expects `repository(owner: "owner", ...)`
    // If double quotes are sent, the regex won't match, and the request will fail (or return generic data which causes failure later).

    // In strict mode (which testutil usually provides), un-matched queries might return errors or default responses that key logic depends on.

    // For this test, we expect success. If the bug is present, this should fail because the query won't match the mock expectation for looking up PRs.
    ctx.gherrit().args(["hook", "pre-push"]).assert().success();
}

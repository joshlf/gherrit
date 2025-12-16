#[test]
fn test_pagination_bug() {
    let ctx = testutil::test_context!().build();
    let repo_path = &ctx.repo_path;

    // 1. Setup base commit on main
    ctx.commit("Init");

    // 2. Checkout feature branch and manage it
    ctx.checkout_new("feature");
    ctx.gherrit().args(["manage", "--force"]).assert().success();

    // 3. Create a commit with a known Change-Id
    let change_id = "I0000000000000000000000000000000000000105";
    let msg = format!("Commit 105\n\ngherrit-pr-id: {}", change_id);
    ctx.commit(&msg);

    // 4. Generate 110 PRs in the mock server state
    let mut prs = Vec::new();
    for i in 1..=110 {
        let is_target = i == 105;
        let head_ref =
            if is_target { change_id.to_string() } else { format!("other-change-{}", i) };

        let pr = serde_json::json!({
            "id": i,
            "number": i,
            "html_url": format!("http://github.com/owner/repo/pull/{}", i),
            "url": format!("http://api.github.com/repos/owner/repo/pulls/{}", i),
            "node_id": format!("PR_{}", i),
            "state": "OPEN",
            "user": {
                "login": "test",
                "id": 1,
                "node_id": "U1",
                "avatar_url": "http://example.com/avatar",
                "gravatar_id": "",
                "url": "http://example.com/users/test",
                "html_url": "http://example.com/users/test",
                "followers_url": "http://example.com/users/test/followers",
                "following_url": "http://example.com/users/test/following{/other_user}",
                "gists_url": "http://example.com/users/test/gists{/gist_id}",
                "starred_url": "http://example.com/users/test/starred{/owner}{/repo}",
                "subscriptions_url": "http://example.com/users/test/subscriptions",
                "organizations_url": "http://example.com/users/test/orgs",
                "repos_url": "http://example.com/users/test/repos",
                "events_url": "http://example.com/users/test/events{/privacy}",
                "received_events_url": "http://example.com/users/test/received_events",
                "type": "User",
                "site_admin": false
            },
            "head": { "ref": head_ref, "sha": "sha" },
            "base": { "ref": "main", "sha": "sha" },
            "created_at": "2024-01-01T00:00:00Z",
            "updated_at": "2024-01-01T00:00:00Z"
        });
        prs.push(pr);
    }

    // Write state directly to file
    let state = serde_json::json!({
        "prs": prs,
        "repo_owner": "owner",
        "repo_name": "repo"
    });
    std::fs::write(repo_path.join("mock_state.json"), serde_json::to_string(&state).unwrap())
        .unwrap();

    // 5. Run gherrit hook pre-push
    let assert = ctx.gherrit().args(["hook", "pre-push"]).env("RUST_LOG", "debug").assert();

    let output = assert.get_output();
    let stderr = String::from_utf8_lossy(&output.stderr);

    println!("Stderr: {}", stderr);

    if !stderr.contains("Found existing PR #105") {
        panic!("Regression: Failed to find PR #105 (likely pagination bug). Logs:\n{}", stderr);
    }
}

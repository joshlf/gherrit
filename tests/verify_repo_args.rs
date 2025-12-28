use serde_json::json;
use testutil::test_context;

#[test]
fn test_repository_query_arguments() {
    let ctx = test_context!().build();
    let url = &ctx.mock_server.as_ref().expect("Mock server not started").url;
    let graphql_url = format!("{}/graphql", url);

    let state = ctx.read_mock_state();
    let owner = state.repo_owner;
    let name = state.repo_name;

    // Query with correct arguments (should succeed)
    let query = format!("query {{ repository(owner: \"{owner}\", name: \"{name}\") {{ id }} }}");

    let resp =
        ureq::post(&graphql_url).send_json(json!({ "query": query })).expect("Request failed");

    let json: serde_json::Value = resp.into_json().expect("Failed to parse JSON");
    let repo_id = json.pointer("/data/repository/id");
    assert_eq!(repo_id, Some(&json!("REPO_NODE_ID")), "Expected correct repo to return ID");

    // Query with incorrect arguments (should return null for repository)
    let query_wrong = "query { repository(owner: \"wrong\", name: \"wrong\") { id } }";

    let resp_wrong = ureq::post(&graphql_url)
        .send_json(json!({ "query": query_wrong }))
        .expect("Request failed");

    let json_wrong: serde_json::Value = resp_wrong.into_json().expect("Failed to parse JSON");
    let repo_wrong = json_wrong.pointer("/data/repository");

    // CURRENTLY: This likely returns REPO_NODE_ID because validation is missing.
    // We expect this assertion to FAIL until we fix the code.
    assert_eq!(
        repo_wrong,
        Some(&serde_json::Value::Null),
        "Expected repository to be null for wrong args, got: {:?}",
        repo_wrong
    );
}

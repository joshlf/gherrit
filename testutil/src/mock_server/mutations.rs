use apollo_compiler::executable;

use super::{
    codec::{get_string_field, input_object, required_string_field, response_key, selected_fields},
    MockPrArgs, MockState, PrEntry,
};

pub(super) fn handle_update_pr(
    mock_state: &mut MockState,
    field: &executable::Field,
    branch_exists: &dyn Fn(&str) -> Result<bool, String>,
) -> Result<serde_json::Value, String> {
    const PATH: &str = "updatePullRequest";
    let input = input_object(field, PATH)?;
    let node_id = required_string_field(input, "pullRequestId", PATH)?;
    let title = get_string_field(input, "title");
    let body = get_string_field(input, "body");
    let base = get_string_field(input, "baseRefName");

    if let Some(base) = &base {
        if !branch_exists(base)? {
            return Err(format!("Base branch `{base}` does not exist"));
        }
    }

    let Some(pr) = mock_state.prs.iter_mut().find(|pr| pr.node_id == node_id) else {
        return Err(format!("Pull request node `{node_id}` does not exist"));
    };
    if base.as_deref() == Some(pr.head.as_str()) {
        return Err("Pull request head and base branches must differ".to_string());
    }
    if let Some(title) = title {
        pr.title = Some(title);
    }
    if let Some(body) = &body {
        pr.body = Some(body.clone());
    }
    if let Some(base) = base {
        pr.base = base;
    }

    let mut response = serde_json::Map::new();
    for field in selected_fields(&field.selection_set, PATH)? {
        match field.name.as_str() {
            "clientMutationId" => {
                response.insert(response_key(field), serde_json::Value::Null);
            }
            _ => unreachable!("request was checked by validate_update_field"),
        }
    }
    Ok(serde_json::Value::Object(response))
}

pub(super) fn handle_create_pr(
    mock_state: &mut MockState,
    field: &executable::Field,
    branch_exists: &dyn Fn(&str) -> Result<bool, String>,
) -> Result<serde_json::Value, String> {
    const PATH: &str = "createPullRequest";
    let input = input_object(field, PATH)?;
    let repository_id = required_string_field(input, "repositoryId", PATH)?;
    let base = required_string_field(input, "baseRefName", PATH)?;
    let head = required_string_field(input, "headRefName", PATH)?;
    let title = required_string_field(input, "title", PATH)?;
    let body = get_string_field(input, "body").unwrap_or_default();

    if repository_id != "REPO_NODE_ID" {
        return Err(format!("Repository node `{repository_id}` does not exist"));
    }
    if base == head {
        return Err("Pull request head and base branches must differ".to_string());
    }
    if !branch_exists(&base)? {
        return Err(format!("Base branch `{base}` does not exist"));
    }
    if !branch_exists(&head)? {
        return Err(format!("Head branch `{head}` does not exist"));
    }
    if mock_state.prs.iter().any(|pr| {
        pr.state == "OPEN"
            && pr.head == head
            && !mock_state.cross_repository_prs.contains(&pr.number)
    }) {
        return Err(format!("An open pull request already exists for head branch `{head}`"));
    }

    let number = mock_state.prs.iter().map(|pr| pr.number as u64).max().unwrap_or(0) + 1;
    let owner = mock_state.repo_owner.clone();
    let repo = mock_state.repo_name.clone();
    let entry = PrEntry::mock(MockPrArgs {
        id: number,
        title,
        body,
        head,
        base,
        repo_owner: &owner,
        repo_name: &repo,
    });
    let node_id = entry.node_id.clone();
    let html_url = entry.html_url.clone();
    mock_state.prs.push(entry);

    let mut response = serde_json::Map::new();
    for field in selected_fields(&field.selection_set, PATH)? {
        match field.name.as_str() {
            "clientMutationId" => {
                response.insert(response_key(field), serde_json::Value::Null);
            }
            "pullRequest" => {
                let mut pull_request = serde_json::Map::new();
                for field in selected_fields(&field.selection_set, "createPullRequest.pullRequest")?
                {
                    let value = match field.name.as_str() {
                        "number" => serde_json::json!(number),
                        "url" => serde_json::json!(html_url),
                        "id" => serde_json::json!(node_id),
                        _ => unreachable!("request was checked by validate_create_field"),
                    };
                    pull_request.insert(response_key(field), value);
                }
                response.insert(response_key(field), serde_json::Value::Object(pull_request));
            }
            _ => unreachable!("request was checked by validate_create_field"),
        }
    }
    Ok(serde_json::Value::Object(response))
}

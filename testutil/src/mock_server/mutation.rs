//! Mutation contracts of the independent HTTP fake.

use super::*;

pub(super) fn extract_input_field<'a>(
    field: &'a executable::Field,
    arg_name: &str,
) -> Option<&'a Vec<(Name, Node<ast::Value>)>> {
    field.arguments.iter().find(|arg| arg.name == arg_name).and_then(|arg| {
        if let ast::Value::Object(obj) = &*arg.value {
            Some(obj)
        } else {
            None
        }
    })
}

pub(super) fn get_string_field(obj: &[(Name, Node<ast::Value>)], key: &str) -> Option<String> {
    obj.iter().find(|(k, _)| k == key).and_then(|(_, v)| {
        if let ast::Value::String(s) = &**v {
            Some(s.to_string())
        } else {
            None
        }
    })
}

pub(super) fn get_bool_field(obj: &[(Name, Node<ast::Value>)], key: &str) -> Option<bool> {
    obj.iter().find(|(name, _)| name == key).and_then(|(_, value)| match &**value {
        ast::Value::Boolean(value) => Some(*value),
        _ => None,
    })
}

pub(super) fn required_string_field(
    input: &[(Name, Node<ast::Value>)],
    key: &str,
    path: &str,
) -> Result<String, String> {
    get_string_field(input, key)
        .ok_or_else(|| format!("The mock GitHub API requires string field `{path}.input.{key}`"))
}

pub(super) fn handle_close_pr(
    mock_state: &mut MockState,
    field: &executable::Field,
) -> Result<serde_json::Value, String> {
    const PATH: &str = "closePullRequest";
    let input = input_object(field, PATH)?;
    let node_id = required_string_field(input, "pullRequestId", PATH)?;
    let client_mutation_id = required_string_field(input, "clientMutationId", PATH)?;

    let Some(pr) = mock_state.prs.iter_mut().find(|pr| pr.node_id == node_id) else {
        return Err(format!("Pull request node `{node_id}` does not exist"));
    };
    if pr.state != PullRequestState::Open {
        return Err(format!("Pull request node `{node_id}` is not OPEN and cannot be closed"));
    }
    pr.state = PullRequestState::Closed;

    let mut response = serde_json::Map::new();
    for field in selected_fields(&field.selection_set, PATH)? {
        match field.name.as_str() {
            "clientMutationId" => {
                response.insert(response_key(field), serde_json::json!(client_mutation_id));
            }
            "pullRequest" => {
                let mut pull_request = serde_json::Map::new();
                for field in selected_fields(&field.selection_set, "closePullRequest.pullRequest")?
                {
                    let value = match field.name.as_str() {
                        "number" => serde_json::json!(pr.number),
                        "id" => serde_json::json!(pr.node_id),
                        "state" => serde_json::json!(pr.state),
                        _ => unreachable!("request was checked by validate_close_field"),
                    };
                    pull_request.insert(response_key(field), value);
                }
                response.insert(response_key(field), serde_json::Value::Object(pull_request));
            }
            _ => unreachable!("request was checked by validate_close_field"),
        }
    }
    Ok(serde_json::Value::Object(response))
}

pub(super) fn handle_draft_pr(
    mock_state: &mut MockState,
    field: &executable::Field,
    branch_oid: &dyn Fn(&str) -> Result<Option<String>, String>,
) -> Result<serde_json::Value, String> {
    const PATH: &str = "convertPullRequestToDraft";
    let input = input_object(field, PATH)?;
    let node_id = required_string_field(input, "pullRequestId", PATH)?;
    let client_mutation_id = required_string_field(input, "clientMutationId", PATH)?;
    let Some(pr) = mock_state.prs.iter_mut().find(|pr| pr.node_id == node_id) else {
        return Err(format!("Pull request node `{node_id}` does not exist"));
    };
    if pr.state != PullRequestState::Open {
        return Err(format!("Pull request node `{node_id}` is not OPEN and cannot become a draft"));
    }
    let head_oid = branch_oid(&pr.head.name)?.unwrap_or_else(|| pr.head.oid.clone());
    let base_oid = branch_oid(&pr.base.name)?.unwrap_or_else(|| pr.base.oid.clone());
    pr.is_draft = true;

    let mut response = serde_json::Map::new();
    for field in selected_fields(&field.selection_set, PATH)? {
        match field.name.as_str() {
            "clientMutationId" => {
                response.insert(response_key(field), serde_json::json!(client_mutation_id));
            }
            "pullRequest" => {
                let mut pull_request = serde_json::Map::new();
                for field in
                    selected_fields(&field.selection_set, "convertPullRequestToDraft.pullRequest")?
                {
                    let value = match field.name.as_str() {
                        "number" => serde_json::json!(pr.number),
                        "id" => serde_json::json!(pr.node_id),
                        "state" => serde_json::json!(pr.state),
                        "isDraft" => serde_json::json!(pr.is_draft),
                        "headRefName" => serde_json::json!(pr.head.name),
                        "headRefOid" => serde_json::json!(head_oid),
                        "baseRefName" => serde_json::json!(pr.base.name),
                        "baseRefOid" => serde_json::json!(base_oid),
                        _ => unreachable!("request was checked by validate_draft_field"),
                    };
                    pull_request.insert(response_key(field), value);
                }
                response.insert(response_key(field), serde_json::Value::Object(pull_request));
            }
            _ => unreachable!("request was checked by validate_draft_field"),
        }
    }
    Ok(serde_json::Value::Object(response))
}

pub(super) fn handle_update_pr(
    mock_state: &mut MockState,
    field: &executable::Field,
    branch_oid: &dyn Fn(&str) -> Result<Option<String>, String>,
    concurrent_close: bool,
) -> Result<serde_json::Value, String> {
    const PATH: &str = "updatePullRequest";
    let input = input_object(field, PATH)?;
    let node_id = required_string_field(input, "pullRequestId", PATH)?;
    let client_mutation_id = required_string_field(input, "clientMutationId", PATH)?;
    let title = get_string_field(input, "title");
    let body = get_string_field(input, "body");
    let base = get_string_field(input, "baseRefName")
        .map(|base| -> Result<_, String> {
            let oid =
                branch_oid(&base)?.ok_or_else(|| format!("Base branch `{base}` does not exist"))?;
            Ok((base, oid))
        })
        .transpose()?;

    let Some(pr) = mock_state.prs.iter_mut().find(|pr| pr.node_id == node_id) else {
        return Err(format!("Pull request node `{node_id}` does not exist"));
    };
    if base.as_ref().map(|(name, _)| name.as_str()) == Some(pr.head.name.as_str()) {
        return Err("Pull request head and base branches must differ".to_string());
    }
    if let Some(title) = title {
        pr.title = title;
    }
    if let Some(body) = &body {
        pr.body = body.clone();
    }
    if let Some((name, oid)) = base {
        pr.base = BranchState { name, oid };
    }
    if concurrent_close {
        pr.state = PullRequestState::Closed;
    }

    let mut response = serde_json::Map::new();
    for field in selected_fields(&field.selection_set, PATH)? {
        match field.name.as_str() {
            "clientMutationId" => {
                response.insert(response_key(field), serde_json::json!(client_mutation_id));
            }
            "pullRequest" => {
                let mut pull_request = serde_json::Map::new();
                for field in selected_fields(&field.selection_set, "updatePullRequest.pullRequest")?
                {
                    match field.name.as_str() {
                        "number" => {
                            pull_request.insert(response_key(field), serde_json::json!(pr.number));
                        }
                        "id" => {
                            pull_request.insert(response_key(field), serde_json::json!(node_id));
                        }
                        "state" => {
                            pull_request.insert(response_key(field), serde_json::json!(pr.state));
                        }
                        "title" => {
                            pull_request.insert(response_key(field), serde_json::json!(pr.title));
                        }
                        "body" => {
                            pull_request.insert(response_key(field), serde_json::json!(pr.body));
                        }
                        "baseRefName" => {
                            pull_request
                                .insert(response_key(field), serde_json::json!(pr.base.name));
                        }
                        "baseRepository" => {
                            pull_request.insert(
                                response_key(field),
                                serde_json::json!({"id":"REPO_NODE_ID"}),
                            );
                        }
                        "headRefOid" | "baseRefOid" => {
                            let branch = if field.name == "headRefOid" {
                                &pr.head.name
                            } else {
                                &pr.base.name
                            };
                            let oid = branch_oid(branch)?
                                .ok_or_else(|| "Missing mutation receipt branch".to_owned())?;
                            pull_request.insert(response_key(field), serde_json::json!(oid));
                        }
                        "isDraft" => {
                            pull_request
                                .insert(response_key(field), serde_json::json!(pr.is_draft));
                        }
                        _ => unreachable!("request was checked by validate_update_field"),
                    }
                }
                response.insert(response_key(field), serde_json::Value::Object(pull_request));
            }
            _ => unreachable!("request was checked by validate_update_field"),
        }
    }
    Ok(serde_json::Value::Object(response))
}

pub(super) fn handle_create_pr(
    mock_state: &mut MockState,
    field: &executable::Field,
    branch_oid: &dyn Fn(&str) -> Result<Option<String>, String>,
) -> Result<serde_json::Value, String> {
    const PATH: &str = "createPullRequest";
    let input = input_object(field, PATH)?;
    let repository_id = required_string_field(input, "repositoryId", PATH)?;
    let head_repository_id = required_string_field(input, "headRepositoryId", PATH)?;
    let base = required_string_field(input, "baseRefName", PATH)?;
    let head = required_string_field(input, "headRefName", PATH)?;
    let title = required_string_field(input, "title", PATH)?;
    let body = required_string_field(input, "body", PATH)?;
    if get_bool_field(input, "draft") != Some(true) {
        return Err("The mock GitHub API requires draft pull request creation".to_string());
    }
    let client_mutation_id = required_string_field(input, "clientMutationId", PATH)?;

    if repository_id != "REPO_NODE_ID" {
        return Err(format!("Repository node `{repository_id}` does not exist"));
    }
    if head_repository_id != repository_id {
        return Err(format!("Head repository node `{head_repository_id}` does not exist"));
    }
    if base == head {
        return Err("Pull request head and base branches must differ".to_string());
    }
    let base_oid =
        branch_oid(&base)?.ok_or_else(|| format!("Base branch `{base}` does not exist"))?;
    let head_oid =
        branch_oid(&head)?.ok_or_else(|| format!("Head branch `{head}` does not exist"))?;
    let number = mock_state.prs.iter().map(|pr| pr.number).max().unwrap_or(0) + 1;
    let mut entry = PrEntry::mock(MockPrArgs {
        number,
        title: title.clone(),
        body: body.clone(),
        head: head.clone(),
        base: base.clone(),
    });
    entry.is_draft = true;
    entry.head.oid = head_oid.clone();
    entry.base.oid = base_oid.clone();
    let node_id = entry.node_id.clone();
    mock_state.prs.push(entry);

    let mut response = serde_json::Map::new();
    for field in selected_fields(&field.selection_set, PATH)? {
        match field.name.as_str() {
            "clientMutationId" => {
                response.insert(response_key(field), serde_json::json!(client_mutation_id));
            }
            "pullRequest" => {
                let mut pull_request = serde_json::Map::new();
                for field in selected_fields(&field.selection_set, "createPullRequest.pullRequest")?
                {
                    let value = match field.name.as_str() {
                        "number" => serde_json::json!(number),
                        "id" => serde_json::json!(node_id),
                        "state" => serde_json::json!("OPEN"),
                        "title" => serde_json::json!(title),
                        "body" => serde_json::json!(body),
                        "isDraft" => serde_json::json!(true),
                        "headRefName" => serde_json::json!(head),
                        "headRefOid" => serde_json::json!(head_oid),
                        "headRepository" | "baseRepository" => {
                            serde_json::json!({ "id": repository_id })
                        }
                        "baseRefName" => serde_json::json!(base),
                        "baseRefOid" => serde_json::json!(base_oid),
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

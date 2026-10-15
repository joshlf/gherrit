use std::collections::HashSet;

use apollo_compiler::{ast, executable};

use super::{
    codec::{argument, resolve_string_argument, response_key, selected_fields, GraphQlVariables},
    MockState, PrEntry, MAX_PULL_REQUEST_CANDIDATES,
};

pub(super) fn handle_repository_query(
    mock_state: &MockState,
    field: &executable::Field,
    variables: &GraphQlVariables,
) -> Result<serde_json::Value, String> {
    const PATH: &str = "repository";
    let owner = resolve_string_argument(field, "owner", PATH, variables)?;
    let name = resolve_string_argument(field, "name", PATH, variables)?;

    if owner != mock_state.repo_owner || name != mock_state.repo_name {
        return Ok(serde_json::Value::Null);
    }

    let mut repo_data = serde_json::Map::new();

    for field in selected_fields(&field.selection_set, PATH)? {
        match field.name.as_str() {
            "pullRequests" => {
                let head = resolve_string_argument(
                    field,
                    "headRefName",
                    "repository.pullRequests",
                    variables,
                )?;
                let states = argument(field, "states")
                    .and_then(|value| match value {
                        ast::Value::List(values) => Some(values),
                        _ => None,
                    })
                    .expect("request was checked by validate_pull_requests_field");
                let states = states
                    .iter()
                    .map(|value| match &**value {
                        ast::Value::Enum(value) => value.as_str(),
                        _ => unreachable!("request was checked by validate_pull_requests_field"),
                    })
                    .collect::<HashSet<_>>();
                let matching_prs = mock_state
                    .prs
                    .iter()
                    .filter(|pr| pr.head.ref_field == head && states.contains(pr.state.as_str()))
                    .collect::<Vec<_>>();
                let has_next_page = matching_prs.len() > MAX_PULL_REQUEST_CANDIDATES;

                let mut connection = serde_json::Map::new();
                for field in selected_fields(&field.selection_set, "repository.pullRequests")? {
                    match field.name.as_str() {
                        "nodes" => {
                            let nodes = matching_prs
                                .iter()
                                .take(MAX_PULL_REQUEST_CANDIDATES)
                                .map(|pr| {
                                    project_pr_node(
                                        pr,
                                        mock_state.cross_repository_prs.contains(&pr.number),
                                        &field.selection_set,
                                    )
                                })
                                .collect::<Result<Vec<_>, _>>()?;
                            connection.insert(response_key(field), serde_json::json!(nodes));
                        }
                        "pageInfo" => {
                            let mut page_info = serde_json::Map::new();
                            for field in selected_fields(
                                &field.selection_set,
                                "repository.pullRequests.pageInfo",
                            )? {
                                match field.name.as_str() {
                                    "hasNextPage" => {
                                        page_info.insert(
                                            response_key(field),
                                            serde_json::json!(has_next_page),
                                        );
                                    }
                                    _ => unreachable!(
                                        "request was checked by validate_pull_requests_field"
                                    ),
                                }
                            }
                            connection
                                .insert(response_key(field), serde_json::Value::Object(page_info));
                        }
                        _ => unreachable!("request was checked by validate_pull_requests_field"),
                    }
                }
                repo_data.insert(response_key(field), serde_json::Value::Object(connection));
            }
            "id" => {
                repo_data.insert(
                    response_key(field),
                    serde_json::Value::String("REPO_NODE_ID".to_string()),
                );
            }
            _ => unreachable!("request was checked by validate_repository_field"),
        }
    }

    Ok(serde_json::Value::Object(repo_data))
}

fn project_pr_node(
    pr: &PrEntry,
    is_cross_repository: bool,
    selection_set: &executable::SelectionSet,
) -> Result<serde_json::Value, String> {
    let mut node = serde_json::Map::new();
    for field in selected_fields(selection_set, "repository.pullRequests.nodes")? {
        let value = match field.name.as_str() {
            "number" => serde_json::json!(pr.number),
            "id" => serde_json::json!(pr.node_id),
            "title" => serde_json::json!(pr.title),
            "body" => serde_json::json!(pr.body),
            "baseRefName" => serde_json::json!(pr.base.ref_field),
            "state" => serde_json::json!(pr.state),
            "isCrossRepository" => serde_json::json!(is_cross_repository),
            _ => unreachable!("request was checked by validate_pull_requests_field"),
        };
        node.insert(response_key(field), value);
    }
    Ok(serde_json::Value::Object(node))
}

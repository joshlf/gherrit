//! Query contracts of the independent HTTP fake.

use super::*;

pub(super) fn remote_branch_oids(app_state: &AppState) -> Result<HashMap<String, String>, String> {
    let output = app_state
        .test_environment
        .command(&app_state.system_git)
        .arg("--git-dir")
        .arg(&app_state.remote_path)
        .args(["for-each-ref", "--format=%(refname)%00%(objectname)", "refs/heads"])
        .output()
        .map_err(|error| format!("Failed to inspect remote Git branches: {error}"))?;
    if !output.status.success() {
        return Err(format!(
            "Inspecting remote Git branches exited with {:?}",
            output.status.code()
        ));
    }

    let mut branches = HashMap::new();
    for record in output.stdout.split(|byte| *byte == b'\n').filter(|record| !record.is_empty()) {
        let record = record.strip_suffix(b"\r").unwrap_or(record);
        let Some(separator) = record.iter().position(|byte| *byte == 0) else {
            return Err("Remote Git branch observation is missing its object ID".to_owned());
        };
        let reference = std::str::from_utf8(&record[..separator])
            .map_err(|_| "Remote Git branch name is not UTF-8".to_owned())?;
        let branch = reference
            .strip_prefix("refs/heads/")
            .ok_or_else(|| format!("Remote Git reported non-branch ref `{reference}`"))?;
        let oid = std::str::from_utf8(&record[separator + 1..])
            .map_err(|_| format!("Remote Git branch `{branch}` has a non-UTF-8 object ID"))?;
        if oid.len() != 40 || !oid.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            return Err(format!("Remote Git branch `{branch}` has an invalid object ID"));
        }
        if branches.insert(branch.to_owned(), oid.to_owned()).is_some() {
            return Err(format!("Remote Git reported branch `{branch}` more than once"));
        }
    }
    Ok(branches)
}

pub(super) fn repository_default_branch(
    app_state: &AppState,
    mock_state: &MockState,
) -> Result<(String, String), String> {
    if let Some(default_branch) = &mock_state.github_default_branch {
        return Ok(default_branch.clone());
    }

    let run = |arguments: &[&str]| {
        app_state
            .test_environment
            .command(&app_state.system_git)
            .arg("--git-dir")
            .arg(&app_state.remote_path)
            .args(arguments)
            .output()
            .map_err(|error| format!("Failed to inspect remote default branch: {error}"))
    };
    let symbolic = run(&["symbolic-ref", "HEAD"])?;
    let tip = run(&["rev-parse", "HEAD"])?;
    if !symbolic.status.success() || !tip.status.success() {
        return Err("The mock repository has no default branch".to_owned());
    }
    let symbolic = std::str::from_utf8(&symbolic.stdout)
        .map_err(|_| "The mock repository default branch is not UTF-8".to_owned())?
        .trim_end_matches(['\r', '\n']);
    let name = symbolic
        .strip_prefix("refs/heads/")
        .ok_or_else(|| "The mock repository HEAD is not a local branch".to_owned())?;
    let tip = std::str::from_utf8(&tip.stdout)
        .map_err(|_| "The mock repository default branch tip is not UTF-8".to_owned())?
        .trim_end_matches(['\r', '\n']);
    Ok((name.to_owned(), tip.to_owned()))
}

pub(super) fn handle_repository_query(
    mock_state: &MockState,
    field: &executable::Field,
    variables: &GraphQlVariables,
    default_branch: &dyn Fn() -> Result<(String, String), String>,
    branch_oid: &dyn Fn(&str) -> Result<Option<String>, String>,
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
            "defaultBranchRef" => {
                let (name, oid) = default_branch()?;
                let mut branch = serde_json::Map::new();
                for field in selected_fields(&field.selection_set, "repository.defaultBranchRef")? {
                    let value = match field.name.as_str() {
                        "name" => serde_json::json!(name),
                        "target" => {
                            let mut target = serde_json::Map::new();
                            for field in selected_fields(
                                &field.selection_set,
                                "repository.defaultBranchRef.target",
                            )? {
                                match field.name.as_str() {
                                    "oid" => {
                                        target.insert(response_key(field), serde_json::json!(oid));
                                    }
                                    _ => unreachable!(
                                        "request was checked by validate_default_branch_ref_field"
                                    ),
                                }
                            }
                            serde_json::Value::Object(target)
                        }
                        _ => {
                            unreachable!("request was checked by validate_default_branch_ref_field")
                        }
                    };
                    branch.insert(response_key(field), value);
                }
                repo_data.insert(response_key(field), serde_json::Value::Object(branch));
            }
            "pullRequests" => {
                let head = resolve_string_argument(
                    field,
                    "headRefName",
                    "repository.pullRequests",
                    variables,
                )?;
                let after = argument(field, "after")
                    .map(|_| {
                        resolve_string_argument(
                            field,
                            "after",
                            "repository.pullRequests",
                            variables,
                        )
                    })
                    .transpose()?;
                let matching_prs = mock_state
                    .prs
                    .iter()
                    .filter(|pr| pr.head.name == head && pr.state == PullRequestState::Open)
                    .collect::<Vec<_>>();
                let offset = after
                    .as_deref()
                    .map(|raw_cursor| {
                        let (cursor_head, offset) = raw_cursor
                            .strip_prefix("cursor:")
                            .and_then(|cursor| cursor.rsplit_once(':'))
                            .ok_or_else(|| format!("Invalid pull request cursor `{raw_cursor}`"))?;
                        if cursor_head != head {
                            return Err(format!(
                                "Pull request cursor `{raw_cursor}` belongs to another local head"
                            ));
                        }
                        offset
                            .parse::<usize>()
                            .map_err(|_| format!("Invalid pull request cursor `{raw_cursor}`"))
                    })
                    .transpose()?
                    .unwrap_or(0);
                let page = matching_prs
                    .iter()
                    .skip(offset)
                    .take(LOCAL_PULL_REQUEST_PAGE_SIZE)
                    .collect::<Vec<_>>();
                let has_next_page = matching_prs.len() > offset + page.len();

                let mut connection = serde_json::Map::new();
                for field in selected_fields(&field.selection_set, "repository.pullRequests")? {
                    match field.name.as_str() {
                        "nodes" => {
                            let nodes = page
                                .iter()
                                .map(|pr| {
                                    let is_cross_repository =
                                        mock_state.cross_repository_prs.contains(&pr.number);
                                    let (head_oid, base_oid) = if is_cross_repository {
                                        (pr.head.oid.clone(), pr.base.oid.clone())
                                    } else {
                                        (
                                            branch_oid(&pr.head.name)?
                                                .unwrap_or_else(|| pr.head.oid.clone()),
                                            branch_oid(&pr.base.name)?
                                                .unwrap_or_else(|| pr.base.oid.clone()),
                                        )
                                    };
                                    project_pr_node(
                                        pr,
                                        is_cross_repository,
                                        &head_oid,
                                        &base_oid,
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
                                    "endCursor" => {
                                        let cursor = has_next_page.then(|| {
                                            format!("cursor:{head}:{}", offset + page.len())
                                        });
                                        page_info
                                            .insert(response_key(field), serde_json::json!(cursor));
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

pub(super) fn project_pr_node(
    pr: &PrEntry,
    is_cross_repository: bool,
    head_oid: &str,
    base_oid: &str,
    selection_set: &executable::SelectionSet,
) -> Result<serde_json::Value, String> {
    let mut node = serde_json::Map::new();
    for field in selected_fields(selection_set, "repository.pullRequests.nodes")? {
        let value = match field.name.as_str() {
            "number" => serde_json::json!(pr.number),
            "id" => serde_json::json!(pr.node_id),
            "title" => serde_json::json!(pr.title),
            "body" => serde_json::json!(pr.body),
            "baseRefName" => serde_json::json!(pr.base.name),
            "baseRefOid" => serde_json::json!(base_oid),
            "headRefName" => serde_json::json!(pr.head.name),
            "headRefOid" => serde_json::json!(head_oid),
            "state" => serde_json::json!(pr.state),
            "isCrossRepository" => serde_json::json!(is_cross_repository),
            "autoMergeRequest" => {
                if pr.auto_merge {
                    serde_json::json!({ "enabledAt": "2026-01-01T00:00:00Z" })
                } else {
                    serde_json::Value::Null
                }
            }
            "isInMergeQueue" => serde_json::json!(pr.in_merge_queue),
            "isDraft" => serde_json::json!(pr.is_draft),
            _ => unreachable!("request was checked by validate_pull_requests_field"),
        };
        node.insert(response_key(field), value);
    }
    Ok(serde_json::Value::Object(node))
}

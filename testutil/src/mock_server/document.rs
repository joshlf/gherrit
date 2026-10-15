//! Document contracts of the independent HTTP fake.

use super::*;

pub(super) fn graphql_operations(document: &ExecutableDocument) -> Vec<GraphQlOperation> {
    document
        .operations
        .iter()
        .flat_map(|operation| operation.selection_set.selections.iter())
        .filter_map(|selection| {
            let executable::Selection::Field(field) = selection else { return None };
            match field.name.as_str() {
                "repository" => Some(GraphQlOperation::Query),
                "createPullRequest" => Some(GraphQlOperation::CreatePr),
                "convertPullRequestToDraft" => Some(GraphQlOperation::DraftPr),
                "closePullRequest" => Some(GraphQlOperation::ClosePr),
                "updatePullRequest" => Some(GraphQlOperation::UpdatePr),
                _ => None,
            }
        })
        .collect()
}

pub(super) type GraphQlVariables = Option<serde_json::Map<String, serde_json::Value>>;

pub(super) fn graphql_variables(payload: &serde_json::Value) -> Result<GraphQlVariables, String> {
    match payload.get("variables") {
        None => Ok(None),
        Some(serde_json::Value::Object(variables)) => Ok(Some(variables.clone())),
        Some(_) => Err("Invalid GraphQL payload: `variables` must be an object".to_string()),
    }
}

pub(super) fn response_key(field: &executable::Field) -> String {
    field
        .alias
        .as_ref()
        .map(|alias| alias.as_str())
        .unwrap_or_else(|| field.name.as_str())
        .to_string()
}

pub(super) fn selected_fields<'a>(
    selection_set: &'a executable::SelectionSet,
    path: &str,
) -> Result<Vec<&'a executable::Field>, String> {
    let fields: Vec<_> = selection_set
        .selections
        .iter()
        .map(|selection| {
            let executable::Selection::Field(field) = selection else {
                return Err(format!("The mock GitHub API does not support fragments at `{path}`"));
            };
            if !field.directives.is_empty() {
                return Err(format!(
                    "The mock GitHub API does not support directives at `{path}.{}`",
                    field.name
                ));
            }
            Ok(field.as_ref())
        })
        .collect::<Result<_, _>>()?;
    let mut response_keys = HashSet::new();
    for field in &fields {
        let key = response_key(field);
        if !response_keys.insert(key.clone()) {
            return Err(format!(
                "The mock GitHub API does not support duplicate response key `{key}` at `{path}`"
            ));
        }
    }
    Ok(fields)
}

pub(super) fn validate_argument_names(
    field: &executable::Field,
    path: &str,
    allowed: &[&str],
) -> Result<(), String> {
    for argument in &field.arguments {
        if !allowed.contains(&argument.name.as_str()) {
            return Err(format!(
                "The mock GitHub API does not support argument `{path}({}: ...)`",
                argument.name
            ));
        }
    }
    Ok(())
}

pub(super) fn validate_scalar_fields(
    selection_set: &executable::SelectionSet,
    path: &str,
    allowed: &[&str],
) -> Result<(), String> {
    for field in selected_fields(selection_set, path)? {
        if !allowed.contains(&field.name.as_str()) {
            return Err(format!(
                "The mock GitHub API does not support field `{path}.{}`",
                field.name
            ));
        }
    }
    Ok(())
}

pub(super) fn validate_exact_fields<'a>(
    selection_set: &'a executable::SelectionSet,
    path: &str,
    required: &[&str],
) -> Result<Vec<&'a executable::Field>, String> {
    let fields = selected_fields(selection_set, path)?;
    let names = fields.iter().map(|field| field.name.as_str()).collect::<HashSet<_>>();
    let required_names = required.iter().copied().collect::<HashSet<_>>();
    if fields.len() != required.len()
        || names != required_names
        || fields.iter().any(|field| field.alias.is_some())
    {
        return Err(format!(
            "The mock GitHub API requires exactly unaliased fields {} at `{path}`",
            required.join(", ")
        ));
    }
    Ok(fields)
}

pub(super) fn input_object<'a>(
    field: &'a executable::Field,
    path: &str,
) -> Result<&'a [(Name, Node<ast::Value>)], String> {
    extract_input_field(field, "input")
        .map(Vec::as_slice)
        .ok_or_else(|| format!("The mock GitHub API requires an inline object at `{path}(input:)`"))
}

pub(super) fn validate_input_fields(
    input: &[(Name, Node<ast::Value>)],
    path: &str,
    string_fields: &[&str],
    boolean_fields: &[&str],
) -> Result<(), String> {
    let mut names = HashSet::new();
    for (name, value) in input {
        if !names.insert(name.as_str()) {
            return Err(format!(
                "The mock GitHub API does not support duplicate input field `{path}.input.{name}`"
            ));
        }
        let expected_type = if string_fields.contains(&name.as_str()) {
            "string"
        } else if boolean_fields.contains(&name.as_str()) {
            "boolean"
        } else {
            return Err(format!(
                "The mock GitHub API does not support input field `{path}.input.{name}`"
            ));
        };
        let has_expected_type = match expected_type {
            "string" => matches!(&**value, ast::Value::String(_)),
            "boolean" => matches!(&**value, ast::Value::Boolean(_)),
            _ => unreachable!("the expected input type is known"),
        };
        if !has_expected_type {
            return Err(format!(
                "The mock GitHub API requires an inline {expected_type} value at \
                 `{path}.input.{name}`"
            ));
        }
    }
    Ok(())
}

pub(super) fn argument<'a>(field: &'a executable::Field, name: &str) -> Option<&'a ast::Value> {
    field.arguments.iter().find(|argument| argument.name == name).map(|argument| &*argument.value)
}

pub(super) fn resolve_string_argument(
    field: &executable::Field,
    name: &str,
    path: &str,
    variables: &GraphQlVariables,
) -> Result<String, String> {
    let value = argument(field, name)
        .ok_or_else(|| format!("Missing GraphQL argument `{path}({name}: ...)`"))?;
    match value {
        ast::Value::String(value) => Ok(value.clone()),
        ast::Value::Variable(variable) => variables
            .as_ref()
            .and_then(|variables| variables.get(variable.as_str()))
            .and_then(serde_json::Value::as_str)
            .map(ToString::to_string)
            .ok_or_else(|| {
                format!("Missing or non-string GraphQL variable `${variable}` for `{path}.{name}`")
            }),
        _ => Err(format!("The mock GitHub API requires a string at `{path}({name}: ...)`")),
    }
}

pub(super) fn validate_pull_request_states(field: &executable::Field) -> Result<(), String> {
    const PATH: &str = "repository.pullRequests";
    let states = argument(field, "states")
        .and_then(|value| match value {
            ast::Value::List(values) => Some(values),
            _ => None,
        })
        .ok_or_else(|| format!("The mock GitHub API requires `{PATH}(states: ...)`"))?;
    let states = states
        .iter()
        .map(|value| match &**value {
            ast::Value::Enum(value) => Some(value.as_str()),
            _ => None,
        })
        .collect::<Option<Vec<_>>>()
        .ok_or_else(|| {
            "The mock GitHub API requires an exact production pull request lifecycle query"
                .to_string()
        })?;
    if states != ["OPEN"] {
        return Err("The mock GitHub API requires states: [OPEN]".to_string());
    }
    Ok(())
}

pub(super) fn validate_pull_requests_field(
    field: &executable::Field,
    variables: &GraphQlVariables,
) -> Result<(), String> {
    const PATH: &str = "repository.pullRequests";
    validate_argument_names(field, PATH, &["headRefName", "first", "after", "states"])?;

    let first = argument(field, "first")
        .and_then(|value| match value {
            ast::Value::Int(value) => Some(value.as_str()),
            _ => None,
        })
        .ok_or_else(|| format!("The mock GitHub API requires `{PATH}(first: 1)`"))?;
    if first != LOCAL_PULL_REQUEST_PAGE_SIZE.to_string() {
        return Err(format!(
            "The mock GitHub API requires `{PATH}(first: {LOCAL_PULL_REQUEST_PAGE_SIZE})`"
        ));
    }

    validate_pull_request_states(field)?;

    if field.alias.is_none() {
        return Err(format!(
            "The mock GitHub API requires every exact `{PATH}` connection to be aliased"
        ));
    }
    if resolve_string_argument(field, "headRefName", PATH, variables)?.is_empty() {
        return Err("The mock GitHub API requires a nonempty local headRefName".to_string());
    }
    if argument(field, "after").is_some()
        && resolve_string_argument(field, "after", PATH, variables)?.is_empty()
    {
        return Err("The mock GitHub API requires a nonempty pagination cursor".to_string());
    }

    for field in validate_exact_fields(&field.selection_set, PATH, &["nodes", "pageInfo"])? {
        match field.name.as_str() {
            "nodes" => {
                let fields = validate_exact_fields(
                    &field.selection_set,
                    "repository.pullRequests.nodes",
                    &[
                        "number",
                        "id",
                        "title",
                        "body",
                        "baseRefName",
                        "baseRefOid",
                        "headRefName",
                        "headRefOid",
                        "state",
                        "isCrossRepository",
                        "autoMergeRequest",
                        "isInMergeQueue",
                        "isDraft",
                    ],
                )?;
                let auto_merge = fields
                    .into_iter()
                    .find(|field| field.name == "autoMergeRequest")
                    .expect("the exact pull request field set contains autoMergeRequest");
                validate_exact_fields(
                    &auto_merge.selection_set,
                    "repository.pullRequests.nodes.autoMergeRequest",
                    &["enabledAt"],
                )?;
            }
            "pageInfo" => {
                validate_exact_fields(
                    &field.selection_set,
                    "repository.pullRequests.pageInfo",
                    &["hasNextPage", "endCursor"],
                )?;
            }
            _ => unreachable!("the exact connection field set was checked"),
        }
    }
    Ok(())
}

pub(super) fn validate_default_branch_ref_field(field: &executable::Field) -> Result<(), String> {
    const PATH: &str = "repository.defaultBranchRef";
    for field in validate_exact_fields(&field.selection_set, PATH, &["name", "target"])? {
        match field.name.as_str() {
            "name" => {}
            "target" => validate_scalar_fields(
                &field.selection_set,
                "repository.defaultBranchRef.target",
                &["oid"],
            )?,
            _ => {
                return Err(format!(
                    "The mock GitHub API does not support field `{PATH}.{}`",
                    field.name
                ));
            }
        }
    }
    Ok(())
}

pub(super) fn validate_repository_field(
    field: &executable::Field,
    variables: &GraphQlVariables,
) -> Result<(), String> {
    const PATH: &str = "repository";
    validate_argument_names(field, PATH, &["owner", "name"])?;
    resolve_string_argument(field, "owner", PATH, variables)?;
    resolve_string_argument(field, "name", PATH, variables)?;

    let fields = selected_fields(&field.selection_set, PATH)?;
    let pull_requests =
        fields.iter().filter(|field| field.name == "pullRequests").copied().collect::<Vec<_>>();
    if pull_requests.is_empty() {
        return Err("The mock GitHub API requires a production pull request query".to_string());
    }
    let repository_facts = fields
        .iter()
        .filter(|field| matches!(field.name.as_str(), "id" | "defaultBranchRef"))
        .copied()
        .collect::<Vec<_>>();
    match repository_facts.len() {
        0 => {}
        2 if ["id", "defaultBranchRef"].iter().all(|name| {
            repository_facts.iter().any(|field| field.name == *name && field.alias.is_none())
        }) => {}
        _ => {
            return Err("The mock GitHub API requires exactly one unaliased repository fact pair"
                .to_string());
        }
    }

    for field in fields {
        match field.name.as_str() {
            "id" => {}
            "defaultBranchRef" => validate_default_branch_ref_field(field)?,
            "pullRequests" => {
                validate_pull_requests_field(field, variables)?;
            }
            _ => {
                return Err(format!(
                    "The mock GitHub API does not support field `{PATH}.{}`",
                    field.name
                ));
            }
        }
    }
    Ok(())
}

pub(super) fn validate_create_field(field: &executable::Field) -> Result<(), String> {
    const PATH: &str = "createPullRequest";
    validate_argument_names(field, PATH, &["input"])?;
    let input = input_object(field, PATH)?;
    validate_input_fields(
        input,
        PATH,
        &[
            "repositoryId",
            "headRepositoryId",
            "baseRefName",
            "headRefName",
            "title",
            "body",
            "clientMutationId",
        ],
        &["draft"],
    )?;
    for required in [
        "repositoryId",
        "headRepositoryId",
        "baseRefName",
        "headRefName",
        "title",
        "body",
        "clientMutationId",
    ] {
        required_string_field(input, required, PATH)?;
    }
    if get_bool_field(input, "draft") != Some(true) {
        return Err(
            "The mock GitHub API requires `createPullRequest.input.draft: true`".to_string()
        );
    }

    for field in
        validate_exact_fields(&field.selection_set, PATH, &["clientMutationId", "pullRequest"])?
    {
        match field.name.as_str() {
            "clientMutationId" => {}
            "pullRequest" => {
                for selected in validate_exact_fields(
                    &field.selection_set,
                    "createPullRequest.pullRequest",
                    &[
                        "number",
                        "id",
                        "state",
                        "title",
                        "body",
                        "isDraft",
                        "headRefName",
                        "headRefOid",
                        "headRepository",
                        "baseRefName",
                        "baseRefOid",
                        "baseRepository",
                    ],
                )? {
                    match selected.name.as_str() {
                        "headRepository" | "baseRepository" => validate_exact_fields(
                            &selected.selection_set,
                            "createPullRequest.pullRequest.repository",
                            &["id"],
                        )
                        .map(|_| ())?,
                        _ => validate_scalar_fields(
                            &selected.selection_set,
                            "createPullRequest.pullRequest.scalar",
                            &[],
                        )?,
                    }
                }
            }
            _ => {
                return Err(format!(
                    "The mock GitHub API does not support field `{PATH}.{}`",
                    field.name
                ));
            }
        }
    }
    Ok(())
}

pub(super) fn validate_update_field(field: &executable::Field) -> Result<(), String> {
    const PATH: &str = "updatePullRequest";
    validate_argument_names(field, PATH, &["input"])?;
    let input = input_object(field, PATH)?;
    validate_input_fields(
        input,
        PATH,
        &["pullRequestId", "baseRefName", "title", "body", "clientMutationId"],
        &[],
    )?;
    required_string_field(input, "pullRequestId", PATH)?;
    required_string_field(input, "clientMutationId", PATH)?;
    if !["baseRefName", "title", "body"].iter().any(|name| get_string_field(input, name).is_some())
    {
        return Err("The mock GitHub API requires at least one pull request update".to_string());
    }
    for field in
        validate_exact_fields(&field.selection_set, PATH, &["clientMutationId", "pullRequest"])?
    {
        match field.name.as_str() {
            "clientMutationId" => {}
            "pullRequest" => {
                let selected = selected_fields(&field.selection_set, PATH)?;
                let mut expected = vec!["number", "id", "state"];
                if selected.len() != 3 {
                    expected.extend(["baseRefName", "isDraft", "headRefOid", "baseRefOid"]);
                    if selected.iter().any(|field| field.name == "baseRepository") {
                        expected.push("baseRepository");
                    }
                    for optional in ["title", "body"] {
                        if selected.iter().any(|field| field.name == optional) {
                            expected.push(optional);
                        }
                    }
                }
                validate_exact_fields(
                    &field.selection_set,
                    "updatePullRequest.pullRequest",
                    &expected,
                )
            }
            .and_then(|fields| {
                for field in &fields {
                    if field.name == "baseRepository" {
                        validate_exact_fields(
                            &field.selection_set,
                            "updatePullRequest.pullRequest.baseRepository",
                            &["id"],
                        )?;
                    }
                }
                fields
                    .into_iter()
                    .find(|field| field.name == "state")
                    .map(|field| {
                        validate_scalar_fields(
                            &field.selection_set,
                            "updatePullRequest.pullRequest.state",
                            &[],
                        )
                    })
                    .unwrap_or(Ok(()))
            })?,
            _ => {
                return Err(format!(
                    "The mock GitHub API does not support field `{PATH}.{}`",
                    field.name
                ));
            }
        }
    }
    Ok(())
}

pub(super) fn validate_close_field(field: &executable::Field) -> Result<(), String> {
    const PATH: &str = "closePullRequest";
    validate_argument_names(field, PATH, &["input"])?;
    let input = input_object(field, PATH)?;
    validate_input_fields(input, PATH, &["pullRequestId", "clientMutationId"], &[])?;
    required_string_field(input, "pullRequestId", PATH)?;
    required_string_field(input, "clientMutationId", PATH)?;

    for field in
        validate_exact_fields(&field.selection_set, PATH, &["clientMutationId", "pullRequest"])?
    {
        match field.name.as_str() {
            "clientMutationId" => {}
            "pullRequest" => {
                for field in validate_exact_fields(
                    &field.selection_set,
                    "closePullRequest.pullRequest",
                    &["number", "id", "state"],
                )? {
                    validate_scalar_fields(
                        &field.selection_set,
                        "closePullRequest.pullRequest.scalar",
                        &[],
                    )?;
                }
            }
            _ => unreachable!("the exact close field set was checked"),
        }
    }
    Ok(())
}

pub(super) fn validate_draft_field(field: &executable::Field) -> Result<(), String> {
    const PATH: &str = "convertPullRequestToDraft";
    validate_argument_names(field, PATH, &["input"])?;
    let input = input_object(field, PATH)?;
    validate_input_fields(input, PATH, &["pullRequestId", "clientMutationId"], &[])?;
    required_string_field(input, "pullRequestId", PATH)?;
    required_string_field(input, "clientMutationId", PATH)?;
    for field in
        validate_exact_fields(&field.selection_set, PATH, &["clientMutationId", "pullRequest"])?
    {
        match field.name.as_str() {
            "clientMutationId" => {}
            "pullRequest" => {
                validate_exact_fields(
                    &field.selection_set,
                    "convertPullRequestToDraft.pullRequest",
                    &[
                        "number",
                        "id",
                        "state",
                        "isDraft",
                        "headRefName",
                        "headRefOid",
                        "baseRefName",
                        "baseRefOid",
                    ],
                )?;
            }
            _ => unreachable!("the exact draft field set was checked"),
        }
    }
    Ok(())
}

pub(super) fn validate_supported_document(
    document: &ExecutableDocument,
    variables: &GraphQlVariables,
) -> Result<(), String> {
    if document.operations.len() != 1 {
        return Err("The mock GitHub API supports exactly one GraphQL operation".to_string());
    }
    if !document.fragments.is_empty() {
        return Err("The mock GitHub API does not support GraphQL fragments".to_string());
    }

    let operation = document.operations.iter().next().unwrap();
    if !operation.directives.is_empty() {
        return Err("The mock GitHub API does not support operation directives".to_string());
    }
    let fields = selected_fields(&operation.selection_set, "operation")?;
    let repository_count = fields.iter().filter(|field| field.name == "repository").count();
    if repository_count != 0 && (repository_count != 1 || fields.len() != 1) {
        return Err(
            "The mock GitHub API requires exactly one repository root field per query".to_string()
        );
    }
    let has_create = fields.iter().any(|field| field.name == "createPullRequest");
    let has_projection = fields
        .iter()
        .any(|field| matches!(field.name.as_str(), "closePullRequest" | "updatePullRequest"));
    if has_create && has_projection {
        return Err("The mock GitHub API does not mix pull request creation with final projection"
            .to_string());
    }
    for field in fields {
        match field.name.as_str() {
            "repository" => validate_repository_field(field, variables)?,
            "createPullRequest" => validate_create_field(field)?,
            "convertPullRequestToDraft" => validate_draft_field(field)?,
            "closePullRequest" => validate_close_field(field)?,
            "updatePullRequest" => validate_update_field(field)?,
            _ => {
                return Err(format!(
                    "The mock GitHub API does not support root field `{}`",
                    field.name
                ));
            }
        }
    }
    Ok(())
}

pub(super) fn local_pull_request_heads(
    document: &ExecutableDocument,
    variables: &GraphQlVariables,
) -> Result<Vec<String>, String> {
    document
        .operations
        .iter()
        .flat_map(|operation| &operation.selection_set.selections)
        .filter_map(|selection| {
            let executable::Selection::Field(field) = selection else { return None };
            (field.name == "repository").then_some(&field.selection_set.selections)
        })
        .flatten()
        .filter_map(|selection| {
            let executable::Selection::Field(field) = selection else { return None };
            (field.name == "pullRequests").then_some(field)
        })
        .map(|field| {
            resolve_string_argument(field, "headRefName", "repository.pullRequests", variables)
        })
        .collect()
}

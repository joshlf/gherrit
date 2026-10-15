use std::collections::HashSet;

use apollo_compiler::{ast, executable, ExecutableDocument, Name, Node};

use super::MAX_PULL_REQUEST_CANDIDATES;
use crate::GraphQlOperation;

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

fn validate_argument_names(
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

fn validate_scalar_fields(
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

pub(super) fn input_object<'a>(
    field: &'a executable::Field,
    path: &str,
) -> Result<&'a [(Name, Node<ast::Value>)], String> {
    extract_input_field(field, "input")
        .map(Vec::as_slice)
        .ok_or_else(|| format!("The mock GitHub API requires an inline object at `{path}(input:)`"))
}

fn validate_input_fields(
    input: &[(Name, Node<ast::Value>)],
    path: &str,
    allowed: &[&str],
) -> Result<(), String> {
    for (name, value) in input {
        if !allowed.contains(&name.as_str()) {
            return Err(format!(
                "The mock GitHub API does not support input field `{path}.input.{name}`"
            ));
        }
        if !matches!(&**value, ast::Value::String(_)) {
            return Err(format!(
                "The mock GitHub API only supports inline string values at `{path}.input.{name}`"
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

fn validate_pull_requests_field(field: &executable::Field) -> Result<(), String> {
    const PATH: &str = "repository.pullRequests";
    validate_argument_names(field, PATH, &["headRefName", "first", "states"])?;

    let first = argument(field, "first")
        .and_then(|value| match value {
            ast::Value::Int(value) => Some(value.as_str()),
            _ => None,
        })
        .ok_or_else(|| {
            format!("The mock GitHub API requires `{PATH}(first: {MAX_PULL_REQUEST_CANDIDATES})`")
        })?;
    if first != MAX_PULL_REQUEST_CANDIDATES.to_string() {
        return Err(format!(
            "The mock GitHub API only supports `{PATH}(first: {MAX_PULL_REQUEST_CANDIDATES})`"
        ));
    }

    let states = argument(field, "states")
        .and_then(|value| match value {
            ast::Value::List(values) => Some(values),
            _ => None,
        })
        .ok_or_else(|| format!("The mock GitHub API requires `{PATH}(states: ...)`"))?;
    let state_count = states.len();
    let states: HashSet<_> = states
        .iter()
        .filter_map(|value| match &**value {
            ast::Value::Enum(value) => Some(value.as_str()),
            _ => None,
        })
        .collect();
    let is_open = state_count == 1 && states == HashSet::from(["OPEN"]);
    let is_historical = state_count == 2 && states == HashSet::from(["CLOSED", "MERGED"]);
    if !is_open && !is_historical {
        return Err(format!(
            "The mock GitHub API only supports `{PATH}(states: [OPEN])` or `{PATH}(states: [CLOSED, MERGED])`"
        ));
    }

    for field in selected_fields(&field.selection_set, PATH)? {
        match field.name.as_str() {
            "nodes" => validate_scalar_fields(
                &field.selection_set,
                "repository.pullRequests.nodes",
                &["number", "id", "title", "body", "baseRefName", "state", "isCrossRepository"],
            )?,
            "pageInfo" => validate_scalar_fields(
                &field.selection_set,
                "repository.pullRequests.pageInfo",
                &["hasNextPage"],
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

fn validate_repository_field(
    field: &executable::Field,
    variables: &GraphQlVariables,
) -> Result<(), String> {
    const PATH: &str = "repository";
    validate_argument_names(field, PATH, &["owner", "name"])?;
    resolve_string_argument(field, "owner", PATH, variables)?;
    resolve_string_argument(field, "name", PATH, variables)?;

    for field in selected_fields(&field.selection_set, PATH)? {
        match field.name.as_str() {
            "id" => {}
            "pullRequests" => {
                validate_pull_requests_field(field)?;
                resolve_string_argument(
                    field,
                    "headRefName",
                    "repository.pullRequests",
                    variables,
                )?;
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

fn validate_create_field(field: &executable::Field) -> Result<(), String> {
    const PATH: &str = "createPullRequest";
    validate_argument_names(field, PATH, &["input"])?;
    let input = input_object(field, PATH)?;
    validate_input_fields(
        input,
        PATH,
        &["repositoryId", "baseRefName", "headRefName", "title", "body"],
    )?;
    for required in ["repositoryId", "baseRefName", "headRefName", "title"] {
        required_string_field(input, required, PATH)?;
    }

    for field in selected_fields(&field.selection_set, PATH)? {
        match field.name.as_str() {
            "clientMutationId" => {}
            "pullRequest" => validate_scalar_fields(
                &field.selection_set,
                "createPullRequest.pullRequest",
                &["number", "url", "id"],
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

fn validate_update_field(field: &executable::Field) -> Result<(), String> {
    const PATH: &str = "updatePullRequest";
    validate_argument_names(field, PATH, &["input"])?;
    let input = input_object(field, PATH)?;
    validate_input_fields(input, PATH, &["pullRequestId", "baseRefName", "title", "body"])?;
    required_string_field(input, "pullRequestId", PATH)?;
    if !["baseRefName", "title", "body"].iter().any(|name| get_string_field(input, name).is_some())
    {
        return Err("The mock GitHub API requires at least one pull request update".to_string());
    }
    validate_scalar_fields(&field.selection_set, PATH, &["clientMutationId"])
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
    for field in selected_fields(&operation.selection_set, "operation")? {
        match field.name.as_str() {
            "repository" => validate_repository_field(field, variables)?,
            "createPullRequest" => validate_create_field(field)?,
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

fn extract_input_field<'a>(
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

pub(super) fn required_string_field(
    input: &[(Name, Node<ast::Value>)],
    key: &str,
    path: &str,
) -> Result<String, String> {
    get_string_field(input, key)
        .ok_or_else(|| format!("The mock GitHub API requires string field `{path}.input.{key}`"))
}

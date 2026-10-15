//! Http contracts of the independent HTTP fake.

use super::*;

pub(super) fn graphql_protocol_error(state: &AppState, message: &str) -> Response {
    state.state.write().unwrap().protocol_violations.push(message.to_owned());
    graphql_http_error(message)
}

pub(super) async fn graphql_wrong_method(
    State(app_state): State<AppState>,
) -> impl axum::response::IntoResponse {
    app_state.state.write().unwrap().graphql_arrivals += 1;
    graphql_protocol_error(&app_state, "GraphQL requires POST")
}

pub(super) async fn graphql(
    State(app_state): State<AppState>,
    headers: axum::http::HeaderMap,
    body: Result<Bytes, axum::extract::rejection::BytesRejection>,
) -> Response {
    app_state.state.write().unwrap().graphql_arrivals += 1;
    let body = match body {
        Ok(body) => body,
        Err(_) => return graphql_protocol_error(&app_state, "Invalid GraphQL request body"),
    };
    // Reuse Axum's JSON/media-type contract after counting the raw arrival.
    let mut request = axum::http::Request::new(axum::body::Body::from(body));
    *request.headers_mut() = headers;
    let payload: serde_json::Value = match <Json<serde_json::Value> as axum::extract::FromRequest<
        (),
    >>::from_request(request, &())
    .await
    {
        Ok(Json(payload)) => payload,
        Err(_) => {
            return graphql_protocol_error(&app_state, "Invalid GraphQL JSON or content type")
        }
    };
    let Some(query) = payload.get("query").and_then(|value| value.as_str()) else {
        return graphql_protocol_error(
            &app_state,
            "Invalid GraphQL payload: missing string field `query`",
        );
    };
    let variables = match graphql_variables(&payload) {
        Ok(variables) => variables,
        Err(message) => return graphql_protocol_error(&app_state, &message),
    };

    let document =
        match ExecutableDocument::parse_and_validate(&GITHUB_SCHEMA, query, "query.graphql") {
            Ok(doc) => doc,
            Err(e) => {
                eprintln!("DEBUG: GraphQL validation errors: {:?}", e.errors);
                return graphql_protocol_error(
                    &app_state,
                    "GraphQL request failed schema validation",
                );
            }
        };
    if let Err(message) = validate_supported_document(&document, &variables) {
        return graphql_protocol_error(&app_state, &message);
    }

    let operations = graphql_operations(&document);
    let local_heads = local_pull_request_heads(&document, &variables)
        .expect("validated pull request queries have string head arguments");
    let overlap_observation =
        app_state.schedules.observation_overlap.as_deref().filter(|overlap| {
            operations.as_slice() == [GraphQlOperation::Query]
                && local_heads.as_slice() == [overlap.expected_id()]
        });
    if let Some(overlap) = overlap_observation {
        // The exact request has been parsed and validated, but has not entered
        // mutable fake state. Its Git peer can therefore rendezvous here
        // without either transport serializing the other through a lock.
        if !overlap.local_github_arrived().await {
            return graphql_http_error("Observation overlap schedule was cancelled");
        }
    }
    let overlap_create = operations.as_slice() == [GraphQlOperation::CreatePr]
        && app_state
            .schedules
            .publication_overlap
            .as_ref()
            .is_some_and(|overlap| overlap.claim_create());
    if overlap_create {
        let overlap = app_state.schedules.publication_overlap.as_ref().unwrap();
        // This rendezvous is intentionally after protocol validation but
        // before request logging or state mutation. Both publishers therefore
        // prove they observed the same empty OPEN set.
        if !overlap.before_create_application().await {
            return graphql_http_error("Publication overlap schedule was cancelled");
        }
    }
    let response = {
        let mut mock_state = app_state.state.write().unwrap();
        mock_state.graphql_requests.push(GraphQlRequestRecord {
            operations: operations.clone(),
            document: query.to_owned(),
        });
        if operations.iter().all(|operation| *operation == GraphQlOperation::Query)
            && mock_state
                .max_graphql_query_operations_per_request
                .is_some_and(|limit| local_heads.len() > limit)
        {
            return graphql_response(
                StatusCode::OK,
                serde_json::json!({
                    "errors": [{
                        "type": "RESOURCE_LIMITS_EXCEEDED",
                        "message": "Request exceeds the mock GraphQL operation limit",
                    }]
                }),
            );
        }
        let failure = check_and_apply_graphql_failure(&mut mock_state, &operations);
        let malformed_json = match failure {
            Some(FailureKind::CreatePrMalformedJson(kind))
            | Some(FailureKind::UpdatePrMalformedJson(kind)) => Some(kind),
            _ => None,
        };
        let wrong_create_content = matches!(failure, Some(FailureKind::CreatePrMismatchedContent));
        let concurrent_close = matches!(failure, Some(FailureKind::UpdatePrConcurrentClose));
        if let Some(failure) = failure
            .filter(|_| malformed_json.is_none() && !concurrent_close && !wrong_create_content)
        {
            if matches!(
                failure,
                FailureKind::CreatePrApplyThenDisconnect
                    | FailureKind::ClosePrApplyThenDisconnect
                    | FailureKind::UpdatePrApplyThenDisconnect
            ) {
                let branch_oids = match remote_branch_oids(&app_state) {
                    Ok(branches) => branches,
                    Err(message) => return graphql_http_error(&message),
                };
                let branch_oid = |branch: &str| Ok(branch_oids.get(branch).cloned());
                for operation in document.operations.iter() {
                    for selection in &operation.selection_set.selections {
                        let executable::Selection::Field(field) = selection else { continue };
                        let result = match field.name.as_str() {
                            "createPullRequest" => {
                                handle_create_pr(&mut mock_state, field, &branch_oid)
                            }
                            "convertPullRequestToDraft" => {
                                handle_draft_pr(&mut mock_state, field, &branch_oid)
                            }
                            "closePullRequest" => handle_close_pr(&mut mock_state, field),
                            "updatePullRequest" => {
                                handle_update_pr(&mut mock_state, field, &branch_oid, false)
                            }
                            _ => unreachable!("request was checked by validate_supported_document"),
                        };
                        if let Err(message) = result {
                            return graphql_http_error(&message);
                        }
                    }
                }
                return graphql_disconnect_response();
            }
            return graphql_failure_response(failure);
        }

        let mut response_data = serde_json::Map::new();

        let mut errors = Vec::new();
        let branch_oids = match remote_branch_oids(&app_state) {
            Ok(branches) => branches,
            Err(message) => return graphql_http_error(&message),
        };
        let branch_oid = |branch: &str| Ok(branch_oids.get(branch).cloned());

        for operation in document.operations.iter() {
            for selection in operation.selection_set.selections.iter() {
                if let executable::Selection::Field(field) = selection {
                    let alias = response_key(field);

                    let result = match field.name.as_str() {
                        "createPullRequest" => {
                            handle_create_pr(&mut mock_state, field, &branch_oid)
                        }
                        "convertPullRequestToDraft" => {
                            handle_draft_pr(&mut mock_state, field, &branch_oid)
                        }
                        "closePullRequest" => handle_close_pr(&mut mock_state, field),
                        "updatePullRequest" => {
                            handle_update_pr(&mut mock_state, field, &branch_oid, concurrent_close)
                        }
                        "repository" => handle_repository_query(
                            &mock_state,
                            field,
                            &variables,
                            &|| repository_default_branch(&app_state, &mock_state),
                            &branch_oid,
                        ),
                        _ => unreachable!("request was checked by validate_supported_document"),
                    };
                    match result {
                        Ok(mut value) => {
                            if wrong_create_content && field.name == "createPullRequest" {
                                value["pullRequest"]["title"] =
                                    serde_json::json!("contradictory title");
                                value["pullRequest"]["body"] =
                                    serde_json::json!("contradictory body");
                            }
                            response_data.insert(alias, value);
                        }
                        Err(message) => {
                            response_data.insert(alias.clone(), serde_json::Value::Null);
                            errors.push(serde_json::json!({
                                "message": message,
                                "path": [alias],
                            }));
                        }
                    }
                }
            }
        }

        let mut response_json = serde_json::Map::new();
        response_json.insert("data".to_string(), serde_json::Value::Object(response_data));
        if !errors.is_empty() {
            response_json.insert("errors".to_string(), serde_json::Value::Array(errors));
        }

        let response_json = serde_json::Value::Object(response_json);
        malformed_json
            .map(|kind| malformed_json_response(response_json.clone(), kind))
            .unwrap_or_else(|| graphql_response(StatusCode::OK, response_json))
    };
    if overlap_create {
        let overlap = app_state.schedules.publication_overlap.as_ref().unwrap();
        // The created draft is durable and the state guard is gone before
        // either response can release a publisher toward marker publication.
        if !overlap.after_create_application().await {
            return graphql_http_error("Publication overlap schedule was cancelled");
        }
    }
    response
}

pub(super) async fn graphql_redirect_trap(State(app_state): State<AppState>) -> Response {
    app_state.state.write().unwrap().graphql_redirect_trap_requests += 1;
    graphql_response(
        StatusCode::INTERNAL_SERVER_ERROR,
        serde_json::json!({ "message": "Mutation redirect trap was reached" }),
    )
}

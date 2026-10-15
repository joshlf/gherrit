//! Failures contracts of the independent HTTP fake.

use super::*;

pub(super) fn retryable_status(status: RetryableHttpStatus) -> StatusCode {
    match status {
        RetryableHttpStatus::TooManyRequests => StatusCode::TOO_MANY_REQUESTS,
        RetryableHttpStatus::ServiceUnavailable => StatusCode::SERVICE_UNAVAILABLE,
    }
}

pub(super) fn redirect_status(status: RedirectStatus) -> StatusCode {
    match status {
        RedirectStatus::Temporary => StatusCode::TEMPORARY_REDIRECT,
        RedirectStatus::Permanent => StatusCode::PERMANENT_REDIRECT,
    }
}

pub(super) fn graphql_failure_response(failure: FailureKind) -> Response {
    use FailureKind::*;

    if matches!(failure, QueryTransport) {
        return graphql_disconnect_response();
    }

    let status = match failure {
        QueryHttp(status) | CreatePrHttp(status) => retryable_status(status),
        CreatePrRedirect(status) => redirect_status(status),
        GraphQl | CreatePr | ClosePr | UpdatePr => StatusCode::OK,
        QueryTransport => unreachable!("handled above"),
        CreatePrApplyThenDisconnect
        | ClosePrApplyThenDisconnect
        | UpdatePrApplyThenDisconnect
        | UpdatePrConcurrentClose => {
            unreachable!("handled before response generation")
        }
        CreatePrMismatchedContent | CreatePrMalformedJson(_) | UpdatePrMalformedJson(_) => {
            unreachable!("handled after applying the mutation")
        }
        LosePublicationPushReceipt(_) | Git(_) => {
            unreachable!("Git publication failures are not handled by the GraphQL endpoint")
        }
    };
    let mut headers = HeaderMap::new();
    // Count-limited retry fixtures explicitly permit an immediate retry.
    // Header-free throttles are exercised by the HTTP adapter contracts.
    if status == StatusCode::TOO_MANY_REQUESTS {
        headers.insert("retry-after", HeaderValue::from_static("0"));
    }
    if matches!(failure, CreatePrRedirect(_)) {
        headers.insert(LOCATION, HeaderValue::from_static("/graphql-redirect-trap"));
    }
    let message = format!("Injected {failure:?} failure");
    (
        status,
        headers,
        Json(serde_json::json!({
            "data": null,
            "errors": [{ "message": message }],
        })),
    )
        .into_response()
}

pub(super) fn graphql_disconnect_response() -> Response {
    let body = Body::from_stream(futures_util::stream::once(async {
        Err::<Bytes, _>(std::io::Error::other("Injected GraphQL response transport failure"))
    }));
    Response::new(body)
}

pub(super) fn graphql_response(status: StatusCode, value: serde_json::Value) -> Response {
    (status, Json(value)).into_response()
}

pub(super) fn malformed_json_response(value: serde_json::Value, kind: MalformedJson) -> Response {
    let body = match kind {
        MalformedJson::DuplicateObjectMember => {
            let data = serde_json::to_string(&value["data"]).unwrap();
            format!(r#"{{"data":{data},"data":{data}}}"#)
        }
        MalformedJson::TrailingValue => format!("{} null", serde_json::to_string(&value).unwrap()),
    };
    Response::builder()
        .status(StatusCode::OK)
        .header(CONTENT_TYPE, "application/json")
        .body(Body::from(body))
        .unwrap()
}

pub(super) fn graphql_http_error(message: &str) -> Response {
    graphql_response(
        StatusCode::BAD_REQUEST,
        serde_json::json!({
            "message": message,
            "errors": [{ "message": message }],
        }),
    )
}

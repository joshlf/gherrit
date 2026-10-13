//! Bounded HTTP transport for the exact-local GitHub adapter.

use std::{fmt, time::Duration};

use color_eyre::{
    Report,
    eyre::{Context as _, Result, bail, eyre},
};
use http_body_util::{BodyExt, LengthLimitError, Limited};
use octocrab::Octocrab;
use serde_json::Value;

use super::{
    ObservedGithub,
    mutation::MutationRequest,
    observation::{CompleteLocalPullRequests, LocalPullRequestAccumulator, ObservationStep},
};
use crate::{
    pre_push::{
        GithubEndpoint,
        destination::{PublicationTarget, PushDestination},
        json::UniqueJson,
        local::LocalStack,
    },
    util,
};

const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const READ_TIMEOUT: Duration = Duration::from_secs(30);
const WRITE_TIMEOUT: Duration = Duration::from_secs(30);
const TOTAL_ATTEMPT_TIMEOUT: Duration = Duration::from_secs(60);
const TOTAL_OBSERVATION_TIMEOUT: Duration = Duration::from_secs(10 * 60);

const QUERY_RETRY_DELAYS: [Duration; 3] =
    [Duration::from_millis(100), Duration::from_millis(200), Duration::from_millis(400)];

const MAX_GRAPHQL_QUERY_BYTES: usize = 256 * 1024;
const MAX_HTTP_ERROR_RESPONSE_BYTES: usize = 64 * 1024;
const MAX_GRAPHQL_QUERY_RESPONSE_BYTES: usize = 64 * 1024 * 1024;
const MAX_GRAPHQL_MUTATION_RESPONSE_BYTES: usize = 4 * 1024 * 1024;
const INDETERMINATE_GRAPHQL_MUTATION: &str = "GraphQL mutation acknowledgement is indeterminate; stop this publication attempt and retry the push to reobserve GitHub state";

#[derive(Debug, Eq, PartialEq)]
enum GraphqlBodyError {
    ExceededLimit,
    Read(String),
}

impl fmt::Display for GraphqlBodyError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ExceededLimit => write!(formatter, "GraphQL response exceeded its body limit"),
            Self::Read(error) => write!(formatter, "Failed to read GraphQL response body: {error}"),
        }
    }
}

impl std::error::Error for GraphqlBodyError {}

#[derive(Debug)]
struct QueryResponseSizeLimit;

impl fmt::Display for QueryResponseSizeLimit {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "GraphQL query response exceeded the local planning limit")
    }
}

impl std::error::Error for QueryResponseSizeLimit {}

async fn collect_body_with_limit<B>(
    body: B,
    limit: usize,
) -> std::result::Result<http_body_util::Collected<B::Data>, GraphqlBodyError>
where
    B: BodyExt,
    B::Error: Into<Box<dyn std::error::Error + Send + Sync>>,
{
    Limited::new(body, limit).collect().await.map_err(|error| {
        if error.is::<LengthLimitError>() {
            GraphqlBodyError::ExceededLimit
        } else {
            GraphqlBodyError::Read(diagnostic_detail(&error.to_string()))
        }
    })
}

fn diagnostic_detail(value: &str) -> String {
    const MAX_BYTES: usize = 160;

    let mut rendered = String::new();
    for character in value.chars() {
        let escaped = character.escape_default().to_string();
        if rendered.len() + escaped.len() > MAX_BYTES {
            rendered.push('…');
            break;
        }
        rendered.push_str(&escaped);
    }
    rendered
}

fn response_error_detail(response: &Value) -> Option<String> {
    response
        .get("errors")
        .and_then(Value::as_array)
        .and_then(|errors| errors.first())
        .and_then(|error| error.get("message"))
        .and_then(Value::as_str)
        .map(diagnostic_detail)
        .filter(|detail| !detail.is_empty())
}

fn http_status_error(status: impl fmt::Display, context: &str, body: Option<&[u8]>) -> Report {
    let detail =
        body.and_then(|body| serde_json::from_slice::<Value>(body).ok()).and_then(|response| {
            response
                .get("message")
                .and_then(Value::as_str)
                .map(diagnostic_detail)
                .filter(|detail| !detail.is_empty())
                .or_else(|| response_error_detail(&response))
        });
    match detail {
        Some(detail) => eyre!("GitHub returned HTTP status {status} for {context}: {detail}"),
        None => eyre!("GitHub returned HTTP status {status} for {context}"),
    }
}

#[derive(Clone, Copy, Debug)]
struct Timeouts {
    connect: Duration,
    read: Duration,
    write: Duration,
    attempt: Duration,
    observation: Duration,
}

impl Timeouts {
    const PRODUCTION: Self = Self {
        connect: CONNECT_TIMEOUT,
        read: READ_TIMEOUT,
        write: WRITE_TIMEOUT,
        attempt: TOTAL_ATTEMPT_TIMEOUT,
        observation: TOTAL_OBSERVATION_TIMEOUT,
    };
}

/// A GitHub client bound to the selected push repository for one attempt.
pub(in crate::pre_push::publication_attempt) struct Github {
    http: Octocrab,
    target: PublicationTarget,
    timeouts: Timeouts,
}

impl Github {
    /// Validates the endpoint pairing before asking `gh` for credentials.
    ///
    /// An unsupported Git destination therefore reports the destination
    /// error even when no GitHub credentials are configured.
    pub(in crate::pre_push::publication_attempt) fn new(
        endpoint: &GithubEndpoint,
        destination: &PushDestination,
    ) -> Result<Self> {
        if endpoint.is_disabled() {
            bail!("Cannot construct a GitHub client while GitHub is disabled");
        }
        endpoint.validate_destination(destination)?;
        Self::with_timeouts(
            util::get_github_token()?,
            endpoint.custom_url(),
            destination.publication_target(),
            Timeouts::PRODUCTION,
        )
    }

    pub(in crate::pre_push::publication_attempt) fn publication_target(
        &self,
    ) -> &PublicationTarget {
        &self.target
    }

    #[cfg(test)]
    pub(in crate::pre_push::publication_attempt) fn for_plan_test(
        destination: &PushDestination,
    ) -> Self {
        Self::with_timeouts(
            "token".to_owned(),
            Some("http://127.0.0.1:1"),
            destination.publication_target(),
            Timeouts::PRODUCTION,
        )
        .expect("the fixed test endpoint is valid")
    }

    fn with_timeouts(
        token: String,
        api_url: Option<&str>,
        target: PublicationTarget,
        timeouts: Timeouts,
    ) -> Result<Self> {
        let mut builder = Octocrab::builder()
            .personal_token(token)
            .set_connect_timeout(Some(timeouts.connect))
            .set_read_timeout(Some(timeouts.read))
            .set_write_timeout(Some(timeouts.write));
        if let Some(api_url) = api_url {
            builder = builder.base_uri(api_url)?;
        }
        Ok(Self { http: builder.build()?, target, timeouts })
    }

    /// Exhausts the OPEN pull request connection for each sealed local change.
    pub(in crate::pre_push::publication_attempt) async fn observe_local_pull_requests(
        self,
        local: &LocalStack,
    ) -> Result<ObservedGithub> {
        let pull_requests = tokio::time::timeout(
            self.timeouts.observation,
            self.observe_local_pull_requests_inner(local),
        )
        .await
        .map_err(|_| eyre!("GitHub exact-local observation exceeded its total deadline"))??;
        Ok(ObservedGithub::new(self, pull_requests))
    }

    async fn observe_local_pull_requests_inner(
        &self,
        local: &LocalStack,
    ) -> Result<CompleteLocalPullRequests> {
        let accumulator =
            LocalPullRequestAccumulator::for_stack(self.target.coordinates().clone(), local)?;
        let mut step = accumulator.next()?;
        loop {
            let mut pending = match step {
                ObservationStep::Complete(complete) => return Ok(complete),
                ObservationStep::Request(pending) => pending,
            };
            loop {
                let document = pending.document();
                if document.len() > MAX_GRAPHQL_QUERY_BYTES {
                    pending = pending.back_off().map_err(|_| {
                        eyre!(
                            "GraphQL query for one local change serializes beyond the {MAX_GRAPHQL_QUERY_BYTES}-byte document limit"
                        )
                    })?;
                    continue;
                }

                match self.run_observation_query(&document).await? {
                    Some(response) => {
                        step = pending.accept(response)?;
                        break;
                    }
                    None => {
                        let attempted = pending.alias_count();
                        pending = pending.back_off().map_err(|_| {
                            eyre!("GitHub query for one local change exceeds resource limits")
                        })?;
                        log::warn!(
                            "Backing off exact-local GraphQL aliases from {} to {}.",
                            attempted,
                            pending.alias_count()
                        );
                    }
                }
            }
        }
    }

    /// Sends one mutation request exactly once and consumes its wire value.
    pub(super) async fn send_mutation_once(&self, request: MutationRequest) -> Result<UniqueJson> {
        let request = request.into_value();
        tokio::time::timeout(self.timeouts.attempt, self.mutation_attempt_inner(request))
            .await
            .map_err(|_| eyre!("GraphQL mutation attempt exceeded its wall-clock deadline"))
            .and_then(|result| result)
            .map_err(indeterminate_mutation)
    }

    async fn mutation_attempt_inner(&self, request: Value) -> Result<UniqueJson> {
        let response = self
            .http
            ._post("/graphql", Some(&request))
            .await
            .wrap_err("GraphQL mutation request failed")?;
        let status = response.status();
        if !status.is_success() {
            let (_, body) = response.into_parts();
            let body = collect_body_with_limit(body, MAX_HTTP_ERROR_RESPONSE_BYTES)
                .await
                .ok()
                .map(http_body_util::Collected::to_bytes);
            return Err(http_status_error(status, "a GraphQL mutation", body.as_deref()));
        }
        let (_, body) = response.into_parts();
        let body = collect_body_with_limit(body, MAX_GRAPHQL_MUTATION_RESPONSE_BYTES)
            .await
            .wrap_err("Failed to collect bounded GraphQL mutation response")?
            .to_bytes();
        UniqueJson::decode(&body)
            .map_err(|_| eyre!("GitHub mutation response contains malformed JSON"))
    }

    async fn run_query(&self, request: &Value) -> Result<UniqueJson> {
        let mut completed_retries = 0;
        loop {
            match self.query_attempt(request).await {
                Ok(response) => return Ok(response),
                Err(QueryAttemptError::Fatal(error)) => return Err(error),
                Err(QueryAttemptError::Transient(error)) => {
                    let Some(delay) = QUERY_RETRY_DELAYS.get(completed_retries).copied() else {
                        return Err(error);
                    };
                    completed_retries += 1;
                    log::warn!(
                        "Retrying read-only GraphQL request after {error} ({completed_retries}/{}) in {} ms",
                        QUERY_RETRY_DELAYS.len(),
                        delay.as_millis()
                    );
                    tokio::time::sleep(delay).await;
                }
            }
        }
    }

    async fn query_attempt(&self, request: &Value) -> Result<UniqueJson, QueryAttemptError> {
        match tokio::time::timeout(self.timeouts.attempt, self.query_attempt_inner(request)).await {
            Ok(result) => result,
            Err(_) => Err(QueryAttemptError::Transient(eyre!(
                "GraphQL read-only attempt exceeded its wall-clock deadline"
            ))),
        }
    }

    async fn query_attempt_inner(&self, request: &Value) -> Result<UniqueJson, QueryAttemptError> {
        let response =
            self.http._post("/graphql", Some(request)).await.map_err(classify_octocrab_error)?;
        let status = response.status();
        if !status.is_success() {
            let retryable = status.is_server_error() || status.as_u16() == 429;
            let (_, body) = response.into_parts();
            let body = collect_body_with_limit(body, MAX_HTTP_ERROR_RESPONSE_BYTES)
                .await
                .ok()
                .map(http_body_util::Collected::to_bytes);
            let error = http_status_error(status, "a read-only GraphQL request", body.as_deref());
            return Err(if retryable {
                QueryAttemptError::Transient(error)
            } else {
                QueryAttemptError::Fatal(error)
            });
        }

        let (_, body) = response.into_parts();
        let body = collect_body_with_limit(body, MAX_GRAPHQL_QUERY_RESPONSE_BYTES)
            .await
            .map_err(|error| match error {
                GraphqlBodyError::ExceededLimit => {
                    QueryAttemptError::Fatal(Report::new(QueryResponseSizeLimit))
                }
                GraphqlBodyError::Read(_) => QueryAttemptError::Transient(Report::from(error)),
            })?
            .to_bytes();
        UniqueJson::decode(&body).map_err(|_| {
            QueryAttemptError::Fatal(eyre!("GitHub query response contains malformed JSON"))
        })
    }

    async fn run_observation_query(&self, document: &str) -> Result<Option<UniqueJson>> {
        debug_assert!(document.len() <= MAX_GRAPHQL_QUERY_BYTES);
        let request = json_request(document);
        let response = match self.run_query(&request).await {
            Ok(response) => response,
            Err(error) if error.downcast_ref::<QueryResponseSizeLimit>().is_some() => {
                return Ok(None);
            }
            Err(error) => return Err(error).wrap_err("GraphQL read-only observation failed"),
        };
        match classify_response(response.as_value()) {
            ResponseDisposition::Success => Ok(Some(response)),
            ResponseDisposition::ResourceLimit => Ok(None),
            ResponseDisposition::Fatal => {
                if let Some(detail) = response_error_detail(response.as_value()) {
                    bail!("GitHub returned fatal GraphQL errors: {detail}");
                }
                bail!("GitHub returned a fatal GraphQL response")
            }
        }
    }
}

fn json_request(document: &str) -> Value {
    serde_json::json!({ "query": document })
}

pub(super) fn indeterminate_mutation(error: Report) -> Report {
    error.wrap_err(INDETERMINATE_GRAPHQL_MUTATION)
}

#[derive(Debug)]
enum QueryAttemptError {
    Transient(Report),
    Fatal(Report),
}

fn classify_octocrab_error(error: octocrab::Error) -> QueryAttemptError {
    if matches!(error, octocrab::Error::Service { .. } | octocrab::Error::Hyper { .. }) {
        QueryAttemptError::Transient(Report::from(error))
    } else {
        QueryAttemptError::Fatal(Report::from(error))
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ResponseDisposition {
    Success,
    ResourceLimit,
    Fatal,
}

fn classify_response(response: &Value) -> ResponseDisposition {
    let Some(response) = response.as_object() else {
        return ResponseDisposition::Fatal;
    };
    if response.keys().any(|field| !matches!(field.as_str(), "data" | "errors" | "extensions"))
        || response.get("extensions").is_some_and(|extensions| !extensions.is_object())
    {
        return ResponseDisposition::Fatal;
    }
    match response.get("errors") {
        None => ResponseDisposition::Success,
        Some(Value::Array(errors)) if errors.is_empty() => ResponseDisposition::Success,
        Some(Value::Array(errors))
            if response.get("data").is_none_or(Value::is_null)
                && !errors.is_empty()
                && errors.iter().all(is_resource_limit_error) =>
        {
            ResponseDisposition::ResourceLimit
        }
        Some(_) => ResponseDisposition::Fatal,
    }
}

fn is_resource_limit_error(error: &Value) -> bool {
    matches!(
        error.get("type").and_then(Value::as_str),
        Some("RESOURCE_LIMITS_EXCEEDED" | "MAX_NODE_LIMIT_EXCEEDED")
    ) || matches!(
        error.get("message").and_then(Value::as_str),
        Some("A query attribute must be specified and must be a string.")
    )
}

#[cfg(test)]
#[path = "transport/tests.rs"]
mod tests;

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
    mutation::MutationRequest,
    observation::{CompleteLocalPullRequests, LocalPullRequestAccumulator, ObservationStep},
};
use crate::pre_push::{
    GithubEndpoint,
    destination::{PublicationTarget, PushDestination},
    json::UniqueJson,
    local::LocalStack,
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
pub(in crate::pre_push) struct Github {
    http: Octocrab,
    target: PublicationTarget,
    timeouts: Timeouts,
}

impl Github {
    pub(in crate::pre_push) fn new(
        token: String,
        endpoint: &GithubEndpoint,
        destination: &PushDestination,
    ) -> Result<Self> {
        if endpoint.is_disabled() {
            bail!("Cannot construct a GitHub client while GitHub is disabled");
        }
        if endpoint.custom_url().is_none() && !destination.supports_production_github() {
            bail!("The selected Git destination is not hosted by the production GitHub endpoint");
        }
        Self::with_timeouts(
            token,
            endpoint.custom_url(),
            destination.publication_target(),
            Timeouts::PRODUCTION,
        )
    }

    pub(in crate::pre_push) fn publication_target(&self) -> &PublicationTarget {
        &self.target
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

    /// Observes every lifecycle state for each change in the sealed stack.
    pub(in crate::pre_push) async fn observe_local_pull_requests(
        &self,
        local: &LocalStack,
    ) -> Result<CompleteLocalPullRequests> {
        tokio::time::timeout(
            self.timeouts.observation,
            self.observe_local_pull_requests_inner(local),
        )
        .await
        .map_err(|_| eyre!("GitHub exact-local observation exceeded its total deadline"))?
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
mod tests {
    use std::{ops::RangeInclusive, time::Instant};

    use gix::ObjectId;
    use http_body_util::Full;
    use serde_json::json;
    use tokio::{
        io::{AsyncReadExt as _, AsyncWriteExt as _},
        net::{TcpListener, TcpStream},
        task::JoinHandle,
    };

    use super::*;
    use crate::pre_push::{
        batching::MAX_MUTATION_REQUEST_BYTES,
        destination::DefaultBranch,
        github::{
            PullRequestIdentity,
            mutation::{PreparedCreates, PreparedUpdates, TestCreate, TestUpdate},
            observation::LocalPullRequestObservation,
            pull_request::PullRequestIdentityRegistry,
        },
        local::GherritPrId,
    };

    const SERVER_STEP_TIMEOUT: Duration = Duration::from_secs(2);
    const EXTRA_REQUEST_WINDOW: Duration = Duration::from_millis(550);

    #[derive(Debug)]
    struct RecordedRequest {
        at: Instant,
        body: Vec<u8>,
    }

    enum Reply {
        Json(Value),
        Raw(Vec<u8>),
        Oversized(Vec<u8>),
        Status(u16),
        Redirect { status: u16, location: String },
        Truncated { declared_bytes: usize, body: Vec<u8> },
        Disconnect,
        Hang(Duration),
    }

    struct Exchange {
        expected: Value,
        reply: Reply,
    }

    fn exchange(expected: Value, reply: Reply) -> Exchange {
        Exchange { expected, reply }
    }

    async fn read_request(stream: &mut TcpStream) -> Vec<u8> {
        const HEADER_LIMIT: usize = 64 * 1024;
        const BODY_LIMIT: usize = 2 * 1024 * 1024;

        let mut request = Vec::new();
        let mut buffer = [0_u8; 4096];
        let header_end = loop {
            let count = stream.read(&mut buffer).await.unwrap();
            assert_ne!(count, 0, "client closed before sending a complete request");
            request.extend_from_slice(&buffer[..count]);
            assert!(request.len() <= HEADER_LIMIT, "request head exceeded test limit");
            if let Some(start) = request.windows(4).position(|window| window == b"\r\n\r\n") {
                break start + 4;
            }
        };
        let head = std::str::from_utf8(&request[..header_end]).unwrap();
        assert!(head.starts_with("POST /graphql HTTP/1.1\r\n"), "unexpected request: {head}");
        let content_length = head
            .split("\r\n")
            .find_map(|line| {
                let (name, value) = line.split_once(':')?;
                name.eq_ignore_ascii_case("content-length")
                    .then(|| value.trim().parse::<usize>().unwrap())
            })
            .expect("request omitted Content-Length");
        assert!(content_length <= BODY_LIMIT, "request body exceeded test limit");
        while request.len() - header_end < content_length {
            let count = stream.read(&mut buffer).await.unwrap();
            assert_ne!(count, 0, "client closed before sending a complete body");
            request.extend_from_slice(&buffer[..count]);
        }
        assert_eq!(request.len() - header_end, content_length);
        request.split_off(header_end)
    }

    async fn write_response(stream: &mut TcpStream, status: u16, body: &[u8]) {
        write_response_with_headers(stream, status, "", body.len(), body).await;
    }

    async fn write_response_with_headers(
        stream: &mut TcpStream,
        status: u16,
        headers: &str,
        declared_bytes: usize,
        body: &[u8],
    ) {
        let response = format!(
            "HTTP/1.1 {status} scripted\r\nContent-Type: application/json\r\n{headers}Content-Length: {declared_bytes}\r\nConnection: close\r\n\r\n"
        );
        stream.write_all(response.as_bytes()).await.unwrap();
        stream.write_all(body).await.unwrap();
    }

    async fn scripted_peer(exchanges: Vec<Exchange>) -> (String, JoinHandle<Vec<RecordedRequest>>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let mut requests = Vec::with_capacity(exchanges.len());
            for Exchange { expected, reply } in exchanges {
                let (mut stream, _) = tokio::time::timeout(SERVER_STEP_TIMEOUT, listener.accept())
                    .await
                    .expect("client did not send the scripted request in time")
                    .unwrap();
                let body = read_request(&mut stream).await;
                let actual: Value = serde_json::from_slice(&body).unwrap();
                assert_eq!(actual, expected, "client sent a different GraphQL request");
                requests.push(RecordedRequest { at: Instant::now(), body });
                match reply {
                    Reply::Json(value) => {
                        write_response(&mut stream, 200, &serde_json::to_vec(&value).unwrap())
                            .await;
                    }
                    Reply::Raw(body) => write_response(&mut stream, 200, &body).await,
                    Reply::Oversized(body) => {
                        let response = format!(
                            "HTTP/1.1 200 scripted\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                            body.len()
                        );
                        stream.write_all(response.as_bytes()).await.unwrap();
                        // The bounded client deliberately closes the body before
                        // the peer necessarily finishes this oversized write.
                        let _ = stream.write_all(&body).await;
                    }
                    Reply::Status(status) => {
                        write_response(&mut stream, status, br#"{"message":"scripted status"}"#)
                            .await;
                    }
                    Reply::Redirect { status, location } => {
                        let headers = format!("Location: {location}\r\n");
                        write_response_with_headers(
                            &mut stream,
                            status,
                            &headers,
                            br#"{"message":"scripted redirect"}"#.len(),
                            br#"{"message":"scripted redirect"}"#,
                        )
                        .await;
                    }
                    Reply::Truncated { declared_bytes, body } => {
                        assert!(declared_bytes > body.len());
                        write_response_with_headers(&mut stream, 200, "", declared_bytes, &body)
                            .await;
                    }
                    Reply::Disconnect => {}
                    Reply::Hang(duration) => tokio::time::sleep(duration).await,
                }
            }

            // A mutation replay is itself the bug under test. Retain any
            // unexpected later request rather than letting connection refusal
            // hide it.
            while let Ok(Ok((mut stream, _))) =
                tokio::time::timeout(EXTRA_REQUEST_WINDOW, listener.accept()).await
            {
                let body = read_request(&mut stream).await;
                requests.push(RecordedRequest { at: Instant::now(), body });
                write_response(&mut stream, 599, br#"{"message":"unexpected request"}"#).await;
            }
            requests
        });
        (format!("http://{address}"), server)
    }

    async fn finish_peer(mut server: JoinHandle<Vec<RecordedRequest>>) -> Vec<RecordedRequest> {
        match tokio::time::timeout(Duration::from_secs(4), &mut server).await {
            Ok(result) => result.unwrap(),
            Err(_) => {
                server.abort();
                panic!("scripted GraphQL peer did not shut down within its bound");
            }
        }
    }

    fn oid(value: u64) -> ObjectId {
        ObjectId::from_hex(format!("{value:040x}").as_bytes()).unwrap()
    }

    fn id(value: &str) -> GherritPrId {
        GherritPrId::from_ref_component(value.as_bytes()).unwrap()
    }

    fn local(ids: &[&str]) -> LocalStack {
        let default = DefaultBranch::new("main".to_owned(), oid(1)).unwrap();
        let mut parent = default.tip();
        let changes = ids.iter().enumerate().map(|(index, value)| {
            let head = oid(index as u64 + 2);
            let change = (id(value), head, parent);
            parent = head;
            change
        });
        LocalStack::for_history_test(default, changes)
    }

    fn query_request(queries: &[(&str, Option<&str>)], include_repository_facts: bool) -> Value {
        let repository_facts = if include_repository_facts {
            "id, defaultBranchRef { name, target { oid } }, "
        } else {
            ""
        };
        let connections = queries
            .iter()
            .enumerate()
            .map(|(index, (id, after))| {
                let after = after
                    .map(|cursor| format!(", after: {}", json!(cursor)))
                    .unwrap_or_default();
                format!(
                    "op{index}: pullRequests(headRefName: {}, first: 1{after}, states: [OPEN, CLOSED, MERGED]) {{ nodes {{ number, id, title, body, baseRefName, baseRefOid, headRefName, headRefOid, state, isCrossRepository, autoMergeRequest {{ enabledAt }}, isInMergeQueue }} pageInfo {{ hasNextPage, endCursor }} }}",
                    json!(id),
                )
            })
            .collect::<Vec<_>>()
            .join(" ");
        json!({
            "query": format!(
                "query {{ repository(owner: \"owner\", name: \"repo\") {{ {repository_facts}{connections} }} }}"
            )
        })
    }

    fn connection(nodes: Vec<Value>, next: Option<&str>) -> Value {
        json!({
            "nodes": nodes,
            "pageInfo": {
                "hasNextPage": next.is_some(),
                "endCursor": next,
            }
        })
    }

    fn fork(number: u64, node_id: &str, head: &str) -> Value {
        json!({
            "number": number,
            "id": node_id,
            "title": null,
            "body": null,
            "baseRefName": null,
            "baseRefOid": null,
            "headRefName": head,
            "headRefOid": null,
            "state": "OPEN",
            "isCrossRepository": true,
            "autoMergeRequest": null,
            "isInMergeQueue": null,
        })
    }

    fn observation_response(include_repository: bool, connections: Vec<Value>) -> Value {
        let mut repository = serde_json::Map::new();
        if include_repository {
            repository.insert("id".to_owned(), Value::String("REPOSITORY_NODE".to_owned()));
            repository.insert(
                "defaultBranchRef".to_owned(),
                json!({ "name": "main", "target": { "oid": oid(1).to_string() } }),
            );
        }
        for (index, connection) in connections.into_iter().enumerate() {
            repository.insert(format!("op{index}"), connection);
        }
        json!({ "data": { "repository": repository } })
    }

    fn test_timeouts() -> Timeouts {
        Timeouts {
            connect: Duration::from_secs(1),
            read: Duration::from_secs(1),
            write: Duration::from_secs(1),
            attempt: Duration::from_secs(2),
            observation: Duration::from_secs(5),
        }
    }

    fn test_github(api_url: &str, timeouts: Timeouts) -> Github {
        let destination = PushDestination::for_test();
        Github::with_timeouts(
            "token".to_owned(),
            Some(api_url),
            destination.publication_target(),
            timeouts,
        )
        .unwrap()
    }

    #[test]
    fn production_client_rejects_destinations_outside_the_builtin_github_transport() {
        let repository = crate::util::Repo::open(".").unwrap();
        for destination in
            ["https://evil.example/owner/repo.git", "HTTPS://github.com/owner/repo.git"]
        {
            let other = PushDestination::for_test_url_in(&repository, destination);
            let error = Github::new("token".to_owned(), &GithubEndpoint::Production, &other)
                .err()
                .expect("a non-built-in GitHub transport must fail before client construction");
            assert_eq!(
                error.to_string(),
                "The selected Git destination is not hosted by the production GitHub endpoint"
            );
        }
    }

    fn test_update(number: u32) -> TestUpdate {
        TestUpdate {
            identity: PullRequestIdentity::new(u64::from(number), format!("PR{number}")).unwrap(),
            title: Some(format!("title {number}")),
            body: None,
            base_branch: None,
        }
    }

    fn test_create(value: &str, head: u64) -> TestCreate {
        TestCreate {
            id: id(value),
            title: format!("title {value}"),
            body: format!("body {value}"),
            head_oid: oid(head),
            base_oid: oid(1),
        }
    }

    fn create_request(specifications: &[(&str, u64)]) -> Value {
        let fields = specifications
            .iter()
            .enumerate()
            .map(|(index, (id, _))| {
                format!(
                    "op{index}: createPullRequest(input: {{ repositoryId: \"REPOSITORY_NODE\", headRepositoryId: \"REPOSITORY_NODE\", baseRefName: \"gherrit-bases/{id}\", headRefName: \"{id}\", title: \"title {id}\", body: \"body {id}\", clientMutationId: \"gherrit:create:{id}\" }}) {{ clientMutationId, pullRequest {{ number, id, state, headRefName, headRefOid, headRepository {{ id }}, baseRefName, baseRefOid, baseRepository {{ id }} }} }}"
                )
            })
            .collect::<Vec<_>>()
            .join(" ");
        json!({ "query": format!("mutation {{ {fields} }}") })
    }

    fn create_response(specifications: &[(&str, u64, u32, &str)]) -> Value {
        let data = specifications
            .iter()
            .enumerate()
            .map(|(index, (id, head, number, node_id))| {
                (
                    format!("op{index}"),
                    json!({
                        "clientMutationId": format!("gherrit:create:{id}"),
                        "pullRequest": {
                            "number": number,
                            "id": node_id,
                            "state": "OPEN",
                            "headRefName": id,
                            "headRefOid": oid(*head).to_string(),
                            "headRepository": { "id": "REPOSITORY_NODE" },
                            "baseRefName": format!("gherrit-bases/{id}"),
                            "baseRefOid": oid(1).to_string(),
                            "baseRepository": { "id": "REPOSITORY_NODE" },
                        },
                    }),
                )
            })
            .collect::<serde_json::Map<_, _>>();
        json!({ "data": data })
    }

    fn update_request(numbers: RangeInclusive<u32>) -> Value {
        let fields = numbers
            .enumerate()
            .map(|(index, number)| {
                format!(
                    "op{index}: updatePullRequest(input: {{ pullRequestId: \"PR{number}\", title: \"title {number}\", clientMutationId: \"gherrit:update:PR{number}\" }}) {{ clientMutationId, pullRequest {{ number, id, state }} }}"
                )
            })
            .collect::<Vec<_>>()
            .join(" ");
        json!({ "query": format!("mutation {{ {fields} }}") })
    }

    fn update_response(numbers: RangeInclusive<u32>) -> Value {
        let data = numbers
            .enumerate()
            .map(|(index, number)| {
                (
                    format!("op{index}"),
                    json!({
                        "clientMutationId": format!("gherrit:update:PR{number}"),
                        "pullRequest": {
                            "number": number,
                            "id": format!("PR{number}"),
                            "state": "OPEN",
                        },
                    }),
                )
            })
            .collect::<serde_json::Map<_, _>>();
        json!({ "data": data })
    }

    #[test]
    fn production_limits_are_explicit_and_finite() {
        assert_eq!(Timeouts::PRODUCTION.connect, Duration::from_secs(10));
        assert_eq!(Timeouts::PRODUCTION.read, Duration::from_secs(30));
        assert_eq!(Timeouts::PRODUCTION.write, Duration::from_secs(30));
        assert_eq!(Timeouts::PRODUCTION.attempt, Duration::from_secs(60));
        assert_eq!(Timeouts::PRODUCTION.observation, Duration::from_secs(10 * 60));
        assert_eq!(QUERY_RETRY_DELAYS.map(|delay| delay.as_millis()), [100, 200, 400]);
        assert_eq!(MAX_GRAPHQL_QUERY_BYTES, 256 * 1024);
        assert_eq!(MAX_HTTP_ERROR_RESPONSE_BYTES, 64 * 1024);
        assert_eq!(MAX_GRAPHQL_QUERY_RESPONSE_BYTES, 64 * 1024 * 1024);
        assert_eq!(MAX_GRAPHQL_MUTATION_RESPONSE_BYTES, 4 * 1024 * 1024);
    }

    #[tokio::test]
    async fn body_limit_is_inclusive() {
        assert_eq!(
            collect_body_with_limit(Full::new(&b"1234"[..]), 4).await.unwrap().to_bytes(),
            &b"1234"[..]
        );
        assert_eq!(
            collect_body_with_limit(Full::new(&b"12345"[..]), 4).await.unwrap_err(),
            GraphqlBodyError::ExceededLimit
        );
    }

    #[test]
    fn response_classification_accepts_only_unmixed_resource_limits() {
        let resource = json!({ "type": "RESOURCE_LIMITS_EXCEEDED" });
        assert_eq!(
            classify_response(&json!({ "errors": [resource.clone()] })),
            ResponseDisposition::ResourceLimit
        );
        for response in [
            json!({ "data": {}, "errors": [resource.clone()] }),
            json!({ "errors": [resource, { "type": "FORBIDDEN" }] }),
            json!({ "errors": "invalid" }),
            json!({ "unexpected": true }),
        ] {
            assert_eq!(classify_response(&response), ResponseDisposition::Fatal);
        }
        for response in [json!({ "data": {} }), json!({ "data": {}, "errors": [] })] {
            assert_eq!(classify_response(&response), ResponseDisposition::Success);
        }
    }

    #[test]
    fn diagnostics_are_single_line_and_bounded() {
        let message = format!("{}\nnot-disclosed", "x".repeat(1_000));
        let body = serde_json::to_vec(&json!({ "message": message })).unwrap();
        let error = http_status_error(400, "a test request", Some(&body)).to_string();
        assert!(!error.contains('\n'));
        assert!(!error.contains("not-disclosed"));
        assert!(error.ends_with('…'));
        assert!(error.len() < 300);
    }

    #[tokio::test]
    async fn facade_paginates_connections_independently() {
        let initial_request = query_request(&[("A", None), ("B", None)], true);
        let next_request = query_request(&[("A", Some("cursor-a"))], false);
        let first = observation_response(
            true,
            vec![
                connection(vec![fork(1, "AONE", "A")], Some("cursor-a")),
                connection(Vec::new(), None),
            ],
        );
        let second =
            observation_response(false, vec![connection(vec![fork(2, "ATWO", "A")], None)]);
        let (api_url, server) = scripted_peer(vec![
            exchange(initial_request, Reply::Json(first)),
            exchange(next_request, Reply::Json(second)),
        ])
        .await;
        let complete = test_github(&api_url, test_timeouts())
            .observe_local_pull_requests(&local(&["A", "B"]))
            .await
            .unwrap();
        let requests = finish_peer(server).await;

        assert_eq!(requests.len(), 2);
        assert!(
            complete
                .local()
                .iter()
                .all(|item| matches!(item, LocalPullRequestObservation::Absent(_)))
        );
        assert_eq!(
            complete.local().iter().map(|item| item.id().as_str()).collect::<Vec<_>>(),
            ["A", "B"]
        );
    }

    #[tokio::test]
    async fn resource_rejection_splits_only_aliases_and_retains_repository_facts() {
        let all = query_request(&[("A", None), ("B", None), ("C", None), ("D", None)], true);
        let first_half = query_request(&[("A", None), ("B", None)], true);
        let second_half = query_request(&[("C", None), ("D", None)], false);
        let resource = json!({ "errors": [{ "type": "RESOURCE_LIMITS_EXCEEDED" }] });
        let (api_url, server) = scripted_peer(vec![
            exchange(all, Reply::Json(resource)),
            exchange(
                first_half,
                Reply::Json(observation_response(
                    true,
                    vec![connection(Vec::new(), None), connection(Vec::new(), None)],
                )),
            ),
            exchange(
                second_half,
                Reply::Json(observation_response(
                    false,
                    vec![connection(Vec::new(), None), connection(Vec::new(), None)],
                )),
            ),
        ])
        .await;
        let complete = test_github(&api_url, test_timeouts())
            .observe_local_pull_requests(&local(&["A", "B", "C", "D"]))
            .await
            .unwrap();
        let requests = finish_peer(server).await;

        assert_eq!(requests.len(), 3);
        assert_eq!(complete.local().len(), 4);
    }

    #[tokio::test]
    async fn transient_query_retries_send_identical_bounded_requests() {
        let request = query_request(&[("A", None)], true);
        let success = observation_response(true, vec![connection(Vec::new(), None)]);
        let (api_url, server) = scripted_peer(vec![
            exchange(request.clone(), Reply::Status(503)),
            exchange(request.clone(), Reply::Status(429)),
            exchange(request, Reply::Json(success)),
        ])
        .await;
        test_github(&api_url, test_timeouts())
            .observe_local_pull_requests(&local(&["A"]))
            .await
            .unwrap();
        let requests = finish_peer(server).await;

        assert_eq!(requests.len(), 3);
        assert!(requests.iter().all(|request| request.body == requests[0].body));
        assert!(requests[1].at.duration_since(requests[0].at) >= Duration::from_millis(90));
        assert!(requests[2].at.duration_since(requests[1].at) >= Duration::from_millis(190));
    }

    #[tokio::test]
    async fn transient_query_retries_exhaust_the_exact_schedule() {
        let request = query_request(&[("A", None)], true);
        let (api_url, server) = scripted_peer(
            (0..=QUERY_RETRY_DELAYS.len())
                .map(|_| exchange(request.clone(), Reply::Status(503)))
                .collect(),
        )
        .await;
        let error = test_github(&api_url, test_timeouts())
            .observe_local_pull_requests(&local(&["A"]))
            .await
            .unwrap_err();
        let requests = finish_peer(server).await;

        assert_eq!(requests.len(), 1 + QUERY_RETRY_DELAYS.len());
        assert!(requests.iter().all(|request| request.body == requests[0].body));
        for (requests, delay) in requests.windows(2).zip(QUERY_RETRY_DELAYS) {
            assert!(
                requests[1].at.duration_since(requests[0].at)
                    >= delay.saturating_sub(Duration::from_millis(10))
            );
        }
        assert!(format!("{error:?}").contains("HTTP status 503"));
    }

    #[tokio::test]
    async fn total_observation_deadline_preempts_a_retry_delay() {
        let request = query_request(&[("A", None)], true);
        let (api_url, server) = scripted_peer(vec![exchange(request, Reply::Status(503))]).await;
        let mut timeouts = test_timeouts();
        timeouts.observation = Duration::from_millis(50);
        let error = test_github(&api_url, timeouts)
            .observe_local_pull_requests(&local(&["A"]))
            .await
            .unwrap_err();
        let requests = finish_peer(server).await;

        assert_eq!(requests.len(), 1);
        assert!(error.to_string().contains("observation exceeded its total deadline"));
    }

    #[tokio::test]
    async fn resource_backoff_rebuilds_the_same_paginated_cursors() {
        let initial = query_request(&[("A", None), ("B", None)], true);
        let both_next = query_request(&[("A", Some("cursor-a")), ("B", Some("cursor-b"))], false);
        let a_next = query_request(&[("A", Some("cursor-a"))], false);
        let b_next = query_request(&[("B", Some("cursor-b"))], false);
        let first = observation_response(
            true,
            vec![
                connection(vec![fork(1, "AONE", "A")], Some("cursor-a")),
                connection(vec![fork(2, "BONE", "B")], Some("cursor-b")),
            ],
        );
        let resource = json!({ "errors": [{ "type": "RESOURCE_LIMITS_EXCEEDED" }] });
        let exhausted = || observation_response(false, vec![connection(Vec::new(), None)]);
        let (api_url, server) = scripted_peer(vec![
            exchange(initial, Reply::Json(first)),
            exchange(both_next, Reply::Json(resource)),
            exchange(a_next, Reply::Json(exhausted())),
            exchange(b_next, Reply::Json(exhausted())),
        ])
        .await;
        let complete = test_github(&api_url, test_timeouts())
            .observe_local_pull_requests(&local(&["A", "B"]))
            .await
            .unwrap();
        let requests = finish_peer(server).await;

        assert_eq!(requests.len(), 4);
        assert!(
            complete
                .local()
                .iter()
                .all(|item| matches!(item, LocalPullRequestObservation::Absent(_)))
        );
    }

    #[tokio::test]
    async fn response_aliases_cannot_be_reassigned_between_requested_heads() {
        let request = query_request(&[("A", None), ("B", None)], true);
        let swapped = observation_response(
            true,
            vec![
                connection(vec![fork(1, "BONE", "B")], None),
                connection(vec![fork(2, "AONE", "A")], None),
            ],
        );
        let (api_url, server) = scripted_peer(vec![exchange(request, Reply::Json(swapped))]).await;
        let error = test_github(&api_url, test_timeouts())
            .observe_local_pull_requests(&local(&["A", "B"]))
            .await
            .unwrap_err();
        let requests = finish_peer(server).await;

        assert_eq!(requests.len(), 1);
        assert!(format!("{error:?}").contains("returned head branch"));
    }

    #[tokio::test]
    async fn total_observation_deadline_preempts_one_attempt() {
        let request = query_request(&[("A", None)], true);
        let (api_url, server) =
            scripted_peer(vec![exchange(request, Reply::Hang(Duration::from_millis(250)))]).await;
        let mut timeouts = test_timeouts();
        timeouts.observation = Duration::from_millis(50);
        let started = Instant::now();
        let error = test_github(&api_url, timeouts)
            .observe_local_pull_requests(&local(&["A"]))
            .await
            .unwrap_err();
        let requests = finish_peer(server).await;

        assert_eq!(requests.len(), 1);
        assert!(error.to_string().contains("observation exceeded its total deadline"));
        assert!(started.elapsed() >= Duration::from_millis(35));
        assert!(started.elapsed() < Duration::from_secs(2));
    }

    #[tokio::test]
    async fn duplicate_json_is_rejected_before_resource_backoff() {
        let request = query_request(&[("A", None), ("B", None)], true);
        let duplicate = br#"{"errors":[{"type":"RESOURCE_LIMITS_EXCEEDED"}],"errors":[{"type":"RESOURCE_LIMITS_EXCEEDED"}]}"#.to_vec();
        let (api_url, server) = scripted_peer(vec![exchange(request, Reply::Raw(duplicate))]).await;
        let error = test_github(&api_url, test_timeouts())
            .observe_local_pull_requests(&local(&["A", "B"]))
            .await
            .unwrap_err();
        let requests = finish_peer(server).await;

        assert_eq!(requests.len(), 1);
        assert!(format!("{error:?}").contains("malformed JSON"));
    }

    #[tokio::test]
    async fn a_one_alias_oversized_cursor_query_is_rejected_without_a_second_request() {
        let oversized_cursor = "x".repeat(MAX_GRAPHQL_QUERY_BYTES);
        let first = observation_response(
            true,
            vec![connection(vec![fork(1, "ONE", "G")], Some(&oversized_cursor))],
        );
        let (api_url, server) =
            scripted_peer(vec![exchange(query_request(&[("G", None)], true), Reply::Json(first))])
                .await;
        let error = test_github(&api_url, test_timeouts())
            .observe_local_pull_requests(&local(&["G"]))
            .await
            .unwrap_err();
        let requests = finish_peer(server).await;

        assert!(error.to_string().contains("document limit"));
        assert_eq!(requests.len(), 1);
    }

    #[tokio::test]
    async fn all_mutation_batches_preflight_before_any_request() {
        let (api_url, server) = scripted_peer(Vec::new()).await;
        let _github = test_github(&api_url, test_timeouts());
        let mut creates = (0..64)
            .map(|index| TestCreate {
                id: id(&format!("G{index}")),
                title: format!("title {index}"),
                body: String::new(),
                head_oid: oid(index + 10),
                base_oid: oid(1),
            })
            .collect::<Vec<_>>();
        creates.push(TestCreate {
            id: id("Goversized"),
            title: "oversized".to_owned(),
            body: "x".repeat(MAX_MUTATION_REQUEST_BYTES),
            head_oid: oid(1_000),
            base_oid: oid(1),
        });
        let result = PreparedCreates::for_test(
            "REPOSITORY_NODE".to_owned(),
            creates,
            PullRequestIdentityRegistry::default(),
        );
        let error = match result {
            Ok(_) => panic!("an oversized later batch was accepted"),
            Err(error) => error,
        };
        let requests = finish_peer(server).await;

        assert!(error.to_string().contains("No mutation was sent"));
        assert!(requests.is_empty());
    }

    #[tokio::test]
    async fn create_execution_returns_ordered_exact_receipts() {
        let request = create_request(&[("A", 10), ("B", 11)]);
        let response = create_response(&[("A", 10, 7, "PRA"), ("B", 11, 8, "PRB")]);
        let (api_url, server) = scripted_peer(vec![exchange(request, Reply::Json(response))]).await;
        let creates = PreparedCreates::for_test(
            "REPOSITORY_NODE".to_owned(),
            vec![test_create("A", 10), test_create("B", 11)],
            PullRequestIdentityRegistry::default(),
        )
        .unwrap();
        let receipts =
            test_github(&api_url, test_timeouts()).create_pull_requests(creates).await.unwrap();
        let requests = finish_peer(server).await;

        assert_eq!(requests.len(), 1);
        assert_eq!(
            receipts
                .iter()
                .map(|(id, identity)| (
                    id.as_str(),
                    identity.number().get(),
                    identity.node_id().as_str()
                ))
                .collect::<Vec<_>>(),
            [("A", 7, "PRA"), ("B", 8, "PRB")]
        );
    }

    #[tokio::test]
    async fn create_receipts_must_be_new_in_both_observed_identity_namespaces() {
        let request = create_request(&[("A", 10)]);
        for (case, number, node_id) in [("number", 7, "NEWNODE"), ("node ID", 8, "OBSERVED")] {
            let response = create_response(&[("A", 10, number, node_id)]);
            let (api_url, server) =
                scripted_peer(vec![exchange(request.clone(), Reply::Json(response))]).await;
            let mut identities = PullRequestIdentityRegistry::default();
            identities
                .insert_observation(&PullRequestIdentity::new(7, "OBSERVED".to_owned()).unwrap())
                .unwrap();
            let creates = PreparedCreates::for_test(
                "REPOSITORY_NODE".to_owned(),
                vec![test_create("A", 10)],
                identities,
            )
            .unwrap();
            let result = test_github(&api_url, test_timeouts()).create_pull_requests(creates).await;
            let requests = finish_peer(server).await;
            let error = match result {
                Ok(_) => panic!("a create receipt reused an observed pull request {case}"),
                Err(error) => error,
            };

            assert_eq!(requests.len(), 1);
            assert!(error.to_string().contains("acknowledgement is indeterminate"));
        }
    }

    #[tokio::test]
    async fn create_receipts_reject_alias_reassignment_and_component_collisions() {
        let request = create_request(&[("A", 10), ("B", 11)]);
        let cases = [
            ("number collision", create_response(&[("A", 10, 7, "PRA"), ("B", 11, 7, "PRB")])),
            ("node collision", create_response(&[("A", 10, 7, "SAME"), ("B", 11, 8, "SAME")])),
            ("swapped aliases", create_response(&[("B", 11, 8, "PRB"), ("A", 10, 7, "PRA")])),
        ];
        for (case, response) in cases {
            let (api_url, server) =
                scripted_peer(vec![exchange(request.clone(), Reply::Json(response))]).await;
            let creates = PreparedCreates::for_test(
                "REPOSITORY_NODE".to_owned(),
                vec![test_create("A", 10), test_create("B", 11)],
                PullRequestIdentityRegistry::default(),
            )
            .unwrap();
            let result = test_github(&api_url, test_timeouts()).create_pull_requests(creates).await;
            let requests = finish_peer(server).await;

            let error = match result {
                Ok(_) => panic!("accepted {case}"),
                Err(error) => error,
            };
            assert_eq!(requests.len(), 1, "unexpected request count for {case}");
            assert!(error.to_string().contains("acknowledgement is indeterminate"));
        }
    }

    #[tokio::test]
    async fn a_cross_batch_create_collision_withholds_receipts_and_later_batches() {
        let ids = (0..129).map(|index| format!("G{index}")).collect::<Vec<_>>();
        let node_ids = (0..129).map(|index| format!("PR{index}")).collect::<Vec<_>>();
        let creates = ids
            .iter()
            .enumerate()
            .map(|(index, id)| test_create(id, index as u64 + 10))
            .collect::<Vec<_>>();

        let first_request = ids[..64]
            .iter()
            .enumerate()
            .map(|(index, id)| (id.as_str(), index as u64 + 10))
            .collect::<Vec<_>>();
        let first_response = ids[..64]
            .iter()
            .enumerate()
            .map(|(index, id)| {
                (
                    id.as_str(),
                    index as u64 + 10,
                    u32::try_from(index + 1).unwrap(),
                    node_ids[index].as_str(),
                )
            })
            .collect::<Vec<_>>();
        let second_request = ids[64..128]
            .iter()
            .enumerate()
            .map(|(offset, id)| (id.as_str(), (offset + 64) as u64 + 10))
            .collect::<Vec<_>>();
        let second_response = ids[64..128]
            .iter()
            .enumerate()
            .map(|(offset, id)| {
                let index = offset + 64;
                (
                    id.as_str(),
                    index as u64 + 10,
                    if offset == 0 { 1 } else { u32::try_from(index + 1).unwrap() },
                    node_ids[index].as_str(),
                )
            })
            .collect::<Vec<_>>();
        let (api_url, server) = scripted_peer(vec![
            exchange(create_request(&first_request), Reply::Json(create_response(&first_response))),
            exchange(
                create_request(&second_request),
                Reply::Json(create_response(&second_response)),
            ),
        ])
        .await;
        let creates = PreparedCreates::for_test(
            "REPOSITORY_NODE".to_owned(),
            creates,
            PullRequestIdentityRegistry::default(),
        )
        .unwrap();
        let result = test_github(&api_url, test_timeouts()).create_pull_requests(creates).await;
        let requests = finish_peer(server).await;

        let error = match result {
            Ok(_) => panic!("a cross-batch identity collision was accepted"),
            Err(error) => error,
        };
        assert_eq!(requests.len(), 2);
        assert!(error.to_string().contains("acknowledgement is indeterminate"));
    }

    #[tokio::test]
    async fn every_unusable_mutation_response_is_indeterminate_and_stops_the_stage() {
        let mut invalid_receipt = update_response(1..=64);
        invalid_receipt["data"]["op0"]["pullRequest"]["id"] = json!("OTHER");
        let replies = [
            Reply::Raw(b"{".to_vec()),
            Reply::Raw(br#"{"data":{},"data":{}}"#.to_vec()),
            Reply::Json(json!({ "errors": [{ "type": "FORBIDDEN" }] })),
            Reply::Json(invalid_receipt),
            Reply::Truncated { declared_bytes: 100, body: b"{}".to_vec() },
            Reply::Oversized(vec![b' '; MAX_GRAPHQL_MUTATION_RESPONSE_BYTES + 1]),
        ];
        for reply in replies {
            let (api_url, server) =
                scripted_peer(vec![exchange(update_request(1..=64), reply)]).await;
            let updates =
                PreparedUpdates::for_test((1..=65).map(test_update).collect::<Vec<_>>()).unwrap();
            let error = test_github(&api_url, test_timeouts())
                .update_pull_requests(updates)
                .await
                .unwrap_err();
            let requests = finish_peer(server).await;

            assert_eq!(requests.len(), 1);
            assert!(error.to_string().contains("acknowledgement is indeterminate"));
        }
    }

    #[tokio::test]
    async fn a_disconnect_after_mutation_send_is_indeterminate_and_never_replayed() {
        let request = update_request(1..=1);
        let (api_url, server) = scripted_peer(vec![exchange(request, Reply::Disconnect)]).await;
        let updates = PreparedUpdates::for_test(vec![test_update(1)]).unwrap();
        let error =
            test_github(&api_url, test_timeouts()).update_pull_requests(updates).await.unwrap_err();
        let requests = finish_peer(server).await;

        assert_eq!(requests.len(), 1);
        assert!(error.to_string().contains("acknowledgement is indeterminate"));
    }

    #[tokio::test]
    async fn a_mutation_attempt_timeout_is_indeterminate_and_never_replayed() {
        let request = update_request(1..=1);
        let (api_url, server) =
            scripted_peer(vec![exchange(request, Reply::Hang(Duration::from_millis(250)))]).await;
        let updates = PreparedUpdates::for_test(vec![test_update(1)]).unwrap();
        let mut timeouts = test_timeouts();
        timeouts.attempt = Duration::from_millis(50);
        let error =
            test_github(&api_url, timeouts).update_pull_requests(updates).await.unwrap_err();
        let requests = finish_peer(server).await;

        assert_eq!(requests.len(), 1);
        assert!(error.to_string().contains("acknowledgement is indeterminate"));
        assert!(format!("{error:?}").contains("wall-clock deadline"));
    }

    #[tokio::test]
    async fn a_later_indeterminate_batch_stops_before_the_third_batch() {
        let updates = (1..=129).map(test_update).collect::<Vec<_>>();
        let first_request = update_request(1..=64);
        let second_request = update_request(65..=128);
        let first_response = update_response(1..=64);
        let (api_url, server) = scripted_peer(vec![
            exchange(first_request, Reply::Json(first_response)),
            exchange(second_request, Reply::Disconnect),
        ])
        .await;
        let updates = PreparedUpdates::for_test(updates).unwrap();
        let error =
            test_github(&api_url, test_timeouts()).update_pull_requests(updates).await.unwrap_err();
        let requests = finish_peer(server).await;

        assert_eq!(requests.len(), 2);
        assert!(error.to_string().contains("acknowledgement is indeterminate"));
    }

    #[tokio::test]
    async fn mutation_redirects_and_retryable_statuses_are_never_replayed() {
        for status in [429, 503] {
            let request = update_request(1..=1);
            let (api_url, server) =
                scripted_peer(vec![exchange(request, Reply::Status(status))]).await;
            let updates = PreparedUpdates::for_test(vec![test_update(1)]).unwrap();
            let error = test_github(&api_url, test_timeouts())
                .update_pull_requests(updates)
                .await
                .unwrap_err();
            let requests = finish_peer(server).await;

            assert_eq!(requests.len(), 1, "HTTP {status} was replayed");
            assert!(error.to_string().contains("acknowledgement is indeterminate"));
        }

        for status in [307, 308] {
            let request = update_request(1..=1);
            let (api_url, server) = scripted_peer(vec![exchange(
                request,
                Reply::Redirect { status, location: "/graphql".to_owned() },
            )])
            .await;
            let updates = PreparedUpdates::for_test(vec![test_update(1)]).unwrap();
            let error = test_github(&api_url, test_timeouts())
                .update_pull_requests(updates)
                .await
                .unwrap_err();
            let requests = finish_peer(server).await;

            assert_eq!(requests.len(), 1, "HTTP {status} redirect was followed");
            assert!(error.to_string().contains("acknowledgement is indeterminate"));
        }
    }
}

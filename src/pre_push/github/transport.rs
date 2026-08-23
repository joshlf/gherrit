use std::{
    collections::{HashMap, HashSet, VecDeque},
    fmt,
    time::Duration,
};

use color_eyre::{
    Report,
    eyre::{Context as _, Result, bail, eyre},
};
use http_body_util::{BodyExt, LengthLimitError, Limited};
use octocrab::Octocrab;
use serde_json::Value;

use super::{
    CompleteCreateReceipts, CorrelatedRepository, PreparedCreates, PreparedUpdates, Repository,
    graphql_error_detail,
    observation::{
        CompleteLocalPullRequests, LocalPullRequestPageEvidence, LocalPullRequestQuery,
        LocalPullRequests,
    },
};
use crate::pre_push::{
    bounded_diagnostic_detail,
    destination::{DefaultBranch, PushDestination, RepositoryCoordinates},
    local::GherritPrId,
    pull_request::correlate_local,
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
/// A read-only GraphQL JSON response may occupy at most 64 MiB locally.
///
/// Overflow is a deterministic query-planning signal: exact-local observation
/// halves aliases and then page size without spending retry budget or
/// advancing a cursor.
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
            GraphqlBodyError::Read(error.to_string())
        }
    })
}

fn http_status_error(status: impl fmt::Display, context: &str, body: Option<&[u8]>) -> Report {
    let detail =
        body.and_then(|body| serde_json::from_slice::<Value>(body).ok()).and_then(|response| {
            response
                .get("message")
                .and_then(Value::as_str)
                .map(bounded_diagnostic_detail)
                .filter(|detail| !detail.is_empty())
                .or_else(|| graphql_error_detail(&response))
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

/// A concrete GitHub adapter bound to one repository for one attempt.
pub(in crate::pre_push) struct Github {
    http: Octocrab,
    coordinates: RepositoryCoordinates,
    timeouts: Timeouts,
}

/// Complete all-state pull request observation for the exact local ID set.
#[derive(Debug)]
pub(in crate::pre_push) struct LocalPullRequestObservationSet {
    repository: Repository,
    rows: CompleteLocalPullRequests,
}

impl LocalPullRequestObservationSet {
    pub(in crate::pre_push) fn correlate(
        self,
        default_branch: &DefaultBranch,
    ) -> Result<CorrelatedRepository> {
        let correlated = correlate_local(default_branch, self.rows)?;
        Ok(CorrelatedRepository::new(self.repository, correlated))
    }
}

#[derive(Debug)]
enum LocalPullRequestProgress {
    Initial,
    Next { cursor: String, seen: HashSet<String> },
    Exhausted,
}

impl LocalPullRequestProgress {
    fn expects(&self, after: Option<&str>) -> bool {
        match self {
            Self::Initial => after.is_none(),
            Self::Next { cursor, .. } => after == Some(cursor),
            Self::Exhausted => false,
        }
    }

    fn advance(self, id: &GherritPrId, next_cursor: Option<String>) -> Result<Self> {
        let Some(next_cursor) = next_cursor else {
            return Ok(Self::Exhausted);
        };
        if next_cursor.is_empty() {
            bail!(
                "local pull request observation returned an empty pagination cursor for '{}'",
                id.as_str()
            );
        }
        let mut seen = match self {
            Self::Initial => HashSet::new(),
            Self::Next { seen, .. } => seen,
            Self::Exhausted => bail!(
                "local pull request observation returned another page after exhausting '{}'",
                id.as_str()
            ),
        };
        if !seen.insert(next_cursor.clone()) {
            bail!(
                "local pull request observation repeated a pagination cursor for '{}'",
                id.as_str()
            );
        }
        Ok(Self::Next { cursor: next_cursor, seen })
    }
}

#[derive(Debug)]
struct LocalPullRequestAccumulator {
    order: Box<[GherritPrId]>,
    progress: HashMap<GherritPrId, LocalPullRequestProgress>,
    rows: HashMap<GherritPrId, Vec<super::ObservedPullRequest>>,
}

impl LocalPullRequestAccumulator {
    fn new(ids: impl IntoIterator<Item = GherritPrId>) -> Result<Self> {
        let mut order = Vec::new();
        let mut progress = HashMap::new();
        for id in ids {
            if progress.insert(id.clone(), LocalPullRequestProgress::Initial).is_some() {
                bail!(
                    "local pull request observation requested change '{}' more than once",
                    id.as_str()
                );
            }
            order.push(id);
        }
        if order.is_empty() {
            bail!("local pull request observation requires at least one change");
        }
        Ok(Self { order: order.into_boxed_slice(), progress, rows: HashMap::new() })
    }

    fn record_page(&mut self, evidence: LocalPullRequestPageEvidence) -> Result<()> {
        let (id, after, page_rows, next_cursor) = evidence.into_parts();
        let progress = self.progress.remove(&id).ok_or_else(|| {
            eyre!("local pull request observation returned unrequested change '{}'", id.as_str())
        })?;
        if !progress.expects(after.as_deref()) {
            bail!(
                "local pull request observation returned an unexpected page cursor for '{}'",
                id.as_str()
            );
        }
        self.rows.entry(id.clone()).or_default().extend(page_rows);
        let progress = progress.advance(&id, next_cursor)?;
        assert!(self.progress.insert(id, progress).is_none());
        Ok(())
    }

    fn finish(self) -> Result<CompleteLocalPullRequests> {
        let mut incomplete = self
            .progress
            .iter()
            .filter(|(_, progress)| !matches!(progress, LocalPullRequestProgress::Exhausted))
            .map(|(id, _)| id.as_str())
            .collect::<Vec<_>>();
        incomplete.sort_unstable();
        if !incomplete.is_empty() {
            bail!(
                "local pull request observation did not exhaust change ID(s): {}",
                incomplete.join(", ")
            );
        }
        let mut rows = self.rows;
        let entries = self
            .order
            .into_vec()
            .into_iter()
            .map(|id| {
                let pull_requests = rows.remove(&id).unwrap_or_default();
                (id, pull_requests)
            })
            .collect();
        debug_assert!(rows.is_empty());
        CompleteLocalPullRequests::new(entries)
    }
}

/// A mutation either has a complete acknowledgement or is indeterminate.
#[derive(Debug)]
pub(in crate::pre_push) enum MutationAcknowledgement<T> {
    Acknowledged(T),
    Indeterminate(Report),
}

impl<T> MutationAcknowledgement<T> {
    pub(in crate::pre_push) fn into_result(self) -> Result<T> {
        match self {
            Self::Acknowledged(value) => Ok(value),
            Self::Indeterminate(error) => Err(error),
        }
    }
}

impl Github {
    pub(in crate::pre_push) fn new(
        token: String,
        api_url: Option<&str>,
        destination: &PushDestination,
    ) -> Result<Self> {
        Self::with_timeouts(
            token,
            api_url,
            destination.repository_coordinates(),
            Timeouts::PRODUCTION,
        )
    }

    fn with_timeouts(
        token: String,
        api_url: Option<&str>,
        coordinates: RepositoryCoordinates,
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
        Ok(Self { http: builder.build()?, coordinates, timeouts })
    }

    /// Renders the browser URL for a PR in the repository bound to this client.
    pub(in crate::pre_push) fn pull_request_url(&self, number: u32) -> String {
        format!("https://github.com{}/pull/{number}", self.coordinates.relative_url())
    }

    /// Observes every lifecycle state for each exact local change ID.
    pub(in crate::pre_push) async fn observe_local_pull_requests(
        &self,
        ids: Box<[GherritPrId]>,
    ) -> Result<LocalPullRequestObservationSet> {
        tokio::time::timeout(self.timeouts.observation, self.observe_local_pull_requests_inner(ids))
            .await
            .map_err(|_| eyre!("GitHub exact-local observation exceeded its total deadline"))?
    }

    async fn observe_local_pull_requests_inner(
        &self,
        ids: Box<[GherritPrId]>,
    ) -> Result<LocalPullRequestObservationSet> {
        #[derive(Debug)]
        struct Pending {
            id: GherritPrId,
            cursor: Option<String>,
        }

        let mut accumulator = LocalPullRequestAccumulator::new(ids.iter().cloned())?;
        let mut pending = ids
            .into_vec()
            .into_iter()
            .map(|id| Pending { id, cursor: None })
            .collect::<VecDeque<_>>();
        let mut repository = None;
        let mut batch_len = LocalPullRequests::MAX_ALIASES;
        let mut page_len = 100;

        while !pending.is_empty() {
            let count = pending.len().min(batch_len);
            let batch = pending.drain(..count).collect::<Vec<_>>();
            let queries = batch
                .iter()
                .map(|pending| {
                    LocalPullRequestQuery::new(pending.id.clone(), pending.cursor.clone(), page_len)
                })
                .collect::<Result<Vec<_>>>()?;
            let operation =
                LocalPullRequests::new(self.coordinates.clone(), queries, repository.is_none())?;
            let response = self.run_observation_query(&operation.document()).await?;
            let Some(response) = response else {
                if batch.len() == 1 {
                    if page_len == 1 {
                        bail!(
                            "GitHub local pull request query for '{}' exceeds resource limits",
                            batch[0].id.as_str()
                        );
                    }
                    let retry_page_len = page_len / 2;
                    log::warn!(
                        "Backing off local pull request page size from {page_len} to {retry_page_len}."
                    );
                    page_len = retry_page_len;
                    pending.push_front(batch.into_iter().next().expect("one local query"));
                    continue;
                }
                let retry_batch_len = batch.len() / 2;
                log::warn!("Hit GitHub resource limit with GraphQL batch of size {}", batch.len());
                log::warn!(
                    "Backing off GraphQL batch size from {} to {retry_batch_len}.",
                    batch.len()
                );
                batch_len = retry_batch_len;
                for pending_item in batch.into_iter().rev() {
                    pending.push_front(pending_item);
                }
                continue;
            };
            let decoded = operation.decode(response)?;
            match (repository.is_none(), decoded.repository) {
                (true, Some(observed)) => repository = Some(observed),
                (false, None) => {}
                (true, None) => bail!("GitHub omitted initial repository facts"),
                (false, Some(_)) => bail!("GitHub repeated repository facts"),
            }
            for evidence in decoded.pages {
                let next_cursor = evidence.next_cursor().map(ToOwned::to_owned);
                let id = evidence.id().clone();
                accumulator.record_page(evidence)?;
                if let Some(cursor) = next_cursor {
                    pending.push_back(Pending { id, cursor: Some(cursor) });
                }
            }
        }

        let repository = repository.ok_or_else(|| eyre!("GitHub omitted repository facts"))?;
        Ok(LocalPullRequestObservationSet { repository, rows: accumulator.finish()? })
    }

    pub(in crate::pre_push) async fn create_pull_requests(
        &self,
        creates: PreparedCreates,
    ) -> MutationAcknowledgement<CompleteCreateReceipts> {
        let PreparedCreates { batches, mut receipts, .. } = creates;
        for batch in batches.into_vec() {
            log::trace!(
                "Sending GraphQL create batch ({} operations, {} bytes)",
                batch.expected.len(),
                batch.serialized_bytes
            );
            let response = match self.send_mutation_once(&batch.request).await {
                Ok(response) => response,
                Err(error) => return Self::indeterminate(error),
            };
            let batch_receipts = match batch.decode(response) {
                Ok(receipts) => receipts,
                Err(error) => return Self::indeterminate(error),
            };
            if let Err(error) = receipts.record(batch_receipts) {
                return Self::indeterminate(error);
            }
        }
        match receipts.finish() {
            Ok(receipts) => MutationAcknowledgement::Acknowledged(receipts),
            Err(error) => Self::indeterminate(error),
        }
    }

    pub(in crate::pre_push) async fn update_pull_requests(
        &self,
        updates: PreparedUpdates,
    ) -> MutationAcknowledgement<()> {
        for batch in updates.batches.into_vec() {
            log::trace!(
                "Sending GraphQL update batch ({} operations, {} bytes)",
                batch.expected.len(),
                batch.serialized_bytes
            );
            let response = match self.send_mutation_once(&batch.request).await {
                Ok(response) => response,
                Err(error) => return Self::indeterminate(error),
            };
            if let Err(error) = batch.decode(response) {
                return Self::indeterminate(error);
            }
        }
        MutationAcknowledgement::Acknowledged(())
    }

    fn indeterminate<T>(error: Report) -> MutationAcknowledgement<T> {
        MutationAcknowledgement::Indeterminate(error.wrap_err(INDETERMINATE_GRAPHQL_MUTATION))
    }

    /// Sends one mutation request exactly once.
    ///
    /// This function is intentionally loop-free. The caller consumes the batch
    /// after this one attempt and never makes it available to query retry code.
    async fn send_mutation_once(&self, request: &Value) -> Result<Value> {
        tokio::time::timeout(self.timeouts.attempt, self.mutation_attempt_inner(request))
            .await
            .map_err(|_| eyre!("GraphQL mutation attempt exceeded its wall-clock deadline"))?
    }

    async fn mutation_attempt_inner(&self, request: &Value) -> Result<Value> {
        let response = self
            .http
            ._post("/graphql", Some(request))
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
        serde_json::from_slice(&body).wrap_err("Failed to decode GraphQL mutation response JSON")
    }

    async fn run_query(&self, request: &Value) -> Result<Value> {
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

    async fn query_attempt(&self, request: &Value) -> Result<Value, QueryAttemptError> {
        match tokio::time::timeout(self.timeouts.attempt, self.query_attempt_inner(request)).await {
            Ok(result) => result,
            Err(_) => Err(QueryAttemptError::Transient(eyre!(
                "GraphQL read-only attempt exceeded its wall-clock deadline"
            ))),
        }
    }

    async fn query_attempt_inner(&self, request: &Value) -> Result<Value, QueryAttemptError> {
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
        serde_json::from_slice(&body).map_err(|error| {
            QueryAttemptError::Fatal(
                Report::from(error).wrap_err("Failed to decode GraphQL query response JSON"),
            )
        })
    }

    async fn run_observation_query(&self, query: &str) -> Result<Option<Value>> {
        if query.len() > MAX_GRAPHQL_QUERY_BYTES {
            return Ok(None);
        }
        let request = serde_json::json!({ "query": query });
        let response = match self.run_query(&request).await {
            Ok(response) => response,
            Err(error) if error.downcast_ref::<QueryResponseSizeLimit>().is_some() => {
                return Ok(None);
            }
            Err(error) => return Err(error).wrap_err("GraphQL read-only observation failed"),
        };
        match classify_response(&response) {
            ResponseDisposition::Success => Ok(Some(response)),
            ResponseDisposition::ResourceLimit => Ok(None),
            ResponseDisposition::Fatal => {
                if let Some(detail) = graphql_error_detail(&response) {
                    bail!("GitHub returned fatal GraphQL errors: {detail}");
                }
                bail!("GitHub returned fatal GraphQL errors")
            }
        }
    }
}

#[derive(Debug)]
enum QueryAttemptError {
    Transient(Report),
    Fatal(Report),
}

fn classify_octocrab_error(error: octocrab::Error) -> QueryAttemptError {
    let retryable =
        matches!(error, octocrab::Error::Service { .. } | octocrab::Error::Hyper { .. });
    if retryable {
        QueryAttemptError::Transient(Report::from(error))
    } else {
        QueryAttemptError::Fatal(Report::from(error))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ResponseDisposition {
    Success,
    ResourceLimit,
    Fatal,
}

fn classify_response(response: &Value) -> ResponseDisposition {
    let Some(errors) = response.get("errors") else {
        return ResponseDisposition::Success;
    };
    if matches!(errors.as_array(), Some(errors) if errors.is_empty()) {
        return ResponseDisposition::Success;
    }
    let has_no_data = response.get("data").is_none_or(Value::is_null);
    let has_only_resource_errors = errors
        .as_array()
        .is_some_and(|errors| !errors.is_empty() && errors.iter().all(is_resource_limit_error));
    if has_no_data && has_only_resource_errors {
        ResponseDisposition::ResourceLimit
    } else {
        ResponseDisposition::Fatal
    }
}

fn is_resource_limit_error(error: &Value) -> bool {
    let is_typed_resource_error = matches!(
        error.get("type").and_then(Value::as_str),
        Some("RESOURCE_LIMITS_EXCEEDED" | "MAX_NODE_LIMIT_EXCEEDED")
    );
    let is_oversized_request_error = matches!(
        error.get("message").and_then(Value::as_str),
        Some("A query attribute must be specified and must be a string.")
    );
    is_typed_resource_error || is_oversized_request_error
}

#[cfg(test)]
mod tests {
    use std::time::Instant;

    use http_body_util::Full;
    use tokio::{
        io::{AsyncReadExt as _, AsyncWriteExt as _},
        net::{TcpListener, TcpStream},
        task::JoinHandle,
    };

    use super::*;

    fn id(value: &str) -> GherritPrId {
        GherritPrId::from_ref_component(value.as_bytes()).unwrap()
    }

    fn observed(number: u64, node_id: &str, head: &str) -> super::super::ObservedPullRequest {
        super::super::ObservedPullRequest {
            identity: super::super::PullRequestIdentity::new(number, node_id.to_owned()).unwrap(),
            title: format!("title {number}"),
            body: format!("body {number}"),
            base_branch: "main".to_owned(),
            head_branch: head.to_owned(),
            base_oid: gix::ObjectId::from_bytes_or_panic(&[2; 20]),
            head_oid: gix::ObjectId::from_bytes_or_panic(&[3; 20]),
            state: super::super::PullRequestState::Open,
            is_cross_repository: false,
            has_auto_merge_request: false,
            is_in_merge_queue: false,
        }
    }

    fn page(
        id_value: &str,
        after: Option<&str>,
        rows: Vec<super::super::ObservedPullRequest>,
        next_cursor: Option<&str>,
    ) -> LocalPullRequestPageEvidence {
        LocalPullRequestPageEvidence::for_test(
            id(id_value),
            after.map(str::to_owned),
            rows,
            next_cursor.map(str::to_owned),
        )
    }

    async fn read_request_head(stream: &mut TcpStream) {
        let mut request = Vec::new();
        let mut buffer = [0_u8; 4096];
        loop {
            let count = stream.read(&mut buffer).await.unwrap();
            assert_ne!(count, 0, "client closed before sending an HTTP request head");
            request.extend_from_slice(&buffer[..count]);
            if request.windows(4).any(|window| window == b"\r\n\r\n") {
                return;
            }
        }
    }

    async fn status_server(statuses: Vec<u16>) -> (String, JoinHandle<Vec<Instant>>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let mut attempts = Vec::new();
            for status in statuses {
                let (mut stream, _) = listener.accept().await.unwrap();
                attempts.push(Instant::now());
                read_request_head(&mut stream).await;
                let body = br#"{"message":"scripted status"}"#;
                let response = format!(
                    "HTTP/1.1 {status} scripted\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                );
                stream.write_all(response.as_bytes()).await.unwrap();
                stream.write_all(body).await.unwrap();
            }
            attempts
        });
        (format!("http://{address}"), server)
    }

    async fn hanging_server() -> (String, JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            read_request_head(&mut stream).await;
            std::future::pending::<()>().await;
        });
        (format!("http://{address}"), server)
    }

    fn test_github(api_url: &str, timeouts: Timeouts) -> Github {
        let destination = PushDestination::for_test(
            "origin",
            &format!("https://github.com/owner/{}.git", testutil::DEFAULT_REPO),
            Vec::new(),
        )
        .unwrap();
        Github::with_timeouts(
            "token".to_owned(),
            Some(api_url),
            destination.repository_coordinates(),
            timeouts,
        )
        .unwrap()
    }

    #[test]
    fn production_transport_limits_are_fixed_and_finite() {
        assert_eq!(Timeouts::PRODUCTION.connect, Duration::from_secs(10));
        assert_eq!(Timeouts::PRODUCTION.read, Duration::from_secs(30));
        assert_eq!(Timeouts::PRODUCTION.write, Duration::from_secs(30));
        assert_eq!(Timeouts::PRODUCTION.attempt, Duration::from_secs(60));
        assert_eq!(Timeouts::PRODUCTION.observation, Duration::from_secs(10 * 60));
        assert_eq!(QUERY_RETRY_DELAYS.map(|delay| delay.as_millis()), [100, 200, 400]);
        assert_eq!(MAX_HTTP_ERROR_RESPONSE_BYTES, 64 * 1024);
        assert_eq!(MAX_GRAPHQL_QUERY_RESPONSE_BYTES, 64 * 1024 * 1024);
        assert_eq!(MAX_GRAPHQL_MUTATION_RESPONSE_BYTES, 4 * 1024 * 1024);
    }

    #[test]
    fn local_accumulator_advances_connections_independently_and_preserves_id_order() {
        let mut accumulator = LocalPullRequestAccumulator::new([id("A"), id("B")]).unwrap();
        accumulator.record_page(page("B", None, vec![observed(2, "PR_B", "B")], None)).unwrap();
        accumulator
            .record_page(page("A", None, vec![observed(1, "PR_A_1", "A")], Some("A_NEXT")))
            .unwrap();
        accumulator
            .record_page(page("A", Some("A_NEXT"), vec![observed(3, "PR_A_2", "A")], None))
            .unwrap();

        let (entries, _) = accumulator.finish().unwrap().into_parts();
        assert_eq!(
            entries
                .iter()
                .map(|(id, rows)| {
                    (
                        id.as_str(),
                        rows.iter().map(|row| row.identity.number().get()).collect::<Vec<_>>(),
                    )
                })
                .collect::<Vec<_>>(),
            [("A", vec![1, 3]), ("B", vec![2])]
        );
    }

    #[test]
    fn local_accumulator_rejects_invalid_progress_transitions() {
        let mut wrong_input = LocalPullRequestAccumulator::new([id("A")]).unwrap();
        assert!(
            wrong_input
                .record_page(page("A", Some("unexpected"), Vec::new(), None))
                .unwrap_err()
                .to_string()
                .contains("unexpected page cursor")
        );

        let mut repeated_output = LocalPullRequestAccumulator::new([id("A")]).unwrap();
        repeated_output.record_page(page("A", None, Vec::new(), Some("same"))).unwrap();
        assert!(
            repeated_output
                .record_page(page("A", Some("same"), Vec::new(), Some("same")))
                .unwrap_err()
                .to_string()
                .contains("repeated a pagination cursor")
        );

        let mut exhausted = LocalPullRequestAccumulator::new([id("A")]).unwrap();
        exhausted.record_page(page("A", None, Vec::new(), None)).unwrap();
        assert!(
            exhausted
                .record_page(page("A", None, Vec::new(), None))
                .unwrap_err()
                .to_string()
                .contains("unexpected page cursor")
        );

        let mut unrequested = LocalPullRequestAccumulator::new([id("A")]).unwrap();
        assert!(
            unrequested
                .record_page(page("B", None, Vec::new(), None))
                .unwrap_err()
                .to_string()
                .contains("unrequested change 'B'")
        );
    }

    #[test]
    fn local_accumulator_requires_exhaustion_and_rejects_cross_page_identity_reuse() {
        let incomplete = LocalPullRequestAccumulator::new([id("B"), id("A")]).unwrap();
        assert!(incomplete.finish().unwrap_err().to_string().contains("change ID(s): A, B"));

        let mut duplicate = LocalPullRequestAccumulator::new([id("A")]).unwrap();
        duplicate
            .record_page(page("A", None, vec![observed(1, "PR_ONE", "A")], Some("next")))
            .unwrap();
        duplicate
            .record_page(page("A", Some("next"), vec![observed(1, "PR_ONE", "A")], None))
            .unwrap();
        assert!(
            duplicate.finish().unwrap_err().to_string().contains("repeated pull request number 1")
        );
    }

    #[tokio::test]
    async fn bounded_body_collector_accepts_the_limit_and_rejects_one_more_byte() {
        assert_eq!(
            collect_body_with_limit(Full::new(&b"1234"[..]), 4).await.unwrap().to_bytes(),
            &b"1234"[..]
        );
        assert_eq!(
            collect_body_with_limit(Full::new(&b"12345"[..]), 4).await.unwrap_err(),
            GraphqlBodyError::ExceededLimit
        );
    }

    #[tokio::test]
    async fn retryable_queries_wait_for_each_backoff_before_the_next_attempt() {
        let (api_url, server) = status_server(vec![503; 4]).await;
        let github = test_github(
            &api_url,
            Timeouts {
                connect: Duration::from_secs(1),
                read: Duration::from_secs(1),
                write: Duration::from_secs(1),
                attempt: Duration::from_secs(2),
                observation: Duration::from_secs(10),
            },
        );

        assert!(
            github
                .run_query(&serde_json::json!({ "query": "query { viewer { id } }" }))
                .await
                .is_err()
        );
        let attempts = server.await.unwrap();
        assert_eq!(attempts.len(), 4);
        for (attempts, expected) in attempts.windows(2).zip(QUERY_RETRY_DELAYS) {
            assert!(
                attempts[1].duration_since(attempts[0]) >= expected,
                "retry started before the {expected:?} backoff elapsed"
            );
        }
    }

    #[tokio::test]
    async fn mutation_attempt_honors_the_total_wall_clock_deadline() {
        let (api_url, server) = hanging_server().await;
        let github = test_github(
            &api_url,
            Timeouts {
                connect: Duration::from_secs(1),
                read: Duration::from_secs(1),
                write: Duration::from_secs(1),
                attempt: Duration::from_millis(40),
                observation: Duration::from_secs(10),
            },
        );

        let started = Instant::now();
        let error = github
            .send_mutation_once(&serde_json::json!({ "query": "mutation { noop }" }))
            .await
            .unwrap_err();
        let elapsed = started.elapsed();
        server.abort();

        assert!(error.to_string().contains("wall-clock deadline"));
        assert!(elapsed >= Duration::from_millis(30));
        assert!(elapsed < Duration::from_millis(500));
    }

    #[tokio::test]
    async fn exact_local_observation_has_one_deadline_across_all_attempts_and_pages() {
        let (api_url, server) = hanging_server().await;
        let github = test_github(
            &api_url,
            Timeouts {
                connect: Duration::from_secs(1),
                read: Duration::from_secs(1),
                write: Duration::from_secs(1),
                attempt: Duration::from_secs(1),
                observation: Duration::from_millis(40),
            },
        );

        let started = Instant::now();
        let error =
            github.observe_local_pull_requests(vec![id("A")].into_boxed_slice()).await.unwrap_err();
        let elapsed = started.elapsed();
        server.abort();

        assert!(error.to_string().contains("exact-local observation exceeded its total deadline"));
        assert!(elapsed >= Duration::from_millis(30));
        assert!(elapsed < Duration::from_millis(500));
    }

    #[test]
    fn graphql_response_disposition_requires_unmixed_resource_errors() {
        assert_eq!(
            classify_response(&serde_json::json!({ "data": {} })),
            ResponseDisposition::Success
        );
        assert_eq!(
            classify_response(&serde_json::json!({ "data": {}, "errors": [] })),
            ResponseDisposition::Success
        );
        assert_eq!(
            classify_response(&serde_json::json!({
                "errors": [{ "type": "RESOURCE_LIMITS_EXCEEDED" }]
            })),
            ResponseDisposition::ResourceLimit
        );
        assert_eq!(
            classify_response(&serde_json::json!({
                "errors": [
                    { "type": "RESOURCE_LIMITS_EXCEEDED" },
                    { "message": "fatal" },
                ]
            })),
            ResponseDisposition::Fatal
        );
    }

    #[test]
    fn untrusted_http_error_detail_is_single_line_and_bounded() {
        let message = format!("{}\nnot-disclosed", "x".repeat(1_000));
        let body = serde_json::to_vec(&serde_json::json!({ "message": message })).unwrap();
        let error = http_status_error(400, "a test request", Some(&body)).to_string();

        assert!(!error.contains('\n'));
        assert!(!error.contains("not-disclosed"));
        assert!(error.ends_with("..."));
        assert!(error.len() <= 320);
    }
}

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
    CompleteCreateReceipts, CompleteOpenRows, CorrelatedRepository, FirstOpenPullRequests,
    FirstOpenPullRequestsPage, NextOpenPullRequests, OpenPullRequest, PreparedCreates,
    PreparedUpdates, Repository, RepositoryTerminalHistories, TerminalPullRequestQuery,
    TerminalPullRequests, graphql_error_detail,
};
use crate::pre_push::{
    bounded_diagnostic_detail,
    destination::{PushDestination, RepositoryCoordinates},
    local::GherritPrId,
    pull_request::{
        CreateAuthorizations, InitialPullRequestIdentities, TerminalExhaustionAccumulator,
        correlate_complete,
    },
    remote::RemoteHeads,
};

const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const READ_TIMEOUT: Duration = Duration::from_secs(30);
const WRITE_TIMEOUT: Duration = Duration::from_secs(30);
const TOTAL_ATTEMPT_TIMEOUT: Duration = Duration::from_secs(60);

const QUERY_RETRY_DELAYS: [Duration; 3] =
    [Duration::from_millis(100), Duration::from_millis(200), Duration::from_millis(400)];

const MAX_GRAPHQL_QUERY_BYTES: usize = 256 * 1024;
const MAX_HTTP_ERROR_RESPONSE_BYTES: usize = 64 * 1024;
/// A read-only GraphQL JSON response may occupy at most 64 MiB locally.
///
/// Overflow is a deterministic query-planning signal: OPEN observation halves
/// its page size, and terminal observation halves aliases then page size,
/// without spending retry budget or advancing a cursor.
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
}

impl Timeouts {
    const PRODUCTION: Self = Self {
        connect: CONNECT_TIMEOUT,
        read: READ_TIMEOUT,
        write: WRITE_TIMEOUT,
        attempt: TOTAL_ATTEMPT_TIMEOUT,
    };
}

/// A concrete GitHub adapter bound to one repository for one attempt.
pub(in crate::pre_push) struct Github {
    http: Octocrab,
    coordinates: RepositoryCoordinates,
    timeouts: Timeouts,
}

/// One complete repository-wide OPEN observation.
///
/// The rows and initial identity namespaces remain opaque. Correlation or the
/// temporary legacy adapter must consume this value directly; no API exposes a
/// detachable list which can be truncated and relabelled as complete.
#[derive(Debug)]
pub(in crate::pre_push) struct OpenObservation {
    repository: Repository,
    rows: CompleteOpenRows,
}

impl OpenObservation {
    #[cfg(test)]
    pub(in crate::pre_push) fn from_complete_response_for_test(
        owner: &str,
        repository: &str,
        response: Value,
    ) -> Result<Self> {
        let page = FirstOpenPullRequests::new(owner.to_owned(), repository.to_owned(), 100)
            .decode(response)?;
        if page.next_cursor.is_some() {
            bail!("a complete test OPEN response cannot advertise another page");
        }
        Ok(Self { repository: page.repository, rows: CompleteOpenRows::new(page.pull_requests)? })
    }

    #[allow(dead_code)] // Consumed by the pending owned-base activation path.
    pub(in crate::pre_push) fn correlate<'a, 'destination>(
        self,
        local_ids: impl IntoIterator<Item = &'a GherritPrId>,
        heads: &RemoteHeads<'destination>,
    ) -> Result<CorrelatedRepository<'destination>> {
        let correlated = correlate_complete(local_ids, heads, self.rows)?;
        Ok(CorrelatedRepository::new(heads.destination(), self.repository, correlated))
    }

    fn into_legacy_selection(self, local_ids: &[GherritPrId]) -> Result<LegacyOpenSelection> {
        let (pull_requests, initial_identities) = self.rows.into_values();
        let local_id_set = local_ids.iter().collect::<HashSet<_>>();
        let mut candidates = HashMap::<GherritPrId, Vec<OpenPullRequest>>::new();

        for pull_request in pull_requests.into_vec() {
            if pull_request.is_cross_repository {
                continue;
            }
            let Ok(id) = GherritPrId::from_ref_component(pull_request.head_branch.as_bytes())
            else {
                continue;
            };
            if local_id_set.contains(&id) {
                candidates.entry(id).or_default().push(pull_request);
            }
        }

        let mut selected = Vec::with_capacity(local_ids.len());
        let mut missing = Vec::new();
        for id in local_ids {
            match candidates.remove(id) {
                None => {
                    missing.push(id.clone());
                    selected.push(None);
                }
                Some(mut candidates) if candidates.len() == 1 => {
                    selected.push(Some(candidates.pop().expect("one candidate is present")));
                }
                Some(candidates) => {
                    let id = bounded_diagnostic_detail(id.as_str());
                    let mut numbers = candidates
                        .iter()
                        .map(|pull_request| pull_request.number)
                        .collect::<Vec<_>>();
                    numbers.sort_unstable();
                    let numbers = numbers
                        .into_iter()
                        .map(|number| format!("#{number}"))
                        .collect::<Vec<_>>()
                        .join(", ");
                    bail!(
                        "Found multiple open pull requests for GHerrit ID '{}': {numbers}. GHerrit cannot safely choose one.",
                        id
                    );
                }
            }
        }

        Ok(LegacyOpenSelection {
            repository: self.repository,
            local_pull_requests: selected,
            initial_identities,
            missing: missing.into_boxed_slice(),
        })
    }
}

#[derive(Debug)]
struct LegacyOpenSelection {
    repository: Repository,
    local_pull_requests: Vec<Option<OpenPullRequest>>,
    initial_identities: InitialPullRequestIdentities,
    missing: Box<[GherritPrId]>,
}

/// Complete compatibility observation consumed by the pre-activation caller.
pub(in crate::pre_push) struct LegacyGithubObservation {
    pub(in crate::pre_push) repository: Repository,
    pub(in crate::pre_push) local_pull_requests: Vec<Option<OpenPullRequest>>,
    pub(in crate::pre_push) initial_identities: InitialPullRequestIdentities,
    pub(in crate::pre_push) create_authorizations: CreateAuthorizations,
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

    /// Observes every page of the repository-wide OPEN connection.
    pub(in crate::pre_push) async fn observe_open_pull_requests(&self) -> Result<OpenObservation> {
        let mut page_len = 100;
        let first_page = loop {
            let operation =
                FirstOpenPullRequests::for_repository(self.coordinates.clone(), page_len);
            let Some(response) = self.run_observation_query(&operation.document()).await? else {
                if page_len == 1 {
                    bail!(
                        "The repository-wide open pull request query exceeds GitHub resource limits"
                    );
                }
                let retry_page_len = page_len / 2;
                log::warn!(
                    "Backing off the repository-wide open pull request page size from {page_len} to {retry_page_len}."
                );
                page_len = retry_page_len;
                continue;
            };
            break operation.decode(response)?;
        };
        let FirstOpenPullRequestsPage {
            repository,
            pull_requests: first_pull_requests,
            next_cursor,
        } = first_page;
        let mut seen_cursors = next_cursor.iter().cloned().collect::<HashSet<_>>();
        let mut pull_requests = first_pull_requests;
        let mut cursor = next_cursor;

        while let Some(current_cursor) = cursor {
            let operation = NextOpenPullRequests::for_repository(
                self.coordinates.clone(),
                current_cursor.clone(),
                page_len,
            );
            let Some(response) = self.run_observation_query(&operation.document()).await? else {
                if page_len == 1 {
                    bail!(
                        "The repository-wide open pull request query exceeds GitHub resource limits"
                    );
                }
                let retry_page_len = page_len / 2;
                log::warn!(
                    "Backing off the repository-wide open pull request page size from {page_len} to {retry_page_len}."
                );
                page_len = retry_page_len;
                cursor = Some(current_cursor);
                continue;
            };
            let page = operation.decode(response)?;
            pull_requests.extend(page.pull_requests);
            if let Some(next_cursor) = &page.next_cursor
                && !seen_cursors.insert(next_cursor.clone())
            {
                bail!("GitHub repeated an open pull request pagination cursor");
            }
            cursor = page.next_cursor;
        }

        Ok(OpenObservation { repository, rows: CompleteOpenRows::new(pull_requests)? })
    }

    /// Returns complete ordered terminal-history evidence for the exact
    /// requested ID set.
    pub(in crate::pre_push) async fn observe_terminal_pull_requests(
        &self,
        ids: Box<[GherritPrId]>,
    ) -> Result<RepositoryTerminalHistories> {
        #[derive(Debug)]
        struct Pending {
            id: GherritPrId,
            cursor: Option<String>,
        }

        let mut accumulator = TerminalExhaustionAccumulator::new(ids.iter().cloned())?;
        let mut pending = ids
            .into_vec()
            .into_iter()
            .map(|id| Pending { id, cursor: None })
            .collect::<VecDeque<_>>();
        let mut batch_len = TerminalPullRequests::MAX_ALIASES;
        let mut page_len = 100;

        while !pending.is_empty() {
            let count = pending.len().min(batch_len);
            let batch = pending.drain(..count).collect::<Vec<_>>();
            let queries = batch
                .iter()
                .map(|pending| {
                    TerminalPullRequestQuery::new(
                        pending.id.clone(),
                        pending.cursor.clone(),
                        page_len,
                    )
                })
                .collect::<Result<Vec<_>>>()?;
            let operation = TerminalPullRequests::new(
                self.coordinates.owner().to_owned(),
                self.coordinates.repository().to_owned(),
                queries,
            )?;
            let response = self.run_observation_query(&operation.document()).await?;
            let Some(response) = response else {
                if batch.len() == 1 {
                    if page_len == 1 {
                        bail!(
                            "GitHub terminal pull request query for '{}' exceeds resource limits",
                            batch[0].id.as_str()
                        );
                    }
                    let retry_page_len = page_len / 2;
                    log::warn!(
                        "Backing off terminal pull request page size from {page_len} to {retry_page_len}."
                    );
                    page_len = retry_page_len;
                    pending.push_front(batch.into_iter().next().expect("one terminal query"));
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
            let data =
                response.get("data").and_then(|data| data.get("repository")).cloned().ok_or_else(
                    || eyre!("GitHub terminal pull request response is missing repository data"),
                )?;
            for evidence in operation.decode(data)? {
                let next_cursor = evidence.next_cursor().map(ToOwned::to_owned);
                let id = evidence.id().clone();
                accumulator = accumulator.record_page(evidence)?;
                if let Some(cursor) = next_cursor {
                    pending.push_back(Pending { id, cursor: Some(cursor) });
                }
            }
        }
        Ok(RepositoryTerminalHistories::from_transport(
            self.coordinates.clone(),
            accumulator.into_terminal_histories()?,
        ))
    }

    /// Temporary adapter for the legacy orchestration retained until activation.
    pub(in crate::pre_push) async fn observe_legacy_pull_requests(
        &self,
        ids: &[GherritPrId],
    ) -> Result<LegacyGithubObservation> {
        let selection = self.observe_open_pull_requests().await?.into_legacy_selection(ids)?;
        let authorizations = self.observe_terminal_pull_requests(selection.missing).await?;
        let authorizations = authorizations.into_legacy_for(&selection.repository.coordinates)?;
        Ok(LegacyGithubObservation {
            repository: selection.repository,
            local_pull_requests: selection.local_pull_requests,
            initial_identities: selection.initial_identities,
            create_authorizations: authorizations,
        })
    }

    pub(in crate::pre_push) async fn create_pull_requests(
        &self,
        creates: PreparedCreates,
    ) -> MutationAcknowledgement<CompleteCreateReceipts> {
        let PreparedCreates { batches, mut receipts } = creates;
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

    fn observation_fields(open: bool) -> Vec<String> {
        let mut fields = if open {
            vec![
                "nodes.autoMergeRequest.enabledAt",
                "nodes.baseRefName",
                "nodes.baseRefOid",
                "nodes.body",
                "nodes.headRefName",
                "nodes.headRefOid",
                "nodes.id",
                "nodes.isCrossRepository",
                "nodes.isInMergeQueue",
                "nodes.number",
                "nodes.state",
                "nodes.title",
            ]
        } else {
            vec![
                "nodes.headRefName",
                "nodes.id",
                "nodes.isCrossRepository",
                "nodes.number",
                "nodes.state",
            ]
        };
        fields.extend(["pageInfo.endCursor", "pageInfo.hasNextPage"]);
        fields.into_iter().map(str::to_owned).collect()
    }

    fn connection(
        alias: Option<&str>,
        head: Option<&str>,
        after: Option<&str>,
        open: bool,
    ) -> testutil::PullRequestConnectionExchange {
        connection_with_first(alias, head, after, open, 100)
    }

    fn connection_with_first(
        alias: Option<&str>,
        head: Option<&str>,
        after: Option<&str>,
        open: bool,
        first: usize,
    ) -> testutil::PullRequestConnectionExchange {
        testutil::PullRequestConnectionExchange {
            alias: alias.map(str::to_owned),
            head: head.map(str::to_owned),
            first,
            after: after.map(str::to_owned),
            states: if open {
                vec!["OPEN".to_owned()]
            } else {
                vec!["CLOSED".to_owned(), "MERGED".to_owned()]
            },
            selected_fields: observation_fields(open),
        }
    }

    fn mock_github_context() -> testutil::TestContext {
        testutil::TestContextBuilder::new(std::env::current_exe().unwrap())
            .with_remote()
            .with_initial_commit()
            .with_mock_github()
            .build()
    }

    fn seed_pull_request(
        context: &testutil::TestContext,
        number: usize,
        head: String,
        state: testutil::PullRequestState,
    ) {
        context.github().seed_pull_request(testutil::PullRequestSeed {
            number,
            title: format!("Title {number}"),
            body: String::new(),
            head,
            head_oid: "2".repeat(40),
            base: "main".to_owned(),
            base_oid: "1".repeat(40),
        });
        context.github().set_pull_request_state(number, state);
    }

    #[test]
    fn production_transport_limits_are_fixed_and_finite() {
        assert_eq!(Timeouts::PRODUCTION.connect, Duration::from_secs(10));
        assert_eq!(Timeouts::PRODUCTION.read, Duration::from_secs(30));
        assert_eq!(Timeouts::PRODUCTION.write, Duration::from_secs(30));
        assert_eq!(Timeouts::PRODUCTION.attempt, Duration::from_secs(60));
        assert_eq!(QUERY_RETRY_DELAYS.map(|delay| delay.as_millis()), [100, 200, 400]);
        assert_eq!(MAX_HTTP_ERROR_RESPONSE_BYTES, 64 * 1024);
        assert_eq!(MAX_GRAPHQL_QUERY_RESPONSE_BYTES, 64 * 1024 * 1024);
        assert_eq!(MAX_GRAPHQL_MUTATION_RESPONSE_BYTES, 4 * 1024 * 1024);
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
    async fn facade_exhausts_every_open_page_before_exposing_observation() {
        let context = mock_github_context();
        for number in 1..=101 {
            seed_pull_request(
                &context,
                number,
                format!("G{}", number - 1),
                testutil::PullRequestState::Open,
            );
        }
        context.github().expect_graphql_transcript([
            testutil::GraphQlExchange::Repository {
                owner: testutil::DEFAULT_OWNER.to_owned(),
                repository: testutil::DEFAULT_REPO.to_owned(),
                selected_fields: vec![
                    "defaultBranchRef.name".to_owned(),
                    "defaultBranchRef.target.oid".to_owned(),
                    "id".to_owned(),
                ],
                connections: vec![connection(None, None, None, true)],
            },
            testutil::GraphQlExchange::Repository {
                owner: testutil::DEFAULT_OWNER.to_owned(),
                repository: testutil::DEFAULT_REPO.to_owned(),
                selected_fields: Vec::new(),
                connections: vec![connection(None, None, Some("cursor:100"), true)],
            },
        ]);
        let github = test_github(context.mock_server_url(), Timeouts::PRODUCTION);

        let ids = [
            GherritPrId::from_ref_component(b"G0").unwrap(),
            GherritPrId::from_ref_component(b"G100").unwrap(),
        ];
        let selection =
            github.observe_open_pull_requests().await.unwrap().into_legacy_selection(&ids).unwrap();

        context.github().assert_graphql_transcript_consumed();
        assert!(selection.local_pull_requests.iter().all(Option::is_some));
        assert_eq!(selection.initial_identities.len(), 101);
        assert_eq!(
            selection
                .local_pull_requests
                .iter()
                .map(|pull_request| pull_request.as_ref().unwrap().number)
                .collect::<Vec<_>>(),
            [1, 101]
        );
    }

    #[tokio::test]
    async fn open_resource_planning_reduces_pages_without_cursor_drift_or_retry() {
        let context = mock_github_context();
        for number in 1..=51 {
            seed_pull_request(
                &context,
                number,
                format!("G{}", number - 1),
                testutil::PullRequestState::Open,
            );
        }
        context.limit_graphql_connection_page_size(25);
        let exchange = |first, after: Option<&str>| testutil::GraphQlExchange::Repository {
            owner: testutil::DEFAULT_OWNER.to_owned(),
            repository: testutil::DEFAULT_REPO.to_owned(),
            selected_fields: if after.is_none() {
                vec![
                    "defaultBranchRef.name".to_owned(),
                    "defaultBranchRef.target.oid".to_owned(),
                    "id".to_owned(),
                ]
            } else {
                Vec::new()
            },
            connections: vec![connection_with_first(None, None, after, true, first)],
        };
        context.github().expect_graphql_transcript([
            exchange(100, None),
            exchange(50, None),
            exchange(25, None),
            exchange(25, Some("cursor:25")),
            exchange(25, Some("cursor:50")),
        ]);
        let github = test_github(context.mock_server_url(), Timeouts::PRODUCTION);
        let ids = [
            GherritPrId::from_ref_component(b"G0").unwrap(),
            GherritPrId::from_ref_component(b"G50").unwrap(),
        ];

        let selection =
            github.observe_open_pull_requests().await.unwrap().into_legacy_selection(&ids).unwrap();

        context.github().assert_graphql_transcript_consumed();
        assert_eq!(context.github().requests().len(), 5);
        assert_eq!(selection.initial_identities.len(), 51);
        assert!(selection.local_pull_requests.iter().all(Option::is_some));
    }

    #[tokio::test]
    async fn terminal_facade_paginates_ids_independently_and_preserves_cursors() {
        let context = mock_github_context();
        for number in 1..=101 {
            seed_pull_request(&context, number, "A".to_owned(), testutil::PullRequestState::Closed);
            context.github().mark_pull_request_cross_repository(number);
        }
        context.github().expect_graphql_transcript([
            testutil::GraphQlExchange::Repository {
                owner: testutil::DEFAULT_OWNER.to_owned(),
                repository: testutil::DEFAULT_REPO.to_owned(),
                selected_fields: Vec::new(),
                connections: vec![
                    connection(Some("op0"), Some("A"), None, false),
                    connection(Some("op1"), Some("B"), None, false),
                ],
            },
            testutil::GraphQlExchange::Repository {
                owner: testutil::DEFAULT_OWNER.to_owned(),
                repository: testutil::DEFAULT_REPO.to_owned(),
                selected_fields: Vec::new(),
                connections: vec![connection(Some("op0"), Some("A"), Some("cursor:100"), false)],
            },
        ]);
        let github = test_github(context.mock_server_url(), Timeouts::PRODUCTION);
        let ids =
            ["A", "B"].map(|id| GherritPrId::from_ref_component(id.as_bytes()).unwrap()).into();

        let _terminal_histories = github.observe_terminal_pull_requests(ids).await.unwrap();

        context.github().assert_graphql_transcript_consumed();
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

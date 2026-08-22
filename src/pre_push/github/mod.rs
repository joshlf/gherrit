use std::collections::{BTreeMap, HashMap, HashSet};

use color_eyre::eyre::{Context as _, Result, bail, eyre};
use serde::Deserialize;
use serde_json::{Value, json};

use super::{
    bounded_diagnostic_detail,
    destination::DefaultBranch,
    local::GherritPrId,
    pull_request::{
        InitialPullRequestIdentities, PullRequestIdentity, PullRequestNodeId, PullRequestNumber,
    },
};

mod transport;

#[allow(unused_imports)]
pub(super) use transport::{
    Github, LegacyGithubObservation, MutationAcknowledgement, OpenObservation,
};

const MAX_MUTATION_ALIASES: usize = 64;
// A 131,072-byte pull-request body made entirely from U+0001 expands to
// 917,504 bytes after GraphQL-string escaping and then outer-JSON escaping.
// One MiB accommodates that worst case plus the mutation's other supported
// fields while retaining a deterministic preflight request limit.
const MAX_MUTATION_REQUEST_BYTES: usize = 1024 * 1024;

fn graphql_error_detail(response: &Value) -> Option<String> {
    response
        .get("errors")
        .and_then(Value::as_array)
        .and_then(|errors| errors.first())
        .and_then(|error| error.get("message"))
        .and_then(Value::as_str)
        .map(bounded_diagnostic_detail)
        .filter(|detail| !detail.is_empty())
}

/// A selected nullable GraphQL field. Unlike `Option<T>`, this rejects a
/// missing response key while accepting an explicit JSON null.
#[derive(Deserialize)]
#[serde(untagged)]
enum Nullable<T> {
    Value(T),
    Null(()),
}

#[derive(Debug, Clone, PartialEq, Eq)]
/// Complete pull-request facts returned by the repository-wide open scan.
///
/// The compatibility selector currently consumes only identity, lifecycle,
/// and head-repository facts. Keeping the exact refs, object IDs, and policy
/// state here makes the observation itself complete, so later validation does
/// not need another network read.
pub(super) struct OpenPullRequest {
    pub(super) number: u64,
    pub(super) node_id: String,
    pub(super) title: String,
    pub(super) body: String,
    pub(super) base_branch: String,
    pub(super) head_branch: String,
    pub(super) base_oid: gix::ObjectId,
    pub(super) head_oid: gix::ObjectId,
    pub(super) is_cross_repository: bool,
    pub(super) has_auto_merge_request: bool,
    pub(super) is_in_merge_queue: bool,
}

/// Complete repository-wide OPEN rows coupled to their identity namespaces.
///
/// Only the paginator constructs this value. It remains nested inside
/// [`OpenObservation`](transport::OpenObservation) until correlation or the
/// temporary legacy selector consumes that observation.
#[derive(Debug)]
pub(super) struct CompleteOpenRows {
    pull_requests: Box<[OpenPullRequest]>,
    initial_identities: InitialPullRequestIdentities,
}

impl CompleteOpenRows {
    fn new(pull_requests: Vec<OpenPullRequest>) -> Result<Self> {
        let initial_identities = InitialPullRequestIdentities::from_open(&pull_requests)?;
        Ok(Self { pull_requests: pull_requests.into_boxed_slice(), initial_identities })
    }

    pub(super) fn into_values(self) -> (Box<[OpenPullRequest]>, InitialPullRequestIdentities) {
        (self.pull_requests, self.initial_identities)
    }

    #[cfg(test)]
    pub(super) fn for_test(pull_requests: Vec<OpenPullRequest>) -> Result<Self> {
        Self::new(pull_requests)
    }
}

fn mutation_response_data(
    response: Value,
    expected_aliases: &[Box<str>],
) -> Result<serde_json::Map<String, Value>> {
    if let Some(errors) = response.get("errors")
        && !matches!(errors.as_array(), Some(errors) if errors.is_empty())
    {
        if let Some(detail) = graphql_error_detail(&response) {
            bail!("GraphQL mutation response contains errors: {detail}");
        }
        bail!("GraphQL mutation response contains errors");
    }

    let mut data = response
        .get("data")
        .ok_or_else(|| eyre!("Missing JSON field in GraphQL mutation response: `data`"))?
        .as_object()
        .ok_or_else(|| eyre!("GraphQL mutation response field `data` is not an object"))?
        .clone();

    let expected_aliases = expected_aliases.iter().map(Box::as_ref).collect::<HashSet<_>>();
    for alias in &expected_aliases {
        if !data.contains_key(*alias) {
            bail!("GraphQL mutation response is missing operation `{alias}`");
        }
    }
    if let Some(alias) = data.keys().find(|alias| !expected_aliases.contains(alias.as_str())) {
        let alias = bounded_diagnostic_detail(alias);
        bail!("GraphQL mutation response contains unexpected operation `{alias}`");
    }
    Ok(std::mem::take(&mut data))
}

/// Repository facts which must agree with the exact Git push destination.
#[derive(Debug, Eq, PartialEq)]
pub(super) struct Repository {
    pub(super) node_id: String,
    pub(super) default_branch: DefaultBranch,
}

/// The first page of the repository-wide open-pull-request connection.
#[derive(Debug, Clone, PartialEq, Eq)]
struct FirstOpenPullRequests {
    owner: String,
    repository: String,
    first: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct NextOpenPullRequests {
    owner: String,
    repository: String,
    after: String,
    first: usize,
}

#[derive(Debug, Eq, PartialEq)]
struct FirstOpenPullRequestsPage {
    repository: Repository,
    pull_requests: Vec<OpenPullRequest>,
    next_cursor: Option<String>,
}

#[derive(Debug, Eq, PartialEq)]
struct NextOpenPullRequestsPage {
    pull_requests: Vec<OpenPullRequest>,
    next_cursor: Option<String>,
}

impl FirstOpenPullRequests {
    fn new(owner: String, repository: String, first: usize) -> Self {
        assert!(first > 0, "an open pull request page size must be positive");
        Self { owner, repository, first }
    }

    fn document(&self) -> String {
        open_pull_requests_document(
            &self.owner,
            &self.repository,
            self.first,
            OpenPullRequestPage::First,
        )
    }

    fn decode(&self, response: Value) -> Result<FirstOpenPullRequestsPage> {
        let DecodedOpenPullRequestsPage {
            repository_node_id,
            default_branch_ref,
            pull_requests,
            next_cursor,
        } = decode_open_pull_requests_page(response)?;
        let node_id = match repository_node_id {
            Some(node_id) if !node_id.is_empty() => node_id,
            Some(_) => bail!("GitHub reported an empty repository node ID"),
            None => bail!("GitHub omitted the repository node ID"),
        };
        let default_branch = match default_branch_ref {
            Some(default_branch) => default_branch,
            None => bail!("GitHub omitted the repository default branch"),
        };
        let target = match default_branch.target {
            Nullable::Value(target) => target,
            Nullable::Null(()) => bail!("GitHub omitted the default branch target"),
        };
        let oid = match target.oid {
            Nullable::Value(oid) => oid,
            Nullable::Null(()) => bail!("GitHub omitted the default branch object ID"),
        };
        let tip = gix::ObjectId::from_hex(oid.as_bytes())
            .map_err(|_| eyre!("GitHub reported an invalid default branch object ID"))?;
        let repository = Repository {
            node_id,
            default_branch: DefaultBranch::new(default_branch.name, tip)
                .map_err(|_| eyre!("GitHub reported an invalid default branch"))?,
        };

        Ok(FirstOpenPullRequestsPage { repository, pull_requests, next_cursor })
    }
}

impl NextOpenPullRequests {
    fn new(owner: String, repository: String, after: String, first: usize) -> Self {
        assert!(!after.is_empty(), "an open pull request cursor must be nonempty");
        assert!(first > 0, "an open pull request page size must be positive");
        Self { owner, repository, after, first }
    }

    fn document(&self) -> String {
        open_pull_requests_document(
            &self.owner,
            &self.repository,
            self.first,
            OpenPullRequestPage::Next { after: &self.after },
        )
    }

    fn decode(&self, response: Value) -> Result<NextOpenPullRequestsPage> {
        if response.pointer("/data/repository").and_then(Value::as_object).is_some_and(
            |repository| {
                repository.contains_key("id") || repository.contains_key("defaultBranchRef")
            },
        ) {
            bail!("GitHub returned unrequested repository facts on a later open PR page");
        }
        let DecodedOpenPullRequestsPage {
            repository_node_id: _,
            default_branch_ref: _,
            pull_requests,
            next_cursor,
        } = decode_open_pull_requests_page(response)?;
        Ok(NextOpenPullRequestsPage { pull_requests, next_cursor })
    }
}

enum OpenPullRequestPage<'a> {
    First,
    Next { after: &'a str },
}

fn open_pull_requests_document(
    owner: &str,
    repository: &str,
    first: usize,
    page: OpenPullRequestPage<'_>,
) -> String {
    let (repository_facts, after) = match page {
        OpenPullRequestPage::First => {
            ("id, defaultBranchRef { name, target { oid } } ", String::new())
        }
        OpenPullRequestPage::Next { after } => ("", format!(", after: {}", json!(after))),
    };
    format!(
        "query {{ repository(owner: {}, name: {}) {{ {repository_facts}pullRequests(first: {first}{after}, states: [OPEN]) {{ nodes {{ number, id, title, body, baseRefName, baseRefOid, headRefName, headRefOid, state, isCrossRepository, autoMergeRequest {{ enabledAt }}, isInMergeQueue }} pageInfo {{ hasNextPage, endCursor }} }} }} }}",
        json!(owner),
        json!(repository),
    )
}

struct DecodedOpenPullRequestsPage {
    repository_node_id: Option<String>,
    default_branch_ref: Option<DefaultBranchRef>,
    pull_requests: Vec<OpenPullRequest>,
    next_cursor: Option<String>,
}

#[derive(Deserialize)]
struct DefaultBranchRef {
    name: String,
    target: Nullable<GitObject>,
}

#[derive(Deserialize)]
struct GitObject {
    oid: Nullable<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
enum OpenPullRequestState {
    Open,
}

fn decode_open_pull_requests_page(response: Value) -> Result<DecodedOpenPullRequestsPage> {
    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct Response {
        id: Option<String>,
        default_branch_ref: Option<DefaultBranchRef>,
        pull_requests: PullRequests,
    }

    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct PullRequests {
        nodes: Vec<Node>,
        page_info: PageInfo,
    }

    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct PageInfo {
        has_next_page: bool,
        end_cursor: Nullable<String>,
    }

    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct AutoMergeRequest {
        enabled_at: Nullable<String>,
    }

    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct Node {
        number: i64,
        id: String,
        title: String,
        body: String,
        base_ref_name: String,
        base_ref_oid: String,
        head_ref_name: String,
        head_ref_oid: String,
        state: OpenPullRequestState,
        is_cross_repository: bool,
        auto_merge_request: Nullable<AutoMergeRequest>,
        is_in_merge_queue: bool,
    }

    let response = response
        .get("data")
        .and_then(|data| data.get("repository"))
        .cloned()
        .ok_or_else(|| eyre!("GitHub open pull request response is missing repository data"))?;
    let response: Response = serde_json::from_value(response)
        .map_err(|_| eyre!("Failed to decode pull request query response"))?;
    let pull_requests = response
        .pull_requests
        .nodes
        .into_iter()
        .map(|node| {
            let number = u64::try_from(node.number)
                .ok()
                .filter(|number| *number > 0 && *number <= i32::MAX as u64)
                .ok_or_else(|| {
                    eyre!("GitHub reported an invalid pull request number {}", node.number)
                })?;
            for (field, value) in [
                ("pull request node ID", &node.id),
                ("pull request base ref name", &node.base_ref_name),
                ("pull request head ref name", &node.head_ref_name),
            ] {
                if value.is_empty() {
                    bail!("GitHub reported an empty {field}");
                }
            }
            let parse_oid = |field: &str, oid: &str| {
                let object_id = gix::ObjectId::from_hex(oid.as_bytes())
                    .map_err(|_| eyre!("GitHub reported an invalid {field}"))?;
                if object_id.is_null() {
                    bail!("GitHub reported a null {field}");
                }
                Ok(object_id)
            };
            let base_oid = parse_oid("pull request base ref object ID", &node.base_ref_oid)?;
            let head_oid = parse_oid("pull request head ref object ID", &node.head_ref_oid)?;
            let OpenPullRequestState::Open = node.state;
            let has_auto_merge_request = match node.auto_merge_request {
                Nullable::Value(request) => {
                    let _ = request.enabled_at;
                    true
                }
                Nullable::Null(()) => false,
            };
            Ok(OpenPullRequest {
                number,
                node_id: node.id,
                title: node.title,
                body: node.body,
                base_branch: node.base_ref_name,
                head_branch: node.head_ref_name,
                base_oid,
                head_oid,
                is_cross_repository: node.is_cross_repository,
                has_auto_merge_request,
                is_in_merge_queue: node.is_in_merge_queue,
            })
        })
        .collect::<Result<Vec<_>>>()?;
    let next_cursor = match response.pull_requests.page_info {
        PageInfo { has_next_page: true, end_cursor: Nullable::Value(cursor) }
            if !cursor.is_empty() =>
        {
            Some(cursor)
        }
        PageInfo { has_next_page: true, .. } => {
            bail!("GitHub reported another open pull request page without an end cursor");
        }
        PageInfo { has_next_page: false, .. } => None,
    };

    Ok(DecodedOpenPullRequestsPage {
        repository_node_id: response.id,
        default_branch_ref: response.default_branch_ref,
        pull_requests,
        next_cursor,
    })
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct TerminalPullRequestQuery {
    id: GherritPrId,
    after: Option<String>,
    first: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct TerminalPullRequestPage {
    pub(super) pull_requests: Vec<TerminalPullRequest>,
    pub(super) next_cursor: Option<String>,
}

/// One decoded terminal page retaining the exact typed request token.
///
/// The ID and input cursor are moved from the query which produced this page;
/// callers cannot relabel otherwise valid evidence before recording it.
#[derive(Debug, PartialEq, Eq)]
pub(super) struct TerminalPullRequestEvidence {
    id: GherritPrId,
    after: Option<String>,
    page: TerminalPullRequestPage,
}

impl TerminalPullRequestEvidence {
    pub(super) fn id(&self) -> &GherritPrId {
        &self.id
    }

    pub(super) fn next_cursor(&self) -> Option<&str> {
        self.page.next_cursor.as_deref()
    }

    pub(super) fn into_parts(self) -> (GherritPrId, Option<String>, TerminalPullRequestPage) {
        (self.id, self.after, self.page)
    }

    #[cfg(test)]
    pub(super) fn for_test(
        id: GherritPrId,
        after: Option<String>,
        page: TerminalPullRequestPage,
    ) -> Self {
        Self { id, after, page }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct TerminalPullRequest {
    pub(super) number: u64,
    pub(super) node_id: String,
    pub(super) state: TerminalPullRequestState,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub(super) enum TerminalPullRequestState {
    Closed,
    Merged,
}

/// A repository-root terminal-history query with independently paged aliases.
#[derive(Debug, Clone, PartialEq, Eq)]
struct TerminalPullRequests {
    owner: String,
    repository: String,
    queries: Vec<TerminalPullRequestQuery>,
}

impl TerminalPullRequests {
    const MAX_ALIASES: usize = 64;

    fn new(
        owner: String,
        repository: String,
        queries: Vec<TerminalPullRequestQuery>,
    ) -> Result<Self> {
        if queries.is_empty() || queries.len() > Self::MAX_ALIASES {
            bail!("A terminal pull request query requires between one and 64 aliases");
        }
        Ok(Self { owner, repository, queries })
    }

    fn document(&self) -> String {
        let fields = self.queries.iter().enumerate().map(|(index, query)| {
            let after = query.after.as_ref()
                .map(|cursor| format!(", after: {}", json!(cursor)))
                .unwrap_or_default();
            format!(
                "op{index}: pullRequests(headRefName: {}, first: {}{after}, states: [CLOSED, MERGED]) {{ nodes {{ number, id, headRefName, state, isCrossRepository }} pageInfo {{ hasNextPage, endCursor }} }}",
                json!(query.id.as_str()),
                query.first,
            )
        }).collect::<String>();
        format!(
            "query {{ repository(owner: {}, name: {}) {{ {fields} }} }}",
            json!(self.owner),
            json!(self.repository),
        )
    }

    fn decode(self, response: Value) -> Result<Vec<TerminalPullRequestEvidence>> {
        #[derive(Deserialize)]
        #[serde(rename_all = "camelCase")]
        struct Connection {
            nodes: Vec<Node>,
            page_info: PageInfo,
        }
        #[derive(Deserialize)]
        #[serde(rename_all = "camelCase")]
        struct PageInfo {
            has_next_page: bool,
            end_cursor: Nullable<String>,
        }
        #[derive(Deserialize)]
        #[serde(rename_all = "camelCase")]
        struct Node {
            number: i64,
            id: String,
            head_ref_name: String,
            state: TerminalPullRequestState,
            is_cross_repository: bool,
        }

        let connections: BTreeMap<String, Connection> = serde_json::from_value(response)
            .map_err(|_| eyre!("Failed to decode terminal pull request query response"))?;
        let expected =
            (0..self.queries.len()).map(|index| format!("op{index}")).collect::<HashSet<_>>();
        if connections.len() != expected.len() {
            bail!("GitHub terminal pull request response has an unexpected alias set");
        }
        if let Some(alias) = connections.keys().find(|alias| !expected.contains(*alias)) {
            let alias = bounded_diagnostic_detail(alias);
            bail!("GitHub terminal pull request response contains unexpected operation `{alias}`");
        }

        self.queries
            .into_iter()
            .enumerate()
            .map(|(index, query)| {
                let alias = format!("op{index}");
                let connection = connections.get(&alias).ok_or_else(|| {
                    eyre!("GitHub terminal pull request response is missing operation `{alias}`")
                })?;
                let pull_requests = connection
                    .nodes
                    .iter()
                    .map(|node| {
                        let number = u64::try_from(node.number)
                            .ok()
                            .filter(|number| *number > 0 && *number <= i32::MAX as u64)
                            .ok_or_else(|| {
                                eyre!(
                                    "GitHub reported an invalid terminal pull request number {}",
                                    node.number
                                )
                            })?;
                        if node.id.is_empty() {
                            bail!("GitHub reported an empty terminal pull request node ID");
                        }
                        if node.head_ref_name != query.id.as_str() {
                            let returned = bounded_diagnostic_detail(&node.head_ref_name);
                            let expected = bounded_diagnostic_detail(query.id.as_str());
                            bail!(
                                "GitHub terminal pull request for '{}' returned head branch '{}'",
                                expected,
                                returned
                            );
                        }
                        Ok((!node.is_cross_repository).then(|| TerminalPullRequest {
                            number,
                            node_id: node.id.clone(),
                            state: node.state,
                        }))
                    })
                    .collect::<Result<Vec<_>>>()?
                    .into_iter()
                    .flatten()
                    .collect::<Vec<_>>();
                let next_cursor = match &connection.page_info {
                    PageInfo { has_next_page: true, end_cursor: Nullable::Value(cursor) }
                        if !cursor.is_empty() =>
                    {
                        Some(cursor.clone())
                    }
                    PageInfo { has_next_page: true, .. } => bail!(
                        "GitHub reported another terminal pull request page without an end cursor"
                    ),
                    PageInfo { has_next_page: false, .. } => None,
                };
                Ok(TerminalPullRequestEvidence {
                    id: query.id,
                    after: query.after,
                    page: TerminalPullRequestPage { pull_requests, next_cursor },
                })
            })
            .collect()
    }
}

impl TerminalPullRequestQuery {
    fn new(id: GherritPrId, after: Option<String>, first: usize) -> Result<Self> {
        if first == 0 {
            bail!("A terminal pull request query requires a positive page size");
        }
        if after.as_deref() == Some("") {
            bail!("A terminal pull request query requires a nonempty pagination cursor");
        }
        Ok(Self { id, after, first })
    }
}

/// A request to create one pull request.
///
/// The head name and response correlation ID derive from the typed change ID.
/// Exact agreement with neutral terminal evidence is checked when the complete
/// create batch is prepared.
#[derive(Debug, PartialEq, Eq)]
pub(super) struct CreatePullRequest {
    id: GherritPrId,
    repository_id: String,
    base_branch: String,
    title: String,
    body: String,
    client_mutation_id: String,
}

impl CreatePullRequest {
    pub(super) fn new(
        id: GherritPrId,
        repository_id: String,
        base_branch: String,
        title: String,
        body: String,
    ) -> Self {
        let client_mutation_id = format!("gherrit:create:{}", id.as_str());
        Self { id, repository_id, base_branch, title, body, client_mutation_id }
    }

    fn document(&self) -> String {
        let fields = [
            ("repositoryId", self.repository_id.as_str()),
            ("baseRefName", self.base_branch.as_str()),
            ("headRefName", self.id.as_str()),
            ("title", self.title.as_str()),
            ("body", self.body.as_str()),
            ("clientMutationId", self.client_mutation_id.as_str()),
        ]
        .map(|(name, value)| format!("{name}: {}", json!(value)))
        .join(", ");
        format!(
            "createPullRequest(input: {{ {fields} }}) {{ clientMutationId, pullRequest {{ number, id, headRefName }} }}"
        )
    }
}

#[derive(Debug, PartialEq, Eq)]
struct ExpectedCreateReceipt {
    alias: Box<str>,
    id: GherritPrId,
    head_branch: Box<str>,
    client_mutation_id: Box<str>,
}

impl ExpectedCreateReceipt {
    fn decode(&self, response: Value) -> Result<(GherritPrId, PullRequestIdentity)> {
        #[derive(Deserialize)]
        #[serde(rename_all = "camelCase")]
        struct Response {
            client_mutation_id: String,
            pull_request: Option<CreatedPullRequestResponse>,
        }

        #[derive(Deserialize)]
        #[serde(rename_all = "camelCase")]
        struct CreatedPullRequestResponse {
            number: u64,
            id: String,
            head_ref_name: String,
        }

        if response.is_null() {
            bail!("GraphQL mutation response operation `{}` is null", self.alias);
        }
        let response: Response = serde_json::from_value(response)
            .map_err(|_| eyre!("Failed to decode createPullRequest response"))?;
        if response.client_mutation_id != self.client_mutation_id.as_ref() {
            let returned = bounded_diagnostic_detail(&response.client_mutation_id);
            let expected = bounded_diagnostic_detail(&self.client_mutation_id);
            bail!(
                "createPullRequest echoed clientMutationId '{}', expected '{}'",
                returned,
                expected
            );
        }
        let created = response.pull_request.ok_or_else(|| {
            eyre!(
                "The batched GraphQL mutation failed to create PR for head branch '{}'. The response pull request was null.",
                self.head_branch
            )
        })?;
        if created.head_ref_name != self.head_branch.as_ref() {
            let returned = bounded_diagnostic_detail(&created.head_ref_name);
            let expected = bounded_diagnostic_detail(&self.head_branch);
            bail!("createPullRequest returned head branch '{}', expected '{}'", returned, expected);
        }
        let identity = PullRequestIdentity::new(created.number, created.id)?;
        Ok((self.id.clone(), identity))
    }
}

#[derive(Debug, PartialEq, Eq)]
pub(super) struct PreparedCreateBatch {
    request: Value,
    serialized_bytes: usize,
    expected: Box<[ExpectedCreateReceipt]>,
}

impl PreparedCreateBatch {
    fn aliases(&self) -> Vec<Box<str>> {
        self.expected.iter().map(|expected| expected.alias.clone()).collect()
    }

    fn decode(self, response: Value) -> Result<Vec<(GherritPrId, PullRequestIdentity)>> {
        let aliases = self.aliases();
        let mut data = mutation_response_data(response, &aliases)?;
        self.expected
            .into_vec()
            .into_iter()
            .map(|expected| {
                let response = data
                    .remove(expected.alias.as_ref())
                    .expect("the complete alias set was checked");
                expected.decode(response).wrap_err_with(|| {
                    format!("Invalid acknowledgement for mutation `{}`", expected.alias)
                })
            })
            .collect()
    }
}

fn serialized_mutation_request(fields: String) -> Result<(Value, usize)> {
    let request = json!({ "query": format!("mutation {{ {fields} }}") });
    let serialized_bytes = serde_json::to_vec(&request)
        .wrap_err("Failed to serialize a GraphQL mutation request")?
        .len();
    Ok((request, serialized_bytes))
}

fn create_batch(operations: &[CreatePullRequest]) -> Result<PreparedCreateBatch> {
    let mut fields = String::new();
    let mut expected = Vec::with_capacity(operations.len());
    for (index, operation) in operations.iter().enumerate() {
        let alias = format!("op{index}");
        fields.push_str(&format!("{alias}: {}", operation.document()));
        expected.push(ExpectedCreateReceipt {
            alias: alias.into_boxed_str(),
            id: operation.id.clone(),
            head_branch: operation.id.as_str().into(),
            client_mutation_id: operation.client_mutation_id.as_str().into(),
        });
    }
    let (request, serialized_bytes) = serialized_mutation_request(fields)?;
    Ok(PreparedCreateBatch { request, serialized_bytes, expected: expected.into_boxed_slice() })
}

fn prepare_create_batches(operations: &[CreatePullRequest]) -> Result<Box<[PreparedCreateBatch]>> {
    let mut batches = Vec::new();
    let mut start = 0;
    while start < operations.len() {
        let max_end = operations.len().min(start + MAX_MUTATION_ALIASES);
        let mut accepted = None;
        for end in start + 1..=max_end {
            let batch = create_batch(&operations[start..end])?;
            if batch.serialized_bytes > MAX_MUTATION_REQUEST_BYTES {
                break;
            }
            accepted = Some((end, batch));
        }
        let Some((end, batch)) = accepted else {
            let bytes = create_batch(&operations[start..start + 1])?.serialized_bytes;
            bail!(
                "GraphQL create mutation at item {start} serializes to {bytes} bytes, which exceeds the {MAX_MUTATION_REQUEST_BYTES}-byte request limit. No mutation was sent."
            );
        };
        batches.push(batch);
        start = end;
    }
    Ok(batches.into_boxed_slice())
}

#[derive(Debug)]
struct CreateReceiptPlan {
    expected: HashSet<GherritPrId>,
    order: Box<[GherritPrId]>,
    initial: InitialPullRequestIdentities,
    numbers: HashSet<PullRequestNumber>,
    node_ids: HashSet<PullRequestNodeId>,
    by_change: HashMap<GherritPrId, PullRequestIdentity>,
}

impl CreateReceiptPlan {
    fn record(&mut self, receipts: Vec<(GherritPrId, PullRequestIdentity)>) -> Result<()> {
        for (id, identity) in receipts {
            if !self.expected.contains(&id) {
                bail!("createPullRequest returned an unplanned head '{}'", id.as_str());
            }
            if self.by_change.contains_key(&id) {
                bail!("createPullRequest returned more than one receipt for '{}'", id.as_str());
            }
            if self.initial.contains_number(identity.number()) {
                bail!(
                    "createPullRequest receipt for '{}' repeats initial OPEN pull request number {}",
                    id.as_str(),
                    identity.number().get()
                );
            }
            if self.initial.contains_node_id(identity.node_id()) {
                let node_id = bounded_diagnostic_detail(identity.node_id().as_str());
                bail!(
                    "createPullRequest receipt for '{}' repeats initial OPEN pull request node ID '{}'",
                    id.as_str(),
                    node_id
                );
            }
            if !self.numbers.insert(identity.number()) {
                bail!(
                    "createPullRequest receipt for '{}' repeats created pull request number {}",
                    id.as_str(),
                    identity.number().get()
                );
            }
            if !self.node_ids.insert(identity.node_id().clone()) {
                let node_id = bounded_diagnostic_detail(identity.node_id().as_str());
                bail!(
                    "createPullRequest receipt for '{}' repeats created pull request node ID '{}'",
                    id.as_str(),
                    node_id
                );
            }
            assert!(self.by_change.insert(id, identity).is_none());
        }
        Ok(())
    }

    fn finish(self) -> Result<CompleteCreateReceipts> {
        if self.by_change.len() != self.expected.len() {
            let acknowledged = self.by_change.keys().cloned().collect::<HashSet<_>>();
            let mut missing = self
                .expected
                .difference(&acknowledged)
                .map(GherritPrId::as_str)
                .collect::<Vec<_>>();
            missing.sort_unstable();
            bail!("createPullRequest receipts are missing head(s): {}", missing.join(", "));
        }
        Ok(CompleteCreateReceipts { order: self.order, by_change: self.by_change })
    }
}

/// Every exact create request, plus the sole complete receipt validator.
#[derive(Debug)]
pub(super) struct PreparedCreates {
    batches: Box<[PreparedCreateBatch]>,
    receipts: CreateReceiptPlan,
}

impl PreparedCreates {
    pub(super) fn new(
        initial: InitialPullRequestIdentities,
        expected: HashSet<GherritPrId>,
        operations: Vec<CreatePullRequest>,
    ) -> Result<Self> {
        if operations.is_empty() {
            bail!("A prepared create action requires at least one operation");
        }
        let mut operation_ids = HashSet::with_capacity(operations.len());
        for (index, operation) in operations.iter().enumerate() {
            if !operation_ids.insert(operation.id.clone()) {
                bail!(
                    "GraphQL create mutation at item {index} repeats change '{}'. No mutation was sent.",
                    operation.id.as_str()
                );
            }
            if !expected.contains(&operation.id) {
                bail!(
                    "GraphQL create mutation at item {index} was not present in the exact missing-OPEN evidence for '{}'. No mutation was sent.",
                    operation.id.as_str()
                );
            }
        }
        if operation_ids != expected {
            let mut missing =
                expected.difference(&operation_ids).map(GherritPrId::as_str).collect::<Vec<_>>();
            missing.sort_unstable();
            bail!(
                "GraphQL create plan omits evidenced missing change(s): {}. No mutation was sent.",
                missing.join(", ")
            );
        }

        let order = operations.iter().map(|operation| operation.id.clone()).collect::<Vec<_>>();
        let batches = prepare_create_batches(&operations)?;
        let receipts = CreateReceiptPlan {
            expected,
            order: order.into_boxed_slice(),
            initial,
            numbers: HashSet::new(),
            node_ids: HashSet::new(),
            by_change: HashMap::new(),
        };
        Ok(Self { batches, receipts })
    }
}

/// Opaque proof that every planned create has one globally valid receipt.
#[derive(Debug)]
pub(super) struct CompleteCreateReceipts {
    order: Box<[GherritPrId]>,
    by_change: HashMap<GherritPrId, PullRequestIdentity>,
}

impl CompleteCreateReceipts {
    #[allow(dead_code)] // Consumed by the pending owned-base activation path.
    pub(super) fn len(&self) -> usize {
        self.by_change.len()
    }

    #[allow(dead_code)] // Consumed by the pending owned-base activation path.
    pub(super) fn identity(&self, id: &GherritPrId) -> Option<&PullRequestIdentity> {
        self.by_change.get(id)
    }

    pub(super) fn into_legacy_created(mut self) -> Vec<CreatedPullRequest> {
        self.order
            .into_vec()
            .into_iter()
            .map(|id| {
                let identity = self.by_change.remove(&id).expect("complete receipt has every ID");
                CreatedPullRequest {
                    head_branch: id.as_str().to_owned(),
                    number: u64::from(identity.number().get()),
                    node_id: identity.node_id().as_str().to_owned(),
                }
            })
            .collect()
    }
}

#[derive(Debug, PartialEq, Eq)]
pub(super) struct CreatedPullRequest {
    pub(super) head_branch: String,
    pub(super) number: u64,
    pub(super) node_id: String,
}

/// A nonempty minimal update to one exact preplanned pull request identity.
#[derive(Debug, PartialEq, Eq)]
pub(super) struct UpdatePullRequest {
    identity: PullRequestIdentity,
    title: Option<String>,
    body: Option<String>,
    base_branch: Option<String>,
    client_mutation_id: String,
}

impl UpdatePullRequest {
    pub(super) fn new(
        identity: PullRequestIdentity,
        title: Option<String>,
        body: Option<String>,
        base_branch: Option<String>,
    ) -> Result<Self> {
        if title.is_none() && body.is_none() && base_branch.is_none() {
            bail!("A pull request update must change at least one field");
        }
        let client_mutation_id = format!("gherrit:update:{}", identity.node_id().as_str());
        Ok(Self { identity, title, body, base_branch, client_mutation_id })
    }

    fn document(&self) -> String {
        let fields = std::iter::once(("pullRequestId", self.identity.node_id().as_str()))
            .chain(self.base_branch.as_deref().map(|value| ("baseRefName", value)))
            .chain(self.title.as_deref().map(|value| ("title", value)))
            .chain(self.body.as_deref().map(|value| ("body", value)))
            .chain(std::iter::once(("clientMutationId", self.client_mutation_id.as_str())))
            .map(|(name, value)| format!("{name}: {}", json!(value)))
            .collect::<Vec<_>>()
            .join(", ");
        format!(
            "updatePullRequest(input: {{ {fields} }}) {{ clientMutationId, pullRequest {{ number, id }} }}"
        )
    }
}

#[derive(Debug, PartialEq, Eq)]
struct ExpectedUpdateReceipt {
    alias: Box<str>,
    identity: PullRequestIdentity,
    client_mutation_id: Box<str>,
}

impl ExpectedUpdateReceipt {
    fn decode(&self, response: Value) -> Result<()> {
        #[derive(Deserialize)]
        #[serde(rename_all = "camelCase")]
        struct Response {
            client_mutation_id: String,
            pull_request: Option<UpdatedPullRequestResponse>,
        }

        #[derive(Deserialize)]
        struct UpdatedPullRequestResponse {
            number: u64,
            id: String,
        }

        if response.is_null() {
            bail!("GraphQL mutation response operation `{}` is null", self.alias);
        }
        let response: Response = serde_json::from_value(response)
            .map_err(|_| eyre!("Failed to decode updatePullRequest response"))?;
        if response.client_mutation_id != self.client_mutation_id.as_ref() {
            let returned = bounded_diagnostic_detail(&response.client_mutation_id);
            let expected = bounded_diagnostic_detail(&self.client_mutation_id);
            bail!(
                "updatePullRequest echoed clientMutationId '{}', expected '{}'",
                returned,
                expected
            );
        }
        let updated = response.pull_request.ok_or_else(|| {
            eyre!(
                "The batched GraphQL mutation failed to update PR #{}. The response pull request was null.",
                self.identity.number().get()
            )
        })?;
        let identity = PullRequestIdentity::new(updated.number, updated.id)?;
        if identity != self.identity {
            let returned_node = bounded_diagnostic_detail(identity.node_id().as_str());
            let expected_node = bounded_diagnostic_detail(self.identity.node_id().as_str());
            bail!(
                "updatePullRequest returned pull request identity #{} / '{}', expected #{} / '{}'",
                identity.number().get(),
                returned_node,
                self.identity.number().get(),
                expected_node
            );
        }
        Ok(())
    }
}

#[derive(Debug, PartialEq, Eq)]
pub(super) struct PreparedUpdateBatch {
    request: Value,
    serialized_bytes: usize,
    expected: Box<[ExpectedUpdateReceipt]>,
}

impl PreparedUpdateBatch {
    fn aliases(&self) -> Vec<Box<str>> {
        self.expected.iter().map(|expected| expected.alias.clone()).collect()
    }

    fn decode(self, response: Value) -> Result<()> {
        let aliases = self.aliases();
        let mut data = mutation_response_data(response, &aliases)?;
        for expected in self.expected.into_vec() {
            let response =
                data.remove(expected.alias.as_ref()).expect("the complete alias set was checked");
            expected.decode(response).wrap_err_with(|| {
                format!("Invalid acknowledgement for mutation `{}`", expected.alias)
            })?;
        }
        Ok(())
    }
}

fn update_batch(operations: &[UpdatePullRequest]) -> Result<PreparedUpdateBatch> {
    let mut fields = String::new();
    let mut expected = Vec::with_capacity(operations.len());
    for (index, operation) in operations.iter().enumerate() {
        let alias = format!("op{index}");
        fields.push_str(&format!("{alias}: {}", operation.document()));
        expected.push(ExpectedUpdateReceipt {
            alias: alias.into_boxed_str(),
            identity: operation.identity.clone(),
            client_mutation_id: operation.client_mutation_id.as_str().into(),
        });
    }
    let (request, serialized_bytes) = serialized_mutation_request(fields)?;
    Ok(PreparedUpdateBatch { request, serialized_bytes, expected: expected.into_boxed_slice() })
}

fn prepare_update_batches(operations: &[UpdatePullRequest]) -> Result<Box<[PreparedUpdateBatch]>> {
    let mut batches = Vec::new();
    let mut start = 0;
    while start < operations.len() {
        let max_end = operations.len().min(start + MAX_MUTATION_ALIASES);
        let mut accepted = None;
        for end in start + 1..=max_end {
            let batch = update_batch(&operations[start..end])?;
            if batch.serialized_bytes > MAX_MUTATION_REQUEST_BYTES {
                break;
            }
            accepted = Some((end, batch));
        }
        let Some((end, batch)) = accepted else {
            let bytes = update_batch(&operations[start..start + 1])?.serialized_bytes;
            bail!(
                "GraphQL update mutation at item {start} serializes to {bytes} bytes, which exceeds the {MAX_MUTATION_REQUEST_BYTES}-byte request limit. No mutation was sent."
            );
        };
        batches.push(batch);
        start = end;
    }
    Ok(batches.into_boxed_slice())
}

/// Every exact update request prepared before the first update is sent.
#[derive(Debug)]
pub(super) struct PreparedUpdates {
    batches: Box<[PreparedUpdateBatch]>,
}

impl PreparedUpdates {
    pub(super) fn new(operations: Vec<UpdatePullRequest>) -> Result<Self> {
        if operations.is_empty() {
            bail!("A prepared update action requires at least one operation");
        }
        let mut numbers = HashSet::with_capacity(operations.len());
        let mut node_ids = HashSet::with_capacity(operations.len());
        let mut client_mutation_ids = HashSet::with_capacity(operations.len());
        for (index, operation) in operations.iter().enumerate() {
            if !numbers.insert(operation.identity.number()) {
                bail!(
                    "GraphQL update mutation at item {index} repeats pull request number {}. No mutation was sent.",
                    operation.identity.number().get()
                );
            }
            if !node_ids.insert(operation.identity.node_id()) {
                bail!(
                    "GraphQL update mutation at item {index} repeats pull request node ID. No mutation was sent."
                );
            }
            if !client_mutation_ids.insert(operation.client_mutation_id.as_str()) {
                let client_mutation_id = bounded_diagnostic_detail(&operation.client_mutation_id);
                bail!(
                    "GraphQL update mutation at item {index} repeats clientMutationId '{}'. No mutation was sent.",
                    client_mutation_id
                );
            }
        }
        Ok(Self { batches: prepare_update_batches(&operations)? })
    }
}

#[cfg(test)]
mod observation_tests {
    use super::*;

    #[test]
    fn untrusted_diagnostic_detail_is_ascii_single_line_and_exactly_bounded() {
        assert_eq!(bounded_diagnostic_detail("line\n\t\u{202e}tail"), "line   tail");

        let detail = bounded_diagnostic_detail(&"x".repeat(1_000));
        assert_eq!(detail.len(), 256);
        assert!(detail.ends_with("..."));
    }

    fn id(value: &str) -> GherritPrId {
        GherritPrId::from_ref_component(value.as_bytes()).unwrap()
    }

    fn open_node(number: i64, head: &str) -> Value {
        json!({
            "number": number,
            "id": format!("PR_{number}"),
            "title": "Title",
            "body": "Body",
            "baseRefName": "main",
            "baseRefOid": "1".repeat(40),
            "headRefName": head,
            "headRefOid": "2".repeat(40),
            "state": "OPEN",
            "isCrossRepository": false,
            "autoMergeRequest": null,
            "isInMergeQueue": false,
        })
    }

    fn open_response(nodes: Vec<Value>, has_next_page: bool, end_cursor: Value) -> Value {
        json!({
            "data": {
                "repository": {
                    "id": "R_1",
                    "defaultBranchRef": {
                        "name": "main",
                        "target": { "oid": "3".repeat(40) },
                    },
                    "pullRequests": {
                        "nodes": nodes,
                        "pageInfo": {
                            "hasNextPage": has_next_page,
                            "endCursor": end_cursor,
                        },
                    },
                },
            },
        })
    }

    fn terminal_node(number: i64, head: &str, state: &str, is_cross_repository: bool) -> Value {
        json!({
            "number": number,
            "id": format!("PR_{number}"),
            "headRefName": head,
            "state": state,
            "isCrossRepository": is_cross_repository,
        })
    }

    fn terminal_query() -> TerminalPullRequests {
        TerminalPullRequests::new(
            "owner".to_string(),
            "repo".to_string(),
            vec![TerminalPullRequestQuery::new(id("G42"), None, 100).unwrap()],
        )
        .unwrap()
    }

    fn terminal_response(nodes: Vec<Value>, has_next_page: bool, end_cursor: Value) -> Value {
        json!({
            "op0": {
                "nodes": nodes,
                "pageInfo": { "hasNextPage": has_next_page, "endCursor": end_cursor },
            },
        })
    }

    #[test]
    fn open_scan_documents_use_one_connection_and_an_exact_cursor() {
        let first = FirstOpenPullRequests::new("owner".to_string(), "repo".to_string(), 100);
        let next = NextOpenPullRequests::new(
            "owner".to_string(),
            "repo".to_string(),
            "opaque cursor".to_string(),
            100,
        );
        assert!(first.document().contains("id, defaultBranchRef"));
        assert!(!next.document().contains("defaultBranchRef"));
        assert!(next.document().contains("after: \"opaque cursor\""));
        assert!(!next.document().contains("headRefName:"));
    }

    #[test]
    fn open_scan_decoder_requires_a_cursor_only_when_more_pages_exist() {
        let query = FirstOpenPullRequests::new("owner".to_string(), "repo".to_string(), 100);
        assert_eq!(
            query
                .decode(open_response(vec![open_node(42, "G42")], true, json!("cursor-1")))
                .unwrap()
                .next_cursor
                .as_deref(),
            Some("cursor-1")
        );
        assert!(
            query.decode(open_response(vec![open_node(42, "G42")], true, Value::Null)).is_err()
        );
        assert_eq!(
            query
                .decode(open_response(vec![open_node(42, "G42")], false, json!("ignored")))
                .unwrap()
                .next_cursor,
            None
        );
    }

    #[test]
    fn open_scan_decoder_requires_complete_first_page_repository_facts() {
        let query = FirstOpenPullRequests::new("owner".to_string(), "repo".to_string(), 100);
        let base = open_response(vec![], false, Value::Null);
        let mut cases = vec![json!({}), json!({ "data": { "repository": null } })];

        for field in ["id", "defaultBranchRef"] {
            let mut response = base.clone();
            response["data"]["repository"].as_object_mut().unwrap().remove(field);
            cases.push(response);
        }
        for (pointer, replacement) in [
            ("/data/repository/id", json!("")),
            ("/data/repository/defaultBranchRef/target", Value::Null),
            ("/data/repository/defaultBranchRef/target/oid", Value::Null),
            ("/data/repository/defaultBranchRef/target/oid", json!("invalid")),
        ] {
            let mut response = base.clone();
            *response.pointer_mut(pointer).unwrap() = replacement;
            cases.push(response);
        }

        for response in cases {
            assert!(query.decode(response).is_err());
        }
    }

    #[test]
    fn later_open_pages_neither_require_nor_accept_repository_facts() {
        let query = NextOpenPullRequests::new(
            "owner".to_string(),
            "repo".to_string(),
            "cursor-1".to_string(),
            100,
        );
        let response = open_response(vec![], false, Value::Null);
        assert!(query.decode(response.clone()).is_err());
        let mut null_facts = response.clone();
        null_facts["data"]["repository"]["id"] = Value::Null;
        null_facts["data"]["repository"]["defaultBranchRef"] = Value::Null;
        assert!(query.decode(null_facts).is_err());

        let mut response = response;
        let repository = response["data"]["repository"].as_object_mut().unwrap();
        repository.remove("id");
        repository.remove("defaultBranchRef");
        assert_eq!(query.decode(response).unwrap().next_cursor, None);
    }

    #[test]
    fn open_scan_decoder_requires_every_selected_node_field() {
        let query = FirstOpenPullRequests::new("owner".to_string(), "repo".to_string(), 100);

        for field in [
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
        ] {
            let mut node = open_node(42, "G42");
            node.as_object_mut().unwrap().remove(field);
            let error = query
                .decode(open_response(vec![node], false, Value::Null))
                .expect_err("a selected field may not be absent");
            assert_eq!(
                error.to_string(),
                "Failed to decode pull request query response",
                "field={field}"
            );
        }

        for field in ["title", "body"] {
            let mut node = open_node(42, "G42");
            node[field] = Value::Null;
            assert!(
                query.decode(open_response(vec![node], false, Value::Null)).is_err(),
                "field={field}"
            );
        }
    }

    #[test]
    fn open_scan_decoder_preserves_projection_and_policy_state() {
        let query = FirstOpenPullRequests::new("owner".to_string(), "repo".to_string(), 100);
        let mut node = open_node(i64::from(i32::MAX), "G42");
        node["autoMergeRequest"] = json!({ "enabledAt": "2026-01-01T00:00:00Z" });
        node["isInMergeQueue"] = json!(true);

        let pull_requests =
            query.decode(open_response(vec![node], false, Value::Null)).unwrap().pull_requests;
        assert_eq!(
            pull_requests,
            [OpenPullRequest {
                number: i32::MAX as u64,
                node_id: format!("PR_{}", i32::MAX),
                title: "Title".to_string(),
                body: "Body".to_string(),
                base_branch: "main".to_string(),
                head_branch: "G42".to_string(),
                base_oid: gix::ObjectId::from_hex("1".repeat(40).as_bytes()).unwrap(),
                head_oid: gix::ObjectId::from_hex("2".repeat(40).as_bytes()).unwrap(),
                is_cross_repository: false,
                has_auto_merge_request: true,
                is_in_merge_queue: true,
            }]
        );

        let mut nullable_enabled_at = open_node(42, "G42");
        nullable_enabled_at["autoMergeRequest"] = json!({ "enabledAt": null });
        assert!(
            query
                .decode(open_response(vec![nullable_enabled_at], false, Value::Null))
                .unwrap()
                .pull_requests[0]
                .has_auto_merge_request
        );

        for invalid_request in [json!({}), json!({ "enabledAt": {} })] {
            let mut node = open_node(42, "G42");
            node["autoMergeRequest"] = invalid_request;
            assert!(query.decode(open_response(vec![node], false, Value::Null)).is_err());
        }
    }

    #[test]
    fn terminal_batches_are_bounded_and_keep_each_cursor_independent() {
        let queries = (0..64)
            .map(|index| {
                TerminalPullRequestQuery::new(
                    id(&format!("G{index}")),
                    (index == 1).then(|| "cursor-1".to_string()),
                    100,
                )
                .unwrap()
            })
            .collect::<Vec<_>>();
        let query =
            TerminalPullRequests::new("owner".to_string(), "repo".to_string(), queries).unwrap();
        assert_eq!(query.document().matches("pullRequests(").count(), 64);
        assert!(TerminalPullRequests::new("o".to_string(), "r".to_string(), vec![]).is_err());
    }

    #[test]
    fn terminal_query_rejects_unusable_connection_arguments() {
        for (after, first) in [(None, 0), (Some(""), 100)] {
            assert!(
                TerminalPullRequestQuery::new(id("G42"), after.map(str::to_string), first,)
                    .is_err()
            );
        }
    }

    #[test]
    fn terminal_decoder_preserves_every_same_repository_candidate() {
        let query = terminal_query();
        let pages = query
            .decode(terminal_response(
                vec![
                    terminal_node(7, "G42", "CLOSED", false),
                    terminal_node(8, "G42", "MERGED", true),
                    terminal_node(9, "G42", "MERGED", false),
                ],
                true,
                json!("terminal-cursor"),
            ))
            .unwrap()
            .into_iter()
            .map(|evidence| evidence.into_parts().2)
            .collect::<Vec<_>>();

        assert_eq!(
            pages,
            [TerminalPullRequestPage {
                pull_requests: vec![
                    TerminalPullRequest {
                        number: 7,
                        node_id: "PR_7".to_string(),
                        state: TerminalPullRequestState::Closed,
                    },
                    TerminalPullRequest {
                        number: 9,
                        node_id: "PR_9".to_string(),
                        state: TerminalPullRequestState::Merged,
                    },
                ],
                next_cursor: Some("terminal-cursor".to_string()),
            }]
        );
    }

    #[test]
    fn terminal_decoder_rejects_incomplete_or_contradictory_evidence() {
        let query = terminal_query();
        for node in [
            terminal_node(0, "G42", "CLOSED", false),
            terminal_node(i64::from(i32::MAX) + 1, "G42", "CLOSED", false),
            terminal_node(42, "other", "CLOSED", false),
            terminal_node(42, "G42", "OPEN", false),
        ] {
            assert!(
                query.clone().decode(terminal_response(vec![node], false, Value::Null)).is_err()
            );
        }

        let mut missing_id = terminal_node(42, "G42", "CLOSED", false);
        missing_id.as_object_mut().unwrap().remove("id");
        assert!(
            query.clone().decode(terminal_response(vec![missing_id], false, Value::Null)).is_err()
        );
        assert!(query.decode(terminal_response(vec![], true, Value::Null)).is_err());
    }
}

#[cfg(test)]
mod mutation_tests {
    use super::*;

    fn id(value: &str) -> GherritPrId {
        GherritPrId::from_ref_component(value.as_bytes()).unwrap()
    }

    fn initial(pull_requests: Vec<OpenPullRequest>) -> InitialPullRequestIdentities {
        InitialPullRequestIdentities::from_open(&pull_requests).unwrap()
    }

    fn missing_ids(ids: &[&str]) -> HashSet<GherritPrId> {
        ids.iter().map(|value| id(value)).collect()
    }

    fn creates(ids: &[&str], initial: InitialPullRequestIdentities) -> PreparedCreates {
        let operations = ids
            .iter()
            .map(|value| {
                CreatePullRequest::new(
                    id(value),
                    "REPO_NODE_ID".to_owned(),
                    "main".to_owned(),
                    format!("Title {value}"),
                    String::new(),
                )
            })
            .collect();
        PreparedCreates::new(initial, missing_ids(ids), operations).unwrap()
    }

    fn created(client_id: &str, head: &str, number: u64, node_id: &str) -> Value {
        json!({
            "clientMutationId": client_id,
            "pullRequest": {
                "number": number,
                "id": node_id,
                "headRefName": head,
            },
        })
    }

    fn receipt_plan(ids: &[&str], initial: InitialPullRequestIdentities) -> CreateReceiptPlan {
        let order = ids.iter().map(|value| id(value)).collect::<Vec<_>>().into_boxed_slice();
        CreateReceiptPlan {
            expected: order.iter().cloned().collect(),
            order,
            initial,
            numbers: HashSet::new(),
            node_ids: HashSet::new(),
            by_change: HashMap::new(),
        }
    }

    fn raw_create(body_len: usize) -> CreatePullRequest {
        let id = id("Gsize");
        CreatePullRequest {
            client_mutation_id: format!("gherrit:create:{}", id.as_str()),
            id,
            repository_id: "REPO_NODE_ID".to_owned(),
            base_branch: "main".to_owned(),
            title: "Title".to_owned(),
            body: "x".repeat(body_len),
        }
    }

    fn raw_update(body_len: usize) -> UpdatePullRequest {
        UpdatePullRequest::new(
            PullRequestIdentity::new(1, "PR_SIZE".to_owned()).unwrap(),
            None,
            Some("x".repeat(body_len)),
            None,
        )
        .unwrap()
    }

    #[test]
    fn create_preparation_requires_the_exact_missing_id_set() {
        let operation = CreatePullRequest::new(
            id("A"),
            "REPO_NODE_ID".to_owned(),
            "main".to_owned(),
            "A".to_owned(),
            String::new(),
        );
        assert!(
            PreparedCreates::new(initial(Vec::new()), missing_ids(&["A", "B"]), vec![operation])
                .is_err()
        );

        let operations = ["A", "B"]
            .map(|value| {
                let value = id(value);
                CreatePullRequest::new(
                    value,
                    "REPO_NODE_ID".to_owned(),
                    "main".to_owned(),
                    "title".to_owned(),
                    String::new(),
                )
            })
            .into_iter()
            .take(1)
            .collect();
        assert!(
            PreparedCreates::new(initial(Vec::new()), missing_ids(&["A", "B"]), operations)
                .is_err()
        );
    }

    #[test]
    fn create_batches_use_the_exact_serialized_one_mibibyte_boundary() {
        let fixed_bytes = create_batch(&[raw_create(0)]).unwrap().serialized_bytes;
        let exact_body_len = MAX_MUTATION_REQUEST_BYTES - fixed_bytes;
        let exact = raw_create(exact_body_len);
        assert_eq!(create_batch(&[exact]).unwrap().serialized_bytes, MAX_MUTATION_REQUEST_BYTES);
        assert!(prepare_create_batches(&[raw_create(exact_body_len)]).is_ok());
        assert!(prepare_create_batches(&[raw_create(exact_body_len + 1)]).is_err());

        let mut operations = (0..MAX_MUTATION_ALIASES)
            .map(|index| CreatePullRequest {
                id: id(&format!("G{index}")),
                repository_id: "REPO_NODE_ID".to_owned(),
                base_branch: "main".to_owned(),
                title: "small".to_owned(),
                body: String::new(),
                client_mutation_id: format!("create-{index}"),
            })
            .collect::<Vec<_>>();
        operations.push(raw_create(exact_body_len + 1));
        assert!(prepare_create_batches(&operations).is_err());
    }

    #[test]
    fn update_batches_use_the_exact_serialized_one_mibibyte_boundary() {
        let fixed_bytes = update_batch(&[raw_update(0)]).unwrap().serialized_bytes;
        let exact_body_len = MAX_MUTATION_REQUEST_BYTES - fixed_bytes;
        assert_eq!(
            update_batch(&[raw_update(exact_body_len)]).unwrap().serialized_bytes,
            MAX_MUTATION_REQUEST_BYTES
        );
        assert!(prepare_update_batches(&[raw_update(exact_body_len)]).is_ok());
        assert!(prepare_update_batches(&[raw_update(exact_body_len + 1)]).is_err());

        let mut operations = (0..MAX_MUTATION_ALIASES)
            .map(|index| {
                UpdatePullRequest::new(
                    PullRequestIdentity::new(
                        u64::try_from(index + 1).unwrap(),
                        format!("PR_{index}"),
                    )
                    .unwrap(),
                    Some("small".to_owned()),
                    None,
                    None,
                )
                .unwrap()
            })
            .collect::<Vec<_>>();
        operations.push(raw_update(exact_body_len + 1));
        assert!(prepare_update_batches(&operations).is_err());
    }

    #[test]
    fn create_decoder_requires_exact_alias_client_id_head_and_typed_identity() {
        let cases = [
            json!({ "data": {} }),
            json!({ "data": { "op0": created("wrong", "A", 1, "PR_1") } }),
            json!({ "data": { "op0": created("gherrit:create:A", "B", 1, "PR_1") } }),
            json!({ "data": { "op0": created("gherrit:create:A", "A", 0, "PR_1") } }),
            json!({ "data": { "op0": created("gherrit:create:A", "A", 1, "") } }),
            json!({
                "data": {
                    "op0": created("gherrit:create:A", "A", 1, "PR_1"),
                    "extra": created("extra", "A", 2, "PR_2"),
                }
            }),
        ];
        for response in cases {
            let prepared = creates(&["A"], initial(Vec::new()));
            let batch = prepared.batches.into_vec().pop().unwrap();
            assert!(batch.decode(response).is_err());
        }
    }

    #[test]
    fn response_derived_mutation_diagnostics_are_terminal_safe_and_bounded() {
        let returned = format!("{}\nnot-disclosed", "x".repeat(1_000));
        let prepared = creates(&["A"], initial(Vec::new()));
        let batch = prepared.batches.into_vec().pop().unwrap();
        let error = batch
            .decode(json!({
                "data": {
                    "op0": created(&returned, "A", 1, "PR_1"),
                }
            }))
            .unwrap_err()
            .to_string();

        assert!(!error.contains('\n'));
        assert!(!error.contains("not-disclosed"));
        assert!(error.len() < 400);
    }

    #[test]
    fn create_receipts_are_globally_disjoint_and_complete() {
        let occupied = vec![OpenPullRequest {
            number: 7,
            node_id: "OPEN_NODE".to_owned(),
            title: String::new(),
            body: String::new(),
            base_branch: "main".to_owned(),
            head_branch: "fork".to_owned(),
            base_oid: gix::ObjectId::null(gix::hash::Kind::Sha1),
            head_oid: gix::ObjectId::null(gix::hash::Kind::Sha1),
            is_cross_repository: true,
            has_auto_merge_request: false,
            is_in_merge_queue: false,
        }];
        let mut number_collision = receipt_plan(&["A"], initial(occupied.clone()));
        assert!(
            number_collision
                .record(vec![(id("A"), PullRequestIdentity::new(7, "new".to_owned()).unwrap())])
                .is_err()
        );
        let mut node_collision = receipt_plan(&["A"], initial(occupied));
        assert!(
            node_collision
                .record(vec![(
                    id("A"),
                    PullRequestIdentity::new(8, "OPEN_NODE".to_owned()).unwrap(),
                )])
                .is_err()
        );

        for second in [
            PullRequestIdentity::new(1, "PR_2".to_owned()).unwrap(),
            PullRequestIdentity::new(2, "PR_1".to_owned()).unwrap(),
        ] {
            let mut plan = receipt_plan(&["A", "B"], initial(Vec::new()));
            plan.record(vec![(id("A"), PullRequestIdentity::new(1, "PR_1".to_owned()).unwrap())])
                .unwrap();
            assert!(plan.record(vec![(id("B"), second)]).is_err());
        }

        let mut incomplete = receipt_plan(&["A", "B"], initial(Vec::new()));
        incomplete
            .record(vec![(id("A"), PullRequestIdentity::new(1, "PR_1".to_owned()).unwrap())])
            .unwrap();
        assert!(incomplete.finish().is_err());

        let mut complete = receipt_plan(&["A", "B"], initial(Vec::new()));
        complete
            .record(vec![
                (id("A"), PullRequestIdentity::new(1, "PR_1".to_owned()).unwrap()),
                (id("B"), PullRequestIdentity::new(2, "PR_2".to_owned()).unwrap()),
            ])
            .unwrap();
        let complete = complete.finish().unwrap();
        assert_eq!(complete.len(), 2);
        assert_eq!(complete.identity(&id("B")).unwrap().number().get(), 2);
    }

    #[test]
    fn update_acknowledgement_requires_the_exact_number_node_pair() {
        assert!(
            UpdatePullRequest::new(
                PullRequestIdentity::new(1, "PR_1".to_owned()).unwrap(),
                None,
                None,
                None,
            )
            .is_err()
        );

        for (client_id, number, node_id) in [
            ("wrong", 1, "PR_1"),
            ("gherrit:update:PR_1", 2, "PR_1"),
            ("gherrit:update:PR_1", 1, "PR_2"),
        ] {
            let update = UpdatePullRequest::new(
                PullRequestIdentity::new(1, "PR_1".to_owned()).unwrap(),
                Some("Title".to_owned()),
                None,
                None,
            )
            .unwrap();
            let batch =
                PreparedUpdates::new(vec![update]).unwrap().batches.into_vec().pop().unwrap();
            assert!(
                batch
                    .decode(json!({
                        "data": {
                            "op0": {
                                "clientMutationId": client_id,
                                "pullRequest": { "number": number, "id": node_id },
                            }
                        }
                    }))
                    .is_err()
            );
        }
    }

    #[test]
    fn update_preparation_rejects_each_duplicate_identity_namespace() {
        let update = |number, node_id: &str| {
            UpdatePullRequest::new(
                PullRequestIdentity::new(number, node_id.to_owned()).unwrap(),
                Some("Title".to_owned()),
                None,
                None,
            )
            .unwrap()
        };

        assert!(PreparedUpdates::new(vec![update(1, "PR_1"), update(1, "PR_2")]).is_err());
        assert!(PreparedUpdates::new(vec![update(1, "PR_1"), update(2, "PR_1")]).is_err());
    }

    #[test]
    fn concrete_prepared_mutation_actions_must_be_nonempty() {
        assert!(PreparedCreates::new(initial(Vec::new()), HashSet::new(), Vec::new()).is_err());
        assert!(PreparedUpdates::new(Vec::new()).is_err());
    }
}

use std::{
    collections::{BTreeMap, HashSet},
    ops::Range,
};

use color_eyre::eyre::{Context as _, Result, bail, eyre};
use serde::Deserialize;
use serde_json::{Value, json};

use super::destination::DefaultBranch;

pub(super) const MAX_MUTATION_ALIASES: usize = 64;
// A 131,072-byte pull-request body made entirely from U+0001 expands to
// 917,504 bytes after GraphQL-string escaping and then outer-JSON escaping.
// One MiB accommodates that worst case plus the mutation's other supported
// fields while retaining a deterministic preflight request limit.
const MAX_MUTATION_REQUEST_BYTES: usize = 1024 * 1024;

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

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct CreatedPullRequest {
    pub(super) head_branch: String,
    pub(super) number: u64,
    pub(super) node_id: String,
}

/// One GraphQL mutation field whose response is an acknowledgement receipt.
///
/// Mutation operations deliberately use an API distinct from the read-only
/// request types. This keeps adaptive query retries unavailable to writes at
/// the type level.
pub(super) trait MutationOperation {
    type Output;

    fn client_mutation_id(&self) -> &str;
    fn document(&self) -> String;
    fn decode_receipt(&self, response: Value) -> Result<Self::Output>;

    /// Validates identities shared by receipts from every transmitted batch.
    fn validate_receipts(_receipts: &[Self::Output]) -> Result<()> {
        Ok(())
    }
}

/// One mutation request prepared before any mutation is transmitted.
#[derive(Debug, PartialEq, Eq)]
pub(super) struct PreparedMutationBatch {
    pub(super) operation_range: Range<usize>,
    pub(super) request: Value,
    pub(super) serialized_bytes: usize,
}

fn mutation_batch_document<O: MutationOperation>(operations: &[O]) -> String {
    let body = operations
        .iter()
        .enumerate()
        .map(|(index, operation)| format!("op{index}: {}", operation.document()))
        .collect::<String>();
    format!("mutation {{ {body} }}")
}

fn mutation_request<O: MutationOperation>(operations: &[O]) -> Result<(Value, usize)> {
    let request = json!({ "query": mutation_batch_document(operations) });
    let serialized_bytes = serde_json::to_vec(&request)
        .wrap_err("Failed to serialize a GraphQL mutation request")?
        .len();
    Ok((request, serialized_bytes))
}

/// Sizes every mutation batch before the first batch is transmitted.
///
/// This is intentionally not adaptive. Discovering that a later operation is
/// too large must not happen after an earlier mutation batch may have run.
pub(super) fn prepare_mutation_batches<O: MutationOperation>(
    operations: &[O],
) -> Result<Vec<PreparedMutationBatch>> {
    let mut client_mutation_ids = HashSet::with_capacity(operations.len());
    for (index, operation) in operations.iter().enumerate() {
        if !client_mutation_ids.insert(operation.client_mutation_id()) {
            bail!(
                "GraphQL mutation at item {index} repeats clientMutationId '{}'. No mutation was sent.",
                operation.client_mutation_id()
            );
        }
    }

    let mut batches = Vec::new();
    let mut start = 0;

    while start < operations.len() {
        let max_end = operations.len().min(start + MAX_MUTATION_ALIASES);
        let mut accepted = None;

        for end in start + 1..=max_end {
            let (request, serialized_bytes) = mutation_request(&operations[start..end])?;
            if serialized_bytes > MAX_MUTATION_REQUEST_BYTES {
                break;
            }
            accepted = Some(PreparedMutationBatch {
                operation_range: start..end,
                request,
                serialized_bytes,
            });
        }

        let Some(batch) = accepted else {
            let (_, serialized_bytes) = mutation_request(&operations[start..start + 1])?;
            bail!(
                "GraphQL mutation at item {start} serializes to {serialized_bytes} bytes, which exceeds the {MAX_MUTATION_REQUEST_BYTES}-byte request limit. No mutation was sent."
            );
        };
        start = batch.operation_range.end;
        batches.push(batch);
    }

    Ok(batches)
}

/// Validates every receipt in one transmitted mutation batch.
pub(super) fn decode_mutation_batch_response<O: MutationOperation>(
    operations: &[O],
    response: Value,
) -> Result<Vec<O::Output>> {
    if let Some(errors) = response.get("errors")
        && !matches!(errors.as_array(), Some(errors) if errors.is_empty())
    {
        bail!("GraphQL mutation response contains errors: {errors:?}");
    }

    let data = response
        .get("data")
        .ok_or_else(|| eyre!("Missing JSON field in GraphQL mutation response: `data`"))?
        .as_object()
        .ok_or_else(|| eyre!("GraphQL mutation response field `data` is not an object"))?;

    let expected_aliases =
        (0..operations.len()).map(|index| format!("op{index}")).collect::<HashSet<_>>();
    for index in 0..operations.len() {
        let alias = format!("op{index}");
        if !data.contains_key(&alias) {
            bail!("GraphQL mutation response is missing operation `{alias}`");
        }
    }
    if let Some(alias) = data.keys().find(|alias| !expected_aliases.contains(*alias)) {
        bail!("GraphQL mutation response contains unexpected operation `{alias}`");
    }

    let receipts = operations
        .iter()
        .enumerate()
        .map(|(index, operation)| {
            let alias = format!("op{index}");
            if data[&alias].is_null() {
                bail!("GraphQL mutation response operation `{alias}` is null");
            }
            operation
                .decode_receipt(data[&alias].clone())
                .wrap_err_with(|| format!("Invalid acknowledgement for mutation `{alias}`"))
        })
        .collect::<Result<Vec<_>>>()?;
    O::validate_receipts(&receipts)?;
    Ok(receipts)
}

/// Repository facts which must agree with the exact Git push destination.
#[derive(Debug, Eq, PartialEq)]
pub(super) struct Repository {
    pub(super) node_id: String,
    pub(super) default_branch: DefaultBranch,
}

/// The first page of the repository-wide open-pull-request connection.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct FirstOpenPullRequests {
    owner: String,
    repository: String,
    first: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct NextOpenPullRequests {
    owner: String,
    repository: String,
    after: String,
    first: usize,
}

#[derive(Debug, Eq, PartialEq)]
pub(super) struct FirstOpenPullRequestsPage {
    pub(super) repository: Repository,
    pub(super) pull_requests: Vec<OpenPullRequest>,
    pub(super) next_cursor: Option<String>,
}

#[derive(Debug, Eq, PartialEq)]
pub(super) struct NextOpenPullRequestsPage {
    pub(super) pull_requests: Vec<OpenPullRequest>,
    pub(super) next_cursor: Option<String>,
}

impl FirstOpenPullRequests {
    pub(super) fn new(owner: String, repository: String, first: usize) -> Self {
        assert!(first > 0, "an open pull request page size must be positive");
        Self { owner, repository, first }
    }

    pub(super) fn document(&self) -> String {
        open_pull_requests_document(
            &self.owner,
            &self.repository,
            self.first,
            OpenPullRequestPage::First,
        )
    }

    pub(super) fn decode(&self, response: Value) -> Result<FirstOpenPullRequestsPage> {
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
            .wrap_err("GitHub reported an invalid default branch object ID")?;
        let repository = Repository {
            node_id,
            default_branch: DefaultBranch::new(default_branch.name, tip)
                .wrap_err("GitHub reported an invalid default branch")?,
        };

        Ok(FirstOpenPullRequestsPage { repository, pull_requests, next_cursor })
    }
}

impl NextOpenPullRequests {
    pub(super) fn new(owner: String, repository: String, after: String, first: usize) -> Self {
        assert!(!after.is_empty(), "an open pull request cursor must be nonempty");
        assert!(first > 0, "an open pull request page size must be positive");
        Self { owner, repository, after, first }
    }

    pub(super) fn document(&self) -> String {
        open_pull_requests_document(
            &self.owner,
            &self.repository,
            self.first,
            OpenPullRequestPage::Next { after: &self.after },
        )
    }

    pub(super) fn decode(&self, response: Value) -> Result<NextOpenPullRequestsPage> {
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
        enabled_at: String,
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
        .wrap_err("Failed to decode pull request query response")?;
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
                    .wrap_err_with(|| format!("GitHub reported an invalid {field}"))?;
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
pub(super) struct TerminalPullRequestQuery {
    head_branch: String,
    after: Option<String>,
    first: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct TerminalPullRequestPage {
    pub(super) pull_requests: Vec<TerminalPullRequest>,
    pub(super) next_cursor: Option<String>,
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
pub(super) struct TerminalPullRequests {
    owner: String,
    repository: String,
    queries: Vec<TerminalPullRequestQuery>,
}

impl TerminalPullRequests {
    pub(super) const MAX_ALIASES: usize = 64;

    pub(super) fn new(
        owner: String,
        repository: String,
        queries: Vec<TerminalPullRequestQuery>,
    ) -> Result<Self> {
        if queries.is_empty() || queries.len() > Self::MAX_ALIASES {
            bail!("A terminal pull request query requires between one and 64 aliases");
        }
        Ok(Self { owner, repository, queries })
    }

    pub(super) fn document(&self) -> String {
        let fields = self.queries.iter().enumerate().map(|(index, query)| {
            let after = query.after.as_ref()
                .map(|cursor| format!(", after: {}", json!(cursor)))
                .unwrap_or_default();
            format!(
                "op{index}: pullRequests(headRefName: {}, first: {}{after}, states: [CLOSED, MERGED]) {{ nodes {{ number, id, headRefName, state, isCrossRepository }} pageInfo {{ hasNextPage, endCursor }} }}",
                json!(query.head_branch),
                query.first,
            )
        }).collect::<String>();
        format!(
            "query {{ repository(owner: {}, name: {}) {{ {fields} }} }}",
            json!(self.owner),
            json!(self.repository),
        )
    }

    pub(super) fn decode(&self, response: Value) -> Result<Vec<TerminalPullRequestPage>> {
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
            .wrap_err("Failed to decode terminal pull request query response")?;
        let expected =
            (0..self.queries.len()).map(|index| format!("op{index}")).collect::<HashSet<_>>();
        if connections.len() != expected.len() {
            bail!("GitHub terminal pull request response has an unexpected alias set");
        }
        if let Some(alias) = connections.keys().find(|alias| !expected.contains(*alias)) {
            bail!("GitHub terminal pull request response contains unexpected operation `{alias}`");
        }

        self.queries
            .iter()
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
                        if node.head_ref_name != query.head_branch {
                            bail!(
                                "GitHub terminal pull request for '{}' returned head branch '{}'",
                                query.head_branch,
                                node.head_ref_name
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
                Ok(TerminalPullRequestPage { pull_requests, next_cursor })
            })
            .collect()
    }
}

impl TerminalPullRequestQuery {
    pub(super) fn new(head_branch: String, after: Option<String>, first: usize) -> Result<Self> {
        if head_branch.is_empty() {
            bail!("A terminal pull request query requires a nonempty head branch");
        }
        if first == 0 {
            bail!("A terminal pull request query requires a positive page size");
        }
        if after.as_deref() == Some("") {
            bail!("A terminal pull request query requires a nonempty pagination cursor");
        }
        Ok(Self { head_branch, after, first })
    }
}

/// A request to create a PR for one commit in the stack.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct CreatePullRequest {
    repository_id: String,
    base_branch: String,
    head_branch: String,
    title: String,
    body: String,
    client_mutation_id: String,
}

impl CreatePullRequest {
    pub(super) fn new(
        repository_id: String,
        base_branch: String,
        head_branch: String,
        title: String,
        body: String,
    ) -> Self {
        let client_mutation_id = format!("gherrit:create:{head_branch}");
        Self { repository_id, base_branch, head_branch, title, body, client_mutation_id }
    }
}

impl MutationOperation for CreatePullRequest {
    type Output = CreatedPullRequest;

    fn client_mutation_id(&self) -> &str {
        &self.client_mutation_id
    }

    fn document(&self) -> String {
        let fields = [
            ("repositoryId", &self.repository_id),
            ("baseRefName", &self.base_branch),
            ("headRefName", &self.head_branch),
            ("title", &self.title),
            ("body", &self.body),
            ("clientMutationId", &self.client_mutation_id),
        ]
        .map(|(name, value)| format!("{name}: {}", json!(value)))
        .join(", ");
        format!(
            "createPullRequest(input: {{ {fields} }}) {{ clientMutationId, pullRequest {{ number, id, headRefName }} }}"
        )
    }

    fn decode_receipt(&self, response: Value) -> Result<Self::Output> {
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

        let response: Response = serde_json::from_value(response)
            .wrap_err("Failed to decode createPullRequest response")?;
        if response.client_mutation_id != self.client_mutation_id {
            bail!(
                "createPullRequest echoed clientMutationId '{}', expected '{}'",
                response.client_mutation_id,
                self.client_mutation_id
            );
        }
        let created = response.pull_request.ok_or_else(|| {
            eyre!(
                "The batched GraphQL mutation failed to create PR for head branch '{}'. The response pull request was null.",
                self.head_branch
            )
        })?;
        if created.head_ref_name != self.head_branch {
            bail!(
                "createPullRequest returned head branch '{}', expected '{}'",
                created.head_ref_name,
                self.head_branch
            );
        }
        if !(1..=i32::MAX as u64).contains(&created.number) {
            bail!(
                "createPullRequest returned invalid pull request number {}; \
                 GitHub pull request numbers must fit GraphQL Int",
                created.number
            );
        }
        if created.id.is_empty() {
            bail!("createPullRequest returned an empty pull request node ID");
        }

        Ok(CreatedPullRequest {
            head_branch: self.head_branch.clone(),
            number: created.number,
            node_id: created.id,
        })
    }

    fn validate_receipts(receipts: &[Self::Output]) -> Result<()> {
        let mut numbers = HashSet::with_capacity(receipts.len());
        let mut node_ids = HashSet::with_capacity(receipts.len());

        for (index, receipt) in receipts.iter().enumerate() {
            if !numbers.insert(receipt.number) {
                bail!(
                    "createPullRequest receipt at item {index} repeats pull request number {}",
                    receipt.number
                );
            }
            if !node_ids.insert(receipt.node_id.as_str()) {
                bail!(
                    "createPullRequest receipt at item {index} repeats pull request node ID '{}'",
                    receipt.node_id
                );
            }
        }

        Ok(())
    }
}

/// A minimal update to an existing PR.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct UpdatePullRequest {
    node_id: String,
    title: Option<String>,
    body: Option<String>,
    base_branch: Option<String>,
    client_mutation_id: String,
}

impl UpdatePullRequest {
    pub(super) fn new(
        node_id: String,
        title: Option<String>,
        body: Option<String>,
        base_branch: Option<String>,
    ) -> Result<Self> {
        if node_id.is_empty() {
            bail!("A pull request update requires a nonempty node ID");
        }
        if title.is_none() && body.is_none() && base_branch.is_none() {
            bail!("A pull request update must change at least one field");
        }
        let client_mutation_id = format!("gherrit:update:{node_id}");
        Ok(Self { node_id, title, body, base_branch, client_mutation_id })
    }
}

impl MutationOperation for UpdatePullRequest {
    type Output = ();

    fn client_mutation_id(&self) -> &str {
        &self.client_mutation_id
    }

    fn document(&self) -> String {
        let fields = std::iter::once(("pullRequestId", &self.node_id))
            .chain(self.base_branch.as_ref().map(|value| ("baseRefName", value)))
            .chain(self.title.as_ref().map(|value| ("title", value)))
            .chain(self.body.as_ref().map(|value| ("body", value)))
            .chain(std::iter::once(("clientMutationId", &self.client_mutation_id)))
            .map(|(name, value)| format!("{name}: {}", json!(value)))
            .collect::<Vec<_>>()
            .join(", ");
        format!(
            "updatePullRequest(input: {{ {fields} }}) {{ clientMutationId, pullRequest {{ id }} }}"
        )
    }

    fn decode_receipt(&self, response: Value) -> Result<Self::Output> {
        #[derive(Deserialize)]
        #[serde(rename_all = "camelCase")]
        struct Response {
            client_mutation_id: String,
            pull_request: Option<UpdatedPullRequestResponse>,
        }

        #[derive(Deserialize)]
        struct UpdatedPullRequestResponse {
            id: String,
        }

        let response: Response = serde_json::from_value(response)
            .wrap_err("Failed to decode updatePullRequest response")?;
        if response.client_mutation_id != self.client_mutation_id {
            bail!(
                "updatePullRequest echoed clientMutationId '{}', expected '{}'",
                response.client_mutation_id,
                self.client_mutation_id
            );
        }
        let updated = response.pull_request.ok_or_else(|| {
            eyre!(
                "The batched GraphQL mutation failed to update PR with node ID '{}'. The response pull request was null.",
                self.node_id
            )
        })?;
        if updated.id != self.node_id {
            bail!(
                "updatePullRequest returned node ID '{}', expected '{}'",
                updated.id,
                self.node_id
            );
        }

        Ok(())
    }
}

#[cfg(test)]
mod observation_tests {
    use super::*;

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
            vec![TerminalPullRequestQuery::new("G42".to_string(), None, 100).unwrap()],
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
            assert!(
                format!("{error:?}").contains("missing field"),
                "field={field}, error={error:?}"
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

        for enabled_at in [Value::Null, json!({})] {
            let mut node = open_node(42, "G42");
            node["autoMergeRequest"] = json!({ "enabledAt": enabled_at });
            assert!(query.decode(open_response(vec![node], false, Value::Null)).is_err());
        }
    }

    #[test]
    fn terminal_batches_are_bounded_and_keep_each_cursor_independent() {
        let queries = (0..64)
            .map(|index| {
                TerminalPullRequestQuery::new(
                    format!("G{index}"),
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
        for (head_branch, after, first) in
            [("", None, 100), ("G42", None, 0), ("G42", Some(""), 100)]
        {
            assert!(
                TerminalPullRequestQuery::new(
                    head_branch.to_string(),
                    after.map(str::to_string),
                    first,
                )
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
            .unwrap();

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
            assert!(query.decode(terminal_response(vec![node], false, Value::Null)).is_err());
        }

        let mut missing_id = terminal_node(42, "G42", "CLOSED", false);
        missing_id.as_object_mut().unwrap().remove("id");
        assert!(query.decode(terminal_response(vec![missing_id], false, Value::Null)).is_err());
        assert!(query.decode(terminal_response(vec![], true, Value::Null)).is_err());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pre_push::body::MAX_BODY_SIZE_BYTES;

    #[test]
    fn create_document_escapes_every_input() {
        let create = CreatePullRequest::new(
            "repo\"id".to_string(),
            "base\nbranch".to_string(),
            "head\\branch".to_string(),
            "A \"title\"".to_string(),
            "line one\nline two".to_string(),
        );

        assert_eq!(
            create.document(),
            r#"createPullRequest(input: { repositoryId: "repo\"id", baseRefName: "base\nbranch", headRefName: "head\\branch", title: "A \"title\"", body: "line one\nline two", clientMutationId: "gherrit:create:head\\branch" }) { clientMutationId, pullRequest { number, id, headRefName } }"#
        );
    }

    #[test]
    fn update_document_omits_unchanged_fields() {
        let update = UpdatePullRequest::new(
            "PR_node".to_string(),
            Some("new \"title\"".to_string()),
            None,
            None,
        )
        .unwrap();

        assert_eq!(
            update.document(),
            r#"updatePullRequest(input: { pullRequestId: "PR_node", title: "new \"title\"", clientMutationId: "gherrit:update:PR_node" }) { clientMutationId, pullRequest { id } }"#
        );
    }

    #[test]
    fn batch_document_aliases_each_operation_exactly() {
        let operations = [
            UpdatePullRequest::new("PR_1".to_string(), Some("one".to_string()), None, None)
                .unwrap(),
            UpdatePullRequest::new("PR_2".to_string(), Some("two".to_string()), None, None)
                .unwrap(),
        ];

        assert_eq!(
            mutation_batch_document(&operations),
            r#"mutation { op0: updatePullRequest(input: { pullRequestId: "PR_1", title: "one", clientMutationId: "gherrit:update:PR_1" }) { clientMutationId, pullRequest { id } }op1: updatePullRequest(input: { pullRequestId: "PR_2", title: "two", clientMutationId: "gherrit:update:PR_2" }) { clientMutationId, pullRequest { id } } }"#
        );
    }

    #[derive(Debug)]
    struct RawMutation {
        client_mutation_id: String,
        document: String,
    }

    impl RawMutation {
        fn new(client_mutation_id: &str, document_bytes: usize) -> Self {
            Self {
                client_mutation_id: client_mutation_id.to_string(),
                document: "x".repeat(document_bytes),
            }
        }
    }

    impl MutationOperation for RawMutation {
        type Output = ();

        fn client_mutation_id(&self) -> &str {
            &self.client_mutation_id
        }

        fn document(&self) -> String {
            self.document.clone()
        }

        fn decode_receipt(&self, _response: Value) -> Result<Self::Output> {
            Ok(())
        }
    }

    fn update(node_id: &str) -> UpdatePullRequest {
        UpdatePullRequest::new(node_id.to_string(), Some("updated".to_string()), None, None)
            .unwrap()
    }

    fn update_receipt(node_id: &str) -> Value {
        json!({
            "clientMutationId": format!("gherrit:update:{node_id}"),
            "pullRequest": { "id": node_id },
        })
    }

    fn create_for(head_branch: &str) -> CreatePullRequest {
        CreatePullRequest::new(
            "R_1".to_string(),
            "main".to_string(),
            head_branch.to_string(),
            "Title".to_string(),
            "Body".to_string(),
        )
    }

    fn create() -> CreatePullRequest {
        create_for("G123")
    }

    fn create_receipt_for(head_branch: &str, number: u64, node_id: &str) -> Value {
        json!({
            "clientMutationId": format!("gherrit:create:{head_branch}"),
            "pullRequest": {
                "number": number,
                "id": node_id,
                "headRefName": head_branch,
            }
        })
    }

    fn create_receipt() -> Value {
        create_receipt_for("G123", 42, "PR_42")
    }

    #[test]
    fn mutation_batch_planning_covers_alias_count_boundaries() {
        for (count, expected_ranges) in [
            (0, vec![]),
            (1, std::iter::once(0..1).collect()),
            (MAX_MUTATION_ALIASES, std::iter::once(0..MAX_MUTATION_ALIASES).collect()),
            (
                MAX_MUTATION_ALIASES + 1,
                vec![0..MAX_MUTATION_ALIASES, MAX_MUTATION_ALIASES..MAX_MUTATION_ALIASES + 1],
            ),
        ] {
            let operations =
                (0..count).map(|index| update(&format!("PR_{index}"))).collect::<Vec<_>>();
            let batches = prepare_mutation_batches(&operations).unwrap();

            assert_eq!(
                batches.iter().map(|batch| batch.operation_range.clone()).collect::<Vec<_>>(),
                expected_ranges,
                "count={count}"
            );
            assert!(
                batches.iter().all(|batch| batch.serialized_bytes <= MAX_MUTATION_REQUEST_BYTES)
            );
        }
    }

    #[test]
    fn mutation_batch_planning_uses_the_exact_serialized_request_size() {
        let empty = RawMutation::new("exact", 0);
        let empty_bytes = mutation_request(&[empty]).unwrap().1;
        let exact = RawMutation::new("exact", MAX_MUTATION_REQUEST_BYTES - empty_bytes);

        let batches = prepare_mutation_batches(&[exact]).unwrap();
        assert_eq!(batches.len(), 1);
        assert_eq!(batches[0].serialized_bytes, MAX_MUTATION_REQUEST_BYTES);

        let oversized = RawMutation::new("oversized", MAX_MUTATION_REQUEST_BYTES - empty_bytes + 1);
        let error = prepare_mutation_batches(&[oversized]).unwrap_err();
        assert_eq!(
            error.to_string(),
            format!(
                "GraphQL mutation at item 0 serializes to {} bytes, which exceeds the {MAX_MUTATION_REQUEST_BYTES}-byte request limit. No mutation was sent.",
                MAX_MUTATION_REQUEST_BYTES + 1
            )
        );
    }

    #[test]
    fn worst_case_supported_pull_request_body_fits_one_request() {
        // U+0001 is one UTF-8 byte, six bytes after GraphQL-string escaping,
        // and seven bytes after the outer JSON string escapes the backslash.
        let operation = CreatePullRequest::new(
            "R_1".to_string(),
            "main".to_string(),
            "G123".to_string(),
            "\u{1}".repeat(256),
            "\u{1}".repeat(MAX_BODY_SIZE_BYTES),
        );
        let empty = CreatePullRequest::new(
            "R_1".to_string(),
            "main".to_string(),
            "G123".to_string(),
            "\u{1}".repeat(256),
            String::new(),
        );
        let empty_bytes = mutation_request(&[empty]).unwrap().1;

        let batches = prepare_mutation_batches(&[operation]).unwrap();

        assert_eq!(batches.len(), 1);
        assert_eq!(batches[0].serialized_bytes, empty_bytes + MAX_BODY_SIZE_BYTES * 7);
        assert!(batches[0].serialized_bytes <= MAX_MUTATION_REQUEST_BYTES);
    }

    #[test]
    fn mutation_batch_planning_splits_before_the_byte_limit() {
        let pair_overhead =
            mutation_request(&[RawMutation::new("first", 0), RawMutation::new("second", 0)])
                .unwrap()
                .1;
        let document_bytes = (MAX_MUTATION_REQUEST_BYTES - pair_overhead) / 2 + 1;
        let operations =
            [RawMutation::new("first", document_bytes), RawMutation::new("second", document_bytes)];

        assert!(mutation_request(&operations[..1]).unwrap().1 <= MAX_MUTATION_REQUEST_BYTES);
        assert!(mutation_request(&operations).unwrap().1 > MAX_MUTATION_REQUEST_BYTES);
        assert_eq!(
            prepare_mutation_batches(&operations)
                .unwrap()
                .into_iter()
                .map(|batch| batch.operation_range)
                .collect::<Vec<_>>(),
            [0..1, 1..2]
        );
    }

    #[test]
    fn every_mutation_batch_is_planned_before_any_can_be_sent() {
        let oversized = RawMutation::new("oversized", MAX_MUTATION_REQUEST_BYTES);
        let error = prepare_mutation_batches(&[RawMutation::new("small", 1), oversized])
            .expect_err("a later oversized operation must reject the whole plan");

        assert!(error.to_string().contains("item 1"));
        assert!(error.to_string().contains("No mutation was sent"));
    }

    #[test]
    fn mutation_batches_reject_duplicate_client_mutation_ids_before_sending() {
        let error = prepare_mutation_batches(&[
            RawMutation::new("duplicate", 1),
            RawMutation::new("duplicate", 1),
        ])
        .unwrap_err();

        assert_eq!(
            error.to_string(),
            "GraphQL mutation at item 1 repeats clientMutationId 'duplicate'. No mutation was sent."
        );
    }

    #[test]
    fn duplicate_create_heads_are_rejected_before_sending() {
        let error = prepare_mutation_batches(&[create_for("G123"), create_for("G123")])
            .expect_err("duplicate heads produce duplicate request identities");

        assert!(error.to_string().contains("repeats clientMutationId 'gherrit:create:G123'"));
        assert!(error.to_string().contains("No mutation was sent"));
    }

    #[test]
    fn an_update_must_change_at_least_one_field() {
        let error = UpdatePullRequest::new("PR_1".to_string(), None, None, None).unwrap_err();
        assert_eq!(error.to_string(), "A pull request update must change at least one field");
    }

    #[test]
    fn an_update_requires_a_nonempty_target_node_id() {
        let error = UpdatePullRequest::new(String::new(), Some("updated".to_string()), None, None)
            .unwrap_err();
        assert_eq!(error.to_string(), "A pull request update requires a nonempty node ID");
    }

    #[test]
    fn a_complete_update_receipt_is_acknowledged() {
        assert_eq!(
            decode_mutation_batch_response(
                &[update("PR_1")],
                json!({ "data": { "op0": update_receipt("PR_1") } }),
            )
            .unwrap(),
            [()]
        );
    }

    #[test]
    fn mutation_response_envelopes_fail_closed() {
        let operation = update("PR_1");
        let cases = [
            (json!({}), "Missing JSON field"),
            (json!({ "data": null }), "is not an object"),
            (json!({ "data": [] }), "is not an object"),
            (json!({ "data": {} }), "missing operation `op0`"),
            (json!({ "data": { "op0": null } }), "operation `op0` is null"),
            (
                json!({
                    "data": { "op0": update_receipt("PR_1") },
                    "errors": [{ "message": "partial" }],
                }),
                "contains errors",
            ),
            (
                json!({
                    "data": { "op0": update_receipt("PR_1") },
                    "errors": "malformed",
                }),
                "contains errors",
            ),
            (
                json!({
                    "data": {
                        "op0": update_receipt("PR_1"),
                        "op00": update_receipt("PR_1"),
                    }
                }),
                "unexpected operation `op00`",
            ),
        ];

        cases.into_iter().for_each(|(response, expected)| {
            let error = decode_mutation_batch_response(std::slice::from_ref(&operation), response)
                .unwrap_err();
            assert!(error.to_string().contains(expected), "error={error:?}");
        });
    }

    #[test]
    fn a_partial_mutation_response_identifies_the_missing_alias() {
        let operations = [update("PR_1"), update("PR_2")];
        let error = decode_mutation_batch_response(
            &operations,
            json!({ "data": { "op0": update_receipt("PR_1") } }),
        )
        .unwrap_err();

        assert_eq!(error.to_string(), "GraphQL mutation response is missing operation `op1`");
    }

    #[test]
    fn update_receipts_must_match_the_request_and_target() {
        let operation = update("PR_1");
        let cases = [
            (
                json!({
                    "clientMutationId": "gherrit:update:other",
                    "pullRequest": { "id": "PR_1" },
                }),
                "echoed clientMutationId",
            ),
            (
                json!({
                    "clientMutationId": "gherrit:update:PR_1",
                    "pullRequest": null,
                }),
                "response pull request was null",
            ),
            (
                json!({
                    "clientMutationId": "gherrit:update:PR_1",
                    "pullRequest": { "id": "PR_other" },
                }),
                "returned node ID 'PR_other'",
            ),
            (json!({ "pullRequest": { "id": "PR_1" } }), "missing field `clientMutationId`"),
            (
                json!({ "clientMutationId": "gherrit:update:PR_1" }),
                "response pull request was null",
            ),
        ];

        cases.into_iter().for_each(|(receipt, expected)| {
            let error = decode_mutation_batch_response(
                std::slice::from_ref(&operation),
                json!({ "data": { "op0": receipt } }),
            )
            .unwrap_err();
            assert!(format!("{error:?}").contains(expected), "error={error:?}");
        });
    }

    #[test]
    fn create_receipts_return_the_assigned_identity_for_the_expected_head() {
        assert_eq!(
            decode_mutation_batch_response(
                &[create()],
                json!({ "data": { "op0": create_receipt() } }),
            )
            .unwrap(),
            [CreatedPullRequest {
                head_branch: "G123".to_string(),
                number: 42,
                node_id: "PR_42".to_string(),
            }]
        );
    }

    #[test]
    fn create_receipt_numbers_must_fit_graphql_int() {
        let maximum = i32::MAX as u64;
        assert_eq!(
            decode_mutation_batch_response(
                &[create()],
                json!({ "data": { "op0": create_receipt_for("G123", maximum, "PR_max") } }),
            )
            .unwrap(),
            [CreatedPullRequest {
                head_branch: "G123".to_string(),
                number: maximum,
                node_id: "PR_max".to_string(),
            }]
        );

        for number in [maximum + 1, u64::MAX] {
            let error = decode_mutation_batch_response(
                &[create()],
                json!({ "data": { "op0": create_receipt_for("G123", number, "PR_too_large") } }),
            )
            .unwrap_err();
            assert!(
                format!("{error:?}").contains(&format!("invalid pull request number {number}")),
                "error={error:?}"
            );
        }
    }

    #[test]
    fn create_receipts_have_unique_pull_request_identities() {
        let operations = [create_for("G1"), create_for("G2")];
        for (second_number, second_node_id, expected) in [
            (42, "PR_43", "repeats pull request number 42"),
            (43, "PR_42", "repeats pull request node ID 'PR_42'"),
        ] {
            let error = decode_mutation_batch_response(
                &operations,
                json!({
                    "data": {
                        "op0": create_receipt_for("G1", 42, "PR_42"),
                        "op1": create_receipt_for("G2", second_number, second_node_id),
                    }
                }),
            )
            .expect_err("duplicate receipt identity must be rejected");
            assert!(error.to_string().contains(expected), "error={error:?}");
        }
    }

    #[test]
    fn create_receipts_must_match_the_request_and_head() {
        let operation = create();
        let cases = [
            (
                json!({
                    "clientMutationId": "gherrit:create:other",
                    "pullRequest": {
                        "number": 42,
                        "id": "PR_42",
                        "headRefName": "G123",
                    }
                }),
                "echoed clientMutationId",
            ),
            (
                json!({
                    "clientMutationId": "gherrit:create:G123",
                    "pullRequest": null,
                }),
                "response pull request was null",
            ),
            (
                json!({
                    "clientMutationId": "gherrit:create:G123",
                    "pullRequest": {
                        "number": 42,
                        "id": "PR_42",
                        "headRefName": "other",
                    }
                }),
                "returned head branch 'other'",
            ),
            (
                json!({
                    "clientMutationId": "gherrit:create:G123",
                    "pullRequest": {
                        "number": 0,
                        "id": "PR_0",
                        "headRefName": "G123",
                    }
                }),
                "invalid pull request number 0",
            ),
            (
                json!({
                    "clientMutationId": "gherrit:create:G123",
                    "pullRequest": {
                        "number": 42,
                        "id": "",
                        "headRefName": "G123",
                    }
                }),
                "empty pull request node ID",
            ),
        ];

        cases.into_iter().for_each(|(receipt, expected)| {
            let error = decode_mutation_batch_response(
                std::slice::from_ref(&operation),
                json!({ "data": { "op0": receipt } }),
            )
            .unwrap_err();
            assert!(format!("{error:?}").contains(expected), "error={error:?}");
        });
    }
}

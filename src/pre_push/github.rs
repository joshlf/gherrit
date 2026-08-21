use std::{collections::HashSet, ops::Range};

use color_eyre::eyre::{Context as _, Result, bail, eyre};
use gix::ObjectId;
use serde::Deserialize;
use serde_json::{Value, json};

use super::{
    batching::{MAX_MUTATION_ALIASES, MAX_MUTATION_REQUEST_BYTES},
    reconcile::PullRequestState,
};

const MAX_PULL_REQUEST_CANDIDATES: usize = 100;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct PullRequest {
    pub(super) number: u64,
    pub(super) node_id: String,
    pub(super) title: Option<String>,
    pub(super) body: Option<String>,
    pub(super) base_branch: String,
    pub(super) head_branch: String,
    pub(super) state: PullRequestState,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct CreatedPullRequest {
    pub(super) head_branch: String,
    pub(super) number: u64,
    pub(super) node_id: String,
}

/// One read-only GraphQL field which may be retried in a smaller query.
pub(super) trait QueryOperation {
    type Output;

    fn document(&self) -> String;
    fn decode(&self, response: Value) -> Result<Self::Output>;
}

/// One GraphQL mutation field whose response is an acknowledgement receipt.
///
/// Mutation operations deliberately do not implement `QueryOperation`. This
/// keeps adaptive query retries unavailable to writes at the type level.
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

/// Builds the exact GraphQL document sent for one adaptive query batch.
pub(super) fn query_batch_document<O: QueryOperation>(operations: &[O]) -> String {
    let body = operations
        .iter()
        .enumerate()
        .map(|(index, operation)| format!("op{index}: {}", operation.document()))
        .collect::<String>();
    format!("query {{ {body} }}")
}

/// Decodes every aliased operation in a successful adaptive query response.
pub(super) fn decode_query_batch_response<O: QueryOperation>(
    operations: &[O],
    response: Value,
) -> Result<Vec<O::Output>> {
    let data = response
        .get("data")
        .ok_or_else(|| eyre!("Missing JSON field in GraphQL response: `data`"))?
        .as_object()
        .ok_or_else(|| eyre!("GraphQL response field `data` is not an object"))?;

    operations
        .iter()
        .enumerate()
        .map(|(index, operation)| {
            let alias = format!("op{index}");
            let response = data
                .get(&alias)
                .cloned()
                .ok_or_else(|| eyre!("GraphQL response is missing operation `{alias}`"))?;
            operation.decode(response)
        })
        .collect()
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

/// A query for the global node ID GitHub requires when creating PRs.
pub(super) struct RepositoryIdQuery {
    owner: String,
    repository: String,
}

impl RepositoryIdQuery {
    const DOCUMENT: &'static str = "query RepositoryID($owner: String!, $name: String!) { repository(owner: $owner, name: $name) { id } }";

    pub(super) fn new(owner: String, repository: String) -> Self {
        Self { owner, repository }
    }

    pub(super) fn request(&self) -> Value {
        json!({
            "query": Self::DOCUMENT,
            "variables": {
                "owner": self.owner,
                "name": self.repository,
            }
        })
    }

    pub(super) fn decode(&self, response: Value) -> Result<String> {
        if let Some(errors) = response.get("errors") {
            bail!("Failed to fetch repository ID: {errors:?}");
        }

        #[derive(Deserialize)]
        struct Response {
            data: Data,
        }

        #[derive(Deserialize)]
        struct Data {
            repository: Repository,
        }

        #[derive(Deserialize)]
        struct Repository {
            id: String,
        }

        let response: Response =
            serde_json::from_value(response).wrap_err("Failed to decode repository ID response")?;
        Ok(response.data.repository.id)
    }
}

/// Looks up the PR whose head branch is a GHerrit ID.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct FindPullRequest {
    owner: String,
    repository: String,
    head_branch: String,
}

impl FindPullRequest {
    pub(super) fn new(owner: String, repository: String, head_branch: String) -> Self {
        Self { owner, repository, head_branch }
    }
}

impl QueryOperation for FindPullRequest {
    type Output = Option<PullRequest>;

    fn document(&self) -> String {
        let connection = |alias: &str, states: &str| {
            format!(
                "{alias}: pullRequests(headRefName: {}, first: {MAX_PULL_REQUEST_CANDIDATES}, states: {states}) {{ nodes {{ number, id, title, body, baseRefName, state, isCrossRepository }} pageInfo {{ hasNextPage }} }}",
                json!(self.head_branch),
            )
        };
        format!(
            "repository(owner: {}, name: {}) {{ {} {} }}",
            json!(self.owner),
            json!(self.repository),
            connection("open", "[OPEN]"),
            connection("historical", "[CLOSED, MERGED]"),
        )
    }

    fn decode(&self, response: Value) -> Result<Self::Output> {
        #[derive(Deserialize)]
        #[serde(rename_all = "camelCase")]
        struct Response {
            open: PullRequests,
            historical: PullRequests,
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
        }

        #[derive(Deserialize)]
        #[serde(rename_all = "camelCase")]
        struct Node {
            number: u64,
            id: String,
            title: Option<String>,
            body: Option<String>,
            base_ref_name: String,
            state: PullRequestState,
            is_cross_repository: bool,
        }

        let response: Response = serde_json::from_value(response)
            .wrap_err("Failed to decode pull request query response")?;
        let select = |kind: &str, pull_requests: PullRequests| -> Result<Option<Node>> {
            let mut candidates = pull_requests
                .nodes
                .into_iter()
                .filter(|node| !node.is_cross_repository)
                .collect::<Vec<_>>();
            if candidates.len() > 1 {
                let candidates = candidates
                    .iter()
                    .map(|node| format!("#{}", node.number))
                    .collect::<Vec<_>>()
                    .join(", ");
                bail!(
                    "Found multiple {kind} pull requests for GHerrit ID '{}': {candidates}. GHerrit cannot safely choose one.",
                    self.head_branch
                );
            }
            if pull_requests.page_info.has_next_page {
                bail!(
                    "Found more than {MAX_PULL_REQUEST_CANDIDATES} {kind} pull request candidates for GHerrit ID '{}'. GHerrit cannot safely inspect them all.",
                    self.head_branch
                );
            }
            Ok(candidates.pop())
        };

        // A sole open PR is authoritative even when the same managed branch
        // has closed or merged history. Only consult history when no open PR
        // from this repository exists.
        let node = match select("open", response.open)? {
            Some(open) => Some(open),
            None => select("historical", response.historical)?,
        };
        Ok(node.map(|node| PullRequest {
            number: node.number,
            node_id: node.id,
            title: node.title,
            body: node.body,
            base_branch: node.base_ref_name,
            head_branch: self.head_branch.clone(),
            state: node.state,
        }))
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
    head_oid: ObjectId,
    base_oid: ObjectId,
    client_mutation_id: String,
}

impl CreatePullRequest {
    pub(super) fn new(
        repository_id: String,
        base_branch: String,
        head_branch: String,
        title: String,
        body: String,
        head_oid: ObjectId,
        base_oid: ObjectId,
    ) -> Self {
        let client_mutation_id = format!("gherrit:create:{head_branch}");
        Self {
            repository_id,
            base_branch,
            head_branch,
            title,
            body,
            head_oid,
            base_oid,
            client_mutation_id,
        }
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
            ("headRepositoryId", &self.repository_id),
            ("baseRefName", &self.base_branch),
            ("headRefName", &self.head_branch),
            ("title", &self.title),
            ("body", &self.body),
            ("clientMutationId", &self.client_mutation_id),
        ]
        .map(|(name, value)| format!("{name}: {}", json!(value)))
        .join(", ");
        format!(
            "createPullRequest(input: {{ {fields} }}) {{ clientMutationId, pullRequest {{ number, id, state, headRefName, headRefOid, headRepository {{ id }}, baseRefName, baseRefOid, baseRepository {{ id }} }} }}"
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
            state: PullRequestState,
            head_ref_name: String,
            head_ref_oid: String,
            head_repository: Option<CreatedRepositoryResponse>,
            base_ref_name: String,
            base_ref_oid: String,
            base_repository: Option<CreatedRepositoryResponse>,
        }

        #[derive(Deserialize)]
        struct CreatedRepositoryResponse {
            id: String,
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
        if created.base_ref_name != self.base_branch {
            bail!(
                "createPullRequest returned base branch '{}', expected '{}'",
                created.base_ref_name,
                self.base_branch
            );
        }
        if created.state != PullRequestState::Open {
            bail!("createPullRequest returned a pull request which is not OPEN");
        }
        for (kind, repository) in
            [("head", created.head_repository), ("base", created.base_repository)]
        {
            let repository = repository
                .ok_or_else(|| eyre!("createPullRequest omitted the {kind} repository"))?;
            if repository.id != self.repository_id {
                bail!("createPullRequest returned a different {kind} repository");
            }
        }
        let parse_oid = |kind: &str, value: &str| {
            ObjectId::from_hex(value.as_bytes())
                .map_err(|_| eyre!("createPullRequest returned an invalid {kind} object ID"))
        };
        if parse_oid("head", &created.head_ref_oid)? != self.head_oid {
            bail!("createPullRequest returned a different head object ID");
        }
        if parse_oid("base", &created.base_ref_oid)? != self.base_oid {
            bail!("createPullRequest returned a different base object ID");
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
    number: u64,
    node_id: String,
    title: Option<String>,
    body: Option<String>,
    base_branch: Option<String>,
    client_mutation_id: String,
}

impl UpdatePullRequest {
    pub(super) fn new(
        number: u64,
        node_id: String,
        title: Option<String>,
        body: Option<String>,
        base_branch: Option<String>,
    ) -> Result<Self> {
        if !(1..=i32::MAX as u64).contains(&number) {
            bail!("A pull request update requires a number in the GraphQL Int range");
        }
        if node_id.is_empty() {
            bail!("A pull request update requires a nonempty node ID");
        }
        if title.is_none() && body.is_none() && base_branch.is_none() {
            bail!("A pull request update must change at least one field");
        }
        let client_mutation_id = format!("gherrit:update:{node_id}");
        Ok(Self { number, node_id, title, body, base_branch, client_mutation_id })
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
            "updatePullRequest(input: {{ {fields} }}) {{ clientMutationId, pullRequest {{ number, id }} }}"
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
            number: u64,
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
        if updated.number != self.number || updated.id != self.node_id {
            bail!(
                "updatePullRequest returned pull request identity #{} / '{}', expected #{} / '{}'",
                updated.number,
                updated.id,
                self.number,
                self.node_id
            );
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pre_push::body::MAX_BODY_SIZE_BYTES;

    fn object_id(hex_digit: u8) -> ObjectId {
        ObjectId::from_hex(&[hex_digit; 40]).unwrap()
    }

    fn pull_request_node(number: u64, state: &str, is_cross_repository: bool) -> Value {
        json!({
            "number": number,
            "id": format!("PR_{number}"),
            "title": "Title",
            "body": null,
            "baseRefName": "main",
            "state": state,
            "isCrossRepository": is_cross_repository,
        })
    }

    fn connection(nodes: Vec<Value>, has_next_page: bool) -> Value {
        json!({
            "nodes": nodes,
            "pageInfo": { "hasNextPage": has_next_page },
        })
    }

    fn lookup_response(open: Value, historical: Value) -> Value {
        json!({ "open": open, "historical": historical })
    }

    fn empty_connection() -> Value {
        connection(Vec::new(), false)
    }

    #[test]
    fn repository_id_query_uses_an_exact_document_and_variables() {
        let query = RepositoryIdQuery::new("o\"wner".to_string(), "repo\nname".to_string());

        assert_eq!(
            query.request(),
            json!({
                "query": RepositoryIdQuery::DOCUMENT,
                "variables": {
                    "owner": "o\"wner",
                    "name": "repo\nname",
                }
            })
        );
    }

    #[test]
    fn query_document_escapes_every_repository_identity_component() {
        let query = FindPullRequest::new(
            "o\"wner".to_string(),
            "repo\nname".to_string(),
            "head\\branch".to_string(),
        );

        assert_eq!(
            query.document(),
            r#"repository(owner: "o\"wner", name: "repo\nname") { open: pullRequests(headRefName: "head\\branch", first: 100, states: [OPEN]) { nodes { number, id, title, body, baseRefName, state, isCrossRepository } pageInfo { hasNextPage } } historical: pullRequests(headRefName: "head\\branch", first: 100, states: [CLOSED, MERGED]) { nodes { number, id, title, body, baseRefName, state, isCrossRepository } pageInfo { hasNextPage } } }"#
        );
    }

    #[test]
    fn create_document_escapes_every_input() {
        let create = CreatePullRequest::new(
            "repo\"id".to_string(),
            "base\nbranch".to_string(),
            "head\\branch".to_string(),
            "A \"title\"".to_string(),
            "line one\nline two".to_string(),
            object_id(b'1'),
            object_id(b'2'),
        );

        assert_eq!(
            create.document(),
            r#"createPullRequest(input: { repositoryId: "repo\"id", headRepositoryId: "repo\"id", baseRefName: "base\nbranch", headRefName: "head\\branch", title: "A \"title\"", body: "line one\nline two", clientMutationId: "gherrit:create:head\\branch" }) { clientMutationId, pullRequest { number, id, state, headRefName, headRefOid, headRepository { id }, baseRefName, baseRefOid, baseRepository { id } } }"#
        );
    }

    #[test]
    fn update_document_omits_unchanged_fields() {
        let update = UpdatePullRequest::new(
            17,
            "PR_node".to_string(),
            Some("new \"title\"".to_string()),
            None,
            None,
        )
        .unwrap();

        assert_eq!(
            update.document(),
            r#"updatePullRequest(input: { pullRequestId: "PR_node", title: "new \"title\"", clientMutationId: "gherrit:update:PR_node" }) { clientMutationId, pullRequest { number, id } }"#
        );
    }

    #[test]
    fn batch_document_aliases_each_operation_exactly() {
        let operations = [
            UpdatePullRequest::new(1, "PR_1".to_string(), Some("one".to_string()), None, None)
                .unwrap(),
            UpdatePullRequest::new(2, "PR_2".to_string(), Some("two".to_string()), None, None)
                .unwrap(),
        ];

        assert_eq!(
            mutation_batch_document(&operations),
            r#"mutation { op0: updatePullRequest(input: { pullRequestId: "PR_1", title: "one", clientMutationId: "gherrit:update:PR_1" }) { clientMutationId, pullRequest { number, id } }op1: updatePullRequest(input: { pullRequestId: "PR_2", title: "two", clientMutationId: "gherrit:update:PR_2" }) { clientMutationId, pullRequest { number, id } } }"#
        );
    }

    #[test]
    fn decodes_repository_id_and_reports_graphql_errors() {
        let query = RepositoryIdQuery::new("owner".to_string(), "repo".to_string());

        assert_eq!(
            query.decode(json!({ "data": { "repository": { "id": "R_1" } } })).unwrap(),
            "R_1"
        );
        assert_eq!(
            query.decode(json!({ "errors": [{ "message": "denied" }] })).unwrap_err().to_string(),
            "Failed to fetch repository ID: Array [Object {\"message\": String(\"denied\")}]"
        );
    }

    #[test]
    fn decodes_pull_request_queries_to_owned_state() {
        let query =
            FindPullRequest::new("owner".to_string(), "repo".to_string(), "G123".to_string());

        assert_eq!(
            query
                .decode(lookup_response(
                    connection(vec![pull_request_node(42, "OPEN", false)], false),
                    empty_connection(),
                ))
                .unwrap(),
            Some(PullRequest {
                number: 42,
                node_id: "PR_42".to_string(),
                title: Some("Title".to_string()),
                body: None,
                base_branch: "main".to_string(),
                head_branch: "G123".to_string(),
                state: PullRequestState::Open,
            })
        );
        assert_eq!(
            query.decode(lookup_response(empty_connection(), empty_connection())).unwrap(),
            None
        );
    }

    #[test]
    fn pull_request_lifecycle_decoding_is_exhaustive_and_fail_closed() {
        let query =
            FindPullRequest::new("owner".to_string(), "repo".to_string(), "G123".to_string());
        let response = |state| {
            let node = pull_request_node(42, state, false);
            match state {
                "OPEN" => lookup_response(connection(vec![node], false), empty_connection()),
                _ => lookup_response(empty_connection(), connection(vec![node], false)),
            }
        };

        [
            ("OPEN", PullRequestState::Open),
            ("CLOSED", PullRequestState::Closed),
            ("MERGED", PullRequestState::Merged),
        ]
        .into_iter()
        .for_each(|(wire_state, expected)| {
            let pull_request = query.decode(response(wire_state)).unwrap().unwrap();
            assert_eq!(pull_request.state, expected, "wire_state={wire_state}");
        });

        let error = query.decode(response("UNKNOWN")).unwrap_err();
        assert_eq!(error.to_string(), "Failed to decode pull request query response");
        assert!(format!("{error:?}").contains("unknown variant `UNKNOWN`"));
    }

    #[test]
    fn a_unique_open_pull_request_wins_over_history() {
        let query =
            FindPullRequest::new("owner".to_string(), "repo".to_string(), "G123".to_string());

        let selected = query
            .decode(lookup_response(
                connection(vec![pull_request_node(42, "OPEN", false)], false),
                connection(
                    vec![
                        pull_request_node(7, "CLOSED", false),
                        pull_request_node(8, "MERGED", false),
                    ],
                    true,
                ),
            ))
            .unwrap()
            .unwrap();

        assert_eq!(selected.number, 42);
        assert_eq!(selected.state, PullRequestState::Open);
    }

    #[test]
    fn fork_pull_requests_do_not_participate_in_selection() {
        let query =
            FindPullRequest::new("owner".to_string(), "repo".to_string(), "G123".to_string());

        let selected = query
            .decode(lookup_response(
                connection(
                    vec![pull_request_node(7, "OPEN", true), pull_request_node(42, "OPEN", false)],
                    false,
                ),
                empty_connection(),
            ))
            .unwrap()
            .unwrap();
        assert_eq!(selected.number, 42);

        assert_eq!(
            query
                .decode(lookup_response(
                    connection(vec![pull_request_node(7, "OPEN", true)], false),
                    connection(vec![pull_request_node(8, "CLOSED", true)], false),
                ))
                .unwrap(),
            None
        );
    }

    #[test]
    fn a_unique_historical_pull_request_preserves_lifecycle_handling() {
        let query =
            FindPullRequest::new("owner".to_string(), "repo".to_string(), "G123".to_string());

        let selected = query
            .decode(lookup_response(
                connection(vec![pull_request_node(7, "OPEN", true)], false),
                connection(vec![pull_request_node(42, "MERGED", false)], false),
            ))
            .unwrap()
            .unwrap();

        assert_eq!(selected.number, 42);
        assert_eq!(selected.state, PullRequestState::Merged);
    }

    #[test]
    fn duplicate_same_repository_candidates_are_ambiguous_by_lifecycle() {
        let query =
            FindPullRequest::new("owner".to_string(), "repo".to_string(), "G123".to_string());

        let open_error = query
            .decode(lookup_response(
                connection(
                    vec![
                        pull_request_node(42, "OPEN", false),
                        pull_request_node(99, "OPEN", false),
                    ],
                    false,
                ),
                empty_connection(),
            ))
            .unwrap_err();

        assert_eq!(
            open_error.to_string(),
            "Found multiple open pull requests for GHerrit ID 'G123': #42, #99. GHerrit cannot safely choose one."
        );

        let historical_error = query
            .decode(lookup_response(
                empty_connection(),
                connection(
                    vec![
                        pull_request_node(42, "CLOSED", false),
                        pull_request_node(99, "MERGED", false),
                    ],
                    false,
                ),
            ))
            .unwrap_err();
        assert_eq!(
            historical_error.to_string(),
            "Found multiple historical pull requests for GHerrit ID 'G123': #42, #99. GHerrit cannot safely choose one."
        );
    }

    #[test]
    fn incomplete_candidate_pages_fail_closed() {
        let query =
            FindPullRequest::new("owner".to_string(), "repo".to_string(), "G123".to_string());

        let open_error = query
            .decode(lookup_response(
                connection(vec![pull_request_node(7, "OPEN", true)], true),
                empty_connection(),
            ))
            .unwrap_err();
        assert_eq!(
            open_error.to_string(),
            "Found more than 100 open pull request candidates for GHerrit ID 'G123'. GHerrit cannot safely inspect them all."
        );

        let historical_error = query
            .decode(lookup_response(
                empty_connection(),
                connection(vec![pull_request_node(7, "CLOSED", true)], true),
            ))
            .unwrap_err();
        assert_eq!(
            historical_error.to_string(),
            "Found more than 100 historical pull request candidates for GHerrit ID 'G123'. GHerrit cannot safely inspect them all."
        );
    }

    #[test]
    fn incomplete_pull_request_nodes_are_errors() {
        let query =
            FindPullRequest::new("owner".to_string(), "repo".to_string(), "G123".to_string());
        let error = query
            .decode(lookup_response(
                connection(
                    vec![json!({
                        "number": 42,
                        "id": "PR_42",
                        "state": "OPEN",
                        "isCrossRepository": false,
                    })],
                    false,
                ),
                empty_connection(),
            ))
            .unwrap_err();

        assert_eq!(error.to_string(), "Failed to decode pull request query response");
        assert!(format!("{error:?}").contains("missing field `baseRefName`"));
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
        let number = node_id
            .strip_prefix("PR_")
            .and_then(|value| value.parse::<u64>().ok())
            .unwrap_or(1)
            .max(1);
        UpdatePullRequest::new(number, node_id.to_string(), Some("updated".to_string()), None, None)
            .unwrap()
    }

    fn update_receipt(node_id: &str) -> Value {
        let number = node_id
            .strip_prefix("PR_")
            .and_then(|value| value.parse::<u64>().ok())
            .unwrap_or(1)
            .max(1);
        json!({
            "clientMutationId": format!("gherrit:update:{node_id}"),
            "pullRequest": { "number": number, "id": node_id },
        })
    }

    fn create_for(head_branch: &str) -> CreatePullRequest {
        CreatePullRequest::new(
            "R_1".to_string(),
            "main".to_string(),
            head_branch.to_string(),
            "Title".to_string(),
            "Body".to_string(),
            object_id(b'1'),
            object_id(b'2'),
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
                "state": "OPEN",
                "headRefName": head_branch,
                "headRefOid": object_id(b'1').to_string(),
                "headRepository": { "id": "R_1" },
                "baseRefName": "main",
                "baseRefOid": object_id(b'2').to_string(),
                "baseRepository": { "id": "R_1" },
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
            object_id(b'1'),
            object_id(b'2'),
        );
        let empty = CreatePullRequest::new(
            "R_1".to_string(),
            "main".to_string(),
            "G123".to_string(),
            "\u{1}".repeat(256),
            String::new(),
            object_id(b'1'),
            object_id(b'2'),
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
        let error = UpdatePullRequest::new(1, "PR_1".to_string(), None, None, None).unwrap_err();
        assert_eq!(error.to_string(), "A pull request update must change at least one field");
    }

    #[test]
    fn an_update_requires_a_nonempty_target_node_id() {
        let error =
            UpdatePullRequest::new(1, String::new(), Some("updated".to_string()), None, None)
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
        let mut wrong_key = update_receipt("PR_1");
        wrong_key["clientMutationId"] = json!("gherrit:update:other");
        let mut wrong_number = update_receipt("PR_1");
        wrong_number["pullRequest"]["number"] = json!(2);
        let mut wrong_node = update_receipt("PR_1");
        wrong_node["pullRequest"]["id"] = json!("PR_other");
        let cases = [
            (wrong_key, "echoed clientMutationId"),
            (
                json!({
                    "clientMutationId": "gherrit:update:PR_1",
                    "pullRequest": null,
                }),
                "response pull request was null",
            ),
            (wrong_number, "returned pull request identity #2 / 'PR_1'"),
            (wrong_node, "returned pull request identity #1 / 'PR_other'"),
            (
                json!({ "pullRequest": { "number": 1, "id": "PR_1" } }),
                "missing field `clientMutationId`",
            ),
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
        let mutate = |pointer: &str, value: Value| {
            let mut receipt = create_receipt();
            *receipt.pointer_mut(pointer).expect("valid receipt pointer") = value;
            receipt
        };
        let cases = vec![
            (mutate("/clientMutationId", json!("gherrit:create:other")), "echoed clientMutationId"),
            (
                json!({
                    "clientMutationId": "gherrit:create:G123",
                    "pullRequest": null,
                }),
                "response pull request was null",
            ),
            (mutate("/pullRequest/headRefName", json!("other")), "returned head branch 'other'"),
            (mutate("/pullRequest/baseRefName", json!("other")), "returned base branch 'other'"),
            (mutate("/pullRequest/state", json!("CLOSED")), "is not OPEN"),
            (mutate("/pullRequest/headRepository", Value::Null), "omitted the head repository"),
            (
                mutate("/pullRequest/headRepository/id", json!("R_other")),
                "different head repository",
            ),
            (mutate("/pullRequest/baseRepository", Value::Null), "omitted the base repository"),
            (
                mutate("/pullRequest/baseRepository/id", json!("R_other")),
                "different base repository",
            ),
            (mutate("/pullRequest/headRefOid", json!("invalid")), "invalid head object ID"),
            (
                mutate("/pullRequest/headRefOid", json!(object_id(b'3').to_string())),
                "different head object ID",
            ),
            (mutate("/pullRequest/baseRefOid", json!("invalid")), "invalid base object ID"),
            (
                mutate("/pullRequest/baseRefOid", json!(object_id(b'3').to_string())),
                "different base object ID",
            ),
            (mutate("/pullRequest/number", json!(0)), "invalid pull request number 0"),
            (mutate("/pullRequest/id", json!("")), "empty pull request node ID"),
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

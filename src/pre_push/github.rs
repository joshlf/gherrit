use color_eyre::eyre::{Context as _, Result, bail, eyre};
use serde::Deserialize;
use serde_json::{Value, json};

use super::reconcile::PullRequestState;

const MAX_PULL_REQUEST_CANDIDATES: usize = 100;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct PullRequest {
    pub(super) number: u64,
    pub(super) node_id: String,
    pub(super) title: String,
    pub(super) body: String,
    pub(super) base_branch: String,
    pub(super) state: PullRequestState,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum OperationType {
    Query,
    Mutation,
}

impl OperationType {
    fn keyword(self) -> &'static str {
        match self {
            Self::Query => "query",
            Self::Mutation => "mutation",
        }
    }
}

/// A self-contained GraphQL operation that can be batched with operations of
/// the same type.
pub(super) trait BatchedOperation {
    type Output;

    const TYPE: OperationType;

    fn document(&self) -> String;
    fn decode(&self, response: Value) -> Result<Self::Output>;
}

/// Builds the exact GraphQL document sent for one adaptive batch.
pub(super) fn batch_document<O: BatchedOperation>(operations: &[O]) -> String {
    let body = operations
        .iter()
        .enumerate()
        .map(|(index, operation)| format!("op{index}: {}", operation.document()))
        .collect::<String>();
    format!("{} {{ {body} }}", O::TYPE.keyword())
}

/// Decodes every aliased operation in a successful adaptive batch response.
pub(super) fn decode_batch_response<O: BatchedOperation>(
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

impl BatchedOperation for FindPullRequest {
    type Output = Option<PullRequest>;

    const TYPE: OperationType = OperationType::Query;

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
            title: String,
            body: String,
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
}

impl CreatePullRequest {
    pub(super) fn new(
        repository_id: String,
        base_branch: String,
        head_branch: String,
        title: String,
        body: String,
    ) -> Self {
        Self { repository_id, base_branch, head_branch, title, body }
    }
}

impl BatchedOperation for CreatePullRequest {
    type Output = PullRequest;

    const TYPE: OperationType = OperationType::Mutation;

    fn document(&self) -> String {
        let fields = [
            ("repositoryId", &self.repository_id),
            ("baseRefName", &self.base_branch),
            ("headRefName", &self.head_branch),
            ("title", &self.title),
            ("body", &self.body),
        ]
        .map(|(name, value)| format!("{name}: {}", json!(value)))
        .join(", ");
        format!(
            "createPullRequest(input: {{ {fields} }}) {{ pullRequest {{ number, id, title, body, baseRefName, state }} }}"
        )
    }

    fn decode(&self, response: Value) -> Result<Self::Output> {
        #[derive(Deserialize)]
        #[serde(rename_all = "camelCase")]
        struct Response {
            pull_request: Option<CreatedPullRequestResponse>,
        }

        #[derive(Deserialize)]
        #[serde(rename_all = "camelCase")]
        struct CreatedPullRequestResponse {
            number: u64,
            id: String,
            title: String,
            body: String,
            base_ref_name: String,
            state: PullRequestState,
        }

        let response: Response = serde_json::from_value(response)
            .wrap_err("Failed to decode createPullRequest response")?;
        let created = response.pull_request.ok_or_else(|| {
            eyre!(
                "The batched GraphQL mutation failed to create PR for head branch '{}'. The response pull request was null.",
                self.head_branch
            )
        })?;

        Ok(PullRequest {
            number: created.number,
            node_id: created.id,
            title: created.title,
            body: created.body,
            base_branch: created.base_ref_name,
            state: created.state,
        })
    }
}

/// A minimal update to an existing PR.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct UpdatePullRequest {
    node_id: String,
    title: Option<String>,
    body: Option<String>,
    base_branch: Option<String>,
}

impl UpdatePullRequest {
    pub(super) fn new(
        node_id: String,
        title: Option<String>,
        body: Option<String>,
        base_branch: Option<String>,
    ) -> Self {
        Self { node_id, title, body, base_branch }
    }
}

impl BatchedOperation for UpdatePullRequest {
    type Output = ();

    const TYPE: OperationType = OperationType::Mutation;

    fn document(&self) -> String {
        let fields = std::iter::once(("pullRequestId", &self.node_id))
            .chain(self.base_branch.as_ref().map(|value| ("baseRefName", value)))
            .chain(self.title.as_ref().map(|value| ("title", value)))
            .chain(self.body.as_ref().map(|value| ("body", value)))
            .map(|(name, value)| format!("{name}: {}", json!(value)))
            .collect::<Vec<_>>()
            .join(", ");
        format!("updatePullRequest(input: {{ {fields} }}) {{ clientMutationId }}")
    }

    fn decode(&self, response: Value) -> Result<Self::Output> {
        if response.is_null() {
            bail!(
                "The batched GraphQL mutation failed to update PR with node ID '{}'. The response for this operation was null.",
                self.node_id
            );
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pull_request_node(number: u64, state: &str, is_cross_repository: bool) -> Value {
        json!({
            "number": number,
            "id": format!("PR_{number}"),
            "title": "Title",
            "body": "Body",
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
        );

        assert_eq!(
            create.document(),
            r#"createPullRequest(input: { repositoryId: "repo\"id", baseRefName: "base\nbranch", headRefName: "head\\branch", title: "A \"title\"", body: "line one\nline two" }) { pullRequest { number, id, title, body, baseRefName, state } }"#
        );
    }

    #[test]
    fn update_document_omits_unchanged_fields() {
        let update = UpdatePullRequest::new(
            "PR_node".to_string(),
            Some("new \"title\"".to_string()),
            None,
            None,
        );

        assert_eq!(
            update.document(),
            r#"updatePullRequest(input: { pullRequestId: "PR_node", title: "new \"title\"" }) { clientMutationId }"#
        );
    }

    #[test]
    fn batch_document_aliases_each_operation_exactly() {
        let operations = [
            UpdatePullRequest::new("PR_1".to_string(), None, None, None),
            UpdatePullRequest::new("PR_2".to_string(), None, None, None),
        ];

        assert_eq!(
            batch_document(&operations),
            r#"mutation { op0: updatePullRequest(input: { pullRequestId: "PR_1" }) { clientMutationId }op1: updatePullRequest(input: { pullRequestId: "PR_2" }) { clientMutationId } }"#
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
                title: "Title".to_string(),
                body: "Body".to_string(),
                base_branch: "main".to_string(),
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
                        "title": "Title",
                        "body": "Body",
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

    #[test]
    fn a_partial_batch_response_identifies_the_missing_alias() {
        let operations = [
            UpdatePullRequest::new("PR_1".to_string(), None, None, None),
            UpdatePullRequest::new("PR_2".to_string(), None, None, None),
        ];
        let error = decode_batch_response(
            &operations,
            json!({ "data": { "op0": { "clientMutationId": null } } }),
        )
        .unwrap_err();

        assert_eq!(error.to_string(), "GraphQL response is missing operation `op1`");
    }

    #[test]
    fn null_mutation_results_are_errors() {
        let update = UpdatePullRequest::new("PR_1".to_string(), None, None, None);
        let error =
            decode_batch_response(&[update], json!({ "data": { "op0": null } })).unwrap_err();

        assert_eq!(
            error.to_string(),
            "The batched GraphQL mutation failed to update PR with node ID 'PR_1'. The response for this operation was null."
        );
    }

    #[test]
    fn decodes_created_pull_requests() {
        let create = CreatePullRequest::new(
            "R_1".to_string(),
            "main".to_string(),
            "G123".to_string(),
            "Title".to_string(),
            "Body".to_string(),
        );

        assert_eq!(
            create
                .decode(json!({
                    "pullRequest": {
                        "number": 42,
                        "id": "PR_42",
                        "title": "Title",
                        "body": "Body",
                        "baseRefName": "main",
                        "state": "OPEN"
                    }
                }))
                .unwrap(),
            PullRequest {
                number: 42,
                node_id: "PR_42".to_string(),
                title: "Title".to_string(),
                body: "Body".to_string(),
                base_branch: "main".to_string(),
                state: PullRequestState::Open,
            }
        );
    }
}

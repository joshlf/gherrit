use color_eyre::eyre::{Context as _, Result, bail, eyre};
use serde::Deserialize;
use serde_json::{Value, json};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub(super) enum PullRequestState {
    Open,
    Closed,
    Merged,
}

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
    pub(super) url: String,
    pub(super) node_id: String,
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
        format!(
            "repository(owner: {}, name: {}) {{ pullRequests(headRefName: {}, first: 2, states: [OPEN, CLOSED, MERGED]) {{ nodes {{ number, id, title, body, baseRefName, state }} }} }}",
            json!(self.owner),
            json!(self.repository),
            json!(self.head_branch),
        )
    }

    fn decode(&self, response: Value) -> Result<Self::Output> {
        #[derive(Deserialize)]
        #[serde(rename_all = "camelCase")]
        struct Response {
            pull_requests: PullRequests,
        }

        #[derive(Deserialize)]
        struct PullRequests {
            nodes: Vec<Node>,
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
        }

        let mut response: Response = serde_json::from_value(response)
            .wrap_err("Failed to decode pull request query response")?;
        if response.pull_requests.nodes.len() > 1 {
            let candidates = response
                .pull_requests
                .nodes
                .iter()
                .map(|node| format!("#{}", node.number))
                .collect::<Vec<_>>()
                .join(", ");
            bail!(
                "Found multiple pull requests for GHerrit ID '{}': {candidates}. GHerrit cannot safely choose one.",
                self.head_branch
            );
        }

        Ok(response.pull_requests.nodes.pop().map(|node| PullRequest {
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
    type Output = CreatedPullRequest;

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
        format!("createPullRequest(input: {{ {fields} }}) {{ pullRequest {{ number, url, id }} }}")
    }

    fn decode(&self, response: Value) -> Result<Self::Output> {
        #[derive(Deserialize)]
        #[serde(rename_all = "camelCase")]
        struct Response {
            pull_request: Option<CreatedPullRequestResponse>,
        }

        #[derive(Deserialize)]
        struct CreatedPullRequestResponse {
            number: u64,
            url: String,
            id: String,
        }

        let response: Response = serde_json::from_value(response)
            .wrap_err("Failed to decode createPullRequest response")?;
        let created = response.pull_request.ok_or_else(|| {
            eyre!(
                "The batched GraphQL mutation failed to create PR for head branch '{}'. The response pull request was null.",
                self.head_branch
            )
        })?;

        Ok(CreatedPullRequest {
            head_branch: self.head_branch.clone(),
            number: created.number,
            url: created.url,
            node_id: created.id,
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
            r#"repository(owner: "o\"wner", name: "repo\nname") { pullRequests(headRefName: "head\\branch", first: 2, states: [OPEN, CLOSED, MERGED]) { nodes { number, id, title, body, baseRefName, state } } }"#
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
            r#"createPullRequest(input: { repositoryId: "repo\"id", baseRefName: "base\nbranch", headRefName: "head\\branch", title: "A \"title\"", body: "line one\nline two" }) { pullRequest { number, url, id } }"#
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
                .decode(json!({
                    "pullRequests": {
                        "nodes": [{
                            "number": 42,
                            "id": "PR_42",
                            "title": "Title",
                            "body": null,
                            "baseRefName": "main",
                            "state": "OPEN"
                        }]
                    }
                }))
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
        assert_eq!(query.decode(json!({ "pullRequests": { "nodes": [] } })).unwrap(), None);
    }

    #[test]
    fn duplicate_pull_request_candidates_are_ambiguous() {
        let query =
            FindPullRequest::new("owner".to_string(), "repo".to_string(), "G123".to_string());
        let node = |number| {
            json!({
                "number": number,
                "id": format!("PR_{number}"),
                "title": "Title",
                "body": "Body",
                "baseRefName": "main",
                "state": "OPEN"
            })
        };

        let error =
            query.decode(json!({ "pullRequests": { "nodes": [node(42), node(99)] } })).unwrap_err();

        assert_eq!(
            error.to_string(),
            "Found multiple pull requests for GHerrit ID 'G123': #42, #99. GHerrit cannot safely choose one."
        );
    }

    #[test]
    fn incomplete_pull_request_nodes_are_errors() {
        let query =
            FindPullRequest::new("owner".to_string(), "repo".to_string(), "G123".to_string());
        let error = query
            .decode(json!({
                "pullRequests": {
                    "nodes": [{
                        "number": 42,
                        "id": "PR_42",
                        "state": "OPEN"
                    }]
                }
            }))
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
                        "url": "https://github.test/pull/42",
                        "id": "PR_42"
                    }
                }))
                .unwrap(),
            CreatedPullRequest {
                head_branch: "G123".to_string(),
                number: 42,
                url: "https://github.test/pull/42".to_string(),
                node_id: "PR_42".to_string(),
            }
        );
    }
}

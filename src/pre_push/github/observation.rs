//! Exact GitHub pull request observation for the local stack.
//!
//! Each GraphQL alias is bound to one local GHerrit ID and one input cursor.
//! No value is exposed until every requested connection has been exhausted.

use std::collections::HashSet;

use color_eyre::eyre::{Result, bail, eyre};
use serde::Deserialize;
use serde_json::{Value, json};

use super::{DefaultBranchRef, Nullable, ObservedPullRequest, PullRequestState, Repository};
use crate::pre_push::{
    bounded_diagnostic_detail,
    destination::{DefaultBranch, RepositoryCoordinates},
    local::GherritPrId,
    pull_request::ExactLocalPullRequestIdentities,
};

type LocalPullRequestEntries = Box<[(GherritPrId, Box<[ObservedPullRequest]>)]>;

/// Complete rows for the exact local IDs, preserved in requested order.
#[derive(Debug)]
pub(in crate::pre_push) struct CompleteLocalPullRequests {
    entries: Box<[(GherritPrId, Box<[ObservedPullRequest]>)]>,
    identities: ExactLocalPullRequestIdentities,
}

impl CompleteLocalPullRequests {
    pub(super) fn new(entries: Vec<(GherritPrId, Vec<ObservedPullRequest>)>) -> Result<Self> {
        let mut ids = HashSet::with_capacity(entries.len());
        for (id, _) in &entries {
            if !ids.insert(id) {
                bail!(
                    "local pull request observation contains change '{}' more than once",
                    id.as_str()
                );
            }
        }
        let identities = ExactLocalPullRequestIdentities::new(
            entries
                .iter()
                .flat_map(|(_, pull_requests)| pull_requests)
                .map(|pull_request| &pull_request.identity),
        )?;
        let entries = entries
            .into_iter()
            .map(|(id, pull_requests)| (id, pull_requests.into_boxed_slice()))
            .collect();
        Ok(Self { entries, identities })
    }

    #[cfg(test)]
    pub(in crate::pre_push) fn for_test(
        entries: Vec<(GherritPrId, Vec<ObservedPullRequest>)>,
    ) -> Result<Self> {
        Self::new(entries)
    }

    pub(in crate::pre_push) fn into_parts(
        self,
    ) -> (LocalPullRequestEntries, ExactLocalPullRequestIdentities) {
        (self.entries, self.identities)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct LocalPullRequestQuery {
    id: GherritPrId,
    after: Option<String>,
    first: usize,
}

impl LocalPullRequestQuery {
    pub(super) fn new(id: GherritPrId, after: Option<String>, first: usize) -> Result<Self> {
        if first == 0 {
            bail!("A local pull request query requires a positive page size");
        }
        if after.as_deref() == Some("") {
            bail!("A local pull request query requires a nonempty pagination cursor");
        }
        Ok(Self { id, after, first })
    }
}

/// One batch of independently paginated exact local-ID connections.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct LocalPullRequests {
    coordinates: RepositoryCoordinates,
    queries: Vec<LocalPullRequestQuery>,
    include_repository_facts: bool,
}

impl LocalPullRequests {
    pub(super) const MAX_ALIASES: usize = 64;

    pub(super) fn new(
        coordinates: RepositoryCoordinates,
        queries: Vec<LocalPullRequestQuery>,
        include_repository_facts: bool,
    ) -> Result<Self> {
        if queries.is_empty() || queries.len() > Self::MAX_ALIASES {
            bail!("A local pull request query requires between one and 64 aliases");
        }
        let mut ids = HashSet::with_capacity(queries.len());
        for query in &queries {
            if !ids.insert(&query.id) {
                bail!("A local pull request query repeats change '{}'", query.id.as_str());
            }
        }
        Ok(Self { coordinates, queries, include_repository_facts })
    }

    pub(super) fn document(&self) -> String {
        let repository_facts = if self.include_repository_facts {
            "id, defaultBranchRef { name, target { oid } }, "
        } else {
            ""
        };
        let connections = self
            .queries
            .iter()
            .enumerate()
            .map(|(index, query)| {
                let after = query
                    .after
                    .as_ref()
                    .map(|cursor| format!(", after: {}", json!(cursor)))
                    .unwrap_or_default();
                format!(
                    "op{index}: pullRequests(headRefName: {}, first: {}{after}, states: [OPEN, CLOSED, MERGED]) {{ nodes {{ number, id, title, body, baseRefName, baseRefOid, headRefName, headRefOid, state, isCrossRepository, autoMergeRequest {{ enabledAt }}, isInMergeQueue }} pageInfo {{ hasNextPage, endCursor }} }}",
                    json!(query.id.as_str()),
                    query.first,
                )
            })
            .collect::<Vec<_>>()
            .join(" ");
        format!(
            "query {{ repository(owner: {}, name: {}) {{ {repository_facts}{connections} }} }}",
            json!(self.coordinates.owner()),
            json!(self.coordinates.repository()),
        )
    }

    pub(super) fn decode(self, response: Value) -> Result<LocalPullRequestBatch> {
        let mut repository = response
            .get("data")
            .and_then(|data| data.get("repository"))
            .and_then(Value::as_object)
            .cloned()
            .ok_or_else(|| {
                eyre!("GitHub local pull request response is missing repository data")
            })?;

        let repository_facts = if self.include_repository_facts {
            Some(decode_repository(&self.coordinates, &mut repository)?)
        } else {
            if repository.contains_key("id") || repository.contains_key("defaultBranchRef") {
                bail!("GitHub returned unrequested repository facts on a later local PR page");
            }
            None
        };

        let expected =
            (0..self.queries.len()).map(|index| format!("op{index}")).collect::<HashSet<_>>();
        if repository.len() != expected.len() {
            bail!("GitHub local pull request response has an unexpected alias set");
        }
        if let Some(alias) = repository.keys().find(|alias| !expected.contains(*alias)) {
            let alias = bounded_diagnostic_detail(alias);
            bail!("GitHub local pull request response contains unexpected operation `{alias}`");
        }

        let pages = self
            .queries
            .into_iter()
            .enumerate()
            .map(|(index, query)| {
                let alias = format!("op{index}");
                let connection = repository.remove(&alias).ok_or_else(|| {
                    eyre!("GitHub local pull request response is missing operation `{alias}`")
                })?;
                decode_connection(query, connection)
            })
            .collect::<Result<Vec<_>>>()?;
        debug_assert!(repository.is_empty());
        Ok(LocalPullRequestBatch { repository: repository_facts, pages })
    }
}

fn decode_repository(
    coordinates: &RepositoryCoordinates,
    repository: &mut serde_json::Map<String, Value>,
) -> Result<Repository> {
    let node_id = repository
        .remove("id")
        .and_then(|value| value.as_str().map(ToOwned::to_owned))
        .ok_or_else(|| eyre!("GitHub omitted the repository node ID"))?;
    if node_id.is_empty() {
        bail!("GitHub reported an empty repository node ID");
    }
    let default_branch: DefaultBranchRef = serde_json::from_value(
        repository
            .remove("defaultBranchRef")
            .ok_or_else(|| eyre!("GitHub omitted the repository default branch"))?,
    )
    .map_err(|_| eyre!("Failed to decode the repository default branch"))?;
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
    Ok(Repository {
        node_id,
        default_branch: DefaultBranch::new(default_branch.name, tip)
            .map_err(|_| eyre!("GitHub reported an invalid default branch"))?,
        coordinates: coordinates.clone(),
    })
}

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
    state: PullRequestState,
    is_cross_repository: bool,
    auto_merge_request: Nullable<AutoMergeRequest>,
    is_in_merge_queue: bool,
}

fn decode_connection(
    query: LocalPullRequestQuery,
    connection: Value,
) -> Result<LocalPullRequestPageEvidence> {
    let connection: Connection = serde_json::from_value(connection)
        .map_err(|_| eyre!("Failed to decode local pull request query response"))?;
    let pull_requests = connection
        .nodes
        .into_iter()
        .map(|node| decode_pull_request(&query.id, node))
        .collect::<Result<Vec<_>>>()?;
    let next_cursor = match connection.page_info {
        PageInfo { has_next_page: true, end_cursor: Nullable::Value(cursor) }
            if !cursor.is_empty() =>
        {
            Some(cursor)
        }
        PageInfo { has_next_page: true, .. } => bail!(
            "GitHub reported another local pull request page for '{}' without an end cursor",
            query.id.as_str()
        ),
        PageInfo { has_next_page: false, .. } => None,
    };
    Ok(LocalPullRequestPageEvidence {
        id: query.id,
        after: query.after,
        pull_requests,
        next_cursor,
    })
}

fn decode_pull_request(id: &GherritPrId, node: Node) -> Result<ObservedPullRequest> {
    let number = u64::try_from(node.number)
        .ok()
        .filter(|number| *number > 0 && *number <= i32::MAX as u64)
        .ok_or_else(|| eyre!("GitHub reported an invalid pull request number {}", node.number))?;
    for (field, value) in [
        ("pull request node ID", &node.id),
        ("pull request base ref name", &node.base_ref_name),
        ("pull request head ref name", &node.head_ref_name),
    ] {
        if value.is_empty() {
            bail!("GitHub reported an empty {field}");
        }
    }
    if node.head_ref_name != id.as_str() {
        let returned = bounded_diagnostic_detail(&node.head_ref_name);
        let expected = bounded_diagnostic_detail(id.as_str());
        bail!("GitHub pull request query for '{}' returned head branch '{}'", expected, returned);
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
    let has_auto_merge_request = match node.auto_merge_request {
        Nullable::Value(request) => {
            let _ = request.enabled_at;
            true
        }
        Nullable::Null(()) => false,
    };
    Ok(ObservedPullRequest {
        identity: super::PullRequestIdentity::new(number, node.id)?,
        title: node.title,
        body: node.body,
        base_branch: node.base_ref_name,
        head_branch: node.head_ref_name,
        base_oid,
        head_oid,
        state: node.state,
        is_cross_repository: node.is_cross_repository,
        has_auto_merge_request,
        is_in_merge_queue: node.is_in_merge_queue,
    })
}

#[derive(Debug)]
pub(super) struct LocalPullRequestBatch {
    pub(super) repository: Option<Repository>,
    pub(super) pages: Vec<LocalPullRequestPageEvidence>,
}

/// One page inseparably bound to its exact requested ID and input cursor.
#[derive(Debug)]
pub(super) struct LocalPullRequestPageEvidence {
    id: GherritPrId,
    after: Option<String>,
    pull_requests: Vec<ObservedPullRequest>,
    next_cursor: Option<String>,
}

impl LocalPullRequestPageEvidence {
    #[cfg(test)]
    pub(super) fn for_test(
        id: GherritPrId,
        after: Option<String>,
        pull_requests: Vec<ObservedPullRequest>,
        next_cursor: Option<String>,
    ) -> Self {
        Self { id, after, pull_requests, next_cursor }
    }

    pub(super) fn id(&self) -> &GherritPrId {
        &self.id
    }

    pub(super) fn next_cursor(&self) -> Option<&str> {
        self.next_cursor.as_deref()
    }

    pub(super) fn into_parts(
        self,
    ) -> (GherritPrId, Option<String>, Vec<ObservedPullRequest>, Option<String>) {
        (self.id, self.after, self.pull_requests, self.next_cursor)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const DEFAULT_OID: &str = "1111111111111111111111111111111111111111";
    const BASE_OID: &str = "2222222222222222222222222222222222222222";
    const HEAD_OID: &str = "3333333333333333333333333333333333333333";

    fn id(value: &str) -> GherritPrId {
        GherritPrId::from_ref_component(value.as_bytes()).unwrap()
    }

    fn coordinates() -> RepositoryCoordinates {
        crate::pre_push::destination::PushDestination::for_test(
            "origin",
            "https://github.com/owner/repository.git",
            Vec::new(),
        )
        .unwrap()
        .repository_coordinates()
    }

    fn query(value: &str, after: Option<&str>, first: usize) -> LocalPullRequestQuery {
        LocalPullRequestQuery::new(id(value), after.map(str::to_owned), first).unwrap()
    }

    fn operation(
        queries: Vec<LocalPullRequestQuery>,
        include_repository_facts: bool,
    ) -> LocalPullRequests {
        LocalPullRequests::new(coordinates(), queries, include_repository_facts).unwrap()
    }

    fn node(number: i64, node_id: &str, head: &str, state: &str) -> Value {
        json!({
            "number": number,
            "id": node_id,
            "title": format!("title {number}"),
            "body": format!("opaque body {number}"),
            "baseRefName": "main",
            "baseRefOid": BASE_OID,
            "headRefName": head,
            "headRefOid": HEAD_OID,
            "state": state,
            "isCrossRepository": false,
            "autoMergeRequest": null,
            "isInMergeQueue": false,
        })
    }

    fn connection(nodes: Vec<Value>, has_next_page: bool, end_cursor: Value) -> Value {
        json!({
            "nodes": nodes,
            "pageInfo": {
                "hasNextPage": has_next_page,
                "endCursor": end_cursor,
            },
        })
    }

    fn response(
        include_repository_facts: bool,
        connections: impl IntoIterator<Item = (&'static str, Value)>,
    ) -> Value {
        let mut repository = serde_json::Map::new();
        if include_repository_facts {
            repository.insert("id".to_owned(), json!("REPOSITORY_NODE"));
            repository.insert(
                "defaultBranchRef".to_owned(),
                json!({ "name": "main", "target": { "oid": DEFAULT_OID } }),
            );
        }
        repository.extend(
            connections.into_iter().map(|(alias, connection)| (alias.to_owned(), connection)),
        );
        json!({ "data": { "repository": repository } })
    }

    fn one_response(node: Value) -> Value {
        response(true, [("op0", connection(vec![node], false, Value::Null))])
    }

    #[test]
    fn document_binds_each_alias_to_one_head_cursor_and_all_states() {
        let first =
            operation(vec![query("Gone", None, 17), query("Gtwo", Some("cursor:\"2"), 3)], true)
                .document();
        assert_eq!(
            first,
            concat!(
                "query { repository(owner: \"owner\", name: \"repository\") { ",
                "id, defaultBranchRef { name, target { oid } }, ",
                "op0: pullRequests(headRefName: \"Gone\", first: 17, states: ",
                "[OPEN, CLOSED, MERGED]) { nodes { number, id, title, body, ",
                "baseRefName, baseRefOid, headRefName, headRefOid, state, ",
                "isCrossRepository, autoMergeRequest { enabledAt }, isInMergeQueue } ",
                "pageInfo { hasNextPage, endCursor } } ",
                "op1: pullRequests(headRefName: \"Gtwo\", first: 3, after: ",
                "\"cursor:\\\"2\", states: [OPEN, CLOSED, MERGED]) { nodes { ",
                "number, id, title, body, baseRefName, baseRefOid, headRefName, ",
                "headRefOid, state, isCrossRepository, autoMergeRequest { enabledAt }, ",
                "isInMergeQueue } pageInfo { hasNextPage, endCursor } } } }"
            )
        );

        let later = operation(vec![query("Gone", Some("next"), 1)], false).document();
        assert!(!later.contains("defaultBranchRef"));
        assert!(later.contains("repository(owner"));
        assert!(
            later.contains("op0: pullRequests(headRefName: \"Gone\", first: 1, after: \"next\"")
        );
    }

    #[test]
    fn constructors_reject_unusable_or_ambiguous_batches() {
        assert!(LocalPullRequestQuery::new(id("Gone"), None, 0).is_err());
        assert!(LocalPullRequestQuery::new(id("Gone"), Some(String::new()), 1).is_err());
        assert!(LocalPullRequests::new(coordinates(), Vec::new(), true).is_err());
        assert!(
            LocalPullRequests::new(
                coordinates(),
                (0..=LocalPullRequests::MAX_ALIASES)
                    .map(|index| query(&format!("G{index}"), None, 1))
                    .collect(),
                true,
            )
            .is_err()
        );
        assert!(
            LocalPullRequests::new(
                coordinates(),
                vec![query("Gone", None, 1), query("Gone", Some("next"), 1)],
                true,
            )
            .is_err()
        );
    }

    #[test]
    fn first_page_decodes_repository_facts_all_states_and_forks() {
        let mut open = node(1, "PR_OPEN", "Gone", "OPEN");
        open["autoMergeRequest"] = json!({ "enabledAt": "now", "futureField": true });
        open["isInMergeQueue"] = json!(true);
        open["futureNodeField"] = json!({ "ignored": true });
        let mut fork = node(4, "PR_FORK", "Gone", "OPEN");
        fork["isCrossRepository"] = json!(true);
        let decoded = operation(vec![query("Gone", None, 100)], true)
            .decode(response(
                true,
                [(
                    "op0",
                    connection(
                        vec![
                            open,
                            node(2, "PR_CLOSED", "Gone", "CLOSED"),
                            node(3, "PR_MERGED", "Gone", "MERGED"),
                            fork,
                        ],
                        false,
                        Value::Null,
                    ),
                )],
            ))
            .unwrap();

        let repository = decoded.repository.unwrap();
        assert_eq!(repository.node_id, "REPOSITORY_NODE");
        assert_eq!(repository.default_branch.name(), "main");
        assert_eq!(repository.default_branch.tip().to_string(), DEFAULT_OID);
        let rows = &decoded.pages[0].pull_requests;
        assert_eq!(rows.len(), 4);
        assert_eq!(
            rows.iter().map(|row| row.state).collect::<Vec<_>>(),
            [
                PullRequestState::Open,
                PullRequestState::Closed,
                PullRequestState::Merged,
                PullRequestState::Open,
            ]
        );
        assert!(rows[0].has_auto_merge_request);
        assert!(rows[0].is_in_merge_queue);
        assert!(rows[3].is_cross_repository);
        assert_eq!(rows[0].body, "opaque body 1");
    }

    #[test]
    fn repository_facts_are_required_once_and_forbidden_later() {
        let page = [("op0", connection(Vec::new(), false, Value::Null))];
        assert!(
            operation(vec![query("Gone", None, 1)], true)
                .decode(response(false, page.clone()))
                .is_err()
        );
        assert!(
            operation(vec![query("Gone", None, 1)], false)
                .decode(response(true, page.clone()))
                .is_err()
        );
        assert!(
            operation(vec![query("Gone", None, 1)], false)
                .decode(response(false, page))
                .unwrap()
                .repository
                .is_none()
        );

        for pointer in [
            "/data/repository/id",
            "/data/repository/defaultBranchRef",
            "/data/repository/defaultBranchRef/target",
            "/data/repository/defaultBranchRef/target/oid",
        ] {
            let mut value = response(true, [("op0", connection(Vec::new(), false, Value::Null))]);
            *value.pointer_mut(pointer).unwrap() = Value::Null;
            assert!(
                operation(vec![query("Gone", None, 1)], true).decode(value).is_err(),
                "accepted null {pointer}"
            );
        }
        for (pointer, replacement) in [
            ("/data/repository/id", json!("")),
            ("/data/repository/defaultBranchRef/name", json!("")),
            ("/data/repository/defaultBranchRef/target/oid", json!("not-an-oid")),
        ] {
            let mut value = response(true, [("op0", connection(Vec::new(), false, Value::Null))]);
            *value.pointer_mut(pointer).unwrap() = replacement;
            assert!(
                operation(vec![query("Gone", None, 1)], true).decode(value).is_err(),
                "accepted invalid {pointer}"
            );
        }
    }

    #[test]
    fn exact_top_level_alias_set_is_mandatory() {
        let operations = || operation(vec![query("Gone", None, 1), query("Gtwo", None, 1)], true);
        let valid = || {
            response(
                true,
                [
                    ("op0", connection(Vec::new(), false, Value::Null)),
                    ("op1", connection(Vec::new(), false, Value::Null)),
                ],
            )
        };
        assert!(operations().decode(valid()).is_ok());

        for alias in ["op0", "op1"] {
            let mut value = valid();
            value["data"]["repository"].as_object_mut().unwrap().remove(alias);
            assert!(operations().decode(value).is_err(), "accepted missing {alias}");
        }
        let mut extra = valid();
        extra["data"]["repository"]["op2"] = connection(Vec::new(), false, Value::Null);
        assert!(operations().decode(extra).is_err());
        let mut scalar = valid();
        scalar["data"]["repository"]["unexpected"] = json!(true);
        assert!(operations().decode(scalar).is_err());
        for value in [json!({}), json!({ "data": null }), json!({ "data": { "repository": null } })]
        {
            assert!(operations().decode(value).is_err());
        }
    }

    #[test]
    fn every_selected_connection_and_node_field_is_required() {
        for pointer in [
            "/data/repository/op0/nodes",
            "/data/repository/op0/pageInfo",
            "/data/repository/op0/pageInfo/hasNextPage",
            "/data/repository/op0/pageInfo/endCursor",
            "/data/repository/op0/nodes/0/number",
            "/data/repository/op0/nodes/0/id",
            "/data/repository/op0/nodes/0/title",
            "/data/repository/op0/nodes/0/body",
            "/data/repository/op0/nodes/0/baseRefName",
            "/data/repository/op0/nodes/0/baseRefOid",
            "/data/repository/op0/nodes/0/headRefName",
            "/data/repository/op0/nodes/0/headRefOid",
            "/data/repository/op0/nodes/0/state",
            "/data/repository/op0/nodes/0/isCrossRepository",
            "/data/repository/op0/nodes/0/autoMergeRequest",
            "/data/repository/op0/nodes/0/isInMergeQueue",
        ] {
            let mut value = one_response(node(1, "PR_ONE", "Gone", "OPEN"));
            let (parent, field) = pointer.rsplit_once('/').unwrap();
            value.pointer_mut(parent).unwrap().as_object_mut().unwrap().remove(field);
            assert!(
                operation(vec![query("Gone", None, 1)], true).decode(value).is_err(),
                "accepted missing {pointer}"
            );
        }

        let mut missing_enabled_at = one_response(node(1, "PR_ONE", "Gone", "OPEN"));
        missing_enabled_at["data"]["repository"]["op0"]["nodes"][0]["autoMergeRequest"] = json!({});
        assert!(operation(vec![query("Gone", None, 1)], true).decode(missing_enabled_at).is_err());
    }

    #[test]
    fn authority_bearing_identity_and_ref_fields_are_validated() {
        let cases = [
            ("/data/repository/op0/nodes/0/number", json!(0)),
            ("/data/repository/op0/nodes/0/number", json!(-1)),
            ("/data/repository/op0/nodes/0/number", json!(i64::from(i32::MAX) + 1)),
            ("/data/repository/op0/nodes/0/id", json!("")),
            ("/data/repository/op0/nodes/0/baseRefName", json!("")),
            ("/data/repository/op0/nodes/0/headRefName", json!("")),
            ("/data/repository/op0/nodes/0/headRefName", json!("Other")),
            ("/data/repository/op0/nodes/0/baseRefOid", json!("bad")),
            (
                "/data/repository/op0/nodes/0/baseRefOid",
                json!("0000000000000000000000000000000000000000"),
            ),
            ("/data/repository/op0/nodes/0/headRefOid", json!("bad")),
            (
                "/data/repository/op0/nodes/0/headRefOid",
                json!("0000000000000000000000000000000000000000"),
            ),
            ("/data/repository/op0/nodes/0/state", json!("DRAFT")),
        ];
        for (pointer, replacement) in cases {
            let mut value = one_response(node(1, "PR_ONE", "Gone", "OPEN"));
            *value.pointer_mut(pointer).unwrap() = replacement;
            assert!(
                operation(vec![query("Gone", None, 1)], true).decode(value).is_err(),
                "accepted invalid {pointer}"
            );
        }
    }

    #[test]
    fn pagination_requires_a_nonempty_cursor_exactly_when_more_pages_exist() {
        for end_cursor in [Value::Null, json!("")] {
            let value = response(true, [("op0", connection(Vec::new(), true, end_cursor))]);
            assert!(operation(vec![query("Gone", None, 1)], true).decode(value).is_err());
        }
        let decoded = operation(vec![query("Gone", Some("input"), 1)], false)
            .decode(response(false, [("op0", connection(Vec::new(), true, json!("output")))]))
            .unwrap();
        assert_eq!(decoded.pages[0].after.as_deref(), Some("input"));
        assert_eq!(decoded.pages[0].next_cursor(), Some("output"));

        for end_cursor in [Value::Null, json!("final-row-cursor")] {
            let decoded = operation(vec![query("Gone", None, 1)], false)
                .decode(response(false, [("op0", connection(Vec::new(), false, end_cursor))]))
                .unwrap();
            assert!(decoded.pages[0].next_cursor().is_none());
        }
    }

    #[test]
    fn complete_observation_rejects_duplicate_ids_and_pull_request_identities() {
        let row = |number: u64, node_id: &str| ObservedPullRequest {
            identity: super::super::PullRequestIdentity::new(number, node_id.to_owned()).unwrap(),
            title: String::new(),
            body: String::new(),
            base_branch: "main".to_owned(),
            head_branch: "Gone".to_owned(),
            base_oid: gix::ObjectId::from_hex(BASE_OID.as_bytes()).unwrap(),
            head_oid: gix::ObjectId::from_hex(HEAD_OID.as_bytes()).unwrap(),
            state: PullRequestState::Open,
            is_cross_repository: false,
            has_auto_merge_request: false,
            is_in_merge_queue: false,
        };
        assert!(
            CompleteLocalPullRequests::new(vec![
                (id("Gone"), Vec::new()),
                (id("Gone"), Vec::new())
            ])
            .is_err()
        );
        assert!(
            CompleteLocalPullRequests::new(vec![(id("Gone"), vec![row(1, "ONE"), row(1, "TWO")])])
                .is_err()
        );
        assert!(
            CompleteLocalPullRequests::new(vec![(id("Gone"), vec![row(1, "ONE"), row(2, "ONE")])])
                .is_err()
        );
        assert!(
            CompleteLocalPullRequests::new(vec![
                (id("Gone"), vec![row(1, "ONE")]),
                (id("Gtwo"), vec![row(1, "TWO")]),
            ])
            .is_err()
        );
    }
}

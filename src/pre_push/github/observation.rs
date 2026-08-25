//! Complete GraphQL evidence for the exact local change IDs.
//!
//! A query page is inseparably bound to its requested change ID and input
//! cursor. Partial pages remain adapter evidence: only the accumulator can
//! expose a complete observation, and it does so only after every requested
//! connection is exhausted.

use std::collections::{HashMap, HashSet};

use color_eyre::eyre::{Context as _, Result, bail, eyre};
use serde::Deserialize;
use serde_json::{Value, json};

use super::{
    PullRequestIdentity, PullRequestState, Repository, RepositoryNodeId, SameRepositoryPullRequest,
};
use crate::pre_push::{
    destination::{DefaultBranch, RepositoryCoordinates},
    local::GherritPrId,
};

const MAX_DIAGNOSTIC_DETAIL_BYTES: usize = 80;

/// Escapes and bounds text received from GitHub before putting it in an error.
fn diagnostic_detail(value: &str) -> String {
    let mut rendered = String::new();
    for character in value.chars() {
        let escaped = character.escape_default().to_string();
        if rendered.len() + escaped.len() > MAX_DIAGNOSTIC_DETAIL_BYTES {
            rendered.push('…');
            break;
        }
        rendered.push_str(&escaped);
    }
    rendered
}

/// One exact local-ID connection and the page it requests next.
#[derive(Clone, Debug, Eq, PartialEq)]
struct LocalPullRequestQuery {
    id: GherritPrId,
    after: Option<String>,
    first: usize,
}

impl LocalPullRequestQuery {
    const MAX_PAGE_SIZE: usize = 100;

    fn new(id: GherritPrId, after: Option<String>, first: usize) -> Result<Self> {
        if !(1..=Self::MAX_PAGE_SIZE).contains(&first) {
            bail!("A local pull request query requires a page size between one and 100");
        }
        if after.as_deref() == Some("") {
            bail!("A local pull request query requires a nonempty pagination cursor");
        }
        Ok(Self { id, after, first })
    }
}

/// One batch of independently paginated local-ID connections.
#[derive(Clone, Debug, Eq, PartialEq)]
struct LocalPullRequests {
    coordinates: RepositoryCoordinates,
    queries: Vec<LocalPullRequestQuery>,
    include_repository_facts: bool,
}

impl LocalPullRequests {
    const MAX_ALIASES: usize = 64;

    fn new(
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

    fn document(&self) -> String {
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

    fn decode(self, response: Value) -> Result<LocalPullRequestBatch> {
        match response.get("errors") {
            None => {}
            Some(Value::Array(errors)) if errors.is_empty() => {}
            Some(_) => bail!("GitHub local pull request response contains GraphQL errors"),
        }

        let data = response
            .get("data")
            .and_then(Value::as_object)
            .ok_or_else(|| eyre!("GitHub local pull request response is missing data"))?;
        if data.len() != 1 || !data.contains_key("repository") {
            bail!("GitHub local pull request response has unexpected top-level data");
        }
        let mut repository = data["repository"].as_object().cloned().ok_or_else(|| {
            eyre!("GitHub local pull request response is missing repository data")
        })?;

        let repository_evidence = if self.include_repository_facts {
            RepositoryEvidence::Observed(decode_repository(
                &mut repository,
                self.coordinates.clone(),
            )?)
        } else {
            if repository.contains_key("id") || repository.contains_key("defaultBranchRef") {
                bail!("GitHub returned repository facts more than once");
            }
            RepositoryEvidence::Coordinates(self.coordinates.clone())
        };

        let expected =
            (0..self.queries.len()).map(|index| format!("op{index}")).collect::<HashSet<_>>();
        if let Some(alias) = repository.keys().find(|alias| !expected.contains(*alias)) {
            bail!(
                "GitHub local pull request response contains unexpected operation `{}`",
                diagnostic_detail(alias)
            );
        }
        if repository.len() != expected.len() {
            bail!("GitHub local pull request response has an incomplete alias set");
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
        Ok(LocalPullRequestBatch { repository: repository_evidence, pages })
    }
}

fn decode_repository(
    repository: &mut serde_json::Map<String, Value>,
    coordinates: RepositoryCoordinates,
) -> Result<Repository> {
    #[derive(Deserialize)]
    #[serde(deny_unknown_fields)]
    struct DefaultBranchRef {
        name: String,
        target: GitObject,
    }

    #[derive(Deserialize)]
    #[serde(deny_unknown_fields)]
    struct GitObject {
        oid: String,
    }

    let node_id = repository
        .remove("id")
        .and_then(|value| value.as_str().map(ToOwned::to_owned))
        .ok_or_else(|| eyre!("GitHub omitted the repository node ID"))?;
    let default_branch: DefaultBranchRef = serde_json::from_value(
        repository
            .remove("defaultBranchRef")
            .ok_or_else(|| eyre!("GitHub omitted the repository default branch"))?,
    )
    .wrap_err("Failed to decode the repository default branch")?;
    let tip = gix::ObjectId::from_hex(default_branch.target.oid.as_bytes())
        .wrap_err("GitHub reported an invalid default branch object ID")?;
    Ok(Repository {
        coordinates,
        node_id: RepositoryNodeId::new(node_id)?,
        default_branch: DefaultBranch::new(default_branch.name, tip)
            .wrap_err("GitHub reported an invalid default branch")?,
    })
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Connection {
    nodes: Vec<Node>,
    page_info: PageInfo,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct PageInfo {
    has_next_page: bool,
    end_cursor: Nullable<String>,
}

/// A selected nullable field which still rejects a missing response key.
#[derive(Deserialize)]
#[serde(untagged)]
enum Nullable<T> {
    Value(T),
    Null(()),
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct AutoMergeRequest {
    enabled_at: Nullable<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
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

/// The only outcomes which survive decoding one selected wire row.
///
/// A cross-repository row is relevant only because it occupies a page. Its
/// selected shape, identity, and exact requested head are validated, but none
/// of that nonlocal evidence is retained. A same-repository row retains the
/// fields which can become local authority or projection evidence.
#[derive(Debug)]
enum DecodedPullRequest {
    CrossRepository,
    SameRepository(SameRepositoryPullRequest),
}

fn decode_connection(
    query: LocalPullRequestQuery,
    connection: Value,
) -> Result<LocalPullRequestPageEvidence> {
    let connection: Connection = serde_json::from_value(connection)
        .wrap_err("Failed to decode local pull request query response")?;
    if connection.nodes.len() > query.first {
        bail!(
            "GitHub returned {} rows for '{}' after at most {} were requested",
            connection.nodes.len(),
            query.id.as_str(),
            query.first
        );
    }
    let pull_requests = connection
        .nodes
        .into_iter()
        .map(|node| decode_pull_request(&query.id, node))
        .collect::<Result<Vec<_>>>()?;
    let PageInfo { has_next_page, end_cursor } = connection.page_info;
    let end_cursor = match end_cursor {
        Nullable::Value(cursor) if cursor.is_empty() => {
            bail!("GitHub reported an empty local pull request pagination cursor")
        }
        Nullable::Value(cursor) => Some(cursor),
        Nullable::Null(()) => None,
    };
    let next_cursor = match (has_next_page, end_cursor) {
        (true, Some(cursor)) => Some(cursor),
        (true, None) => bail!(
            "GitHub reported another local pull request page for '{}' without an end cursor",
            query.id.as_str()
        ),
        (false, _) => None,
    };
    Ok(LocalPullRequestPageEvidence {
        id: query.id,
        after: query.after,
        pull_requests,
        next_cursor,
    })
}

fn decode_pull_request(id: &GherritPrId, node: Node) -> Result<DecodedPullRequest> {
    let number = u64::try_from(node.number)
        .map_err(|_| eyre!("GitHub reported an invalid pull request number {}", node.number))?;
    let identity = PullRequestIdentity::new(number, node.id)?;
    if node.head_ref_name != id.as_str() {
        bail!(
            "GitHub pull request query for '{}' returned head branch '{}'",
            id.as_str(),
            diagnostic_detail(&node.head_ref_name)
        );
    }
    if node.is_cross_repository {
        return Ok(DecodedPullRequest::CrossRepository);
    }
    if node.base_ref_name.is_empty() {
        bail!("GitHub reported an empty pull request base ref name");
    }
    let parse_oid = |field: &str, oid: &str| {
        let object_id = gix::ObjectId::from_hex(oid.as_bytes())
            .wrap_err_with(|| format!("GitHub reported an invalid {field}"))?;
        if object_id.is_null() {
            bail!("GitHub reported a null {field}");
        }
        Ok(object_id)
    };
    let has_auto_merge_request = match node.auto_merge_request {
        Nullable::Value(request) => {
            let _ = request.enabled_at;
            true
        }
        Nullable::Null(()) => false,
    };
    Ok(DecodedPullRequest::SameRepository(SameRepositoryPullRequest {
        identity,
        title: node.title,
        body: node.body,
        base_branch: node.base_ref_name,
        base_oid: parse_oid("pull request base ref object ID", &node.base_ref_oid)?,
        head_oid: parse_oid("pull request head ref object ID", &node.head_ref_oid)?,
        state: node.state,
        has_auto_merge_request,
        is_in_merge_queue: node.is_in_merge_queue,
    }))
}

/// The one repository capability carried before or after facts are observed.
#[derive(Debug)]
enum RepositoryEvidence {
    Coordinates(RepositoryCoordinates),
    Observed(Repository),
}

impl RepositoryEvidence {
    fn coordinates(&self) -> &RepositoryCoordinates {
        match self {
            Self::Coordinates(coordinates) => coordinates,
            Self::Observed(repository) => &repository.coordinates,
        }
    }
}

#[derive(Debug)]
struct LocalPullRequestBatch {
    repository: RepositoryEvidence,
    pages: Vec<LocalPullRequestPageEvidence>,
}

/// One decoded page bound to its exact requested ID and input cursor.
#[derive(Debug)]
struct LocalPullRequestPageEvidence {
    id: GherritPrId,
    after: Option<String>,
    pull_requests: Vec<DecodedPullRequest>,
    next_cursor: Option<String>,
}

#[derive(Debug)]
enum Progress {
    Initial,
    Next { cursor: String, seen: HashSet<String> },
    Exhausted,
}

impl Progress {
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
        let mut seen = match self {
            Self::Initial => HashSet::new(),
            Self::Next { seen, .. } => seen,
            Self::Exhausted => bail!(
                "Local pull request observation returned another page after exhausting '{}'",
                id.as_str()
            ),
        };
        if !seen.insert(next_cursor.clone()) {
            bail!(
                "Local pull request observation repeated a pagination cursor for '{}'",
                id.as_str()
            );
        }
        Ok(Self::Next { cursor: next_cursor, seen })
    }
}

/// Pull request identities proved to belong to this repository.
///
/// Fork rows are deliberately excluded: their identities cannot authorize a
/// later mutation of a same-repository pull request.
#[derive(Debug, Default)]
struct IdentityRegistry {
    numbers: HashSet<u64>,
    node_ids: HashSet<Box<str>>,
}

impl IdentityRegistry {
    fn insert(&mut self, identity: &PullRequestIdentity) -> Result<()> {
        if !self.numbers.insert(identity.number) {
            bail!("Local pull request observation repeats number {}", identity.number);
        }
        if !self.node_ids.insert(identity.node_id.clone()) {
            bail!(
                "Local pull request observation repeats node ID '{}'",
                diagnostic_detail(&identity.node_id)
            );
        }
        Ok(())
    }
}

/// In-progress evidence for exactly one ordered local change-ID set.
#[derive(Debug)]
struct LocalPullRequestAccumulator {
    repository: RepositoryEvidence,
    order: Box<[GherritPrId]>,
    progress: HashMap<GherritPrId, Progress>,
    rows: HashMap<GherritPrId, Vec<SameRepositoryPullRequest>>,
    identities: IdentityRegistry,
}

impl LocalPullRequestAccumulator {
    fn new(
        coordinates: RepositoryCoordinates,
        ids: impl IntoIterator<Item = GherritPrId>,
    ) -> Result<Self> {
        let mut order = Vec::new();
        let mut progress = HashMap::new();
        for id in ids {
            if progress.insert(id.clone(), Progress::Initial).is_some() {
                bail!(
                    "Local pull request observation requested change '{}' more than once",
                    id.as_str()
                );
            }
            order.push(id);
        }
        if order.is_empty() {
            bail!("Local pull request observation requires at least one change");
        }
        Ok(Self {
            repository: RepositoryEvidence::Coordinates(coordinates),
            order: order.into_boxed_slice(),
            progress,
            rows: HashMap::new(),
            identities: IdentityRegistry::default(),
        })
    }

    /// Consumes a decoded batch so an invalid partial batch cannot be reused.
    fn record_batch(mut self, batch: LocalPullRequestBatch) -> Result<Self> {
        if batch.repository.coordinates() != self.repository.coordinates() {
            bail!("Local pull request pages identify different repositories");
        }
        self.repository = match (self.repository, batch.repository) {
            (RepositoryEvidence::Coordinates(_), RepositoryEvidence::Observed(repository)) => {
                RepositoryEvidence::Observed(repository)
            }
            (RepositoryEvidence::Coordinates(_), RepositoryEvidence::Coordinates(_)) => {
                bail!("The first local pull request page omitted repository facts")
            }
            (RepositoryEvidence::Observed(_), RepositoryEvidence::Observed(_)) => {
                bail!("A later local pull request page repeated repository facts")
            }
            (repository @ RepositoryEvidence::Observed(_), RepositoryEvidence::Coordinates(_)) => {
                repository
            }
        };
        for page in batch.pages {
            self.record_page(page)?;
        }
        Ok(self)
    }

    fn record_page(&mut self, page: LocalPullRequestPageEvidence) -> Result<()> {
        let LocalPullRequestPageEvidence { id, after, pull_requests, next_cursor } = page;
        let progress = self.progress.remove(&id).ok_or_else(|| {
            eyre!("Local pull request observation returned unrequested change '{}'", id.as_str())
        })?;
        if !progress.expects(after.as_deref()) {
            bail!(
                "Local pull request observation returned an unexpected page cursor for '{}'",
                id.as_str()
            );
        }
        let rows = self.rows.entry(id.clone()).or_default();
        for pull_request in pull_requests {
            let DecodedPullRequest::SameRepository(pull_request) = pull_request else {
                continue;
            };
            self.identities.insert(&pull_request.identity)?;
            rows.push(pull_request);
        }
        let progress = progress.advance(&id, next_cursor)?;
        assert!(self.progress.insert(id, progress).is_none());
        Ok(())
    }

    fn finish(self) -> Result<CompleteLocalPullRequests> {
        let mut incomplete = self
            .progress
            .iter()
            .filter(|(_, progress)| !matches!(progress, Progress::Exhausted))
            .map(|(id, _)| id.as_str())
            .collect::<Vec<_>>();
        incomplete.sort_unstable();
        if !incomplete.is_empty() {
            bail!(
                "Local pull request observation did not exhaust change ID(s): {}",
                incomplete.join(", ")
            );
        }
        let repository = match self.repository {
            RepositoryEvidence::Observed(repository) => repository,
            RepositoryEvidence::Coordinates(_) => {
                bail!("Local pull request observation omitted repository facts")
            }
        };
        let mut rows = self.rows;
        let entries = self
            .order
            .into_vec()
            .into_iter()
            .map(|id| {
                let pull_requests = rows.remove(&id).unwrap_or_default();
                (id, pull_requests)
            })
            .collect::<Vec<_>>();
        debug_assert!(rows.is_empty());

        Ok(CompleteLocalPullRequests { repository, entries: entries.into_boxed_slice() })
    }
}

/// Repository facts and complete same-repository rows for the requested IDs.
#[derive(Debug)]
struct CompleteLocalPullRequests {
    repository: Repository,
    entries: Box<[(GherritPrId, Vec<SameRepositoryPullRequest>)]>,
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

    fn query(value: &str, after: Option<&str>, first: usize) -> LocalPullRequestQuery {
        LocalPullRequestQuery::new(id(value), after.map(str::to_owned), first).unwrap()
    }

    fn coordinates() -> RepositoryCoordinates {
        RepositoryCoordinates::for_test("owner", "repository")
    }

    fn accumulator(
        ids: impl IntoIterator<Item = GherritPrId>,
    ) -> Result<LocalPullRequestAccumulator> {
        LocalPullRequestAccumulator::new(coordinates(), ids)
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
        repository.extend(connections.into_iter().map(|(alias, value)| (alias.to_owned(), value)));
        json!({ "data": { "repository": repository } })
    }

    fn one_response(node: Value) -> Value {
        response(true, [("op0", connection(vec![node], false, Value::Null))])
    }

    #[test]
    fn document_binds_each_alias_to_one_exact_head_cursor_and_all_states() {
        let document =
            operation(vec![query("Gone", None, 17), query("Gtwo", Some("cursor:\"2"), 3)], true)
                .document();

        assert_eq!(
            document,
            concat!(
                "query { repository(owner: \"owner\", name: \"repository\") { ",
                "id, defaultBranchRef { name, target { oid } }, ",
                "op0: pullRequests(headRefName: \"Gone\", first: 17, ",
                "states: [OPEN, CLOSED, MERGED]) { nodes { number, id, ",
                "title, body, baseRefName, baseRefOid, headRefName, ",
                "headRefOid, state, isCrossRepository, autoMergeRequest { ",
                "enabledAt }, isInMergeQueue } pageInfo { hasNextPage, ",
                "endCursor } } op1: pullRequests(headRefName: \"Gtwo\", ",
                "first: 3, after: \"cursor:\\\"2\", states: [OPEN, CLOSED, ",
                "MERGED]) { nodes { number, id, title, body, baseRefName, ",
                "baseRefOid, headRefName, headRefOid, state, ",
                "isCrossRepository, autoMergeRequest { enabledAt }, ",
                "isInMergeQueue } pageInfo { hasNextPage, endCursor } } } }",
            )
        );

        let later = operation(vec![query("Gone", Some("next"), 1)], false).document();
        assert_eq!(
            later,
            concat!(
                "query { repository(owner: \"owner\", name: \"repository\") { ",
                "op0: pullRequests(headRefName: \"Gone\", first: 1, ",
                "after: \"next\", states: [OPEN, CLOSED, MERGED]) { nodes { ",
                "number, id, title, body, baseRefName, baseRefOid, ",
                "headRefName, headRefOid, state, isCrossRepository, ",
                "autoMergeRequest { enabledAt }, isInMergeQueue } pageInfo { ",
                "hasNextPage, endCursor } } } }",
            )
        );
    }

    #[test]
    fn page_cannot_exceed_its_requested_size() {
        let oversized = response(
            true,
            [(
                "op0",
                connection(
                    vec![node(1, "PR_ONE", "Gone", "OPEN"), node(2, "PR_TWO", "Gone", "OPEN")],
                    false,
                    Value::Null,
                ),
            )],
        );

        assert!(operation(vec![query("Gone", None, 1)], true).decode(oversized).is_err());
    }

    #[test]
    fn untrusted_diagnostic_details_are_escaped_and_bounded() {
        assert_eq!(diagnostic_detail("line\n\x1b'\\"), r"line\n\u{1b}\'\\");

        let rendered = diagnostic_detail(&"雪".repeat(100));
        assert!(rendered.ends_with('…'));
        assert!(rendered.len() <= MAX_DIAGNOSTIC_DETAIL_BYTES + '…'.len_utf8());
    }

    #[test]
    fn constructors_reject_empty_oversized_and_duplicate_requests() {
        assert!(LocalPullRequestQuery::new(id("Gone"), None, 0).is_err());
        assert!(LocalPullRequestQuery::new(id("Gone"), None, 101).is_err());
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
        assert!(accumulator(Vec::<GherritPrId>::new()).is_err());
        assert!(accumulator([id("Gone"), id("Gone")]).is_err());
    }

    #[test]
    fn complete_rows_retain_every_local_authority_field_and_discard_forks() {
        let opaque_body = "<!-- gherrit-meta: { not identity } -->\nmanual text";
        let mut open = node(1, "PR_OPEN", "Gone", "OPEN");
        open["body"] = json!(opaque_body);
        open["autoMergeRequest"] = json!({ "enabledAt": null });
        open["isInMergeQueue"] = json!(true);
        let mut fork = node(4, "PR_FORK", "Gone", "OPEN");
        fork["isCrossRepository"] = json!(true);
        fork["baseRefName"] = json!("");
        fork["baseRefOid"] = json!("not-an-object-id");
        fork["headRefOid"] = json!("not-an-object-id");
        let batch = operation(vec![query("Gone", None, 100)], true)
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
                        json!("last-row"),
                    ),
                )],
            ))
            .unwrap();
        let complete =
            accumulator([id("Gone")]).unwrap().record_batch(batch).unwrap().finish().unwrap();

        assert_eq!(complete.repository.node_id.as_str(), "REPOSITORY_NODE");
        assert_eq!(complete.repository.coordinates, coordinates());
        assert_eq!(
            complete.repository.default_branch,
            DefaultBranch::new(
                "main".to_owned(),
                gix::ObjectId::from_hex(DEFAULT_OID.as_bytes()).unwrap(),
            )
            .unwrap()
        );
        assert_eq!(complete.entries.len(), 1);
        let rows = &complete.entries[0].1;
        assert_eq!(rows.len(), 3);
        assert_eq!(rows[0].identity.number, 1);
        assert_eq!(rows[0].identity.node_id.as_ref(), "PR_OPEN");
        assert_eq!(rows[0].body, opaque_body);
        assert_eq!(rows[0].base_branch, "main");
        assert_eq!(rows[0].base_oid.to_string(), BASE_OID);
        assert_eq!(rows[0].head_oid.to_string(), HEAD_OID);
        assert!(rows[0].has_auto_merge_request);
        assert!(rows[0].is_in_merge_queue);
        assert_eq!(
            rows.iter().map(|row| row.state).collect::<Vec<_>>(),
            [PullRequestState::Open, PullRequestState::Closed, PullRequestState::Merged,]
        );
    }

    #[test]
    fn fork_identities_do_not_participate_in_local_identity_evidence() {
        let mut first_fork = node(1, "PR_ONE", "Gone", "OPEN");
        first_fork["isCrossRepository"] = json!(true);
        let mut second_fork = first_fork.clone();
        second_fork["state"] = json!("CLOSED");
        let batch = operation(vec![query("Gone", None, 100)], true)
            .decode(response(
                true,
                [(
                    "op0",
                    connection(
                        vec![first_fork, second_fork, node(1, "PR_ONE", "Gone", "OPEN")],
                        false,
                        Value::Null,
                    ),
                )],
            ))
            .unwrap();
        let complete =
            accumulator([id("Gone")]).unwrap().record_batch(batch).unwrap().finish().unwrap();

        let rows = &complete.entries[0].1;
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].identity.number, 1);
        assert_eq!(rows[0].identity.node_id.as_ref(), "PR_ONE");
    }

    #[test]
    fn fork_rows_still_require_valid_identity_and_the_requested_head() {
        for (pointer, replacement) in [
            ("/data/repository/op0/nodes/0/number", json!(0)),
            ("/data/repository/op0/nodes/0/number", json!(-1)),
            ("/data/repository/op0/nodes/0/id", json!("")),
            ("/data/repository/op0/nodes/0/headRefName", json!("")),
            ("/data/repository/op0/nodes/0/headRefName", json!("Other")),
        ] {
            let mut fork = node(1, "PR_ONE", "Gone", "OPEN");
            fork["isCrossRepository"] = json!(true);
            let mut value = one_response(fork);
            *value.pointer_mut(pointer).unwrap() = replacement;
            assert!(
                operation(vec![query("Gone", None, 1)], true).decode(value).is_err(),
                "accepted invalid fork field {pointer}"
            );
        }
    }

    #[test]
    fn repository_facts_are_required_exactly_once() {
        let empty_page = || connection(Vec::new(), false, Value::Null);
        assert!(
            operation(vec![query("Gone", None, 1)], true)
                .decode(response(false, [("op0", empty_page())]))
                .is_err()
        );
        assert!(
            operation(vec![query("Gone", None, 1)], false)
                .decode(response(true, [("op0", empty_page())]))
                .is_err()
        );

        for pointer in [
            "/data/repository/id",
            "/data/repository/defaultBranchRef",
            "/data/repository/defaultBranchRef/target",
            "/data/repository/defaultBranchRef/target/oid",
        ] {
            let mut value = response(true, [("op0", empty_page())]);
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
            (
                "/data/repository/defaultBranchRef/target/oid",
                json!("0000000000000000000000000000000000000000"),
            ),
        ] {
            let mut value = response(true, [("op0", empty_page())]);
            *value.pointer_mut(pointer).unwrap() = replacement;
            assert!(
                operation(vec![query("Gone", None, 1)], true).decode(value).is_err(),
                "accepted invalid {pointer}"
            );
        }

        let first = operation(vec![query("Gone", None, 1)], true)
            .decode(response(true, [("op0", empty_page())]))
            .unwrap();
        let partial = accumulator([id("Gone")]).unwrap().record_batch(first).unwrap();
        let repeated = operation(vec![query("Gone", None, 1)], true)
            .decode(response(true, [("op0", empty_page())]))
            .unwrap();
        assert!(partial.record_batch(repeated).is_err());

        let later_first = operation(vec![query("Gone", None, 1)], false)
            .decode(response(false, [("op0", empty_page())]))
            .unwrap();
        assert!(accumulator([id("Gone")]).unwrap().record_batch(later_first).is_err());
    }

    #[test]
    fn every_page_remains_bound_to_the_same_repository() {
        let other_repository = RepositoryCoordinates::for_test("owner", "other");
        let first_from_other =
            LocalPullRequests::new(other_repository.clone(), vec![query("Gone", None, 1)], true)
                .unwrap()
                .decode(response(true, [("op0", connection(Vec::new(), false, Value::Null))]))
                .unwrap();
        assert!(accumulator([id("Gone")]).unwrap().record_batch(first_from_other).is_err());

        let first = operation(vec![query("Gone", None, 1)], true)
            .decode(response(true, [("op0", connection(Vec::new(), true, json!("next")))]))
            .unwrap();
        let later =
            LocalPullRequests::new(other_repository, vec![query("Gone", Some("next"), 1)], false)
                .unwrap()
                .decode(response(false, [("op0", connection(Vec::new(), false, Value::Null))]))
                .unwrap();

        assert!(
            accumulator([id("Gone")])
                .unwrap()
                .record_batch(first)
                .unwrap()
                .record_batch(later)
                .is_err()
        );
    }

    #[test]
    fn response_requires_the_exact_alias_set_and_usable_data() {
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
        let mut null_alias = valid();
        null_alias["data"]["repository"]["op0"] = Value::Null;
        assert!(operations().decode(null_alias).is_err());
        for value in [
            json!({}),
            json!({ "data": null }),
            json!({ "data": {} }),
            json!({ "data": { "repository": null } }),
            json!({ "data": { "repository": {}, "viewer": {} } }),
        ] {
            assert!(operations().decode(value).is_err());
        }
        let mut errors = valid();
        errors["errors"] = json!([{ "message": "partial result" }]);
        assert!(operations().decode(errors).is_err());
        let mut malformed_errors = valid();
        malformed_errors["errors"] = Value::Null;
        assert!(operations().decode(malformed_errors).is_err());
        let mut empty_errors = valid();
        empty_errors["errors"] = json!([]);
        assert!(operations().decode(empty_errors).is_ok());
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
        for pointer in ["/data/repository/op0/nodes/0/title", "/data/repository/op0/nodes/0/body"] {
            let mut value = one_response(node(1, "PR_ONE", "Gone", "OPEN"));
            *value.pointer_mut(pointer).unwrap() = Value::Null;
            assert!(
                operation(vec![query("Gone", None, 1)], true).decode(value).is_err(),
                "accepted null {pointer}"
            );
        }
    }

    #[test]
    fn selected_nested_objects_reject_unrequested_fields() {
        for pointer in [
            "/data/repository/defaultBranchRef",
            "/data/repository/defaultBranchRef/target",
            "/data/repository/op0",
            "/data/repository/op0/pageInfo",
            "/data/repository/op0/nodes/0",
            "/data/repository/op0/nodes/0/autoMergeRequest",
        ] {
            let mut value = one_response(node(1, "PR_ONE", "Gone", "OPEN"));
            value["data"]["repository"]["op0"]["nodes"][0]["autoMergeRequest"] =
                json!({ "enabledAt": null });
            value
                .pointer_mut(pointer)
                .unwrap()
                .as_object_mut()
                .unwrap()
                .insert("notSelected".to_owned(), json!(true));
            assert!(
                operation(vec![query("Gone", None, 1)], true).decode(value).is_err(),
                "accepted an unrequested field at {pointer}"
            );
        }
    }

    #[test]
    fn authority_bearing_row_fields_are_validated_and_coupled() {
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
    fn pagination_is_independent_complete_and_preserves_requested_order() {
        let mut first_a = node(1, "PR_A1", "A", "OPEN");
        first_a["body"] = json!("first page");
        let first = operation(vec![query("A", None, 1)], true)
            .decode(response(true, [("op0", connection(vec![first_a], true, json!("A_NEXT")))]))
            .unwrap();
        let second = operation(vec![query("B", None, 1)], false)
            .decode(response(
                false,
                [("op0", connection(vec![node(2, "PR_B", "B", "OPEN")], false, Value::Null))],
            ))
            .unwrap();
        let third = operation(vec![query("A", Some("A_NEXT"), 1)], false)
            .decode(response(
                false,
                [("op0", connection(vec![node(3, "PR_A2", "A", "CLOSED")], false, json!("last")))],
            ))
            .unwrap();

        let partial = accumulator([id("B"), id("A")]).unwrap().record_batch(first).unwrap();
        assert!(partial.finish().is_err(), "partial pages must not become an observation");

        let first = operation(vec![query("A", None, 1)], true)
            .decode(response(
                true,
                [("op0", connection(vec![node(1, "PR_A1", "A", "OPEN")], true, json!("A_NEXT")))],
            ))
            .unwrap();
        let complete = accumulator([id("B"), id("A")])
            .unwrap()
            .record_batch(first)
            .unwrap()
            .record_batch(second)
            .unwrap()
            .record_batch(third)
            .unwrap()
            .finish()
            .unwrap();
        assert_eq!(
            complete.entries.iter().map(|(id, _)| id.as_str()).collect::<Vec<_>>(),
            ["B", "A"]
        );
        assert_eq!(complete.entries[0].1.len(), 1);
        assert_eq!(complete.entries[1].1.len(), 2);
    }

    #[test]
    fn pagination_rejects_missing_empty_repeated_and_wrong_cursors() {
        for end_cursor in [Value::Null, json!("")] {
            let value = response(true, [("op0", connection(Vec::new(), true, end_cursor))]);
            assert!(operation(vec![query("A", None, 1)], true).decode(value).is_err());
        }
        let empty_terminal = response(true, [("op0", connection(Vec::new(), false, json!("")))]);
        assert!(operation(vec![query("A", None, 1)], true).decode(empty_terminal).is_err());
        for terminal_cursor in [Value::Null, json!("last-edge")] {
            let terminal =
                response(true, [("op0", connection(Vec::new(), false, terminal_cursor))]);
            let decoded = operation(vec![query("A", None, 1)], true).decode(terminal).unwrap();
            assert!(decoded.pages[0].next_cursor.is_none());
        }

        let initial = operation(vec![query("A", None, 1)], true)
            .decode(response(true, [("op0", connection(Vec::new(), true, json!("same")))]))
            .unwrap();
        let repeated = operation(vec![query("A", Some("same"), 1)], false)
            .decode(response(false, [("op0", connection(Vec::new(), true, json!("same")))]))
            .unwrap();
        assert!(
            accumulator([id("A")])
                .unwrap()
                .record_batch(initial)
                .unwrap()
                .record_batch(repeated)
                .is_err()
        );

        let initial = operation(vec![query("A", None, 1)], true)
            .decode(response(true, [("op0", connection(Vec::new(), true, json!("expected")))]))
            .unwrap();
        let wrong = operation(vec![query("A", Some("wrong"), 1)], false)
            .decode(response(false, [("op0", connection(Vec::new(), false, Value::Null))]))
            .unwrap();
        assert!(
            accumulator([id("A")])
                .unwrap()
                .record_batch(initial)
                .unwrap()
                .record_batch(wrong)
                .is_err()
        );

        let exhausted = operation(vec![query("A", None, 1)], true)
            .decode(response(true, [("op0", connection(Vec::new(), false, Value::Null))]))
            .unwrap();
        let another = operation(vec![query("A", None, 1)], false)
            .decode(response(false, [("op0", connection(Vec::new(), false, Value::Null))]))
            .unwrap();
        assert!(
            accumulator([id("A")])
                .unwrap()
                .record_batch(exhausted)
                .unwrap()
                .record_batch(another)
                .is_err()
        );
    }

    #[test]
    fn pull_request_identities_are_unique_across_pages_and_change_ids() {
        let first = operation(vec![query("A", None, 1)], true)
            .decode(response(
                true,
                [("op0", connection(vec![node(1, "PR_ONE", "A", "OPEN")], true, json!("next")))],
            ))
            .unwrap();
        let repeated_number = operation(vec![query("A", Some("next"), 1)], false)
            .decode(response(
                false,
                [("op0", connection(vec![node(1, "PR_OTHER", "A", "CLOSED")], false, Value::Null))],
            ))
            .unwrap();
        assert!(
            accumulator([id("A")])
                .unwrap()
                .record_batch(first)
                .unwrap()
                .record_batch(repeated_number)
                .is_err()
        );

        let first = operation(vec![query("A", None, 1)], true)
            .decode(response(
                true,
                [("op0", connection(vec![node(1, "PR_ONE", "A", "OPEN")], false, Value::Null))],
            ))
            .unwrap();
        let repeated_node = operation(vec![query("B", None, 1)], false)
            .decode(response(
                false,
                [("op0", connection(vec![node(2, "PR_ONE", "B", "OPEN")], false, Value::Null))],
            ))
            .unwrap();
        assert!(
            accumulator([id("A"), id("B")])
                .unwrap()
                .record_batch(first)
                .unwrap()
                .record_batch(repeated_node)
                .is_err()
        );
    }
}

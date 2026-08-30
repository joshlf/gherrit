//! Complete GraphQL evidence for the exact local change IDs.
//!
//! Each query page is inseparably bound to its requested change ID and input
//! cursor. Rows are validated, registered, and folded as pages arrive. Raw
//! rows are never collected into a second complete representation, and no
//! correlated value is exposed until every requested connection is exhausted.

use std::{collections::HashSet, num::NonZeroUsize};

use color_eyre::eyre::{Result, bail, eyre};
use gix::ObjectId;
use serde::Deserialize;
use serde_json::{Value, json};

use super::{
    CreatePreparation, PullRequestIdentity, Repository, RepositoryNodeId,
    pull_request::PullRequestIdentityRegistry,
};
use crate::pre_push::{
    destination::{DefaultBranch, PushDestination, RepositoryCoordinates},
    json::UniqueJson,
    local::{GherritPrId, LocalStack},
};

const MAX_DIAGNOSTIC_DETAIL_BYTES: usize = 80;
const EXCESS_OBSERVATION_ROWS: usize = 99;
const MAX_DIAGNOSTIC_IDENTITIES: usize = 20;

/// Escapes and bounds an identity or wire detail before putting it in an error.
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

#[cfg(test)]
fn decode_unique_json(response: &[u8]) -> Result<Value> {
    Ok(UniqueJson::decode(response)
        .map_err(|_| eyre!("GitHub local pull request response contains malformed JSON"))?
        .into_value())
}

/// The only base kinds supported by a managed OPEN pull request.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(in crate::pre_push::publication_attempt) enum BaseKind {
    Default,
    Owned,
}

/// A classified base name coupled to the object GitHub observed for it.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(in crate::pre_push::publication_attempt) struct ObservedBase {
    kind: BaseKind,
    oid: ObjectId,
}

impl ObservedBase {
    pub(in crate::pre_push::publication_attempt) fn kind(&self) -> BaseKind {
        self.kind
    }

    pub(in crate::pre_push::publication_attempt) fn oid(&self) -> ObjectId {
        self.oid
    }

    #[cfg(test)]
    pub(in crate::pre_push::publication_attempt) fn for_plan_test(
        kind: BaseKind,
        oid: ObjectId,
    ) -> Self {
        Self { kind, oid }
    }
}

/// The history and policy evidence retained for one same-repository OPEN row.
///
/// The requested connection and enclosing value retain the head branch name,
/// so storing it again would make disagreement representable.
#[derive(Debug)]
pub(in crate::pre_push::publication_attempt) struct ManagedOpenPullRequestCandidate {
    identity: PullRequestIdentity,
    head_oid: ObjectId,
    base: ObservedBase,
    has_landing_automation: bool,
}

impl ManagedOpenPullRequestCandidate {
    fn from_observation(
        default_branch: &str,
        id: &GherritPrId,
        identity: PullRequestIdentity,
        base_name: String,
        base_oid: ObjectId,
        head_oid: ObjectId,
        has_landing_automation: bool,
    ) -> Result<Self> {
        let kind = if base_name == default_branch {
            BaseKind::Default
        } else if base_name == owned_base_name(id) {
            BaseKind::Owned
        } else {
            let id = diagnostic_detail(id.as_str());
            bail!(
                "GitHub pull request #{} for '{}' has an unsupported base",
                identity.number().get(),
                id
            );
        };
        Ok(Self {
            identity,
            head_oid,
            base: ObservedBase { kind, oid: base_oid },
            has_landing_automation,
        })
    }

    pub(in crate::pre_push::publication_attempt) fn identity(&self) -> &PullRequestIdentity {
        &self.identity
    }

    pub(in crate::pre_push::publication_attempt) fn head_oid(&self) -> ObjectId {
        self.head_oid
    }

    pub(in crate::pre_push::publication_attempt) fn base(&self) -> ObservedBase {
        self.base
    }

    pub(in crate::pre_push::publication_attempt) fn has_landing_automation(&self) -> bool {
        self.has_landing_automation
    }
}

/// All same-repository OPEN evidence for one exact local change.
///
/// Rows are stored in increasing immutable pull request number order only to
/// make the representation deterministic. That order does not grant authority
/// to any row: planning rejects more than one candidate until durable Git
/// evidence can authenticate an identity.
#[derive(Debug)]
pub(in crate::pre_push::publication_attempt) struct ManagedOpenPullRequest {
    id: GherritPrId,
    canonical: ManagedOpenPullRequestCandidate,
    title: Box<str>,
    body: Box<str>,
    is_draft: bool,
    duplicates: Box<[ManagedOpenPullRequestCandidate]>,
}

impl ManagedOpenPullRequest {
    fn from_observations(
        default_branch: &str,
        id: GherritPrId,
        mut observations: Vec<OpenPullRequest>,
    ) -> Result<Self> {
        observations.sort_unstable_by_key(|open| open.identity.number().get());
        let mut observations = observations.into_iter();
        let canonical = observations.next().expect("the nonempty OPEN collection was checked");
        let OpenPullRequest {
            identity,
            title,
            body,
            base_name,
            base_oid,
            head_oid,
            is_draft,
            has_landing_automation,
        } = canonical;
        let canonical = ManagedOpenPullRequestCandidate::from_observation(
            default_branch,
            &id,
            identity,
            base_name,
            base_oid,
            head_oid,
            has_landing_automation,
        )?;
        let duplicates = observations
            .map(|open| {
                ManagedOpenPullRequestCandidate::from_observation(
                    default_branch,
                    &id,
                    open.identity,
                    open.base_name,
                    open.base_oid,
                    open.head_oid,
                    open.has_landing_automation,
                )
            })
            .collect::<Result<Box<[_]>>>()?;
        Ok(Self {
            id,
            canonical,
            title: title.into_boxed_str(),
            body: body.into_boxed_str(),
            is_draft,
            duplicates,
        })
    }

    pub(in crate::pre_push::publication_attempt) fn id(&self) -> &GherritPrId {
        &self.id
    }

    pub(in crate::pre_push::publication_attempt) fn identity(&self) -> &PullRequestIdentity {
        self.canonical.identity()
    }

    pub(in crate::pre_push::publication_attempt) fn base(&self) -> ObservedBase {
        self.canonical.base()
    }

    #[cfg(test)]
    pub(in crate::pre_push::publication_attempt) fn head_oid(&self) -> ObjectId {
        self.canonical.head_oid()
    }

    pub(in crate::pre_push::publication_attempt) fn title(&self) -> &str {
        &self.title
    }

    pub(in crate::pre_push::publication_attempt) fn body(&self) -> &str {
        &self.body
    }

    pub(in crate::pre_push::publication_attempt) fn is_draft(&self) -> bool {
        self.is_draft
    }

    #[cfg(test)]
    pub(in crate::pre_push::publication_attempt) fn has_landing_automation(&self) -> bool {
        self.canonical.has_landing_automation()
    }

    pub(in crate::pre_push::publication_attempt) fn canonical_candidate(
        &self,
    ) -> &ManagedOpenPullRequestCandidate {
        &self.canonical
    }

    pub(in crate::pre_push::publication_attempt) fn duplicate_candidates(
        &self,
    ) -> impl Iterator<Item = &ManagedOpenPullRequestCandidate> {
        self.duplicates.iter()
    }

    pub(in crate::pre_push::publication_attempt) fn duplicate_identities(
        &self,
    ) -> impl Iterator<Item = &PullRequestIdentity> {
        self.duplicate_candidates().map(ManagedOpenPullRequestCandidate::identity)
    }

    #[cfg(test)]
    #[allow(clippy::too_many_arguments)]
    pub(in crate::pre_push::publication_attempt) fn for_plan_test(
        id: GherritPrId,
        identity: PullRequestIdentity,
        head_oid: ObjectId,
        base: ObservedBase,
        title: &str,
        body: &str,
        is_draft: bool,
        has_landing_automation: bool,
    ) -> Self {
        Self {
            id,
            canonical: ManagedOpenPullRequestCandidate {
                identity,
                head_oid,
                base,
                has_landing_automation,
            },
            title: title.into(),
            body: body.into(),
            is_draft,
            duplicates: Box::new([]),
        }
    }

    #[cfg(test)]
    pub(in crate::pre_push::publication_attempt) fn with_duplicates_for_plan_test(
        mut self,
        duplicates: Vec<(PullRequestIdentity, ObjectId, ObservedBase, bool)>,
    ) -> Self {
        let mut previous_number = self.canonical.identity.number().get();
        let mut node_ids = HashSet::from([self.canonical.identity.node_id().as_str()]);
        for (identity, ..) in &duplicates {
            assert!(
                identity.number().get() > previous_number,
                "plan-test duplicate identities must follow canonical number order"
            );
            assert!(
                node_ids.insert(identity.node_id().as_str()),
                "plan-test duplicate identities must have distinct node IDs"
            );
            previous_number = identity.number().get();
        }
        self.duplicates = duplicates
            .into_iter()
            .map(|(identity, head_oid, base, has_landing_automation)| {
                ManagedOpenPullRequestCandidate { identity, head_oid, base, has_landing_automation }
            })
            .collect();
        self
    }
}

/// Proof that an exhausted exact OPEN connection had no same-repository row.
/// Cross-repository rows do not prevent this conclusion.
#[derive(Debug)]
pub(in crate::pre_push::publication_attempt) struct AbsentPullRequest {
    id: GherritPrId,
}

impl AbsentPullRequest {
    fn after_exhaustion(id: GherritPrId) -> Self {
        Self { id }
    }

    pub(in crate::pre_push::publication_attempt) fn id(&self) -> &GherritPrId {
        &self.id
    }

    pub(super) fn into_id(self) -> GherritPrId {
        self.id
    }

    #[cfg(test)]
    pub(in crate::pre_push::publication_attempt) fn for_plan_test(id: GherritPrId) -> Self {
        Self::after_exhaustion(id)
    }
}

/// Correlated state for one local change, in exact local stack order.
#[derive(Debug)]
pub(in crate::pre_push::publication_attempt) enum LocalPullRequestObservation {
    Open(ManagedOpenPullRequest),
    Absent(AbsentPullRequest),
}

impl LocalPullRequestObservation {
    pub(in crate::pre_push::publication_attempt) fn id(&self) -> &GherritPrId {
        match self {
            Self::Open(pull_request) => pull_request.id(),
            Self::Absent(absent) => absent.id(),
        }
    }
}

/// Complete repository facts, ordered local rows, and every retained identity.
///
/// Only exhausting the request-bound accumulator can construct this value. Its
/// identity registry cannot be detached from or recombined with another local
/// observation.
#[derive(Debug)]
pub(in crate::pre_push::publication_attempt) struct CompleteLocalPullRequests {
    repository: Repository,
    local: Box<[LocalPullRequestObservation]>,
    identities: PullRequestIdentityRegistry,
}

impl CompleteLocalPullRequests {
    #[cfg(test)]
    pub(in crate::pre_push::publication_attempt) fn repository(&self) -> &Repository {
        &self.repository
    }

    #[cfg(test)]
    pub(in crate::pre_push::publication_attempt) fn local(&self) -> &[LocalPullRequestObservation] {
        &self.local
    }

    /// Consumes exact-local evidence only after binding its retained
    /// repository to the selected push destination.
    pub(in crate::pre_push::publication_attempt) fn into_planning_parts_for(
        self,
        destination: &PushDestination,
    ) -> Result<(DefaultBranch, Box<[LocalPullRequestObservation]>, CreatePreparation)> {
        if self.repository.coordinates() != destination.coordinates() {
            bail!("GitHub observation belongs to a different push repository");
        }
        let (repository_id, default_branch) = self.repository.into_create_parts();
        Ok((default_branch, self.local, CreatePreparation::new(repository_id, self.identities)))
    }

    #[cfg(test)]
    pub(in crate::pre_push::publication_attempt) fn for_plan_test(
        coordinates: RepositoryCoordinates,
        default_branch: DefaultBranch,
        local: Vec<LocalPullRequestObservation>,
        additional_identities: &[PullRequestIdentity],
    ) -> Result<Self> {
        Self::for_plan_test_with_repository_node(
            coordinates,
            default_branch,
            local,
            additional_identities,
            "REPOSITORY_NODE",
        )
    }

    #[cfg(test)]
    pub(in crate::pre_push::publication_attempt) fn for_plan_test_with_repository_node(
        coordinates: RepositoryCoordinates,
        default_branch: DefaultBranch,
        local: Vec<LocalPullRequestObservation>,
        additional_identities: &[PullRequestIdentity],
        repository_node_id: &str,
    ) -> Result<Self> {
        let mut identities = PullRequestIdentityRegistry::default();
        for observation in &local {
            if let LocalPullRequestObservation::Open(pull_request) = observation {
                for candidate in std::iter::once(pull_request.canonical_candidate())
                    .chain(pull_request.duplicate_candidates())
                {
                    identities.insert_observation(candidate.identity())?;
                }
            }
        }
        for identity in additional_identities {
            identities.insert_observation(identity)?;
        }
        Ok(Self {
            repository: Repository::for_plan_test_with_node(
                coordinates,
                default_branch,
                repository_node_id,
            ),
            local: local.into_boxed_slice(),
            identities,
        })
    }
}

fn owned_base_name(id: &GherritPrId) -> String {
    format!("gherrit-bases/{}", id.as_str())
}

/// One exact local-ID connection and the page it requests next.
#[derive(Clone, Debug, Eq, PartialEq)]
struct LocalPullRequestQuery {
    id: GherritPrId,
    after: Option<String>,
}

impl LocalPullRequestQuery {
    #[cfg(test)]
    fn new(id: GherritPrId, after: Option<String>) -> Result<Self> {
        if after.as_deref() == Some("") {
            bail!("A local pull request query requires a nonempty pagination cursor");
        }
        Ok(Self { id, after })
    }
}

/// One batch of independently paginated exact local-ID connections.
#[derive(Debug, Eq, PartialEq)]
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
                bail!(
                    "A local pull request query repeats change '{}'",
                    diagnostic_detail(query.id.as_str())
                );
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
                    "op{index}: pullRequests(headRefName: {}, first: 1{after}, states: [OPEN]) {{ nodes {{ number, id, title, body, baseRefName, baseRefOid, headRefName, headRefOid, state, isDraft, isCrossRepository, autoMergeRequest {{ enabledAt }}, isInMergeQueue }} pageInfo {{ hasNextPage, endCursor }} }}",
                    json!(query.id.as_str()),
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

    /// Decodes one bounded raw response without first collapsing duplicate JSON
    /// members.
    #[cfg(test)]
    fn decode(self, response: &[u8]) -> Result<LocalPullRequestBatch> {
        self.decode_unique_value(decode_unique_json(response)?)
    }

    fn decode_unique(self, response: UniqueJson) -> Result<LocalPullRequestBatch> {
        self.decode_unique_value(response.into_value())
    }

    fn alias_count(&self) -> NonZeroUsize {
        NonZeroUsize::new(self.queries.len()).expect("a local pull request query is nonempty")
    }

    #[cfg(test)]
    fn decode_value(self, response: Value) -> Result<LocalPullRequestBatch> {
        let response = serde_json::to_vec(&response).expect("JSON value is serializable");
        self.decode(&response)
    }

    fn decode_unique_value(self, response: Value) -> Result<LocalPullRequestBatch> {
        let response = response
            .as_object()
            .ok_or_else(|| eyre!("GitHub local pull request response is not an object"))?;
        if response.keys().any(|field| !matches!(field.as_str(), "data" | "errors" | "extensions"))
        {
            bail!("GitHub local pull request response has unexpected top-level fields");
        }
        if response.get("extensions").is_some_and(|extensions| !extensions.is_object()) {
            bail!("GitHub local pull request response has malformed extensions");
        }
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

        let repository_facts = if self.include_repository_facts {
            Some(decode_repository(&mut repository, self.coordinates.clone())?)
        } else {
            if repository.contains_key("id") || repository.contains_key("defaultBranchRef") {
                bail!("GitHub returned repository facts more than once");
            }
            None
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
        Ok(LocalPullRequestBatch {
            coordinates: self.coordinates,
            repository: repository_facts,
            pages,
        })
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
    .map_err(|_| eyre!("GitHub returned malformed repository default branch data"))?;
    let tip = gix::ObjectId::from_hex(default_branch.target.oid.as_bytes())
        .map_err(|_| eyre!("GitHub reported an invalid default branch object ID"))?;
    Ok(Repository {
        coordinates,
        node_id: RepositoryNodeId::new(node_id)?,
        default_branch: DefaultBranch::new(default_branch.name, tip)
            .map_err(|_| eyre!("GitHub reported an invalid default branch"))?,
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

/// The selected wire row keeps projection fields untyped until repository
/// ownership is known. Fork rows must have the selected shape, but their
/// irrelevant projection payload is deliberately not interpreted.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Node {
    number: i64,
    id: String,
    title: Value,
    body: Value,
    base_ref_name: Value,
    base_ref_oid: Value,
    head_ref_name: String,
    head_ref_oid: Value,
    state: WirePullRequestState,
    is_draft: bool,
    is_cross_repository: bool,
    auto_merge_request: Value,
    is_in_merge_queue: Value,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
enum WirePullRequestState {
    Open,
}

/// The only row kinds which survive wire decoding.
///
/// Cross-repository rows retain nothing. Their wire identity and exact
/// requested head are validated before decoding reaches this type, but a fork
/// is not local repository evidence and cannot constrain later creates.
#[derive(Debug)]
enum DecodedPullRequest {
    CrossRepository,
    Open(OpenPullRequest),
}

#[derive(Debug)]
struct OpenPullRequest {
    identity: PullRequestIdentity,
    title: String,
    body: String,
    base_name: String,
    base_oid: gix::ObjectId,
    head_oid: gix::ObjectId,
    is_draft: bool,
    has_landing_automation: bool,
}

fn decode_connection(
    query: LocalPullRequestQuery,
    connection: Value,
) -> Result<LocalPullRequestPageEvidence> {
    let connection: Connection = serde_json::from_value(connection)
        .map_err(|_| eyre!("GitHub returned malformed local pull request connection data"))?;
    if connection.nodes.len() > 1 {
        let id = diagnostic_detail(query.id.as_str());
        bail!(
            "GitHub returned {} rows for '{}' after exactly one was requested",
            connection.nodes.len(),
            id
        );
    }
    let row = connection
        .nodes
        .into_iter()
        .next()
        .map(|node| decode_pull_request(&query.id, node))
        .transpose()?;
    let PageInfo { has_next_page, end_cursor } = connection.page_info;
    let end = match (has_next_page, row, end_cursor) {
        (true, None, _) => {
            let id = diagnostic_detail(query.id.as_str());
            bail!("GitHub reported an empty advancing pull request page for '{id}'")
        }
        (true, Some(row), Nullable::Value(next_cursor)) if !next_cursor.is_empty() => {
            PageEnd::Advancing { row, next_cursor }
        }
        (true, Some(_), _) => {
            let id = diagnostic_detail(query.id.as_str());
            bail!(
                "GitHub reported another local pull request page for '{id}' without an end cursor"
            )
        }
        (false, row, _) => PageEnd::Exhausted { row },
    };
    Ok(LocalPullRequestPageEvidence { id: query.id, after: query.after, end })
}

fn decode_pull_request(id: &GherritPrId, node: Node) -> Result<DecodedPullRequest> {
    let number = u64::try_from(node.number)
        .map_err(|_| eyre!("GitHub reported an invalid pull request number {}", node.number))?;
    let identity = PullRequestIdentity::new(number, node.id)?;
    if node.head_ref_name != id.as_str() {
        let id = diagnostic_detail(id.as_str());
        bail!(
            "GitHub pull request query for '{}' returned head branch '{}'",
            id,
            diagnostic_detail(&node.head_ref_name)
        );
    }

    let WirePullRequestState::Open = node.state;
    if node.is_cross_repository {
        return Ok(DecodedPullRequest::CrossRepository);
    }

    let string = |field: &str, value: Value| match value {
        Value::String(value) => Ok(value),
        _ => bail!("GitHub reported an invalid {field}"),
    };
    let title = string("pull request title", node.title)?;
    let body = string("pull request body", node.body)?;
    let base_name = string("pull request base ref name", node.base_ref_name)?;
    if base_name.is_empty() {
        bail!("GitHub reported an empty pull request base ref name");
    }
    let parse_oid = |field: &str, value: Value| {
        let oid = string(field, value)?;
        let object_id = gix::ObjectId::from_hex(oid.as_bytes())
            .map_err(|_| eyre!("GitHub reported an invalid {field}"))?;
        if object_id.is_null() {
            bail!("GitHub reported a null {field}");
        }
        Ok(object_id)
    };
    let base_oid = parse_oid("pull request base ref object ID", node.base_ref_oid)?;
    let head_oid = parse_oid("pull request head ref object ID", node.head_ref_oid)?;
    let has_auto_merge_request = match node.auto_merge_request {
        Value::Null => false,
        value => {
            let request: AutoMergeRequest = serde_json::from_value(value)
                .map_err(|_| eyre!("GitHub reported an invalid pull request auto-merge request"))?;
            let _ = request.enabled_at;
            true
        }
    };
    let is_in_merge_queue = node
        .is_in_merge_queue
        .as_bool()
        .ok_or_else(|| eyre!("GitHub reported invalid pull request merge-queue state"))?;
    Ok(DecodedPullRequest::Open(OpenPullRequest {
        identity,
        title,
        body,
        base_name,
        base_oid,
        head_oid,
        is_draft: node.is_draft,
        has_landing_automation: has_auto_merge_request || is_in_merge_queue,
    }))
}

#[derive(Debug)]
struct LocalPullRequestBatch {
    coordinates: RepositoryCoordinates,
    repository: Option<Repository>,
    pages: Vec<LocalPullRequestPageEvidence>,
}

/// One decoded page bound to its exact requested ID and input cursor.
#[derive(Debug)]
struct LocalPullRequestPageEvidence {
    id: GherritPrId,
    after: Option<String>,
    end: PageEnd,
}

/// A decoded page can either exhaust its connection or advance with exactly
/// one row. The forbidden empty-advancing combination exists only while the
/// wire `PageInfo` and `nodes` fields are being decoded.
#[derive(Debug)]
enum PageEnd {
    Exhausted { row: Option<DecodedPullRequest> },
    Advancing { row: DecodedPullRequest, next_cursor: String },
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
        let diagnostic_id = || diagnostic_detail(id.as_str());
        let mut seen = match self {
            Self::Initial => HashSet::new(),
            Self::Next { seen, .. } => seen,
            Self::Exhausted => bail!(
                "Local pull request observation returned another page after exhausting '{}'",
                diagnostic_id()
            ),
        };
        if !seen.insert(next_cursor.clone()) {
            bail!(
                "Local pull request observation repeated a pagination cursor for '{}'",
                diagnostic_id()
            );
        }
        Ok(Self::Next { cursor: next_cursor, seen })
    }
}

/// Incremental correlation state for one exact local-ID connection.
#[derive(Debug)]
struct ConnectionObservation {
    progress: Progress,
    baseline_row_available: bool,
    opens: Vec<OpenPullRequest>,
}

/// One local change and the only observation state which can belong to it.
///
/// Keeping the ID beside its state preserves local order without a second
/// membership index whose contents could disagree.
#[derive(Debug)]
struct ConnectionSlot {
    id: GherritPrId,
    observation: ConnectionObservation,
}

impl Default for ConnectionObservation {
    fn default() -> Self {
        Self { progress: Progress::Initial, baseline_row_available: true, opens: Vec::new() }
    }
}

/// In-progress evidence for exactly one ordered local change-ID set.
///
/// Each requested ID owns one baseline row. All IDs then share exactly 99
/// excess rows. A large local stack therefore cannot donate unused baseline
/// rows to one pathological connection. Because every advancing page must
/// contain its one requested row, an N-ID observation accepts at most N + 99
/// rows and at most 2N + 99 pages (one final empty page per connection).
#[derive(Debug)]
pub(super) struct LocalPullRequestAccumulator {
    coordinates: RepositoryCoordinates,
    repository: Option<Repository>,
    connections: Box<[ConnectionSlot]>,
    alias_limit: NonZeroUsize,
    excess_rows_remaining: usize,
    identities: PullRequestIdentityRegistry,
}

/// The only two outcomes of consuming one exact-local accumulator.
pub(super) enum ObservationStep {
    Complete(CompleteLocalPullRequests),
    Request(PendingObservationRequest),
}

/// One immutable request inseparably coupled to the accumulator it advances.
pub(super) struct PendingObservationRequest {
    accumulator: LocalPullRequestAccumulator,
    request: LocalPullRequests,
    slots: Box<[usize]>,
}

impl PendingObservationRequest {
    pub(super) fn document(&self) -> String {
        self.request.document()
    }

    pub(super) fn alias_count(&self) -> NonZeroUsize {
        self.request.alias_count()
    }

    /// Reduces this attempt's persistent alias limit and derives the same
    /// leading logical pages again without advancing any evidence.
    pub(super) fn back_off(mut self) -> Result<Self> {
        let attempted = self.request.alias_count().get();
        self.accumulator.alias_limit = NonZeroUsize::new(attempted / 2)
            .ok_or_else(|| eyre!("A one-alias local pull request query cannot be reduced"))?;
        match self.accumulator.next()? {
            ObservationStep::Request(request) => Ok(request),
            ObservationStep::Complete(_) => {
                unreachable!("discarding a pending request cannot complete observation")
            }
        }
    }

    /// Atomically decodes and records the exact response for this request.
    pub(super) fn accept(self, response: UniqueJson) -> Result<ObservationStep> {
        let batch = self.request.decode_unique(response)?;
        self.accumulator.record_selected_batch(batch, self.slots)?.next()
    }
}

impl LocalPullRequestAccumulator {
    fn new(
        coordinates: RepositoryCoordinates,
        ids: impl IntoIterator<Item = GherritPrId>,
    ) -> Result<Self> {
        let mut connections = Vec::new();
        let mut seen = HashSet::new();
        for id in ids {
            if !seen.insert(id.clone()) {
                let id = diagnostic_detail(id.as_str());
                bail!("Local pull request observation requested change '{}' more than once", id);
            }
            connections.push(ConnectionSlot { id, observation: ConnectionObservation::default() });
        }
        if connections.is_empty() {
            bail!("Local pull request observation requires at least one change");
        }
        Ok(Self {
            coordinates,
            repository: None,
            connections: connections.into_boxed_slice(),
            alias_limit: NonZeroUsize::new(LocalPullRequests::MAX_ALIASES)
                .expect("the production alias limit is nonzero"),
            excess_rows_remaining: EXCESS_OBSERVATION_ROWS,
            identities: PullRequestIdentityRegistry::default(),
        })
    }

    pub(super) fn for_stack(
        coordinates: RepositoryCoordinates,
        local: &LocalStack,
    ) -> Result<Self> {
        Self::new(coordinates, local.iter().map(|change| change.id().clone()))
    }

    /// Derives the next immutable request from the sole retained cursor state.
    ///
    /// The alias limit is part of the consumed observation state, so a caller
    /// cannot accidentally reset backoff while progressing through pages.
    pub(super) fn next(self) -> Result<ObservationStep> {
        let selected = self
            .connections
            .iter()
            .enumerate()
            .filter_map(|(index, slot)| match &slot.observation.progress {
                Progress::Initial => {
                    Some(LocalPullRequestQuery { id: slot.id.clone(), after: None })
                        .map(|query| (index, query))
                }
                Progress::Next { cursor, .. } => {
                    Some(LocalPullRequestQuery { id: slot.id.clone(), after: Some(cursor.clone()) })
                        .map(|query| (index, query))
                }
                Progress::Exhausted => None,
            })
            .take(self.alias_limit.get())
            .collect::<Vec<_>>();
        if selected.is_empty() {
            return self.finish().map(ObservationStep::Complete);
        }
        let (slots, queries): (Vec<_>, Vec<_>) = selected.into_iter().unzip();
        let request =
            LocalPullRequests::new(self.coordinates.clone(), queries, self.repository.is_none())?;
        Ok(ObservationStep::Request(PendingObservationRequest {
            accumulator: self,
            request,
            slots: slots.into_boxed_slice(),
        }))
    }

    /// Consumes the old accumulator and returns a new one only if every page
    /// in the batch is accepted. An error therefore cannot expose the state
    /// produced by an earlier page in the same batch.
    fn record_selected_batch(
        mut self,
        batch: LocalPullRequestBatch,
        slots: Box<[usize]>,
    ) -> Result<Self> {
        if batch.coordinates != self.coordinates {
            bail!("Local pull request pages identify different repositories");
        }
        self.repository = match (self.repository, batch.repository) {
            (None, Some(repository)) => Some(repository),
            (None, None) => bail!("The first local pull request page omitted repository facts"),
            (Some(_), Some(_)) => {
                bail!("A later local pull request page repeated repository facts")
            }
            (repository @ Some(_), None) => repository,
        };
        if batch.pages.len() != slots.len() {
            bail!("Local pull request response has an incomplete page set");
        }
        slots
            .into_vec()
            .into_iter()
            .zip(batch.pages)
            .try_fold(self, |accumulator, (slot, page)| accumulator.record_page(slot, page))
    }

    /// Consuming page insertion is the atomic adapter boundary. Any mutation
    /// before a validation failure is dropped with `self`.
    fn record_page(
        mut self,
        slot_index: usize,
        page: LocalPullRequestPageEvidence,
    ) -> Result<Self> {
        let LocalPullRequestPageEvidence { id, after, end } = page;
        let slot = self.connections.get_mut(slot_index).ok_or_else(|| {
            eyre!("Local pull request observation returned an unrequested connection")
        })?;
        if slot.id != id {
            bail!(
                "Local pull request observation returned unrequested change '{}'",
                diagnostic_detail(id.as_str())
            );
        }
        let connection = &mut slot.observation;
        if !connection.progress.expects(after.as_deref()) {
            bail!(
                "Local pull request observation returned an unexpected page cursor for '{}'",
                diagnostic_detail(id.as_str())
            );
        }
        let (row, next_cursor) = match end {
            PageEnd::Exhausted { row } => (row, None),
            PageEnd::Advancing { row, next_cursor } => (Some(row), Some(next_cursor)),
        };

        if let Some(row) = row {
            if connection.baseline_row_available {
                connection.baseline_row_available = false;
            } else {
                self.excess_rows_remaining =
                    self.excess_rows_remaining.checked_sub(1).ok_or_else(|| {
                        eyre!(
                            "Local pull request observation exceeds its {} excess-row limit",
                            EXCESS_OBSERVATION_ROWS
                        )
                    })?;
            }
            match row {
                DecodedPullRequest::CrossRepository => {}
                DecodedPullRequest::Open(open) => {
                    self.identities.insert_observation(&open.identity)?;
                    connection.opens.push(open);
                }
            }
        }

        let progress = std::mem::replace(&mut connection.progress, Progress::Exhausted);
        connection.progress = progress.advance(&id, next_cursor)?;
        Ok(self)
    }

    /// Unit tests may inject already-decoded pages to exhaustively exercise
    /// correlation. Production can only record pages through a pending token.
    #[cfg(test)]
    fn record_batch(self, batch: LocalPullRequestBatch) -> Result<Self> {
        let mut selected = HashSet::new();
        let slots = batch
            .pages
            .iter()
            .map(|page| {
                let index =
                    self.connections.iter().position(|slot| slot.id == page.id).ok_or_else(
                        || {
                            eyre!(
                                "Local pull request observation returned unrequested change '{}'",
                                diagnostic_detail(page.id.as_str())
                            )
                        },
                    )?;
                if !selected.insert(index) {
                    bail!("Local pull request observation returned one connection twice");
                }
                Ok(index)
            })
            .collect::<Result<Vec<_>>>()?;
        self.record_selected_batch(batch, slots.into_boxed_slice())
    }

    fn finish(self) -> Result<CompleteLocalPullRequests> {
        let mut incomplete = self
            .connections
            .iter()
            .filter(|slot| !matches!(slot.observation.progress, Progress::Exhausted))
            .map(|slot| slot.id.as_str())
            .collect::<Vec<_>>();
        incomplete.sort_unstable();
        if !incomplete.is_empty() {
            let total = incomplete.len();
            let displayed = incomplete
                .into_iter()
                .take(MAX_DIAGNOSTIC_IDENTITIES)
                .map(diagnostic_detail)
                .collect::<Vec<_>>()
                .join(", ");
            let omitted = total.saturating_sub(MAX_DIAGNOSTIC_IDENTITIES);
            if omitted == 0 {
                bail!("Local pull request observation did not exhaust change ID(s): {displayed}");
            }
            bail!(
                "Local pull request observation did not exhaust change ID(s): {displayed}; additional change IDs omitted: {omitted}"
            );
        }
        let repository = self
            .repository
            .ok_or_else(|| eyre!("Local pull request observation omitted repository facts"))?;
        let default_branch = repository.default_branch().name();
        let mut local = Vec::with_capacity(self.connections.len());

        for ConnectionSlot { id, observation: connection } in self.connections.into_vec() {
            if !connection.opens.is_empty() {
                local.push(LocalPullRequestObservation::Open(
                    ManagedOpenPullRequest::from_observations(
                        default_branch,
                        id,
                        connection.opens,
                    )?,
                ));
            } else {
                local.push(LocalPullRequestObservation::Absent(
                    AbsentPullRequest::after_exhaustion(id),
                ));
            }
        }

        Ok(CompleteLocalPullRequests {
            repository,
            local: local.into_boxed_slice(),
            identities: self.identities,
        })
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

    fn query(value: &str, after: Option<&str>) -> LocalPullRequestQuery {
        LocalPullRequestQuery::new(id(value), after.map(str::to_owned)).unwrap()
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
            "isDraft": true,
            "isCrossRepository": false,
            "autoMergeRequest": null,
            "isInMergeQueue": false,
        })
    }

    fn fork(number: i64, node_id: &str, head: &str) -> Value {
        let mut node = node(number, node_id, head, "OPEN");
        node["isCrossRepository"] = json!(true);
        node
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
        connections: impl IntoIterator<Item = (String, Value)>,
    ) -> Value {
        let mut repository = serde_json::Map::new();
        if include_repository_facts {
            repository.insert("id".to_owned(), json!("REPOSITORY_NODE"));
            repository.insert(
                "defaultBranchRef".to_owned(),
                json!({ "name": "main", "target": { "oid": DEFAULT_OID } }),
            );
        }
        repository.extend(connections);
        json!({ "data": { "repository": repository } })
    }

    fn one_response(node: Value) -> Value {
        response(true, [("op0".to_owned(), connection(vec![node], false, Value::Null))])
    }

    fn decoded_page(
        id: &str,
        after: Option<&str>,
        row: Option<Value>,
        next: Option<&str>,
        include_repository_facts: bool,
    ) -> LocalPullRequestBatch {
        operation(vec![query(id, after)], include_repository_facts)
            .decode_value(response(
                include_repository_facts,
                [(
                    "op0".to_owned(),
                    connection(
                        row.into_iter().collect(),
                        next.is_some(),
                        next.map_or(Value::Null, |cursor| json!(cursor)),
                    ),
                )],
            ))
            .unwrap()
    }

    fn record_rows(
        mut accumulator: LocalPullRequestAccumulator,
        id: &str,
        start: usize,
        count: usize,
        finish_connection: bool,
        include_repository_facts: bool,
    ) -> Result<LocalPullRequestAccumulator> {
        let mut after: Option<String> = None;
        for offset in 0..count {
            let index = start + offset;
            let next =
                (!finish_connection || offset + 1 != count).then(|| format!("cursor-{id}-{index}"));
            let row = fork(i64::try_from(index + 1).unwrap(), &format!("NODE-{id}-{index}"), id);
            let batch = decoded_page(
                id,
                after.as_deref(),
                Some(row),
                next.as_deref(),
                include_repository_facts && offset == 0,
            );
            accumulator = accumulator.record_batch(batch)?;
            after = next;
        }
        Ok(accumulator)
    }

    fn kinds(complete: &CompleteLocalPullRequests) -> Vec<(&str, &'static str)> {
        complete
            .local()
            .iter()
            .map(|observation| {
                (
                    observation.id().as_str(),
                    match observation {
                        LocalPullRequestObservation::Open(_) => "open",
                        LocalPullRequestObservation::Absent(_) => "absent",
                    },
                )
            })
            .collect()
    }

    #[test]
    fn document_binds_fixed_one_row_pages_to_exact_heads_and_cursors() {
        let document =
            operation(vec![query("Gone", None), query("Gtwo", Some("cursor:\"2"))], true)
                .document();
        insta::assert_snapshot!("initial_exact_local_query", document);

        let later = operation(vec![query("Gone", Some("next"))], false).document();
        insta::assert_snapshot!("later_exact_local_query", later);
    }

    #[test]
    fn resource_backoff_is_persistent_and_retains_repository_fact_authority() {
        let ObservationStep::Request(initial) =
            accumulator([id("A"), id("B"), id("C"), id("D")]).unwrap().next().unwrap()
        else {
            panic!("a nonempty observation starts with a request");
        };
        assert_eq!(initial.alias_count().get(), 4);
        assert!(initial.document().contains("defaultBranchRef"));

        let first_half = initial.back_off().unwrap();
        assert_eq!(first_half.alias_count().get(), 2);
        assert!(first_half.document().contains("headRefName: \"A\""));
        assert!(first_half.document().contains("headRefName: \"B\""));
        assert!(!first_half.document().contains("headRefName: \"C\""));
        assert!(first_half.document().contains("defaultBranchRef"));

        let first_response = response(
            true,
            [
                ("op0".to_owned(), connection(Vec::new(), false, Value::Null)),
                ("op1".to_owned(), connection(Vec::new(), false, Value::Null)),
            ],
        );
        let first_response =
            UniqueJson::decode(&serde_json::to_vec(&first_response).unwrap()).unwrap();
        let ObservationStep::Request(second_half) = first_half.accept(first_response).unwrap()
        else {
            panic!("two local connections remain");
        };
        assert_eq!(second_half.alias_count().get(), 2);
        assert!(second_half.document().contains("headRefName: \"C\""));
        assert!(second_half.document().contains("headRefName: \"D\""));
        assert!(!second_half.document().contains("defaultBranchRef"));

        let second_response = response(
            false,
            [
                ("op0".to_owned(), connection(Vec::new(), false, Value::Null)),
                ("op1".to_owned(), connection(Vec::new(), false, Value::Null)),
            ],
        );
        let second_response =
            UniqueJson::decode(&serde_json::to_vec(&second_response).unwrap()).unwrap();
        let ObservationStep::Complete(complete) = second_half.accept(second_response).unwrap()
        else {
            panic!("all local connections were exhausted");
        };
        assert_eq!(
            kinds(&complete),
            [("A", "absent"), ("B", "absent"), ("C", "absent"), ("D", "absent")]
        );
    }

    #[test]
    fn constructors_reject_unusable_or_ambiguous_requests() {
        assert!(LocalPullRequestQuery::new(id("Gone"), Some(String::new())).is_err());
        assert!(LocalPullRequests::new(coordinates(), Vec::new(), true).is_err());
        assert!(
            LocalPullRequests::new(
                coordinates(),
                (0..=LocalPullRequests::MAX_ALIASES)
                    .map(|index| query(&format!("G{index}"), None))
                    .collect(),
                true,
            )
            .is_err()
        );
        assert!(
            LocalPullRequests::new(
                coordinates(),
                vec![query("Gone", None), query("Gone", Some("next"))],
                true,
            )
            .is_err()
        );
        assert!(accumulator(Vec::<GherritPrId>::new()).is_err());
        assert!(accumulator([id("Gone"), id("Gone")]).is_err());
    }

    #[test]
    fn one_row_wire_limit_is_exact() {
        let oversized = response(
            true,
            [(
                "op0".to_owned(),
                connection(
                    vec![node(1, "ONE", "G", "OPEN"), node(2, "TWO", "G", "CLOSED")],
                    false,
                    Value::Null,
                ),
            )],
        );
        assert!(operation(vec![query("G", None)], true).decode_value(oversized).is_err());
    }

    #[test]
    fn empty_terminal_pages_are_valid_but_empty_advancing_pages_are_not() {
        for terminal_cursor in [Value::Null, json!(""), json!("unused")] {
            let terminal = response(
                true,
                [("op0".to_owned(), connection(Vec::new(), false, terminal_cursor))],
            );
            assert!(operation(vec![query("G", None)], true).decode_value(terminal).is_ok());
        }
        for advancing_cursor in [Value::Null, json!(""), json!("next")] {
            let advancing = response(
                true,
                [("op0".to_owned(), connection(Vec::new(), true, advancing_cursor))],
            );
            assert!(operation(vec![query("G", None)], true).decode_value(advancing).is_err());
        }
    }

    #[test]
    fn one_id_accepts_its_baseline_plus_exactly_99_excess_rows() {
        let accumulator =
            record_rows(accumulator([id("G")]).unwrap(), "G", 0, 100, true, true).unwrap();
        let complete = accumulator.finish().unwrap();
        assert_eq!(kinds(&complete), [("G", "absent")]);
        assert_eq!(complete.identities.lengths(), (0, 0));
    }

    #[test]
    fn the_100th_excess_row_is_rejected() {
        let accumulator =
            record_rows(accumulator([id("G")]).unwrap(), "G", 0, 100, false, true).unwrap();
        let over_limit =
            decoded_page("G", Some("cursor-G-99"), Some(fork(101, "NODE-G-100", "G")), None, false);
        assert!(accumulator.record_batch(over_limit).is_err());
    }

    #[test]
    fn baselines_are_per_id_and_shared_excess_is_observation_wide() {
        let ids = [id("A"), id("B")];
        let within_limit = record_rows(accumulator(ids).unwrap(), "A", 0, 100, true, true).unwrap();
        let within_limit = record_rows(within_limit, "B", 1000, 1, true, false).unwrap();
        assert_eq!(kinds(&within_limit.finish().unwrap()), [("A", "absent"), ("B", "absent")]);

        let ids = [id("A"), id("B")];
        let accumulator = record_rows(accumulator(ids).unwrap(), "A", 0, 100, true, true).unwrap();
        let accumulator = record_rows(accumulator, "B", 1000, 1, false, false).unwrap();
        let over_limit = decoded_page(
            "B",
            Some("cursor-B-1000"),
            Some(fork(1002, "NODE-B-1001", "B")),
            None,
            false,
        );
        assert!(accumulator.record_batch(over_limit).is_err());
    }

    #[test]
    fn a_large_stack_cannot_donate_unused_baselines_to_one_head() {
        let ids = (0..500).map(|index| id(&format!("G{index}"))).collect::<Vec<_>>();
        let accumulator =
            record_rows(accumulator(ids).unwrap(), "G0", 0, 100, false, true).unwrap();
        let over_limit = decoded_page(
            "G0",
            Some("cursor-G0-99"),
            Some(fork(101, "NODE-G0-100", "G0")),
            None,
            false,
        );
        assert!(accumulator.record_batch(over_limit).is_err());
    }

    #[test]
    fn maximum_one_id_page_chain_includes_a_final_empty_page() {
        let accumulator =
            record_rows(accumulator([id("G")]).unwrap(), "G", 0, 100, false, true).unwrap();
        let terminal = decoded_page("G", Some("cursor-G-99"), None, None, false);
        let complete = accumulator.record_batch(terminal).unwrap().finish().unwrap();
        assert_eq!(kinds(&complete), [("G", "absent")]);
    }

    #[test]
    fn cursor_repetition_rejects_a_long_chain() {
        let accumulator =
            record_rows(accumulator([id("G")]).unwrap(), "G", 0, 99, false, true).unwrap();
        let repeated = decoded_page(
            "G",
            Some("cursor-G-98"),
            Some(fork(100, "NODE-G-99", "G")),
            Some("cursor-G-10"),
            false,
        );
        assert!(accumulator.record_batch(repeated).is_err());
    }

    #[test]
    fn consuming_batch_recording_cannot_return_partial_over_limit_state() {
        let ids = [id("A"), id("B"), id("C")];
        let accumulator = record_rows(accumulator(ids).unwrap(), "A", 0, 100, true, true).unwrap();
        let accumulator = record_rows(accumulator, "B", 1000, 1, false, false).unwrap();
        let batch = LocalPullRequestBatch {
            coordinates: coordinates(),
            repository: None,
            pages: vec![
                LocalPullRequestPageEvidence {
                    id: id("C"),
                    after: None,
                    end: PageEnd::Exhausted { row: Some(DecodedPullRequest::CrossRepository) },
                },
                LocalPullRequestPageEvidence {
                    id: id("B"),
                    after: Some("cursor-B-1000".to_owned()),
                    end: PageEnd::Exhausted { row: Some(DecodedPullRequest::CrossRepository) },
                },
            ],
        };
        assert!(accumulator.record_batch(batch).is_err());
        // `record_batch` consumed the only accumulator; no partially recorded
        // value exists which a caller could finish or reuse.
    }

    #[test]
    fn fork_identities_do_not_participate_in_local_identity_evidence() {
        let first = decoded_page("G", None, Some(fork(1, "ONE", "G")), Some("one"), true);
        let second = decoded_page("G", Some("one"), Some(fork(1, "ONE", "G")), Some("two"), false);
        let third = decoded_page("G", Some("two"), Some(node(1, "ONE", "G", "OPEN")), None, false);
        let complete = accumulator([id("G")])
            .unwrap()
            .record_batch(first)
            .unwrap()
            .record_batch(second)
            .unwrap()
            .record_batch(third)
            .unwrap()
            .finish()
            .unwrap();

        assert_eq!(kinds(&complete), [("G", "open")]);
        assert_eq!(complete.identities.number_values(), HashSet::from([1]));
        assert_eq!(complete.identities.node_id_values(), HashSet::from(["ONE"]));
    }

    #[test]
    fn fork_only_connection_yields_sealed_absence() {
        let mut foreign = fork(1, "FORK", "G");
        foreign["title"] = Value::Null;
        foreign["body"] = json!(17);
        foreign["baseRefName"] = json!([]);
        foreign["baseRefOid"] = json!({});
        foreign["headRefOid"] = json!(false);
        foreign["autoMergeRequest"] = json!("ignored");
        foreign["isInMergeQueue"] = Value::Null;
        let batch = decoded_page("G", None, Some(foreign), None, true);
        let complete =
            accumulator([id("G")]).unwrap().record_batch(batch).unwrap().finish().unwrap();
        assert_eq!(kinds(&complete), [("G", "absent")]);
    }

    #[test]
    fn multiple_opens_are_retained_in_deterministic_number_order() {
        let first = decoded_page("G", None, Some(node(2, "TWO", "G", "OPEN")), Some("next"), true);
        let second =
            decoded_page("G", Some("next"), Some(node(1, "ONE", "G", "OPEN")), None, false);
        let complete = accumulator([id("G")])
            .unwrap()
            .record_batch(first)
            .unwrap()
            .record_batch(second)
            .unwrap()
            .finish()
            .unwrap();
        let LocalPullRequestObservation::Open(open) = &complete.local()[0] else {
            panic!("G must have OPEN rows");
        };
        assert_eq!(open.identity().number().get(), 1);
        assert_eq!(
            open.duplicate_identities().map(|identity| identity.number().get()).collect::<Vec<_>>(),
            [2]
        );
    }

    #[test]
    fn identity_number_node_and_pair_collisions_reject_across_ids_and_pages() {
        for (second_number, second_node) in [(1, "TWO"), (2, "ONE"), (1, "ONE")] {
            let first = decoded_page("A", None, Some(node(1, "ONE", "A", "OPEN")), None, true);
            let second = decoded_page(
                "B",
                None,
                Some(node(second_number, second_node, "B", "OPEN")),
                None,
                false,
            );
            assert!(
                accumulator([id("A"), id("B")])
                    .unwrap()
                    .record_batch(first)
                    .unwrap()
                    .record_batch(second)
                    .is_err(),
                "number={second_number}, node={second_node}"
            );
        }

        for (second_number, second_node) in [(1, "TWO"), (2, "ONE"), (1, "ONE")] {
            let first =
                decoded_page("A", None, Some(node(1, "ONE", "A", "OPEN")), Some("next"), true);
            let second = decoded_page(
                "A",
                Some("next"),
                Some(node(second_number, second_node, "A", "OPEN")),
                None,
                false,
            );
            assert!(
                accumulator([id("A")])
                    .unwrap()
                    .record_batch(first)
                    .unwrap()
                    .record_batch(second)
                    .is_err(),
                "number={second_number}, node={second_node}"
            );
        }
    }

    #[test]
    fn fork_projection_fields_are_not_interpreted() {
        let mut row = fork(1, "FORK", "G");
        for field in [
            "title",
            "body",
            "baseRefName",
            "baseRefOid",
            "headRefOid",
            "autoMergeRequest",
            "isInMergeQueue",
        ] {
            row[field] = json!({ "arbitrary": [null, 17] });
        }
        assert!(operation(vec![query("G", None)], true).decode_value(one_response(row)).is_ok());
    }

    #[test]
    fn rows_outside_the_requested_open_lifecycle_are_rejected() {
        for state in ["CLOSED", "MERGED"] {
            let row = node(1, "ONE", "G", state);
            assert!(
                operation(vec![query("G", None)], true).decode_value(one_response(row)).is_err(),
                "accepted {state} despite the OPEN-only query"
            );
        }
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
            let mut value = one_response(fork(1, "ONE", "G"));
            *value.pointer_mut(pointer).unwrap() = replacement;
            assert!(
                operation(vec![query("G", None)], true).decode_value(value).is_err(),
                "accepted invalid fork field {pointer}"
            );
        }
    }

    #[test]
    fn every_row_validates_identity_schema_state_and_requested_head() {
        let cases = [
            ("/data/repository/op0/nodes/0/number", json!(0)),
            ("/data/repository/op0/nodes/0/number", json!(-1)),
            ("/data/repository/op0/nodes/0/number", json!(i64::from(i32::MAX) + 1)),
            ("/data/repository/op0/nodes/0/id", json!("")),
            ("/data/repository/op0/nodes/0/headRefName", json!("")),
            ("/data/repository/op0/nodes/0/headRefName", json!("Other")),
            ("/data/repository/op0/nodes/0/state", json!("DRAFT")),
            ("/data/repository/op0/nodes/0/isDraft", Value::Null),
            ("/data/repository/op0/nodes/0/isCrossRepository", json!(null)),
        ];
        for (pointer, replacement) in &cases {
            let mut value = one_response(node(1, "ONE", "G", "OPEN"));
            *value.pointer_mut(pointer).unwrap() = replacement.clone();
            assert!(
                operation(vec![query("G", None)], true).decode_value(value).is_err(),
                "pointer={pointer}"
            );
        }

        for field in [
            "number",
            "id",
            "title",
            "body",
            "baseRefName",
            "baseRefOid",
            "headRefName",
            "headRefOid",
            "state",
            "isDraft",
            "isCrossRepository",
            "autoMergeRequest",
            "isInMergeQueue",
        ] {
            let mut value = one_response(fork(1, "ONE", "G"));
            value["data"]["repository"]["op0"]["nodes"][0].as_object_mut().unwrap().remove(field);
            assert!(
                operation(vec![query("G", None)], true).decode_value(value).is_err(),
                "missing {field}"
            );
        }
    }

    #[test]
    fn open_projection_fields_are_validated_and_minimized() {
        let cases = [
            ("/data/repository/op0/nodes/0/title", Value::Null),
            ("/data/repository/op0/nodes/0/body", json!(17)),
            ("/data/repository/op0/nodes/0/baseRefName", json!("")),
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
            ("/data/repository/op0/nodes/0/autoMergeRequest", json!({})),
            ("/data/repository/op0/nodes/0/isInMergeQueue", json!("no")),
        ];
        for (pointer, replacement) in cases {
            let mut value = one_response(node(1, "ONE", "G", "OPEN"));
            *value.pointer_mut(pointer).unwrap() = replacement;
            assert!(
                operation(vec![query("G", None)], true).decode_value(value).is_err(),
                "accepted invalid {pointer}"
            );
        }

        for (auto_merge_request, has_auto_merge_request) in [
            (Value::Null, false),
            (json!({ "enabledAt": "now" }), true),
            (json!({ "enabledAt": null }), true),
        ] {
            for in_queue in [false, true] {
                let mut open = node(1, "ONE", "G", "OPEN");
                open["autoMergeRequest"] = auto_merge_request.clone();
                open["isInMergeQueue"] = json!(in_queue);
                let complete = accumulator([id("G")])
                    .unwrap()
                    .record_batch(decoded_page("G", None, Some(open), None, true))
                    .unwrap()
                    .finish()
                    .unwrap();
                let LocalPullRequestObservation::Open(open) = &complete.local()[0] else {
                    panic!("G must be open");
                };
                assert_eq!(open.has_landing_automation(), has_auto_merge_request || in_queue);
            }
        }
    }

    #[test]
    fn unsupported_open_base_rejects_only_after_complete_observation() {
        let mut open = node(1, "ONE", "G", "OPEN");
        open["baseRefName"] = json!("feature");
        let batch = decoded_page("G", None, Some(open), None, true);
        let accumulator = accumulator([id("G")]).unwrap().record_batch(batch).unwrap();
        assert!(accumulator.finish().is_err());
    }

    #[test]
    fn output_preserves_exact_local_order_and_complete_repository_facts() {
        let first = operation(vec![query("A", None), query("C", None)], true)
            .decode_value(response(
                true,
                [
                    ("op0".to_owned(), connection(Vec::new(), false, Value::Null)),
                    ("op1".to_owned(), connection(vec![fork(3, "C", "C")], false, Value::Null)),
                ],
            ))
            .unwrap();
        let second = decoded_page("B", None, Some(node(2, "B", "B", "OPEN")), None, false);
        let complete = accumulator([id("C"), id("A"), id("B")])
            .unwrap()
            .record_batch(first)
            .unwrap()
            .record_batch(second)
            .unwrap()
            .finish()
            .unwrap();

        assert_eq!(kinds(&complete), [("C", "absent"), ("A", "absent"), ("B", "open")]);
        assert_eq!(complete.repository().coordinates(), &coordinates());
        assert_eq!(complete.repository().node_id().as_str(), "REPOSITORY_NODE");
        assert_eq!(complete.repository().default_branch().name(), "main");
        assert_eq!(complete.repository().default_branch().tip().to_string(), DEFAULT_OID);
    }

    #[test]
    fn repository_facts_are_required_once_and_every_page_matches_coordinates() {
        let empty = || connection(Vec::new(), false, Value::Null);
        assert!(
            operation(vec![query("G", None)], true)
                .decode_value(response(false, [("op0".to_owned(), empty())]))
                .is_err()
        );
        assert!(
            operation(vec![query("G", None)], false)
                .decode_value(response(true, [("op0".to_owned(), empty())]))
                .is_err()
        );

        let first = decoded_page("G", None, None, None, true);
        let repeated = decoded_page("G", None, None, None, true);
        assert!(
            accumulator([id("G")])
                .unwrap()
                .record_batch(first)
                .unwrap()
                .record_batch(repeated)
                .is_err()
        );

        let other = RepositoryCoordinates::for_test("owner", "other");
        let foreign = LocalPullRequests::new(other, vec![query("G", None)], true)
            .unwrap()
            .decode_value(response(true, [("op0".to_owned(), empty())]))
            .unwrap();
        assert!(accumulator([id("G")]).unwrap().record_batch(foreign).is_err());
    }

    #[test]
    fn partial_connections_wrong_cursors_and_pages_after_exhaustion_reject() {
        let partial = accumulator([id("A"), id("B")])
            .unwrap()
            .record_batch(decoded_page("A", None, None, None, true))
            .unwrap();
        assert!(partial.finish().is_err());

        let initial = decoded_page("G", None, Some(fork(1, "ONE", "G")), Some("expected"), true);
        let wrong = decoded_page("G", Some("wrong"), None, None, false);
        assert!(
            accumulator([id("G")])
                .unwrap()
                .record_batch(initial)
                .unwrap()
                .record_batch(wrong)
                .is_err()
        );

        let exhausted = decoded_page("G", None, None, None, true);
        let another = decoded_page("G", None, None, None, false);
        assert!(
            accumulator([id("G")])
                .unwrap()
                .record_batch(exhausted)
                .unwrap()
                .record_batch(another)
                .is_err()
        );
    }

    #[test]
    fn incomplete_connection_diagnostic_is_bounded() {
        let ids = (0..MAX_DIAGNOSTIC_IDENTITIES + 7)
            .map(|index| id(&format!("G{}{}", index, "x".repeat(100))))
            .collect::<Vec<_>>();
        let error = accumulator(ids).unwrap().finish().unwrap_err().to_string();
        assert!(error.contains("additional change IDs omitted: 7"));
        assert!(error.contains('…'));
        assert!(error.len() < 2_500, "diagnostic was {} bytes", error.len());
    }

    #[test]
    fn raw_json_rejects_duplicate_members_at_every_authority_depth() {
        let valid = serde_json::to_string(&one_response(node(1, "ONE", "G", "OPEN"))).unwrap();
        let insert_before = |needle: &str, member: &str| {
            let index = valid.find(needle).unwrap_or_else(|| panic!("missing {needle}"));
            let mut response = valid.clone();
            response.insert_str(index, member);
            response
        };
        let duplicates = [
            format!("{{\"data\":null,{}", &valid[1..]),
            format!("{{\"errors\":[],\"errors\":[],{}", &valid[1..]),
            insert_before("\"op0\":", "\"op0\":null,"),
            insert_before("\"number\":1", "\"number\":2,"),
            insert_before("\"id\":\"ONE\"", "\"id\":\"OTHER\","),
            insert_before("\"state\":\"OPEN\"", "\"state\":\"CLOSED\","),
            insert_before("\"hasNextPage\":false", "\"hasNextPage\":true,"),
        ];
        for response in duplicates {
            let error = operation(vec![query("G", None)], true)
                .decode(response.as_bytes())
                .unwrap_err()
                .to_string();
            assert_eq!(
                error, "GitHub local pull request response contains malformed JSON",
                "response={response}"
            );
        }
    }

    #[test]
    fn raw_json_rejects_null_alias_trailing_data_and_untrusted_schema_keys() {
        let mut null_alias = one_response(node(1, "ONE", "G", "OPEN"));
        null_alias["data"]["repository"]["op0"] = Value::Null;
        let null_alias = serde_json::to_vec(&null_alias).unwrap();
        let error =
            operation(vec![query("G", None)], true).decode(&null_alias).unwrap_err().to_string();
        assert_eq!(error, "GitHub returned malformed local pull request connection data");

        let mut trailing = serde_json::to_vec(&one_response(node(1, "ONE", "G", "OPEN"))).unwrap();
        trailing.extend_from_slice(b" null");
        let error =
            operation(vec![query("G", None)], true).decode(&trailing).unwrap_err().to_string();
        assert_eq!(error, "GitHub local pull request response contains malformed JSON");

        let mut unknown = one_response(node(1, "ONE", "G", "OPEN"));
        let key = format!("{}\n\x1b[31m", "untrusted".repeat(200));
        unknown["data"]["repository"]["op0"]["nodes"][0]
            .as_object_mut()
            .unwrap()
            .insert(key.clone(), json!(true));
        let unknown = serde_json::to_vec(&unknown).unwrap();
        let error = operation(vec![query("G", None)], true).decode(&unknown).unwrap_err();
        assert_eq!(
            error.to_string(),
            "GitHub returned malformed local pull request connection data"
        );
        let rendered = format!("{error:?}");
        assert!(!rendered.contains("untrusted"));
    }

    #[test]
    fn raw_json_preserves_the_parser_recursion_limit() {
        let depth = 256;
        let response = format!("{}null{}", "[".repeat(depth), "]".repeat(depth));
        let error = decode_unique_json(response.as_bytes()).unwrap_err().to_string();
        assert_eq!(error, "GitHub local pull request response contains malformed JSON");
    }

    #[test]
    fn response_requires_exact_aliases_schema_and_usable_data() {
        let operations = || operation(vec![query("A", None), query("B", None)], true);
        let valid = || {
            response(
                true,
                [
                    ("op0".to_owned(), connection(Vec::new(), false, Value::Null)),
                    ("op1".to_owned(), connection(Vec::new(), false, Value::Null)),
                ],
            )
        };
        assert!(operations().decode_value(valid()).is_ok());
        for alias in ["op0", "op1"] {
            let mut value = valid();
            value["data"]["repository"].as_object_mut().unwrap().remove(alias);
            assert!(operations().decode_value(value).is_err());
        }
        let mut extra = valid();
        extra["data"]["repository"]["op2"] = connection(Vec::new(), false, Value::Null);
        assert!(operations().decode_value(extra).is_err());
        for value in [
            json!({}),
            json!({ "data": null }),
            json!({ "data": {} }),
            json!({ "data": { "repository": null } }),
            json!({ "data": { "repository": {}, "viewer": {} } }),
        ] {
            assert!(operations().decode_value(value).is_err());
        }
        let mut errors = valid();
        errors["errors"] = json!([{ "message": "partial result" }]);
        assert!(operations().decode_value(errors).is_err());
        let mut empty_errors = valid();
        empty_errors["errors"] = json!([]);
        assert!(operations().decode_value(empty_errors).is_ok());
        let mut extensions = valid();
        extensions["extensions"] = json!({ "requestId": "opaque" });
        assert!(operations().decode_value(extensions).is_ok());
        let mut malformed_extensions = valid();
        malformed_extensions["extensions"] = json!(null);
        assert!(operations().decode_value(malformed_extensions).is_err());
        let mut unexpected_top_level = valid();
        unexpected_top_level["unexpected"] = json!(true);
        assert!(operations().decode_value(unexpected_top_level).is_err());

        for pointer in [
            "/data/repository/op0/nodes",
            "/data/repository/op0/pageInfo",
            "/data/repository/op0/pageInfo/hasNextPage",
            "/data/repository/op0/pageInfo/endCursor",
        ] {
            let mut value = one_response(node(1, "ONE", "G", "OPEN"));
            let (parent, field) = pointer.rsplit_once('/').unwrap();
            value.pointer_mut(parent).unwrap().as_object_mut().unwrap().remove(field);
            assert!(
                operation(vec![query("G", None)], true).decode_value(value).is_err(),
                "accepted missing {pointer}"
            );
        }

        for pointer in [
            "/data/repository/defaultBranchRef",
            "/data/repository/defaultBranchRef/target",
            "/data/repository/op0",
            "/data/repository/op0/pageInfo",
            "/data/repository/op0/nodes/0",
            "/data/repository/op0/nodes/0/autoMergeRequest",
        ] {
            let mut value = one_response(node(1, "ONE", "G", "OPEN"));
            value["data"]["repository"]["op0"]["nodes"][0]["autoMergeRequest"] =
                json!({ "enabledAt": "now" });
            value
                .pointer_mut(pointer)
                .unwrap()
                .as_object_mut()
                .unwrap()
                .insert("notSelected".to_owned(), json!(true));
            assert!(operation(vec![query("G", None)], true).decode_value(value).is_err());
        }
    }

    #[test]
    fn repository_facts_are_strict_and_complete() {
        for pointer in [
            "/data/repository/id",
            "/data/repository/defaultBranchRef",
            "/data/repository/defaultBranchRef/target",
            "/data/repository/defaultBranchRef/target/oid",
        ] {
            let mut value =
                response(true, [("op0".to_owned(), connection(Vec::new(), false, Value::Null))]);
            *value.pointer_mut(pointer).unwrap() = Value::Null;
            assert!(operation(vec![query("G", None)], true).decode_value(value).is_err());
        }
        for (pointer, replacement) in [
            ("/data/repository/id", json!("")),
            ("/data/repository/defaultBranchRef/name", json!("")),
            ("/data/repository/defaultBranchRef/target/oid", json!("bad")),
            (
                "/data/repository/defaultBranchRef/target/oid",
                json!("0000000000000000000000000000000000000000"),
            ),
        ] {
            let mut value =
                response(true, [("op0".to_owned(), connection(Vec::new(), false, Value::Null))]);
            *value.pointer_mut(pointer).unwrap() = replacement;
            assert!(operation(vec![query("G", None)], true).decode_value(value).is_err());
        }
    }

    #[test]
    fn untrusted_diagnostic_details_are_escaped_and_bounded() {
        assert_eq!(diagnostic_detail("line\n\x1b'\\"), r"line\n\u{1b}\'\\");
        let rendered = diagnostic_detail(&"雪".repeat(100));
        assert!(rendered.ends_with('…'));
        assert!(rendered.len() <= MAX_DIAGNOSTIC_DETAIL_BYTES + '…'.len_utf8());
    }
}

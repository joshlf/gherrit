use std::collections::{HashMap, HashSet};

use color_eyre::eyre::{Context as _, Result, bail, eyre};
use serde::Deserialize;
use serde_json::{Value, json};

use super::{
    bounded_diagnostic_detail,
    destination::{DefaultBranch, PushDestination, RepositoryCoordinates},
    local::GherritPrId,
    plan::{PlannedCreate, PlannedUpdate},
    pull_request::{
        ExactLocalPullRequestIdentities, PullRequestIdentity, PullRequestNodeId, PullRequestNumber,
        owned_base_name,
    },
};
mod observation;
mod transport;

pub(super) use observation::CompleteLocalPullRequests;
pub(super) use transport::Github;

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
/// Complete pull-request facts returned by one exact local-head query.
///
/// Correlation consumes identity, lifecycle, and head-repository facts while
/// retaining the exact refs, object IDs, and policy state. Keeping the row
/// complete means later validation never needs another network read.
pub(super) struct ObservedPullRequest {
    pub(super) identity: PullRequestIdentity,
    pub(super) title: String,
    pub(super) body: String,
    pub(super) base_branch: String,
    pub(super) head_branch: String,
    pub(super) base_oid: gix::ObjectId,
    pub(super) head_oid: gix::ObjectId,
    pub(super) state: PullRequestState,
    pub(super) is_cross_repository: bool,
    pub(super) has_auto_merge_request: bool,
    pub(super) is_in_merge_queue: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub(super) enum PullRequestState {
    Open,
    Closed,
    Merged,
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
    node_id: String,
    default_branch: DefaultBranch,
    coordinates: RepositoryCoordinates,
}

impl Repository {
    pub(super) fn into_parts(self) -> (String, DefaultBranch) {
        (self.node_id, self.default_branch)
    }
}

/// One complete GitHub repository row correlated with its exact local PR set.
///
/// The retained repository coordinates are the only GitHub-to-Git binding.
/// Planning compares them with the actual push destination before combining
/// the observations.
pub(super) struct CorrelatedRepository {
    repository: Repository,
    pull_requests: super::pull_request::CorrelatedPullRequests,
}

impl CorrelatedRepository {
    fn new(
        repository: Repository,
        pull_requests: super::pull_request::CorrelatedPullRequests,
    ) -> Self {
        Self { repository, pull_requests }
    }

    /// Constructs decoded and correlated repository evidence for semantic tests.
    #[cfg(test)]
    pub(super) fn from_typed_for_test(
        destination: &PushDestination,
        repository_node_id: String,
        default_branch: DefaultBranch,
        local: Vec<super::pull_request::LocalPullRequestObservation>,
    ) -> Result<Self> {
        let repository = Repository {
            node_id: repository_node_id,
            default_branch,
            coordinates: destination.repository_coordinates(),
        };
        let pull_requests =
            super::pull_request::CorrelatedPullRequests::from_typed_for_test(local)?;
        Ok(Self::new(repository, pull_requests))
    }

    pub(super) fn into_planning_parts_for(
        self,
        destination: &PushDestination,
    ) -> Result<(Repository, super::pull_request::CorrelatedPullRequests)> {
        let destination_coordinates = destination.repository_coordinates();
        if self.repository.coordinates != destination_coordinates {
            bail!("Git and GitHub planning evidence identify different repositories");
        }
        Ok((self.repository, self.pull_requests))
    }
}

/// A request to create one pull request.
///
/// Construction is reachable only through a planner-owned specification. The
/// planner issues that specification at the exact join of OPEN absence,
/// terminal-history emptiness, and marker absence. The head and response
/// correlation ID both derive from that authorized change ID.
#[derive(Debug, PartialEq, Eq)]
struct CreatePullRequest {
    id: GherritPrId,
    repository_id: String,
    base_branch: String,
    title: String,
    body: String,
    head_oid: gix::ObjectId,
    base_oid: gix::ObjectId,
    client_mutation_id: String,
}

impl CreatePullRequest {
    /// Converts one planner-owned create specification into its wire model.
    fn new(
        id: GherritPrId,
        repository_id: String,
        base_branch: String,
        title: String,
        body: String,
        head_oid: gix::ObjectId,
        base_oid: gix::ObjectId,
    ) -> Self {
        let client_mutation_id = format!("gherrit:create:{}", id.as_str());
        Self { id, repository_id, base_branch, title, body, head_oid, base_oid, client_mutation_id }
    }

    fn document(&self) -> String {
        let fields = [
            ("repositoryId", self.repository_id.as_str()),
            ("headRepositoryId", self.repository_id.as_str()),
            ("baseRefName", self.base_branch.as_str()),
            ("headRefName", self.id.as_str()),
            ("title", self.title.as_str()),
            ("body", self.body.as_str()),
            ("clientMutationId", self.client_mutation_id.as_str()),
        ]
        .map(|(name, value)| format!("{name}: {}", json!(value)))
        .join(", ");
        format!(
            "createPullRequest(input: {{ {fields} }}) {{ clientMutationId, pullRequest {{ number, id, state, headRefName, headRefOid, headRepository {{ id }}, baseRefName, baseRefOid, baseRepository {{ id }} }} }}"
        )
    }
}

#[derive(Debug, PartialEq, Eq)]
struct ExpectedCreateReceipt {
    alias: Box<str>,
    id: GherritPrId,
    repository_id: Box<str>,
    head_branch: Box<str>,
    base_branch: Box<str>,
    head_oid: gix::ObjectId,
    base_oid: gix::ObjectId,
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
        if created.base_ref_name != self.base_branch.as_ref() {
            let returned = bounded_diagnostic_detail(&created.base_ref_name);
            let expected = bounded_diagnostic_detail(&self.base_branch);
            bail!("createPullRequest returned base branch '{}', expected '{}'", returned, expected);
        }
        if created.state != PullRequestState::Open {
            bail!("createPullRequest returned a pull request which is not OPEN");
        }
        for (kind, repository) in
            [("head", created.head_repository), ("base", created.base_repository)]
        {
            let repository = repository
                .ok_or_else(|| eyre!("createPullRequest omitted the {kind} repository"))?;
            if repository.id != self.repository_id.as_ref() {
                bail!("createPullRequest returned a different {kind} repository");
            }
        }
        let parse_oid = |kind: &str, value: &str| {
            gix::ObjectId::from_hex(value.as_bytes())
                .map_err(|_| eyre!("createPullRequest returned an invalid {kind} object ID"))
        };
        if parse_oid("head", &created.head_ref_oid)? != self.head_oid {
            bail!("createPullRequest returned a different head object ID");
        }
        if parse_oid("base", &created.base_ref_oid)? != self.base_oid {
            bail!("createPullRequest returned a different base object ID");
        }
        let identity = PullRequestIdentity::new(created.number, created.id)?;
        Ok((self.id.clone(), identity))
    }
}

#[derive(Debug, PartialEq, Eq)]
struct PreparedCreateBatch {
    request: Value,
    serialized_bytes: usize,
    expected: Box<[ExpectedCreateReceipt]>,
    #[cfg(test)]
    effects: Vec<super::test_effect::CreateEffect>,
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
    #[cfg(test)]
    let mut effects = Vec::with_capacity(operations.len());
    for (index, operation) in operations.iter().enumerate() {
        let alias = format!("op{index}");
        fields.push_str(&format!("{alias}: {}", operation.document()));
        expected.push(ExpectedCreateReceipt {
            alias: alias.into_boxed_str(),
            id: operation.id.clone(),
            repository_id: operation.repository_id.as_str().into(),
            head_branch: operation.id.as_str().into(),
            base_branch: operation.base_branch.as_str().into(),
            head_oid: operation.head_oid,
            base_oid: operation.base_oid,
            client_mutation_id: operation.client_mutation_id.as_str().into(),
        });
        #[cfg(test)]
        effects.push(super::test_effect::CreateEffect {
            id: operation.id.clone(),
            repository_id: operation.repository_id.clone(),
            base_branch: operation.base_branch.clone(),
            title: operation.title.clone(),
            body: operation.body.clone(),
            head_oid: operation.head_oid,
            base_oid: operation.base_oid,
        });
    }
    let (request, serialized_bytes) = serialized_mutation_request(fields)?;
    Ok(PreparedCreateBatch {
        request,
        serialized_bytes,
        expected: expected.into_boxed_slice(),
        #[cfg(test)]
        effects,
    })
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
            if !self.numbers.insert(identity.number()) {
                bail!(
                    "createPullRequest receipt for '{}' reuses pull request number {} already retained for this attempt",
                    id.as_str(),
                    identity.number().get()
                );
            }
            if !self.node_ids.insert(identity.node_id().clone()) {
                let node_id = bounded_diagnostic_detail(identity.node_id().as_str());
                bail!(
                    "createPullRequest receipt for '{}' reuses pull request node ID '{}' already retained for this attempt",
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
    /// Prepares operations whose complete authorization set was consumed by
    /// the planner's exact ordered join.
    fn from_exact(
        operations: Vec<CreatePullRequest>,
        observed_identities: ExactLocalPullRequestIdentities,
    ) -> Result<Self> {
        if operations.is_empty() {
            bail!("A prepared create action requires at least one operation");
        }
        let mut expected = HashSet::with_capacity(operations.len());
        for (index, operation) in operations.iter().enumerate() {
            if !expected.insert(operation.id.clone()) {
                bail!(
                    "GraphQL create mutation at item {index} repeats change '{}'. No mutation was sent.",
                    operation.id.as_str()
                );
            }
        }
        let order = operations.iter().map(|operation| operation.id.clone()).collect::<Vec<_>>();
        let batches = prepare_create_batches(&operations)?;
        let (numbers, node_ids) = observed_identities.into_sets();
        let receipts = CreateReceiptPlan {
            expected,
            order: order.into_boxed_slice(),
            numbers,
            node_ids,
            by_change: HashMap::new(),
        };
        Ok(Self { batches, receipts })
    }

    pub(super) fn planned_ids(&self) -> Box<[GherritPrId]> {
        self.receipts.order.clone()
    }

    #[cfg(test)]
    pub(super) fn complete_for_test(
        mut self,
        receipts: Vec<(GherritPrId, PullRequestIdentity)>,
    ) -> Result<CompleteCreateReceipts> {
        self.receipts.record(receipts)?;
        self.receipts.finish()
    }

    pub(super) fn operation_count(&self) -> usize {
        self.batches.iter().map(|batch| batch.expected.len()).sum()
    }

    /// Returns typed create operations in their GraphQL request batches.
    #[cfg(test)]
    pub(super) fn effect_batches_for_test(
        &self,
    ) -> super::test_effect::EffectBatches<super::test_effect::CreateEffect> {
        self.batches.iter().map(|batch| batch.effects.clone().into_boxed_slice()).collect()
    }
}

/// Opaque proof that every planned create has one exact acknowledgement whose
/// identity is unique across the retained local observation and this attempt's
/// other create acknowledgements.
#[derive(Debug)]
pub(super) struct CompleteCreateReceipts {
    order: Box<[GherritPrId]>,
    by_change: HashMap<GherritPrId, PullRequestIdentity>,
}

/// Create receipts after an exact consuming ordered join.
#[derive(Debug)]
pub(super) struct ExactCreateReceipts {
    values: Box<[(GherritPrId, PullRequestIdentity)]>,
}

impl ExactCreateReceipts {
    pub(super) fn iter(
        &self,
    ) -> impl ExactSizeIterator<Item = (&GherritPrId, &PullRequestIdentity)> {
        self.values.iter().map(|(id, identity)| (id, identity))
    }
}

impl CompleteCreateReceipts {
    /// Visits acknowledged identities in the exact planned create order.
    pub(super) fn iter(
        &self,
    ) -> impl ExactSizeIterator<Item = (&GherritPrId, &PullRequestIdentity)> {
        self.order.iter().map(|id| {
            let identity = self
                .by_change
                .get(id)
                .expect("complete create receipts retain every planned identity");
            (id, identity)
        })
    }

    /// Consumes this receipt proof against one exact expected order.
    pub(super) fn into_exact(mut self, expected: &[GherritPrId]) -> Result<ExactCreateReceipts> {
        if self.order.as_ref() != expected {
            bail!("createPullRequest receipt order does not match the projection seed");
        }
        if self.by_change.len() != expected.len() {
            bail!("createPullRequest receipt count does not match the projection seed");
        }
        let values = expected
            .iter()
            .map(|id| {
                self.by_change.remove(id).map(|identity| (id.clone(), identity)).ok_or_else(|| {
                    eyre!("createPullRequest receipts omit projection change '{}'", id.as_str())
                })
            })
            .collect::<Result<Box<[_]>>>()?;
        if !self.by_change.is_empty() {
            bail!("createPullRequest receipts contain a change outside the projection seed");
        }
        Ok(ExactCreateReceipts { values })
    }
}

/// A nonempty minimal update to one exact preplanned pull request identity.
#[derive(Debug, PartialEq, Eq)]
struct UpdatePullRequest {
    identity: PullRequestIdentity,
    title: Option<String>,
    body: Option<String>,
    base_branch: Option<String>,
    client_mutation_id: String,
}

impl UpdatePullRequest {
    fn new(
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
        update_document(
            &self.identity,
            self.title.as_deref(),
            self.body.as_deref(),
            self.base_branch.as_deref(),
            &self.client_mutation_id,
        )
    }
}

fn update_document(
    identity: &PullRequestIdentity,
    title: Option<&str>,
    body: Option<&str>,
    base_branch: Option<&str>,
    client_mutation_id: &str,
) -> String {
    let fields = std::iter::once(("pullRequestId", identity.node_id().as_str()))
        .chain(base_branch.map(|value| ("baseRefName", value)))
        .chain(title.map(|value| ("title", value)))
        .chain(body.map(|value| ("body", value)))
        .chain(std::iter::once(("clientMutationId", client_mutation_id)))
        .map(|(name, value)| format!("{name}: {}", json!(value)))
        .collect::<Vec<_>>()
        .join(", ");
    format!(
        "updatePullRequest(input: {{ {fields} }}) {{ clientMutationId, pullRequest {{ number, id }} }}"
    )
}

/// One borrowed conservative update used only for request-size preflight.
pub(super) struct UpdatePreflight<'a> {
    identity: &'a PullRequestIdentity,
    title: Option<&'a str>,
    body: Option<&'a str>,
    base_branch: Option<&'a str>,
    client_mutation_id: String,
}

impl<'a> UpdatePreflight<'a> {
    pub(super) fn new(
        identity: &'a PullRequestIdentity,
        title: Option<&'a str>,
        body: Option<&'a str>,
        base_branch: Option<&'a str>,
    ) -> Result<Self> {
        if title.is_none() && body.is_none() && base_branch.is_none() {
            bail!("A pull request update preflight must include at least one field");
        }
        Ok(Self {
            identity,
            title,
            body,
            base_branch,
            client_mutation_id: format!("gherrit:update:{}", identity.node_id().as_str()),
        })
    }

    fn document(&self) -> String {
        update_document(
            self.identity,
            self.title,
            self.body,
            self.base_branch,
            &self.client_mutation_id,
        )
    }
}

/// Preflights a complete conservative update set without retaining its bytes.
pub(super) fn preflight_updates(operations: &[UpdatePreflight<'_>]) -> Result<()> {
    for (index, operation) in operations.iter().enumerate() {
        let fields = format!("op0: {}", operation.document());
        let (_, bytes) = serialized_mutation_request(fields)?;
        if bytes > MAX_MUTATION_REQUEST_BYTES {
            bail!(
                "GraphQL update preflight at item {index} serializes to {bytes} bytes, which exceeds the {MAX_MUTATION_REQUEST_BYTES}-byte request limit. No mutation was sent."
            );
        }
    }
    Ok(())
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
struct PreparedUpdateBatch {
    request: Value,
    serialized_bytes: usize,
    expected: Box<[ExpectedUpdateReceipt]>,
    #[cfg(test)]
    effects: Vec<super::test_effect::UpdateEffect>,
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
    #[cfg(test)]
    let mut effects = Vec::with_capacity(operations.len());
    for (index, operation) in operations.iter().enumerate() {
        let alias = format!("op{index}");
        fields.push_str(&format!("{alias}: {}", operation.document()));
        expected.push(ExpectedUpdateReceipt {
            alias: alias.into_boxed_str(),
            identity: operation.identity.clone(),
            client_mutation_id: operation.client_mutation_id.as_str().into(),
        });
        #[cfg(test)]
        effects.push(super::test_effect::UpdateEffect {
            identity: operation.identity.clone(),
            title: operation.title.clone(),
            body: operation.body.clone(),
            base_branch: operation.base_branch.clone(),
        });
    }
    let (request, serialized_bytes) = serialized_mutation_request(fields)?;
    Ok(PreparedUpdateBatch {
        request,
        serialized_bytes,
        expected: expected.into_boxed_slice(),
        #[cfg(test)]
        effects,
    })
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
    fn new(operations: Vec<UpdatePullRequest>) -> Result<Self> {
        if operations.is_empty() {
            bail!("A prepared update action requires at least one operation");
        }
        let mut numbers = HashSet::with_capacity(operations.len());
        let mut node_ids = HashSet::with_capacity(operations.len());
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
        }
        let batches = prepare_update_batches(&operations)?;
        Ok(Self { batches })
    }

    pub(super) fn operation_count(&self) -> usize {
        self.batches.iter().map(|batch| batch.expected.len()).sum()
    }

    /// Visits the exact preplanned update identities in request order.
    pub(super) fn identities(&self) -> impl Iterator<Item = &PullRequestIdentity> {
        self.batches.iter().flat_map(|batch| batch.expected.iter().map(|receipt| &receipt.identity))
    }

    /// Returns typed update operations in their GraphQL request batches.
    #[cfg(test)]
    pub(super) fn effect_batches_for_test(
        &self,
    ) -> super::test_effect::EffectBatches<super::test_effect::UpdateEffect> {
        self.batches.iter().map(|batch| batch.effects.clone().into_boxed_slice()).collect()
    }
}

/// Converts planner-owned create specifications into exact GraphQL wire data.
pub(super) fn prepare_creates(
    planned: Box<[PlannedCreate]>,
    observed_identities: ExactLocalPullRequestIdentities,
) -> Result<PreparedCreates> {
    let operations = planned
        .into_vec()
        .into_iter()
        .map(|planned| {
            let (repository_id, id, title, body, head_oid, base_oid) = planned.into_parts();
            let base_branch = owned_base_name(&id);
            CreatePullRequest::new(id, repository_id, base_branch, title, body, head_oid, base_oid)
        })
        .collect();
    PreparedCreates::from_exact(operations, observed_identities)
}

/// Converts planner-owned update specifications into exact GraphQL wire data.
pub(super) fn prepare_updates(planned: Box<[PlannedUpdate]>) -> Result<PreparedUpdates> {
    let operations = planned
        .into_vec()
        .into_iter()
        .map(|planned| {
            let (identity, title, body, base_branch) = planned.into_parts();
            UpdatePullRequest::new(identity, title, body, base_branch)
        })
        .collect::<Result<Vec<_>>>()?;
    PreparedUpdates::new(operations)
}

#[cfg(test)]
mod mutation_tests {
    use super::*;

    fn id(value: &str) -> GherritPrId {
        GherritPrId::from_ref_component(value.as_bytes()).unwrap()
    }

    fn oid(byte: u8) -> gix::ObjectId {
        gix::ObjectId::from_bytes_or_panic(&[byte; 20])
    }

    fn operation() -> CreatePullRequest {
        CreatePullRequest::new(
            id("Gone"),
            "REPO_NODE_ID".to_owned(),
            "gherrit-bases/Gone".to_owned(),
            "Title".to_owned(),
            "Body".to_owned(),
            oid(1),
            oid(2),
        )
    }

    fn identities(
        values: impl IntoIterator<Item = (u64, &'static str)>,
    ) -> ExactLocalPullRequestIdentities {
        let values = values
            .into_iter()
            .map(|(number, node_id)| PullRequestIdentity::new(number, node_id.to_owned()).unwrap())
            .collect::<Vec<_>>();
        ExactLocalPullRequestIdentities::new(&values).unwrap()
    }

    fn acknowledgement() -> Value {
        json!({
            "data": {
                "op0": {
                    "clientMutationId": "gherrit:create:Gone",
                    "pullRequest": {
                        "number": 7,
                        "id": "PR_7",
                        "state": "OPEN",
                        "headRefName": "Gone",
                        "headRefOid": oid(1).to_string(),
                        "headRepository": { "id": "REPO_NODE_ID" },
                        "baseRefName": "gherrit-bases/Gone",
                        "baseRefOid": oid(2).to_string(),
                        "baseRepository": { "id": "REPO_NODE_ID" }
                    }
                }
            }
        })
    }

    fn update(number: u64, node_id: &str) -> UpdatePullRequest {
        UpdatePullRequest::new(
            PullRequestIdentity::new(number, node_id.to_owned()).unwrap(),
            Some("Title".to_owned()),
            None,
            None,
        )
        .unwrap()
    }

    fn update_acknowledgement(client_id: &str, number: u64, node_id: &str) -> Value {
        json!({
            "data": {
                "op0": {
                    "clientMutationId": client_id,
                    "pullRequest": { "number": number, "id": node_id }
                }
            }
        })
    }

    fn raw_create(body_len: usize) -> CreatePullRequest {
        CreatePullRequest::new(
            id("Gsize"),
            "REPO_NODE_ID".to_owned(),
            "gherrit-bases/Gsize".to_owned(),
            "Title".to_owned(),
            "x".repeat(body_len),
            oid(1),
            oid(2),
        )
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
    fn create_request_names_same_repository_and_selects_exact_receipt_facts() {
        let document = operation().document();
        for required in [
            "repositoryId: \"REPO_NODE_ID\"",
            "headRepositoryId: \"REPO_NODE_ID\"",
            "headRefName: \"Gone\"",
            "baseRefName: \"gherrit-bases/Gone\"",
            "clientMutationId",
            "number, id, state",
            "headRefOid",
            "headRepository { id }",
            "baseRefOid",
            "baseRepository { id }",
        ] {
            assert!(document.contains(required), "{document}");
        }
    }

    #[test]
    fn create_receipt_is_bound_to_every_planned_effect_fact() {
        let decode = |response| create_batch(&[operation()]).unwrap().decode(response);
        let receipt = decode(acknowledgement()).unwrap();
        assert_eq!(receipt[0].0.as_str(), "Gone");
        assert_eq!(receipt[0].1.number().get(), 7);

        let cases = [
            ("/data/op0/clientMutationId", json!("wrong")),
            ("/data/op0/pullRequest", Value::Null),
            ("/data/op0/pullRequest/number", json!(0)),
            ("/data/op0/pullRequest/id", json!("")),
            ("/data/op0/pullRequest/state", json!("CLOSED")),
            ("/data/op0/pullRequest/headRefName", json!("Other")),
            ("/data/op0/pullRequest/headRefOid", json!(oid(3).to_string())),
            ("/data/op0/pullRequest/headRepository", Value::Null),
            ("/data/op0/pullRequest/headRepository/id", json!("OTHER")),
            ("/data/op0/pullRequest/baseRefName", json!("main")),
            ("/data/op0/pullRequest/baseRefOid", json!(oid(3).to_string())),
            ("/data/op0/pullRequest/baseRepository", Value::Null),
            ("/data/op0/pullRequest/baseRepository/id", json!("OTHER")),
        ];
        for (pointer, replacement) in cases {
            let mut response = acknowledgement();
            *response.pointer_mut(pointer).unwrap() = replacement;
            assert!(decode(response).is_err(), "accepted mismatch at {pointer}");
        }
    }

    #[test]
    fn create_receipts_are_complete_unique_and_alias_exact() {
        let operations = vec![
            operation(),
            CreatePullRequest::new(
                id("Gtwo"),
                "REPO_NODE_ID".to_owned(),
                "gherrit-bases/Gtwo".to_owned(),
                "Title two".to_owned(),
                "Body two".to_owned(),
                oid(3),
                oid(4),
            ),
        ];
        let prepared = PreparedCreates::from_exact(operations, identities([])).unwrap();
        assert!(prepared.complete_for_test(Vec::new()).is_err());

        let duplicate = PullRequestIdentity::new(7, "PR_7".to_owned()).unwrap();
        let mut prepared = PreparedCreates::from_exact(vec![operation()], identities([])).unwrap();
        assert!(
            prepared
                .receipts
                .record(vec![(id("Gone"), duplicate.clone()), (id("Gone"), duplicate),])
                .is_err()
        );

        let mut extra = acknowledgement();
        extra["data"]["extra"] = Value::Null;
        assert!(create_batch(&[operation()]).unwrap().decode(extra).is_err());

        for observed in [identities([(7, "OTHER")]), identities([(8, "PR_7")])] {
            let prepared = PreparedCreates::from_exact(vec![operation()], observed).unwrap();
            assert!(
                prepared
                    .complete_for_test(vec![(
                        id("Gone"),
                        PullRequestIdentity::new(7, "PR_7".to_owned()).unwrap(),
                    )])
                    .is_err()
            );
        }
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

        let decode = |response| update_batch(&[update(1, "PR_1")]).unwrap().decode(response);
        decode(update_acknowledgement("gherrit:update:PR_1", 1, "PR_1")).unwrap();
        for response in [
            update_acknowledgement("wrong", 1, "PR_1"),
            update_acknowledgement("gherrit:update:PR_1", 2, "PR_1"),
            update_acknowledgement("gherrit:update:PR_1", 1, "PR_2"),
            json!({ "data": { "op0": null } }),
            json!({
                "data": {
                    "op0": {
                        "clientMutationId": "gherrit:update:PR_1",
                        "pullRequest": null
                    }
                }
            }),
        ] {
            assert!(decode(response).is_err());
        }
    }

    #[test]
    fn update_preparation_rejects_each_independent_identity_collision() {
        assert!(PreparedUpdates::new(vec![update(1, "PR_1"), update(1, "PR_2")]).is_err());
        assert!(PreparedUpdates::new(vec![update(1, "PR_1"), update(2, "PR_1")]).is_err());
    }

    #[test]
    fn mutation_batches_use_the_exact_serialized_one_mibibyte_boundary() {
        let create_fixed = create_batch(&[raw_create(0)]).unwrap().serialized_bytes;
        let create_body_len = MAX_MUTATION_REQUEST_BYTES - create_fixed;
        assert_eq!(
            create_batch(&[raw_create(create_body_len)]).unwrap().serialized_bytes,
            MAX_MUTATION_REQUEST_BYTES
        );
        assert!(prepare_create_batches(&[raw_create(create_body_len)]).is_ok());
        assert!(prepare_create_batches(&[raw_create(create_body_len + 1)]).is_err());

        let update_fixed = update_batch(&[raw_update(0)]).unwrap().serialized_bytes;
        let update_body_len = MAX_MUTATION_REQUEST_BYTES - update_fixed;
        assert_eq!(
            update_batch(&[raw_update(update_body_len)]).unwrap().serialized_bytes,
            MAX_MUTATION_REQUEST_BYTES
        );
        assert!(prepare_update_batches(&[raw_update(update_body_len)]).is_ok());
        assert!(prepare_update_batches(&[raw_update(update_body_len + 1)]).is_err());

        let mut creates = (0..MAX_MUTATION_ALIASES)
            .map(|index| {
                CreatePullRequest::new(
                    id(&format!("G{index}")),
                    "REPO_NODE_ID".to_owned(),
                    format!("gherrit-bases/G{index}"),
                    "small".to_owned(),
                    String::new(),
                    oid(1),
                    oid(2),
                )
            })
            .collect::<Vec<_>>();
        creates.push(raw_create(create_body_len + 1));
        assert!(prepare_create_batches(&creates).is_err());

        let mut updates = (0..MAX_MUTATION_ALIASES)
            .map(|index| update(u64::try_from(index + 1).unwrap(), &format!("PR_{index}")))
            .collect::<Vec<_>>();
        updates.push(raw_update(update_body_len + 1));
        assert!(prepare_update_batches(&updates).is_err());
    }

    #[test]
    fn response_derived_mutation_diagnostics_are_single_line_and_bounded() {
        let returned = format!("{}\nnot-disclosed", "x".repeat(1_000));
        let create_error = create_batch(&[operation()])
            .unwrap()
            .decode({
                let mut response = acknowledgement();
                response["data"]["op0"]["clientMutationId"] = json!(returned);
                response
            })
            .unwrap_err()
            .to_string();

        assert!(!create_error.contains('\n'));
        assert!(!create_error.contains("not-disclosed"));
        assert!(create_error.len() < 500);

        let update_error = update_batch(&[update(1, "PR_1")])
            .unwrap()
            .decode(update_acknowledgement("gherrit:update:PR_1", 1, &returned))
            .unwrap_err()
            .to_string();
        assert!(!update_error.contains('\n'));
        assert!(!update_error.contains("not-disclosed"));
        assert!(update_error.len() < 800);
    }

    #[test]
    fn concrete_mutation_actions_must_be_nonempty() {
        assert!(PreparedCreates::from_exact(Vec::new(), identities([])).is_err());
        assert!(PreparedUpdates::new(Vec::new()).is_err());
    }
}

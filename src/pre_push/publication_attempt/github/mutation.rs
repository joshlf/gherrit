//! Bounded GraphQL mutations and exact acknowledgement receipts.
//!
//! Construction remains private to this adapter until the publication planner
//! supplies the one-use authority which permits each effect. Every batch is
//! preflighted before the first request can be sent, and each response token
//! is consumed exactly once.

use std::collections::HashSet;

use color_eyre::eyre::{Context as _, Result, bail, eyre};
use gix::ObjectId;
use serde::Deserialize;
use serde_json::{Map, Value, json};

use super::{
    super::{body::GeneratedBody, refs::PublicationRevision},
    RepositoryNodeId,
    pull_request::{PullRequestIdentity, PullRequestIdentityRegistry},
    transport::{Github, indeterminate_mutation},
};
use crate::pre_push::{
    batching::{MAX_MUTATION_ALIASES, MAX_MUTATION_REQUEST_BYTES},
    json::UniqueJson,
    local::{GherritPrId, PullRequestTitle},
};

fn owned_base_name(id: &GherritPrId) -> String {
    format!("gherrit-bases/{}", id.as_str())
}

fn create_client_mutation_id(id: &GherritPrId) -> String {
    format!("gherrit:create:{}", id.as_str())
}

fn update_client_mutation_id(identity: &PullRequestIdentity) -> String {
    format!("gherrit:update:{}", identity.node_id().as_str())
}

fn response_data(response: UniqueJson, expected_operations: usize) -> Result<Map<String, Value>> {
    let response = response.into_value();
    let response =
        response.as_object().ok_or_else(|| eyre!("GraphQL mutation response is not an object"))?;
    if response.keys().any(|field| !matches!(field.as_str(), "data" | "errors" | "extensions")) {
        bail!("GraphQL mutation response has unexpected top-level fields");
    }
    if response.get("extensions").is_some_and(|extensions| !extensions.is_object()) {
        bail!("GraphQL mutation response has malformed extensions");
    }
    match response.get("errors") {
        None => {}
        Some(Value::Array(errors)) if errors.is_empty() => {}
        Some(_) => bail!("GraphQL mutation response contains errors"),
    }

    let mut data = response
        .get("data")
        .and_then(Value::as_object)
        .cloned()
        .ok_or_else(|| eyre!("GraphQL mutation response is missing data"))?;
    let expected =
        (0..expected_operations).map(|index| format!("op{index}")).collect::<HashSet<_>>();
    if data.keys().any(|alias| !expected.contains(alias)) {
        bail!("GraphQL mutation response contains an unexpected operation");
    }
    if data.len() != expected_operations {
        bail!("GraphQL mutation response has an incomplete alias set");
    }
    Ok(std::mem::take(&mut data))
}

/// One preflighted, non-cloneable GraphQL mutation request.
///
/// Only this module can construct the token. The transport consumes it, so no
/// sibling can bypass batch preflight or retain a value to replay.
pub(super) struct MutationRequest(Value);

impl MutationRequest {
    pub(super) fn into_value(self) -> Value {
        self.0
    }
}

fn serialized_request(fields: String) -> Result<(MutationRequest, usize)> {
    let request = json!({ "query": format!("mutation {{ {fields} }}") });
    let bytes = serde_json::to_vec(&request)
        .wrap_err("Failed to serialize a GraphQL mutation request")?
        .len();
    Ok((MutationRequest(request), bytes))
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
enum PullRequestState {
    Open,
    Closed,
    Merged,
}

/// Exact wire intent for one stable-key pull request creation.
#[derive(Debug, Eq, PartialEq)]
pub(in crate::pre_push::publication_attempt) struct CreatePullRequest {
    id: GherritPrId,
    title: String,
    body: String,
    head_oid: ObjectId,
    base_oid: ObjectId,
}

impl CreatePullRequest {
    fn new(
        id: GherritPrId,
        title: String,
        body: String,
        head_oid: ObjectId,
        base_oid: ObjectId,
    ) -> Self {
        Self { id, title, body, head_oid, base_oid }
    }

    /// Consumes exact absence and typed local content into one stable-key
    /// create operation.
    pub(in crate::pre_push::publication_attempt) fn from_absence(
        absence: super::observation::AbsentPullRequest,
        title: PullRequestTitle,
        body: GeneratedBody,
        revision: PublicationRevision,
    ) -> Self {
        Self::new(
            absence.into_id(),
            title.as_str().to_owned(),
            body.into_string(),
            revision.head(),
            revision.owned_base(),
        )
    }

    fn document(&self, repository_id: &RepositoryNodeId) -> String {
        let base = owned_base_name(&self.id);
        let client_mutation_id = create_client_mutation_id(&self.id);
        let fields = [
            ("repositoryId", repository_id.as_str()),
            ("headRepositoryId", repository_id.as_str()),
            ("baseRefName", base.as_str()),
            ("headRefName", self.id.as_str()),
            ("title", self.title.as_str()),
            ("body", self.body.as_str()),
            ("clientMutationId", client_mutation_id.as_str()),
        ]
        .map(|(name, value)| format!("{name}: {}", json!(value)))
        .join(", ");
        format!(
            "createPullRequest(input: {{ {fields} }}) {{ clientMutationId, pullRequest {{ number, id, state, headRefName, headRefOid, headRepository {{ id }}, baseRefName, baseRefOid, baseRepository {{ id }} }} }}"
        )
    }

    #[cfg(test)]
    fn test_view(&self) -> TestCreate {
        TestCreate {
            id: self.id.clone(),
            title: self.title.clone(),
            body: self.body.clone(),
            head_oid: self.head_oid,
            base_oid: self.base_oid,
        }
    }
}

#[derive(Debug, Eq, PartialEq)]
struct ExpectedCreateReceipt {
    id: GherritPrId,
    head_oid: ObjectId,
    base_oid: ObjectId,
}

impl ExpectedCreateReceipt {
    fn decode(
        &self,
        repository_id: &RepositoryNodeId,
        response: Value,
    ) -> Result<(GherritPrId, PullRequestIdentity)> {
        #[derive(Deserialize)]
        #[serde(rename_all = "camelCase", deny_unknown_fields)]
        struct Response {
            client_mutation_id: String,
            pull_request: Option<CreatedPullRequest>,
        }

        #[derive(Deserialize)]
        #[serde(rename_all = "camelCase", deny_unknown_fields)]
        struct CreatedPullRequest {
            number: u64,
            id: String,
            state: PullRequestState,
            head_ref_name: String,
            head_ref_oid: String,
            head_repository: Option<CreatedRepository>,
            base_ref_name: String,
            base_ref_oid: String,
            base_repository: Option<CreatedRepository>,
        }

        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct CreatedRepository {
            id: String,
        }

        if response.is_null() {
            bail!("GraphQL create mutation response operation is null");
        }
        let response: Response = serde_json::from_value(response)
            .map_err(|_| eyre!("Failed to decode createPullRequest response"))?;
        if response.client_mutation_id != create_client_mutation_id(&self.id) {
            bail!("createPullRequest returned a different clientMutationId");
        }
        let created = response.pull_request.ok_or_else(|| {
            eyre!("createPullRequest returned a null pull request for its planned head")
        })?;
        if created.state != PullRequestState::Open {
            bail!("createPullRequest returned a pull request which is not OPEN");
        }
        if created.head_ref_name != self.id.as_str() {
            bail!("createPullRequest returned a different head branch");
        }
        if created.base_ref_name != owned_base_name(&self.id) {
            bail!("createPullRequest returned a different base branch");
        }
        for (kind, repository) in
            [("head", created.head_repository), ("base", created.base_repository)]
        {
            let repository = repository
                .ok_or_else(|| eyre!("createPullRequest omitted the {kind} repository"))?;
            if repository.id != repository_id.as_str() {
                bail!("createPullRequest returned a different {kind} repository");
            }
        }
        let parse_oid = |kind: &str, oid: &str| {
            let oid = ObjectId::from_hex(oid.as_bytes())
                .map_err(|_| eyre!("createPullRequest returned an invalid {kind} object ID"))?;
            if oid.is_null() {
                bail!("createPullRequest returned a null {kind} object ID");
            }
            Ok(oid)
        };
        if parse_oid("head", &created.head_ref_oid)? != self.head_oid {
            bail!("createPullRequest returned a different head object ID");
        }
        if parse_oid("base", &created.base_ref_oid)? != self.base_oid {
            bail!("createPullRequest returned a different base object ID");
        }
        Ok((self.id.clone(), PullRequestIdentity::new(created.number, created.id)?))
    }
}

struct PreparedCreateBatch {
    request: MutationRequest,
    serialized_bytes: usize,
    expected: Box<[ExpectedCreateReceipt]>,
}

impl PreparedCreateBatch {
    fn into_request(self) -> (MutationRequest, CreateReceiptDecoder) {
        (self.request, CreateReceiptDecoder { expected: self.expected })
    }
}

struct CreateReceiptDecoder {
    expected: Box<[ExpectedCreateReceipt]>,
}

impl CreateReceiptDecoder {
    fn decode(
        self,
        repository_id: &RepositoryNodeId,
        response: UniqueJson,
    ) -> Result<Box<[(GherritPrId, PullRequestIdentity)]>> {
        let mut data = response_data(response, self.expected.len())?;
        self.expected
            .into_vec()
            .into_iter()
            .enumerate()
            .map(|(index, expected)| {
                let alias = format!("op{index}");
                let response = data.remove(&alias).expect("the exact alias set was checked");
                expected
                    .decode(repository_id, response)
                    .wrap_err_with(|| format!("Invalid acknowledgement for mutation `{alias}`"))
            })
            .collect()
    }
}

fn create_batch(
    repository_id: &RepositoryNodeId,
    operations: &[CreatePullRequest],
) -> Result<PreparedCreateBatch> {
    let mut fields = String::new();
    let mut expected = Vec::with_capacity(operations.len());
    for (index, operation) in operations.iter().enumerate() {
        let alias = format!("op{index}");
        if !fields.is_empty() {
            fields.push(' ');
        }
        fields.push_str(&format!("{alias}: {}", operation.document(repository_id)));
        expected.push(ExpectedCreateReceipt {
            id: operation.id.clone(),
            head_oid: operation.head_oid,
            base_oid: operation.base_oid,
        });
    }
    let (request, serialized_bytes) = serialized_request(fields)?;
    Ok(PreparedCreateBatch { request, serialized_bytes, expected: expected.into_boxed_slice() })
}

fn prepare_create_batches(
    repository_id: &RepositoryNodeId,
    operations: &[CreatePullRequest],
) -> Result<Box<[PreparedCreateBatch]>> {
    let mut batches = Vec::new();
    let mut start = 0;
    while start < operations.len() {
        let max_end = operations.len().min(start + MAX_MUTATION_ALIASES);
        let mut accepted = None;
        for end in start + 1..=max_end {
            let batch = create_batch(repository_id, &operations[start..end])?;
            if batch.serialized_bytes > MAX_MUTATION_REQUEST_BYTES {
                break;
            }
            accepted = Some((end, batch));
        }
        let Some((end, batch)) = accepted else {
            let bytes =
                create_batch(repository_id, &operations[start..start + 1])?.serialized_bytes;
            bail!(
                "GraphQL create mutation at item {start} serializes to {bytes} bytes, which exceeds the {MAX_MUTATION_REQUEST_BYTES}-byte request limit. No mutation was sent."
            );
        };
        batches.push(batch);
        start = end;
    }
    Ok(batches.into_boxed_slice())
}

/// Every create batch preflighted before the first request can be sent.
pub(in crate::pre_push::publication_attempt) struct PreparedCreates {
    repository_id: RepositoryNodeId,
    batches: Box<[PreparedCreateBatch]>,
    identities: PullRequestIdentityRegistry,
    #[cfg(test)]
    operations: Box<[TestCreate]>,
}

impl PreparedCreates {
    pub(super) fn new(
        repository_id: RepositoryNodeId,
        operations: Vec<CreatePullRequest>,
        identities: PullRequestIdentityRegistry,
    ) -> Result<Self> {
        if operations.is_empty() {
            bail!("A prepared create action requires at least one operation");
        }
        let mut ids = HashSet::with_capacity(operations.len());
        for (index, operation) in operations.iter().enumerate() {
            if !ids.insert(&operation.id) {
                bail!(
                    "GraphQL create mutation at item {index} repeats a change ID. No mutation was sent."
                );
            }
        }
        let batches = prepare_create_batches(&repository_id, &operations)?;
        #[cfg(test)]
        let operations = operations.iter().map(CreatePullRequest::test_view).collect();
        Ok(Self {
            repository_id,
            batches,
            identities,
            #[cfg(test)]
            operations,
        })
    }

    async fn execute(self, github: &Github) -> Result<CompleteCreateReceipts> {
        let Self { repository_id, batches, mut identities, .. } = self;
        let operation_count = batches.iter().map(|batch| batch.expected.len()).sum();
        let mut values = Vec::with_capacity(operation_count);
        for batch in batches.into_vec() {
            log::trace!(
                "Sending GraphQL create batch ({} operations, {} bytes)",
                batch.expected.len(),
                batch.serialized_bytes
            );
            let (request, decoder) = batch.into_request();
            let response = github.send_mutation_once(request).await?;
            let receipts =
                decoder.decode(&repository_id, response).map_err(indeterminate_mutation)?;
            for (id, identity) in receipts.into_vec() {
                identities.insert_create_receipt(&identity).map_err(indeterminate_mutation)?;
                values.push((id, identity));
            }
        }
        Ok(CompleteCreateReceipts { values: values.into_boxed_slice() })
    }

    #[cfg(test)]
    pub(in crate::pre_push::publication_attempt) fn operations_for_test(&self) -> &[TestCreate] {
        &self.operations
    }

    #[cfg(test)]
    pub(super) fn for_test(
        repository_id: String,
        operations: Vec<TestCreate>,
        identities: PullRequestIdentityRegistry,
    ) -> Result<Self> {
        Self::new(
            RepositoryNodeId::new(repository_id).expect("valid test repository node ID"),
            operations.into_iter().map(TestCreate::into_operation).collect(),
            identities,
        )
    }
}

/// Opaque proof of one exact receipt for every create, in request order.
pub(in crate::pre_push::publication_attempt) struct CompleteCreateReceipts {
    values: Box<[(GherritPrId, PullRequestIdentity)]>,
}

impl CompleteCreateReceipts {
    #[cfg(test)]
    pub(super) fn iter(
        &self,
    ) -> impl ExactSizeIterator<Item = (&GherritPrId, &PullRequestIdentity)> {
        self.values.iter().map(|(id, identity)| (id, identity))
    }

    /// Preserves exact create-request order for the planner's positional join.
    pub(in crate::pre_push::publication_attempt) fn into_values(
        self,
    ) -> Box<[(GherritPrId, PullRequestIdentity)]> {
        self.values
    }

    #[cfg(test)]
    pub(in crate::pre_push::publication_attempt) fn for_plan_test(
        values: Vec<(GherritPrId, PullRequestIdentity)>,
    ) -> Self {
        Self { values: values.into_boxed_slice() }
    }
}

#[derive(Debug, Eq, PartialEq)]
pub(in crate::pre_push::publication_attempt) struct UpdatePullRequest {
    identity: PullRequestIdentity,
    title: Option<String>,
    body: Option<String>,
    base_branch: Option<String>,
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
        Ok(Self { identity, title, body, base_branch })
    }

    /// Converts validated semantic differences into one bounded wire update.
    pub(in crate::pre_push::publication_attempt) fn from_projection(
        identity: PullRequestIdentity,
        title: Option<PullRequestTitle>,
        body: Option<GeneratedBody>,
        base_branch: Option<String>,
    ) -> Result<Self> {
        Self::new(
            identity,
            title.map(|title| title.as_str().to_owned()),
            body.map(GeneratedBody::into_string),
            base_branch,
        )
    }

    fn document(&self) -> String {
        let client_mutation_id = update_client_mutation_id(&self.identity);
        let fields = std::iter::once(("pullRequestId", self.identity.node_id().as_str()))
            .chain(self.base_branch.as_deref().map(|value| ("baseRefName", value)))
            .chain(self.title.as_deref().map(|value| ("title", value)))
            .chain(self.body.as_deref().map(|value| ("body", value)))
            .chain(std::iter::once(("clientMutationId", client_mutation_id.as_str())))
            .map(|(name, value)| format!("{name}: {}", json!(value)))
            .collect::<Vec<_>>()
            .join(", ");
        format!(
            "updatePullRequest(input: {{ {fields} }}) {{ clientMutationId, pullRequest {{ number, id }} }}"
        )
    }

    #[cfg(test)]
    pub(in crate::pre_push::publication_attempt) fn into_test(self) -> TestUpdate {
        TestUpdate {
            identity: self.identity,
            title: self.title,
            body: self.body,
            base_branch: self.base_branch,
        }
    }
}

#[derive(Debug, Eq, PartialEq)]
struct ExpectedUpdateReceipt {
    identity: PullRequestIdentity,
}

impl ExpectedUpdateReceipt {
    fn decode(&self, response: Value) -> Result<()> {
        #[derive(Deserialize)]
        #[serde(rename_all = "camelCase", deny_unknown_fields)]
        struct Response {
            client_mutation_id: String,
            pull_request: Option<UpdatedPullRequest>,
        }

        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct UpdatedPullRequest {
            number: u64,
            id: String,
        }

        if response.is_null() {
            bail!("GraphQL update mutation response operation is null");
        }
        let response: Response = serde_json::from_value(response)
            .map_err(|_| eyre!("Failed to decode updatePullRequest response"))?;
        if response.client_mutation_id != update_client_mutation_id(&self.identity) {
            bail!("updatePullRequest returned a different clientMutationId");
        }
        let updated = response
            .pull_request
            .ok_or_else(|| eyre!("updatePullRequest returned a null pull request"))?;
        let identity = PullRequestIdentity::new(updated.number, updated.id)?;
        if identity != self.identity {
            bail!("updatePullRequest returned a different pull request identity");
        }
        Ok(())
    }
}

struct PreparedUpdateBatch {
    request: MutationRequest,
    serialized_bytes: usize,
    expected: Box<[ExpectedUpdateReceipt]>,
}

impl PreparedUpdateBatch {
    fn into_request(self) -> (MutationRequest, UpdateReceiptDecoder) {
        (self.request, UpdateReceiptDecoder { expected: self.expected })
    }
}

struct UpdateReceiptDecoder {
    expected: Box<[ExpectedUpdateReceipt]>,
}

impl UpdateReceiptDecoder {
    fn decode(self, response: UniqueJson) -> Result<()> {
        let mut data = response_data(response, self.expected.len())?;
        for (index, expected) in self.expected.into_vec().into_iter().enumerate() {
            let alias = format!("op{index}");
            let response = data.remove(&alias).expect("the exact alias set was checked");
            expected
                .decode(response)
                .wrap_err_with(|| format!("Invalid acknowledgement for mutation `{alias}`"))?;
        }
        Ok(())
    }
}

fn update_batch(operations: &[UpdatePullRequest]) -> Result<PreparedUpdateBatch> {
    let mut fields = String::new();
    let mut expected = Vec::with_capacity(operations.len());
    for (index, operation) in operations.iter().enumerate() {
        let alias = format!("op{index}");
        if !fields.is_empty() {
            fields.push(' ');
        }
        fields.push_str(&format!("{alias}: {}", operation.document()));
        expected.push(ExpectedUpdateReceipt { identity: operation.identity.clone() });
    }
    let (request, serialized_bytes) = serialized_request(fields)?;
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

/// Every update batch preflighted before the first request can be sent.
///
/// The empty value is the complete representation of a projection which is
/// already current; callers do not need a parallel optional-update state.
pub(in crate::pre_push::publication_attempt) struct PreparedUpdates {
    batches: Box<[PreparedUpdateBatch]>,
    #[cfg(test)]
    operations: Box<[TestUpdate]>,
}

impl PreparedUpdates {
    pub(in crate::pre_push::publication_attempt) fn new(
        operations: Vec<UpdatePullRequest>,
    ) -> Result<Self> {
        let mut numbers = HashSet::with_capacity(operations.len());
        let mut node_ids = HashSet::with_capacity(operations.len());
        for (index, operation) in operations.iter().enumerate() {
            if !numbers.insert(operation.identity.number()) {
                bail!(
                    "GraphQL update mutation at item {index} repeats a pull request number. No mutation was sent."
                );
            }
            if !node_ids.insert(operation.identity.node_id()) {
                bail!(
                    "GraphQL update mutation at item {index} repeats a pull request node ID. No mutation was sent."
                );
            }
        }
        let batches = prepare_update_batches(&operations)?;
        #[cfg(test)]
        let operations = operations.into_iter().map(UpdatePullRequest::into_test).collect();
        Ok(Self {
            batches,
            #[cfg(test)]
            operations,
        })
    }

    #[cfg(test)]
    pub(in crate::pre_push::publication_attempt) fn operations_for_test(&self) -> &[TestUpdate] {
        &self.operations
    }

    async fn execute(self, github: &Github) -> Result<()> {
        for batch in self.batches.into_vec() {
            log::trace!(
                "Sending GraphQL update batch ({} operations, {} bytes)",
                batch.expected.len(),
                batch.serialized_bytes
            );
            let (request, decoder) = batch.into_request();
            let response = github.send_mutation_once(request).await?;
            decoder.decode(response).map_err(indeterminate_mutation)?;
        }
        Ok(())
    }

    #[cfg(test)]
    pub(super) fn for_test(operations: Vec<TestUpdate>) -> Result<Self> {
        Self::new(
            operations.into_iter().map(TestUpdate::into_operation).collect::<Result<Vec<_>>>()?,
        )
    }
}

impl Github {
    pub(in crate::pre_push::publication_attempt) async fn create_pull_requests(
        &self,
        creates: PreparedCreates,
    ) -> Result<CompleteCreateReceipts> {
        creates.execute(self).await
    }

    pub(in crate::pre_push::publication_attempt) async fn update_pull_requests(
        &self,
        updates: PreparedUpdates,
    ) -> Result<()> {
        updates.execute(self).await
    }
}

#[cfg(test)]
#[derive(Clone, Debug, Eq, PartialEq)]
pub(in crate::pre_push::publication_attempt) struct TestCreate {
    pub(in crate::pre_push::publication_attempt) id: GherritPrId,
    pub(in crate::pre_push::publication_attempt) title: String,
    pub(in crate::pre_push::publication_attempt) body: String,
    pub(in crate::pre_push::publication_attempt) head_oid: ObjectId,
    pub(in crate::pre_push::publication_attempt) base_oid: ObjectId,
}

#[cfg(test)]
impl TestCreate {
    fn into_operation(self) -> CreatePullRequest {
        CreatePullRequest::new(self.id, self.title, self.body, self.head_oid, self.base_oid)
    }
}

#[cfg(test)]
#[derive(Clone, Debug, Eq, PartialEq)]
pub(in crate::pre_push::publication_attempt) struct TestUpdate {
    pub(in crate::pre_push::publication_attempt) identity: PullRequestIdentity,
    pub(in crate::pre_push::publication_attempt) title: Option<String>,
    pub(in crate::pre_push::publication_attempt) body: Option<String>,
    pub(in crate::pre_push::publication_attempt) base_branch: Option<String>,
}

#[cfg(test)]
impl TestUpdate {
    fn into_operation(self) -> Result<UpdatePullRequest> {
        UpdatePullRequest::new(self.identity, self.title, self.body, self.base_branch)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const HEAD: &str = "1111111111111111111111111111111111111111";
    const BASE: &str = "2222222222222222222222222222222222222222";

    fn id(value: &str) -> GherritPrId {
        GherritPrId::from_ref_component(value.as_bytes()).unwrap()
    }

    fn oid(value: &str) -> ObjectId {
        ObjectId::from_hex(value.as_bytes()).unwrap()
    }

    fn create(value: &str) -> CreatePullRequest {
        CreatePullRequest::new(
            id(value),
            format!("title {value}"),
            format!("body {value}"),
            oid(HEAD),
            oid(BASE),
        )
    }

    fn create_response(expected: &CreatePullRequest, number: u64, node_id: &str) -> UniqueJson {
        let response = json!({
            "data": {
                "op0": {
                    "clientMutationId": create_client_mutation_id(&expected.id),
                    "pullRequest": {
                        "number": number,
                        "id": node_id,
                        "state": "OPEN",
                        "headRefName": expected.id.as_str(),
                        "headRefOid": expected.head_oid.to_string(),
                        "headRepository": { "id": "REPOSITORY" },
                        "baseRefName": owned_base_name(&expected.id),
                        "baseRefOid": expected.base_oid.to_string(),
                        "baseRepository": { "id": "REPOSITORY" },
                    },
                },
            },
        });
        UniqueJson::decode(&serde_json::to_vec(&response).unwrap()).unwrap()
    }

    #[test]
    fn create_document_uses_the_stable_owned_base_and_json_escaping() {
        let mut operation = create("Gone");
        operation.title = "quoted \" title".to_owned();
        operation.body = "line one\nline two".to_owned();
        let repository_id = RepositoryNodeId::new("REPOSITORY".to_owned()).unwrap();
        let document = operation.document(&repository_id);
        insta::assert_snapshot!(document, @r###"createPullRequest(input: { repositoryId: "REPOSITORY", headRepositoryId: "REPOSITORY", baseRefName: "gherrit-bases/Gone", headRefName: "Gone", title: "quoted \" title", body: "line one\nline two", clientMutationId: "gherrit:create:Gone" }) { clientMutationId, pullRequest { number, id, state, headRefName, headRefOid, headRepository { id }, baseRefName, baseRefOid, baseRepository { id } } }"###);
    }

    #[test]
    fn create_receipt_requires_every_exact_authority_field() {
        let operation = create("Gone");
        let repository_id = RepositoryNodeId::new("REPOSITORY".to_owned()).unwrap();
        let batch = create_batch(&repository_id, std::slice::from_ref(&operation)).unwrap();
        let (_, decoder) = batch.into_request();
        let receipts =
            decoder.decode(&repository_id, create_response(&operation, 1, "PR_ONE")).unwrap();
        assert_eq!(receipts[0].0, id("Gone"));
        assert_eq!(receipts[0].1.number().get(), 1);
        assert_eq!(receipts[0].1.node_id().as_str(), "PR_ONE");

        let valid = create_response(&operation, 1, "PR_ONE").into_value();
        for (pointer, replacement) in [
            ("/data/op0/clientMutationId", json!("wrong")),
            ("/data/op0/pullRequest", Value::Null),
            ("/data/op0/pullRequest/number", json!(0)),
            ("/data/op0/pullRequest/id", json!("")),
            ("/data/op0/pullRequest/state", json!("CLOSED")),
            ("/data/op0/pullRequest/headRefName", json!("Other")),
            ("/data/op0/pullRequest/headRefOid", json!(BASE)),
            ("/data/op0/pullRequest/headRefOid", Value::Null),
            ("/data/op0/pullRequest/headRepository/id", json!("OTHER")),
            ("/data/op0/pullRequest/headRepository", Value::Null),
            ("/data/op0/pullRequest/baseRefName", json!("main")),
            ("/data/op0/pullRequest/baseRefOid", json!(HEAD)),
            ("/data/op0/pullRequest/baseRefOid", Value::Null),
            ("/data/op0/pullRequest/baseRepository/id", json!("OTHER")),
            ("/data/op0/pullRequest/baseRepository", Value::Null),
        ] {
            let mut response = valid.clone();
            *response.pointer_mut(pointer).unwrap() = replacement;
            let response = UniqueJson::decode(&serde_json::to_vec(&response).unwrap()).unwrap();
            let (_, decoder) = create_batch(&repository_id, std::slice::from_ref(&operation))
                .unwrap()
                .into_request();
            assert!(
                decoder.decode(&repository_id, response).is_err(),
                "accepted mutation at {pointer}"
            );
        }
    }

    #[test]
    fn mutation_envelope_and_aliases_are_exact_and_duplicate_safe() {
        let operation = create("Gone");
        let repository_id = RepositoryNodeId::new("REPOSITORY".to_owned()).unwrap();
        let response = create_response(&operation, 1, "PR_ONE").into_value();
        for mutate in [
            |value: &mut Value| {
                value["errors"] = json!([{ "message": "partial" }]);
            },
            |value: &mut Value| {
                value["data"]["extra"] = Value::Null;
            },
            |value: &mut Value| {
                value["data"].as_object_mut().unwrap().remove("op0");
            },
        ] {
            let mut invalid = response.clone();
            mutate(&mut invalid);
            let invalid = UniqueJson::decode(&serde_json::to_vec(&invalid).unwrap()).unwrap();
            let (_, decoder) = create_batch(&repository_id, std::slice::from_ref(&operation))
                .unwrap()
                .into_request();
            assert!(decoder.decode(&repository_id, invalid).is_err());
        }

        let duplicate = br#"{"data":{"op0":null,"op0":null}}"#;
        assert!(UniqueJson::decode(duplicate).is_err());
    }

    #[test]
    fn all_batches_are_preflighted_at_exact_alias_and_byte_bounds() {
        let operations = (0..=MAX_MUTATION_ALIASES)
            .map(|index| create(&format!("G{index}")))
            .collect::<Vec<_>>();
        let prepared = PreparedCreates::new(
            RepositoryNodeId::new("REPOSITORY".to_owned()).unwrap(),
            operations,
            PullRequestIdentityRegistry::default(),
        )
        .unwrap();
        assert_eq!(prepared.batches.len(), 2);
        assert_eq!(prepared.batches[0].expected.len(), MAX_MUTATION_ALIASES);
        assert_eq!(prepared.batches[1].expected.len(), 1);
        assert!(
            prepared
                .batches
                .iter()
                .all(|batch| batch.serialized_bytes <= MAX_MUTATION_REQUEST_BYTES)
        );

        let mut exact = create("Gexact");
        let repository_id = RepositoryNodeId::new("REPOSITORY".to_owned()).unwrap();
        exact.body.clear();
        let fixed =
            create_batch(&repository_id, std::slice::from_ref(&exact)).unwrap().serialized_bytes;
        exact.body = "x".repeat(MAX_MUTATION_REQUEST_BYTES - fixed);
        assert_eq!(
            create_batch(&repository_id, std::slice::from_ref(&exact)).unwrap().serialized_bytes,
            MAX_MUTATION_REQUEST_BYTES
        );
        exact.body.push('x');
        assert!(
            PreparedCreates::new(
                repository_id,
                vec![exact],
                PullRequestIdentityRegistry::default()
            )
            .is_err()
        );
    }

    #[test]
    fn update_batches_use_the_same_exact_alias_and_byte_bounds() {
        let operations = (1..=MAX_MUTATION_ALIASES + 1)
            .map(|number| {
                UpdatePullRequest::new(
                    PullRequestIdentity::new(u64::try_from(number).unwrap(), format!("PR{number}"))
                        .unwrap(),
                    Some(format!("title {number}")),
                    None,
                    None,
                )
                .unwrap()
            })
            .collect::<Vec<_>>();
        let prepared = PreparedUpdates::new(operations).unwrap();
        assert_eq!(prepared.batches.len(), 2);
        assert_eq!(prepared.batches[0].expected.len(), MAX_MUTATION_ALIASES);
        assert_eq!(prepared.batches[1].expected.len(), 1);

        let mut exact = UpdatePullRequest::new(
            PullRequestIdentity::new(1, "PR1".to_owned()).unwrap(),
            None,
            Some(String::new()),
            None,
        )
        .unwrap();
        let fixed = update_batch(std::slice::from_ref(&exact)).unwrap().serialized_bytes;
        exact.body = Some("x".repeat(MAX_MUTATION_REQUEST_BYTES - fixed));
        assert_eq!(
            update_batch(std::slice::from_ref(&exact)).unwrap().serialized_bytes,
            MAX_MUTATION_REQUEST_BYTES
        );
        exact.body.as_mut().unwrap().push('x');
        assert!(PreparedUpdates::new(vec![exact]).is_err());

        let mut late_oversized = (1..=MAX_MUTATION_ALIASES)
            .map(|number| {
                UpdatePullRequest::new(
                    PullRequestIdentity::new(
                        u64::try_from(number).unwrap(),
                        format!("LATE{number}"),
                    )
                    .unwrap(),
                    Some(format!("title {number}")),
                    None,
                    None,
                )
                .unwrap()
            })
            .collect::<Vec<_>>();
        late_oversized.push(
            UpdatePullRequest::new(
                PullRequestIdentity::new(10_000, "OVERSIZED".to_owned()).unwrap(),
                None,
                Some("x".repeat(MAX_MUTATION_REQUEST_BYTES)),
                None,
            )
            .unwrap(),
        );
        assert!(PreparedUpdates::new(late_oversized).is_err());
    }

    #[test]
    fn mutation_preflight_accepts_empty_updates_and_rejects_invalid_actions() {
        let repository_id = RepositoryNodeId::new("REPOSITORY".to_owned()).unwrap();
        assert!(
            PreparedCreates::new(
                repository_id.clone(),
                Vec::new(),
                PullRequestIdentityRegistry::default(),
            )
            .is_err()
        );
        assert!(
            PreparedCreates::new(
                repository_id,
                vec![create("Gone"), create("Gone")],
                PullRequestIdentityRegistry::default(),
            )
            .is_err()
        );

        assert!(PreparedUpdates::new(Vec::new()).unwrap().operations_for_test().is_empty());
        let one = PullRequestIdentity::new(1, "ONE".to_owned()).unwrap();
        assert!(UpdatePullRequest::new(one.clone(), None, None, None).is_err());
        let same_number = PullRequestIdentity::new(1, "TWO".to_owned()).unwrap();
        let same_node = PullRequestIdentity::new(2, "ONE".to_owned()).unwrap();
        let update = |identity| {
            UpdatePullRequest::new(identity, Some("title".to_owned()), None, None).unwrap()
        };
        assert!(PreparedUpdates::new(vec![update(one.clone()), update(same_number)]).is_err());
        assert!(PreparedUpdates::new(vec![update(one), update(same_node)]).is_err());
    }

    #[test]
    fn update_document_is_an_exact_json_escaped_graphql_operation() {
        let operation = UpdatePullRequest::new(
            PullRequestIdentity::new(7, "PR_\"SEVEN".to_owned()).unwrap(),
            Some("title \" seven".to_owned()),
            Some("line one\nline two".to_owned()),
            Some("base/\"seven".to_owned()),
        )
        .unwrap();
        insta::assert_snapshot!(operation.document(), @r###"updatePullRequest(input: { pullRequestId: "PR_\"SEVEN", baseRefName: "base/\"seven", title: "title \" seven", body: "line one\nline two", clientMutationId: "gherrit:update:PR_\"SEVEN" }) { clientMutationId, pullRequest { number, id } }"###);
    }

    #[test]
    fn update_receipt_requires_the_exact_coupled_identity() {
        let identity = PullRequestIdentity::new(7, "PR_SEVEN".to_owned()).unwrap();
        let operation = UpdatePullRequest::new(
            identity.clone(),
            Some("new title".to_owned()),
            None,
            Some("main".to_owned()),
        )
        .unwrap();
        let batch = update_batch(std::slice::from_ref(&operation)).unwrap();
        let (_, decoder) = batch.into_request();
        let response = json!({
            "data": {
                "op0": {
                    "clientMutationId": update_client_mutation_id(&operation.identity),
                    "pullRequest": { "number": 7, "id": "PR_SEVEN" },
                },
            },
        });
        let response = UniqueJson::decode(&serde_json::to_vec(&response).unwrap()).unwrap();
        decoder.decode(response).unwrap();

        let valid = json!({
            "data": {
                "op0": {
                    "clientMutationId": update_client_mutation_id(&operation.identity),
                    "pullRequest": { "number": 7, "id": "PR_SEVEN" },
                },
            },
        });
        for (pointer, replacement) in [
            ("/data/op0/clientMutationId", json!("wrong")),
            ("/data/op0/pullRequest", Value::Null),
            ("/data/op0/pullRequest/number", json!(0)),
            ("/data/op0/pullRequest/number", json!(8)),
            ("/data/op0/pullRequest/id", json!("")),
            ("/data/op0/pullRequest/id", json!("PR_OTHER")),
        ] {
            let (_, decoder) =
                update_batch(std::slice::from_ref(&operation)).unwrap().into_request();
            let mut response = valid.clone();
            *response.pointer_mut(pointer).unwrap() = replacement;
            let response = UniqueJson::decode(&serde_json::to_vec(&response).unwrap()).unwrap();
            assert!(decoder.decode(response).is_err(), "accepted mutation at {pointer}");
        }
    }
}

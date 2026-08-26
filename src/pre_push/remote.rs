//! Exact remote Git evidence for the local change set.
//!
//! Git's ref-pattern matching does not turn an omitted record into proof that
//! a ref is absent. This module first plans every bounded query, then decodes
//! one response per query. The first response must repeat the default branch
//! at the tip from which the local stack was derived. Only after that check do
//! missing requested refs become exact absence evidence.
//!
//! The decoded values deliberately remain structural. This layer validates
//! advertisement framing, ref ownership, and canonical names, but it does not
//! decide whether version history is contiguous or whether heads, bases, and
//! markers describe a valid published change. A later domain layer consumes
//! the complete raw tuple and makes those decisions.

use std::{
    collections::{BTreeMap, HashMap, HashSet},
    str,
};

use color_eyre::eyre::{Context as _, Result, bail, eyre};
use gix::{ObjectId, bstr::ByteSlice as _};

use super::{
    destination::{DefaultBranch, ExactObjectFetchMode, PushDestination, git_output_records},
    history::{
        ExternalGraphRoot, GraphLoadError, PreparedExactLocalHistories, ValidatedChangeHistory,
    },
    local::{GherritPrId, LocalStack},
    subprocess::{
        self, REGULAR_FILE_STDIN_LIMIT, REMOTE_GIT_EXECUTION_TIMEOUT, RegularFileStdin,
        RegularFileStdinBuilder,
    },
    version::Version,
};
use crate::util;

// This stays well below Windows' roughly 32-KiB command-line limit and is a
// conservative bound on POSIX systems. The count includes one terminating
// byte per argument.
const QUERY_ARGV_BUDGET_BYTES: usize = 16 * 1024;
const HEAD_PREFIX: &[u8] = b"refs/heads/";
const OWNED_BASE_PREFIX: &[u8] = b"refs/heads/gherrit-bases/";
const MANAGED_TAG_ROOT: &[u8] = b"refs/tags/gherrit";
const MANAGED_TAG_PREFIX: &[u8] = b"refs/tags/gherrit/";

/// A complete, bounded request plan for exactly the supplied local IDs.
///
/// Construction validates all request arguments before any caller can execute
/// the first query. Responses must later be decoded in this same order.
struct QueryPlan {
    default_branch: DefaultBranch,
    queries: Vec<Query>,
}

/// Exact request plan bound to the sealed stack which authorized it.
struct ExactLocalQueryPlan<'local> {
    local: &'local LocalStack,
    plan: QueryPlan,
}

impl QueryPlan {
    #[cfg(test)]
    pub(super) fn new(default_branch: DefaultBranch, ids: &[GherritPrId]) -> Result<Self> {
        Self::with_budget(default_branch, ids, QUERY_ARGV_BUDGET_BYTES)
    }

    fn with_budget(
        default_branch: DefaultBranch,
        ids: &[GherritPrId],
        budget: usize,
    ) -> Result<Self> {
        if ids.is_empty() {
            bail!("exact local remote observation requires at least one change");
        }
        if ids.iter().any(|id| id.as_str() == default_branch.name()) {
            bail!("a local GHerrit change aliases the repository default branch");
        }

        let default_pattern_bytes = default_branch.full_ref_name().len() + 1;
        let local_budget = budget.checked_sub(default_pattern_bytes).ok_or_else(|| {
            eyre!("The repository default branch is too long for exact remote observation")
        })?;
        if local_budget == 0 {
            bail!("The repository default branch is too long for exact remote observation");
        }

        let queries = plan_queries(ids, local_budget)?;
        Ok(Self { default_branch, queries })
    }

    /// Returns one exact ref-pattern vector per required network request.
    ///
    /// Only the first query repeats the default branch. Every other argument
    /// names a requested change's candidate head, owned base, or complete
    /// managed-tag namespace, including its pull-request marker.
    #[cfg(test)]
    fn patterns(&self) -> impl ExactSizeIterator<Item = Vec<String>> + '_ {
        self.queries
            .iter()
            .enumerate()
            .map(|(index, query)| query.patterns((index == 0).then_some(&self.default_branch)))
    }

    /// Decodes one successful, complete response per planned query.
    ///
    /// Process success is intentionally not represented here. The eventual
    /// command boundary must admit only stdout from a successful query before
    /// calling this pure decoder.
    #[cfg(test)]
    pub(super) fn decode<'output>(
        self,
        outputs: impl IntoIterator<Item = &'output [u8]>,
    ) -> Result<RawExactLocalObservation> {
        let Self { default_branch, queries } = self;
        let mut outputs = outputs.into_iter();
        let mut changes = Vec::new();

        for (index, query) in queries.iter().enumerate() {
            let output = outputs.next().ok_or_else(|| {
                eyre!("exact local remote observation omitted a planned query response")
            })?;
            let expected_default = (index == 0).then_some(&default_branch);
            changes.extend(parse_query(output, query.ids(), expected_default)?);
        }
        if outputs.next().is_some() {
            bail!("exact local remote observation supplied an unplanned query response");
        }

        Ok(RawExactLocalObservation { default_branch, changes: changes.into_boxed_slice() })
    }
}

#[cfg(test)]
pub(super) fn decode_for_test<'output>(
    default_branch: DefaultBranch,
    ids: &[GherritPrId],
    outputs: impl IntoIterator<Item = &'output [u8]>,
) -> Result<RawExactLocalObservation> {
    QueryPlan::new(default_branch, ids)?.decode(outputs)
}

impl<'local> ExactLocalQueryPlan<'local> {
    /// Binds exact remote acquisition to the path origin and ordered IDs
    /// already proved by the sealed local stack.
    fn for_stack(stack: &'local LocalStack) -> Result<Self> {
        let ids = stack.iter().map(|change| change.id().clone()).collect::<Vec<_>>();
        let plan =
            QueryPlan::with_budget(stack.default_branch().clone(), &ids, QUERY_ARGV_BUDGET_BYTES)?;
        Ok(Self { local: stack, plan })
    }

    #[cfg(test)]
    pub(super) fn patterns(&self) -> impl ExactSizeIterator<Item = Vec<String>> + '_ {
        self.plan.patterns()
    }

    /// Runs every preplanned exact query sequentially and seals its stack,
    /// repository, and destination into the resulting acquisition authority.
    async fn observe<'repository, 'destination>(
        self,
        repository: &'repository util::Repo,
        destination: &'destination PushDestination,
    ) -> Result<ExactLocalObservation<'local, 'repository, 'destination>> {
        let QueryPlan { default_branch, queries } = self.plan;
        let mut changes = Vec::new();
        for (index, query) in queries.iter().enumerate() {
            let expected_default = (index == 0).then_some(&default_branch);
            let patterns = query.patterns(expected_default);
            let output =
                destination.observe_refs_from(repository, patterns).await.wrap_err_with(|| {
                    format!(
                        "Failed to observe exact Git state for GHerrit remote '{}'",
                        destination.configured_remote()
                    )
                })?;
            if !output.status().success() {
                return Err(remote_command_failure(
                    format!(
                        "Exact Git observation failed for GHerrit remote '{}'",
                        destination.configured_remote()
                    ),
                    &output,
                    destination,
                ));
            }
            changes.extend(parse_query(output.stdout(), query.ids(), expected_default)?);
        }
        let raw = RawExactLocalObservation { default_branch, changes: changes.into_boxed_slice() };
        Ok(ExactLocalObservation { local: self.local, repository, destination, raw })
    }
}

/// Exact ref evidence bound to the stack, repository, and destination which
/// produced it.
///
/// None of the borrows has an accessor. The only production transition loads,
/// optionally acquires, reloads, and validates histories through those exact
/// capabilities.
struct ExactLocalObservation<'local, 'repository, 'destination> {
    local: &'local LocalStack,
    repository: &'repository util::Repo,
    destination: &'destination PushDestination,
    raw: RawExactLocalObservation,
}

impl ExactLocalObservation<'_, '_, '_> {
    /// Consumes this exact observation through one straight-line graph load.
    ///
    /// Complete and invalid evidence return without constructing acquisition
    /// input. A missing advertised root negotiates its exact aliases. A
    /// missing ancestor refetches that causal root only for a repository which
    /// was already promisor; an ordinary repository fails before network I/O.
    /// One successful fetch permits exactly one authoritative final reload.
    async fn validate_histories(self) -> Result<Box<[ValidatedChangeHistory]>> {
        let Self { local, repository, destination, raw } = self;
        let prepared = PreparedExactLocalHistories::prepare(raw, local, repository)?;
        let request = match prepared.load_graph() {
            Ok(graph) => return prepared.validate(&graph),
            Err(GraphLoadError::Invalid(error)) => return Err(error),
            Err(GraphLoadError::MissingAdvertisedRoot(missing)) => {
                ExactAcquisition::Negotiated(missing.root())
            }
            Err(GraphLoadError::MissingAncestor(missing)) => {
                if !repository.has_promisor_remote()? {
                    bail!(
                        "The ordinary local Git object database is incomplete or corrupt: commit {} is missing from ancestry of advertised graph root {}; ordinary fetch negotiation cannot repair missing ancestry",
                        missing.oid(),
                        missing.root().oid()
                    );
                }
                ExactAcquisition::Refetch(missing.root())
            }
        };

        acquire(repository, destination, request).await?;
        // This second and final load is authoritative. There is deliberately
        // no loop or reclassification after the one acquisition attempt.
        let graph = prepared.load_graph().map_err(GraphLoadError::into_report)?;
        prepared.validate(&graph)
    }
}

/// Obtains and validates the complete remote Git history for the local stack.
///
/// This is the only operation exposed to production callers. Its private
/// intermediate types prevent callers from retaining, relabelling, or
/// combining raw ref evidence from separate observations, or from changing
/// the authorized stack or destination before validation. If advertised
/// history is absent locally, this may perform one bounded acquisition which
/// adds objects but does not update any local ref.
pub(super) async fn observe_and_validate_histories(
    local: &LocalStack,
    repository: &util::Repo,
    destination: &PushDestination,
) -> Result<Box<[ValidatedChangeHistory]>> {
    ExactLocalQueryPlan::for_stack(local)?
        .observe(repository, destination)
        .await?
        .validate_histories()
        .await
}

async fn acquire(
    repository: &util::Repo,
    destination: &PushDestination,
    request: ExactAcquisition<'_>,
) -> Result<()> {
    let (root, mode) = request.into_parts();
    let input = prepare_fetch_input(root)?;
    let mut command = destination.exact_object_fetch(mode);
    command.current_dir(repository.workdir().unwrap_or(repository.path()));
    let output =
        subprocess::output_with_regular_file_stdin(command, input, REMOTE_GIT_EXECUTION_TIMEOUT)
            .await
            .wrap_err_with(|| {
                format!(
                    "Failed to acquire exact Git history for GHerrit remote '{}'",
                    destination.configured_remote()
                )
            })?;
    if !output.status().success() {
        return Err(remote_command_failure(
            format!(
                "Exact Git history acquisition failed for GHerrit remote '{}'",
                destination.configured_remote()
            ),
            &output,
            destination,
        ));
    }
    Ok(())
}

/// Produces the only caller-visible form of a remote Git child failure.
///
/// Process status is evidence that the operation failed, but child output is
/// neither trustworthy nor publication evidence. The subprocess boundary
/// retains only a bounded suffix, and the destination applies conservative
/// redaction and terminal escaping before this function can display it.
fn remote_command_failure(
    message: String,
    output: &subprocess::CommandOutput,
    destination: &PushDestination,
) -> color_eyre::Report {
    output.child_diagnostic(destination).map_or_else(
        || eyre!(message.clone()),
        |diagnostic| {
            eyre!(
                "{message}\n\nRemote Git diagnostic (untrusted and not publication evidence):\n{diagnostic}"
            )
        },
    )
}

enum ExactAcquisition<'root> {
    Negotiated(&'root ExternalGraphRoot),
    Refetch(&'root ExternalGraphRoot),
}

impl<'root> ExactAcquisition<'root> {
    fn into_parts(self) -> (&'root ExternalGraphRoot, ExactObjectFetchMode) {
        match self {
            Self::Negotiated(root) => (root, ExactObjectFetchMode::Negotiated),
            Self::Refetch(root) => (root, ExactObjectFetchMode::Refetch),
        }
    }
}

fn prepare_fetch_input(root: &ExternalGraphRoot) -> Result<RegularFileStdin> {
    checked_payload_length(
        root.source_refs().map(|source_ref| {
            u64::try_from(source_ref.len())
                .map_err(|_| eyre!("Exact acquisition source-ref length overflowed"))
        }),
        REGULAR_FILE_STDIN_LIMIT,
    )?;

    // Validate the total size before creating or writing the named temporary
    // file. The builder owns GHerrit's writable handle and removes the name
    // before ownership crosses the subprocess boundary.
    let mut input = RegularFileStdinBuilder::new()?;
    write_fetch_payload(root, &mut input)
        .wrap_err("Failed to prepare bounded exact Git acquisition input")?;
    input.finish().map_err(Into::into)
}

fn write_fetch_payload(
    root: &ExternalGraphRoot,
    output: &mut impl std::io::Write,
) -> std::io::Result<()> {
    for source_ref in root.source_refs() {
        output.write_all(source_ref.as_bytes())?;
        output.write_all(b"\n")?;
    }
    Ok(())
}

fn checked_payload_length(
    lengths: impl IntoIterator<Item = Result<u64>>,
    limit: u64,
) -> Result<u64> {
    let mut total = 0_u64;
    for length in lengths {
        total = total
            .checked_add(length?)
            .and_then(|total| total.checked_add(1))
            .ok_or_else(|| eyre!("Exact acquisition stdin length overflowed"))?;
        if total > limit {
            bail!("Exact acquisition stdin exceeds the {limit}-byte limit");
        }
    }
    Ok(total)
}

/// Exact remote ref evidence for the requested local IDs, in request order.
#[derive(Debug)]
pub(super) struct RawExactLocalObservation {
    default_branch: DefaultBranch,
    changes: Box<[RawExactLocalChange]>,
}

impl RawExactLocalObservation {
    pub(super) fn into_parts(self) -> (DefaultBranch, Box<[RawExactLocalChange]>) {
        (self.default_branch, self.changes)
    }

    #[cfg(test)]
    pub(super) fn default_branch(&self) -> &DefaultBranch {
        &self.default_branch
    }

    #[cfg(test)]
    pub(super) fn iter(&self) -> impl ExactSizeIterator<Item = &RawExactLocalChange> {
        self.changes.iter()
    }
}

/// The unnormalized refs advertised for one requested change.
///
/// An absent field means that its exact ref or namespace was requested and no
/// corresponding record appeared after the default-branch recheck succeeded.
#[derive(Debug)]
pub(super) struct RawExactLocalChange {
    id: GherritPrId,
    candidate_head: Option<ObjectId>,
    owned_base: Option<ObjectId>,
    versions: Box<[RawVersionRef]>,
    pull_request_marker: Option<ObjectId>,
}

/// The components of one consumed raw exact-local change.
pub(super) struct RawExactLocalChangeParts {
    pub(super) id: GherritPrId,
    pub(super) candidate_head: Option<ObjectId>,
    pub(super) owned_base: Option<ObjectId>,
    pub(super) versions: Box<[RawVersionRef]>,
    pub(super) pull_request_marker: Option<ObjectId>,
}

impl RawExactLocalChange {
    pub(super) fn into_parts(self) -> RawExactLocalChangeParts {
        RawExactLocalChangeParts {
            id: self.id,
            candidate_head: self.candidate_head,
            owned_base: self.owned_base,
            versions: self.versions,
            pull_request_marker: self.pull_request_marker,
        }
    }

    #[cfg(test)]
    pub(super) fn id(&self) -> &GherritPrId {
        &self.id
    }

    #[cfg(test)]
    pub(super) fn candidate_head(&self) -> Option<ObjectId> {
        self.candidate_head
    }

    #[cfg(test)]
    pub(super) fn owned_base(&self) -> Option<ObjectId> {
        self.owned_base
    }

    #[cfg(test)]
    pub(super) fn versions(&self) -> impl ExactSizeIterator<Item = &RawVersionRef> {
        self.versions.iter()
    }

    #[cfg(test)]
    pub(super) fn pull_request_marker(&self) -> Option<ObjectId> {
        self.pull_request_marker
    }
}

/// One exact, canonical lightweight version ref from an advertisement.
#[derive(Debug)]
pub(super) struct RawVersionRef {
    version: Version,
    object_id: ObjectId,
    source_ref: Box<str>,
}

impl RawVersionRef {
    pub(super) fn into_parts(self) -> (Version, ObjectId, Box<str>) {
        (self.version, self.object_id, self.source_ref)
    }

    #[cfg(test)]
    pub(super) fn version(&self) -> Version {
        self.version
    }

    #[cfg(test)]
    pub(super) fn object_id(&self) -> ObjectId {
        self.object_id
    }

    #[cfg(test)]
    pub(super) fn source_ref(&self) -> &str {
        &self.source_ref
    }
}

#[derive(Debug, Eq, PartialEq)]
struct Query {
    first: GherritPrId,
    rest: Vec<GherritPrId>,
}

impl Query {
    fn new(first: GherritPrId) -> Self {
        Self { first, rest: Vec::new() }
    }

    fn ids(&self) -> impl Iterator<Item = &GherritPrId> {
        std::iter::once(&self.first).chain(&self.rest)
    }

    fn patterns(&self, default_branch: Option<&DefaultBranch>) -> Vec<String> {
        default_branch
            .map(DefaultBranch::full_ref_name)
            .into_iter()
            .chain(self.ids().flat_map(local_patterns))
            .collect()
    }
}

fn plan_queries(ids: &[GherritPrId], budget: usize) -> Result<Vec<Query>> {
    let mut seen = HashSet::new();
    let planned = ids
        .iter()
        .map(|id| {
            if !seen.insert(id.as_str()) {
                bail!("remote observation requested the same GHerrit change twice");
            }
            let bytes = local_pattern_bytes(id);
            if bytes > budget {
                bail!(
                    "GHerrit change ID is too long for a remote observation query ({} bytes)",
                    id.as_str().len()
                );
            }
            Ok((id.clone(), bytes))
        })
        .collect::<Result<Vec<_>>>()?;

    let mut queries = Vec::new();
    let mut current = None::<Query>;
    let mut current_bytes = 0;
    for (id, bytes) in planned {
        if current.is_some() && current_bytes > budget - bytes {
            queries.push(current.take().expect("a full query exists"));
            current_bytes = 0;
        }
        current_bytes += bytes;
        match &mut current {
            Some(query) => query.rest.push(id),
            None => current = Some(Query::new(id)),
        }
    }
    if let Some(query) = current {
        queries.push(query);
    }
    Ok(queries)
}

fn local_patterns(id: &GherritPrId) -> [String; 4] {
    let tag_root = format!("refs/tags/gherrit/{}", id.as_str());
    [
        format!("refs/heads/{}", id.as_str()),
        format!("refs/heads/gherrit-bases/{}", id.as_str()),
        tag_root.clone(),
        format!("{tag_root}/*"),
    ]
}

fn local_pattern_bytes(id: &GherritPrId) -> usize {
    local_patterns(id).iter().map(|pattern| pattern.len() + 1).sum()
}

#[derive(Default)]
struct PendingChange {
    candidate_head: Option<ObjectId>,
    owned_base: Option<ObjectId>,
    versions: BTreeMap<Version, RawVersionRef>,
    pull_request_marker: Option<ObjectId>,
}

fn parse_query<'id>(
    output: &[u8],
    ids: impl Iterator<Item = &'id GherritPrId>,
    expected_default: Option<&DefaultBranch>,
) -> Result<Vec<RawExactLocalChange>> {
    let ids = ids.collect::<Vec<_>>();
    let requested = ids
        .iter()
        .enumerate()
        .map(|(index, id)| (id.as_str().as_bytes().to_vec(), index))
        .collect::<HashMap<_, _>>();
    debug_assert_eq!(requested.len(), ids.len());
    let mut pending = (0..ids.len()).map(|_| PendingChange::default()).collect::<Vec<_>>();
    let expected_default_name =
        expected_default.map(|default| default.full_ref_name().into_bytes());
    let mut observed_default = false;
    let mut seen_records = HashSet::new();

    for record in records(output) {
        let record = record?;
        if !seen_records.insert((record.name.to_vec(), record.peeled)) {
            bail!("exact local observation returned the same remote ref record more than once");
        }
        if expected_default_name.as_deref() == Some(record.name) {
            if observed_default {
                bail!("exact local observation returned the default branch more than once");
            }
            if record.peeled {
                bail!("exact local observation returned a peeled default branch");
            }
            let expected = expected_default.expect("a default name came from a default value");
            if record.object_id != expected.tip() {
                bail!("the default branch moved after local stack derivation");
            }
            observed_default = true;
            continue;
        }

        if let Some(id) = parse_owned_base_name(record.name)? {
            let index = requested.get(id.as_str().as_bytes()).ok_or_else(|| {
                eyre!("exact local observation returned an unrequested GHerrit owned base")
            })?;
            if record.peeled {
                bail!("exact local observation returned a peeled owned base");
            }
            if pending[*index].owned_base.replace(record.object_id).is_some() {
                bail!("exact local observation returned the same owned base more than once");
            }
            continue;
        }

        if let Some(id) = parse_top_level_change_head(record.name) {
            let index = requested.get(id.as_str().as_bytes()).ok_or_else(|| {
                eyre!("exact local observation returned an unrequested GHerrit head")
            })?;
            if record.peeled {
                bail!("exact local observation returned a peeled candidate head");
            }
            if pending[*index].candidate_head.replace(record.object_id).is_some() {
                bail!("exact local observation returned the same candidate head more than once");
            }
            continue;
        }

        if record.name == MANAGED_TAG_ROOT {
            bail!("remote ref uses the managed-tag namespace root");
        }
        if let Some(component) =
            record.name.strip_prefix(MANAGED_TAG_PREFIX).filter(|suffix| !suffix.contains(&b'/'))
        {
            let id = GherritPrId::from_ref_component(component)
                .wrap_err("remote managed-tag namespace root has an invalid change ID")?;
            if !requested.contains_key(id.as_str().as_bytes()) {
                bail!("remote advertised a managed-tag namespace for an unrequested change");
            }
            bail!("remote managed-tag namespace root exists for a requested GHerrit change");
        }
        if let Some((id, tag)) = parse_managed_tag_name(record.name)? {
            let index = requested.get(id.as_str().as_bytes()).ok_or_else(|| {
                eyre!("remote advertised managed tags for an unrequested GHerrit change")
            })?;
            if record.peeled {
                bail!(
                    "remote managed tag for GHerrit change '{}' is annotated rather than lightweight",
                    id.as_str()
                );
            }
            let change = &mut pending[*index];
            match tag {
                ManagedTag::Version(version) => {
                    let source_ref = str::from_utf8(record.name)
                        .expect("a validated managed version ref is ASCII")
                        .into();
                    let version_ref =
                        RawVersionRef { version, object_id: record.object_id, source_ref };
                    if change.versions.insert(version, version_ref).is_some() {
                        bail!(
                            "remote advertised version v{version} for GHerrit change '{}' more than once",
                            id.as_str()
                        );
                    }
                }
                ManagedTag::PullRequestMarker => {
                    if change.pull_request_marker.replace(record.object_id).is_some() {
                        bail!(
                            "remote advertised the pull-request marker for GHerrit change '{}' more than once",
                            id.as_str()
                        );
                    }
                }
            }
            continue;
        }

        // Ref patterns match a slash-delimited tail, not necessarily a full
        // ref name. An archival namespace can therefore legitimately produce
        // a validated record such as
        // `refs/heads/archive/refs/heads/Gone`. It is not GHerrit-owned state
        // and must neither populate nor invalidate the exact requested tuple.
        if is_requested_tail_match(record.name, &ids, expected_default) {
            continue;
        }
        bail!("exact local observation returned an unrelated remote ref");
    }

    if expected_default.is_some() && !observed_default {
        bail!("exact local observation omitted the default branch");
    }

    Ok(ids
        .into_iter()
        .zip(pending)
        .map(|(id, pending)| RawExactLocalChange {
            id: id.clone(),
            candidate_head: pending.candidate_head,
            owned_base: pending.owned_base,
            versions: pending.versions.into_values().collect(),
            pull_request_marker: pending.pull_request_marker,
        })
        .collect())
}

fn is_requested_tail_match(
    name: &[u8],
    ids: &[&GherritPrId],
    expected_default: Option<&DefaultBranch>,
) -> bool {
    if expected_default
        .is_some_and(|default| has_exact_ref_tail(name, default.full_ref_name().as_bytes()))
    {
        return true;
    }

    ids.iter().any(|id| {
        let [candidate, owned_base, tag_root, _] = local_patterns(id);
        has_exact_ref_tail(name, candidate.as_bytes())
            || has_exact_ref_tail(name, owned_base.as_bytes())
            || has_exact_ref_tail(name, tag_root.as_bytes())
            || has_namespace_tail(name, tag_root.as_bytes())
    })
}

fn has_exact_ref_tail(name: &[u8], tail: &[u8]) -> bool {
    name.strip_suffix(tail).is_some_and(|prefix| prefix.is_empty() || prefix.ends_with(b"/"))
}

fn has_namespace_tail(name: &[u8], root: &[u8]) -> bool {
    std::iter::once(0)
        .chain(
            name.iter()
                .enumerate()
                .filter_map(|(index, byte)| (*byte == b'/').then_some(index + 1)),
        )
        .any(|index| {
            name[index..]
                .strip_prefix(root)
                .and_then(|suffix| suffix.strip_prefix(b"/"))
                .is_some_and(|leaf| !leaf.is_empty())
        })
}

fn parse_top_level_change_head(name: &[u8]) -> Option<GherritPrId> {
    let id = name.strip_prefix(HEAD_PREFIX)?;
    (!id.contains(&b'/')).then(|| GherritPrId::from_ref_component(id).ok()).flatten()
}

fn parse_owned_base_name(name: &[u8]) -> Result<Option<GherritPrId>> {
    let Some(id) = name.strip_prefix(OWNED_BASE_PREFIX) else {
        return Ok(None);
    };
    let id = GherritPrId::from_ref_component(id)
        .wrap_err("remote owned-base ref has an invalid change ID")?;
    Ok(Some(id))
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ManagedTag {
    Version(Version),
    PullRequestMarker,
}

fn parse_managed_tag_name(name: &[u8]) -> Result<Option<(GherritPrId, ManagedTag)>> {
    let Some(suffix) = name.strip_prefix(MANAGED_TAG_PREFIX) else {
        return Ok(None);
    };
    let mut components = suffix.split(|byte| *byte == b'/');
    let (Some(id), Some(leaf), None) = (components.next(), components.next(), components.next())
    else {
        bail!("remote managed tag does not have the canonical change/(vN|pr) shape");
    };
    let id = GherritPrId::from_ref_component(id)
        .wrap_err("remote managed tag has an invalid change ID")?;
    let tag = if leaf == b"pr" {
        ManagedTag::PullRequestMarker
    } else {
        ManagedTag::Version(parse_version(leaf).wrap_err("remote managed tag is not canonical")?)
    };
    Ok(Some((id, tag)))
}

struct Record<'output> {
    object_id: ObjectId,
    name: &'output [u8],
    peeled: bool,
}

fn records(output: &[u8]) -> impl Iterator<Item = Result<Record<'_>>> {
    git_output_records(output).enumerate().map(|(index, record)| {
        let mut fields = record.split(|byte| *byte == b'\t');
        let (Some(value), Some(name), None) = (fields.next(), fields.next(), fields.next()) else {
            bail!("malformed `git ls-remote` record {} ({} bytes)", index + 1, record.len());
        };
        if value.starts_with(b"ref: ") {
            bail!("exact local observation unexpectedly contained a symbolic ref");
        }
        if value.len() != 40
            || !value.iter().all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f'))
        {
            bail!("remote ref value is not a canonical SHA-1 object ID");
        }
        if value.iter().all(|byte| *byte == b'0') {
            bail!("remote ref has a null object ID");
        }

        let (logical_name, peeled) = match name.strip_suffix(b"^{}") {
            Some(name) => (name, true),
            None => (name, false),
        };
        let full_name = gix::refs::FullName::try_from(logical_name.as_bstr())
            .wrap_err("remote ref has an invalid name")?;
        if peeled && full_name.category() != Some(gix::refs::Category::Tag) {
            bail!("peeled remote ref is not a tag");
        }
        let object_id =
            ObjectId::from_hex(value).wrap_err("remote ref value is not a SHA-1 object ID")?;
        Ok(Record { object_id, name: logical_name, peeled })
    })
}

fn parse_version(suffix: &[u8]) -> Result<Version> {
    let digits =
        suffix.strip_prefix(b"v").ok_or_else(|| eyre!("version does not use the vN form"))?;
    if digits.is_empty() || !digits.iter().all(u8::is_ascii_digit) {
        bail!("version is not decimal");
    }
    if digits[0] == b'0' {
        bail!("version is zero or has a leading zero");
    }
    let value = digits.iter().try_fold(0_u64, |value, digit| {
        value
            .checked_mul(10)
            .and_then(|value| value.checked_add(u64::from(*digit - b'0')))
            .ok_or_else(|| eyre!("version overflows u64"))
    })?;
    Version::new(value).ok_or_else(|| eyre!("version is zero"))
}

#[cfg(test)]
mod tests {
    use std::{
        env, fs,
        path::{Path, PathBuf},
        time::Duration,
    };

    use super::*;
    use crate::util;

    const DEFAULT: &str = "1111111111111111111111111111111111111111";
    const HEAD: &str = "2222222222222222222222222222222222222222";
    const BASE: &str = "3333333333333333333333333333333333333333";
    const MARKER: &str = "4444444444444444444444444444444444444444";
    const SHA256: &str = "5555555555555555555555555555555555555555555555555555555555555555";

    const EMPTY_TREE: &str = "4b825dc642cb6eb9a060e54bf8d69288fbee4904";
    const REAL_MODE: &str = "GHERRIT_375_REAL_MODE";
    const REAL_REMOTE: &str = "GHERRIT_375_REAL_REMOTE";
    const REAL_ROOT: &str = "GHERRIT_375_REAL_ROOT";
    const REAL_TEST: &str = "pre_push::remote::tests::real_boundary_reexec";
    const REAL_TEST_TIMEOUT: Duration = Duration::from_secs(10);

    fn id(value: &str) -> GherritPrId {
        GherritPrId::from_ref_component(value.as_bytes()).unwrap()
    }

    fn default_branch() -> DefaultBranch {
        DefaultBranch::new("main".to_owned(), ObjectId::from_hex(DEFAULT.as_bytes()).unwrap())
            .unwrap()
    }

    #[derive(Clone, Copy)]
    enum RealMode {
        Complete,
        Negotiated,
        Refetch,
        Ordinary,
    }

    impl RealMode {
        fn as_str(self) -> &'static str {
            match self {
                Self::Complete => "complete",
                Self::Negotiated => "negotiated",
                Self::Refetch => "refetch",
                Self::Ordinary => "ordinary",
            }
        }

        fn parse(value: &str) -> Self {
            match value {
                "complete" => Self::Complete,
                "negotiated" => Self::Negotiated,
                "refetch" => Self::Refetch,
                "ordinary" => Self::Ordinary,
                other => panic!("unknown real-Git boundary mode {other}"),
            }
        }
    }

    fn git_output(
        context: &testutil::TestContext,
        args: impl IntoIterator<Item = impl AsRef<std::ffi::OsStr>>,
    ) -> std::process::Output {
        let args = args.into_iter().map(|arg| arg.as_ref().to_owned()).collect::<Vec<_>>();
        let output = context.git_cmd().args(&args).output().unwrap();
        assert!(output.status.success(), "git failed with args {args:?}: {output:?}");
        output
    }

    fn run_git(
        context: &testutil::TestContext,
        args: impl IntoIterator<Item = impl AsRef<std::ffi::OsStr>>,
    ) {
        let _ = git_output(context, args);
    }

    fn git_stdout(
        context: &testutil::TestContext,
        args: impl IntoIterator<Item = impl AsRef<std::ffi::OsStr>>,
    ) -> String {
        String::from_utf8(git_output(context, args).stdout).unwrap().trim().to_owned()
    }

    fn commit(
        context: &testutil::TestContext,
        directory: &Path,
        parent: ObjectId,
        message: &str,
    ) -> ObjectId {
        let arguments = vec![
            "-C".to_owned(),
            directory.to_string_lossy().into_owned(),
            "commit-tree".to_owned(),
            EMPTY_TREE.to_owned(),
            "-p".to_owned(),
            parent.to_string(),
            "-m".to_owned(),
            message.to_owned(),
        ];
        ObjectId::from_hex(git_stdout(context, arguments).as_bytes()).unwrap()
    }

    struct RealFixture {
        context: testutil::TestContext,
        remote: PathBuf,
        seed: PathBuf,
        external_parent: ObjectId,
        external: ObjectId,
    }

    fn real_fixture() -> RealFixture {
        let context = testutil::TestContextBuilder::new(env::current_exe().unwrap())
            .with_remote()
            .with_initial_commit()
            .build();
        let remote = PathBuf::from(git_stdout(&context, ["remote", "get-url", "origin"]));
        let base = ObjectId::from_hex(context.head_oid().as_bytes()).unwrap();
        let proposal =
            commit(&context, &context.repo_path, base, "proposal\n\ngherrit-pr-id: Local\n");
        run_git(
            &context,
            ["update-ref".to_owned(), "refs/heads/proposal".to_owned(), proposal.to_string()],
        );

        let seed = context.dir.path().join("seed");
        run_git(
            &context,
            [
                "clone".to_owned(),
                remote.to_string_lossy().into_owned(),
                seed.to_string_lossy().into_owned(),
            ],
        );
        let external_parent = commit(&context, &seed, base, "external parent\n");
        let external =
            commit(&context, &seed, external_parent, "external\n\ngherrit-pr-id: Local\n");
        let unrelated_external =
            commit(&context, &seed, external, "unrelated external\n\ngherrit-pr-id: Second\n");
        run_git(
            &context,
            [
                "-C".to_owned(),
                seed.to_string_lossy().into_owned(),
                "push".to_owned(),
                "origin".to_owned(),
                format!("{external}:refs/heads/Local"),
                format!("{external_parent}:refs/heads/gherrit-bases/Local"),
                format!("{external}:refs/tags/gherrit/Local/v1"),
                format!("{unrelated_external}:refs/heads/Second"),
                format!("{external}:refs/heads/gherrit-bases/Second"),
                format!("{unrelated_external}:refs/tags/gherrit/Second/v1"),
            ],
        );

        RealFixture { context, remote, seed, external_parent, external }
    }

    fn git_config_value(path: &Path) -> String {
        format!("\"{}\"", path.to_string_lossy().replace('\\', "\\\\").replace('"', "\\\""))
    }

    struct HostileConfig {
        system: PathBuf,
        global: PathBuf,
        bundle: PathBuf,
        bundle_object: ObjectId,
        traces: [PathBuf; 3],
        system_contents: Vec<u8>,
        global_contents: Vec<u8>,
        bundle_contents: Vec<u8>,
    }

    fn write_hostile_config(fixture: &RealFixture) -> HostileConfig {
        let root = fixture.context.dir.path();
        let system = root.join("system.config");
        let global = root.join("global.config");
        let bundle = root.join("configured.bundle");
        let traces = [root.join("event.trace"), root.join("normal.trace"), root.join("perf.trace")];

        // This commit exists only in a valid configured bundle. If the exact
        // fetch accidentally honors fetch.bundleURI, Git imports the commit
        // even though no requested remote ref can reach it.
        let bundle_object =
            commit(&fixture.context, &fixture.seed, fixture.external, "configured bundle only\n");
        run_git(
            &fixture.context,
            [
                "-C".to_owned(),
                fixture.seed.to_string_lossy().into_owned(),
                "update-ref".to_owned(),
                "refs/heads/configured-bundle-only".to_owned(),
                bundle_object.to_string(),
            ],
        );
        run_git(
            &fixture.context,
            [
                "-C".to_owned(),
                fixture.seed.to_string_lossy().into_owned(),
                "bundle".to_owned(),
                "create".to_owned(),
                bundle.to_string_lossy().into_owned(),
                "refs/heads/configured-bundle-only".to_owned(),
            ],
        );
        let bundle_contents = fs::read(&bundle).unwrap();
        assert!(!bundle_contents.is_empty());
        assert!(!object_exists(&fixture.context, bundle_object));

        let system_contents = format!(
            "[fetch]\n\tbundleURI = {}\n[trace2]\n\teventTarget = {}\n",
            git_config_value(&bundle),
            git_config_value(&traces[0])
        )
        .into_bytes();
        let global_contents = format!(
            "[trace2]\n\tnormalTarget = {}\n\tperfTarget = {}\n",
            git_config_value(&traces[1]),
            git_config_value(&traces[2])
        )
        .into_bytes();
        fs::write(&system, &system_contents).unwrap();
        fs::write(&global, &global_contents).unwrap();
        assert!(traces.iter().all(|trace| !trace.exists()));
        HostileConfig {
            system,
            global,
            bundle,
            bundle_object,
            traces,
            system_contents,
            global_contents,
            bundle_contents,
        }
    }

    /// Every local Git surface which observation and acquisition promise not
    /// to change. The object database is deliberately absent: acquisition may
    /// add the requested history and transport-adjacent objects.
    #[derive(Debug, Eq, PartialEq)]
    struct LocalGitState {
        refs: Vec<u8>,
        head: Vec<u8>,
        config: Vec<u8>,
        worktree_config: Option<Vec<u8>>,
        fetch_head: Option<Vec<u8>>,
        shallow: Option<Vec<u8>>,
    }

    fn file_state(path: impl AsRef<Path>) -> Option<Vec<u8>> {
        let path = path.as_ref();
        match fs::read(path) {
            Ok(contents) => Some(contents),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
            Err(error) => panic!("failed to snapshot {path:?}: {error}"),
        }
    }

    fn git_dir(context: &testutil::TestContext) -> PathBuf {
        PathBuf::from(git_stdout(context, ["rev-parse", "--absolute-git-dir"]))
    }

    fn local_git_state(context: &testutil::TestContext) -> LocalGitState {
        let git_dir = git_dir(context);
        LocalGitState {
            refs: git_output(context, ["for-each-ref", "--format=%(refname)%00%(objectname)"])
                .stdout,
            head: git_output(context, ["symbolic-ref", "HEAD"]).stdout,
            config: fs::read(git_dir.join("config")).unwrap(),
            worktree_config: file_state(git_dir.join("config.worktree")),
            fetch_head: file_state(git_dir.join("FETCH_HEAD")),
            shallow: file_state(git_dir.join("shallow")),
        }
    }

    fn object_exists(context: &testutil::TestContext, oid: ObjectId) -> bool {
        // `git cat-file` may lazily contact a configured promisor remote for a
        // missing object. Inspect the local object database directly so this
        // assertion cannot repair the very absence it is meant to observe.
        util::Repo::open(context.repo_path.to_str().unwrap())
            .unwrap()
            .try_find_object(oid)
            .unwrap()
            .is_some()
    }

    fn prime_complete_graph(fixture: &RealFixture) {
        run_git(
            &fixture.context,
            [
                "fetch",
                "--quiet",
                "--no-progress",
                "--no-write-fetch-head",
                "--no-tags",
                "origin",
                "refs/tags/gherrit/Local/v1",
            ],
        );
        assert!(object_exists(&fixture.context, fixture.external));
        assert!(object_exists(&fixture.context, fixture.external_parent));
    }

    fn prime_missing_ancestor(fixture: &RealFixture) {
        run_git(
            &fixture.context,
            [
                "fetch",
                "--quiet",
                "--no-progress",
                "--no-write-fetch-head",
                "--no-tags",
                "--depth=1",
                "origin",
                "refs/tags/gherrit/Local/v1",
            ],
        );
        // Removing the boundary turns the intentionally shallow fixture into
        // an ordinary object database containing the root but not its parent.
        fs::remove_file(git_dir(&fixture.context).join("shallow")).unwrap();
        assert!(object_exists(&fixture.context, fixture.external));
        assert!(!object_exists(&fixture.context, fixture.external_parent));
    }

    fn write_fetch_head_sentinel(context: &testutil::TestContext) {
        fs::write(git_dir(context).join("FETCH_HEAD"), b"preexisting FETCH_HEAD bytes\n").unwrap();
    }

    fn invoke_real(fixture: &RealFixture, mode: RealMode, hostile: &HostileConfig) {
        let mut command = fixture.context.gherrit_cmd();
        command
            .args(["--exact", REAL_TEST, "--nocapture"])
            .env(REAL_MODE, mode.as_str())
            .env(REAL_ROOT, fixture.context.dir.path())
            .env(REAL_REMOTE, &fixture.remote)
            // TestContext normally suppresses the machine's system config.
            // This explicit false value makes only our hostile fixture visible.
            .env("GIT_CONFIG_NOSYSTEM", "0")
            .env("GIT_CONFIG_SYSTEM", &hostile.system)
            .env("GIT_CONFIG_GLOBAL", &hostile.global)
            .timeout(REAL_TEST_TIMEOUT);
        let output = command.output().unwrap();
        assert!(
            output.status.success(),
            "real boundary child failed:\nstdout={}\nstderr={}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }

    #[test]
    fn real_boundary_reexec() {
        let Ok(mode) = env::var(REAL_MODE) else { return };
        let mode = RealMode::parse(&mode);
        let root = PathBuf::from(env::var_os(REAL_ROOT).unwrap());
        let remote = PathBuf::from(env::var_os(REAL_REMOTE).unwrap());
        let local = env::current_dir().unwrap();
        let repository = crate::util::Repo::open(local.to_str().unwrap()).unwrap();
        let default = repository.rev_parse_single("refs/heads/main").unwrap().detach();
        let proposal = repository.rev_parse_single("refs/heads/proposal").unwrap().detach();
        let changes = [(id("Local"), proposal, default)];
        let stack = LocalStack::for_history_test(
            DefaultBranch::new("main".to_owned(), default).unwrap(),
            changes,
        );
        let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();

        // This bounded local command proves that the hostile Trace2 config is
        // active. Destination-bearing commands must suppress the same targets.
        let mut trace_probe = util::cmd("git", ["rev-parse", "--git-dir"]);
        trace_probe.current_dir(&local);
        let probe = runtime.block_on(subprocess::output(trace_probe, REAL_TEST_TIMEOUT)).unwrap();
        assert!(probe.status().success());

        let destination =
            PushDestination::resolve(util::RemoteName::from_config(b"origin").unwrap()).unwrap();
        let result = match mode {
            // Making the remote unavailable after observation proves these
            // paths fail or succeed without attempting acquisition.
            RealMode::Complete | RealMode::Ordinary => {
                let observed = runtime
                    .block_on(
                        ExactLocalQueryPlan::for_stack(&stack)
                            .unwrap()
                            .observe(&repository, &destination),
                    )
                    .unwrap();
                fs::rename(&remote, root.join("remote.unavailable")).unwrap();
                runtime.block_on(observed.validate_histories())
            }
            // Acquisition paths exercise the single production operation.
            RealMode::Negotiated | RealMode::Refetch => {
                runtime.block_on(observe_and_validate_histories(&stack, &repository, &destination))
            }
        };
        match mode {
            RealMode::Complete => {
                assert!(result.is_ok(), "complete graph unexpectedly acquired: {result:?}")
            }
            RealMode::Negotiated | RealMode::Refetch => {
                assert!(result.is_ok(), "acquisition failed: {result:?}")
            }
            RealMode::Ordinary => {
                let error = format!("{:?}", result.unwrap_err());
                assert!(error.contains("ordinary local Git object database is incomplete"));
                assert!(error.contains("ordinary fetch negotiation cannot repair"));
            }
        }
    }

    fn assert_path_absent(text: &str, path: &Path) {
        let native = path.to_string_lossy();
        for spelling in
            [native.to_string(), native.replace('\\', "/"), native.replace('\\', "\\\\")]
        {
            assert!(!text.contains(&spelling), "trace exposed private path {spelling:?}: {text}");
        }
    }

    fn assert_hostile_state(fixture: &RealFixture, hostile: &HostileConfig) {
        assert_eq!(fs::read(&hostile.system).unwrap(), hostile.system_contents);
        assert_eq!(fs::read(&hostile.global).unwrap(), hostile.global_contents);
        assert_eq!(
            fs::read(&hostile.bundle).unwrap(),
            hostile.bundle_contents,
            "configured bundle source was changed"
        );
        assert!(
            !object_exists(&fixture.context, hostile.bundle_object),
            "exact fetch consumed the configured secondary object source"
        );
        for trace in &hostile.traces {
            let bytes = fs::read(trace).expect("the local probe must activate every Trace2 target");
            let text = String::from_utf8_lossy(&bytes);
            assert!(text.contains("rev-parse"), "Trace2 fixture was not active: {text}");
            assert!(!text.contains("fetch"), "destination-bearing fetch reached Trace2: {text}");
            assert!(!text.contains("ls-remote"), "destination observation reached Trace2: {text}");
            assert!(!text.contains("GHERRIT_PRIVATE_PUSH_DESTINATION"), "{text}");
            assert_path_absent(&text, &fixture.remote);
            assert_path_absent(&text, &hostile.bundle);
        }
    }

    fn assert_local_state_unchanged(
        fixture: &RealFixture,
        before: &LocalGitState,
        hostile: &HostileConfig,
    ) {
        assert_hostile_state(fixture, hostile);
        assert_eq!(
            &local_git_state(&fixture.context),
            before,
            "observation or acquisition changed refs, HEAD, config, FETCH_HEAD, or shallow state"
        );
    }

    #[test]
    fn real_complete_graph_skips_acquisition() {
        let fixture = real_fixture();
        prime_complete_graph(&fixture);
        write_fetch_head_sentinel(&fixture.context);
        let before = local_git_state(&fixture.context);
        let hostile = write_hostile_config(&fixture);

        invoke_real(&fixture, RealMode::Complete, &hostile);

        assert_local_state_unchanged(&fixture, &before, &hostile);
        assert!(object_exists(&fixture.context, fixture.external));
        assert!(object_exists(&fixture.context, fixture.external_parent));
    }

    #[test]
    fn real_missing_root_negotiates_without_ref_side_effects() {
        let fixture = real_fixture();
        assert!(!object_exists(&fixture.context, fixture.external));
        assert!(!object_exists(&fixture.context, fixture.external_parent));
        write_fetch_head_sentinel(&fixture.context);
        let before = local_git_state(&fixture.context);
        let hostile = write_hostile_config(&fixture);

        invoke_real(&fixture, RealMode::Negotiated, &hostile);

        assert_local_state_unchanged(&fixture, &before, &hostile);
        assert!(object_exists(&fixture.context, fixture.external));
        assert!(object_exists(&fixture.context, fixture.external_parent));
    }

    #[test]
    fn real_promisor_missing_ancestor_refetches_without_ref_side_effects() {
        let fixture = real_fixture();
        let version = git_output(&fixture.context, ["--version"]);
        if util::parse_git_version(&version.stdout).unwrap() < (2, 45) {
            return;
        }
        prime_missing_ancestor(&fixture);
        run_git(&fixture.context, ["config", "remote.origin.promisor", "true"]);
        write_fetch_head_sentinel(&fixture.context);
        let before = local_git_state(&fixture.context);
        let hostile = write_hostile_config(&fixture);

        invoke_real(&fixture, RealMode::Refetch, &hostile);

        assert_local_state_unchanged(&fixture, &before, &hostile);
        assert!(object_exists(&fixture.context, fixture.external));
        assert!(object_exists(&fixture.context, fixture.external_parent));
    }

    #[test]
    fn real_ordinary_missing_ancestor_fails_before_acquisition() {
        let fixture = real_fixture();
        prime_missing_ancestor(&fixture);
        write_fetch_head_sentinel(&fixture.context);
        let before = local_git_state(&fixture.context);
        let hostile = write_hostile_config(&fixture);

        invoke_real(&fixture, RealMode::Ordinary, &hostile);

        assert_local_state_unchanged(&fixture, &before, &hostile);
        assert!(object_exists(&fixture.context, fixture.external));
        assert!(!object_exists(&fixture.context, fixture.external_parent));
    }

    #[test]
    fn exact_acquisition_payload_contains_only_the_causal_roots_aliases() {
        let root = ExternalGraphRoot::for_test(
            ObjectId::from_hex(HEAD.as_bytes()).unwrap(),
            "refs/tags/gherrit/A/v1",
            &["refs/tags/gherrit/A/v3"],
        );
        let mut payload = Vec::new();

        write_fetch_payload(&root, &mut payload).unwrap();

        assert_eq!(payload, b"refs/tags/gherrit/A/v1\nrefs/tags/gherrit/A/v3\n");
        assert!(!payload.windows(HEAD.len()).any(|bytes| bytes == HEAD.as_bytes()));
        assert!(
            !payload
                .windows(b"refs/tags/gherrit/B/v1".len())
                .any(|bytes| { bytes == b"refs/tags/gherrit/B/v1" })
        );
        let _sealed = prepare_fetch_input(&root).unwrap();
    }

    #[test]
    fn exact_acquisition_payload_length_is_checked_before_file_creation() {
        let ok = |length| Ok::<u64, color_eyre::Report>(length);

        assert_eq!(checked_payload_length([ok(3), ok(4)], 9).unwrap(), 9);
        assert!(checked_payload_length([ok(3), ok(4)], 8).is_err());
        assert!(checked_payload_length([ok(u64::MAX)], u64::MAX).is_err());
    }

    #[tokio::test(flavor = "current_thread")]
    async fn consuming_complete_observation_uses_its_sealed_stack_without_acquisition() {
        let directory = tempfile::tempdir().unwrap();
        gix::init_bare(directory.path()).unwrap();
        let repository = util::Repo::open(directory.path().to_str().unwrap()).unwrap();
        let destination = PushDestination::for_test();
        let change_id = id("Absent");
        let default = default_branch();
        let proposal = ObjectId::from_hex(HEAD.as_bytes()).unwrap();
        let local = LocalStack::for_history_test(
            default.clone(),
            [(change_id.clone(), proposal, default.tip())],
        );
        let output = format!("{DEFAULT}\trefs/heads/main\n");
        let raw = QueryPlan::new(default, std::slice::from_ref(&change_id))
            .unwrap()
            .decode([output.as_bytes()])
            .unwrap();
        let observation = ExactLocalObservation {
            local: &local,
            repository: &repository,
            destination: &destination,
            raw,
        };

        let histories = observation.validate_histories().await.unwrap();

        assert_eq!(histories.len(), 1);
        assert_eq!(histories[0].id(), &change_id);
        assert!(histories[0].needs_publication());
    }

    fn decode<'output>(
        ids: &[GherritPrId],
        outputs: impl IntoIterator<Item = &'output [u8]>,
    ) -> Result<RawExactLocalObservation> {
        let default = default_branch();
        QueryPlan::new(default, ids)?.decode(outputs)
    }

    fn full_output() -> String {
        format!(
            "{DEFAULT}\trefs/heads/main\n\
             {HEAD}\trefs/heads/Gone\n\
             {BASE}\trefs/heads/gherrit-bases/Gone\n\
             {HEAD}\trefs/tags/gherrit/Gone/v1\n\
             {MARKER}\trefs/tags/gherrit/Gone/pr\n"
        )
    }

    #[test]
    fn plans_only_exact_local_namespaces_and_rechecks_default_once() {
        let default = default_branch();
        let ids = [id("A"), id("B")];
        let plan = QueryPlan::new(default, &ids).unwrap();
        let patterns = plan.patterns().collect::<Vec<_>>();

        assert_eq!(patterns.len(), 1);
        assert_eq!(
            patterns[0],
            [
                "refs/heads/main",
                "refs/heads/A",
                "refs/heads/gherrit-bases/A",
                "refs/tags/gherrit/A",
                "refs/tags/gherrit/A/*",
                "refs/heads/B",
                "refs/heads/gherrit-bases/B",
                "refs/tags/gherrit/B",
                "refs/tags/gherrit/B/*",
            ]
        );
    }

    #[test]
    fn stack_bound_plan_reuses_the_exact_path_origin_and_ordered_ids() {
        let default = default_branch();
        let a = id("A");
        let b = id("B");
        let a_head = ObjectId::from_hex(HEAD.as_bytes()).unwrap();
        let b_head = ObjectId::from_hex(BASE.as_bytes()).unwrap();
        let stack = LocalStack::for_history_test(
            default.clone(),
            [(a, a_head, default.tip()), (b, b_head, a_head)],
        );

        let plan = ExactLocalQueryPlan::for_stack(&stack).unwrap();
        assert!(std::ptr::eq(plan.local, &stack));
        let patterns = plan.patterns().collect::<Vec<_>>();

        assert_eq!(stack.default_branch(), &default);
        assert_eq!(patterns.len(), 1);
        assert_eq!(patterns[0][0], default.full_ref_name());
        assert_eq!(patterns[0][1], "refs/heads/A");
        assert_eq!(patterns[0][5], "refs/heads/B");
    }

    #[test]
    fn plans_every_batch_before_observation_and_rejects_invalid_sets() {
        let default = default_branch();
        let a = id("A");
        let b = id("B");
        let per_query_budget = local_pattern_bytes(&a) + local_pattern_bytes(&b) - 1;
        let total_budget = default.full_ref_name().len() + 1 + per_query_budget;
        let plan = QueryPlan::with_budget(default, &[a.clone(), b], total_budget).unwrap();
        let patterns = plan.patterns().collect::<Vec<_>>();
        assert_eq!(patterns.len(), 2);
        assert_eq!(patterns[0][0], "refs/heads/main");
        assert!(!patterns[1].iter().any(|pattern| pattern == "refs/heads/main"));

        assert!(QueryPlan::new(default_branch(), &[]).is_err());
        assert!(QueryPlan::new(default_branch(), &[a.clone(), a]).is_err());
        assert!(QueryPlan::new(default_branch(), &[id("main")]).is_err());
        assert!(
            QueryPlan::with_budget(default_branch(), &[id("A")], total_budget - per_query_budget)
                .is_err()
        );
    }

    #[test]
    fn decodes_unordered_records_but_preserves_requested_change_order() {
        let ids = [id("First"), id("Second")];
        let output = format!(
            "{HEAD}\trefs/tags/gherrit/Second/v2\n\
             {BASE}\trefs/heads/gherrit-bases/First\n\
             {DEFAULT}\trefs/heads/main\n\
             {MARKER}\trefs/tags/gherrit/Second/pr\n\
             {HEAD}\trefs/heads/Second\n\
             {BASE}\trefs/tags/gherrit/Second/v1\n"
        );
        let observed = decode(&ids, [output.as_bytes()]).unwrap();
        assert_eq!(observed.default_branch().name(), "main");
        assert_eq!(observed.default_branch().tip().to_string(), DEFAULT);
        let changes = observed.iter().collect::<Vec<_>>();

        assert_eq!(
            changes.iter().map(|change| change.id().as_str()).collect::<Vec<_>>(),
            ["First", "Second"]
        );
        assert_eq!(changes[0].owned_base().unwrap().to_string(), BASE);
        assert_eq!(changes[1].candidate_head().unwrap().to_string(), HEAD);
        assert_eq!(changes[1].pull_request_marker().unwrap().to_string(), MARKER);
        assert_eq!(
            changes[1]
                .versions()
                .map(|version| (
                    version.version().get(),
                    version.object_id().to_string(),
                    version.source_ref()
                ))
                .collect::<Vec<_>>(),
            [
                (1, BASE.to_owned(), "refs/tags/gherrit/Second/v1"),
                (2, HEAD.to_owned(), "refs/tags/gherrit/Second/v2"),
            ]
        );
    }

    #[test]
    fn missing_refs_mean_absence_only_after_the_default_tip_is_rechecked() {
        let ids = [id("Gone")];
        let present_default = format!("{DEFAULT}\trefs/heads/main\n");
        let observed = decode(&ids, [present_default.as_bytes()]).unwrap();
        let change = observed.iter().next().unwrap();
        assert_eq!(change.candidate_head(), None);
        assert_eq!(change.owned_base(), None);
        assert_eq!(change.versions().len(), 0);
        assert_eq!(change.pull_request_marker(), None);

        assert!(decode(&ids, [b"".as_slice()]).is_err());
        let moved = format!("{HEAD}\trefs/heads/main\n");
        assert!(decode(&ids, [moved.as_bytes()]).unwrap_err().to_string().contains("moved"));
        let duplicate = format!("{DEFAULT}\trefs/heads/main\n{DEFAULT}\trefs/heads/main\n");
        assert!(decode(&ids, [duplicate.as_bytes()]).is_err());
    }

    #[test]
    fn each_planned_batch_has_exact_coverage_and_response_cardinality() {
        let default = default_branch();
        let a = id("A");
        let b = id("B");
        let local_budget = local_pattern_bytes(&a) + local_pattern_bytes(&b) - 1;
        let budget = default.full_ref_name().len() + 1 + local_budget;
        let first = format!("{DEFAULT}\trefs/heads/main\n{HEAD}\trefs/heads/A\n");
        let second = format!("{BASE}\trefs/heads/B\n");

        let observed = QueryPlan::with_budget(default, &[a.clone(), b.clone()], budget)
            .unwrap()
            .decode([first.as_bytes(), second.as_bytes()])
            .unwrap();
        assert_eq!(observed.iter().map(|change| change.id()).collect::<Vec<_>>(), [&a, &b]);

        assert!(
            QueryPlan::with_budget(default_branch(), &[a.clone(), b.clone()], budget)
                .unwrap()
                .decode([first.as_bytes()])
                .is_err()
        );
        assert!(
            QueryPlan::with_budget(default_branch(), &[a, b], budget)
                .unwrap()
                .decode([first.as_bytes(), second.as_bytes(), b"".as_slice()])
                .is_err()
        );
    }

    #[test]
    fn later_batches_reject_a_default_record_instead_of_reinterpreting_it() {
        let default = default_branch();
        let a = id("A");
        let b = id("B");
        let local_budget = local_pattern_bytes(&a) + local_pattern_bytes(&b) - 1;
        let budget = default.full_ref_name().len() + 1 + local_budget;
        let first = format!("{DEFAULT}\trefs/heads/main\n");
        let second = format!("{DEFAULT}\trefs/heads/main\n");
        assert!(
            QueryPlan::with_budget(default, &[a, b], budget)
                .unwrap()
                .decode([first.as_bytes(), second.as_bytes()])
                .is_err()
        );
    }

    #[test]
    fn rejects_unrequested_records_in_every_owned_namespace() {
        let ids = [id("Gone")];
        for record in [
            format!("{HEAD}\trefs/heads/Other\n"),
            format!("{HEAD}\trefs/heads/gherrit-bases/Other\n"),
            format!("{HEAD}\trefs/tags/gherrit/Other/v1\n"),
            format!("{HEAD}\trefs/tags/gherrit/Other/pr\n"),
            format!("{HEAD}\trefs/tags/unrelated\n"),
        ] {
            let output = format!("{DEFAULT}\trefs/heads/main\n{record}");
            assert!(decode(&ids, [output.as_bytes()]).is_err(), "accepted {output:?}");
        }
    }

    #[test]
    fn validated_tail_matches_are_ignored_without_weakening_absence() {
        let ids = [id("Gone")];
        let output = format!(
            "{DEFAULT}\trefs/heads/main\n\
             {HEAD}\trefs/heads/archive/refs/heads/main\n\
             {HEAD}\trefs/heads/archive/refs/heads/Gone\n\
             {BASE}\trefs/heads/archive/refs/heads/gherrit-bases/Gone\n\
             {HEAD}\trefs/tags/archive/refs/tags/gherrit/Gone\n\
             {BASE}\trefs/tags/archive/refs/tags/gherrit/Gone/v9\n\
             {MARKER}\trefs/tags/archive/refs/tags/gherrit/Gone/pr\n"
        );
        let observed = decode(&ids, [output.as_bytes()]).unwrap();
        let change = observed.iter().next().unwrap();
        assert_eq!(change.candidate_head(), None);
        assert_eq!(change.owned_base(), None);
        assert_eq!(change.versions().len(), 0);
        assert_eq!(change.pull_request_marker(), None);

        let duplicate_noise = format!(
            "{DEFAULT}\trefs/heads/main\n\
             {HEAD}\trefs/heads/archive/refs/heads/Gone\n\
             {HEAD}\trefs/heads/archive/refs/heads/Gone\n"
        );
        assert!(decode(&ids, [duplicate_noise.as_bytes()]).is_err());

        let peeled_noise = format!(
            "{DEFAULT}\trefs/heads/main\n\
             {HEAD}\trefs/tags/archive/refs/tags/gherrit/Gone/v1\n\
             {BASE}\trefs/tags/archive/refs/tags/gherrit/Gone/v1^{{}}\n"
        );
        let observed = decode(&ids, [peeled_noise.as_bytes()]).unwrap();
        let change = observed.iter().next().unwrap();
        assert_eq!(change.candidate_head(), None);
        assert_eq!(change.owned_base(), None);
        assert_eq!(change.versions().len(), 0);
        assert_eq!(change.pull_request_marker(), None);
    }

    #[test]
    fn rejects_duplicate_refs_and_all_peeled_owned_refs() {
        let ids = [id("Gone")];
        for records in [
            format!("{HEAD}\trefs/heads/Gone\n{BASE}\trefs/heads/Gone\n"),
            format!(
                "{HEAD}\trefs/heads/gherrit-bases/Gone\n{BASE}\trefs/heads/gherrit-bases/Gone\n"
            ),
            format!("{HEAD}\trefs/tags/gherrit/Gone/v1\n{BASE}\trefs/tags/gherrit/Gone/v1\n"),
            format!("{HEAD}\trefs/tags/gherrit/Gone/pr\n{BASE}\trefs/tags/gherrit/Gone/pr\n"),
            format!("{HEAD}\trefs/heads/Gone^{{}}\n"),
            format!("{HEAD}\trefs/heads/gherrit-bases/Gone^{{}}\n"),
            format!("{HEAD}\trefs/tags/gherrit/Gone/v1\n{BASE}\trefs/tags/gherrit/Gone/v1^{{}}\n"),
            format!("{HEAD}\trefs/tags/gherrit/Gone/pr\n{BASE}\trefs/tags/gherrit/Gone/pr^{{}}\n"),
        ] {
            let output = format!("{DEFAULT}\trefs/heads/main\n{records}");
            assert!(decode(&ids, [output.as_bytes()]).is_err(), "accepted {output:?}");
        }
    }

    #[test]
    fn rejects_malformed_symbolic_null_non_sha1_and_noncanonical_object_ids() {
        let ids = [id("Gone")];
        for record in [
            b"not-a-record\n".to_vec(),
            b"ref: refs/heads/other\trefs/heads/Gone\n".to_vec(),
            format!("{}\trefs/heads/Gone\n", "0".repeat(40)).into_bytes(),
            format!("{SHA256}\trefs/heads/Gone\n").into_bytes(),
            format!("{}\trefs/heads/Gone\n", "ABCDEF0123456789ABCDEF0123456789ABCDEF01")
                .into_bytes(),
            format!("{HEAD}\trefs/heads/Gone\textra\n").into_bytes(),
            format!("{HEAD}\trefs/heads/bad..name\n").into_bytes(),
        ] {
            let mut output = format!("{DEFAULT}\trefs/heads/main\n").into_bytes();
            output.extend(record);
            assert!(decode(&ids, [&output[..]]).is_err(), "accepted {output:?}");
        }
    }

    #[test]
    fn managed_tag_names_are_canonical() {
        let ids = [id("Gone")];
        for name in [
            "refs/tags/gherrit",
            "refs/tags/gherrit/Gone",
            "refs/tags/gherrit/Gone/v0",
            "refs/tags/gherrit/Gone/v01",
            "refs/tags/gherrit/Gone/v",
            "refs/tags/gherrit/Gone/vx",
            "refs/tags/gherrit/Gone/other",
            "refs/tags/gherrit/Gone/v18446744073709551616",
            "refs/tags/gherrit/Gone/v1/extra",
            "refs/tags/gherrit/Gone/pr/extra",
        ] {
            let output = format!("{DEFAULT}\trefs/heads/main\n{HEAD}\t{name}\n");
            assert!(decode(&ids, [output.as_bytes()]).is_err(), "accepted {name}");
        }
    }

    #[test]
    fn raw_observation_does_not_claim_history_or_relationship_validity() {
        let ids = [id("Gone")];
        let output = format!(
            "{DEFAULT}\trefs/heads/main\n\
             {HEAD}\trefs/heads/Gone\n\
             {BASE}\trefs/tags/gherrit/Gone/v3\n\
             {MARKER}\trefs/tags/gherrit/Gone/pr\n"
        );
        let observed = decode(&ids, [output.as_bytes()]).unwrap();
        let change = observed.iter().next().unwrap();
        assert_eq!(change.candidate_head().unwrap().to_string(), HEAD);
        assert_eq!(change.owned_base(), None);
        assert_eq!(change.versions().next().unwrap().version().get(), 3);
        assert_eq!(change.pull_request_marker().unwrap().to_string(), MARKER);
    }

    #[test]
    fn native_line_framing_allows_only_the_hosts_terminator() {
        let ids = [id("Gone")];
        let native = full_output();
        let no_final_lf = native.strip_suffix('\n').unwrap();
        let crlf = native.replace('\n', "\r\n");
        for output in [native.as_bytes(), no_final_lf.as_bytes()] {
            assert!(decode(&ids, [output]).is_ok(), "rejected {output:?}");
        }
        #[cfg(windows)]
        assert!(decode(&ids, [crlf.as_bytes()]).is_ok());
        #[cfg(not(windows))]
        assert!(decode(&ids, [crlf.as_bytes()]).is_err());
    }
}

//! Byte-oriented observations of the push repository's advertised refs.
//!
//! Repository-wide heads and exact active immutable histories have different
//! completeness domains. A missing `RemoteHeads` entry proves absence because
//! every head was advertised. Active histories are accumulated only by
//! consuming the destination-bound head observation, so independently observed
//! heads and tags cannot be joined for normalization.

use std::{
    collections::{BTreeMap, HashMap, HashSet, hash_map::Entry},
    fmt,
    process::Command,
    str,
    time::{Duration, Instant},
};

use color_eyre::eyre::{Context as _, Result, bail, eyre};
use gix::{ObjectId, bstr::ByteSlice as _};

use super::{
    destination::{DefaultBranch, PushDestination, git_output_records},
    history::{CommitGraphEvidence, GraphLoadError},
    local::{GherritPrId, LocalChange, LocalStack},
    subprocess,
    version::Version,
};
use crate::util;

// Variable arguments are kept well below Windows' roughly 32-KiB command-line
// limit. This also gives POSIX implementations a conservative bound.
const QUERY_ARGV_BUDGET_BYTES: usize = 16 * 1024;
const HEAD_ADVERTISEMENT_PATTERNS: [&str; 3] = ["HEAD", "refs/heads/*", "refs/tags/gherrit"];
const HEAD_PREFIX: &[u8] = b"refs/heads/";
const OWNED_BASE_ROOT: &[u8] = b"refs/heads/gherrit-bases";
const OWNED_BASE_PREFIX: &[u8] = b"refs/heads/gherrit-bases/";
const MANAGED_TAG_ROOT: &[u8] = b"refs/tags/gherrit";
const MANAGED_TAG_PREFIX: &[u8] = b"refs/tags/gherrit/";

/// Complete, syntactically valid state from one repository-wide head query.
pub(super) struct RemoteHeads<'destination> {
    destination: &'destination PushDestination,
    parsed: ParsedRemoteHeads,
}

#[derive(Debug)]
struct ParsedRemoteHeads {
    default_branch: DefaultBranch,
    candidate_heads: HashMap<GherritPrId, ObjectId>,
    owned_bases: HashMap<GherritPrId, ObjectId>,
}

impl ParsedRemoteHeads {
    fn default_branch(&self) -> &DefaultBranch {
        &self.default_branch
    }

    fn candidate_head(&self, id: &GherritPrId) -> Option<ObjectId> {
        self.candidate_heads.get(id).copied()
    }

    fn owned_base(&self, id: &GherritPrId) -> Option<ObjectId> {
        self.owned_bases.get(id).copied()
    }
}

impl fmt::Debug for RemoteHeads<'_> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("RemoteHeads")
            .field("default_branch", &self.parsed.default_branch)
            .field("candidate_head_count", &self.parsed.candidate_heads.len())
            .field("owned_base_count", &self.parsed.owned_bases.len())
            .finish()
    }
}

impl RemoteHeads<'_> {
    pub(super) fn default_branch(&self) -> &DefaultBranch {
        self.parsed.default_branch()
    }

    /// Returns a syntactically eligible top-level head.
    ///
    /// A matching name is only candidate evidence. Pull-request metadata or
    /// the corresponding owned-base ref must establish managed identity.
    pub(super) fn candidate_head(&self, id: &GherritPrId) -> Option<ObjectId> {
        self.parsed.candidate_head(id)
    }

    pub(super) fn owned_base(&self, id: &GherritPrId) -> Option<ObjectId> {
        self.parsed.owned_base(id)
    }
}

impl<'destination> RemoteHeads<'destination> {
    /// Returns the exact opaque destination capability which produced this
    /// complete head observation.
    pub(super) fn destination(&self) -> &'destination PushDestination {
        self.destination
    }

    #[cfg(test)]
    pub(super) fn into_active_for_test(
        self,
        local_ids: &[GherritPrId],
        nonlocal_ids: &[GherritPrId],
        managed_tag_output: &[u8],
    ) -> Result<ActiveRemoteChanges<'destination>> {
        let expected = local_ids.iter().chain(nonlocal_ids);
        let ParsedManagedTags { histories } = parse_managed_tags(managed_tag_output, expected)?;
        DestinationObservation { heads: self, histories }.into_active(local_ids, nonlocal_ids)
    }
}

impl<'destination> RemoteHeads<'destination> {
    /// Consumes the complete head observation to begin cumulative history
    /// observation at the same exact destination.
    #[allow(dead_code)]
    pub(super) async fn observe_managed_tags(
        self,
        local_ids: &[GherritPrId],
    ) -> Result<DestinationObservation<'destination>> {
        let histories = observe_managed_tag_namespaces(self.destination, local_ids.iter()).await?;
        Ok(DestinationObservation { heads: self, histories })
    }
}

/// Cumulative complete history coverage from the destination which produced
/// the retained repository-wide head observation.
pub(super) struct DestinationObservation<'destination> {
    heads: RemoteHeads<'destination>,
    histories: HashMap<GherritPrId, AdvertisedChangeNamespace>,
}

impl fmt::Debug for DestinationObservation<'_> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("DestinationObservation")
            .field("heads", &self.heads)
            .field("covered_change_count", &self.histories.len())
            .field(
                "advertised_ref_count",
                &self.histories.values().map(AdvertisedChangeNamespace::ref_count).sum::<usize>(),
            )
            .finish()
    }
}

impl<'destination> DestinationObservation<'destination> {
    #[allow(dead_code)]
    pub(super) fn remote_heads(&self) -> &RemoteHeads<'destination> {
        &self.heads
    }

    /// Consumes this capability and adds one disjoint logical wave.
    ///
    /// There is deliberately no destination argument. Even empty requested
    /// namespaces are retained as complete coverage evidence.
    #[allow(dead_code)]
    pub(super) async fn observe_additional(mut self, nonlocal_ids: &[GherritPrId]) -> Result<Self> {
        unique_id_names(nonlocal_ids, "additional remote observation")?;
        if nonlocal_ids.iter().any(|id| self.histories.contains_key(id)) {
            bail!("additional remote observation overlaps previously covered changes");
        }

        let additional =
            observe_managed_tag_namespaces(self.heads.destination, nonlocal_ids.iter()).await?;
        for (id, history) in additional {
            if self.histories.insert(id, history).is_some() {
                bail!("additional remote observation overlaps previously covered changes");
            }
        }
        Ok(self)
    }

    /// Consumes cumulative coverage and proves the exact ordered active set.
    #[allow(dead_code)]
    pub(super) fn into_active(
        mut self,
        local_ids: &[GherritPrId],
        nonlocal_ids: &[GherritPrId],
    ) -> Result<ActiveRemoteChanges<'destination>> {
        let local_names = unique_id_names(local_ids, "local active changes")?;
        let nonlocal_names = unique_id_names(nonlocal_ids, "nonlocal active changes")?;
        if !local_names.is_disjoint(&nonlocal_names) {
            bail!("local and nonlocal active changes overlap");
        }

        let expected = local_names.union(&nonlocal_names).copied().collect::<HashSet<_>>();
        if let Some(missing) =
            local_ids.iter().chain(nonlocal_ids).find(|id| !self.histories.contains_key(*id))
        {
            bail!("active remote observation is missing GHerrit change '{}'", missing.as_str());
        }
        if self.histories.keys().any(|id| !expected.contains(id.as_str())) {
            bail!("active remote observation contains a change outside the exact active set");
        }

        let heads = &self.heads;
        let observe =
            |id: &GherritPrId, namespace: AdvertisedChangeNamespace| ObservedChangeHistory {
                id: id.clone(),
                candidate_head: heads.candidate_head(id),
                owned_base: heads.owned_base(id),
                versions: namespace
                    .versions
                    .into_iter()
                    .map(|(version, advertised)| (version, advertised.object_id))
                    .collect(),
                pull_request_marker: namespace.pull_request_marker,
            };
        let local = local_ids
            .iter()
            .map(|id| {
                let namespace =
                    self.histories.remove(id).expect("exact local coverage was proved above");
                observe(id, namespace)
            })
            .collect();
        let nonlocal = nonlocal_ids
            .iter()
            .map(|id| {
                let namespace =
                    self.histories.remove(id).expect("exact nonlocal coverage was proved above");
                observe(id, namespace)
            })
            .collect();
        debug_assert!(self.histories.is_empty());

        Ok(ActiveRemoteChanges {
            destination: self.heads.destination,
            default_branch: self.heads.parsed.default_branch,
            local,
            nonlocal,
        })
    }

    /// Selects every exact advertised version ref for one covered wave.
    fn acquisition_for_changes(
        &self,
        wave_ids: &[GherritPrId],
    ) -> Result<Option<ObjectAcquisition<'destination>>> {
        unique_id_names(wave_ids, "object-acquisition wave")?;
        let mut object_ids = HashSet::new();
        let mut source_refs = Vec::new();
        for id in wave_ids {
            let history = self.histories.get(id).ok_or_else(|| {
                eyre!("object acquisition requested unobserved GHerrit change '{}'", id.as_str())
            })?;
            for advertised in history.versions.values() {
                object_ids.insert(advertised.object_id);
                source_refs.push(advertised.source_ref.clone());
            }
        }
        if source_refs.is_empty() {
            return Ok(None);
        }
        object_acquisition(self.heads.destination, object_ids.len(), source_refs).map(Some)
    }
}

/// Exact active remote evidence, split in caller-supplied logical order.
#[allow(dead_code)]
pub(super) struct ActiveRemoteChanges<'destination> {
    destination: &'destination PushDestination,
    default_branch: DefaultBranch,
    local: Box<[ObservedChangeHistory]>,
    nonlocal: Box<[ObservedChangeHistory]>,
}

impl fmt::Debug for ActiveRemoteChanges<'_> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ActiveRemoteChanges")
            .field("default_branch", &self.default_branch)
            .field("local", &self.local)
            .field("nonlocal", &self.nonlocal)
            .finish()
    }
}

impl<'destination> ActiveRemoteChanges<'destination> {
    pub(super) fn into_parts(
        self,
    ) -> (
        &'destination PushDestination,
        DefaultBranch,
        Box<[ObservedChangeHistory]>,
        Box<[ObservedChangeHistory]>,
    ) {
        (self.destination, self.default_branch, self.local, self.nonlocal)
    }

    #[cfg(test)]
    pub(super) fn destination(&self) -> &'destination PushDestination {
        self.destination
    }

    #[cfg(test)]
    pub(super) fn local(&self) -> &[ObservedChangeHistory] {
        &self.local
    }

    #[cfg(test)]
    pub(super) fn nonlocal(&self) -> &[ObservedChangeHistory] {
        &self.nonlocal
    }
}

/// Complete remote tuple evidence for exactly one covered change.
///
/// All fields are private and there is no constructor. Only exact cumulative
/// active-set consumption can produce this value, and history normalization
/// consumes it whole.
#[derive(Debug)]
pub(super) struct ObservedChangeHistory {
    id: GherritPrId,
    candidate_head: Option<ObjectId>,
    owned_base: Option<ObjectId>,
    versions: Box<[(Version, ObjectId)]>,
    pull_request_marker: Option<ObjectId>,
}

impl ObservedChangeHistory {
    pub(super) fn id(&self) -> &GherritPrId {
        &self.id
    }

    pub(super) fn candidate_head(&self) -> Option<ObjectId> {
        self.candidate_head
    }

    pub(super) fn owned_base(&self) -> Option<ObjectId> {
        self.owned_base
    }

    pub(super) fn versions(&self) -> impl ExactSizeIterator<Item = (Version, ObjectId)> + '_ {
        self.versions.iter().copied()
    }

    pub(super) fn pull_request_marker_target(&self) -> Option<ObjectId> {
        self.pull_request_marker
    }
}

fn unique_id_names<'id>(ids: &'id [GherritPrId], context: &str) -> Result<HashSet<&'id str>> {
    let mut unique = HashSet::with_capacity(ids.len());
    for id in ids {
        if !unique.insert(id.as_str()) {
            bail!("{context} contains duplicate GHerrit change IDs");
        }
    }
    Ok(unique)
}

/// Legacy publisher view of separately observed immutable histories.
///
/// FIXME(#264): Delete this compatibility value with [`ObservedStack`] when
/// activation switches orchestration to [`DestinationObservation`]. It cannot
/// enter complete-history normalization.
pub(super) struct ActiveManagedTags<'destination> {
    destination: &'destination PushDestination,
    histories: HashMap<GherritPrId, AdvertisedChangeNamespace>,
}

impl fmt::Debug for ActiveManagedTags<'_> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ActiveManagedTags")
            .field("covered_change_count", &self.histories.len())
            .field(
                "advertised_ref_count",
                &self.histories.values().map(AdvertisedChangeNamespace::ref_count).sum::<usize>(),
            )
            .finish()
    }
}

impl ActiveManagedTags<'_> {
    fn take_history(&mut self, id: &GherritPrId) -> Result<Option<BTreeMap<Version, ObjectId>>> {
        self.histories
            .remove(id)
            .map(|history| {
                if history.pull_request_marker.is_some() {
                    bail!(
                        "legacy publication cannot safely consume the pull-request marker for '{}'",
                        id.as_str()
                    );
                }
                Ok(history
                    .versions
                    .into_iter()
                    .map(|(version, advertised)| (version, advertised.object_id))
                    .collect())
            })
            .transpose()
    }
}

/// One exact lightweight version tag accepted from an advertisement.
///
/// `source_ref` remains private so acquisition cannot be redirected to a raw
/// object ID, a derived tag spelling, or a ref absent from the observation.
struct AdvertisedVersionRef {
    object_id: ObjectId,
    source_ref: String,
}

#[derive(Default)]
struct AdvertisedChangeNamespace {
    versions: BTreeMap<Version, AdvertisedVersionRef>,
    pull_request_marker: Option<ObjectId>,
}

impl AdvertisedChangeNamespace {
    fn ref_count(&self) -> usize {
        self.versions.len() + usize::from(self.pull_request_marker.is_some())
    }
}

/// Legacy remote state for each local change, in stack order.
///
/// FIXME(#264): Delete this compatibility seam when the active planner consumes
/// opaque [`ObservedChangeHistory`] values.
pub(super) struct ObservedStack<'stack, 'destination> {
    destination: &'destination PushDestination,
    changes: Vec<ObservedChange<'stack>>,
}

impl fmt::Debug for ObservedStack<'_, '_> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.debug_struct("ObservedStack").field("changes", &self.changes).finish()
    }
}

impl<'stack, 'destination> ObservedStack<'stack, 'destination> {
    pub(super) fn couple(
        stack: &'stack LocalStack,
        heads: &RemoteHeads<'destination>,
        mut managed_tags: ActiveManagedTags<'destination>,
    ) -> Result<Self> {
        if !std::ptr::eq(heads.destination, managed_tags.destination) {
            bail!("legacy head and managed-tag observations came from different destinations");
        }
        let changes = stack
            .iter()
            .map(|change| {
                let history = managed_tags.take_history(change.id())?.ok_or_else(|| {
                    eyre!(
                        "managed tag namespace for GHerrit change '{}' was not observed",
                        change.id().as_str()
                    )
                })?;
                Ok(ObservedChange {
                    change,
                    head: heads.candidate_head(change.id()),
                    owned_base: heads.owned_base(change.id()),
                    versions: history,
                })
            })
            .collect::<Result<Vec<_>>>()?;
        if !managed_tags.histories.is_empty() {
            bail!("remote managed-tag observation contains a change outside the local stack");
        }
        Ok(Self { destination: heads.destination, changes })
    }

    pub(super) fn destination(&self) -> &'destination PushDestination {
        self.destination
    }

    pub(super) fn iter(&self) -> impl ExactSizeIterator<Item = &ObservedChange<'stack>> {
        self.changes.iter()
    }

    #[cfg(test)]
    pub(super) fn for_test_at(
        destination: &'destination PushDestination,
        stack: &'stack LocalStack,
        states: impl IntoIterator<
            Item = (Option<ObjectId>, Option<ObjectId>, BTreeMap<Version, ObjectId>),
        >,
    ) -> Self {
        let states = states.into_iter().collect::<Vec<_>>();
        assert_eq!(stack.iter().len(), states.len());
        let changes = stack
            .iter()
            .zip(states)
            .map(|(change, (head, owned_base, versions))| ObservedChange {
                change,
                head,
                owned_base,
                versions,
            })
            .collect();
        Self { destination, changes }
    }
}

impl<'stack> ObservedStack<'stack, 'static> {
    #[cfg(test)]
    pub(super) fn for_test(
        stack: &'stack LocalStack,
        states: impl IntoIterator<
            Item = (Option<ObjectId>, Option<ObjectId>, BTreeMap<Version, ObjectId>),
        >,
    ) -> Self {
        let destination = test_push_destination();
        ObservedStack::for_test_at(destination, stack, states)
    }
}

#[cfg(test)]
fn test_push_destination() -> &'static PushDestination {
    static DESTINATION: std::sync::OnceLock<PushDestination> = std::sync::OnceLock::new();
    DESTINATION.get_or_init(|| {
        PushDestination::for_test("origin", "https://github.com/owner/repository.git", Vec::new())
            .expect("the test destination is valid")
    })
}

/// One local change coupled to its complete remote publication observation.
#[derive(Debug)]
pub(super) struct ObservedChange<'stack> {
    change: &'stack LocalChange,
    head: Option<ObjectId>,
    owned_base: Option<ObjectId>,
    versions: BTreeMap<Version, ObjectId>,
}

impl<'stack> ObservedChange<'stack> {
    pub(super) fn change(&self) -> &'stack LocalChange {
        self.change
    }

    pub(super) fn head(&self) -> Option<ObjectId> {
        self.head
    }

    pub(super) fn owned_base(&self) -> Option<ObjectId> {
        self.owned_base
    }

    pub(super) fn versions(&self) -> &BTreeMap<Version, ObjectId> {
        &self.versions
    }
}

/// Observes the default branch and every remote head with constant arguments.
pub(super) async fn observe_remote_heads(destination: &PushDestination) -> Result<RemoteHeads<'_>> {
    let command = destination.ls_remote(
        ["--quiet".to_owned(), "--symref".to_owned()],
        HEAD_ADVERTISEMENT_PATTERNS.map(str::to_owned),
    );
    let parsed = observe_remote_heads_command(
        command,
        destination.configured_remote(),
        subprocess::REMOTE_GIT_EXECUTION_TIMEOUT,
    )
    .await?;
    Ok(RemoteHeads { destination, parsed })
}

async fn observe_remote_heads_command(
    command: Command,
    configured_remote: &str,
    timeout: Duration,
) -> Result<ParsedRemoteHeads> {
    let started = Instant::now();
    let output = subprocess::output(command, timeout)
        .await
        .wrap_err_with(|| format!("Failed to observe GHerrit remote '{configured_remote}'"))?;
    if !output.status().success() {
        bail!("`git ls-remote` failed for GHerrit remote '{configured_remote}'");
    }
    let record_count = git_output_records(output.stdout()).count();
    log::trace!(
        "Observed GHerrit remote heads ({} bytes, {} records) in {:?}",
        output.stdout().len(),
        record_count,
        started.elapsed()
    );
    parse_remote_heads(output.stdout()).wrap_err_with(|| {
        format!("GHerrit remote '{configured_remote}' reported an invalid head advertisement")
    })
}

/// Legacy free-standing history observation used only by the active publisher.
///
/// FIXME(#264): Delete this wrapper at activation. New code must consume
/// [`RemoteHeads::observe_managed_tags`] so head and tag provenance cannot
/// diverge.
pub(super) async fn observe_active_managed_tags<'destination, 'id>(
    destination: &'destination PushDestination,
    ids: impl IntoIterator<Item = &'id GherritPrId>,
) -> Result<ActiveManagedTags<'destination>> {
    let histories = observe_managed_tag_namespaces(destination, ids).await?;
    Ok(ActiveManagedTags { destination, histories })
}

async fn observe_managed_tag_namespaces<'id>(
    destination: &PushDestination,
    ids: impl IntoIterator<Item = &'id GherritPrId>,
) -> Result<HashMap<GherritPrId, AdvertisedChangeNamespace>> {
    let ids = ids.into_iter().collect::<Vec<_>>();
    let tag_queries = plan_queries(&ids, managed_tag_pattern_bytes)?;
    let started = Instant::now();
    let mut total_bytes = 0_usize;
    let mut total_records = 0_usize;
    let mut histories = HashMap::new();
    for query in &tag_queries {
        let command = destination.ls_remote(["--quiet".to_owned()], query.managed_tag_patterns());
        let output = observe_active_managed_tag_query(
            command,
            destination.configured_remote(),
            subprocess::REMOTE_GIT_EXECUTION_TIMEOUT,
        )
        .await?;
        total_bytes = total_bytes.saturating_add(output.stdout().len());
        total_records = total_records.saturating_add(git_output_records(output.stdout()).count());
        let ParsedManagedTags { histories: query_histories } =
            parse_managed_tags(output.stdout(), query.ids())?;
        for (id, namespace) in query_histories {
            if histories.insert(id, namespace).is_some() {
                bail!("managed tag namespace was returned by more than one query");
            }
        }
    }

    log::trace!(
        "Observed GHerrit managed tag namespaces for {} active change(s) in {} request(s) ({} bytes, {} records) in {:?}",
        ids.len(),
        tag_queries.len(),
        total_bytes,
        total_records,
        started.elapsed()
    );
    Ok(histories)
}

async fn observe_active_managed_tag_query(
    command: Command,
    configured_remote: &str,
    timeout: Duration,
) -> Result<subprocess::CommandOutput> {
    let output = subprocess::output(command, timeout).await.wrap_err_with(|| {
        format!("Failed to observe active managed tags at GHerrit remote '{configured_remote}'")
    })?;
    if !output.status().success() {
        bail!(
            "`git ls-remote` failed while observing active managed tags at GHerrit remote '{configured_remote}'"
        );
    }
    Ok(output)
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

    fn managed_tag_patterns(&self) -> Vec<String> {
        self.ids().flat_map(managed_tag_patterns).collect()
    }
}

fn plan_queries(
    ids: &[&GherritPrId],
    encoded_bytes: impl Fn(&GherritPrId) -> usize,
) -> Result<Vec<Query>> {
    plan_queries_with_budget(ids, encoded_bytes, QUERY_ARGV_BUDGET_BYTES)
}

fn plan_queries_with_budget(
    ids: &[&GherritPrId],
    encoded_bytes: impl Fn(&GherritPrId) -> usize,
    budget: usize,
) -> Result<Vec<Query>> {
    let mut seen = HashSet::new();
    let planned = ids
        .iter()
        .map(|id| {
            if !seen.insert(id.as_str()) {
                bail!("remote observation requested the same GHerrit change twice");
            }
            let bytes = encoded_bytes(id);
            if bytes > budget {
                bail!(
                    "GHerrit change ID is too long for a remote observation query ({} bytes)",
                    id.as_str().len()
                );
            }
            Ok(((*id).clone(), bytes))
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

fn managed_tag_patterns(id: &GherritPrId) -> [String; 2] {
    let root =
        format!("{}{}", str::from_utf8(MANAGED_TAG_PREFIX).expect("ASCII prefix"), id.as_str());
    [root.clone(), format!("{root}/*")]
}

fn managed_tag_pattern_bytes(id: &GherritPrId) -> usize {
    managed_tag_patterns(id).iter().map(|pattern| pattern.len() + 1).sum()
}

/// A source-only acquisition inseparably bound to one remote observation.
///
/// Construction is private to destination-bound observations. The action owns
/// its exact advertised refs and validated command batches, while retaining
/// the destination which produced them. It therefore has no destination or
/// ref parameter which a caller could substitute at execution time.
struct ObjectAcquisition<'destination> {
    destination: &'destination PushDestination,
    object_count: usize,
    source_ref_count: usize,
    batches: Vec<FetchBatch>,
}

fn object_acquisition<'destination>(
    destination: &'destination PushDestination,
    object_count: usize,
    source_refs: Vec<String>,
) -> Result<ObjectAcquisition<'destination>> {
    if source_refs.is_empty() {
        bail!("object acquisition requires at least one exact advertised version ref");
    }
    let source_ref_count = source_refs.len();
    let batches = plan_fetches(&source_refs)?;
    Ok(ObjectAcquisition { destination, object_count, source_ref_count, batches })
}

impl ObjectAcquisition<'_> {
    /// Acquires the selected objects through this action's bound destination.
    ///
    /// `refetch` is deliberately one explicit caller choice rather than an
    /// internal retry loop. A caller may set it only after the repository's
    /// existing promisor fact is true and a normal acquisition still leaves
    /// history missing.
    async fn execute(&self, repo: &util::Repo, refetch: bool) -> Result<()> {
        if refetch && !repo.has_promisor_remote()? {
            bail!("`git fetch --refetch` requires a repository with promisor configuration");
        }
        let started = Instant::now();
        let mut response_bytes = 0_u64;

        for batch in &self.batches {
            let mut command = self.destination.fetch(batch.source_refs(), refetch);
            command.current_dir(repo.workdir().unwrap_or(repo.path()));
            let output = acquire_batch(
                command,
                self.destination.configured_remote(),
                subprocess::REMOTE_GIT_EXECUTION_TIMEOUT,
            )
            .await?;
            response_bytes = response_bytes
                .saturating_add(u64::try_from(output.stdout().len()).unwrap_or(u64::MAX))
                .saturating_add(output.stderr_bytes());
        }

        log::trace!(
            "Acquired {} advertised object(s) through {} exact version ref(s) in {} request(s) ({} response bytes) in {:?}",
            self.object_count,
            self.source_ref_count,
            self.batches.len(),
            response_bytes,
            started.elapsed()
        );
        Ok(())
    }
}

/// Loads one complete literal graph, with at most one ordinary acquisition and
/// one promisor refetch using the same destination-bound exact refs.
#[allow(dead_code)]
pub(super) async fn complete_graph_wave(
    repo: &util::Repo,
    observed: &DestinationObservation<'_>,
    wave_ids: &[GherritPrId],
    required_roots: &[ObjectId],
) -> Result<CommitGraphEvidence> {
    let mut complete_roots = required_roots.to_vec();
    complete_roots.extend(
        observed
            .histories
            .values()
            .flat_map(|history| history.versions.values())
            .map(|advertised| advertised.object_id),
    );
    complete_roots.sort_unstable();
    complete_roots.dedup();

    let missing = match CommitGraphEvidence::load(repo, complete_roots.iter().copied()) {
        Ok(graph) => return Ok(graph),
        Err(GraphLoadError::Invalid(error)) => return Err(error),
        Err(error @ GraphLoadError::MissingObject { .. }) => error,
    };

    let Some(acquisition) = observed.acquisition_for_changes(wave_ids)? else {
        return Err(graph_load_report(missing));
    };
    acquisition.execute(repo, false).await?;
    match CommitGraphEvidence::load(repo, complete_roots.iter().copied()) {
        Ok(graph) => Ok(graph),
        Err(GraphLoadError::Invalid(error)) => Err(error),
        Err(GraphLoadError::MissingObject { .. }) if repo.has_promisor_remote()? => {
            acquisition.execute(repo, true).await?;
            CommitGraphEvidence::load(repo, complete_roots.iter().copied())
                .map_err(graph_load_report)
        }
        Err(error) => Err(graph_load_report(error)),
    }
}

fn graph_load_report(error: GraphLoadError) -> color_eyre::Report {
    match error {
        GraphLoadError::Invalid(error) => error,
        error @ GraphLoadError::MissingObject { .. } => color_eyre::Report::new(error),
    }
}

async fn acquire_batch(
    command: Command,
    configured_remote: &str,
    timeout: Duration,
) -> Result<subprocess::CommandOutput> {
    let output = subprocess::output(command, timeout).await.wrap_err_with(|| {
        format!("Failed to acquire remote Git objects for GHerrit remote '{configured_remote}'")
    })?;
    if !output.status().success() {
        bail!(
            "`git fetch` failed while acquiring objects for GHerrit remote '{configured_remote}'"
        );
    }
    Ok(output)
}

struct FetchBatch {
    first: String,
    rest: Vec<String>,
}

impl FetchBatch {
    fn new(first: String) -> Self {
        Self { first, rest: Vec::new() }
    }

    fn source_refs(&self) -> impl Iterator<Item = String> + '_ {
        std::iter::once(&self.first).chain(&self.rest).cloned()
    }
}

fn plan_fetches(source_refs: &[String]) -> Result<Vec<FetchBatch>> {
    plan_fetches_with_budget(source_refs, QUERY_ARGV_BUDGET_BYTES)
}

fn plan_fetches_with_budget(source_refs: &[String], budget: usize) -> Result<Vec<FetchBatch>> {
    let mut source_refs = source_refs.to_vec();
    source_refs.sort_unstable();
    let mut seen = HashSet::new();
    let planned = source_refs
        .into_iter()
        .map(|source| {
            if !seen.insert(source.clone()) {
                bail!("object acquisition requested the same advertised source ref twice");
            }
            let bytes = source.len() + 1;
            if bytes > budget {
                bail!(
                    "An advertised version ref is too long for an object-acquisition request ({} bytes)",
                    source.len()
                );
            }
            Ok((source, bytes))
        })
        .collect::<Result<Vec<_>>>()?;

    let mut batches = Vec::new();
    let mut current = None::<FetchBatch>;
    let mut current_bytes = 0;
    for (source, bytes) in planned {
        if current.is_some() && current_bytes > budget - bytes {
            batches.push(current.take().expect("a full fetch batch exists"));
            current_bytes = 0;
        }
        current_bytes += bytes;
        match &mut current {
            Some(batch) => batch.rest.push(source),
            None => current = Some(FetchBatch::new(source)),
        }
    }
    if let Some(batch) = current {
        batches.push(batch);
    }
    Ok(batches)
}

#[derive(Clone, Copy, Eq, PartialEq)]
enum ObjectFormat {
    Sha1,
    Sha256,
}

impl ObjectFormat {
    fn from_hex(value: &[u8]) -> Result<Self> {
        let format = match value.len() {
            40 => Self::Sha1,
            64 => Self::Sha256,
            _ => bail!("remote ref value is not a full SHA-1 or SHA-256 object ID"),
        };
        if !value.iter().all(u8::is_ascii_hexdigit) {
            bail!("remote ref value is not a hexadecimal object ID");
        }
        if value.iter().all(|byte| *byte == b'0') {
            bail!("remote ref has a null object ID");
        }
        Ok(format)
    }
}

struct AdvertisedRefs {
    direct: HashMap<Vec<u8>, ObjectId>,
    symbolic: HashMap<Vec<u8>, Vec<u8>>,
}

/// Parses and validates every advertisement record before selecting heads.
fn parse_advertised_refs(output: &[u8]) -> Result<AdvertisedRefs> {
    let mut object_format = None;
    let mut direct = HashMap::<Vec<u8>, Vec<u8>>::new();
    let mut symbolic = HashMap::<Vec<u8>, Vec<u8>>::new();

    for (index, record) in git_output_records(output).enumerate() {
        let mut fields = record.split(|byte| *byte == b'\t');
        let (Some(value), Some(name), None) = (fields.next(), fields.next(), fields.next()) else {
            bail!("malformed `git ls-remote` record {} ({} bytes)", index + 1, record.len());
        };
        if let Some(target) = value.strip_prefix(b"ref: ") {
            validate_advertised_ref_name(name)?;
            validate_reserved_ref_name(name)?;
            gix::refs::FullName::try_from(target.as_bstr())
                .wrap_err("symbolic remote ref has an invalid target")?;
            if is_managed_ref_name(name) {
                bail!("managed remote ref is symbolic rather than direct");
            }
            match symbolic.entry(name.to_vec()) {
                Entry::Vacant(entry) => {
                    entry.insert(target.to_vec());
                }
                Entry::Occupied(_) => bail!("duplicate symbolic remote ref"),
            }
            continue;
        }

        let format = ObjectFormat::from_hex(value)?;
        if object_format.replace(format).is_some_and(|previous| previous != format) {
            bail!("remote advertisement mixes SHA-1 and SHA-256 object IDs");
        }
        let logical_name = name.strip_suffix(b"^{}").unwrap_or(name);
        validate_direct_advertised_ref_name(name)?;
        validate_reserved_ref_name(logical_name)?;
        if name != logical_name && is_managed_tag_name(logical_name) {
            bail!("managed tag is annotated rather than lightweight");
        }
        match direct.entry(name.to_vec()) {
            Entry::Vacant(entry) => {
                entry.insert(value.to_vec());
            }
            Entry::Occupied(_) => bail!("duplicate direct remote ref"),
        }
    }

    if object_format == Some(ObjectFormat::Sha256) {
        bail!("SHA-256 Git repositories are not supported");
    }
    let direct = direct
        .into_iter()
        .map(|(name, value)| {
            ObjectId::from_hex(&value)
                .wrap_err("remote ref value is not a SHA-1 object ID")
                .map(|object_id| (name, object_id))
        })
        .collect::<Result<HashMap<_, _>>>()?;
    Ok(AdvertisedRefs { direct, symbolic })
}

fn parse_remote_heads(output: &[u8]) -> Result<ParsedRemoteHeads> {
    let AdvertisedRefs { direct, symbolic } = parse_advertised_refs(output)?;
    let symbolic_head =
        symbolic.get(b"HEAD".as_slice()).ok_or_else(|| eyre!("missing symbolic HEAD"))?;
    let direct_head = direct.get(b"HEAD".as_slice()).ok_or_else(|| eyre!("missing direct HEAD"))?;
    let target_tip = direct
        .get(symbolic_head.as_slice())
        .ok_or_else(|| eyre!("symbolic HEAD target was not advertised"))?;
    if direct_head != target_tip {
        bail!("direct HEAD disagrees with its advertised target branch");
    }
    let target = gix::refs::FullName::try_from(symbolic_head.as_bstr())
        .wrap_err("symbolic HEAD has an invalid target")?;
    if target.category() != Some(gix::refs::Category::LocalBranch) {
        bail!("symbolic HEAD does not target a local branch");
    }
    let branch = symbolic_head
        .strip_prefix(HEAD_PREFIX)
        .ok_or_else(|| eyre!("symbolic HEAD does not target a local branch"))?;
    let branch = str::from_utf8(branch).wrap_err("default branch name is not UTF-8")?.to_owned();
    let default_branch = DefaultBranch::new(branch, *direct_head)?;

    let mut candidate_heads = HashMap::new();
    let mut owned_bases = HashMap::new();
    for (name, object_id) in direct {
        if name.ends_with(b"^{}") {
            continue;
        }
        if let Some(id) = parse_owned_base_name(&name)? {
            owned_bases.insert(id, object_id);
        } else if parse_managed_tag_name(&name)?.is_some() {
            bail!("head advertisement unexpectedly included managed tag history");
        } else if let Some(id) = parse_top_level_change_head(&name) {
            candidate_heads.insert(id, object_id);
        }
    }
    Ok(ParsedRemoteHeads { default_branch, candidate_heads, owned_bases })
}

#[cfg(test)]
pub(super) fn parse_remote_heads_for_test(output: &[u8]) -> Result<RemoteHeads<'static>> {
    // Tests in neighboring behavior modules deliberately enter through the
    // production byte parser instead of manufacturing a validated head set.
    let destination = Box::leak(Box::new(PushDestination::for_test(
        "origin",
        "https://github.com/owner/repository.git",
        Vec::new(),
    )?));
    parse_remote_heads_for_destination_for_test(destination, output)
}

#[cfg(test)]
pub(super) fn parse_remote_heads_for_destination_for_test<'destination>(
    destination: &'destination PushDestination,
    output: &[u8],
) -> Result<RemoteHeads<'destination>> {
    Ok(RemoteHeads { destination, parsed: parse_remote_heads(output)? })
}

#[cfg(test)]
pub(super) fn parse_active_change_for_test(
    id: GherritPrId,
    head_output: &[u8],
    managed_tag_output: &[u8],
) -> Result<ObservedChangeHistory> {
    // History-domain tests enter through both production byte parsers and the
    // exact active-set consumer. They never manufacture the opaque value.
    let heads = parse_remote_heads_for_test(head_output)?;
    let ParsedManagedTags { histories } = parse_managed_tags(managed_tag_output, [&id])?;
    let active = DestinationObservation { heads, histories }.into_active(&[id], &[])?;
    let mut local = active.local.into_vec();
    if local.len() != 1 {
        bail!("test active observation did not contain exactly one local change");
    }
    Ok(local.pop().expect("the exact test active set has one change"))
}

fn validate_advertised_ref_name(name: &[u8]) -> Result<()> {
    if name == b"HEAD" {
        return Ok(());
    }
    gix::refs::FullName::try_from(name.as_bstr()).wrap_err("remote ref has an invalid name")?;
    Ok(())
}

fn validate_direct_advertised_ref_name(name: &[u8]) -> Result<()> {
    let Some(tag) = name.strip_suffix(b"^{}") else {
        return validate_advertised_ref_name(name);
    };
    let tag = gix::refs::FullName::try_from(tag.as_bstr())
        .wrap_err("peeled remote ref has an invalid tag name")?;
    if tag.category() != Some(gix::refs::Category::Tag) {
        bail!("peeled remote ref is not a tag");
    }
    Ok(())
}

fn validate_reserved_ref_name(name: &[u8]) -> Result<()> {
    if name == OWNED_BASE_ROOT {
        bail!("remote ref uses the reserved owned-base namespace root");
    }
    if name.starts_with(OWNED_BASE_PREFIX) {
        parse_owned_base_name(name)?.expect("the reserved prefix was checked above");
    }
    if name == MANAGED_TAG_ROOT {
        bail!("remote ref uses the reserved managed-tag namespace root");
    }
    if name.starts_with(MANAGED_TAG_PREFIX) {
        parse_managed_tag_name(name)?.expect("the reserved prefix was checked above");
    }
    Ok(())
}

fn is_managed_ref_name(name: &[u8]) -> bool {
    parse_top_level_change_head(name).is_some()
        || name.starts_with(OWNED_BASE_PREFIX)
        || name.starts_with(MANAGED_TAG_PREFIX)
}

fn is_managed_tag_name(name: &[u8]) -> bool {
    name.starts_with(MANAGED_TAG_PREFIX)
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

struct Record<'a> {
    object_id: ObjectId,
    name: &'a [u8],
    peeled: bool,
}

fn records(output: &[u8]) -> impl Iterator<Item = Result<Record<'_>>> {
    git_output_records(output).enumerate().map(|(index, record)| {
        let mut fields = record.split(|byte| *byte == b'\t');
        let (Some(value), Some(name), None) = (fields.next(), fields.next(), fields.next()) else {
            bail!("malformed `git ls-remote` record {} ({} bytes)", index + 1, record.len());
        };
        if value.starts_with(b"ref: ") {
            bail!("remote observation unexpectedly contained a symbolic ref");
        }
        if value.len() != 40 || !value.iter().all(u8::is_ascii_hexdigit) {
            bail!("remote ref value is not a full SHA-1 object ID");
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

fn requested_names<'a>(
    ids: impl IntoIterator<Item = &'a GherritPrId>,
    name: impl Fn(&GherritPrId) -> String,
) -> Result<HashMap<Vec<u8>, GherritPrId>> {
    ids.into_iter().try_fold(HashMap::new(), |mut names, id| {
        match names.entry(name(id).into_bytes()) {
            Entry::Vacant(entry) => {
                entry.insert(id.clone());
            }
            Entry::Occupied(_) => bail!("remote observation requested the same change twice"),
        }
        Ok(names)
    })
}

struct ParsedManagedTags {
    histories: HashMap<GherritPrId, AdvertisedChangeNamespace>,
}

impl fmt::Debug for ParsedManagedTags {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ParsedManagedTags")
            .field("covered_change_count", &self.histories.len())
            .field(
                "advertised_ref_count",
                &self.histories.values().map(AdvertisedChangeNamespace::ref_count).sum::<usize>(),
            )
            .finish()
    }
}

fn parse_managed_tags<'a>(
    output: &[u8],
    ids: impl IntoIterator<Item = &'a GherritPrId>,
) -> Result<ParsedManagedTags> {
    let requested = requested_names(ids, |id| id.as_str().to_owned())?;
    let mut histories = requested
        .values()
        .cloned()
        .map(|id| (id, AdvertisedChangeNamespace::default()))
        .collect::<HashMap<_, _>>();

    for record in records(output) {
        let record = record?;
        if record.name == MANAGED_TAG_ROOT {
            bail!("remote ref uses the managed-tag namespace root");
        }
        if let Some(component) =
            record.name.strip_prefix(MANAGED_TAG_PREFIX).filter(|suffix| !suffix.contains(&b'/'))
        {
            GherritPrId::from_ref_component(component)
                .wrap_err("remote managed-tag namespace root has an invalid change ID")?;
            if requested.contains_key(component) {
                bail!("remote managed-tag namespace root exists for a requested GHerrit change");
            }
            bail!("remote advertised a managed-tag namespace for an unrequested GHerrit change");
        }
        let Some((parsed_id, tag)) = parse_managed_tag_name(record.name)? else {
            continue;
        };
        let id = requested.get(parsed_id.as_str().as_bytes()).ok_or_else(|| {
            eyre!("remote advertised managed tags for an unrequested GHerrit change")
        })?;
        if record.peeled {
            bail!(
                "remote managed tag for GHerrit change '{}' is annotated rather than lightweight",
                id.as_str()
            );
        }
        let namespace =
            histories.get_mut(id).expect("requested changes have initialized histories");
        match tag {
            ManagedTag::Version(version) => {
                let source_ref = str::from_utf8(record.name)
                    .expect("a validated managed version ref is ASCII")
                    .to_owned();
                let advertised = AdvertisedVersionRef { object_id: record.object_id, source_ref };
                if namespace.versions.insert(version, advertised).is_some() {
                    bail!(
                        "remote advertised version v{version} for GHerrit change '{}' more than once",
                        id.as_str()
                    );
                }
            }
            ManagedTag::PullRequestMarker => {
                if namespace.pull_request_marker.replace(record.object_id).is_some() {
                    bail!(
                        "remote advertised the pull-request marker for GHerrit change '{}' more than once",
                        id.as_str()
                    );
                }
            }
        }
    }
    Ok(ParsedManagedTags { histories })
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
        env,
        ffi::{OsStr, OsString},
        fs,
        io::Write as _,
        path::{Path, PathBuf},
        process::{self, Output},
        thread,
    };

    use super::*;

    const MAIN: &str = "1111111111111111111111111111111111111111";
    const ONE: &str = "2222222222222222222222222222222222222222";
    const TWO: &str = "3333333333333333333333333333333333333333";
    const SHA256: &str = "4444444444444444444444444444444444444444444444444444444444444444";
    const REEXEC_MODE: &str = "GHERRIT_REMOTE_COMMAND_TEST_MODE";
    const REEXEC_STDERR: &str = "GHERRIT_REMOTE_COMMAND_TEST_STDERR";
    const REEXEC_STATUS: &str = "GHERRIT_REMOTE_COMMAND_TEST_STATUS";
    const REEXEC_TEST: &str = "pre_push::remote::tests::remote_command_reexec_helper";

    fn head_advertisement(records: &str) -> Vec<u8> {
        format!("ref: refs/heads/main\tHEAD\n{MAIN}\tHEAD\n{MAIN}\trefs/heads/main\n{records}")
            .into_bytes()
    }

    fn id(value: &str) -> GherritPrId {
        GherritPrId::from_ref_component(value.as_bytes()).unwrap()
    }

    fn ids(values: &[&str]) -> Vec<GherritPrId> {
        values.iter().map(|value| id(value)).collect()
    }

    fn for_id<'a, T>(values: &'a HashMap<GherritPrId, T>, value: &str) -> &'a T {
        values.get(&id(value)).expect("requested change must be present")
    }

    fn empty_observation(ids: &[GherritPrId]) -> DestinationObservation<'static> {
        let heads = parse_remote_heads_for_test(&head_advertisement("")).unwrap();
        let ParsedManagedTags { histories } = parse_managed_tags(b"", ids).unwrap();
        DestinationObservation { heads, histories }
    }

    fn failing_reexec(stderr: &str, status: i32) -> Command {
        let mut command = Command::new(env::current_exe().unwrap());
        command
            .args(["--exact", REEXEC_TEST, "--nocapture"])
            .env(REEXEC_MODE, "fail")
            .env(REEXEC_STDERR, stderr)
            .env(REEXEC_STATUS, status.to_string());
        command
    }

    fn hanging_reexec() -> Command {
        let mut command = Command::new(env::current_exe().unwrap());
        command.args(["--exact", REEXEC_TEST, "--nocapture"]).env(REEXEC_MODE, "hang");
        command
    }

    #[test]
    fn remote_command_reexec_helper() {
        let Ok(mode) = env::var(REEXEC_MODE) else { return };
        match mode.as_str() {
            "fail" => {
                std::io::stderr().write_all(env::var(REEXEC_STDERR).unwrap().as_bytes()).unwrap();
                process::exit(env::var(REEXEC_STATUS).unwrap().parse().unwrap());
            }
            "hang" => thread::sleep(Duration::from_secs(10)),
            other => panic!("unknown remote-command re-exec mode {other}"),
        }
    }

    #[derive(Clone)]
    struct GitTestEnvironment {
        variables: Vec<(OsString, OsString)>,
    }

    impl GitTestEnvironment {
        fn new(root: &Path) -> Self {
            let home = root.join("home");
            let temporary = root.join("tmp");
            fs::create_dir_all(&home).unwrap();
            fs::create_dir_all(&temporary).unwrap();

            let mut variables = vec![
                (OsString::from("HOME"), home.clone().into_os_string()),
                (OsString::from("XDG_CONFIG_HOME"), home.join(".config").into_os_string()),
                (OsString::from("TMPDIR"), temporary.clone().into_os_string()),
                (OsString::from("TMP"), temporary.clone().into_os_string()),
                (OsString::from("TEMP"), temporary.into_os_string()),
                (OsString::from("GIT_CONFIG_NOSYSTEM"), OsString::from("1")),
                (OsString::from("GIT_TERMINAL_PROMPT"), OsString::from("0")),
                (OsString::from("LANG"), OsString::from("C")),
                (OsString::from("LC_ALL"), OsString::from("C")),
            ];
            if let Some(path) = env::var_os("PATH") {
                variables.push((OsString::from("PATH"), path));
            }
            #[cfg(windows)]
            for name in ["SystemRoot", "WINDIR", "COMSPEC", "PATHEXT"] {
                if let Some(value) = env::var_os(name) {
                    variables.push((OsString::from(name), value));
                }
            }
            Self { variables }
        }

        fn command(
            &self,
            current_dir: &Path,
            arguments: impl IntoIterator<Item = impl AsRef<OsStr>>,
        ) -> Output {
            let mut command = Command::new("git");
            command
                .env_clear()
                .envs(self.variables.iter().cloned())
                .current_dir(current_dir)
                .args(arguments);
            let output = command.output().unwrap();
            assert!(
                output.status.success(),
                "git failed: {}",
                String::from_utf8_lossy(&output.stderr)
            );
            output
        }

        fn ls_remote(
            &self,
            current_dir: &Path,
            remote: &Path,
            options: &[&str],
            patterns: &[&str],
        ) -> Command {
            let mut command = Command::new("git");
            command
                .env_clear()
                .envs(self.variables.iter().cloned())
                .current_dir(current_dir)
                .args(options)
                .arg("--")
                .arg(remote)
                .args(patterns);
            command
        }

        fn command_fails(
            &self,
            current_dir: &Path,
            arguments: impl IntoIterator<Item = impl AsRef<OsStr>>,
        ) -> Output {
            let mut command = Command::new("git");
            command
                .env_clear()
                .envs(self.variables.iter().cloned())
                .current_dir(current_dir)
                .args(arguments);
            let output = command.output().unwrap();
            assert!(!output.status.success());
            output
        }

        fn stdout(
            &self,
            current_dir: &Path,
            arguments: impl IntoIterator<Item = impl AsRef<OsStr>>,
        ) -> String {
            String::from_utf8(self.command(current_dir, arguments).stdout)
                .unwrap()
                .trim()
                .to_owned()
        }
    }

    fn seeded_remote() -> (tempfile::TempDir, GitTestEnvironment, PathBuf) {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path();
        let environment = GitTestEnvironment::new(root);
        let remote = root.join("remote.git");
        let seed = root.join("seed");
        environment.command(root, ["init", "--bare", "--initial-branch=main", "remote.git"]);
        environment.command(root, ["init", "--initial-branch=main", "seed"]);
        environment.command(
            &seed,
            [
                "-c",
                "user.name=GHerrit Test",
                "-c",
                "user.email=gherrit@example.invalid",
                "commit",
                "--allow-empty",
                "-m",
                "initial",
            ],
        );
        environment.command(&seed, ["push", remote.to_str().unwrap(), "HEAD:refs/heads/main"]);
        (directory, environment, remote)
    }

    #[tokio::test(flavor = "current_thread")]
    async fn async_head_observation_accepts_success_and_rejects_nonzero_status() {
        let (directory, environment, remote) = seeded_remote();
        let heads = observe_remote_heads_command(
            environment.ls_remote(
                directory.path(),
                &remote,
                &["ls-remote", "--quiet", "--symref"],
                &["HEAD", "refs/heads/*", "refs/tags/gherrit"],
            ),
            "origin",
            Duration::from_secs(5),
        )
        .await
        .unwrap();
        assert_eq!(heads.default_branch().name(), "main");

        let error = observe_remote_heads_command(
            failing_reexec("private-destination", 23),
            "origin",
            Duration::from_secs(5),
        )
        .await
        .unwrap_err();
        let diagnostic = format!("{error:?}");
        assert!(diagnostic.contains("`git ls-remote` failed"), "{diagnostic}");
        assert!(!diagnostic.contains("private-destination"), "{diagnostic}");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn async_head_observation_has_a_finite_execution_deadline() {
        let error =
            observe_remote_heads_command(hanging_reexec(), "origin", Duration::from_millis(100))
                .await
                .unwrap_err();

        assert!(format!("{error:?}").contains("timed out"), "error={error:?}");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn async_managed_tag_observation_accepts_success_and_rejects_nonzero_status() {
        let (directory, environment, remote) = seeded_remote();
        let main = environment.stdout(
            directory.path(),
            ["--git-dir", remote.to_str().unwrap(), "rev-parse", "refs/heads/main"],
        );
        environment.command(
            directory.path(),
            [
                "--git-dir",
                remote.to_str().unwrap(),
                "update-ref",
                "refs/tags/gherrit/Gone/v1",
                main.as_str(),
            ],
        );
        let requested = ids(&["Gone"]);
        let query = Query::new(requested[0].clone());
        let output = observe_active_managed_tag_query(
            environment.ls_remote(
                directory.path(),
                &remote,
                &["ls-remote", "--quiet"],
                &["refs/tags/gherrit/Gone/v1"],
            ),
            "origin",
            Duration::from_secs(5),
        )
        .await
        .unwrap();
        let observed = parse_managed_tags(output.stdout(), query.ids()).unwrap();
        assert_eq!(for_id(&observed.histories, "Gone").versions.len(), 1);

        let error = observe_active_managed_tag_query(
            failing_reexec("private-ref", 29),
            "origin",
            Duration::from_secs(5),
        )
        .await
        .unwrap_err();
        let diagnostic = format!("{error:?}");
        assert!(diagnostic.contains("`git ls-remote` failed"), "{diagnostic}");
        assert!(!diagnostic.contains("private-ref"), "{diagnostic}");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn async_managed_tag_observation_has_a_finite_execution_deadline() {
        let error = observe_active_managed_tag_query(
            hanging_reexec(),
            "origin",
            Duration::from_millis(100),
        )
        .await
        .unwrap_err();

        assert!(format!("{error:?}").contains("timed out"), "error={error:?}");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn observation_bound_action_cannot_be_redirected_and_has_no_repository_side_effects() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path();
        let environment = GitTestEnvironment::new(root);
        let push_remote = root.join("push-owner/push-repo.git");
        let fetch_remote = root.join("fetch-owner/fetch-repo.git");
        let push_seed = root.join("push-seed");
        let fetch_seed = root.join("fetch-seed");
        let client = root.join("client");
        fs::create_dir_all(push_remote.parent().unwrap()).unwrap();
        fs::create_dir_all(fetch_remote.parent().unwrap()).unwrap();

        for remote in [&push_remote, &fetch_remote] {
            environment.command(
                root,
                ["init", "--bare", "--initial-branch=main", remote.to_str().unwrap()],
            );
        }
        for (seed, message, remote) in [
            (&push_seed, "push history", &push_remote),
            (&fetch_seed, "fetch history", &fetch_remote),
        ] {
            environment.command(root, ["init", "--initial-branch=main", seed.to_str().unwrap()]);
            environment.command(
                seed,
                [
                    "-c",
                    "user.name=GHerrit Test",
                    "-c",
                    "user.email=gherrit@example.invalid",
                    "commit",
                    "--allow-empty",
                    "-m",
                    message,
                ],
            );
            environment.command(
                seed,
                [
                    "push",
                    remote.to_str().unwrap(),
                    "HEAD:refs/heads/main",
                    "HEAD:refs/tags/gherrit/Gone/v1",
                ],
            );
        }
        let push_oid = environment.stdout(&push_seed, ["rev-parse", "HEAD"]);
        let fetch_oid = environment.stdout(&fetch_seed, ["rev-parse", "HEAD"]);
        assert_ne!(push_oid, fetch_oid);

        environment.command(root, ["init", "--initial-branch=main", client.to_str().unwrap()]);
        environment.command(
            &client,
            [
                "-c",
                "user.name=GHerrit Test",
                "-c",
                "user.email=gherrit@example.invalid",
                "commit",
                "--allow-empty",
                "-m",
                "local history",
            ],
        );
        environment.command(&client, ["remote", "add", "origin", fetch_remote.to_str().unwrap()]);

        let push_destination = PushDestination::for_test(
            "origin",
            push_remote.to_str().unwrap(),
            environment.variables.clone(),
        )
        .unwrap();
        let fetch_destination = PushDestination::for_test(
            "upstream",
            fetch_remote.to_str().unwrap(),
            environment.variables.clone(),
        )
        .unwrap();
        let requested = id("Gone");
        let push_observation = observe_remote_heads(&push_destination)
            .await
            .unwrap()
            .observe_managed_tags(std::slice::from_ref(&requested))
            .await
            .unwrap();
        assert_eq!(push_observation.remote_heads().default_branch().name(), "main");
        assert_eq!(
            push_observation.remote_heads().candidate_head(&id("main")).unwrap().to_string(),
            push_oid
        );
        let fetch_observation = observe_remote_heads(&fetch_destination)
            .await
            .unwrap()
            .observe_managed_tags(std::slice::from_ref(&requested))
            .await
            .unwrap();
        let push_history = push_observation.histories[&requested]
            .versions
            .iter()
            .map(|(version, advertised)| (*version, advertised.object_id))
            .collect::<Vec<_>>();
        let fetch_history = fetch_observation.histories[&requested]
            .versions
            .iter()
            .map(|(version, advertised)| (*version, advertised.object_id))
            .collect::<Vec<_>>();
        assert_eq!(push_history.len(), 1);
        assert_eq!(push_history[0].1.to_string(), push_oid);
        assert_eq!(fetch_history.len(), 1);
        assert_eq!(fetch_history[0].1.to_string(), fetch_oid);

        let legacy_heads = observe_remote_heads(&push_destination).await.unwrap();
        let legacy_fetch_managed_tags =
            observe_active_managed_tags(&fetch_destination, [&requested]).await.unwrap();
        let stack = LocalStack::for_test(
            ObjectId::from_hex(MAIN.as_bytes()).unwrap(),
            [(requested.clone(), ObjectId::from_hex(ONE.as_bytes()).unwrap())],
        );
        let error = ObservedStack::couple(&stack, &legacy_heads, legacy_fetch_managed_tags)
            .expect_err("even the legacy seam rejects A-heads/B-tags");
        assert!(error.to_string().contains("different destinations"), "{error:?}");

        let acquisition = push_observation
            .acquisition_for_changes(std::slice::from_ref(&requested))
            .unwrap()
            .expect("the observed history has one exact ref");

        let push_commit = format!("{push_oid}^{{commit}}");
        let fetch_commit = format!("{fetch_oid}^{{commit}}");
        environment.command_fails(&client, ["cat-file", "-e", &push_commit]);
        environment.command_fails(&client, ["cat-file", "-e", &fetch_commit]);
        let refs_before =
            environment.command(&client, ["for-each-ref", "--format=%(refname)%00%(objectname)"]);
        let config_before = environment.command(&client, ["config", "--local", "--null", "--list"]);
        let repo = util::Repo::open(client.to_str().unwrap()).unwrap();

        acquisition.execute(&repo, false).await.unwrap();

        environment.command(&client, ["cat-file", "-e", &push_commit]);
        environment.command_fails(&client, ["cat-file", "-e", &fetch_commit]);
        assert_eq!(
            environment
                .command(&client, ["for-each-ref", "--format=%(refname)%00%(objectname)"])
                .stdout,
            refs_before.stdout
        );
        assert_eq!(
            environment.command(&client, ["config", "--local", "--null", "--list"]).stdout,
            config_before.stdout
        );
        assert!(!client.join(".git/FETCH_HEAD").exists());
        assert!(!client.join(".git/gc.log").exists());
        assert!(!client.join(".git/objects/info/commit-graph").exists());
        assert!(!client.join(".git/objects/pack/multi-pack-index").exists());
        assert!(
            fs::read_dir(client.join(".git/objects/pack")).unwrap().all(|entry| entry
                .unwrap()
                .path()
                .extension()
                != Some(OsStr::new("promisor")))
        );

        let error = acquisition.execute(&repo, true).await.unwrap_err();
        assert!(error.to_string().contains("promisor configuration"), "error={error:?}");

        environment.command(&client, ["config", "remote.origin.promisor", "true"]);
        let config_before_refetch =
            environment.command(&client, ["config", "--local", "--null", "--list"]);
        let promisor_repo = util::Repo::open(client.to_str().unwrap()).unwrap();
        acquisition.execute(&promisor_repo, true).await.unwrap();

        let active = push_observation.into_active(std::slice::from_ref(&requested), &[]).unwrap();
        assert!(std::ptr::eq(active.destination(), &push_destination));
        assert_eq!(active.local()[0].id(), &requested);
        assert_eq!(active.local()[0].versions().next().unwrap().1.to_string(), push_oid);

        assert_eq!(
            environment
                .command(&client, ["for-each-ref", "--format=%(refname)%00%(objectname)"])
                .stdout,
            refs_before.stdout
        );
        assert_eq!(
            environment.command(&client, ["config", "--local", "--null", "--list"]).stdout,
            config_before_refetch.stdout
        );
        assert!(!client.join(".git/FETCH_HEAD").exists());
        assert!(!client.join(".git/gc.log").exists());
        assert!(!client.join(".git/objects/info/commit-graph").exists());
        assert!(!client.join(".git/objects/pack/multi-pack-index").exists());
        assert!(
            fs::read_dir(client.join(".git/objects/pack")).unwrap().all(|entry| entry
                .unwrap()
                .path()
                .extension()
                != Some(OsStr::new("promisor")))
        );
    }

    #[cfg(unix)]
    #[tokio::test(flavor = "current_thread")]
    async fn missing_parent_uses_one_acquisition_and_one_promisor_refetch() {
        use std::os::unix::fs::PermissionsExt as _;

        let directory = tempfile::tempdir().unwrap();
        let root = directory.path();
        let mut environment = GitTestEnvironment::new(root);
        let remote = root.join("remote.git");
        let seed = root.join("seed");
        let client = root.join("client");
        environment.command(root, ["init", "--bare", "--initial-branch=main", "remote.git"]);
        environment.command(root, ["init", "--initial-branch=main", "seed"]);
        for message in ["parent", "advertised head"] {
            environment.command(
                &seed,
                [
                    "-c",
                    "user.name=GHerrit Test",
                    "-c",
                    "user.email=gherrit@example.invalid",
                    "commit",
                    "--allow-empty",
                    "-m",
                    message,
                ],
            );
        }
        environment.command(
            &seed,
            [
                "push",
                remote.to_str().unwrap(),
                "HEAD:refs/heads/main",
                "HEAD:refs/tags/gherrit/Gone/v1",
            ],
        );
        let head = environment.stdout(&seed, ["rev-parse", "HEAD"]);
        let parent = environment.stdout(&seed, ["rev-parse", "HEAD^"]);
        environment.command(root, ["init", "--initial-branch=main", "client"]);
        environment.command(&client, ["config", "remote.origin.promisor", "true"]);

        let source_object = seed.join(".git/objects").join(&head[..2]).join(&head[2..]);
        let target_directory = client.join(".git/objects").join(&head[..2]);
        fs::create_dir_all(&target_directory).unwrap();
        fs::copy(&source_object, target_directory.join(&head[2..])).unwrap();
        environment.command(&client, ["cat-file", "-e", &format!("{head}^{{commit}}")]);
        environment.command_fails(&client, ["cat-file", "-e", &format!("{parent}^{{commit}}")]);

        let real_git = env::split_paths(&env::var_os("PATH").unwrap())
            .map(|directory| directory.join("git"))
            .find(|candidate| candidate.is_file())
            .expect("git executable on PATH");
        let wrapper_directory = root.join("git-wrapper");
        let wrapper = wrapper_directory.join("git");
        let fetch_log = root.join("fetch.log");
        fs::create_dir_all(&wrapper_directory).unwrap();
        fs::write(
            &wrapper,
            b"#!/bin/sh\n\
              case \" $* \" in\n\
                *\" fetch \"*)\n\
                  printf 'fetch\\n' >> \"$GHERRIT_TEST_FETCH_LOG\"\n\
                  case \" $* \" in\n\
                    *\" --refetch \"*) exec \"$GHERRIT_TEST_REAL_GIT\" \"$@\" ;;\n\
                    *) exit 0 ;;\n\
                  esac\n\
                  ;;\n\
              esac\n\
              exec \"$GHERRIT_TEST_REAL_GIT\" \"$@\"\n",
        )
        .unwrap();
        fs::set_permissions(&wrapper, fs::Permissions::from_mode(0o755)).unwrap();
        let original_path = env::var_os("PATH").unwrap();
        let wrapped_path = env::join_paths(
            std::iter::once(wrapper_directory.clone()).chain(env::split_paths(&original_path)),
        )
        .unwrap();
        environment.variables.retain(|(name, _)| name != "PATH");
        environment.variables.extend([
            (OsString::from("PATH"), wrapped_path),
            (OsString::from("GHERRIT_TEST_REAL_GIT"), real_git.into_os_string()),
            (OsString::from("GHERRIT_TEST_FETCH_LOG"), fetch_log.clone().into_os_string()),
        ]);

        let destination = PushDestination::for_test(
            "origin",
            remote.to_str().unwrap(),
            environment.variables.clone(),
        )
        .unwrap();
        let change = id("Gone");
        let observed = observe_remote_heads(&destination)
            .await
            .unwrap()
            .observe_managed_tags(std::slice::from_ref(&change))
            .await
            .unwrap();
        let repo = util::Repo::open(client.to_str().unwrap()).unwrap();
        let head = ObjectId::from_hex(head.as_bytes()).unwrap();
        let parent = ObjectId::from_hex(parent.as_bytes()).unwrap();
        assert!(matches!(
            CommitGraphEvidence::load(&repo, [head]),
            Err(GraphLoadError::MissingObject { oid }) if oid == parent
        ));

        let graph = complete_graph_wave(&repo, &observed, std::slice::from_ref(&change), &[head])
            .await
            .unwrap();

        drop(graph);
        assert!(CommitGraphEvidence::load(&repo, [head]).is_ok());
        assert_eq!(fs::read_to_string(fetch_log).unwrap().lines().count(), 2);
        assert!(!client.join(".git/FETCH_HEAD").exists());
    }

    #[tokio::test(flavor = "current_thread")]
    async fn empty_acquisition_wave_preserves_missing_error_and_invalid_never_fetches() {
        let directory = tempfile::tempdir().unwrap();
        let writer = gix::init_bare(directory.path()).unwrap();
        let repo = util::Repo::open(directory.path().to_str().unwrap()).unwrap();
        let change = id("Gone");
        let observed = empty_observation(std::slice::from_ref(&change));
        let missing = ObjectId::from_bytes_or_panic(&[0x55; 20]);

        let error =
            complete_graph_wave(&repo, &observed, std::slice::from_ref(&change), &[missing])
                .await
                .err()
                .expect("no exact refs can repair an unrelated missing object");
        assert!(error.to_string().contains(&format!("Commit object {missing} is missing")));

        let blob = writer.write_blob(b"not a commit").unwrap().detach();
        let error = complete_graph_wave(&repo, &observed, std::slice::from_ref(&change), &[blob])
            .await
            .err()
            .expect("invalid evidence never reaches acquisition");
        assert!(error.to_string().contains("not a commit"));
    }

    #[test]
    fn parses_arbitrary_head_order_and_ignores_tail_matches() {
        let mut output = format!(
            "{MAIN}\trefs/heads/main\n\
             {ONE}\trefs/heads/Gone\n\
             ref: refs/remotes/origin/main\trefs/remotes/origin/HEAD\n\
             {MAIN}\trefs/remotes/origin/HEAD\n\
             {TWO}\trefs/archive/refs/heads/Gtail\n\
             ref: refs/heads/main\tHEAD\n\
             {TWO}\trefs/heads/gherrit-bases/Gone\n\
             {MAIN}\tHEAD\n"
        )
        .into_bytes();
        output.extend_from_slice(format!("{TWO}\trefs/heads/archive/").as_bytes());
        output.extend_from_slice(b"\xff\n");

        let heads = parse_remote_heads(&output).unwrap();
        assert_eq!(heads.default_branch().name(), "main");
        assert_eq!(heads.candidate_head(&id("Gone")).unwrap().to_string(), ONE);
        assert_eq!(heads.owned_base(&id("Gone")).unwrap().to_string(), TWO);
        assert_eq!(heads.candidate_head(&id("Gmissing")), None);
        assert_eq!(heads.owned_base(&id("Gmissing")), None);
    }

    #[test]
    fn complete_head_observation_proves_missing_managed_refs_absent() {
        let default = "default-branch";
        let output = format!(
            "ref: refs/heads/{default}\tHEAD\n\
             {MAIN}\tHEAD\n\
             {MAIN}\trefs/heads/{default}\n"
        );
        let heads = parse_remote_heads(output.as_bytes()).unwrap();

        assert!(heads.candidate_heads.is_empty());
        assert!(heads.owned_bases.is_empty());
        assert_eq!(heads.candidate_head(&id("Gone")), None);
        assert_eq!(heads.owned_base(&id("Gone")), None);
    }

    #[test]
    fn head_requires_symbolic_direct_and_target_agreement() {
        for output in [
            format!("{MAIN}\tHEAD\n{MAIN}\trefs/heads/main\n"),
            format!("ref: refs/heads/main\tHEAD\n{MAIN}\trefs/heads/main\n"),
            format!("ref: refs/heads/main\tHEAD\n{MAIN}\tHEAD\n"),
            format!("ref: refs/heads/main\tHEAD\n{MAIN}\tHEAD\n{ONE}\trefs/heads/main\n"),
            format!("ref: refs/tags/main\tHEAD\n{MAIN}\tHEAD\n{MAIN}\trefs/tags/main\n"),
        ] {
            assert!(parse_remote_heads(output.as_bytes()).is_err(), "output={output:?}");
        }
    }

    #[test]
    fn accepts_native_lines_and_an_optional_final_line_feed() {
        let with_final_line_feed = head_advertisement("");
        let without_final_line_feed =
            format!("ref: refs/heads/main\tHEAD\n{MAIN}\tHEAD\n{MAIN}\trefs/heads/main");
        for output in [with_final_line_feed, without_final_line_feed.into_bytes()] {
            assert_eq!(parse_remote_heads(&output).unwrap().default_branch().name(), "main");
        }
    }

    #[cfg(windows)]
    #[test]
    fn accepts_git_for_windows_crlf_records() {
        let output =
            format!("ref: refs/heads/main\tHEAD\r\n{MAIN}\tHEAD\r\n{MAIN}\trefs/heads/main\r\n");
        assert_eq!(parse_remote_heads(output.as_bytes()).unwrap().default_branch().name(), "main");
    }

    #[test]
    fn rejects_duplicate_symbolic_and_direct_records() {
        for (description, records) in [
            ("managed head", format!("{ONE}\trefs/heads/Gone\n{ONE}\trefs/heads/Gone\n")),
            ("direct HEAD", format!("{ONE}\tHEAD\n")),
            ("default target", format!("{ONE}\trefs/heads/main\n")),
            ("symbolic HEAD", "ref: refs/heads/main\tHEAD\n".to_owned()),
            (
                "unrelated symbolic ref",
                "ref: refs/heads/one\trefs/remotes/origin/HEAD\n\
                 ref: refs/heads/two\trefs/remotes/origin/HEAD\n"
                    .to_owned(),
            ),
        ] {
            let error = parse_remote_heads(&head_advertisement(&records)).unwrap_err();
            assert!(error.to_string().contains("duplicate"), "{description}: {error:?}");
        }

        let valid_pair = head_advertisement(&format!(
            "ref: refs/heads/unrelated\trefs/remotes/origin/HEAD\n\
             {ONE}\trefs/remotes/origin/HEAD\n"
        ));
        assert!(parse_remote_heads(&valid_pair).is_ok());
    }

    #[test]
    fn validates_every_record_including_unrelated_non_utf8_refs() {
        let mut valid = head_advertisement("");
        valid.extend_from_slice(format!("{ONE}\trefs/heads/archive/").as_bytes());
        valid.extend_from_slice(b"\xff\n");
        assert!(parse_remote_heads(&valid).is_ok());

        for malformed in [
            b"\n".to_vec(),
            b"not a record\n".to_vec(),
            b"xyz\trefs/heads/unrelated\n".to_vec(),
            format!("{}\trefs/heads/unrelated\n", "0".repeat(40)).into_bytes(),
            format!("{ONE}\tinvalid\n").into_bytes(),
        ] {
            let mut output = head_advertisement("");
            output.extend(malformed);
            assert!(parse_remote_heads(&output).is_err(), "output={output:?}");
        }
    }

    #[test]
    fn malformed_records_do_not_disclose_their_contents() {
        const HEAD_SECRET: &str = "refs/heads/privateSecretName";
        const VERSION_SECRET: &str = "refs/tags/gherrit/GprivateSecret/v1";

        let malformed_head = format!("{ONE}\t{HEAD_SECRET}\textra");
        let error = parse_remote_heads(&head_advertisement(&malformed_head)).unwrap_err();
        let diagnostic = format!("{error:?}");
        assert!(
            diagnostic.contains(&format!("record 4 ({} bytes)", malformed_head.len())),
            "diagnostic={diagnostic}"
        );
        assert!(!diagnostic.contains(HEAD_SECRET), "diagnostic={diagnostic}");

        let malformed_version = format!("{ONE}\t{VERSION_SECRET}\textra");
        let requested = ids(&["GprivateSecret"]);
        let error = parse_managed_tags(malformed_version.as_bytes(), &requested).unwrap_err();
        let diagnostic = format!("{error:?}");
        assert!(
            diagnostic.contains(&format!("record 1 ({} bytes)", malformed_version.len())),
            "diagnostic={diagnostic}"
        );
        assert!(!diagnostic.contains(VERSION_SECRET), "diagnostic={diagnostic}");

        const UNREQUESTED_SECRET: &str = "GunrequestedSecret";
        let requested = ids(&["Grequested"]);
        for name in [
            format!("refs/tags/gherrit/{UNREQUESTED_SECRET}"),
            format!("refs/tags/gherrit/{UNREQUESTED_SECRET}/v1"),
        ] {
            let output = format!("{ONE}\t{name}\n");
            let error = parse_managed_tags(output.as_bytes(), &requested).unwrap_err();
            let diagnostic = format!("{error:?}");
            assert!(diagnostic.contains("unrequested GHerrit change"), "{diagnostic}");
            assert!(!diagnostic.contains(UNREQUESTED_SECRET), "diagnostic={diagnostic}");
        }
    }

    #[test]
    fn rejects_unsupported_and_mixed_object_formats() {
        let sha256 =
            format!("ref: refs/heads/main\tHEAD\n{SHA256}\tHEAD\n{SHA256}\trefs/heads/main\n");
        assert!(parse_remote_heads(sha256.as_bytes()).unwrap_err().to_string().contains("SHA-256"));

        let mixed =
            format!("ref: refs/heads/main\tHEAD\n{MAIN}\tHEAD\n{SHA256}\trefs/heads/main\n");
        assert!(parse_remote_heads(mixed.as_bytes()).unwrap_err().to_string().contains("mixes"));
    }

    #[test]
    fn reserved_namespaces_and_default_branch_are_rejected() {
        for name in [
            "refs/heads/gherrit-bases",
            "refs/heads/gherrit-bases/",
            "refs/heads/gherrit-bases/G-one",
            "refs/heads/gherrit-bases/Gone/extra",
            "refs/tags/gherrit",
            "refs/tags/gherrit/",
            "refs/tags/gherrit/Gone/v1",
            "refs/tags/gherrit/Gone/pr",
        ] {
            assert!(
                parse_remote_heads(&head_advertisement(&format!("{ONE}\t{name}\n"))).is_err(),
                "name={name}"
            );
        }
        for target in [
            "refs/heads/gherrit-bases",
            "refs/heads/gherrit-bases/Gone",
            "refs/heads/gherrit-bases/nested/name",
        ] {
            let output = format!("ref: {target}\tHEAD\n{MAIN}\tHEAD\n{MAIN}\t{target}\n");
            assert!(parse_remote_heads(output.as_bytes()).is_err(), "target={target}");
        }
    }

    #[test]
    fn rejects_non_utf8_reserved_names_and_symbolic_managed_refs() {
        for prefix in [OWNED_BASE_PREFIX, MANAGED_TAG_PREFIX] {
            let mut output = head_advertisement("");
            output.extend_from_slice(format!("{ONE}\t").as_bytes());
            output.extend_from_slice(prefix);
            output.extend_from_slice(b"\xff\n");
            assert!(parse_remote_heads(&output).is_err(), "prefix={prefix:?}");
        }
        for name in ["refs/heads/Gone", "refs/heads/gherrit-bases/Gone"] {
            let output = head_advertisement(&format!("ref: refs/heads/other\t{name}\n"));
            assert!(parse_remote_heads(&output).is_err(), "name={name}");
        }
    }

    #[test]
    fn rejects_peeled_managed_heads_and_owned_bases() {
        for name in ["refs/heads/Gone", "refs/heads/gherrit-bases/Gone"] {
            let output = head_advertisement(&format!("{ONE}\t{name}^{{}}\n"));
            let error = parse_remote_heads(&output).unwrap_err();
            assert!(error.to_string().contains("peeled remote ref is not a tag"), "name={name}");
        }
    }

    #[test]
    fn rejects_symbolic_managed_tags_and_reserved_namespace_roots() {
        for (name, expected) in [
            ("refs/tags/gherrit/Gone/v1", "symbolic rather than direct"),
            ("refs/tags/gherrit/Gone/pr", "symbolic rather than direct"),
            ("refs/heads/gherrit-bases", "reserved owned-base namespace root"),
            ("refs/tags/gherrit", "reserved managed-tag namespace root"),
        ] {
            let output = head_advertisement(&format!("ref: refs/heads/unrelated\t{name}\n"));
            let error = parse_remote_heads(&output).unwrap_err();
            assert!(error.to_string().contains(expected), "name={name}: {error:?}");
        }
    }

    #[test]
    fn exact_managed_tag_parser_rejects_a_symbolic_pull_request_marker() {
        let requested = ids(&["Gone"]);
        let output = b"ref: refs/tags/gherrit/Gone/v1\trefs/tags/gherrit/Gone/pr\n";

        let error = parse_managed_tags(output, &requested).unwrap_err();

        assert!(error.to_string().contains("unexpectedly contained a symbolic ref"));
    }

    #[test]
    fn version_history_is_ordered_complete_and_may_repeat_objects() {
        let output = format!(
            "{ONE}\trefs/tags/gherrit/Gone/v2\n{TWO}\trefs/tags/gherrit/Gone/pr\n{ONE}\trefs/tags/gherrit/Gone/v1\n"
        );
        let requested = ids(&["Gone", "Gmissing"]);
        let observation = parse_managed_tags(output.as_bytes(), &requested).unwrap();

        assert_eq!(
            for_id(&observation.histories, "Gone")
                .versions
                .iter()
                .map(|(version, advertised)| { (version.get(), advertised.object_id.to_string()) })
                .collect::<Vec<_>>(),
            [(1, ONE.to_owned()), (2, ONE.to_owned())]
        );
        assert!(for_id(&observation.histories, "Gmissing").versions.is_empty());
        let sources = for_id(&observation.histories, "Gone").versions.values().collect::<Vec<_>>();
        assert_eq!(sources[0].object_id.to_string(), ONE);
        assert_eq!(sources[0].source_ref, "refs/tags/gherrit/Gone/v1");
        assert_eq!(sources[1].object_id.to_string(), ONE);
        assert_eq!(sources[1].source_ref, "refs/tags/gherrit/Gone/v2");
        assert_eq!(
            for_id(&observation.histories, "Gone").pull_request_marker,
            Some(ObjectId::from_hex(TWO.as_bytes()).unwrap())
        );
        assert_eq!(for_id(&observation.histories, "Gmissing").pull_request_marker, None);
    }

    #[test]
    fn rejects_malformed_duplicate_annotated_and_noncanonical_managed_refs() {
        let requested = ids(&["Gone"]);
        for output in [
            format!("{ONE}\trefs/tags/gherrit/Gone\n"),
            format!("{ONE}\trefs/tags/gherrit/Gone/v0\n"),
            format!("{ONE}\trefs/tags/gherrit/Gone/v01\n"),
            format!("{ONE}\trefs/tags/gherrit/Gone/v1/extra\n"),
            format!("{ONE}\trefs/tags/gherrit/Gone/pr/extra\n"),
            format!("{ONE}\trefs/tags/gherrit/Gone/pr0\n"),
            format!("{ONE}\trefs/tags/gherrit/Gone/v18446744073709551616\n"),
            format!("{ONE}\trefs/tags/gherrit/Gone/v1\n{TWO}\trefs/tags/gherrit/Gone/v1\n"),
            format!("{ONE}\trefs/tags/gherrit/Gone/v1\n{TWO}\trefs/tags/gherrit/Gone/v1^{{}}\n"),
            format!("{ONE}\trefs/tags/gherrit/Gone/pr\n{TWO}\trefs/tags/gherrit/Gone/pr\n"),
            format!("{ONE}\trefs/tags/gherrit/Gone/pr^{{}}\n"),
            format!("{ONE}\trefs/tags/gherrit/Gother/pr\n"),
        ] {
            assert!(parse_managed_tags(output.as_bytes(), &requested).is_err(), "{output:?}");
        }
    }

    #[test]
    fn active_query_planning_uses_exact_patterns_and_preflights_every_id() {
        let requested = ids(&["Gone", "Gtwo"]);
        let refs = requested.iter().collect::<Vec<_>>();
        let one = managed_tag_pattern_bytes(&requested[0]);
        let two = managed_tag_pattern_bytes(&requested[1]);
        let split =
            plan_queries_with_budget(&refs, managed_tag_pattern_bytes, one + two - 1).unwrap();

        assert_eq!(split.len(), 2);
        assert_eq!(
            split[0].managed_tag_patterns(),
            ["refs/tags/gherrit/Gone", "refs/tags/gherrit/Gone/*"]
        );
        assert_eq!(
            split[1].managed_tag_patterns(),
            ["refs/tags/gherrit/Gtwo", "refs/tags/gherrit/Gtwo/*"]
        );
        assert!(plan_queries_with_budget(&refs, managed_tag_pattern_bytes, one - 1).is_err());
        assert!(plan_queries_with_budget(&[], managed_tag_pattern_bytes, one).unwrap().is_empty());
        assert!(
            plan_queries_with_budget(
                &[&requested[0], &requested[0]],
                managed_tag_pattern_bytes,
                usize::MAX
            )
            .is_err()
        );
    }

    #[test]
    fn acquisition_batches_only_exact_advertised_source_refs() {
        let sources =
            ["refs/tags/gherrit/Gone/v1".to_owned(), "refs/tags/gherrit/Gtwo/v1".to_owned()];
        let refs = sources.iter().rev().cloned().collect::<Vec<_>>();
        let first_bytes = sources[0].len() + 1;
        let second_bytes = sources[1].len() + 1;
        let batches = plan_fetches_with_budget(&refs, first_bytes + second_bytes - 1).unwrap();

        assert_eq!(batches.len(), 2);
        assert_eq!(batches[0].source_refs().collect::<Vec<_>>(), ["refs/tags/gherrit/Gone/v1"]);
        assert_eq!(batches[1].source_refs().collect::<Vec<_>>(), ["refs/tags/gherrit/Gtwo/v1"]);
        assert!(plan_fetches_with_budget(&refs, first_bytes - 1).is_err());
        assert!(
            plan_fetches_with_budget(&[sources[0].clone(), sources[0].clone()], usize::MAX)
                .is_err()
        );
        assert!(plan_fetches_with_budget(&[], usize::MAX).unwrap().is_empty());
    }

    #[test]
    fn acquisition_selects_every_exact_ref_when_versions_repeat_an_object() {
        let requested = ids(&["Gone"]);
        let output = format!(
            "{ONE}\trefs/tags/gherrit/Gone/v2\n{TWO}\trefs/tags/gherrit/Gone/pr\n{ONE}\trefs/tags/gherrit/Gone/v1\n"
        );
        let ParsedManagedTags { histories } =
            parse_managed_tags(output.as_bytes(), &requested).unwrap();
        let heads = parse_remote_heads_for_test(&head_advertisement("")).unwrap();
        let observation = DestinationObservation { heads, histories };

        let acquisition = observation
            .acquisition_for_changes(&requested)
            .unwrap()
            .expect("the covered change has exact refs");

        assert_eq!(acquisition.object_count, 1);
        assert_eq!(acquisition.source_ref_count, 2);
        assert_eq!(acquisition.batches.len(), 1);
        assert_eq!(
            acquisition.batches[0].source_refs().collect::<Vec<_>>(),
            ["refs/tags/gherrit/Gone/v1", "refs/tags/gherrit/Gone/v2"]
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn cumulative_waves_are_disjoint_deterministic_and_retain_empty_coverage() {
        let (directory, environment, remote) = seeded_remote();
        let main = environment.stdout(
            directory.path(),
            ["--git-dir", remote.to_str().unwrap(), "rev-parse", "refs/heads/main"],
        );
        for name in [
            "refs/tags/gherrit/Gone/v2",
            "refs/tags/gherrit/Gone/v1",
            "refs/tags/gherrit/Gnonlocal/v1",
        ] {
            environment.command(
                directory.path(),
                ["--git-dir", remote.to_str().unwrap(), "update-ref", name, main.as_str()],
            );
        }
        let destination = PushDestination::for_test(
            "origin",
            remote.to_str().unwrap(),
            environment.variables.clone(),
        )
        .unwrap();
        let empty = id("Gempty");
        let local = id("Gone");
        let nonlocal = id("Gnonlocal");

        let observed = observe_remote_heads(&destination)
            .await
            .unwrap()
            .observe_managed_tags(&[empty.clone(), local.clone()])
            .await
            .unwrap();
        assert!(observed.histories[&empty].versions.is_empty());
        assert_eq!(observed.histories[&local].versions.len(), 2);
        let observed = observed.observe_additional(std::slice::from_ref(&nonlocal)).await.unwrap();
        let active = observed
            .into_active(&[local.clone(), empty.clone()], std::slice::from_ref(&nonlocal))
            .unwrap();

        assert_eq!(
            active.local().iter().map(ObservedChangeHistory::id).collect::<Vec<_>>(),
            [&local, &empty]
        );
        assert_eq!(
            active.local()[0].versions().map(|(version, _)| version.get()).collect::<Vec<_>>(),
            [1, 2]
        );
        assert_eq!(active.local()[1].versions().len(), 0);
        assert_eq!(active.nonlocal()[0].id(), &nonlocal);

        let duplicate_error = observe_remote_heads(&destination)
            .await
            .unwrap()
            .observe_managed_tags(&[local.clone(), local.clone()])
            .await
            .expect_err("the first wave rejects duplicate IDs");
        assert!(duplicate_error.to_string().contains("same GHerrit change twice"));

        let observed = observe_remote_heads(&destination)
            .await
            .unwrap()
            .observe_managed_tags(std::slice::from_ref(&local))
            .await
            .unwrap();
        let overlap_error = observed
            .observe_additional(std::slice::from_ref(&local))
            .await
            .expect_err("the second wave rejects prior coverage");
        assert!(overlap_error.to_string().contains("overlaps"));

        let observed = observe_remote_heads(&destination)
            .await
            .unwrap()
            .observe_managed_tags(std::slice::from_ref(&local))
            .await
            .unwrap();
        let duplicate_error = observed
            .observe_additional(&[nonlocal.clone(), nonlocal])
            .await
            .expect_err("the second wave rejects duplicate IDs");
        assert!(duplicate_error.to_string().contains("duplicate"));
    }

    #[test]
    fn exact_active_consumption_rejects_alignment_failures_and_preserves_order() {
        let a = id("Ga");
        let b = id("Gb");
        let c = id("Gc");

        let error = empty_observation(&[a.clone(), b.clone()])
            .into_active(std::slice::from_ref(&a), &[])
            .expect_err("extra cumulative coverage is rejected");
        assert!(error.to_string().contains("outside the exact active set"));

        let error = empty_observation(std::slice::from_ref(&a))
            .into_active(&[a.clone(), b.clone()], &[])
            .expect_err("missing cumulative coverage is rejected");
        assert!(error.to_string().contains("missing GHerrit change"));

        let error = empty_observation(&[a.clone(), b.clone()])
            .into_active(&[a.clone(), a.clone()], std::slice::from_ref(&b))
            .expect_err("duplicate local IDs are rejected");
        assert!(error.to_string().contains("duplicate"));

        let error = empty_observation(&[a.clone(), b.clone()])
            .into_active(std::slice::from_ref(&a), &[b.clone(), b.clone()])
            .expect_err("duplicate nonlocal IDs are rejected");
        assert!(error.to_string().contains("duplicate"));

        let error = empty_observation(std::slice::from_ref(&a))
            .into_active(std::slice::from_ref(&a), std::slice::from_ref(&a))
            .expect_err("local and nonlocal IDs must be disjoint");
        assert!(error.to_string().contains("overlap"));

        let active = empty_observation(&[a.clone(), b.clone(), c.clone()])
            .into_active(&[b.clone(), a.clone()], std::slice::from_ref(&c))
            .unwrap();
        assert_eq!(
            active.local().iter().map(ObservedChangeHistory::id).collect::<Vec<_>>(),
            [&b, &a]
        );
        assert_eq!(active.nonlocal()[0].id(), &c);
        assert!(active.local().iter().all(|change| change.versions().len() == 0));
    }

    #[test]
    fn coupling_rejects_missing_active_managed_tag_coverage() {
        let change = id("Gone");
        let stack = LocalStack::for_test(
            ObjectId::from_hex(MAIN.as_bytes()).unwrap(),
            [(change, ObjectId::from_hex(ONE.as_bytes()).unwrap())],
        );
        let destination =
            PushDestination::for_test("origin", "https://github.com/owner/repo.git", Vec::new())
                .unwrap();
        let heads = RemoteHeads {
            destination: &destination,
            parsed: parse_remote_heads(&head_advertisement("")).unwrap(),
        };
        let managed_tags =
            ActiveManagedTags { destination: &destination, histories: HashMap::new() };

        let error = ObservedStack::couple(&stack, &heads, managed_tags).unwrap_err();
        assert!(error.to_string().contains("was not observed"), "error={error:?}");
    }

    #[test]
    fn coupling_rejects_extra_active_managed_tag_coverage() {
        let change = id("Gone");
        let stack = LocalStack::for_test(
            ObjectId::from_hex(MAIN.as_bytes()).unwrap(),
            [(change.clone(), ObjectId::from_hex(ONE.as_bytes()).unwrap())],
        );
        let destination =
            PushDestination::for_test("origin", "https://github.com/owner/repo.git", Vec::new())
                .unwrap();
        let heads = RemoteHeads {
            destination: &destination,
            parsed: parse_remote_heads(&head_advertisement("")).unwrap(),
        };
        let managed_tags = ActiveManagedTags {
            destination: &destination,
            histories: HashMap::from([
                (change, AdvertisedChangeNamespace::default()),
                (id("Gextra"), AdvertisedChangeNamespace::default()),
            ]),
        };

        let error = ObservedStack::couple(&stack, &heads, managed_tags).unwrap_err();
        assert!(error.to_string().contains("outside the local stack"), "error={error:?}");
    }

    #[test]
    fn legacy_coupling_fails_closed_on_a_pull_request_marker() {
        let change = id("Gone");
        let stack = LocalStack::for_test(
            ObjectId::from_hex(MAIN.as_bytes()).unwrap(),
            [(change.clone(), ObjectId::from_hex(ONE.as_bytes()).unwrap())],
        );
        let destination =
            PushDestination::for_test("origin", "https://github.com/owner/repo.git", Vec::new())
                .unwrap();
        let heads = RemoteHeads {
            destination: &destination,
            parsed: parse_remote_heads(&head_advertisement("")).unwrap(),
        };
        let ParsedManagedTags { histories } =
            parse_managed_tags(format!("{ONE}\trefs/tags/gherrit/Gone/pr\n").as_bytes(), [&change])
                .unwrap();
        let managed_tags = ActiveManagedTags { destination: &destination, histories };

        let error = ObservedStack::couple(&stack, &heads, managed_tags).unwrap_err();
        assert!(error.to_string().contains("cannot safely consume the pull-request marker"));
    }
}

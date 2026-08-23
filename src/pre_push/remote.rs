//! Byte-oriented observations of the push repository's advertised refs.
//!
//! The symbolic default is observed first. Once it defines the local stack,
//! each later query names only that stack's candidate heads, owned bases, and
//! immutable tag namespaces. The first local query also rechecks the default
//! tip, so one observation never combines a stack derived from one default tip
//! with exact-local refs observed after that tip moved.

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
    local::GherritPrId,
    subprocess,
    version::Version,
};
use crate::util;

// Variable arguments are kept well below Windows' roughly 32-KiB command-line
// limit. This also gives POSIX implementations a conservative bound.
const QUERY_ARGV_BUDGET_BYTES: usize = 16 * 1024;
const DEFAULT_ADVERTISEMENT_PATTERNS: [&str; 1] = ["HEAD"];
const HEAD_PREFIX: &[u8] = b"refs/heads/";
const OWNED_BASE_PREFIX: &[u8] = b"refs/heads/gherrit-bases/";
const MANAGED_TAG_ROOT: &[u8] = b"refs/tags/gherrit";
const MANAGED_TAG_PREFIX: &[u8] = b"refs/tags/gherrit/";

/// The symbolic default branch and direct HEAD tip from one exact query.
pub(super) struct RemoteDefault<'destination> {
    destination: &'destination PushDestination,
    default_branch: DefaultBranch,
}

impl RemoteDefault<'_> {
    pub(super) fn default_branch(&self) -> &DefaultBranch {
        &self.default_branch
    }
}

impl<'destination> RemoteDefault<'destination> {
    /// Observes only the refs owned by the derived local change set.
    pub(super) async fn observe_local_state(
        self,
        local_ids: &[GherritPrId],
    ) -> Result<DestinationObservation<'destination>> {
        let LocalRemoteState { candidate_heads, owned_bases, histories } =
            observe_local_namespaces(self.destination, &self.default_branch, local_ids.iter())
                .await?;
        Ok(DestinationObservation {
            destination: self.destination,
            default_branch: self.default_branch,
            candidate_heads,
            owned_bases,
            histories,
        })
    }
}

/// Complete exact Git observation for the local change set.
pub(super) struct DestinationObservation<'destination> {
    destination: &'destination PushDestination,
    default_branch: DefaultBranch,
    candidate_heads: HashMap<GherritPrId, ObjectId>,
    owned_bases: HashMap<GherritPrId, ObjectId>,
    histories: HashMap<GherritPrId, AdvertisedChangeNamespace>,
}

impl fmt::Debug for DestinationObservation<'_> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("DestinationObservation")
            .field("default_branch", &self.default_branch)
            .field("candidate_head_count", &self.candidate_heads.len())
            .field("owned_base_count", &self.owned_bases.len())
            .field("covered_change_count", &self.histories.len())
            .field(
                "advertised_ref_count",
                &self.histories.values().map(AdvertisedChangeNamespace::ref_count).sum::<usize>(),
            )
            .finish()
    }
}

impl<'destination> DestinationObservation<'destination> {
    pub(super) fn default_branch(&self) -> &DefaultBranch {
        &self.default_branch
    }

    /// Consumes cumulative coverage and proves the exact ordered active set.
    pub(super) fn into_active(
        mut self,
        local_ids: &[GherritPrId],
    ) -> Result<ActiveRemoteChanges<'destination>> {
        let local_names = unique_id_names(local_ids, "local active changes")?;
        if let Some(missing) = local_ids.iter().find(|id| !self.histories.contains_key(*id)) {
            bail!("active remote observation is missing GHerrit change '{}'", missing.as_str());
        }
        if self.histories.keys().any(|id| !local_names.contains(id.as_str())) {
            bail!("active remote observation contains a change outside the exact active set");
        }

        let candidate_heads = &self.candidate_heads;
        let owned_bases = &self.owned_bases;
        let observe =
            |id: &GherritPrId, namespace: AdvertisedChangeNamespace| ObservedChangeHistory {
                id: id.clone(),
                candidate_head: candidate_heads.get(id).copied(),
                owned_base: owned_bases.get(id).copied(),
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
        debug_assert!(self.histories.is_empty());

        Ok(ActiveRemoteChanges {
            destination: self.destination,
            default_branch: self.default_branch,
            local,
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
        object_acquisition(self.destination, object_ids.len(), source_refs).map(Some)
    }
}

/// Exact active remote evidence, split in caller-supplied logical order.
pub(super) struct ActiveRemoteChanges<'destination> {
    destination: &'destination PushDestination,
    default_branch: DefaultBranch,
    local: Box<[ObservedChangeHistory]>,
}

impl fmt::Debug for ActiveRemoteChanges<'_> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ActiveRemoteChanges")
            .field("default_branch", &self.default_branch)
            .field("local", &self.local)
            .finish()
    }
}

impl<'destination> ActiveRemoteChanges<'destination> {
    pub(super) fn into_parts(
        self,
    ) -> (&'destination PushDestination, DefaultBranch, Box<[ObservedChangeHistory]>) {
        (self.destination, self.default_branch, self.local)
    }

    /// Constructs already-decoded planner input for semantic tests.
    #[cfg(test)]
    pub(super) fn from_typed_for_test(
        destination: &'destination PushDestination,
        default_branch: DefaultBranch,
        local: Vec<ObservedChangeHistory>,
    ) -> Self {
        Self { destination, default_branch, local: local.into_boxed_slice() }
    }

    #[cfg(test)]
    pub(super) fn local(&self) -> &[ObservedChangeHistory] {
        &self.local
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

    /// Constructs an exact decoded history without rendering an advertisement.
    #[cfg(test)]
    pub(super) fn from_typed_for_test(
        id: GherritPrId,
        published: &[(ObjectId, ObjectId)],
        pull_request_marker: Option<ObjectId>,
    ) -> Result<Self> {
        if let Some(marker) = pull_request_marker
            && !published.iter().any(|(head, _)| *head == marker)
        {
            bail!("literal test marker does not target published history");
        }
        let (candidate_head, owned_base) =
            published.last().copied().map_or((None, None), |(head, base)| (Some(head), Some(base)));
        let versions = published
            .iter()
            .enumerate()
            .map(|(index, (head, _))| {
                Version::from_history_index(index)
                    .map(|version| (version, *head))
                    .ok_or_else(|| eyre!("literal test history has too many versions"))
            })
            .collect::<Result<Vec<_>>>()?
            .into_boxed_slice();
        Ok(Self { id, candidate_head, owned_base, versions, pull_request_marker })
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

/// Observes only the symbolic default branch and its current tip.
pub(super) async fn observe_remote_default(
    destination: &PushDestination,
) -> Result<RemoteDefault<'_>> {
    let command = destination.ls_remote(
        ["--quiet".to_owned(), "--symref".to_owned()],
        DEFAULT_ADVERTISEMENT_PATTERNS.map(str::to_owned),
    );
    let default_branch = observe_remote_default_command(
        command,
        destination.configured_remote(),
        subprocess::REMOTE_GIT_EXECUTION_TIMEOUT,
    )
    .await?;
    Ok(RemoteDefault { destination, default_branch })
}

async fn observe_remote_default_command(
    command: Command,
    configured_remote: &str,
    timeout: Duration,
) -> Result<DefaultBranch> {
    let started = Instant::now();
    let output = subprocess::output(command, timeout)
        .await
        .wrap_err_with(|| format!("Failed to observe GHerrit remote '{configured_remote}'"))?;
    if !output.status().success() {
        bail!("`git ls-remote` failed for GHerrit remote '{configured_remote}'");
    }
    let record_count = git_output_records(output.stdout()).count();
    log::trace!(
        "Observed GHerrit remote default ({} bytes, {} records) in {:?}",
        output.stdout().len(),
        record_count,
        started.elapsed()
    );
    parse_remote_default(output.stdout()).wrap_err_with(|| {
        format!("GHerrit remote '{configured_remote}' reported an invalid default advertisement")
    })
}

struct LocalRemoteState {
    candidate_heads: HashMap<GherritPrId, ObjectId>,
    owned_bases: HashMap<GherritPrId, ObjectId>,
    histories: HashMap<GherritPrId, AdvertisedChangeNamespace>,
}

async fn observe_local_namespaces<'id>(
    destination: &PushDestination,
    default_branch: &DefaultBranch,
    ids: impl IntoIterator<Item = &'id GherritPrId>,
) -> Result<LocalRemoteState> {
    let ids = ids.into_iter().collect::<Vec<_>>();
    let default_pattern = format!("refs/heads/{}", default_branch.name());
    let default_bytes = default_pattern.len() + 1;
    if default_bytes >= QUERY_ARGV_BUDGET_BYTES {
        bail!("The repository default branch is too long for exact remote observation");
    }
    let queries = plan_queries_with_budget(
        &ids,
        local_observation_pattern_bytes,
        QUERY_ARGV_BUDGET_BYTES - default_bytes,
    )?;
    if queries.is_empty() {
        bail!("exact local remote observation requires at least one change");
    }
    let started = Instant::now();
    let mut total_bytes = 0_usize;
    let mut total_records = 0_usize;
    let mut candidate_heads = HashMap::new();
    let mut owned_bases = HashMap::new();
    let mut histories = HashMap::new();
    for (index, query) in queries.iter().enumerate() {
        let expected_default = (index == 0).then_some(default_branch);
        let command =
            destination.ls_remote(["--quiet".to_owned()], query.local_patterns(expected_default));
        let output = observe_exact_local_ref_query(
            command,
            destination.configured_remote(),
            subprocess::REMOTE_GIT_EXECUTION_TIMEOUT,
        )
        .await?;
        total_bytes = total_bytes.saturating_add(output.stdout().len());
        total_records = total_records.saturating_add(git_output_records(output.stdout()).count());
        let query_state = parse_local_remote_state(output.stdout(), query.ids(), expected_default)?;
        for (id, object_id) in query_state.candidate_heads {
            if candidate_heads.insert(id, object_id).is_some() {
                bail!("local candidate head was returned by more than one query");
            }
        }
        for (id, object_id) in query_state.owned_bases {
            if owned_bases.insert(id, object_id).is_some() {
                bail!("local owned base was returned by more than one query");
            }
        }
        for (id, namespace) in query_state.histories {
            if histories.insert(id, namespace).is_some() {
                bail!("managed tag namespace was returned by more than one query");
            }
        }
    }

    log::trace!(
        "Observed exact GHerrit refs for {} local change(s) in {} request(s) ({} bytes, {} records) in {:?}",
        ids.len(),
        queries.len(),
        total_bytes,
        total_records,
        started.elapsed()
    );
    Ok(LocalRemoteState { candidate_heads, owned_bases, histories })
}

async fn observe_exact_local_ref_query(
    command: Command,
    configured_remote: &str,
    timeout: Duration,
) -> Result<subprocess::CommandOutput> {
    let output = subprocess::output(command, timeout).await.wrap_err_with(|| {
        format!("Failed to observe exact local refs at GHerrit remote '{configured_remote}'")
    })?;
    if !output.status().success() {
        bail!(
            "`git ls-remote` failed while observing exact local refs at GHerrit remote '{configured_remote}'"
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

    fn local_patterns(&self, default_branch: Option<&DefaultBranch>) -> Vec<String> {
        default_branch
            .map(|default| format!("refs/heads/{}", default.name()))
            .into_iter()
            .chain(self.ids().flat_map(local_observation_patterns))
            .collect()
    }
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

fn local_observation_patterns(id: &GherritPrId) -> [String; 4] {
    let candidate = format!("refs/heads/{}", id.as_str());
    let owned_base = format!("refs/heads/gherrit-bases/{}", id.as_str());
    let [tag_root, tags] = managed_tag_patterns(id);
    [candidate, owned_base, tag_root, tags]
}

fn local_observation_pattern_bytes(id: &GherritPrId) -> usize {
    local_observation_patterns(id).iter().map(|pattern| pattern.len() + 1).sum()
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
            gix::refs::FullName::try_from(target.as_bstr())
                .wrap_err("symbolic remote ref has an invalid target")?;
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
        validate_direct_advertised_ref_name(name)?;
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

fn parse_remote_default(output: &[u8]) -> Result<DefaultBranch> {
    let AdvertisedRefs { direct, symbolic } = parse_advertised_refs(output)?;
    // `git ls-remote` patterns match ref tails, so the literal pattern `HEAD`
    // may also return refs such as `refs/heads/HEAD` and `refs/tags/HEAD`.
    // Validate those records above, but consume only the pseudo-ref `HEAD`.
    // Any other name is not a possible result of the query we issued.
    if direct.keys().chain(symbolic.keys()).any(|name| {
        name.as_slice() != b"HEAD" && !name.strip_suffix(b"^{}").unwrap_or(name).ends_with(b"/HEAD")
    }) {
        bail!("default observation returned a ref which does not tail-match HEAD");
    }
    let symbolic_head =
        symbolic.get(b"HEAD".as_slice()).ok_or_else(|| eyre!("missing symbolic HEAD"))?;
    let direct_head = direct.get(b"HEAD".as_slice()).ok_or_else(|| eyre!("missing direct HEAD"))?;
    let target = gix::refs::FullName::try_from(symbolic_head.as_bstr())
        .wrap_err("symbolic HEAD has an invalid target")?;
    if target.category() != Some(gix::refs::Category::LocalBranch) {
        bail!("symbolic HEAD does not target a local branch");
    }
    let branch = symbolic_head
        .strip_prefix(HEAD_PREFIX)
        .ok_or_else(|| eyre!("symbolic HEAD does not target a local branch"))?;
    let branch = str::from_utf8(branch).wrap_err("default branch name is not UTF-8")?.to_owned();
    DefaultBranch::new(branch, *direct_head)
}

fn parse_local_remote_state<'a>(
    output: &[u8],
    ids: impl IntoIterator<Item = &'a GherritPrId>,
    expected_default: Option<&DefaultBranch>,
) -> Result<LocalRemoteState> {
    let requested = requested_names(ids, |id| id.as_str().to_owned())?;
    let mut histories = requested
        .values()
        .cloned()
        .map(|id| (id, AdvertisedChangeNamespace::default()))
        .collect::<HashMap<_, _>>();
    let mut candidate_heads = HashMap::new();
    let mut owned_bases = HashMap::new();
    let expected_default_name =
        expected_default.map(|default| format!("refs/heads/{}", default.name()).into_bytes());
    let mut observed_default = false;
    for record in records(output) {
        let record = record?;
        if expected_default_name.as_deref() == Some(record.name) {
            if observed_default {
                bail!("exact local observation returned the default branch more than once");
            }
            if record.peeled {
                bail!("exact local observation returned a peeled default branch");
            }
            let expected = expected_default.expect("a default name came from a default value");
            if record.object_id != expected.tip() {
                bail!("the default branch moved after symbolic HEAD observation");
            }
            observed_default = true;
            continue;
        }
        if let Some(id) = parse_owned_base_name(record.name)? {
            let requested_id = requested.get(id.as_str().as_bytes()).ok_or_else(|| {
                eyre!("exact local observation returned an unrequested GHerrit owned base")
            })?;
            if record.peeled {
                bail!("exact local observation returned a peeled owned base");
            }
            if owned_bases.insert(requested_id.clone(), record.object_id).is_some() {
                bail!("exact local observation returned the same owned base more than once");
            }
            continue;
        }
        if let Some(id) = parse_top_level_change_head(record.name) {
            let requested_id = requested.get(id.as_str().as_bytes()).ok_or_else(|| {
                eyre!("exact local observation returned an unrequested GHerrit head")
            })?;
            if record.peeled {
                bail!("exact local observation returned a peeled candidate head");
            }
            if candidate_heads.insert(requested_id.clone(), record.object_id).is_some() {
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
            let requested_id = requested.get(id.as_str().as_bytes()).ok_or_else(|| {
                eyre!("remote advertised managed tags for an unrequested GHerrit change")
            })?;
            if record.peeled {
                bail!(
                    "remote managed tag for GHerrit change '{}' is annotated rather than lightweight",
                    requested_id.as_str()
                );
            }
            let namespace = histories
                .get_mut(requested_id)
                .expect("requested changes have initialized histories");
            match tag {
                ManagedTag::Version(version) => {
                    let source_ref = str::from_utf8(record.name)
                        .expect("a validated managed version ref is ASCII")
                        .to_owned();
                    let advertised =
                        AdvertisedVersionRef { object_id: record.object_id, source_ref };
                    if namespace.versions.insert(version, advertised).is_some() {
                        bail!(
                            "remote advertised version v{version} for GHerrit change '{}' more than once",
                            requested_id.as_str()
                        );
                    }
                }
                ManagedTag::PullRequestMarker => {
                    if namespace.pull_request_marker.replace(record.object_id).is_some() {
                        bail!(
                            "remote advertised the pull-request marker for GHerrit change '{}' more than once",
                            requested_id.as_str()
                        );
                    }
                }
            }
            continue;
        }
        bail!("exact local observation returned an unrelated remote ref");
    }
    if expected_default.is_some() && !observed_default {
        bail!("exact local observation omitted the default branch");
    }
    Ok(LocalRemoteState { candidate_heads, owned_bases, histories })
}

#[cfg(test)]
pub(super) fn parse_active_change_for_test(
    id: GherritPrId,
    default_branch: DefaultBranch,
    local_output: &[u8],
) -> Result<ObservedChangeHistory> {
    let destination = Box::leak(Box::new(PushDestination::for_test(
        "origin",
        "https://github.com/owner/repository.git",
        Vec::new(),
    )?));
    let LocalRemoteState { candidate_heads, owned_bases, histories } =
        parse_local_remote_state(local_output, [&id], Some(&default_branch))?;
    let active = DestinationObservation {
        destination,
        default_branch,
        candidate_heads,
        owned_bases,
        histories,
    }
    .into_active(&[id])?;
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

    const DEFAULT: &str = "1111111111111111111111111111111111111111";
    const HEAD: &str = "2222222222222222222222222222222222222222";
    const BASE: &str = "3333333333333333333333333333333333333333";
    const SHA256: &str = "4444444444444444444444444444444444444444444444444444444444444444";
    const REEXEC_MODE: &str = "GHERRIT_EXACT_REMOTE_COMMAND_TEST_MODE";
    const REEXEC_STDERR: &str = "GHERRIT_EXACT_REMOTE_COMMAND_TEST_STDERR";
    const REEXEC_STATUS: &str = "GHERRIT_EXACT_REMOTE_COMMAND_TEST_STATUS";
    const REEXEC_TEST: &str = "pre_push::remote::tests::remote_command_reexec_helper";

    fn id(value: &str) -> GherritPrId {
        GherritPrId::from_ref_component(value.as_bytes()).unwrap()
    }

    fn default_branch() -> DefaultBranch {
        DefaultBranch::new("main".to_owned(), ObjectId::from_hex(DEFAULT.as_bytes()).unwrap())
            .unwrap()
    }

    fn default_output() -> String {
        format!("ref: refs/heads/main\tHEAD\n{DEFAULT}\tHEAD\n")
    }

    fn local_output() -> String {
        format!(
            "{DEFAULT}\trefs/heads/main\n\
             {HEAD}\trefs/heads/Gone\n\
             {BASE}\trefs/heads/gherrit-bases/Gone\n\
             {HEAD}\trefs/tags/gherrit/Gone/v1\n\
             {HEAD}\trefs/tags/gherrit/Gone/pr\n"
        )
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

        fn ls_remote(
            &self,
            current_dir: &Path,
            remote: &Path,
            options: &[&str],
            patterns: &[String],
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

    fn empty_observation(ids: &[GherritPrId]) -> DestinationObservation<'static> {
        let destination = Box::leak(Box::new(
            PushDestination::for_test(
                "origin",
                "https://github.com/owner/repository.git",
                Vec::new(),
            )
            .unwrap(),
        ));
        DestinationObservation {
            destination,
            default_branch: default_branch(),
            candidate_heads: HashMap::new(),
            owned_bases: HashMap::new(),
            histories: ids
                .iter()
                .cloned()
                .map(|id| (id, AdvertisedChangeNamespace::default()))
                .collect(),
        }
    }

    fn assert_no_fetch_side_effects(client: &Path) {
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

    #[tokio::test(flavor = "current_thread")]
    async fn remote_command_boundaries_accept_success_and_censor_nonzero_output() {
        let (directory, environment, remote) = seeded_remote();
        let default = observe_remote_default_command(
            environment.ls_remote(
                directory.path(),
                &remote,
                &["ls-remote", "--quiet", "--symref"],
                &["HEAD".to_owned()],
            ),
            "origin",
            Duration::from_secs(5),
        )
        .await
        .unwrap();
        assert_eq!(default.name(), "main");

        let change = id("Gone");
        let patterns = Query::new(change.clone()).local_patterns(Some(&default));
        let output = observe_exact_local_ref_query(
            environment.ls_remote(directory.path(), &remote, &["ls-remote", "--quiet"], &patterns),
            "origin",
            Duration::from_secs(5),
        )
        .await
        .unwrap();
        assert!(parse_local_remote_state(output.stdout(), [&change], Some(&default)).is_ok());

        for error in [
            observe_remote_default_command(
                failing_reexec("private-default-output", 23),
                "origin",
                Duration::from_secs(5),
            )
            .await
            .unwrap_err(),
            observe_exact_local_ref_query(
                failing_reexec("private-local-output", 29),
                "origin",
                Duration::from_secs(5),
            )
            .await
            .unwrap_err(),
        ] {
            let diagnostic = format!("{error:?}");
            assert!(diagnostic.contains("`git ls-remote` failed"), "{diagnostic}");
            assert!(!diagnostic.contains("private-"), "{diagnostic}");
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn every_remote_observation_command_has_a_finite_execution_deadline() {
        let default =
            observe_remote_default_command(hanging_reexec(), "origin", Duration::from_millis(100))
                .await
                .unwrap_err();
        let local =
            observe_exact_local_ref_query(hanging_reexec(), "origin", Duration::from_millis(100))
                .await
                .unwrap_err();

        assert!(format!("{default:?}").contains("timed out"));
        assert!(format!("{local:?}").contains("timed out"));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn acquisition_is_bound_to_observed_exact_refs_without_local_ref_side_effects() {
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

        let destination = PushDestination::for_test(
            "origin",
            push_remote.to_str().unwrap(),
            environment.variables.clone(),
        )
        .unwrap();
        let requested = id("Gone");
        let observed = observe_remote_default(&destination)
            .await
            .unwrap()
            .observe_local_state(std::slice::from_ref(&requested))
            .await
            .unwrap();
        let acquisition = observed
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
        assert_no_fetch_side_effects(&client);

        let error = acquisition.execute(&repo, true).await.unwrap_err();
        assert!(error.to_string().contains("promisor configuration"), "{error:?}");

        environment.command(&client, ["config", "remote.origin.promisor", "true"]);
        let config_before_refetch =
            environment.command(&client, ["config", "--local", "--null", "--list"]);
        let promisor_repo = util::Repo::open(client.to_str().unwrap()).unwrap();
        acquisition.execute(&promisor_repo, true).await.unwrap();

        let active = observed.into_active(std::slice::from_ref(&requested)).unwrap();
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
        assert_no_fetch_side_effects(&client);
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
        let observed = observe_remote_default(&destination)
            .await
            .unwrap()
            .observe_local_state(std::slice::from_ref(&change))
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
        assert_no_fetch_side_effects(&client);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn empty_acquisition_preserves_missing_errors_and_invalid_objects_never_fetch() {
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
    fn default_observation_accepts_only_symbolic_and_direct_head() {
        let observed = parse_remote_default(default_output().as_bytes()).unwrap();
        assert_eq!(observed.name(), "main");
        assert_eq!(observed.tip().to_string(), DEFAULT);

        let tail_matches = format!(
            "{}ref: refs/heads/other\trefs/heads/HEAD\n{HEAD}\trefs/heads/HEAD\n{BASE}\trefs/tags/HEAD\n{BASE}\trefs/tags/gherrit/Gone/HEAD\n{HEAD}\trefs/tags/gherrit/Gone/HEAD^{{}}\n",
            default_output()
        );
        let observed = parse_remote_default(tail_matches.as_bytes()).unwrap();
        assert_eq!(observed.name(), "main");
        assert_eq!(observed.tip().to_string(), DEFAULT);

        for output in [
            String::new(),
            format!("{DEFAULT}\tHEAD\n"),
            "ref: refs/heads/main\tHEAD\n".to_string(),
            format!("{}{DEFAULT}\trefs/heads/main\n", default_output()),
            format!("ref: refs/tags/main\tHEAD\n{DEFAULT}\tHEAD\n"),
        ] {
            assert!(parse_remote_default(output.as_bytes()).is_err(), "{output:?}");
        }
    }

    #[test]
    fn advertisement_decoders_reject_malformed_duplicate_symbolic_null_and_mixed_records() {
        let duplicate_symbolic =
            format!("ref: refs/heads/main\tHEAD\nref: refs/heads/other\tHEAD\n{DEFAULT}\tHEAD\n");
        let duplicate_direct =
            format!("ref: refs/heads/main\tHEAD\n{DEFAULT}\tHEAD\n{HEAD}\tHEAD\n");
        let mixed =
            format!("ref: refs/heads/main\tHEAD\n{DEFAULT}\tHEAD\n{SHA256}\trefs/heads/HEAD\n");
        for output in [
            b"not-a-record\n".as_slice(),
            duplicate_symbolic.as_bytes(),
            duplicate_direct.as_bytes(),
            format!("ref: refs/heads/main\tHEAD\n{}\tHEAD\n", "0".repeat(40)).as_bytes(),
            mixed.as_bytes(),
        ] {
            assert!(parse_remote_default(output).is_err(), "accepted {output:?}");
        }

        let change = id("Gone");
        for record in [
            b"not-a-record\n".as_slice(),
            b"ref: refs/heads/other\trefs/heads/Gone\n".as_slice(),
            format!("{}\trefs/heads/Gone\n", "0".repeat(40)).as_bytes(),
            format!("{SHA256}\trefs/heads/Gone\n").as_bytes(),
            format!("{HEAD}\trefs/heads/Gone\n{HEAD}\trefs/heads/Gone\n").as_bytes(),
        ] {
            assert!(
                parse_local_remote_state(record, [&change], None).is_err(),
                "accepted {record:?}"
            );
        }
    }

    #[test]
    fn advertisements_use_native_line_framing_and_allow_an_optional_final_terminator() {
        let native = default_output();
        let no_final_lf = native.strip_suffix('\n').unwrap();
        let crlf = native.replace('\n', "\r\n");
        for output in [native.as_bytes(), no_final_lf.as_bytes()] {
            assert!(parse_remote_default(output).is_ok(), "rejected {output:?}");
        }
        #[cfg(windows)]
        assert!(parse_remote_default(crlf.as_bytes()).is_ok());
        #[cfg(not(windows))]
        assert!(parse_remote_default(crlf.as_bytes()).is_err());

        let change = id("Gone");
        let native = local_output();
        let no_final_lf = native.strip_suffix('\n').unwrap();
        let crlf = native.replace('\n', "\r\n");
        for output in [native.as_bytes(), no_final_lf.as_bytes()] {
            assert!(
                parse_local_remote_state(output, [&change], Some(&default_branch())).is_ok(),
                "rejected {output:?}"
            );
        }
        #[cfg(windows)]
        assert!(
            parse_local_remote_state(crlf.as_bytes(), [&change], Some(&default_branch())).is_ok()
        );
        #[cfg(not(windows))]
        assert!(
            parse_local_remote_state(crlf.as_bytes(), [&change], Some(&default_branch())).is_err()
        );
    }

    #[test]
    fn exact_local_parser_couples_all_owned_refs_and_default_agreement() {
        let change = id("Gone");
        let parsed =
            parse_local_remote_state(local_output().as_bytes(), [&change], Some(&default_branch()))
                .unwrap();
        assert_eq!(parsed.candidate_heads[&change].to_string(), HEAD);
        assert_eq!(parsed.owned_bases[&change].to_string(), BASE);
        let history = &parsed.histories[&change];
        assert_eq!(history.versions.len(), 1);
        assert_eq!(history.pull_request_marker.unwrap().to_string(), HEAD);
    }

    #[test]
    fn exact_local_parser_proves_absence_only_after_default_recheck() {
        let change = id("Gone");
        let output = format!("{DEFAULT}\trefs/heads/main\n");
        let parsed =
            parse_local_remote_state(output.as_bytes(), [&change], Some(&default_branch()))
                .unwrap();
        assert!(parsed.candidate_heads.is_empty());
        assert!(parsed.owned_bases.is_empty());
        assert!(parsed.histories[&change].versions.is_empty());

        assert!(parse_local_remote_state(b"", [&change], Some(&default_branch())).is_err());
        let moved = format!("{HEAD}\trefs/heads/main\n");
        assert!(
            parse_local_remote_state(moved.as_bytes(), [&change], Some(&default_branch()))
                .err()
                .expect("a moved default must be rejected")
                .to_string()
                .contains("moved")
        );

        assert!(parse_local_remote_state(b"", [&change], None).is_ok());
        assert!(
            parse_local_remote_state(output.as_bytes(), [&change], None).is_err(),
            "later batches must not silently consume a default-branch record"
        );
    }

    #[test]
    fn exact_local_parser_rejects_unrequested_duplicate_and_annotated_refs() {
        let change = id("Gone");
        for suffix in [
            format!("{HEAD}\trefs/heads/Other\n"),
            format!("{HEAD}\trefs/heads/gherrit-bases/Other\n"),
            format!("{HEAD}\trefs/tags/gherrit/Other/v1\n"),
            format!("{HEAD}\trefs/heads/Gone\n{HEAD}\trefs/heads/Gone\n"),
            format!("{HEAD}\trefs/tags/gherrit/Gone/v1\n{BASE}\trefs/tags/gherrit/Gone/v1^{{}}\n"),
            format!("{HEAD}\trefs/heads/Gone^{{}}\n"),
            format!("{HEAD}\trefs/heads/gherrit-bases/Gone^{{}}\n"),
            format!("{HEAD}\trefs/heads/unrelated/nested\n"),
        ] {
            let output = format!("{DEFAULT}\trefs/heads/main\n{suffix}");
            assert!(
                parse_local_remote_state(output.as_bytes(), [&change], Some(&default_branch()),)
                    .is_err(),
                "{output:?}"
            );
        }

        let mut non_utf8 = format!("{DEFAULT}\trefs/heads/main\n{HEAD}\trefs/heads/").into_bytes();
        non_utf8.extend_from_slice(b"\xff\n");
        assert!(parse_local_remote_state(&non_utf8, [&change], Some(&default_branch()),).is_err());

        let symbolic_managed =
            format!("ref: refs/heads/other\trefs/heads/Gone\n{DEFAULT}\trefs/heads/main\n");
        assert!(
            parse_local_remote_state(
                symbolic_managed.as_bytes(),
                [&change],
                Some(&default_branch()),
            )
            .is_err()
        );
    }

    #[test]
    fn managed_tag_namespaces_are_canonical_and_marker_identity_is_unique() {
        let change = id("Gone");
        for invalid_ref in [
            "refs/tags/gherrit",
            "refs/tags/gherrit/Gone",
            "refs/tags/gherrit/Gone/v0",
            "refs/tags/gherrit/Gone/v01",
            "refs/tags/gherrit/Gone/v",
            "refs/tags/gherrit/Gone/vx",
            "refs/tags/gherrit/Gone/other",
            "refs/tags/gherrit/Gone/v1/extra",
            "refs/tags/gherrit/Gone/pr/extra",
        ] {
            let output = format!("{DEFAULT}\trefs/heads/main\n{HEAD}\t{invalid_ref}\n");
            assert!(
                parse_local_remote_state(output.as_bytes(), [&change], Some(&default_branch()),)
                    .is_err(),
                "accepted {invalid_ref}"
            );
        }

        let duplicate_marker = format!(
            "{DEFAULT}\trefs/heads/main\n{HEAD}\trefs/tags/gherrit/Gone/pr\n{BASE}\trefs/tags/gherrit/Gone/pr\n"
        );
        assert!(
            parse_local_remote_state(
                duplicate_marker.as_bytes(),
                [&change],
                Some(&default_branch()),
            )
            .is_err()
        );

        // Parsing retains the exact sparse version set. Structural history
        // normalization owns the exhaustive contiguous-version check.
        let gap = format!(
            "{DEFAULT}\trefs/heads/main\n{HEAD}\trefs/tags/gherrit/Gone/v1\n{BASE}\trefs/tags/gherrit/Gone/v3\n"
        );
        let parsed =
            parse_local_remote_state(gap.as_bytes(), [&change], Some(&default_branch())).unwrap();
        assert_eq!(
            parsed.histories[&change]
                .versions
                .keys()
                .map(|version| version.get())
                .collect::<Vec<_>>(),
            [1, 3]
        );
    }

    #[test]
    fn version_history_is_ordered_and_retains_every_exact_ref_even_for_repeated_objects() {
        let change = id("Gone");
        let output = format!(
            "{DEFAULT}\trefs/heads/main\n{HEAD}\trefs/tags/gherrit/Gone/v2\n{BASE}\trefs/tags/gherrit/Gone/pr\n{HEAD}\trefs/tags/gherrit/Gone/v1\n"
        );
        let parsed =
            parse_local_remote_state(output.as_bytes(), [&change], Some(&default_branch()))
                .unwrap();
        let history = &parsed.histories[&change];
        assert_eq!(
            history
                .versions
                .iter()
                .map(|(version, advertised)| {
                    (
                        version.get(),
                        advertised.object_id.to_string(),
                        advertised.source_ref.as_str(),
                    )
                })
                .collect::<Vec<_>>(),
            [
                (1, HEAD.to_owned(), "refs/tags/gherrit/Gone/v1"),
                (2, HEAD.to_owned(), "refs/tags/gherrit/Gone/v2"),
            ]
        );
        assert_eq!(history.pull_request_marker.unwrap().to_string(), BASE);
    }

    #[test]
    fn exact_local_parser_handles_multiple_ids_in_arbitrary_record_order() {
        let a = id("A");
        let b = id("B");
        let output = format!(
            "{HEAD}\trefs/tags/gherrit/B/v1\n{BASE}\trefs/heads/gherrit-bases/A\n{DEFAULT}\trefs/heads/main\n{HEAD}\trefs/heads/B\n{BASE}\trefs/tags/gherrit/A/v1\n"
        );
        let parsed =
            parse_local_remote_state(output.as_bytes(), [&a, &b], Some(&default_branch())).unwrap();
        assert_eq!(parsed.histories.len(), 2);
        assert_eq!(parsed.histories[&a].versions.len(), 1);
        assert_eq!(parsed.histories[&b].versions.len(), 1);
        assert_eq!(parsed.candidate_heads[&b].to_string(), HEAD);
        assert_eq!(parsed.owned_bases[&a].to_string(), BASE);
    }

    #[test]
    fn exact_query_planning_is_unique_bounded_and_names_only_local_refs() {
        let a = id("A");
        let b = id("B");
        let one = local_observation_pattern_bytes(&a);
        let two = local_observation_pattern_bytes(&b);
        let split =
            plan_queries_with_budget(&[&a, &b], local_observation_pattern_bytes, one + two - 1)
                .unwrap();
        assert_eq!(split.len(), 2);
        assert_eq!(
            split[0].local_patterns(Some(&default_branch())),
            [
                "refs/heads/main",
                "refs/heads/A",
                "refs/heads/gherrit-bases/A",
                "refs/tags/gherrit/A",
                "refs/tags/gherrit/A/*",
            ]
        );
        assert_eq!(
            split[1].local_patterns(None),
            [
                "refs/heads/B",
                "refs/heads/gherrit-bases/B",
                "refs/tags/gherrit/B",
                "refs/tags/gherrit/B/*",
            ]
        );
        assert!(
            plan_queries_with_budget(&[&a, &a], local_observation_pattern_bytes, usize::MAX)
                .is_err()
        );
        assert!(plan_queries_with_budget(&[&a], local_observation_pattern_bytes, one - 1).is_err());
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
        assert_eq!(batches[0].source_refs().collect::<Vec<_>>(), [sources[0].clone()]);
        assert_eq!(batches[1].source_refs().collect::<Vec<_>>(), [sources[1].clone()]);
        assert!(plan_fetches_with_budget(&refs, first_bytes - 1).is_err());
        assert!(
            plan_fetches_with_budget(&[sources[0].clone(), sources[0].clone()], usize::MAX)
                .is_err()
        );
        assert!(plan_fetches_with_budget(&[], usize::MAX).unwrap().is_empty());
    }

    #[test]
    fn acquisition_selects_every_exact_ref_when_versions_repeat_an_object() {
        let change = id("Gone");
        let output = format!(
            "{DEFAULT}\trefs/heads/main\n{HEAD}\trefs/tags/gherrit/Gone/v2\n{BASE}\trefs/tags/gherrit/Gone/pr\n{HEAD}\trefs/tags/gherrit/Gone/v1\n"
        );
        let LocalRemoteState { candidate_heads, owned_bases, histories } =
            parse_local_remote_state(output.as_bytes(), [&change], Some(&default_branch()))
                .unwrap();
        let destination = Box::leak(Box::new(
            PushDestination::for_test(
                "origin",
                "https://github.com/owner/repository.git",
                Vec::new(),
            )
            .unwrap(),
        ));
        let observation = DestinationObservation {
            destination,
            default_branch: default_branch(),
            candidate_heads,
            owned_bases,
            histories,
        };

        let acquisition = observation
            .acquisition_for_changes(std::slice::from_ref(&change))
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

    #[test]
    fn exact_active_consumption_preserves_order_and_rejects_alignment_failures() {
        let a = id("A");
        let b = id("B");
        let destination = Box::leak(Box::new(
            PushDestination::for_test(
                "origin",
                "https://github.com/owner/repository.git",
                Vec::new(),
            )
            .unwrap(),
        ));
        let observation = |ids: &[GherritPrId]| DestinationObservation {
            destination,
            default_branch: default_branch(),
            candidate_heads: HashMap::new(),
            owned_bases: HashMap::new(),
            histories: ids
                .iter()
                .cloned()
                .map(|id| (id, AdvertisedChangeNamespace::default()))
                .collect(),
        };

        let active =
            observation(&[a.clone(), b.clone()]).into_active(&[b.clone(), a.clone()]).unwrap();
        assert_eq!(
            active.local().iter().map(ObservedChangeHistory::id).collect::<Vec<_>>(),
            [&b, &a]
        );
        assert!(
            observation(&[a.clone(), b.clone()]).into_active(std::slice::from_ref(&a)).is_err()
        );
        assert!(observation(std::slice::from_ref(&a)).into_active(&[a.clone(), b]).is_err());
    }
}

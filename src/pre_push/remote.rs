//! Byte-oriented observations of the push repository's advertised refs.
//!
//! Repository-wide heads and exact active immutable histories have different
//! completeness domains. A missing `RemoteHeads` entry proves absence because
//! every head was advertised. A missing `ActiveVersionTags` entry is an error
//! because only explicitly requested histories are complete.

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
const VERSION_ROOT: &[u8] = b"refs/tags/gherrit";
const VERSION_PREFIX: &[u8] = b"refs/tags/gherrit/";

/// Complete, syntactically valid state from one repository-wide head query.
#[derive(Debug)]
pub(super) struct RemoteHeads {
    default_branch: DefaultBranch,
    candidate_heads: HashMap<GherritPrId, ObjectId>,
    owned_bases: HashMap<GherritPrId, ObjectId>,
}

impl RemoteHeads {
    pub(super) fn default_branch(&self) -> &DefaultBranch {
        &self.default_branch
    }

    /// Returns a syntactically eligible top-level head.
    ///
    /// A matching name is only candidate evidence. Pull-request metadata or
    /// the corresponding owned-base ref must establish managed identity.
    pub(super) fn candidate_head(&self, id: &GherritPrId) -> Option<ObjectId> {
        self.candidate_heads.get(id).copied()
    }

    pub(super) fn owned_base(&self, id: &GherritPrId) -> Option<ObjectId> {
        self.owned_bases.get(id).copied()
    }
}

/// Complete immutable histories from one destination for exactly the
/// explicitly requested IDs.
pub(super) struct ActiveVersionTags<'destination> {
    destination: &'destination PushDestination,
    histories: HashMap<GherritPrId, BTreeMap<Version, AdvertisedVersionRef>>,
}

impl fmt::Debug for ActiveVersionTags<'_> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ActiveVersionTags")
            .field("covered_change_count", &self.histories.len())
            .field(
                "advertised_ref_count",
                &self.histories.values().map(BTreeMap::len).sum::<usize>(),
            )
            .finish()
    }
}

impl<'destination> ActiveVersionTags<'destination> {
    /// Returns the complete version/object evidence for one covered change.
    ///
    /// Source refs stay private. Normalization can inspect every advertised
    /// object without recovering or fabricating the ref which carried it.
    #[allow(dead_code)]
    pub(super) fn history(
        &self,
        id: &GherritPrId,
    ) -> Option<impl ExactSizeIterator<Item = (Version, ObjectId)> + '_> {
        self.histories.get(id).map(|history| {
            history.iter().map(|(version, advertised)| (*version, advertised.object_id))
        })
    }

    /// Constructs an acquisition bound to this observation's destination.
    ///
    /// Each object ID may be requested once and must occur in this
    /// observation. If multiple version tags advertise the same object, all
    /// of their exact refs are selected in deterministic order. That makes a
    /// repeated object unambiguous without inventing a preferred source ref.
    #[allow(dead_code)]
    pub(super) fn acquisition(
        &self,
        object_ids: impl IntoIterator<Item = ObjectId>,
    ) -> Result<ObjectAcquisition<'destination>> {
        let mut requested = HashSet::new();
        let mut source_refs = Vec::new();
        for object_id in object_ids {
            if !requested.insert(object_id) {
                bail!("object acquisition requested the same advertised object twice");
            }
            let previous_len = source_refs.len();
            source_refs.extend(
                self.histories
                    .values()
                    .flat_map(BTreeMap::values)
                    .filter(|advertised| advertised.object_id == object_id)
                    .map(|advertised| advertised.source_ref.clone()),
            );
            if source_refs.len() == previous_len {
                bail!("object acquisition requested an object absent from the observation");
            }
        }
        if requested.is_empty() {
            bail!("object acquisition requires at least one advertised object");
        }

        let source_ref_count = source_refs.len();
        let batches = plan_fetches(&source_refs)?;
        Ok(ObjectAcquisition {
            destination: self.destination,
            object_count: requested.len(),
            source_ref_count,
            batches,
        })
    }

    fn take_history(&mut self, id: &GherritPrId) -> Option<BTreeMap<Version, ObjectId>> {
        self.histories.remove(id).map(|history| {
            history
                .into_iter()
                .map(|(version, advertised)| (version, advertised.object_id))
                .collect()
        })
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

/// Remote head and immutable history for each local change, in stack order.
#[derive(Debug)]
pub(super) struct ObservedStack<'stack> {
    changes: Vec<ObservedChange<'stack>>,
}

impl<'stack> ObservedStack<'stack> {
    pub(super) fn couple(
        stack: &'stack LocalStack,
        heads: &RemoteHeads,
        mut versions: ActiveVersionTags<'_>,
    ) -> Result<Self> {
        let changes = stack
            .iter()
            .map(|change| {
                let history = versions.take_history(change.id()).ok_or_else(|| {
                    eyre!(
                        "version history for GHerrit change '{}' was not observed",
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
        if !versions.histories.is_empty() {
            bail!("remote version observation contains a change outside the local stack");
        }
        Ok(Self { changes })
    }

    #[cfg(test)]
    pub(super) fn for_test(
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
        Self { changes }
    }

    pub(super) fn iter(&self) -> impl ExactSizeIterator<Item = &ObservedChange<'stack>> {
        self.changes.iter()
    }
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
pub(super) async fn observe_remote_heads(destination: &PushDestination) -> Result<RemoteHeads> {
    let command = destination.ls_remote(
        ["--quiet".to_owned(), "--symref".to_owned()],
        HEAD_ADVERTISEMENT_PATTERNS.map(str::to_owned),
    );
    observe_remote_heads_command(
        command,
        destination.configured_remote(),
        subprocess::REMOTE_GIT_EXECUTION_TIMEOUT,
    )
    .await
}

async fn observe_remote_heads_command(
    command: Command,
    configured_remote: &str,
    timeout: Duration,
) -> Result<RemoteHeads> {
    let started = Instant::now();
    let output = subprocess::output(command, timeout)
        .await
        .wrap_err_with(|| format!("Failed to observe GHerrit remote '{configured_remote}'"))?;
    if !output.status.success() {
        bail!("`git ls-remote` failed for GHerrit remote '{configured_remote}'");
    }
    let record_count = git_output_records(&output.stdout).count();
    log::trace!(
        "Observed GHerrit remote heads ({} bytes, {} records) in {:?}",
        output.stdout.len(),
        record_count,
        started.elapsed()
    );
    parse_remote_heads(&output.stdout).wrap_err_with(|| {
        format!("GHerrit remote '{configured_remote}' reported an invalid head advertisement")
    })
}

/// Observes exact immutable histories only for explicitly requested IDs.
pub(super) async fn observe_active_version_tags<'destination, 'id>(
    destination: &'destination PushDestination,
    ids: impl IntoIterator<Item = &'id GherritPrId>,
) -> Result<ActiveVersionTags<'destination>> {
    let ids = ids.into_iter().collect::<Vec<_>>();
    let version_queries = plan_queries(&ids, version_pattern_bytes)?;
    let started = Instant::now();
    let mut total_bytes = 0;
    let mut total_records = 0;
    let mut histories = HashMap::new();
    for query in &version_queries {
        let command = destination.ls_remote(["--quiet".to_owned()], query.version_patterns());
        let output = observe_active_version_query(
            command,
            destination.configured_remote(),
            subprocess::REMOTE_GIT_EXECUTION_TIMEOUT,
        )
        .await?;
        total_bytes += output.stdout.len();
        total_records += git_output_records(&output.stdout).count();
        let ParsedVersionTags { histories: query_histories } =
            parse_versions(&output.stdout, query.ids())?;
        for (id, versions) in query_histories {
            if histories.insert(id, versions).is_some() {
                bail!("version history was returned by more than one query");
            }
        }
    }

    log::trace!(
        "Observed GHerrit version history for {} active change(s) in {} request(s) ({} bytes, {} records) in {:?}",
        ids.len(),
        version_queries.len(),
        total_bytes,
        total_records,
        started.elapsed()
    );
    Ok(ActiveVersionTags { destination, histories })
}

async fn observe_active_version_query(
    command: Command,
    configured_remote: &str,
    timeout: Duration,
) -> Result<std::process::Output> {
    let output = subprocess::output(command, timeout).await.wrap_err_with(|| {
        format!("Failed to observe active version history at GHerrit remote '{configured_remote}'")
    })?;
    if !output.status.success() {
        bail!(
            "`git ls-remote` failed while observing active version history at GHerrit remote '{configured_remote}'"
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

    fn version_patterns(&self) -> Vec<String> {
        self.ids().flat_map(version_patterns).collect()
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

fn version_patterns(id: &GherritPrId) -> [String; 2] {
    let root = format!("{}{}", str::from_utf8(VERSION_PREFIX).expect("ASCII prefix"), id.as_str());
    [root.clone(), format!("{root}/*")]
}

fn version_pattern_bytes(id: &GherritPrId) -> usize {
    version_patterns(id).iter().map(|pattern| pattern.len() + 1).sum()
}

/// A source-only acquisition inseparably bound to one remote observation.
///
/// Construction is private to [`ActiveVersionTags`]. The action owns its
/// exact advertised refs and validated command batches, while retaining the
/// destination which produced them. It therefore has no destination or ref
/// parameter which a caller could substitute at execution time.
pub(super) struct ObjectAcquisition<'destination> {
    destination: &'destination PushDestination,
    object_count: usize,
    source_ref_count: usize,
    batches: Vec<FetchBatch>,
}

impl ObjectAcquisition<'_> {
    /// Acquires the selected objects through this action's bound destination.
    ///
    /// `refetch` is deliberately one explicit caller choice rather than an
    /// internal retry loop. A caller may set it only after the repository's
    /// existing promisor fact is true and a normal acquisition still leaves
    /// history missing.
    #[allow(dead_code)]
    pub(super) async fn execute(&self, repo: &util::Repo, refetch: bool) -> Result<()> {
        if refetch && !repo.has_promisor_remote()? {
            bail!("`git fetch --refetch` requires a repository with promisor configuration");
        }
        let started = Instant::now();
        let mut response_bytes = 0;

        for batch in &self.batches {
            let mut command = self.destination.fetch(batch.source_refs(), refetch);
            command.current_dir(repo.workdir().unwrap_or(repo.path()));
            let output = acquire_batch(
                command,
                self.destination.configured_remote(),
                subprocess::REMOTE_GIT_EXECUTION_TIMEOUT,
            )
            .await?;
            response_bytes += output.stdout.len() + output.stderr.len();
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

async fn acquire_batch(
    command: Command,
    configured_remote: &str,
    timeout: Duration,
) -> Result<std::process::Output> {
    let output = subprocess::output(command, timeout).await.wrap_err_with(|| {
        format!("Failed to acquire remote Git objects for GHerrit remote '{configured_remote}'")
    })?;
    if !output.status.success() {
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
        if name != logical_name && is_version_tag_name(logical_name) {
            bail!("managed version tag is annotated rather than lightweight");
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

fn parse_remote_heads(output: &[u8]) -> Result<RemoteHeads> {
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
        } else if parse_version_tag_name(&name)?.is_some() {
            bail!("head advertisement unexpectedly included immutable version history");
        } else if let Some(id) = parse_top_level_change_head(&name) {
            candidate_heads.insert(id, object_id);
        }
    }
    Ok(RemoteHeads { default_branch, candidate_heads, owned_bases })
}

#[cfg(test)]
pub(super) fn parse_remote_heads_for_test(output: &[u8]) -> Result<RemoteHeads> {
    // Tests in neighboring behavior modules deliberately enter through the
    // production byte parser instead of manufacturing a validated head set.
    parse_remote_heads(output)
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
    if name == VERSION_ROOT {
        bail!("remote ref uses the reserved version-tag namespace root");
    }
    if name.starts_with(VERSION_PREFIX) {
        parse_version_tag_name(name)?.expect("the reserved prefix was checked above");
    }
    Ok(())
}

fn is_managed_ref_name(name: &[u8]) -> bool {
    parse_top_level_change_head(name).is_some()
        || name.starts_with(OWNED_BASE_PREFIX)
        || name.starts_with(VERSION_PREFIX)
}

fn is_version_tag_name(name: &[u8]) -> bool {
    name.starts_with(VERSION_PREFIX)
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

fn parse_version_tag_name(name: &[u8]) -> Result<Option<(GherritPrId, Version)>> {
    let Some(suffix) = name.strip_prefix(VERSION_PREFIX) else {
        return Ok(None);
    };
    let mut components = suffix.split(|byte| *byte == b'/');
    let (Some(id), Some(version), None) = (components.next(), components.next(), components.next())
    else {
        bail!("remote version tag does not have the canonical change/vN shape");
    };
    let id = GherritPrId::from_ref_component(id)
        .wrap_err("remote version tag has an invalid change ID")?;
    let version = parse_version(version).wrap_err("remote version tag is not canonical")?;
    Ok(Some((id, version)))
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

struct ParsedVersionTags {
    histories: HashMap<GherritPrId, BTreeMap<Version, AdvertisedVersionRef>>,
}

impl fmt::Debug for ParsedVersionTags {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ParsedVersionTags")
            .field("covered_change_count", &self.histories.len())
            .field(
                "advertised_ref_count",
                &self.histories.values().map(BTreeMap::len).sum::<usize>(),
            )
            .finish()
    }
}

fn parse_versions<'a>(
    output: &[u8],
    ids: impl IntoIterator<Item = &'a GherritPrId>,
) -> Result<ParsedVersionTags> {
    let requested = requested_names(ids, |id| id.as_str().to_owned())?;
    let mut histories =
        requested.values().cloned().map(|id| (id, BTreeMap::new())).collect::<HashMap<_, _>>();

    for record in records(output) {
        let record = record?;
        if record.name == VERSION_ROOT {
            bail!("remote ref uses the version-tag namespace root");
        }
        let Some(suffix) = record.name.strip_prefix(VERSION_PREFIX) else {
            continue;
        };
        let Some(separator) = suffix.iter().position(|byte| *byte == b'/') else {
            if let Some(id) = requested.get(suffix) {
                bail!("remote version namespace root exists for GHerrit change '{}'", id.as_str());
            }
            GherritPrId::from_ref_component(suffix)
                .wrap_err("remote version namespace root has an invalid change ID")?;
            bail!("remote advertised a version namespace for an unrequested GHerrit change");
        };
        let (id_component, suffix) = suffix.split_at(separator);
        GherritPrId::from_ref_component(id_component)
            .wrap_err("remote version tag has an invalid change ID")?;
        let id = requested.get(id_component).ok_or_else(|| {
            eyre!("remote advertised version history for an unrequested GHerrit change")
        })?;
        let suffix = suffix.strip_prefix(b"/").ok_or_else(|| {
            eyre!("remote version tag for GHerrit change '{}' has no version", id.as_str())
        })?;
        if record.peeled {
            bail!(
                "remote version tag for GHerrit change '{}' is annotated rather than lightweight",
                id.as_str()
            );
        }
        let version = parse_version(suffix).wrap_err_with(|| {
            format!("remote version tag for GHerrit change '{}' is not canonical", id.as_str())
        })?;
        let source_ref = str::from_utf8(record.name)
            .expect("a validated managed version ref is ASCII")
            .to_owned();
        let advertised = AdvertisedVersionRef { object_id: record.object_id, source_ref };
        if histories
            .get_mut(id)
            .expect("requested changes have initialized histories")
            .insert(version, advertised)
            .is_some()
        {
            bail!(
                "remote advertised version v{version} for GHerrit change '{}' more than once",
                id.as_str()
            );
        }
    }
    Ok(ParsedVersionTags { histories })
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
        path::Path,
        process::Output,
    };

    use super::*;

    const MAIN: &str = "1111111111111111111111111111111111111111";
    const ONE: &str = "2222222222222222222222222222222222222222";
    const TWO: &str = "3333333333333333333333333333333333333333";
    const SHA256: &str = "4444444444444444444444444444444444444444444444444444444444444444";

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

    #[cfg(unix)]
    fn shell(script: &str) -> Command {
        let mut command = Command::new("/bin/sh");
        command.env_clear().arg("-c").arg(script);
        command
    }

    #[cfg(unix)]
    fn shell_output(stdout: &str, status: i32) -> Command {
        let mut command = shell("printf '%s' \"$1\"; exit \"$2\"");
        command.arg("gherrit-test").arg(stdout).arg(status.to_string());
        command
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
    }

    #[cfg(unix)]
    #[tokio::test(flavor = "current_thread")]
    async fn async_head_observation_accepts_success_and_rejects_nonzero_status() {
        let advertisement = String::from_utf8(head_advertisement("")).unwrap();
        let heads = observe_remote_heads_command(
            shell_output(&advertisement, 0),
            "origin",
            Duration::from_secs(5),
        )
        .await
        .unwrap();
        assert_eq!(heads.default_branch().name(), "main");

        let error = observe_remote_heads_command(
            shell("printf private-destination >&2; exit 23"),
            "origin",
            Duration::from_secs(5),
        )
        .await
        .unwrap_err();
        let diagnostic = format!("{error:?}");
        assert!(diagnostic.contains("`git ls-remote` failed"), "{diagnostic}");
        assert!(!diagnostic.contains("private-destination"), "{diagnostic}");
    }

    #[cfg(unix)]
    #[tokio::test(flavor = "current_thread")]
    async fn async_head_observation_has_a_finite_execution_deadline() {
        let error = observe_remote_heads_command(
            shell("while :; do :; done"),
            "origin",
            Duration::from_millis(25),
        )
        .await
        .unwrap_err();

        assert!(format!("{error:?}").contains("timed out"), "error={error:?}");
    }

    #[cfg(unix)]
    #[tokio::test(flavor = "current_thread")]
    async fn async_version_observation_accepts_success_and_rejects_nonzero_status() {
        let requested = ids(&["Gone"]);
        let query = Query::new(requested[0].clone());
        let advertisement = format!("{ONE}\trefs/tags/gherrit/Gone/v1\n");
        let output = observe_active_version_query(
            shell_output(&advertisement, 0),
            "origin",
            Duration::from_secs(5),
        )
        .await
        .unwrap();
        let observed = parse_versions(&output.stdout, query.ids()).unwrap();
        assert_eq!(for_id(&observed.histories, "Gone").len(), 1);

        let error = observe_active_version_query(
            shell("printf private-ref >&2; exit 29"),
            "origin",
            Duration::from_secs(5),
        )
        .await
        .unwrap_err();
        let diagnostic = format!("{error:?}");
        assert!(diagnostic.contains("`git ls-remote` failed"), "{diagnostic}");
        assert!(!diagnostic.contains("private-ref"), "{diagnostic}");
    }

    #[cfg(unix)]
    #[tokio::test(flavor = "current_thread")]
    async fn async_version_observation_has_a_finite_execution_deadline() {
        let error = observe_active_version_query(
            shell("while :; do :; done"),
            "origin",
            Duration::from_millis(25),
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
        let heads = observe_remote_heads(&push_destination).await.unwrap();
        assert_eq!(heads.default_branch().name(), "main");
        assert_eq!(heads.candidate_head(&id("main")).unwrap().to_string(), push_oid);

        let requested = id("Gone");
        let push_versions =
            observe_active_version_tags(&push_destination, [&requested]).await.unwrap();
        let fetch_versions =
            observe_active_version_tags(&fetch_destination, [&requested]).await.unwrap();
        let push_history = push_versions.history(&requested).unwrap().collect::<Vec<_>>();
        let fetch_history = fetch_versions.history(&requested).unwrap().collect::<Vec<_>>();
        assert_eq!(push_history.len(), 1);
        assert_eq!(push_history[0].1.to_string(), push_oid);
        assert_eq!(fetch_history.len(), 1);
        assert_eq!(fetch_history[0].1.to_string(), fetch_oid);

        let push_object = ObjectId::from_hex(push_oid.as_bytes()).unwrap();
        let unknown_object = ObjectId::from_hex(MAIN.as_bytes()).unwrap();
        let error = push_versions
            .acquisition(std::iter::empty())
            .err()
            .expect("an empty acquisition must be rejected");
        assert!(error.to_string().contains("at least one"), "error={error:?}");
        let error = push_versions
            .acquisition([unknown_object])
            .err()
            .expect("an unknown object must be rejected");
        assert!(error.to_string().contains("absent"), "error={error:?}");
        let error = push_versions
            .acquisition([push_object, push_object])
            .err()
            .expect("a duplicate object must be rejected");
        assert!(error.to_string().contains("same advertised object"), "error={error:?}");
        let acquisition = push_versions.acquisition([push_object]).unwrap();

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
        let error = parse_versions(malformed_version.as_bytes(), &requested).unwrap_err();
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
            let error = parse_versions(output.as_bytes(), &requested).unwrap_err();
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
        for prefix in [OWNED_BASE_PREFIX, VERSION_PREFIX] {
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
    fn rejects_symbolic_version_tags_and_reserved_namespace_roots() {
        for (name, expected) in [
            ("refs/tags/gherrit/Gone/v1", "symbolic rather than direct"),
            ("refs/heads/gherrit-bases", "reserved owned-base namespace root"),
            ("refs/tags/gherrit", "reserved version-tag namespace root"),
        ] {
            let output = head_advertisement(&format!("ref: refs/heads/unrelated\t{name}\n"));
            let error = parse_remote_heads(&output).unwrap_err();
            assert!(error.to_string().contains(expected), "name={name}: {error:?}");
        }
    }

    #[test]
    fn version_history_is_ordered_complete_and_may_repeat_objects() {
        let output =
            format!("{ONE}\trefs/tags/gherrit/Gone/v2\n{ONE}\trefs/tags/gherrit/Gone/v1\n");
        let requested = ids(&["Gone", "Gmissing"]);
        let observation = parse_versions(output.as_bytes(), &requested).unwrap();

        assert_eq!(
            for_id(&observation.histories, "Gone")
                .iter()
                .map(|(version, advertised)| { (version.get(), advertised.object_id.to_string()) })
                .collect::<Vec<_>>(),
            [(1, ONE.to_owned()), (2, ONE.to_owned())]
        );
        assert!(for_id(&observation.histories, "Gmissing").is_empty());
        let sources = for_id(&observation.histories, "Gone").values().collect::<Vec<_>>();
        assert_eq!(sources[0].object_id.to_string(), ONE);
        assert_eq!(sources[0].source_ref, "refs/tags/gherrit/Gone/v1");
        assert_eq!(sources[1].object_id.to_string(), ONE);
        assert_eq!(sources[1].source_ref, "refs/tags/gherrit/Gone/v2");
    }

    #[test]
    fn rejects_malformed_duplicate_annotated_and_noncanonical_managed_refs() {
        let requested = ids(&["Gone"]);
        for output in [
            format!("{ONE}\trefs/tags/gherrit/Gone\n"),
            format!("{ONE}\trefs/tags/gherrit/Gone/v0\n"),
            format!("{ONE}\trefs/tags/gherrit/Gone/v01\n"),
            format!("{ONE}\trefs/tags/gherrit/Gone/v1/extra\n"),
            format!("{ONE}\trefs/tags/gherrit/Gone/v18446744073709551616\n"),
            format!("{ONE}\trefs/tags/gherrit/Gone/v1\n{TWO}\trefs/tags/gherrit/Gone/v1\n"),
            format!("{ONE}\trefs/tags/gherrit/Gone/v1\n{TWO}\trefs/tags/gherrit/Gone/v1^{{}}\n"),
        ] {
            assert!(parse_versions(output.as_bytes(), &requested).is_err(), "{output:?}");
        }
    }

    #[test]
    fn active_query_planning_uses_exact_patterns_and_preflights_every_id() {
        let requested = ids(&["Gone", "Gtwo"]);
        let refs = requested.iter().collect::<Vec<_>>();
        let one = version_pattern_bytes(&requested[0]);
        let two = version_pattern_bytes(&requested[1]);
        let split = plan_queries_with_budget(&refs, version_pattern_bytes, one + two - 1).unwrap();

        assert_eq!(split.len(), 2);
        assert_eq!(
            split[0].version_patterns(),
            ["refs/tags/gherrit/Gone", "refs/tags/gherrit/Gone/*"]
        );
        assert_eq!(
            split[1].version_patterns(),
            ["refs/tags/gherrit/Gtwo", "refs/tags/gherrit/Gtwo/*"]
        );
        assert!(plan_queries_with_budget(&refs, version_pattern_bytes, one - 1).is_err());
        assert!(plan_queries_with_budget(&[], version_pattern_bytes, one).unwrap().is_empty());
        assert!(
            plan_queries_with_budget(
                &[&requested[0], &requested[0]],
                version_pattern_bytes,
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
        let output =
            format!("{ONE}\trefs/tags/gherrit/Gone/v2\n{ONE}\trefs/tags/gherrit/Gone/v1\n");
        let ParsedVersionTags { histories } =
            parse_versions(output.as_bytes(), &requested).unwrap();
        let destination =
            PushDestination::for_test("origin", "https://github.com/owner/repo.git", Vec::new())
                .unwrap();
        let observation = ActiveVersionTags { destination: &destination, histories };

        let acquisition =
            observation.acquisition([ObjectId::from_hex(ONE.as_bytes()).unwrap()]).unwrap();

        assert_eq!(acquisition.object_count, 1);
        assert_eq!(acquisition.source_ref_count, 2);
        assert_eq!(acquisition.batches.len(), 1);
        assert_eq!(
            acquisition.batches[0].source_refs().collect::<Vec<_>>(),
            ["refs/tags/gherrit/Gone/v1", "refs/tags/gherrit/Gone/v2"]
        );
    }

    #[test]
    fn coupling_rejects_missing_active_version_coverage() {
        let change = id("Gone");
        let stack = LocalStack::for_test(
            ObjectId::from_hex(MAIN.as_bytes()).unwrap(),
            [(change, ObjectId::from_hex(ONE.as_bytes()).unwrap())],
        );
        let heads = parse_remote_heads(&head_advertisement("")).unwrap();
        let destination =
            PushDestination::for_test("origin", "https://github.com/owner/repo.git", Vec::new())
                .unwrap();
        let versions = ActiveVersionTags { destination: &destination, histories: HashMap::new() };

        let error = ObservedStack::couple(&stack, &heads, versions).unwrap_err();
        assert!(error.to_string().contains("was not observed"), "error={error:?}");
    }

    #[test]
    fn coupling_rejects_extra_active_version_coverage() {
        let change = id("Gone");
        let stack = LocalStack::for_test(
            ObjectId::from_hex(MAIN.as_bytes()).unwrap(),
            [(change.clone(), ObjectId::from_hex(ONE.as_bytes()).unwrap())],
        );
        let heads = parse_remote_heads(&head_advertisement("")).unwrap();
        let destination =
            PushDestination::for_test("origin", "https://github.com/owner/repo.git", Vec::new())
                .unwrap();
        let versions = ActiveVersionTags {
            destination: &destination,
            histories: HashMap::from([(change, BTreeMap::new()), (id("Gextra"), BTreeMap::new())]),
        };

        let error = ObservedStack::couple(&stack, &heads, versions).unwrap_err();
        assert!(error.to_string().contains("outside the local stack"), "error={error:?}");
    }
}

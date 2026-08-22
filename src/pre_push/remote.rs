//! Byte-oriented observations of the push repository's advertised refs.
//!
//! Repository-wide heads and exact active immutable histories have different
//! completeness domains. A missing `RemoteHeads` entry proves absence because
//! every head was advertised. A missing `ActiveVersionTags` entry is an error
//! because only explicitly requested histories are complete.

use std::{
    collections::{BTreeMap, HashMap, HashSet, hash_map::Entry},
    str,
    time::Instant,
};

use color_eyre::eyre::{Context as _, Result, bail, eyre};
use gix::{ObjectId, bstr::ByteSlice as _};

use super::{
    destination::{DefaultBranch, PushDestination, git_output_records},
    local::{GherritPrId, LocalChange, LocalStack},
    version::Version,
};

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

/// Complete immutable histories for exactly the explicitly requested IDs.
#[derive(Debug)]
pub(super) struct ActiveVersionTags {
    histories: HashMap<GherritPrId, BTreeMap<Version, ObjectId>>,
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
        mut versions: ActiveVersionTags,
    ) -> Result<Self> {
        let changes = stack
            .iter()
            .map(|change| {
                let history = versions.histories.remove(change.id()).ok_or_else(|| {
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
pub(super) fn observe_remote_heads(destination: &PushDestination) -> Result<RemoteHeads> {
    let started = Instant::now();
    let output = destination
        .ls_remote(
            ["--quiet".to_owned(), "--symref".to_owned()],
            HEAD_ADVERTISEMENT_PATTERNS.map(str::to_owned),
        )
        .output()
        .wrap_err_with(|| {
            format!("Failed to observe GHerrit remote '{}'", destination.configured_remote())
        })?;
    if !output.status.success() {
        bail!("`git ls-remote` failed for GHerrit remote '{}'", destination.configured_remote());
    }
    let record_count = git_output_records(&output.stdout).count();
    log::trace!(
        "Observed GHerrit remote heads ({} bytes, {} records) in {:?}",
        output.stdout.len(),
        record_count,
        started.elapsed()
    );
    parse_remote_heads(&output.stdout).wrap_err_with(|| {
        format!(
            "GHerrit remote '{}' reported an invalid head advertisement",
            destination.configured_remote()
        )
    })
}

/// Observes exact immutable histories only for explicitly requested IDs.
pub(super) fn observe_active_version_tags<'a>(
    destination: &PushDestination,
    ids: impl IntoIterator<Item = &'a GherritPrId>,
) -> Result<ActiveVersionTags> {
    let ids = ids.into_iter().collect::<Vec<_>>();
    let version_queries = plan_queries(&ids, version_pattern_bytes)?;
    let started = Instant::now();
    let mut total_bytes = 0;
    let mut total_records = 0;
    let mut histories = HashMap::new();
    for query in &version_queries {
        let output = destination
            .ls_remote(["--quiet".to_owned()], query.version_patterns())
            .output()
            .wrap_err_with(|| {
                format!(
                    "Failed to observe active version history at GHerrit remote '{}'",
                    destination.configured_remote()
                )
            })?;
        if !output.status.success() {
            bail!(
                "`git ls-remote` failed while observing active version history at GHerrit remote '{}'",
                destination.configured_remote()
            );
        }
        total_bytes += output.stdout.len();
        total_records += git_output_records(&output.stdout).count();
        for (id, versions) in parse_versions(&output.stdout, query.ids())? {
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
    Ok(ActiveVersionTags { histories })
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

fn parse_versions<'a>(
    output: &[u8],
    ids: impl IntoIterator<Item = &'a GherritPrId>,
) -> Result<HashMap<GherritPrId, BTreeMap<Version, ObjectId>>> {
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
        if histories
            .get_mut(id)
            .expect("requested changes have initialized histories")
            .insert(version, record.object_id)
            .is_some()
        {
            bail!(
                "remote advertised version v{version} for GHerrit change '{}' more than once",
                id.as_str()
            );
        }
    }
    Ok(histories)
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
        let histories = parse_versions(output.as_bytes(), &requested).unwrap();

        assert_eq!(
            for_id(&histories, "Gone")
                .iter()
                .map(|(version, object)| (version.get(), object.to_string()))
                .collect::<Vec<_>>(),
            [(1, ONE.to_owned()), (2, ONE.to_owned())]
        );
        assert!(for_id(&histories, "Gmissing").is_empty());
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
    fn coupling_rejects_missing_active_version_coverage() {
        let change = id("Gone");
        let stack = LocalStack::for_test(
            ObjectId::from_hex(MAIN.as_bytes()).unwrap(),
            [(change, ObjectId::from_hex(ONE.as_bytes()).unwrap())],
        );
        let heads = parse_remote_heads(&head_advertisement("")).unwrap();
        let versions = ActiveVersionTags { histories: HashMap::new() };

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
        let versions = ActiveVersionTags {
            histories: HashMap::from([(change, BTreeMap::new()), (id("Gextra"), BTreeMap::new())]),
        };

        let error = ObservedStack::couple(&stack, &heads, versions).unwrap_err();
        assert!(error.to_string().contains("outside the local stack"), "error={error:?}");
    }
}

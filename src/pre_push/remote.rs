//! Byte-oriented observations of the push repository's advertised refs.
//!
//! `git ls-remote` is a protocol boundary. Its output is not trusted merely
//! because Git produced it: a server can advertise malformed, duplicate, or
//! contradictory records. This module validates the wire representation and
//! preserves logically contradictory active state for the publication layer
//! to interpret. Repository-wide heads and active immutable histories are read
//! separately so work does not grow with inactive history.

use std::{
    collections::{BTreeMap, HashMap, hash_map::Entry},
    str,
    time::Instant,
};

use color_eyre::eyre::{Context as _, Result, bail, eyre};
use gix::{ObjectId, bstr::ByteSlice as _};

use super::{
    destination::{DefaultBranch, PushDestination, git_output_records},
    local::ChangeId,
    version::Version,
};

const HEAD_ADVERTISEMENT_PATTERNS: [&str; 3] = ["HEAD", "refs/heads/*", "refs/tags/gherrit"];
// The fixed private-remote command occupies well under 1 KiB. Limiting the
// ASCII ref-pattern portion to 16 KiB, including one separator byte per
// argument, leaves conservative headroom beneath Windows' roughly 32-KiB
// command-line limit while also bounding POSIX argv encoding.
const ACTIVE_VERSION_PATTERN_ARGV_BUDGET_BYTES: usize = 16 * 1024;
const OWNED_BASE_ROOT: &[u8] = b"refs/heads/gherrit-bases";
const OWNED_BASE_PREFIX: &[u8] = b"refs/heads/gherrit-bases/";
const HEAD_PREFIX: &[u8] = b"refs/heads/";
const VERSION_TAG_ROOT: &[u8] = b"refs/tags/gherrit";
const VERSION_TAG_PREFIX: &[u8] = b"refs/tags/gherrit/";

/// Syntactically valid head state from the repository-wide observation.
#[derive(Debug)]
pub(super) struct RemoteHeads {
    default_branch: DefaultBranch,
    managed_heads: HashMap<ChangeId, ObjectId>,
    owned_bases: HashMap<ChangeId, ObjectId>,
}

impl RemoteHeads {
    #[cfg(test)]
    pub(super) fn for_test(
        default_branch: DefaultBranch,
        managed_heads: HashMap<ChangeId, ObjectId>,
        owned_bases: HashMap<ChangeId, ObjectId>,
    ) -> Self {
        Self { default_branch, managed_heads, owned_bases }
    }

    pub(super) fn default_branch(&self) -> &DefaultBranch {
        &self.default_branch
    }

    pub(super) fn managed_head(&self, id: &ChangeId) -> Option<ObjectId> {
        self.managed_heads.get(id).copied()
    }

    pub(super) fn owned_base(&self, id: &ChangeId) -> Option<ObjectId> {
        self.owned_bases.get(id).copied()
    }
}

/// Immutable version history for exactly the active local changes.
#[derive(Debug)]
pub(super) struct ActiveVersionTags {
    version_tags: HashMap<ChangeId, BTreeMap<Version, ObjectId>>,
}

impl ActiveVersionTags {
    #[cfg(test)]
    pub(super) fn for_test(version_tags: HashMap<ChangeId, BTreeMap<Version, ObjectId>>) -> Self {
        Self { version_tags }
    }

    pub(super) fn observed_tags_for(&self, id: &ChangeId) -> Result<&BTreeMap<Version, ObjectId>> {
        self.version_tags.get(id).ok_or_else(|| {
            eyre!("active version history for GHerrit change '{}' was not observed", id.as_str())
        })
    }
}

/// Observes the default branch and every potentially managed head in one
/// constant-argument network request. The literal tag root detects a
/// directory/file conflict without requesting any immutable history.
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

/// Observes immutable history only for active local changes.
///
/// Every query is planned before the first request. This prevents an
/// individually oversized ID late in the stack from failing after earlier
/// batches have already been observed.
pub(super) fn observe_active_version_tags<'a>(
    destination: &PushDestination,
    ids: impl IntoIterator<Item = &'a ChangeId>,
) -> Result<ActiveVersionTags> {
    let queries = plan_active_version_queries(ids)?;
    let query_count = queries.len();
    let active_change_count = queries.iter().map(ActiveVersionQuery::len).sum::<usize>();
    let started = Instant::now();
    let mut total_bytes = 0;
    let mut total_records = 0;
    let mut version_tags = HashMap::new();

    for query in queries {
        let patterns = query.patterns();
        let output = destination
            .ls_remote(["--quiet".to_owned()], patterns)
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
        let observed =
            parse_active_version_tags(&output.stdout, query.ids()).wrap_err_with(|| {
                format!(
                    "GHerrit remote '{}' reported invalid active version history",
                    destination.configured_remote()
                )
            })?;
        for (id, versions) in observed {
            if version_tags.insert(id, versions).is_some() {
                bail!("active version history was returned by more than one query");
            }
        }
    }

    log::trace!(
        "Observed GHerrit version history for {active_change_count} active change(s) \
         in {query_count} request(s) \
         ({total_bytes} bytes, {total_records} records) in {:?}",
        started.elapsed()
    );
    Ok(ActiveVersionTags { version_tags })
}

#[derive(Debug, Eq, PartialEq)]
struct ActiveVersionQuery {
    first: ChangeId,
    rest: Vec<ChangeId>,
}

impl ActiveVersionQuery {
    fn new(first: ChangeId) -> Self {
        Self { first, rest: Vec::new() }
    }

    fn ids(&self) -> impl Iterator<Item = &ChangeId> {
        std::iter::once(&self.first).chain(&self.rest)
    }

    fn len(&self) -> usize {
        1 + self.rest.len()
    }

    fn encoded_pattern_bytes(&self) -> usize {
        self.ids().map(active_version_pattern_bytes).sum()
    }

    fn patterns(&self) -> Vec<String> {
        self.ids().flat_map(active_version_patterns).collect()
    }

    fn push(&mut self, id: ChangeId) {
        self.rest.push(id);
    }
}

fn active_version_patterns(id: &ChangeId) -> [String; 2] {
    let root = format!("refs/tags/gherrit/{}", id.as_str());
    [root.clone(), format!("{root}/*")]
}

fn active_version_pattern_bytes(id: &ChangeId) -> usize {
    active_version_patterns(id).iter().map(|pattern| pattern.len() + 1).sum()
}

fn plan_active_version_queries<'a>(
    ids: impl IntoIterator<Item = &'a ChangeId>,
) -> Result<Vec<ActiveVersionQuery>> {
    plan_active_version_queries_with_budget(ids, ACTIVE_VERSION_PATTERN_ARGV_BUDGET_BYTES)
}

fn plan_active_version_queries_with_budget<'a>(
    ids: impl IntoIterator<Item = &'a ChangeId>,
    budget: usize,
) -> Result<Vec<ActiveVersionQuery>> {
    let planned = ids
        .into_iter()
        .map(|id| {
            let encoded_bytes = active_version_pattern_bytes(id);
            if encoded_bytes > budget {
                bail!(
                    "GHerrit change ID is too long to observe its remote version history ({} bytes)",
                    id.as_str().len()
                );
            }
            Ok((id.clone(), encoded_bytes))
        })
        .collect::<Result<Vec<_>>>()?;

    let mut queries = Vec::new();
    let mut current = None::<ActiveVersionQuery>;
    for (id, encoded_bytes) in planned {
        if current
            .as_ref()
            .is_some_and(|query| query.encoded_pattern_bytes() > budget - encoded_bytes)
        {
            queries.push(current.take().expect("a full active-version query exists"));
        }
        match &mut current {
            Some(query) => query.push(id),
            None => current = Some(ActiveVersionQuery::new(id)),
        }
    }
    if let Some(current) = current {
        queries.push(current);
    }
    Ok(queries)
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

/// Strictly parses every record before callers select their relevant refs.
fn parse_advertised_refs(output: &[u8]) -> Result<AdvertisedRefs> {
    let mut object_format = None;
    let mut direct = HashMap::<Vec<u8>, Vec<u8>>::new();
    let mut symbolic = HashMap::<Vec<u8>, Vec<u8>>::new();

    for record in git_output_records(output) {
        let mut fields = record.split(|byte| *byte == b'\t');
        let (Some(value), Some(name), None) = (fields.next(), fields.next(), fields.next()) else {
            bail!("malformed `git ls-remote` record: {record:?}");
        };
        if let Some(target) = value.strip_prefix(b"ref: ") {
            validate_advertised_ref_name(name)?;
            validate_reserved_ref_name(name)?;
            gix::refs::FullName::try_from(target.as_bstr())
                .wrap_err("symbolic remote ref has an invalid target")?;
            // The default branch anchors local stack derivation and must be
            // stable independently of per-change publication. An owned base
            // is mutable change state, so allowing HEAD to target one would
            // let publication force-update the repository default branch.
            if name == b"HEAD" && is_owned_base_ref_name(target) {
                bail!("symbolic HEAD targets GHerrit's reserved owned-base namespace");
            }
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

    let mut managed_heads = HashMap::new();
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
            managed_heads.insert(id, object_id);
        }
    }

    Ok(RemoteHeads { default_branch, managed_heads, owned_bases })
}

fn parse_active_version_tags<'a>(
    output: &[u8],
    requested_ids: impl IntoIterator<Item = &'a ChangeId>,
) -> Result<HashMap<ChangeId, BTreeMap<Version, ObjectId>>> {
    let AdvertisedRefs { direct, symbolic } = parse_advertised_refs(output)?;
    if !symbolic.is_empty() {
        bail!("active version history unexpectedly contained a symbolic ref");
    }
    let mut version_tags = HashMap::<ChangeId, BTreeMap<Version, ObjectId>>::new();
    for id in requested_ids {
        match version_tags.entry(id.clone()) {
            Entry::Vacant(entry) => {
                entry.insert(BTreeMap::new());
            }
            Entry::Occupied(_) => bail!("active version history requested the same change twice"),
        }
    }
    for (name, object_id) in direct {
        if name.ends_with(b"^{}") {
            continue;
        }
        let Some((id, version)) = parse_version_tag_name(&name)? else {
            continue;
        };
        let tags = version_tags.get_mut(&id).ok_or_else(|| {
            eyre!("remote advertised version history for an unrequested GHerrit change")
        })?;
        if tags.insert(version, object_id).is_some() {
            bail!("remote advertised a version tag more than once");
        }
    }
    Ok(version_tags)
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
    if name == VERSION_TAG_ROOT {
        bail!("remote ref uses the reserved version-tag namespace root");
    }
    if name.starts_with(VERSION_TAG_PREFIX) {
        parse_version_tag_name(name)?.expect("the reserved prefix was checked above");
    }
    Ok(())
}

fn is_managed_ref_name(name: &[u8]) -> bool {
    parse_top_level_change_head(name).is_some()
        || name.starts_with(OWNED_BASE_PREFIX)
        || name.starts_with(VERSION_TAG_PREFIX)
}

fn is_owned_base_ref_name(name: &[u8]) -> bool {
    name == OWNED_BASE_ROOT || name.starts_with(OWNED_BASE_PREFIX)
}

fn is_version_tag_name(name: &[u8]) -> bool {
    name.starts_with(VERSION_TAG_PREFIX)
}

fn parse_top_level_change_head(name: &[u8]) -> Option<ChangeId> {
    let id = name.strip_prefix(HEAD_PREFIX)?;
    (!id.contains(&b'/')).then(|| ChangeId::from_ref_component(id).ok()).flatten()
}

fn parse_owned_base_name(name: &[u8]) -> Result<Option<ChangeId>> {
    let Some(id) = name.strip_prefix(OWNED_BASE_PREFIX) else {
        return Ok(None);
    };
    let id = ChangeId::from_ref_component(id)
        .wrap_err("remote owned-base ref has an invalid change ID")?;
    Ok(Some(id))
}

fn parse_version_tag_name(name: &[u8]) -> Result<Option<(ChangeId, Version)>> {
    let Some(suffix) = name.strip_prefix(VERSION_TAG_PREFIX) else {
        return Ok(None);
    };
    let mut components = suffix.split(|byte| *byte == b'/');
    let (Some(id), Some(version), None) = (components.next(), components.next(), components.next())
    else {
        bail!("remote version tag does not have the canonical change/vN shape");
    };
    let id =
        ChangeId::from_ref_component(id).wrap_err("remote version tag has an invalid change ID")?;
    let digits = version
        .strip_prefix(b"v")
        .ok_or_else(|| eyre!("remote version tag does not use a vN version"))?;
    if digits.is_empty() || !digits.iter().all(u8::is_ascii_digit) {
        bail!("remote version tag has a non-decimal version");
    }
    if digits[0] == b'0' {
        bail!("remote version tag has a zero or non-canonical version");
    }
    let version = digits.iter().try_fold(0_u64, |value, digit| {
        value
            .checked_mul(10)
            .and_then(|value| value.checked_add(u64::from(*digit - b'0')))
            .ok_or_else(|| eyre!("remote version tag number overflows u64"))
    })?;
    let version = Version::new(version)
        .ok_or_else(|| eyre!("remote version tag has a zero or non-canonical version"))?;
    Ok(Some((id, version)))
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

    fn change_id(value: &str) -> ChangeId {
        ChangeId::from_ref_component(value.as_bytes()).expect("valid test change ID")
    }

    fn change_ids(values: &[&str]) -> Vec<ChangeId> {
        values.iter().map(|value| change_id(value)).collect()
    }

    #[test]
    fn parses_arbitrary_head_order_and_valid_tail_matches() {
        let mut output = format!(
            "{MAIN}\trefs/heads/main\n\
             {ONE}\trefs/heads/Gone\n\
             ref: refs/remotes/origin/main\trefs/remotes/origin/HEAD\n\
             {MAIN}\trefs/remotes/origin/HEAD\n\
             ref: refs/heads/main\tHEAD\n\
             {TWO}\trefs/heads/gherrit-bases/Gone\n\
             {MAIN}\tHEAD\n"
        )
        .into_bytes();
        output.extend_from_slice(format!("{TWO}\trefs/heads/archive/").as_bytes());
        output.extend_from_slice(b"\xff\n");

        let observed = parse_remote_heads(&output).unwrap();

        assert_eq!(observed.default_branch.name(), "main");
        assert_eq!(observed.managed_head(&change_id("Gone")).unwrap().to_string(), ONE);
        assert_eq!(observed.owned_base(&change_id("Gone")).unwrap().to_string(), TWO);
    }

    #[test]
    fn active_history_retains_repeated_objects_and_ignores_valid_tail_matches() {
        let mut output = format!(
            "{ONE}\trefs/tags/gherrit/Gone/v2\n\
             {ONE}\trefs/tags/gherrit/Gone/v1\n\
             {TWO}\trefs/tags/unrelated\n\
             {TWO}\trefs/tags/unrelated^{{}}\n"
        )
        .into_bytes();
        output.extend_from_slice(format!("{TWO}\trefs/tags/archive/").as_bytes());
        output.extend_from_slice(b"\xff\n");

        let ids = change_ids(&["Gone", "Gtwo"]);
        let observed = parse_active_version_tags(&output, &ids).unwrap();
        assert_eq!(
            observed
                .get("Gone")
                .unwrap()
                .iter()
                .map(|(version, object)| (version.get(), object.to_string()))
                .collect::<Vec<_>>(),
            [(1, ONE.to_owned()), (2, ONE.to_owned())]
        );
        assert!(
            observed.get("Gtwo").expect("queried tagless change remains in the domain").is_empty()
        );
    }

    #[test]
    fn active_history_rejects_duplicate_requested_ids() {
        let ids = change_ids(&["Gone", "Gone"]);
        let error = parse_active_version_tags(&[], &ids).unwrap_err();
        assert!(error.to_string().contains("same change twice"), "error={error:?}");
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
    fn head_cannot_target_the_owned_base_namespace() {
        for target in [
            "refs/heads/gherrit-bases",
            "refs/heads/gherrit-bases/Gone",
            "refs/heads/gherrit-bases/nested/name",
        ] {
            let output = format!("ref: {target}\tHEAD\n{MAIN}\tHEAD\n{MAIN}\t{target}\n");
            let error = parse_remote_heads(output.as_bytes()).unwrap_err();
            assert!(error.to_string().contains("owned-base namespace"), "error={error:?}");
        }
    }

    #[test]
    fn accepts_crlf_and_an_optional_final_line_feed() {
        for output in [
            format!("ref: refs/heads/main\tHEAD\r\n{MAIN}\tHEAD\r\n{MAIN}\trefs/heads/main\r\n"),
            format!("ref: refs/heads/main\tHEAD\n{MAIN}\tHEAD\n{MAIN}\trefs/heads/main"),
        ] {
            assert_eq!(
                parse_remote_heads(output.as_bytes()).unwrap().default_branch.name(),
                "main"
            );
        }
    }

    #[test]
    fn ignores_tail_matches_but_validates_every_record() {
        let mut output = head_advertisement(&format!(
            "{ONE}\trefs/heads/HEAD\n\
             {ONE}\trefs/tags/HEAD\n\
             {ONE}\trefs/tags/HEAD^{{}}\n"
        ));
        output.extend_from_slice(format!("{ONE}\trefs/heads/archive/").as_bytes());
        output.extend_from_slice(b"\xff-HEAD\n");
        assert!(parse_remote_heads(&output).is_ok());

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
    fn rejects_duplicate_and_contradictory_records() {
        for records in [
            format!("{ONE}\trefs/heads/Gone\n{ONE}\trefs/heads/Gone\n"),
            "ref: refs/heads/main\tHEAD\n".to_owned(),
            format!("ref: refs/heads/other\trefs/heads/Gone\n{ONE}\trefs/heads/Gone\n"),
            "ref: refs/heads/one\trefs/remotes/origin/HEAD\n\
             ref: refs/heads/two\trefs/remotes/origin/HEAD\n"
                .to_owned(),
        ] {
            assert!(
                parse_remote_heads(&head_advertisement(&records)).is_err(),
                "records={records:?}"
            );
        }
    }

    #[test]
    fn rejects_unsupported_and_mixed_object_formats() {
        let sha256 =
            format!("ref: refs/heads/main\tHEAD\n{SHA256}\tHEAD\n{SHA256}\trefs/heads/main\n");
        let error = parse_remote_heads(sha256.as_bytes()).unwrap_err();
        assert!(error.to_string().contains("SHA-256"), "error={error:?}");

        let mixed =
            format!("ref: refs/heads/main\tHEAD\n{MAIN}\tHEAD\n{SHA256}\trefs/heads/main\n");
        let error = parse_remote_heads(mixed.as_bytes()).unwrap_err();
        assert!(error.to_string().contains("mixes SHA-1 and SHA-256"), "error={error:?}");
    }

    #[test]
    fn reserved_owned_base_names_have_one_exact_change_component() {
        assert!(
            parse_remote_heads(&head_advertisement(&format!(
                "{ONE}\trefs/heads/gherrit-bases/Gone\n"
            )))
            .is_ok()
        );

        for name in [
            "refs/heads/gherrit-bases",
            "refs/heads/gherrit-bases/",
            "refs/heads/gherrit-bases/G-one",
            "refs/heads/gherrit-bases/Gone/extra",
        ] {
            assert!(
                parse_remote_heads(&head_advertisement(&format!("{ONE}\t{name}\n"))).is_err(),
                "name={name}"
            );
        }
    }

    #[test]
    fn rejects_non_utf8_names_inside_reserved_namespaces() {
        for (prefix, suffix) in
            [(OWNED_BASE_PREFIX, b"\xff".as_slice()), (VERSION_TAG_PREFIX, b"\xff/v1".as_slice())]
        {
            let mut output = head_advertisement("");
            output.extend_from_slice(format!("{ONE}\t").as_bytes());
            output.extend_from_slice(prefix);
            output.extend_from_slice(suffix);
            output.push(b'\n');
            assert!(parse_remote_heads(&output).is_err(), "prefix={prefix:?}");
        }
    }

    #[test]
    fn reserved_version_tags_have_one_id_and_positive_canonical_u64_version() {
        let gone = change_ids(&["Gone"]);
        assert!(
            parse_active_version_tags(
                format!("{ONE}\trefs/tags/gherrit/Gone/v18446744073709551615\n").as_bytes(),
                &gone,
            )
            .is_ok()
        );

        for name in [
            "refs/tags/gherrit",
            "refs/tags/gherrit/",
            "refs/tags/gherrit/Gone",
            "refs/tags/gherrit/Gone/v",
            "refs/tags/gherrit/Gone/v0",
            "refs/tags/gherrit/Gone/v01",
            "refs/tags/gherrit/Gone/v1/extra",
            "refs/tags/gherrit/G-one/v1",
            "refs/tags/gherrit/Gone/v18446744073709551616",
        ] {
            assert!(
                parse_active_version_tags(format!("{ONE}\t{name}\n").as_bytes(), &gone,).is_err(),
                "name={name}"
            );
        }
    }

    #[test]
    fn managed_version_tags_must_be_lightweight() {
        let gone = change_ids(&["Gone"]);
        let output = format!(
            "{ONE}\trefs/tags/gherrit/Gone/v1\n\
             {TWO}\trefs/tags/gherrit/Gone/v1^{{}}\n"
        );
        let error = parse_active_version_tags(output.as_bytes(), &gone).unwrap_err();
        assert!(error.to_string().contains("annotated"), "error={error:?}");
    }

    #[test]
    fn active_history_rejects_unrequested_managed_tags_and_symbolic_records() {
        let gone = change_ids(&["Gone"]);
        let unrequested = format!("{ONE}\trefs/tags/gherrit/Gother/v1\n");
        assert!(parse_active_version_tags(unrequested.as_bytes(), &gone).is_err());

        let symbolic = b"ref: refs/tags/unrelated\trefs/tags/archive\n";
        assert!(parse_active_version_tags(symbolic, &gone).is_err());
    }

    #[test]
    fn active_query_planning_uses_exact_patterns_and_encoded_boundaries() {
        let ids = change_ids(&["Gone", "Gtwo"]);
        let one = plan_active_version_queries_with_budget([&ids[0]], usize::MAX).unwrap();
        assert_eq!(one.len(), 1);
        assert_eq!(one[0].patterns(), ["refs/tags/gherrit/Gone", "refs/tags/gherrit/Gone/*"]);
        let one_cost = one[0].encoded_pattern_bytes();
        assert_eq!(plan_active_version_queries_with_budget([&ids[0]], one_cost).unwrap(), one);
        assert!(plan_active_version_queries_with_budget([&ids[0]], one_cost - 1).is_err());

        let two_cost = plan_active_version_queries_with_budget([&ids[1]], usize::MAX).unwrap()[0]
            .encoded_pattern_bytes();
        let split_budget = one_cost + two_cost - 1;
        let split = plan_active_version_queries_with_budget(&ids, split_budget).unwrap();
        assert_eq!(split.len(), 2);
        assert_eq!(split[0].ids().map(ChangeId::as_str).collect::<Vec<_>>(), ["Gone"]);
        assert_eq!(split[1].ids().map(ChangeId::as_str).collect::<Vec<_>>(), ["Gtwo"]);
        assert!(split.iter().all(|query| query.encoded_pattern_bytes() <= split_budget));
        assert!(
            plan_active_version_queries_with_budget(std::iter::empty::<&ChangeId>(), one_cost)
                .unwrap()
                .is_empty()
        );
    }
}

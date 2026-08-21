//! Complete destination-scoped Git publication observations.
//!
//! A missing ref is meaningful only when its exact namespace was queried.
//! `ObservedStack` can therefore be built only by querying every change in a
//! validated `LocalStack`. Publication planning consumes that coupled value
//! instead of independently supplied maps whose coverage could be incomplete.

use std::{
    collections::{BTreeMap, HashMap, HashSet, hash_map::Entry},
    str,
};

use color_eyre::eyre::{Context as _, Result, bail, eyre};
use gix::{ObjectId, bstr::ByteSlice as _};

use super::{
    destination::{PushDestination, git_output_records},
    local::{GherritPrId, LocalChange, LocalStack},
    version::Version,
};

// Variable arguments are kept well below Windows' roughly 32-KiB command-line
// limit. This also gives POSIX implementations a conservative bound.
const QUERY_ARGV_BUDGET_BYTES: usize = 16 * 1024;
const HEAD_PREFIX: &str = "refs/heads/";
const OWNED_BASE_PREFIX: &str = "refs/heads/gherrit-bases/";
const VERSION_PREFIX: &str = "refs/tags/gherrit/";

/// Remote head and immutable history for each local change, in stack order.
#[derive(Debug)]
pub(super) struct ObservedStack<'stack> {
    changes: Vec<ObservedChange<'stack>>,
}

impl<'stack> ObservedStack<'stack> {
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

/// Observes exactly the managed heads and version histories in `stack`.
///
/// Every query is planned before the first network request. Thus an oversized
/// ID late in the stack cannot fail after an earlier prefix was observed.
/// Ordinary stacks use one head query and one version query, in addition to
/// the separate default-branch query made before local stack collection.
pub(super) fn observe_publications<'stack>(
    destination: &PushDestination,
    stack: &'stack LocalStack,
) -> Result<ObservedStack<'stack>> {
    let ids = stack.iter().map(LocalChange::id).collect::<Vec<_>>();
    let head_queries = plan_queries(&ids, head_pattern_bytes)?;
    let version_queries = plan_queries(&ids, version_pattern_bytes)?;

    let mut heads = HashMap::new();
    for query in &head_queries {
        let output = destination
            .ls_remote(["--quiet".to_owned()], query.head_patterns())
            .output()
            .wrap_err_with(|| {
                format!(
                    "Failed to observe managed heads at GHerrit remote '{}'",
                    destination.configured_remote()
                )
            })?;
        if !output.status.success() {
            bail!(
                "`git ls-remote` failed while observing managed heads at GHerrit remote '{}'",
                destination.configured_remote()
            );
        }
        for (id, observed) in parse_heads(&output.stdout, query.ids())? {
            if heads.insert(id, observed).is_some() {
                bail!("managed refs were returned by more than one query");
            }
        }
    }

    let mut histories = HashMap::new();
    for query in &version_queries {
        let output = destination
            .ls_remote(["--quiet".to_owned()], query.version_patterns())
            .output()
            .wrap_err_with(|| {
                format!(
                    "Failed to observe version history at GHerrit remote '{}'",
                    destination.configured_remote()
                )
            })?;
        if !output.status.success() {
            bail!(
                "`git ls-remote` failed while observing version history at GHerrit remote '{}'",
                destination.configured_remote()
            );
        }
        for (id, versions) in parse_versions(&output.stdout, query.ids())? {
            if histories.insert(id, versions).is_some() {
                bail!("version history was returned by more than one query");
            }
        }
    }

    let changes = stack
        .iter()
        .map(|change| {
            let versions = histories.remove(change.id()).ok_or_else(|| {
                eyre!(
                    "version history for GHerrit change '{}' was not observed",
                    change.id().as_str()
                )
            })?;
            let observed = heads.remove(change.id()).ok_or_else(|| {
                eyre!(
                    "managed refs for GHerrit change '{}' were not observed",
                    change.id().as_str()
                )
            })?;
            Ok(ObservedChange {
                change,
                head: observed.head,
                owned_base: observed.owned_base,
                versions,
            })
        })
        .collect::<Result<Vec<_>>>()?;
    if !heads.is_empty() || !histories.is_empty() {
        bail!("remote observation contains a change outside the local stack");
    }
    Ok(ObservedStack { changes })
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

    fn head_patterns(&self) -> Vec<String> {
        self.ids().flat_map(head_patterns).collect()
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

fn head_patterns(id: &GherritPrId) -> [String; 2] {
    [format!("{HEAD_PREFIX}{}", id.as_str()), format!("{OWNED_BASE_PREFIX}{}", id.as_str())]
}

fn head_pattern_bytes(id: &GherritPrId) -> usize {
    head_patterns(id).iter().map(|pattern| pattern.len() + 1).sum()
}

fn version_patterns(id: &GherritPrId) -> [String; 2] {
    let root = format!("{VERSION_PREFIX}{}", id.as_str());
    [root.clone(), format!("{root}/*")]
}

fn version_pattern_bytes(id: &GherritPrId) -> usize {
    version_patterns(id).iter().map(|pattern| pattern.len() + 1).sum()
}

struct Record<'a> {
    object_id: ObjectId,
    name: &'a [u8],
    peeled: bool,
}

fn records(output: &[u8]) -> impl Iterator<Item = Result<Record<'_>>> {
    git_output_records(output).map(|record| {
        let mut fields = record.split(|byte| *byte == b'\t');
        let (Some(value), Some(name), None) = (fields.next(), fields.next(), fields.next()) else {
            bail!("malformed `git ls-remote` record: {record:?}");
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

#[derive(Default)]
struct ObservedHeads {
    head: Option<ObjectId>,
    owned_base: Option<ObjectId>,
}

fn parse_heads<'a>(
    output: &[u8],
    ids: impl IntoIterator<Item = &'a GherritPrId>,
) -> Result<HashMap<GherritPrId, ObservedHeads>> {
    let ids = ids.into_iter().collect::<Vec<_>>();
    let requested_heads = requested_names(ids.iter().copied(), |id| head_patterns(id)[0].clone())?;
    let requested_bases = requested_names(ids.iter().copied(), |id| head_patterns(id)[1].clone())?;
    let mut observed =
        ids.into_iter().map(|id| (id.clone(), ObservedHeads::default())).collect::<HashMap<_, _>>();
    for record in records(output) {
        let record = record?;
        if let Some(id) = requested_heads.get(record.name) {
            if record.peeled {
                bail!("managed head was advertised as a peeled tag");
            }
            let head = &mut observed.get_mut(id).expect("requested head has a record").head;
            if head.replace(record.object_id).is_some() {
                bail!("remote advertised a managed head more than once");
            }
        } else if let Some(id) = requested_bases.get(record.name) {
            if record.peeled {
                bail!("owned base was advertised as a peeled tag");
            }
            let owned_base =
                &mut observed.get_mut(id).expect("requested base has a record").owned_base;
            if owned_base.replace(record.object_id).is_some() {
                bail!("remote advertised an owned base more than once");
            }
        } else if record.name == OWNED_BASE_PREFIX.trim_end_matches('/').as_bytes() {
            bail!("remote ref uses the owned-base namespace root");
        } else if let Some(id) = record.name.strip_prefix(OWNED_BASE_PREFIX.as_bytes()) {
            GherritPrId::from_ref_component(id)
                .wrap_err("remote owned-base ref has an invalid change ID")?;
            bail!("remote advertised an owned base for an unrequested GHerrit change");
        } else if let Some(id) = record.name.strip_prefix(HEAD_PREFIX.as_bytes())
            && !id.contains(&b'/')
            && GherritPrId::from_ref_component(id).is_ok()
        {
            bail!("remote advertised a managed head for an unrequested GHerrit change");
        }
    }
    Ok(observed)
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
        if record.name == VERSION_PREFIX.trim_end_matches('/').as_bytes() {
            bail!("remote ref uses the version-tag namespace root");
        }
        let Some(suffix) = record.name.strip_prefix(VERSION_PREFIX.as_bytes()) else {
            continue;
        };
        let Some(separator) = suffix.iter().position(|byte| *byte == b'/') else {
            if let Some(id) = requested.get(suffix) {
                bail!("remote version namespace root exists for GHerrit change '{}'", id.as_str());
            }
            let id = GherritPrId::from_ref_component(suffix)
                .wrap_err("remote version namespace root has an invalid change ID")?;
            bail!(
                "remote advertised a version namespace for unrequested GHerrit change '{}'",
                id.as_str()
            );
        };
        let (id_component, suffix) = suffix.split_at(separator);
        let parsed_id = GherritPrId::from_ref_component(id_component)
            .wrap_err("remote version tag has an invalid change ID")?;
        let id = requested.get(id_component).ok_or_else(|| {
            eyre!(
                "remote advertised version history for unrequested GHerrit change '{}'",
                parsed_id.as_str()
            )
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

    const ONE: &str = "1111111111111111111111111111111111111111";
    const TWO: &str = "2222222222222222222222222222222222222222";

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
    fn parses_exact_heads_and_ignores_valid_tail_matches() {
        let mut output =
            format!("{ONE}\trefs/heads/Gone\n{TWO}\trefs/heads/archive/refs/heads/Gone\n")
                .into_bytes();
        output.extend_from_slice(format!("{TWO}\trefs/heads/archive/").as_bytes());
        output.extend_from_slice(b"\xff\n");
        let requested = ids(&["Gone", "Gmissing"]);
        let heads = parse_heads(&output, &requested).unwrap();

        assert_eq!(for_id(&heads, "Gone").head.unwrap().to_string(), ONE);
        assert_eq!(for_id(&heads, "Gmissing").head, None);
        assert_eq!(for_id(&heads, "Gmissing").owned_base, None);
    }

    #[test]
    fn observes_owned_bases_separately_from_managed_heads() {
        let output = format!("{ONE}\trefs/heads/Gone\n{TWO}\trefs/heads/gherrit-bases/Gone\n");
        let requested = ids(&["Gone"]);
        let heads = parse_heads(output.as_bytes(), &requested).unwrap();

        assert_eq!(for_id(&heads, "Gone").head.unwrap().to_string(), ONE);
        assert_eq!(for_id(&heads, "Gone").owned_base.unwrap().to_string(), TWO);
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
    fn rejects_invalid_records_even_when_the_ref_is_unrelated() {
        let requested = ids(&["Gone"]);
        for output in [
            b"not a record\n".as_slice(),
            b"xyz\trefs/heads/unrelated\n",
            b"0000000000000000000000000000000000000000\trefs/heads/unrelated\n",
            b"ref: refs/heads/main\trefs/heads/unrelated\n",
        ] {
            assert!(parse_heads(output, &requested).is_err(), "{output:?}");
        }
    }

    #[test]
    fn query_planning_uses_exact_patterns_and_preflights_every_id() {
        let requested = ids(&["Gone", "Gtwo"]);
        let refs = requested.iter().collect::<Vec<_>>();
        let one = head_pattern_bytes(&requested[0]);
        let two = head_pattern_bytes(&requested[1]);
        let split = plan_queries_with_budget(&refs, head_pattern_bytes, one + two - 1).unwrap();

        assert_eq!(split.len(), 2);
        assert_eq!(split[0].head_patterns(), ["refs/heads/Gone", "refs/heads/gherrit-bases/Gone"]);
        assert_eq!(split[1].head_patterns(), ["refs/heads/Gtwo", "refs/heads/gherrit-bases/Gtwo"]);
        assert!(plan_queries_with_budget(&refs, head_pattern_bytes, one - 1).is_err());
        assert!(plan_queries_with_budget(&[], head_pattern_bytes, one).unwrap().is_empty());
    }
}

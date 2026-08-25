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
    destination::{DefaultBranch, git_output_records},
    local::GherritPrId,
    version::Version,
};

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
pub(super) struct ExactLocalQueryPlan {
    default_branch: DefaultBranch,
    queries: Vec<Query>,
}

impl ExactLocalQueryPlan {
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
    pub(super) fn patterns(&self) -> impl ExactSizeIterator<Item = Vec<String>> + '_ {
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

/// Exact remote ref evidence for the requested local IDs, in request order.
#[derive(Debug)]
pub(super) struct RawExactLocalObservation {
    default_branch: DefaultBranch,
    changes: Box<[RawExactLocalChange]>,
}

impl RawExactLocalObservation {
    pub(super) fn default_branch(&self) -> &DefaultBranch {
        &self.default_branch
    }

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
    pull_request_marker: Option<RawPullRequestMarker>,
}

impl RawExactLocalChange {
    pub(super) fn id(&self) -> &GherritPrId {
        &self.id
    }

    pub(super) fn candidate_head(&self) -> Option<ObjectId> {
        self.candidate_head
    }

    pub(super) fn owned_base(&self) -> Option<ObjectId> {
        self.owned_base
    }

    pub(super) fn versions(&self) -> impl ExactSizeIterator<Item = &RawVersionRef> {
        self.versions.iter()
    }

    pub(super) fn pull_request_marker(&self) -> Option<&RawPullRequestMarker> {
        self.pull_request_marker.as_ref()
    }
}

/// Exact annotated-tag evidence for the immutable pull-request marker.
///
/// Git advertises an annotated tag as two records: the tag object itself and
/// the commit to which it peels. Keeping them together prevents either a
/// lightweight marker or a lone peeled record from masquerading as the
/// protocol marker.
#[derive(Debug)]
pub(super) struct RawPullRequestMarker {
    tag: ObjectId,
    v1: ObjectId,
}

impl RawPullRequestMarker {
    pub(super) fn tag(&self) -> ObjectId {
        self.tag
    }

    pub(super) fn v1(&self) -> ObjectId {
        self.v1
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
    pub(super) fn version(&self) -> Version {
        self.version
    }

    pub(super) fn object_id(&self) -> ObjectId {
        self.object_id
    }

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
    pull_request_marker: PendingPullRequestMarker,
}

#[derive(Default)]
struct PendingPullRequestMarker {
    tag: Option<ObjectId>,
    v1: Option<ObjectId>,
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
            let change = &mut pending[*index];
            match tag {
                ManagedTag::Version(version) => {
                    if record.peeled {
                        bail!(
                            "remote managed version tag for GHerrit change '{}' is annotated rather than lightweight",
                            id.as_str()
                        );
                    }
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
                    let slot = if record.peeled {
                        &mut change.pull_request_marker.v1
                    } else {
                        &mut change.pull_request_marker.tag
                    };
                    if slot.replace(record.object_id).is_some() {
                        bail!(
                            "remote advertised the same pull-request marker framing record more than once"
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

    ids
        .into_iter()
        .zip(pending)
        .map(|(id, pending)| {
            let pull_request_marker = match (
                pending.pull_request_marker.tag,
                pending.pull_request_marker.v1,
            ) {
                (None, None) => None,
                (Some(tag), Some(v1)) => Some(RawPullRequestMarker { tag, v1 }),
                (Some(_), None) => bail!(
                    "remote pull-request marker for GHerrit change '{}' is lightweight or omitted its peeled v1 commit",
                    id.as_str()
                ),
                (None, Some(_)) => bail!(
                    "remote pull-request marker for GHerrit change '{}' omitted its unpeeled annotated tag object",
                    id.as_str()
                ),
            };
            Ok(RawExactLocalChange {
                id: id.clone(),
                candidate_head: pending.candidate_head,
                owned_base: pending.owned_base,
                versions: pending.versions.into_values().collect(),
                pull_request_marker,
            })
        })
        .collect()
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
    use super::*;

    const DEFAULT: &str = "1111111111111111111111111111111111111111";
    const HEAD: &str = "2222222222222222222222222222222222222222";
    const BASE: &str = "3333333333333333333333333333333333333333";
    const MARKER: &str = "4444444444444444444444444444444444444444";
    const SHA256: &str = "5555555555555555555555555555555555555555555555555555555555555555";

    fn id(value: &str) -> GherritPrId {
        GherritPrId::from_ref_component(value.as_bytes()).unwrap()
    }

    fn default_branch() -> DefaultBranch {
        DefaultBranch::new("main".to_owned(), ObjectId::from_hex(DEFAULT.as_bytes()).unwrap())
            .unwrap()
    }

    fn decode<'output>(
        ids: &[GherritPrId],
        outputs: impl IntoIterator<Item = &'output [u8]>,
    ) -> Result<RawExactLocalObservation> {
        let default = default_branch();
        ExactLocalQueryPlan::new(default, ids)?.decode(outputs)
    }

    fn full_output() -> String {
        format!(
            "{DEFAULT}\trefs/heads/main\n\
             {HEAD}\trefs/heads/Gone\n\
             {BASE}\trefs/heads/gherrit-bases/Gone\n\
             {HEAD}\trefs/tags/gherrit/Gone/v1\n\
             {MARKER}\trefs/tags/gherrit/Gone/pr\n\
             {HEAD}\trefs/tags/gherrit/Gone/pr^{{}}\n"
        )
    }

    #[test]
    fn plans_only_exact_local_namespaces_and_rechecks_default_once() {
        let default = default_branch();
        let ids = [id("A"), id("B")];
        let plan = ExactLocalQueryPlan::new(default, &ids).unwrap();
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
    fn plans_every_batch_before_observation_and_rejects_invalid_sets() {
        let default = default_branch();
        let a = id("A");
        let b = id("B");
        let per_query_budget = local_pattern_bytes(&a) + local_pattern_bytes(&b) - 1;
        let total_budget = default.full_ref_name().len() + 1 + per_query_budget;
        let plan =
            ExactLocalQueryPlan::with_budget(default, &[a.clone(), b], total_budget).unwrap();
        let patterns = plan.patterns().collect::<Vec<_>>();
        assert_eq!(patterns.len(), 2);
        assert_eq!(patterns[0][0], "refs/heads/main");
        assert!(!patterns[1].iter().any(|pattern| pattern == "refs/heads/main"));

        assert!(ExactLocalQueryPlan::new(default_branch(), &[]).is_err());
        assert!(ExactLocalQueryPlan::new(default_branch(), &[a.clone(), a]).is_err());
        assert!(ExactLocalQueryPlan::new(default_branch(), &[id("main")]).is_err());
        assert!(
            ExactLocalQueryPlan::with_budget(
                default_branch(),
                &[id("A")],
                total_budget - per_query_budget
            )
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
             {BASE}\trefs/tags/gherrit/Second/pr^{{}}\n\
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
        let marker = changes[1].pull_request_marker().unwrap();
        assert_eq!(marker.tag().to_string(), MARKER);
        assert_eq!(marker.v1().to_string(), BASE);
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
        assert!(change.pull_request_marker().is_none());

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

        let observed = ExactLocalQueryPlan::with_budget(default, &[a.clone(), b.clone()], budget)
            .unwrap()
            .decode([first.as_bytes(), second.as_bytes()])
            .unwrap();
        assert_eq!(observed.iter().map(|change| change.id()).collect::<Vec<_>>(), [&a, &b]);

        assert!(
            ExactLocalQueryPlan::with_budget(default_branch(), &[a.clone(), b.clone()], budget)
                .unwrap()
                .decode([first.as_bytes()])
                .is_err()
        );
        assert!(
            ExactLocalQueryPlan::with_budget(default_branch(), &[a, b], budget)
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
            ExactLocalQueryPlan::with_budget(default, &[a, b], budget)
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
        assert!(change.pull_request_marker().is_none());

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
        assert!(change.pull_request_marker().is_none());
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
        ] {
            let output = format!("{DEFAULT}\trefs/heads/main\n{records}");
            assert!(decode(&ids, [output.as_bytes()]).is_err(), "accepted {output:?}");
        }
    }

    #[test]
    fn marker_requires_one_unpeeled_tag_and_one_peeled_v1_commit() {
        let ids = [id("Gone")];
        let framed = format!(
            "{DEFAULT}\trefs/heads/main\n\
             {MARKER}\trefs/tags/gherrit/Gone/pr\n\
             {HEAD}\trefs/tags/gherrit/Gone/pr^{{}}\n"
        );
        let observed = decode(&ids, [framed.as_bytes()]).unwrap();
        let marker = observed.iter().next().unwrap().pull_request_marker().unwrap();
        assert_eq!(marker.tag().to_string(), MARKER);
        assert_eq!(marker.v1().to_string(), HEAD);

        let unpeeled_only =
            format!("{DEFAULT}\trefs/heads/main\n{MARKER}\trefs/tags/gherrit/Gone/pr\n");
        assert!(decode(&ids, [unpeeled_only.as_bytes()]).is_err());

        let peeled_only =
            format!("{DEFAULT}\trefs/heads/main\n{HEAD}\trefs/tags/gherrit/Gone/pr^{{}}\n");
        assert!(decode(&ids, [peeled_only.as_bytes()]).is_err());
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
             {MARKER}\trefs/tags/gherrit/Gone/pr\n\
             {HEAD}\trefs/tags/gherrit/Gone/pr^{{}}\n"
        );
        let observed = decode(&ids, [output.as_bytes()]).unwrap();
        let change = observed.iter().next().unwrap();
        assert_eq!(change.candidate_head().unwrap().to_string(), HEAD);
        assert_eq!(change.owned_base(), None);
        assert_eq!(change.versions().next().unwrap().version().get(), 3);
        let marker = change.pull_request_marker().unwrap();
        assert_eq!(marker.tag().to_string(), MARKER);
        assert_eq!(marker.v1().to_string(), HEAD);
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

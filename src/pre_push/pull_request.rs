//! Pull-request identity evidence for one publication attempt.
//!
//! This module is the pure boundary between the complete repository-wide OPEN
//! observation and later whole-repository validation. It classifies managed
//! pull requests without consulting version tags, and it retains every OPEN
//! identity so a later create receipt cannot reuse one which appeared in the
//! initial observation.
//!
//! Correlation deliberately stops at evidence: the absence of an OPEN row is
//! not permission to create a pull request, and this module exposes no such
//! capability.
#![allow(dead_code)]

use std::{
    collections::{HashMap, HashSet},
    num::NonZeroU32,
};

use color_eyre::eyre::{Context as _, Result, bail, eyre};
use gix::ObjectId;
use serde::Deserialize;

use super::{github::OpenPullRequest, local::GherritPrId, remote::RemoteHeads};

const METADATA_PREFIX: &str = "<!-- gherrit-meta:";
const METADATA_LINE_PREFIX: &str = "<!-- gherrit-meta: ";
const METADATA_LINE_SUFFIX: &str = " -->";

/// Identity metadata embedded in a generated pull-request body.
///
/// Parent and child are projection hints and may be stale. Only `id`
/// establishes which managed change the body describes.
#[derive(Debug, Eq, PartialEq)]
pub(super) struct Metadata {
    id: GherritPrId,
    parent: Option<GherritPrId>,
    child: Option<GherritPrId>,
}

impl Metadata {
    pub(super) fn id(&self) -> &GherritPrId {
        &self.id
    }

    pub(super) fn parent(&self) -> Option<&GherritPrId> {
        self.parent.as_ref()
    }

    pub(super) fn child(&self) -> Option<&GherritPrId> {
        self.child.as_ref()
    }
}

#[derive(Deserialize)]
#[serde(untagged)]
enum NullableString {
    String(String),
    Null(()),
}

impl NullableString {
    fn into_option(self) -> Option<String> {
        match self {
            Self::String(value) => Some(value),
            Self::Null(()) => None,
        }
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawMetadata {
    id: String,
    // These are intentionally required nullable fields. Plain `Option` would
    // silently turn a missing field into `None`.
    parent: NullableString,
    child: NullableString,
}

/// Parses the one reserved metadata claim in a pull-request body.
///
/// Absence is distinct from an invalid claim. Once the reserved prefix occurs,
/// it must occupy one complete line and contain exactly the three required JSON
/// fields. JSON object field order and insignificant whitespace are accepted;
/// the outer comment spelling is exact. Both LF and CRLF bodies are supported.
pub(super) fn parse_metadata(body: &str) -> Result<Option<Metadata>> {
    let mut markers = body.match_indices(METADATA_PREFIX);
    let Some((marker_offset, _)) = markers.next() else {
        return Ok(None);
    };
    if markers.next().is_some() {
        bail!("pull request body contains repeated GHerrit metadata markers");
    }

    let line_start = body[..marker_offset].rfind('\n').map_or(0, |offset| offset + 1);
    if marker_offset != line_start {
        bail!("GHerrit metadata marker is not a standalone line");
    }
    let line_end =
        body[marker_offset..].find('\n').map_or(body.len(), |offset| marker_offset + offset);
    let line = body[line_start..line_end].strip_suffix('\r').unwrap_or(&body[line_start..line_end]);
    let Some(json) = line.strip_prefix(METADATA_LINE_PREFIX) else {
        bail!("GHerrit metadata marker has invalid framing");
    };
    let Some(json) = json.strip_suffix(METADATA_LINE_SUFFIX) else {
        bail!("GHerrit metadata marker is unterminated or not a standalone line");
    };

    let RawMetadata { id, parent, child } =
        serde_json::from_str(json).wrap_err("GHerrit metadata contains invalid JSON")?;
    let id = parse_id_field("id", id)?;
    let parent = parent.into_option().map(|value| parse_id_field("parent", value)).transpose()?;
    let child = child.into_option().map(|value| parse_id_field("child", value)).transpose()?;

    if parent.as_ref() == Some(&id) || child.as_ref() == Some(&id) {
        bail!("GHerrit metadata links a change to itself");
    }
    if parent.is_some() && parent == child {
        bail!("GHerrit metadata uses the same change as both parent and child");
    }

    Ok(Some(Metadata { id, parent, child }))
}

fn parse_id_field(field: &str, value: String) -> Result<GherritPrId> {
    GherritPrId::from_ref_component(value.as_bytes())
        .wrap_err_with(|| format!("GHerrit metadata field `{field}` is not a valid change ID"))
}

/// A positive pull-request number in GitHub's GraphQL `Int` range.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub(super) struct PullRequestNumber(NonZeroU32);

impl PullRequestNumber {
    fn new(value: u64) -> Result<Self> {
        let value = u32::try_from(value)
            .ok()
            .and_then(NonZeroU32::new)
            .filter(|value| value.get() <= i32::MAX as u32)
            .ok_or_else(|| eyre!("GitHub reported invalid pull request number {value}"))?;
        Ok(Self(value))
    }

    pub(super) fn get(self) -> u32 {
        self.0.get()
    }
}

/// A nonempty opaque GraphQL node ID.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub(super) struct PullRequestNodeId(Box<str>);

impl PullRequestNodeId {
    fn new(value: String) -> Result<Self> {
        if value.is_empty() {
            bail!("GitHub reported an empty pull request node ID");
        }
        Ok(Self(value.into_boxed_str()))
    }

    pub(super) fn as_str(&self) -> &str {
        &self.0
    }
}

/// The two values which together identify one GitHub pull request.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub(super) struct PullRequestIdentity {
    number: PullRequestNumber,
    node_id: PullRequestNodeId,
}

impl PullRequestIdentity {
    fn new(number: u64, node_id: String) -> Result<Self> {
        Ok(Self {
            number: PullRequestNumber::new(number)?,
            node_id: PullRequestNodeId::new(node_id)?,
        })
    }

    pub(super) fn number(&self) -> PullRequestNumber {
        self.number
    }

    pub(super) fn node_id(&self) -> &PullRequestNodeId {
        &self.node_id
    }
}

/// Every identity from the complete initial OPEN observation.
///
/// The registry proves collision absence independently in the number and node
/// ID namespaces. Each managed pull request separately retains its coupled
/// [`PullRequestIdentity`]. There is deliberately no insertion API after
/// correlation.
#[derive(Debug)]
pub(super) struct InitialPullRequestIdentities {
    numbers: HashSet<PullRequestNumber>,
    node_ids: HashSet<PullRequestNodeId>,
}

impl InitialPullRequestIdentities {
    fn from_open(pull_requests: &[OpenPullRequest]) -> Result<Self> {
        let mut numbers = HashSet::with_capacity(pull_requests.len());
        let mut node_ids = HashSet::with_capacity(pull_requests.len());

        for pull_request in pull_requests {
            let identity =
                PullRequestIdentity::new(pull_request.number, pull_request.node_id.clone())?;
            if !numbers.insert(identity.number) {
                bail!(
                    "GitHub OPEN observation repeats pull request number {}",
                    identity.number.get()
                );
            }
            if !node_ids.insert(identity.node_id) {
                bail!(
                    "GitHub OPEN observation repeats pull request node ID '{}'",
                    pull_request.node_id
                );
            }
        }

        Ok(Self { numbers, node_ids })
    }

    pub(super) fn len(&self) -> usize {
        self.numbers.len()
    }

    pub(super) fn contains(&self, identity: &PullRequestIdentity) -> bool {
        self.numbers.contains(&identity.number) && self.node_ids.contains(&identity.node_id)
    }

    pub(super) fn contains_number(&self, number: PullRequestNumber) -> bool {
        self.numbers.contains(&number)
    }

    pub(super) fn contains_node_id(&self, node_id: &PullRequestNodeId) -> bool {
        self.node_ids.contains(node_id)
    }
}

/// The only two supported base names for a managed pull request.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum BaseKind {
    Default,
    Owned,
}

impl BaseKind {
    /// Derives the complete base name from repository context and the outer ID.
    pub(super) fn branch_name(self, default: &str, id: &GherritPrId) -> String {
        match self {
            Self::Default => default.to_owned(),
            Self::Owned => owned_base_name(id),
        }
    }
}

/// A classified base name coupled to the object GitHub observed for it.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct ObservedBase {
    kind: BaseKind,
    oid: ObjectId,
}

impl ObservedBase {
    pub(super) fn kind(&self) -> BaseKind {
        self.kind
    }

    pub(super) fn oid(&self) -> ObjectId {
        self.oid
    }
}

/// Same-repository OPEN evidence which consistently identifies one change.
///
/// The object IDs are still observation evidence, not validated publication
/// state. Whole-repository validation must require the head OID to occur in
/// published history and an owned-base OID to occur among published first
/// parents. A proposed-only OID cannot explain an initial pre-write GitHub
/// observation.
#[derive(Debug)]
pub(super) struct ManagedOpenPullRequest {
    id: GherritPrId,
    identity: PullRequestIdentity,
    head_oid: ObjectId,
    base: ObservedBase,
    title: Box<str>,
    body: Box<str>,
    has_landing_automation: bool,
}

impl ManagedOpenPullRequest {
    pub(super) fn id(&self) -> &GherritPrId {
        &self.id
    }

    pub(super) fn identity(&self) -> &PullRequestIdentity {
        &self.identity
    }

    pub(super) fn head_oid(&self) -> ObjectId {
        self.head_oid
    }

    pub(super) fn base(&self) -> ObservedBase {
        self.base
    }

    pub(super) fn title(&self) -> &str {
        &self.title
    }

    pub(super) fn body(&self) -> &str {
        &self.body
    }

    pub(super) fn has_landing_automation(&self) -> bool {
        self.has_landing_automation
    }
}

/// Correlated state for one local ID, in exact local stack order.
#[derive(Debug)]
pub(super) enum LocalPullRequestObservation {
    Open(ManagedOpenPullRequest),
    NeedsTerminalProof(GherritPrId),
}

impl LocalPullRequestObservation {
    pub(super) fn id(&self) -> &GherritPrId {
        match self {
            Self::Open(pull_request) => pull_request.id(),
            Self::NeedsTerminalProof(id) => id,
        }
    }
}

/// Complete correlation output for one initial OPEN observation.
#[derive(Debug)]
pub(super) struct CorrelatedPullRequests {
    local: Box<[LocalPullRequestObservation]>,
    nonlocal: Box<[ManagedOpenPullRequest]>,
    initial_identities: InitialPullRequestIdentities,
}

impl CorrelatedPullRequests {
    pub(super) fn local(&self) -> &[LocalPullRequestObservation] {
        &self.local
    }

    pub(super) fn nonlocal(&self) -> &[ManagedOpenPullRequest] {
        &self.nonlocal
    }

    pub(super) fn initial_identities(&self) -> &InitialPullRequestIdentities {
        &self.initial_identities
    }
}

/// Correlates one complete OPEN scan without consulting version tags.
///
/// Every number/node pair is validated before forks are discarded. A fork can
/// therefore neither inject malformed identity evidence nor hide a collision,
/// but its body and branch names never participate in managed correlation.
pub(super) fn correlate<'a>(
    local_ids: impl IntoIterator<Item = &'a GherritPrId>,
    heads: &RemoteHeads,
    open_pull_requests: Vec<OpenPullRequest>,
) -> Result<CorrelatedPullRequests> {
    let local_ids = local_ids.into_iter().cloned().collect::<Vec<_>>();
    let mut local_positions = HashMap::with_capacity(local_ids.len());
    for (index, id) in local_ids.iter().enumerate() {
        if local_positions.insert(id.clone(), index).is_some() {
            bail!(
                "local pull request correlation received change '{}' more than once",
                id.as_str()
            );
        }
    }

    let initial_identities = InitialPullRequestIdentities::from_open(&open_pull_requests)?;
    let mut local = (0..local_ids.len()).map(|_| None).collect::<Vec<_>>();
    let mut nonlocal = Vec::new();
    let mut managed_ids = HashSet::new();

    for pull_request in open_pull_requests {
        let identity = PullRequestIdentity::new(pull_request.number, pull_request.node_id.clone())?;
        if pull_request.is_cross_repository {
            continue;
        }

        let metadata = parse_metadata(&pull_request.body).wrap_err_with(|| {
            format!(
                "GitHub pull request #{} contains invalid GHerrit metadata",
                identity.number.get()
            )
        })?;
        let head_id = GherritPrId::from_ref_component(pull_request.head_branch.as_bytes()).ok();
        let managed_id = match metadata {
            Some(metadata) => {
                if metadata.id().as_str() != pull_request.head_branch {
                    bail!(
                        "GitHub pull request #{} metadata identifies '{}' but its head is '{}'",
                        identity.number.get(),
                        metadata.id().as_str(),
                        pull_request.head_branch
                    );
                }
                Some(metadata.id)
            }
            None => head_id
                .clone()
                .filter(|id| heads.candidate_head(id).is_some() && heads.owned_base(id).is_some()),
        };

        let Some(id) = managed_id else {
            if head_id.as_ref().is_some_and(|id| local_positions.contains_key(id)) {
                bail!(
                    "GitHub pull request #{} uses local GHerrit head '{}' without managed metadata and owned-base evidence",
                    identity.number.get(),
                    pull_request.head_branch
                );
            }
            continue;
        };

        if !managed_ids.insert(id.clone()) {
            bail!("GitHub has more than one managed OPEN pull request for '{}'", id.as_str());
        }
        let base = classify_base(
            heads.default_branch().name(),
            &id,
            &pull_request.base_branch,
            pull_request.base_oid,
        )
        .wrap_err_with(|| {
            format!(
                "GitHub pull request #{} for '{}' has an unsupported base",
                identity.number.get(),
                id.as_str()
            )
        })?;
        let managed = ManagedOpenPullRequest {
            id: id.clone(),
            identity,
            head_oid: pull_request.head_oid,
            base,
            title: pull_request.title.into_boxed_str(),
            body: pull_request.body.into_boxed_str(),
            has_landing_automation: pull_request.has_auto_merge_request
                || pull_request.is_in_merge_queue,
        };

        if let Some(index) = local_positions.get(&id).copied() {
            local[index] = Some(managed);
        } else {
            nonlocal.push(managed);
        }
    }

    let local = local
        .into_iter()
        .zip(local_ids)
        .map(|(pull_request, id)| {
            pull_request
                .map_or(LocalPullRequestObservation::NeedsTerminalProof(id), |pull_request| {
                    LocalPullRequestObservation::Open(pull_request)
                })
        })
        .collect();

    Ok(CorrelatedPullRequests { local, nonlocal: nonlocal.into_boxed_slice(), initial_identities })
}

fn classify_base(
    default_branch: &str,
    id: &GherritPrId,
    observed_name: &str,
    oid: ObjectId,
) -> Result<ObservedBase> {
    let kind = if observed_name == default_branch {
        BaseKind::Default
    } else if observed_name == owned_base_name(id) {
        BaseKind::Owned
    } else {
        bail!(
            "expected '{}' or '{}', found '{observed_name}'",
            default_branch,
            owned_base_name(id)
        );
    };
    Ok(ObservedBase { kind, oid })
}

fn owned_base_name(id: &GherritPrId) -> String {
    format!("gherrit-bases/{}", id.as_str())
}

#[cfg(test)]
mod tests {
    use std::fmt::Write as _;

    use super::*;
    use crate::pre_push::remote;

    fn object_id(byte: u8) -> ObjectId {
        ObjectId::from_bytes_or_panic(&[byte; 20])
    }

    fn id(value: &str) -> GherritPrId {
        GherritPrId::from_ref_component(value.as_bytes()).unwrap()
    }

    fn raw_heads(candidate_heads: &[&str], owned_bases: &[&str]) -> RemoteHeads {
        let default = object_id(1);
        let mut advertisement =
            format!("ref: refs/heads/main\tHEAD\n{default}\tHEAD\n{default}\trefs/heads/main\n");
        for (index, candidate) in candidate_heads.iter().enumerate() {
            writeln!(
                advertisement,
                "{}\trefs/heads/{candidate}",
                object_id(u8::try_from(index + 10).unwrap())
            )
            .unwrap();
        }
        for (index, owned) in owned_bases.iter().enumerate() {
            writeln!(
                advertisement,
                "{}\trefs/heads/gherrit-bases/{owned}",
                object_id(u8::try_from(index + 100).unwrap())
            )
            .unwrap();
        }
        remote::parse_remote_heads_for_test(advertisement.as_bytes()).unwrap()
    }

    fn raw_open(
        number: u64,
        node_id: &str,
        head: &str,
        base: &str,
        body: &str,
        is_cross_repository: bool,
    ) -> OpenPullRequest {
        OpenPullRequest {
            number,
            node_id: node_id.to_owned(),
            title: format!("title for {head}"),
            body: body.to_owned(),
            base_branch: base.to_owned(),
            head_branch: head.to_owned(),
            base_oid: object_id(2),
            head_oid: object_id(3),
            is_cross_repository,
            has_auto_merge_request: false,
            is_in_merge_queue: false,
        }
    }

    fn metadata(id: &str, parent: Option<&str>, child: Option<&str>) -> String {
        format!(
            "<!-- gherrit-meta: {{\"id\":{id},\"parent\":{parent},\"child\":{child}}} -->",
            id = serde_json::to_string(id).unwrap(),
            parent = serde_json::to_string(&parent).unwrap(),
            child = serde_json::to_string(&child).unwrap(),
        )
    }

    fn local_kinds(correlated: &CorrelatedPullRequests) -> Vec<(&str, &'static str)> {
        correlated
            .local()
            .iter()
            .map(|pull_request| {
                (
                    pull_request.id().as_str(),
                    match pull_request {
                        LocalPullRequestObservation::Open(_) => "open",
                        LocalPullRequestObservation::NeedsTerminalProof(_) => "terminal",
                    },
                )
            })
            .collect()
    }

    #[test]
    fn metadata_absence_is_distinct_from_every_reserved_claim() {
        for body in
            ["", "ordinary body", "<!-- gherrit-meta : {} -->", "<!-- some-other-metadata: {} -->"]
        {
            assert_eq!(parse_metadata(body).unwrap(), None, "body={body:?}");
        }
    }

    #[test]
    fn metadata_accepts_all_valid_topology_shapes_and_json_field_orders() {
        let cases = [
            (metadata("G", None, None), "G", None, None),
            (metadata("G", Some("P"), None), "G", Some("P"), None),
            (metadata("G", None, Some("C")), "G", None, Some("C")),
            (metadata("G", Some("P"), Some("C")), "G", Some("P"), Some("C")),
            (
                r#"<!-- gherrit-meta: { "child": "C", "id": "G", "parent": "P" } -->"#.to_owned(),
                "G",
                Some("P"),
                Some("C"),
            ),
        ];

        for (line, expected_id, expected_parent, expected_child) in cases {
            let body = format!("human text\r\n{line}\r\nfooter");
            let parsed = parse_metadata(&body).unwrap().unwrap();
            assert_eq!(parsed.id().as_str(), expected_id);
            assert_eq!(parsed.parent().map(GherritPrId::as_str), expected_parent);
            assert_eq!(parsed.child().map(GherritPrId::as_str), expected_child);
        }
    }

    #[test]
    fn metadata_rejects_every_invalid_claim_shape() {
        let invalid = [
            "prefix <!-- gherrit-meta: {} -->",
            " <!-- gherrit-meta: {} -->",
            "<!-- gherrit-meta:{} -->",
            "<!-- gherrit-meta: {}",
            "<!-- gherrit-meta: {} --> suffix",
            "<!-- gherrit-meta: not-json -->",
            "<!-- gherrit-meta: null -->",
            "<!-- gherrit-meta: [] -->",
            r#"<!-- gherrit-meta: {"id":"G","id":"H","parent":null,"child":null} -->"#,
            r#"<!-- gherrit-meta: {"id":"G","parent":null,"child":null,"other":null} -->"#,
            r#"<!-- gherrit-meta: {"parent":null,"child":null} -->"#,
            r#"<!-- gherrit-meta: {"id":"G","child":null} -->"#,
            r#"<!-- gherrit-meta: {"id":"G","parent":null} -->"#,
            r#"<!-- gherrit-meta: {"id":null,"parent":null,"child":null} -->"#,
            r#"<!-- gherrit-meta: {"id":7,"parent":null,"child":null} -->"#,
            r#"<!-- gherrit-meta: {"id":"G","parent":7,"child":null} -->"#,
            r#"<!-- gherrit-meta: {"id":"G","parent":null,"child":false} -->"#,
            r#"<!-- gherrit-meta: {"id":"","parent":null,"child":null} -->"#,
            r#"<!-- gherrit-meta: {"id":"G-1","parent":null,"child":null} -->"#,
            r#"<!-- gherrit-meta: {"id":"G","parent":"P-1","child":null} -->"#,
            r#"<!-- gherrit-meta: {"id":"G","parent":null,"child":"C/1"} -->"#,
            r#"<!-- gherrit-meta: {"id":"G","parent":"G","child":null} -->"#,
            r#"<!-- gherrit-meta: {"id":"G","parent":null,"child":"G"} -->"#,
            r#"<!-- gherrit-meta: {"id":"G","parent":"X","child":"X"} -->"#,
        ];

        for body in invalid {
            assert!(parse_metadata(body).is_err(), "body={body:?}");
        }

        let repeated = format!("{}\n{}", metadata("G", None, None), metadata("G", None, None));
        assert!(parse_metadata(&repeated).is_err());
    }

    #[test]
    fn pull_request_identity_wrappers_enforce_exact_graphql_bounds() {
        for number in [1, i32::MAX as u64] {
            assert_eq!(
                PullRequestNumber::new(number).unwrap().get(),
                u32::try_from(number).unwrap()
            );
        }
        for number in [0, i32::MAX as u64 + 1, u64::MAX] {
            assert!(PullRequestNumber::new(number).is_err(), "number={number}");
        }

        assert_eq!(PullRequestNodeId::new("node".to_owned()).unwrap().as_str(), "node");
        assert_eq!(PullRequestNodeId::new(" ".to_owned()).unwrap().as_str(), " ");
        assert!(PullRequestNodeId::new(String::new()).is_err());
    }

    #[test]
    fn every_open_identity_including_forks_is_validated_and_retained() {
        let heads = raw_heads(&[], &[]);
        let open = vec![
            raw_open(7, "same-repo", "feature/x", "main", "ordinary", false),
            raw_open(9, "fork", "Gfork", "main", "<!-- gherrit-meta: malformed -->", true),
        ];
        let correlated = correlate(std::iter::empty(), &heads, open).unwrap();
        assert!(correlated.local().is_empty());
        assert!(correlated.nonlocal().is_empty());
        assert_eq!(correlated.initial_identities().len(), 2);

        let same_repo = PullRequestIdentity::new(7, "same-repo".to_owned()).unwrap();
        let fork = PullRequestIdentity::new(9, "fork".to_owned()).unwrap();
        assert!(correlated.initial_identities().contains(&same_repo));
        assert!(correlated.initial_identities().contains(&fork));
        assert!(correlated.initial_identities().contains_number(fork.number()));
        assert!(correlated.initial_identities().contains_node_id(fork.node_id()));
    }

    #[test]
    fn identity_collisions_fail_before_fork_filtering() {
        let heads = raw_heads(&[], &[]);
        let cases = [
            vec![
                raw_open(7, "node-a", "fork-a", "main", "", true),
                raw_open(7, "node-b", "other", "main", "", false),
            ],
            vec![
                raw_open(7, "same-node", "fork-a", "main", "", true),
                raw_open(8, "same-node", "other", "main", "", false),
            ],
            vec![
                raw_open(7, "same", "fork-a", "main", "", true),
                raw_open(7, "same", "fork-b", "main", "", true),
            ],
        ];

        for open in cases {
            assert!(correlate(std::iter::empty(), &heads, open).is_err());
        }
    }

    #[test]
    fn malformed_fork_identities_are_not_hidden_by_isolation() {
        let heads = raw_heads(&[], &[]);
        for pull_request in [
            raw_open(0, "node", "fork", "main", "", true),
            raw_open(i32::MAX as u64 + 1, "node", "fork", "main", "", true),
            raw_open(1, "", "fork", "main", "", true),
        ] {
            assert!(correlate(std::iter::empty(), &heads, vec![pull_request]).is_err());
        }
    }

    #[test]
    fn correlation_rejects_duplicate_local_identity_input() {
        let g = id("G");
        let heads = raw_heads(&[], &[]);
        assert!(correlate([&g, &g], &heads, Vec::new()).is_err());
    }

    #[test]
    fn correlation_preserves_local_order_and_separates_nonlocal_evidence() {
        let local = [id("A"), id("B"), id("C")];
        let heads = raw_heads(&["A", "B", "X"], &["A", "B", "X"]);
        let mut b = raw_open(11, "node-b", "B", "gherrit-bases/B", "ordinary", false);
        b.is_in_merge_queue = true;
        let x = raw_open(
            12,
            "node-x",
            "X",
            "gherrit-bases/X",
            &metadata("X", Some("old"), None),
            false,
        );
        let mut a = raw_open(13, "node-a", "A", "main", &metadata("A", None, Some("B")), false);
        a.has_auto_merge_request = true;

        let correlated = correlate(local.iter(), &heads, vec![b, x, a]).unwrap();
        assert_eq!(local_kinds(&correlated), [("A", "open"), ("B", "open"), ("C", "terminal")]);
        assert_eq!(
            correlated
                .nonlocal()
                .iter()
                .map(|pull_request| pull_request.id().as_str())
                .collect::<Vec<_>>(),
            ["X"]
        );
        assert_eq!(correlated.initial_identities().len(), 3);

        let LocalPullRequestObservation::Open(a) = &correlated.local()[0] else {
            panic!("A should be open");
        };
        assert_eq!(a.identity().number().get(), 13);
        assert_eq!(a.base().kind(), BaseKind::Default);
        assert_eq!(a.base().oid(), object_id(2));
        assert_eq!(a.head_oid(), object_id(3));
        assert_eq!(a.title(), "title for A");
        assert_eq!(a.body(), metadata("A", None, Some("B")));
        assert!(a.has_landing_automation());

        let LocalPullRequestObservation::Open(b) = &correlated.local()[1] else {
            panic!("B should be open");
        };
        assert_eq!(b.base().kind(), BaseKind::Owned);
        assert_eq!(b.base().kind().branch_name("main", b.id()), "gherrit-bases/B");
        assert!(b.has_landing_automation());
    }

    #[test]
    fn fallback_management_requires_both_exact_refs() {
        for candidate in [false, true] {
            for owned_base in [false, true] {
                let heads = raw_heads(
                    if candidate { &["G"][..] } else { &[] },
                    if owned_base { &["G"][..] } else { &[] },
                );
                let local = [id("G")];
                let open = vec![raw_open(1, "node", "G", "main", "ordinary", false)];
                let result = correlate(local.iter(), &heads, open);
                if candidate && owned_base {
                    assert_eq!(local_kinds(&result.unwrap()), [("G", "open")]);
                } else {
                    assert!(result.is_err(), "candidate={candidate}, owned_base={owned_base}");
                }

                let open = vec![raw_open(1, "node", "G", "main", "ordinary", false)];
                let result = correlate(std::iter::empty(), &heads, open).unwrap();
                assert_eq!(result.nonlocal().len(), usize::from(candidate && owned_base));
            }
        }
    }

    #[test]
    fn metadata_correlates_deleted_refs_for_later_validation() {
        let local = [id("G")];
        let heads = raw_heads(&[], &[]);
        let pull_request =
            raw_open(1, "node", "G", "gherrit-bases/G", &metadata("G", None, None), false);
        let correlated = correlate(local.iter(), &heads, vec![pull_request]).unwrap();
        assert_eq!(local_kinds(&correlated), [("G", "open")]);
    }

    #[test]
    fn metadata_must_agree_with_the_head_name() {
        let heads = raw_heads(&["H"], &["H"]);
        let pull_request = raw_open(1, "node", "H", "main", &metadata("G", None, None), false);
        assert!(correlate(std::iter::empty(), &heads, vec![pull_request]).is_err());
    }

    #[test]
    fn managed_base_names_are_exactly_default_or_the_outer_ids_owned_base() {
        let local = [id("G")];
        let heads = raw_heads(&[], &[]);
        for (base, expected) in [
            ("main", Some(BaseKind::Default)),
            ("gherrit-bases/G", Some(BaseKind::Owned)),
            ("gherrit-bases/H", None),
            ("feature", None),
        ] {
            let pull_request = raw_open(1, "node", "G", base, &metadata("G", None, None), false);
            let result = correlate(local.iter(), &heads, vec![pull_request]);
            match expected {
                Some(expected) => {
                    let correlated = result.unwrap();
                    let LocalPullRequestObservation::Open(pull_request) = &correlated.local()[0]
                    else {
                        panic!("G should be open");
                    };
                    assert_eq!(pull_request.base().kind(), expected);
                    assert_eq!(
                        pull_request.base().kind().branch_name("main", pull_request.id()),
                        base
                    );
                }
                None => assert!(result.is_err(), "base={base}"),
            }
        }
    }

    #[test]
    fn duplicate_or_conflicting_managed_prs_fail_closed() {
        let local = [id("G")];
        let heads = raw_heads(&["G"], &["G"]);
        let metadata_pr = raw_open(1, "one", "G", "main", &metadata("G", None, None), false);
        let fallback_pr = raw_open(2, "two", "G", "gherrit-bases/G", "ordinary", false);
        assert!(correlate(local.iter(), &heads, vec![metadata_pr, fallback_pr]).is_err());
    }

    #[test]
    fn invalid_same_repository_metadata_is_never_treated_as_unrelated() {
        let heads = raw_heads(&[], &[]);
        let pull_request = raw_open(
            1,
            "node",
            "unrelated/branch",
            "main",
            "<!-- gherrit-meta: malformed -->",
            false,
        );
        assert!(correlate(std::iter::empty(), &heads, vec![pull_request]).is_err());
    }

    #[test]
    fn forks_are_ignored_before_metadata_and_local_collision_checks() {
        let local = [id("G")];
        let heads = raw_heads(&["G"], &["G"]);
        let fork = raw_open(1, "fork-node", "G", "main", "<!-- gherrit-meta: malformed -->", true);
        let correlated = correlate(local.iter(), &heads, vec![fork]).unwrap();
        assert_eq!(local_kinds(&correlated), [("G", "terminal")]);
        assert_eq!(correlated.initial_identities().len(), 1);
    }

    #[test]
    fn local_same_name_without_managed_evidence_is_rejected() {
        let local = [id("G")];
        let heads = raw_heads(&[], &[]);
        let pull_request = raw_open(1, "node", "G", "main", "ordinary", false);
        assert!(correlate(local.iter(), &heads, vec![pull_request]).is_err());
    }
}

//! Literal commit graph evidence and complete local change histories.
//!
//! Every raw history is structurally checked before the object database is
//! touched. Sealed local evidence supplies proposals directly; one shared graph
//! resolves only published commits external to each proposal. Complete history
//! validation consumes that coupled value, and only the resulting newtype
//! exposes revisions to later planning.

use std::{
    collections::{HashMap, HashSet, VecDeque},
    fmt,
};

use color_eyre::eyre::{Context as _, Report, Result, bail, eyre};
use gix::ObjectId;

use super::{
    destination::DefaultBranch,
    local::{
        GHERRIT_ID_TRAILER_FORMAT, GherritPrId, LocalChange, LocalStack, gherrit_id_trailer_value,
    },
    remote::{RawExactLocalChange, RawExactLocalChangeParts, RawExactLocalObservation},
    version::Version,
};
use crate::util::{self, CommandExt as _};

/// One commit and the literal first parent recorded in that commit object.
///
/// Only sealed [`LocalChange`] evidence or [`CommitGraphEvidence`] can
/// construct this pair. Validation therefore never needs a second,
/// potentially conflicting proof of its fields.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub(super) struct Revision {
    head: ObjectId,
    first_parent: ObjectId,
}

impl Revision {
    fn from_local(change: &LocalChange) -> Self {
        Self { head: change.head(), first_parent: change.first_parent() }
    }

    pub(super) fn head(self) -> ObjectId {
        self.head
    }

    pub(super) fn first_parent(self) -> ObjectId {
        self.first_parent
    }
}

/// A structurally nonempty sequence of literal published revisions.
///
/// Version numbers are derived from positions: `first` is v1 and position
/// zero in `later` is v2. Every immutable tag position remains distinct,
/// including adjacent and nonadjacent repeats. A validated marker target is
/// reduced to presence because its particular historical head has no later
/// semantic meaning.
#[derive(Clone, Debug, Eq, PartialEq)]
struct PublishedHistory {
    first: Revision,
    later: Box<[Revision]>,
    has_pull_request_marker: bool,
}

impl PublishedHistory {
    fn len(&self) -> usize {
        1 + self.later.len()
    }

    fn iter(&self) -> impl DoubleEndedIterator<Item = Revision> + ExactSizeIterator + '_ {
        (0..self.len()).map(|index| self.at(index))
    }

    fn at(&self, index: usize) -> Revision {
        if index == 0 { self.first } else { self.later[index - 1] }
    }

    fn versioned(
        &self,
    ) -> impl DoubleEndedIterator<Item = (Version, Revision)> + ExactSizeIterator + '_ {
        self.iter().enumerate().map(|(index, revision)| {
            let version = Version::from_history_index(index)
                .expect("an in-memory history position always fits in u64");
            (version, revision)
        })
    }

    fn current(&self) -> CurrentVersion {
        let index = self.len() - 1;
        let number = Version::from_history_index(index)
            .expect("an in-memory history position always fits in u64");
        let revision = self.later.last().copied().unwrap_or(self.first);
        CurrentVersion { number, revision }
    }

    fn has_pull_request_marker(&self) -> bool {
        self.has_pull_request_marker
    }
}

/// The current entry in one nonempty published or projected history.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct CurrentVersion {
    number: Version,
    revision: Revision,
}

impl CurrentVersion {
    pub(super) fn number(self) -> Version {
        self.number
    }

    pub(super) fn revision(self) -> Revision {
        self.revision
    }
}

/// Positional history which has passed every check not requiring the ODB.
struct PreparedPublishedHistory {
    first: ObjectId,
    later: Box<[ObjectId]>,
    owned_base: ObjectId,
    has_pull_request_marker: bool,
}

/// Structurally checked version which still owns exact acquisition provenance.
struct SourcedPreparedVersion {
    head: ObjectId,
    source_ref: Box<str>,
}

struct SourcedPreparedPublishedHistory {
    first: SourcedPreparedVersion,
    later: Box<[SourcedPreparedVersion]>,
    owned_base: ObjectId,
    has_pull_request_marker: bool,
}

/// One raw history after structural preflight and before provenance is split
/// from the history facts used by validation.
struct SourcedPreparedHistory {
    id: GherritPrId,
    published: Option<SourcedPreparedPublishedHistory>,
}

impl PreparedPublishedHistory {
    fn len(&self) -> usize {
        1 + self.later.len()
    }

    fn iter(&self) -> impl DoubleEndedIterator<Item = ObjectId> + ExactSizeIterator + '_ {
        (0..self.len()).map(|index| if index == 0 { self.first } else { self.later[index - 1] })
    }

    fn resolve_with(
        self,
        id: &GherritPrId,
        mut resolve: impl FnMut(ObjectId) -> Result<Revision>,
    ) -> Result<PublishedHistory> {
        let mut revisions = self
            .iter()
            .enumerate()
            .map(|(index, head)| {
                let version = Version::from_history_index(index)
                    .expect("a prepared history position fits in u64");
                resolve(head).wrap_err_with(|| {
                    format!(
                        "Version v{version} of GHerrit change '{}' is not a complete literal revision",
                        id.as_str()
                    )
                })
            })
            .collect::<Result<Vec<_>>>()?
            .into_iter();
        let first = revisions.next().expect("a prepared published history is nonempty");
        let later = revisions.collect::<Box<[_]>>();
        let latest = later.last().copied().unwrap_or(first);
        if self.owned_base != latest.first_parent() {
            bail!(
                "Remote GHerrit change '{}' owned base does not match the latest version's first parent",
                id.as_str()
            );
        }
        Ok(PublishedHistory { first, later, has_pull_request_marker: self.has_pull_request_marker })
    }
}

/// Provenance-free history facts ready for object-database resolution.
struct PreparedHistory {
    id: GherritPrId,
    published: Option<PreparedPublishedHistory>,
}

impl SourcedPreparedHistory {
    fn from_raw(raw: RawExactLocalChange) -> Result<Self> {
        let RawExactLocalChangeParts {
            id,
            candidate_head,
            owned_base,
            versions,
            pull_request_marker: marker_target,
        } = raw.into_parts();
        let versions = versions.into_vec().into_iter().map(|version| {
            let (number, head, source_ref) = version.into_parts();
            (number, SourcedPreparedVersion { head, source_ref })
        });
        Self::from_parts(id, candidate_head, owned_base, versions, marker_target)
    }

    #[cfg(test)]
    fn from_raw_ref(raw: &RawExactLocalChange) -> Result<Self> {
        let versions = raw.versions().map(|version| {
            (
                version.version(),
                SourcedPreparedVersion {
                    head: version.object_id(),
                    source_ref: version.source_ref().into(),
                },
            )
        });
        Self::from_parts(
            raw.id().clone(),
            raw.candidate_head(),
            raw.owned_base(),
            versions,
            raw.pull_request_marker(),
        )
    }

    fn from_parts(
        id: GherritPrId,
        candidate_head: Option<ObjectId>,
        owned_base: Option<ObjectId>,
        versions: impl ExactSizeIterator<Item = (Version, SourcedPreparedVersion)>,
        marker_target: Option<ObjectId>,
    ) -> Result<Self> {
        let version_count = versions.len();

        if version_count == 0 && marker_target.is_some() {
            bail!(
                "Remote GHerrit change '{}' has a pull-request marker but no published history",
                id.as_str()
            );
        }
        match (candidate_head, owned_base, version_count == 0) {
            (None, None, true) => return Ok(Self { id, published: None }),
            (Some(_), Some(_), false) => {}
            (None, _, false) => {
                bail!(
                    "Remote GHerrit change '{}' has version tags but no managed head",
                    id.as_str()
                );
            }
            (_, None, _) => {
                bail!(
                    "Remote GHerrit change '{}' does not have a complete owned base",
                    id.as_str()
                );
            }
            (_, _, true) => {
                bail!(
                    "Remote GHerrit change '{}' has managed refs but no version tags",
                    id.as_str()
                );
            }
        }

        let slots = versions
            .enumerate()
            .map(|(index, (actual, prepared_version))| {
                let expected = Version::from_history_index(index).ok_or_else(|| {
                    eyre!("Remote GHerrit change '{}' has too many versions", id.as_str())
                })?;
                if actual != expected {
                    bail!(
                        "Remote GHerrit change '{}' has noncontiguous version tags: expected v{expected}, observed v{actual}",
                        id.as_str()
                    );
                }
                Ok(prepared_version)
            })
            .collect::<Result<Vec<_>>>()?;
        let latest_head = slots.last().expect("a complete published shape is nonempty").head;
        if candidate_head != Some(latest_head) {
            bail!(
                "Remote GHerrit change '{}' head does not match its latest version tag",
                id.as_str()
            );
        }
        let has_pull_request_marker = marker_target
            .map(|target| {
                if !slots.iter().any(|version| version.head == target) {
                    bail!(
                        "Pull-request marker for GHerrit change '{}' does not target a published version head",
                        id.as_str()
                    );
                }
                Ok(true)
            })
            .transpose()?
            .unwrap_or(false);

        let mut slots = slots.into_iter();
        let first = slots.next().expect("a complete published shape is nonempty");
        let published = SourcedPreparedPublishedHistory {
            first,
            later: slots.collect(),
            owned_base: owned_base.expect("the complete shape has an owned base"),
            has_pull_request_marker,
        };
        Ok(Self { id, published: Some(published) })
    }

    #[cfg(test)]
    fn version_heads(&self) -> impl Iterator<Item = ObjectId> + '_ {
        self.published
            .iter()
            .flat_map(|published| std::iter::once(&published.first).chain(&published.later))
            .map(|version| version.head)
    }

    /// Consumes every source ref while producing provenance-free history.
    fn into_prepared(self, mut consume_source: impl FnMut(ObjectId, Box<str>)) -> PreparedHistory {
        let Self { id, published } = self;
        let published = published.map(|published| {
            let SourcedPreparedPublishedHistory {
                first,
                later,
                owned_base,
                has_pull_request_marker,
            } = published;
            let mut heads = std::iter::once(first).chain(later.into_vec()).map(|version| {
                let SourcedPreparedVersion { head, source_ref } = version;
                consume_source(head, source_ref);
                head
            });
            let first = heads.next().expect("a sourced prepared history is nonempty");
            PreparedPublishedHistory {
                first,
                later: heads.collect(),
                owned_base,
                has_pull_request_marker,
            }
        });
        PreparedHistory { id, published }
    }

    #[cfg(test)]
    fn normalize_for_test(
        repository: &util::Repo,
        raw: &RawExactLocalChange,
    ) -> Result<(GherritPrId, Option<PublishedHistory>)> {
        let sourced = Self::from_raw_ref(raw)?;
        let roots = sourced.version_heads().collect::<Vec<_>>();
        let prepared = sourced.into_prepared(|_, _| {});
        let graph =
            CommitGraphEvidence::load_for_test(repository, roots).map_err(graph_load_report)?;
        prepared.resolve_remote_for_test(&graph)
    }
}

impl PreparedHistory {
    fn resolve(self, local: &LocalChange, graph: &CommitGraphEvidence) -> Result<ChangeHistory> {
        let Self { id, published } = self;
        let proposed = Revision::from_local(local);
        let published = published
            .map(|history| {
                history.resolve_with(&id, |head| {
                    if head == proposed.head() { Ok(proposed) } else { graph.revision(head) }
                })
            })
            .transpose()?;
        Ok(ChangeHistory { id, published, proposed })
    }

    #[cfg(test)]
    fn resolve_remote_for_test(
        self,
        graph: &CommitGraphEvidence,
    ) -> Result<(GherritPrId, Option<PublishedHistory>)> {
        let Self { id, published } = self;
        let published = published
            .map(|history| history.resolve_with(&id, |head| graph.revision(head)))
            .transpose()?;
        Ok((id, published))
    }
}

/// One distinct external graph root and its nonempty exact advertised aliases.
#[derive(Debug)]
pub(super) struct ExternalGraphRoot {
    oid: ObjectId,
    first_source_ref: Box<str>,
    later_source_refs: Box<[Box<str>]>,
}

impl ExternalGraphRoot {
    pub(super) fn oid(&self) -> ObjectId {
        self.oid
    }

    pub(super) fn source_refs(&self) -> impl Iterator<Item = &str> {
        std::iter::once(self.first_source_ref.as_ref())
            .chain(self.later_source_refs.iter().map(Box::as_ref))
    }

    #[cfg(test)]
    pub(super) fn for_test(oid: ObjectId, first: &str, later: &[&str]) -> Self {
        Self {
            oid,
            first_source_ref: first.into(),
            later_source_refs: later.iter().map(|source_ref| Box::from(*source_ref)).collect(),
        }
    }
}

/// Whole-set structural proof which keeps exact acquisition provenance alive.
pub(super) struct PreparedExactLocalHistories<'local, 'repository> {
    repository: &'repository util::Repo,
    histories: StructurallyPreparedHistories<'local>,
}

struct StructurallyPreparedHistories<'local> {
    local: &'local LocalStack,
    prepared: Box<[PreparedHistory]>,
    roots: Box<[ExternalGraphRoot]>,
}

impl<'local, 'repository> PreparedExactLocalHistories<'local, 'repository> {
    pub(super) fn prepare(
        observation: RawExactLocalObservation,
        local: &'local LocalStack,
        repository: &'repository util::Repo,
    ) -> Result<Self> {
        let histories = StructurallyPreparedHistories::prepare(observation, local)?;
        Ok(Self { repository, histories })
    }

    /// Loads from the repository sealed into this preparation only.
    pub(super) fn load_graph(
        &self,
    ) -> std::result::Result<CommitGraphEvidence, GraphLoadError<'_>> {
        CommitGraphEvidence::load(self.repository, &self.histories.roots)
    }

    pub(super) fn validate(
        self,
        graph: &CommitGraphEvidence,
    ) -> Result<Box<[ValidatedChangeHistory]>> {
        self.histories.validate(graph)
    }
}

impl<'local> StructurallyPreparedHistories<'local> {
    fn prepare(observation: RawExactLocalObservation, local: &'local LocalStack) -> Result<Self> {
        let (default_branch, changes) = observation.into_parts();
        let sourced = changes.into_vec().into_iter().map(SourcedPreparedHistory::from_raw);
        Self::finish(&default_branch, sourced, local)
    }

    #[cfg(test)]
    fn prepare_raw(
        observation: &RawExactLocalObservation,
        local: &'local LocalStack,
    ) -> Result<Self> {
        Self::finish(
            observation.default_branch(),
            observation.iter().map(SourcedPreparedHistory::from_raw_ref),
            local,
        )
    }

    fn finish(
        default_branch: &DefaultBranch,
        sourced: impl Iterator<Item = Result<SourcedPreparedHistory>>,
        local: &'local LocalStack,
    ) -> Result<Self> {
        // Collecting the whole iterator first is the authority boundary: every
        // raw structural error precedes every object database access.
        let sourced = sourced.collect::<Result<Box<[_]>>>()?;
        if default_branch != local.default_branch() {
            bail!("Exact local Git observation does not match the local stack's default branch");
        }
        if sourced.len() != local.len()
            || sourced.iter().zip(local.iter()).any(|(history, change)| history.id != *change.id())
        {
            bail!("Exact local Git histories do not match the ordered local stack");
        }

        let mut root_indexes = HashMap::<ObjectId, usize>::new();
        let mut roots = Vec::<(ObjectId, Box<str>, Vec<Box<str>>)>::new();
        let prepared = sourced
            .into_vec()
            .into_iter()
            .zip(local.iter())
            .map(|(sourced, local)| {
                let proposal = local.head();
                sourced.into_prepared(|head, source_ref| {
                    // Failure precedence and acquisition order are semantic:
                    // local-stack order, then immutable version order. A
                    // proposal-equal slot contributes neither a root nor an
                    // alias. The same OID may remain external to another
                    // change and then owns that external slot's source ref.
                    if head == proposal {
                        return;
                    }
                    match root_indexes.get(&head).copied() {
                        Some(index) => roots[index].2.push(source_ref),
                        None => {
                            root_indexes.insert(head, roots.len());
                            roots.push((head, source_ref, Vec::new()));
                        }
                    }
                })
            })
            .collect::<Box<[_]>>();
        let roots = roots
            .into_iter()
            .map(|(oid, first_source_ref, later_source_refs)| ExternalGraphRoot {
                oid,
                first_source_ref,
                later_source_refs: later_source_refs.into_boxed_slice(),
            })
            .collect();
        Ok(Self { local, prepared, roots })
    }

    #[cfg(test)]
    pub(super) fn graph_roots(&self) -> Box<[ObjectId]> {
        self.roots.iter().map(ExternalGraphRoot::oid).collect()
    }

    #[cfg(test)]
    pub(super) fn external_roots(&self) -> impl ExactSizeIterator<Item = &ExternalGraphRoot> {
        self.roots.iter()
    }

    fn validate(self, graph: &CommitGraphEvidence) -> Result<Box<[ValidatedChangeHistory]>> {
        let histories = self
            .prepared
            .into_vec()
            .into_iter()
            .zip(self.local.iter())
            .map(|(prepared, local)| prepared.resolve(local, graph))
            .collect::<Result<Vec<_>>>()?;
        histories.into_iter().map(|history| history.validate(graph)).collect()
    }
}

/// Complete, unvalidated history for exactly one local change.
///
/// There is no inspection surface. The entire value must be consumed by
/// [`ChangeHistory::validate`] before later planning can see a revision.
#[derive(Debug)]
struct ChangeHistory {
    id: GherritPrId,
    published: Option<PublishedHistory>,
    proposed: Revision,
}

impl ChangeHistory {
    fn external_published_revisions(&self) -> impl Iterator<Item = Revision> + '_ {
        self.published
            .iter()
            .flat_map(PublishedHistory::iter)
            .filter(|revision| revision.head() != self.proposed.head())
    }

    fn validate(self, graph: &CommitGraphEvidence) -> Result<ValidatedChangeHistory> {
        let mut heads = Vec::new();
        let mut head_set = HashSet::new();
        for revision in self.external_published_revisions() {
            if head_set.insert(revision.head()) {
                heads.push(revision.head());
            }
        }

        let expected = self.id.as_str().as_bytes();
        for head in &heads {
            let identities = graph.canonical_ids(*head);
            if identities.len() != 1 || identities[0].as_ref() != expected {
                bail!(
                    "Head commit {head} must have exactly one gherrit-pr-id trailer equal to '{}'",
                    self.id.as_str()
                );
            }
        }

        // LocalStack already proves the proposal's exact identity, literal
        // first parent, default descent, and absence of this active ID from
        // every other commit in complete proposal ancestry. For external
        // published heads, exact own-ID checks plus this one all-parent union
        // walk supply the complementary proof. Together those invariants imply
        // owned-base and default-base safety without loading either local root.
        let proper_ancestry =
            heads.iter().flat_map(|head| graph.parents(*head).iter().copied()).collect::<Vec<_>>();
        if let Some(duplicate) = graph.first_reachable(proper_ancestry, |oid| {
            graph.canonical_ids(oid).iter().any(|identity| identity.as_ref() == expected)
        }) {
            bail!(
                "Proper ancestry of GHerrit change '{}' repeats its gherrit-pr-id at commit {duplicate}",
                self.id.as_str()
            );
        }

        Ok(ValidatedChangeHistory(self))
    }
}

/// Complete history evidence safe for planning to inspect.
///
/// This newtype owns the entire validated input and delegates read-only views;
/// it has no decomposition or dereference escape hatch.
#[derive(Debug)]
pub(super) struct ValidatedChangeHistory(ChangeHistory);

impl ValidatedChangeHistory {
    /// Constructs trusted synthetic literal facts for renderer unit tests.
    ///
    /// History validation itself is exercised in this module's boundary
    /// tests. Body tests use this narrow constructor so they do not rebuild a
    /// repository merely to test deterministic formatting and byte limits.
    /// Callers are responsible for supplying facts appropriate to their case.
    #[cfg(test)]
    pub(super) fn for_body_test(
        id: GherritPrId,
        published: &[(ObjectId, ObjectId)],
        proposal: (ObjectId, ObjectId),
    ) -> Self {
        Self::for_plan_test(id, published, proposal, false)
    }

    /// Constructs trusted synthetic planning facts after history validation
    /// has been tested at its own boundary.
    #[cfg(test)]
    pub(super) fn for_plan_test(
        id: GherritPrId,
        published: &[(ObjectId, ObjectId)],
        proposal: (ObjectId, ObjectId),
        has_pull_request_marker: bool,
    ) -> Self {
        assert!(
            !has_pull_request_marker || !published.is_empty(),
            "a pull request marker requires published history"
        );
        let published = published.split_first().map(|(first, later)| PublishedHistory {
            first: Revision { head: first.0, first_parent: first.1 },
            later: later
                .iter()
                .map(|(head, first_parent)| Revision { head: *head, first_parent: *first_parent })
                .collect(),
            has_pull_request_marker,
        });
        Self(ChangeHistory {
            id,
            published,
            proposed: Revision { head: proposal.0, first_parent: proposal.1 },
        })
    }

    pub(super) fn id(&self) -> &GherritPrId {
        &self.0.id
    }

    pub(super) fn published_len(&self) -> usize {
        self.0.published.as_ref().map_or(0, PublishedHistory::len)
    }

    pub(super) fn published_versions(
        &self,
    ) -> impl DoubleEndedIterator<Item = (Version, Revision)> + ExactSizeIterator + '_ {
        (0..self.published_len()).map(|index| {
            let history =
                self.0.published.as_ref().expect("a positive published length has a history");
            let version = Version::from_history_index(index)
                .expect("an in-memory history position always fits in u64");
            (version, history.at(index))
        })
    }

    pub(super) fn published_current(&self) -> Option<CurrentVersion> {
        self.0.published.as_ref().map(PublishedHistory::current)
    }

    pub(super) fn has_pull_request_marker(&self) -> bool {
        self.0.published.as_ref().is_some_and(PublishedHistory::has_pull_request_marker)
    }

    pub(super) fn proposed(&self) -> Revision {
        self.0.proposed
    }

    pub(super) fn needs_publication(&self) -> bool {
        self.published_current().is_none_or(|current| current.revision() != self.proposed())
    }

    pub(super) fn projected_versions(
        &self,
    ) -> impl DoubleEndedIterator<Item = (Version, Revision)> + ExactSizeIterator + '_ {
        let published_len = self.published_len();
        let projected_len = published_len + usize::from(self.needs_publication());
        (0..projected_len).map(move |index| {
            let version = Version::from_history_index(index)
                .expect("an in-memory projected position always fits in u64");
            let revision = if index < published_len {
                self.0.published.as_ref().expect("a published position has a history").at(index)
            } else {
                self.proposed()
            };
            (version, revision)
        })
    }

    pub(super) fn projected_current(&self) -> CurrentVersion {
        if self.needs_publication() {
            let number = Version::from_history_index(self.published_len())
                .expect("an in-memory projected position always fits in u64");
            CurrentVersion { number, revision: self.proposed() }
        } else {
            self.published_current().expect("only a published revision can equal the proposal")
        }
    }

    pub(super) fn contains_published_head(&self, head: ObjectId) -> bool {
        self.published_versions().any(|(_, revision)| revision.head() == head)
    }

    pub(super) fn contains_published_first_parent(&self, first_parent: ObjectId) -> bool {
        self.published_versions().any(|(_, revision)| revision.first_parent() == first_parent)
    }
}

#[derive(Debug)]
struct CommitFacts {
    parents: Box<[ObjectId]>,
}

type CanonicalIdsByCommit = HashMap<ObjectId, Box<[Box<[u8]>]>>;
const CANONICAL_ID_QUERY_BATCH_LEN: usize = 120;

/// Complete literal all-parent graph shared by every exact local history.
#[derive(Debug)]
pub(super) struct CommitGraphEvidence {
    commits: HashMap<ObjectId, CommitFacts>,
    canonical_ids: CanonicalIdsByCommit,
}

/// Separates exact acquisition capabilities from invalid local graph evidence.
#[derive(Debug)]
pub(super) enum GraphLoadError<'root> {
    MissingAdvertisedRoot(MissingAdvertisedRoot<'root>),
    MissingAncestor(MissingAncestor<'root>),
    Invalid(Report),
}

#[derive(Debug)]
pub(super) struct MissingAdvertisedRoot<'root> {
    root: &'root ExternalGraphRoot,
}

impl<'root> MissingAdvertisedRoot<'root> {
    pub(super) fn root(&self) -> &'root ExternalGraphRoot {
        self.root
    }
}

#[derive(Debug)]
pub(super) struct MissingAncestor<'root> {
    oid: ObjectId,
    root: &'root ExternalGraphRoot,
}

impl<'root> MissingAncestor<'root> {
    pub(super) fn oid(&self) -> ObjectId {
        self.oid
    }

    pub(super) fn root(&self) -> &'root ExternalGraphRoot {
        self.root
    }
}

impl fmt::Display for GraphLoadError<'_> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::MissingAdvertisedRoot(missing) => {
                write!(formatter, "Advertised graph-root commit {} is missing", missing.root.oid())
            }
            Self::MissingAncestor(missing) => write!(
                formatter,
                "Commit {} in ancestry of advertised graph root {} is missing",
                missing.oid,
                missing.root.oid()
            ),
            Self::Invalid(error) => error.fmt(formatter),
        }
    }
}

impl std::error::Error for GraphLoadError<'_> {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::MissingAdvertisedRoot(_) | Self::MissingAncestor(_) => None,
            Self::Invalid(error) => error.source(),
        }
    }
}

impl GraphLoadError<'_> {
    pub(super) fn into_report(self) -> Report {
        match self {
            Self::Invalid(error) => error,
            error => eyre!(error.to_string()),
        }
    }
}

#[cfg(test)]
#[derive(Debug)]
enum TestGraphLoadError {
    MissingAdvertisedRoot { root: ObjectId },
    MissingAncestor { oid: ObjectId, root: ObjectId },
    Invalid(Report),
}

#[cfg(test)]
impl fmt::Display for TestGraphLoadError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::MissingAdvertisedRoot { root } => {
                write!(formatter, "Advertised graph-root commit {root} is missing")
            }
            Self::MissingAncestor { oid, root } => write!(
                formatter,
                "Commit {oid} in ancestry of advertised graph root {root} is missing"
            ),
            Self::Invalid(error) => error.fmt(formatter),
        }
    }
}

#[cfg(test)]
fn graph_load_report(error: TestGraphLoadError) -> Report {
    match error {
        TestGraphLoadError::Invalid(error) => error,
        TestGraphLoadError::MissingAdvertisedRoot { root } => {
            eyre!("Advertised graph-root commit {root} is missing")
        }
        TestGraphLoadError::MissingAncestor { oid, root } => {
            eyre!("Commit {oid} in ancestry of advertised graph root {root} is missing")
        }
    }
}

impl CommitGraphEvidence {
    /// Loads and fully decodes each reachable commit once breadth-first: all
    /// direct roots precede ancestry, while roots and each commit's declared
    /// parents retain their supplied order.
    fn load<'root>(
        repository: &util::Repo,
        roots: &'root [ExternalGraphRoot],
    ) -> std::result::Result<Self, GraphLoadError<'root>> {
        let mut scheduled = HashSet::new();
        let mut pending = roots
            .iter()
            .filter(|root| scheduled.insert(root.oid()))
            .map(|root| (root.oid(), root))
            .collect::<VecDeque<_>>();
        let mut commits = HashMap::new();
        let mut trailer_candidates = Vec::new();

        while let Some((oid, causal_root)) = pending.pop_front() {
            let object = repository
                .try_find_object(oid)
                .map_err(|error| {
                    GraphLoadError::Invalid(
                        Report::new(error).wrap_err(format!("Failed to read object {oid}")),
                    )
                })?
                .ok_or_else(|| {
                    if oid == causal_root.oid() {
                        GraphLoadError::MissingAdvertisedRoot(MissingAdvertisedRoot {
                            root: causal_root,
                        })
                    } else {
                        GraphLoadError::MissingAncestor(MissingAncestor { oid, root: causal_root })
                    }
                })?;
            if object.kind != gix::object::Kind::Commit {
                return Err(GraphLoadError::Invalid(eyre!(
                    "Object {oid} is {}, not a commit",
                    object.kind
                )));
            }
            let commit = object
                .try_into_commit()
                .map_err(|error| GraphLoadError::Invalid(Report::new(error)))?;
            let decoded = commit.decode().map_err(|error| {
                GraphLoadError::Invalid(
                    Report::new(error).wrap_err(format!("Commit {oid} has malformed encoding")),
                )
            })?;
            if message_may_contain_identity(decoded.message) {
                trailer_candidates.push(oid);
            }
            let parents = decoded
                .parents
                .iter()
                .map(|parent| {
                    ObjectId::from_hex(parent).map_err(|error| {
                        GraphLoadError::Invalid(
                            Report::new(error)
                                .wrap_err(format!("Commit {oid} has an invalid parent ID")),
                        )
                    })
                })
                .collect::<std::result::Result<Box<[_]>, _>>()?;
            pending.extend(
                parents
                    .iter()
                    .copied()
                    .filter(|parent| scheduled.insert(*parent))
                    .map(|parent| (parent, causal_root)),
            );
            assert!(commits.insert(oid, CommitFacts { parents }).is_none());
        }

        let canonical_ids =
            read_canonical_ids(repository, trailer_candidates).map_err(GraphLoadError::Invalid)?;
        Ok(Self { commits, canonical_ids })
    }

    #[cfg(test)]
    fn load_for_test(
        repository: &util::Repo,
        roots: impl IntoIterator<Item = ObjectId>,
    ) -> std::result::Result<Self, TestGraphLoadError> {
        let roots = roots
            .into_iter()
            .map(|oid| ExternalGraphRoot {
                oid,
                first_source_ref: "refs/tags/gherrit/Test/v1".into(),
                later_source_refs: Box::new([]),
            })
            .collect::<Vec<_>>();
        Self::load(repository, &roots).map_err(|error| match error {
            GraphLoadError::MissingAdvertisedRoot(missing) => {
                TestGraphLoadError::MissingAdvertisedRoot { root: missing.root().oid() }
            }
            GraphLoadError::MissingAncestor(missing) => TestGraphLoadError::MissingAncestor {
                oid: missing.oid(),
                root: missing.root().oid(),
            },
            GraphLoadError::Invalid(error) => TestGraphLoadError::Invalid(error),
        })
    }

    fn revision(&self, head: ObjectId) -> Result<Revision> {
        let commit = self
            .commits
            .get(&head)
            .ok_or_else(|| eyre!("Commit {head} is absent from complete graph evidence"))?;
        let first_parent = commit
            .parents
            .first()
            .copied()
            .ok_or_else(|| eyre!("Commit {head} has no first parent"))?;
        Ok(Revision { head, first_parent })
    }

    fn parents(&self, oid: ObjectId) -> &[ObjectId] {
        &self.commits.get(&oid).expect("complete graph contains every revision").parents
    }

    fn canonical_ids(&self, oid: ObjectId) -> &[Box<[u8]>] {
        self.canonical_ids.get(&oid).map(Box::as_ref).unwrap_or_default()
    }

    fn first_reachable(
        &self,
        roots: impl IntoIterator<Item = ObjectId>,
        mut target: impl FnMut(ObjectId) -> bool,
    ) -> Option<ObjectId> {
        let mut pending = VecDeque::from_iter(roots);
        let mut visited = HashSet::new();
        while let Some(oid) = pending.pop_front() {
            if !visited.insert(oid) {
                continue;
            }
            let commit = self
                .commits
                .get(&oid)
                .expect("graph traversal starts within the loaded transitive closure");
            if target(oid) {
                return Some(oid);
            }
            pending.extend(commit.parents.iter().copied());
        }
        None
    }
}

fn message_may_contain_identity(message: &[u8]) -> bool {
    const KEY: &[u8] = b"gherrit-pr-id";
    message.windows(KEY.len()).any(|window| window.eq_ignore_ascii_case(KEY))
}

fn read_canonical_ids(
    repository: &util::Repo,
    commits: impl IntoIterator<Item = ObjectId>,
) -> Result<CanonicalIdsByCommit> {
    read_canonical_ids_with(commits, |batch| {
        let arguments = [
            "log",
            "--no-walk=unsorted",
            "--no-show-signature",
            "--no-notes",
            "--no-decorate",
            "-z",
            GHERRIT_ID_TRAILER_FORMAT,
        ]
        .into_iter()
        .map(ToOwned::to_owned)
        .chain(batch.iter().map(ObjectId::to_string));
        let mut command = util::cmd("git", arguments);
        command.current_dir(repository.workdir().unwrap_or(repository.path()));
        Ok(command.checked_output().wrap_err("Failed to parse commit trailers")?.stdout)
    })
}

fn read_canonical_ids_with(
    commits: impl IntoIterator<Item = ObjectId>,
    mut query: impl FnMut(&[ObjectId]) -> Result<Vec<u8>>,
) -> Result<CanonicalIdsByCommit> {
    let commits = commits.into_iter().collect::<Vec<_>>();
    if commits.iter().copied().collect::<HashSet<_>>().len() != commits.len() {
        bail!("Canonical trailer query contains duplicate commits");
    }
    let mut observed = HashMap::with_capacity(commits.len());
    for batch in commits.chunks(CANONICAL_ID_QUERY_BATCH_LEN) {
        let output = query(batch)?;
        for (oid, identities) in decode_canonical_ids(&output, batch)? {
            assert!(observed.insert(oid, identities).is_none());
        }
    }
    Ok(observed)
}

fn decode_canonical_ids(output: &[u8], requested: &[ObjectId]) -> Result<CanonicalIdsByCommit> {
    let expected = requested.iter().copied().collect::<HashSet<_>>();
    if expected.len() != requested.len() {
        bail!("Canonical trailer query contains duplicate commits");
    }
    let mut observed = HashMap::with_capacity(requested.len());
    let mut fields = output.split(|byte| *byte == 0);
    loop {
        let oid = fields.next().ok_or_else(|| eyre!("Git returned malformed trailer data"))?;
        if oid.is_empty() {
            if fields.next().is_some() {
                bail!("Git returned trailing fields after commit trailer data");
            }
            break;
        }
        let oid = ObjectId::from_hex(oid).wrap_err("Git returned an invalid trailer object ID")?;
        if !expected.contains(&oid) {
            bail!("Git returned trailer data for unrequested commit {oid}");
        }
        let trailers =
            fields.next().ok_or_else(|| eyre!("Git omitted trailer data for commit {oid}"))?;
        let identities = trailers
            .split(|byte| *byte == b'\n')
            .filter_map(gherrit_id_trailer_value)
            .map(|identity| identity.to_vec().into_boxed_slice())
            .collect::<Box<[_]>>();
        if observed.insert(oid, identities).is_some() {
            bail!("Git returned duplicate trailer data for commit {oid}");
        }
    }
    if observed.len() != expected.len() {
        bail!("Git omitted trailer data for one or more requested commits");
    }
    Ok(observed)
}

#[cfg(test)]
mod tests {
    use gix::{ObjectId, prelude::Write as _};
    use tempfile::TempDir;

    use super::{
        super::{
            destination::DefaultBranch,
            remote::{RawExactLocalObservation, decode_for_test},
        },
        *,
    };

    struct TestRepository {
        directory: TempDir,
        writer: gix::Repository,
    }

    impl TestRepository {
        fn new() -> Self {
            let directory = tempfile::tempdir().expect("temporary repository directory");
            let writer = gix::init_bare(directory.path()).expect("initialize bare repository");
            Self { directory, writer }
        }

        fn commit(&self, message: &str, parents: &[ObjectId]) -> ObjectId {
            let signature = gix::actor::Signature {
                name: "GHerrit test".into(),
                email: "test@example.com".into(),
                time: gix::actor::date::Time::new(0, 0),
            };
            self.writer
                .write_object(&gix::objs::Commit {
                    tree: ObjectId::empty_tree(self.writer.object_hash()),
                    parents: parents.iter().copied().collect(),
                    author: signature.clone(),
                    committer: signature,
                    encoding: None,
                    message: message.into(),
                    extra_headers: Vec::new(),
                })
                .expect("write test commit")
                .detach()
        }

        fn malformed_commit_after_parent(&self, parent: ObjectId, tail: &str) -> ObjectId {
            let tree = ObjectId::empty_tree(self.writer.object_hash());
            let bytes = format!("tree {tree}\nparent {parent}\n{tail}");
            self.writer
                .write_buf(gix::object::Kind::Commit, bytes.as_bytes())
                .expect("write malformed test commit")
        }

        fn corrupt_loose_object(&self, oid: ObjectId) {
            let hex = oid.to_string();
            let directory = self.directory.path().join("objects").join(&hex[..2]);
            std::fs::create_dir_all(&directory).expect("create loose-object fanout");
            std::fs::write(directory.join(&hex[2..]), b"not a compressed Git object")
                .expect("write corrupt loose object");
        }

        fn open(&self) -> util::Repo {
            util::Repo::open(self.directory.path().to_str().expect("UTF-8 test path"))
                .expect("open test repository")
        }

        fn trailer_output(
            &self,
            format: &str,
            commits: impl IntoIterator<Item = ObjectId>,
        ) -> Vec<u8> {
            let arguments = [
                "log",
                "--no-walk=unsorted",
                "--no-show-signature",
                "--no-notes",
                "--no-decorate",
                "-z",
                format,
            ]
            .into_iter()
            .map(ToOwned::to_owned)
            .chain(commits.into_iter().map(|oid| oid.to_string()));
            let mut command = util::cmd("git", arguments);
            command.current_dir(self.directory.path());
            let output = command.output().expect("run Git trailer formatter");
            assert!(output.status.success(), "Git trailer formatter failed");
            output.stdout
        }
    }

    fn id(value: &str) -> GherritPrId {
        GherritPrId::from_ref_component(value.as_bytes()).expect("valid test change ID")
    }

    fn default_branch(name: &str, tip: ObjectId) -> DefaultBranch {
        DefaultBranch::new(name.to_owned(), tip).unwrap()
    }

    fn version(value: u64) -> Version {
        Version::new(value).expect("test version is nonzero")
    }

    fn observe(
        id: &GherritPrId,
        default_tip: ObjectId,
        candidate_head: Option<ObjectId>,
        owned_base: Option<ObjectId>,
        versions: &[(u64, ObjectId)],
        marker_target: Option<ObjectId>,
    ) -> RawExactLocalObservation {
        let default = default_branch("main", default_tip);
        let mut output = format!("{default_tip}\trefs/heads/main\n");
        if let Some(head) = candidate_head {
            output.push_str(&format!("{head}\trefs/heads/{}\n", id.as_str()));
        }
        if let Some(base) = owned_base {
            output.push_str(&format!("{base}\trefs/heads/gherrit-bases/{}\n", id.as_str()));
        }
        for (version, target) in versions {
            output.push_str(&format!("{target}\trefs/tags/gherrit/{}/v{version}\n", id.as_str()));
        }
        if let Some(target) = marker_target {
            output.push_str(&format!("{target}\trefs/tags/gherrit/{}/pr\n", id.as_str()));
        }
        decode_for_test(default, std::slice::from_ref(id), [output.as_bytes()]).unwrap()
    }

    fn observe_many(
        ids: &[GherritPrId],
        default_tip: ObjectId,
        records: impl fmt::Display,
    ) -> RawExactLocalObservation {
        observe_many_on(default_branch("main", default_tip), ids, records)
    }

    fn observe_many_on(
        default: DefaultBranch,
        ids: &[GherritPrId],
        records: impl fmt::Display,
    ) -> RawExactLocalObservation {
        let output = format!("{}\trefs/heads/{}\n{records}", default.tip(), default.name());
        decode_for_test(default, ids, [output.as_bytes()]).unwrap()
    }

    fn external_history(
        graph: &CommitGraphEvidence,
        change_id: &GherritPrId,
        published: ObjectId,
        proposed: Revision,
    ) -> ChangeHistory {
        ChangeHistory {
            id: change_id.clone(),
            published: Some(PublishedHistory {
                first: graph.revision(published).expect("external test head is a revision"),
                later: Box::new([]),
                has_pull_request_marker: false,
            }),
            proposed,
        }
    }

    fn normalize(
        repository: &TestRepository,
        id: &GherritPrId,
        default_tip: ObjectId,
        candidate_head: Option<ObjectId>,
        owned_base: Option<ObjectId>,
        versions: &[(u64, ObjectId)],
        marker_target: Option<ObjectId>,
    ) -> Result<(GherritPrId, Option<PublishedHistory>)> {
        let observed =
            observe(id, default_tip, candidate_head, owned_base, versions, marker_target);
        SourcedPreparedHistory::normalize_for_test(
            &repository.open(),
            observed.iter().next().unwrap(),
        )
    }

    #[test]
    fn normalizes_exactly_wholly_absent_or_complete_remote_shapes() {
        let repository = TestRepository::new();
        let root = repository.commit("root", &[]);
        let head = repository.commit("head", &[root]);
        let change_id = id("Gone");

        for has_head in [false, true] {
            for has_base in [false, true] {
                for has_versions in [false, true] {
                    for has_marker in [false, true] {
                        let one_version = [(1, head)];
                        let versions: &[(u64, ObjectId)] =
                            if has_versions { &one_version } else { &[] };
                        let result = normalize(
                            &repository,
                            &change_id,
                            root,
                            has_head.then_some(head),
                            has_base.then_some(root),
                            versions,
                            has_marker.then_some(head),
                        );
                        let valid = matches!(
                            (has_head, has_base, has_versions, has_marker),
                            (false, false, false, false)
                                | (true, true, true, false)
                                | (true, true, true, true)
                        );
                        assert_eq!(
                            result.is_ok(),
                            valid,
                            "head={has_head}, base={has_base}, versions={has_versions}, marker={has_marker}"
                        );
                        if valid {
                            let normalized = result.unwrap();
                            assert_eq!(normalized.0, change_id);
                            assert_eq!(normalized.1.is_some(), has_versions);
                            if let Some(history) = normalized.1.as_ref() {
                                assert_eq!(history.has_pull_request_marker(), has_marker);
                            }
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn derives_versions_from_positions_and_preserves_all_repeats() {
        let repository = TestRepository::new();
        let root = repository.commit("root", &[]);
        let a = repository.commit("A", &[root]);
        let b = repository.commit("B", &[root]);
        let change_id = id("Gone");

        for expected in [vec![a, a], vec![a, b, a]] {
            let versions = expected
                .iter()
                .enumerate()
                .map(|(index, head)| ((index + 1) as u64, *head))
                .collect::<Vec<_>>();
            let normalized = normalize(
                &repository,
                &change_id,
                root,
                expected.last().copied(),
                Some(root),
                &versions,
                None,
            )
            .expect("repeated literal history is valid");
            let history = normalized.1.unwrap();
            assert_eq!(history.iter().len(), expected.len());
            assert_eq!(history.versioned().len(), expected.len());
            assert_eq!(
                history
                    .versioned()
                    .map(|(version, revision)| (version.get(), revision.head()))
                    .collect::<Vec<_>>(),
                expected
                    .iter()
                    .enumerate()
                    .map(|(index, head)| ((index + 1) as u64, *head))
                    .collect::<Vec<_>>()
            );
            let current = history.current();
            assert_eq!(current.number().get(), expected.len() as u64);
            assert_eq!(current.revision().head(), *expected.last().unwrap());
            assert_eq!(
                history.iter().rev().map(Revision::head).collect::<Vec<_>>(),
                expected.iter().rev().copied().collect::<Vec<_>>()
            );
            assert_eq!(
                history
                    .versioned()
                    .rev()
                    .map(|(version, revision)| (version.get(), revision.head()))
                    .collect::<Vec<_>>(),
                expected
                    .iter()
                    .enumerate()
                    .rev()
                    .map(|(index, head)| ((index + 1) as u64, *head))
                    .collect::<Vec<_>>()
            );
        }
    }

    #[test]
    fn accepts_only_prefixes_of_the_bounded_raw_version_domain() {
        let repository = TestRepository::new();
        let root = repository.commit("root", &[]);
        let revisions = (1..=4)
            .map(|index| repository.commit(&format!("head {index}"), &[root]))
            .collect::<Vec<_>>();
        let change_id = id("Gone");

        for mask in 1_u8..1 << revisions.len() {
            let observed = revisions
                .iter()
                .enumerate()
                .filter(|(index, _)| mask & (1 << index) != 0)
                .map(|(index, oid)| ((index + 1) as u64, *oid))
                .collect::<Vec<_>>();
            let current = observed.last().unwrap().1;
            let result = normalize(
                &repository,
                &change_id,
                root,
                Some(current),
                Some(root),
                &observed,
                None,
            );
            let contiguous = mask == (1 << observed.len()) - 1;
            assert_eq!(result.is_ok(), contiguous, "mask={mask:04b}");
        }
    }

    #[test]
    fn validates_all_raw_oid_facts_before_loading_and_caches_repeated_heads() {
        let repository = TestRepository::new();
        let root = repository.commit("root", &[]);
        let a = repository.commit("A", &[root]);
        let b = repository.commit("B", &[root]);
        let missing = ObjectId::from_bytes_or_panic(&[0x44; 20]);
        let change_id = id("Gone");

        let gap = normalize(
            &repository,
            &change_id,
            root,
            Some(b),
            Some(root),
            &[(1, missing), (3, b)],
            None,
        )
        .unwrap_err();
        assert!(gap.to_string().contains("noncontiguous version tags"));

        let head = normalize(
            &repository,
            &change_id,
            root,
            Some(root),
            Some(root),
            &[(1, missing), (2, b)],
            None,
        )
        .unwrap_err();
        assert!(head.to_string().contains("head does not match"));

        let marker = normalize(
            &repository,
            &change_id,
            root,
            Some(b),
            Some(root),
            &[(1, missing), (2, b)],
            Some(root),
        )
        .unwrap_err();
        assert!(marker.to_string().contains("does not target a published version head"));

        let graph = CommitGraphEvidence::load_for_test(&repository.open(), [a, a, b, a]).unwrap();
        let resolved =
            [a, a, b, a].into_iter().map(|head| graph.revision(head).unwrap()).collect::<Vec<_>>();
        assert_eq!(
            resolved.iter().map(|revision| revision.head()).collect::<Vec<_>>(),
            [a, a, b, a]
        );
        assert_eq!(graph.commits.len(), 3, "A, B, and their shared root load once");
    }

    #[test]
    fn validates_then_discards_mutable_head_and_owned_base_facts() {
        let repository = TestRepository::new();
        let root = repository.commit("root", &[]);
        let other_base = repository.commit("other base", &[]);
        let head = repository.commit("head", &[root]);
        let other_head = repository.commit("other head", &[root]);
        let change_id = id("Gone");
        let versions = [(1, head)];

        let head_error =
            normalize(&repository, &change_id, root, Some(other_head), Some(root), &versions, None)
                .unwrap_err();
        assert!(head_error.to_string().contains("head does not match"));

        let base_error =
            normalize(&repository, &change_id, root, Some(head), Some(other_base), &versions, None)
                .unwrap_err();
        assert!(base_error.to_string().contains("owned base does not match"));

        let normalized =
            normalize(&repository, &change_id, root, Some(head), Some(root), &versions, None)
                .unwrap();
        let history = normalized.1.unwrap();
        assert_eq!(history.current().revision(), Revision { head, first_parent: root });
    }

    #[test]
    fn validates_marker_target_against_every_head_then_retains_only_presence() {
        let repository = TestRepository::new();
        let root = repository.commit("root", &[]);
        let a = repository.commit("A", &[root]);
        let b = repository.commit("B", &[root]);
        let unrelated = repository.commit("unrelated", &[root]);
        let missing = ObjectId::from_bytes_or_panic(&[0x55; 20]);
        let change_id = id("Gone");
        let versions = [(1, a), (2, b), (3, a)];

        for marker in [a, b] {
            let normalized = normalize(
                &repository,
                &change_id,
                root,
                Some(a),
                Some(root),
                &versions,
                Some(marker),
            )
            .expect("marker names a published head");
            assert!(normalized.1.unwrap().has_pull_request_marker());
        }
        for marker in [root, unrelated, missing] {
            let error = normalize(
                &repository,
                &change_id,
                root,
                Some(a),
                Some(root),
                &versions,
                Some(marker),
            )
            .unwrap_err();
            assert!(error.to_string().contains("does not target a published version head"));
        }
    }

    #[test]
    fn rejects_missing_noncommit_and_parentless_version_targets() {
        let repository = TestRepository::new();
        let root = repository.commit("root", &[]);
        let missing = ObjectId::from_bytes_or_panic(&[0x66; 20]);
        let blob = repository.writer.write_blob(b"not a commit").unwrap().detach();
        let change_id = id("Gone");

        for target in [missing, blob, root] {
            let result = normalize(
                &repository,
                &change_id,
                root,
                Some(target),
                Some(root),
                &[(1, target)],
                None,
            );
            assert!(result.is_err(), "accepted non-revision target {target}");
        }
    }

    #[test]
    fn distinguishes_absent_objects_from_object_database_failures() {
        let repository = TestRepository::new();
        let missing = ObjectId::from_bytes_or_panic(&[0x68; 20]);
        let missing_error =
            CommitGraphEvidence::load_for_test(&repository.open(), [missing]).unwrap_err();
        assert_eq!(
            missing_error.to_string(),
            format!("Advertised graph-root commit {missing} is missing")
        );

        let corrupt = ObjectId::from_bytes_or_panic(&[0x69; 20]);
        repository.corrupt_loose_object(corrupt);
        let read_error =
            CommitGraphEvidence::load_for_test(&repository.open(), [corrupt]).unwrap_err();
        assert_eq!(read_error.to_string(), format!("Failed to read object {corrupt}"));
        assert!(!format!("{read_error:?}").contains("Commit object {corrupt} is missing"));
    }

    #[test]
    fn rejects_malformed_commit_tails_after_a_valid_first_parent() {
        let repository = TestRepository::new();
        let parent = repository.commit("parent", &[]);
        let missing_committer = repository.malformed_commit_after_parent(
            parent,
            "author GHerrit test <test@example.com> 0 +0000\n",
        );
        let malformed_author = repository.malformed_commit_after_parent(
            parent,
            "author malformed\ncommitter GHerrit test <test@example.com> 0 +0000\n\nmessage\n",
        );
        let change_id = id("Gone");

        for head in [missing_committer, malformed_author] {
            let commit = repository.writer.find_object(head).unwrap().try_into_commit().unwrap();
            assert_eq!(
                commit.parent_ids().next().unwrap().detach(),
                parent,
                "fixture must expose the valid first parent through the lossy iterator"
            );

            let error = normalize(
                &repository,
                &change_id,
                parent,
                Some(head),
                Some(parent),
                &[(1, head)],
                None,
            )
            .expect_err("a valid-looking first parent must not launder a malformed commit");
            assert!(format!("{error:?}").contains("malformed encoding"));
        }
    }

    #[test]
    fn complete_graph_loading_requires_the_literal_parent() {
        let repository = TestRepository::new();
        let default_tip = repository.commit("default", &[]);
        let missing_parent = ObjectId::from_bytes_or_panic(&[0x77; 20]);
        let head = repository.commit("head", &[missing_parent]);
        let change_id = id("Gone");
        let error = normalize(
            &repository,
            &change_id,
            default_tip,
            Some(head),
            Some(missing_parent),
            &[(1, head)],
            None,
        )
        .expect_err("complete graph evidence must include the literal parent");
        assert!(error.to_string().contains(&missing_parent.to_string()));
    }

    #[test]
    fn retains_each_rebased_parent_and_uses_parent_zero_for_merges() {
        let repository = TestRepository::new();
        let root = repository.commit("root", &[]);
        let rebased_parent = repository.commit("rebased parent", &[root]);
        let first = repository.commit("first", &[root]);
        let rebased = repository.commit("rebased", &[rebased_parent]);
        let change_id = id("Gone");

        let normalized = normalize(
            &repository,
            &change_id,
            root,
            Some(rebased),
            Some(rebased_parent),
            &[(1, first), (2, rebased)],
            None,
        )
        .expect("each version retains its own literal parent");
        assert_eq!(
            normalized.1.unwrap().iter().map(Revision::first_parent).collect::<Vec<_>>(),
            [root, rebased_parent]
        );

        let second_parent = repository.commit("second parent", &[root]);
        let merge = repository.commit("merge", &[rebased_parent, second_parent]);
        normalize(
            &repository,
            &change_id,
            root,
            Some(merge),
            Some(rebased_parent),
            &[(1, merge)],
            None,
        )
        .expect("merge owned base is its literal first parent");
        let error = normalize(
            &repository,
            &change_id,
            root,
            Some(merge),
            Some(second_parent),
            &[(1, merge)],
            None,
        )
        .unwrap_err();
        assert!(error.to_string().contains("owned base does not match"));
    }

    #[test]
    fn request_identity_is_bound_while_all_commit_trailer_text_is_opaque() {
        let repository = TestRepository::new();
        let root = repository.commit("root", &[]);
        let messages = [
            "no trailer",
            "subject\n\ngherrit-pr-id: Requested\n",
            "subject\n\ngherrit-pr-id: Other\n",
            "subject\n\ngherrit-pr-id: Requested\ngherrit-pr-id: Other\n",
            "subject\n\ngherrit-pr-id: Other\n\nThis is body text, not a trailer.\n",
            "subject\n\ngherrit-pr-id: Other\n continuation\n",
        ];
        let requested = id("Requested");

        for message in messages {
            let head = repository.commit(message, &[root]);
            let observed = observe(&requested, root, Some(head), Some(root), &[(1, head)], None);
            let raw = observed.iter().next().unwrap();
            let first =
                SourcedPreparedHistory::normalize_for_test(&repository.open(), raw).unwrap();
            let second =
                SourcedPreparedHistory::normalize_for_test(&repository.open(), raw).unwrap();
            assert_eq!(first.0, requested);
            assert_eq!(second.0, requested);
            assert_eq!(raw.id(), &requested, "normalization must borrow raw evidence");
        }
    }

    #[test]
    fn aggregate_retains_root_aliases_and_exposes_only_whole_validated_history() {
        let repository = TestRepository::new();
        let root = repository.commit("root", &[]);
        let published = repository.commit("published\n\ngherrit-pr-id: Gone\n", &[root]);
        let proposal = repository.commit("proposal\n\ngherrit-pr-id: Gone\n", &[root]);
        let change_id = id("Gone");
        let local = LocalStack::for_history_test(
            default_branch("main", root),
            [(change_id.clone(), proposal, root)],
        );
        let observation = observe(
            &change_id,
            root,
            Some(published),
            Some(root),
            &[(1, published)],
            Some(published),
        );

        let prepared = StructurallyPreparedHistories::prepare_raw(&observation, &local).unwrap();
        assert_eq!(prepared.graph_roots().as_ref(), [published]);
        assert_eq!(
            prepared.external_roots().next().unwrap().source_refs().collect::<Vec<_>>(),
            ["refs/tags/gherrit/Gone/v1"]
        );

        let graph =
            CommitGraphEvidence::load_for_test(&repository.open(), prepared.graph_roots()).unwrap();
        let mut histories = prepared.validate(&graph).unwrap().into_vec();
        let validated = histories.pop().unwrap();

        assert!(histories.is_empty());
        assert_eq!(validated.id(), &change_id);
        assert_eq!(validated.published_len(), 1);
        assert_eq!(
            validated.published_versions().collect::<Vec<_>>(),
            [(version(1), Revision { head: published, first_parent: root })]
        );
        assert_eq!(
            validated.published_current(),
            Some(CurrentVersion {
                number: version(1),
                revision: Revision { head: published, first_parent: root },
            })
        );
        assert!(validated.has_pull_request_marker());
        assert_eq!(validated.proposed(), Revision { head: proposal, first_parent: root });
        assert!(validated.needs_publication());
        assert_eq!(
            validated.projected_versions().collect::<Vec<_>>(),
            [
                (version(1), Revision { head: published, first_parent: root }),
                (version(2), Revision { head: proposal, first_parent: root }),
            ]
        );
        assert_eq!(
            validated.projected_current(),
            CurrentVersion {
                number: version(2),
                revision: Revision { head: proposal, first_parent: root },
            }
        );
        assert!(validated.contains_published_head(published));
        assert!(!validated.contains_published_head(proposal));
        assert!(validated.contains_published_first_parent(root));
    }

    #[test]
    fn aggregate_rejects_an_unrelated_complete_graph_without_panicking() {
        let repository = TestRepository::new();
        let root = repository.commit("root", &[]);
        let unrelated = repository.commit("unrelated", &[root]);
        let published = repository.commit("published\n\ngherrit-pr-id: Gone\n", &[root]);
        let proposal = repository.commit("proposal\n\ngherrit-pr-id: Gone\n", &[root]);
        let change_id = id("Gone");
        let local = LocalStack::for_history_test(
            default_branch("main", root),
            [(change_id.clone(), proposal, root)],
        );
        let observation =
            observe(&change_id, root, Some(published), Some(root), &[(1, published)], None);
        let prepared = StructurallyPreparedHistories::prepare_raw(&observation, &local).unwrap();
        let unrelated_graph =
            CommitGraphEvidence::load_for_test(&repository.open(), [unrelated]).unwrap();

        let error = prepared.validate(&unrelated_graph).unwrap_err();
        let causes = error.chain().map(ToString::to_string).collect::<Vec<_>>();

        assert!(causes.iter().any(|cause| cause.contains(&published.to_string())));
        assert!(causes.iter().any(|cause| cause.contains("absent from complete graph evidence")));
    }

    #[test]
    fn validated_projection_covers_absent_v1_and_repeated_a_b_a_no_op() {
        let repository = TestRepository::new();
        let root = repository.commit("root", &[]);
        let a = repository.commit("A\n\ngherrit-pr-id: Gone\n", &[root]);
        let b = repository.commit("B\n\ngherrit-pr-id: Gone\n", &[root]);
        let change_id = id("Gone");
        let local = LocalStack::for_history_test(
            default_branch("main", root),
            [(change_id.clone(), a, root)],
        );

        let absent = observe(&change_id, root, None, None, &[], None);
        let absent = StructurallyPreparedHistories::prepare_raw(&absent, &local).unwrap();
        assert!(absent.graph_roots().is_empty());
        let empty_graph =
            CommitGraphEvidence::load_for_test(&repository.open(), absent.graph_roots()).unwrap();
        let mut histories = absent.validate(&empty_graph).unwrap().into_vec();
        let absent = histories.pop().unwrap();
        assert!(absent.needs_publication());
        assert_eq!(
            absent.projected_versions().collect::<Vec<_>>(),
            [(version(1), Revision { head: a, first_parent: root })]
        );
        assert_eq!(absent.projected_current().number(), version(1));

        let repeated =
            observe(&change_id, root, Some(a), Some(root), &[(1, a), (2, b), (3, a)], None);
        let repeated = StructurallyPreparedHistories::prepare_raw(&repeated, &local).unwrap();
        assert_eq!(repeated.graph_roots().as_ref(), [b]);
        let graph =
            CommitGraphEvidence::load_for_test(&repository.open(), repeated.graph_roots()).unwrap();
        let mut histories = repeated.validate(&graph).unwrap().into_vec();
        let repeated = histories.pop().unwrap();

        assert!(!repeated.needs_publication());
        assert_eq!(repeated.published_len(), 3);
        assert_eq!(
            repeated.projected_versions().collect::<Vec<_>>(),
            [
                (version(1), Revision { head: a, first_parent: root }),
                (version(2), Revision { head: b, first_parent: root }),
                (version(3), Revision { head: a, first_parent: root }),
            ]
        );
        assert_eq!(
            repeated.projected_current(),
            CurrentVersion {
                number: version(3),
                revision: Revision { head: a, first_parent: root },
            }
        );
    }

    #[test]
    fn whole_set_structural_preflight_precedes_any_object_loading() {
        let repository = TestRepository::new();
        let root = repository.commit("root", &[]);
        let proposal_a = repository.commit("proposal A\n\ngherrit-pr-id: A\n", &[root]);
        let proposal_b = repository.commit("proposal B\n\ngherrit-pr-id: B\n", &[proposal_a]);
        let missing_a = ObjectId::from_bytes_or_panic(&[0xa1; 20]);
        let missing_b1 = ObjectId::from_bytes_or_panic(&[0xb1; 20]);
        let missing_b3 = ObjectId::from_bytes_or_panic(&[0xb3; 20]);
        let ids = [id("A"), id("B")];
        let local = LocalStack::for_history_test(
            default_branch("main", root),
            [(ids[0].clone(), proposal_a, root), (ids[1].clone(), proposal_b, proposal_a)],
        );
        let records = format!(
            "{missing_a}\trefs/heads/A\n\
             {root}\trefs/heads/gherrit-bases/A\n\
             {missing_a}\trefs/tags/gherrit/A/v1\n\
             {missing_b3}\trefs/heads/B\n\
             {proposal_a}\trefs/heads/gherrit-bases/B\n\
             {missing_b1}\trefs/tags/gherrit/B/v1\n\
             {missing_b3}\trefs/tags/gherrit/B/v3\n"
        );
        let observation = observe_many(&ids, root, records);

        let error = StructurallyPreparedHistories::prepare_raw(&observation, &local)
            .err()
            .expect("the later raw history has a structural gap");

        assert!(error.to_string().contains("'B'"));
        assert!(error.to_string().contains("noncontiguous version tags"));
    }

    #[test]
    fn aggregate_rejects_default_name_or_tip_mismatch_before_graph_loading() {
        let root = ObjectId::from_bytes_or_panic(&[0x31; 20]);
        let other_tip = ObjectId::from_bytes_or_panic(&[0x32; 20]);
        let proposal = ObjectId::from_bytes_or_panic(&[0x33; 20]);
        let change_id = id("Gone");
        let local_default = default_branch("main", root);
        let local = LocalStack::for_history_test(
            local_default.clone(),
            [(change_id.clone(), proposal, root)],
        );

        for observed_default in [default_branch("main", other_tip), default_branch("trunk", root)] {
            let observation =
                observe_many_on(observed_default, std::slice::from_ref(&change_id), "");
            let error = StructurallyPreparedHistories::prepare_raw(&observation, &local)
                .err()
                .expect("the exact default path origin differs");
            assert!(error.to_string().contains("does not match the local stack's default branch"));
        }

        let observation = observe_many_on(local_default, std::slice::from_ref(&change_id), "");
        StructurallyPreparedHistories::prepare_raw(&observation, &local)
            .expect("the exact same default path origin is accepted");
    }

    #[test]
    fn graph_roots_and_missing_errors_follow_semantic_discovery_order() {
        let repository = TestRepository::new();
        let root = repository.commit("root", &[]);
        let proposal_a = repository.commit("proposal A\n\ngherrit-pr-id: A\n", &[root]);
        let proposal_b = repository.commit("proposal B\n\ngherrit-pr-id: B\n", &[proposal_a]);
        let published_a2 = repository.commit("published A2\n\ngherrit-pr-id: A\n", &[root]);
        let published_a1 = proposal_b;
        let published_b = proposal_b;
        let ids = [id("A"), id("B")];
        let local = LocalStack::for_history_test(
            default_branch("main", root),
            [(ids[0].clone(), proposal_a, root), (ids[1].clone(), proposal_b, proposal_a)],
        );
        let records = format!(
            "{proposal_b}\trefs/heads/A\n\
             {proposal_a}\trefs/heads/gherrit-bases/A\n\
             {published_a1}\trefs/tags/gherrit/A/v1\n\
             {published_a2}\trefs/tags/gherrit/A/v2\n\
             {published_a1}\trefs/tags/gherrit/A/v3\n\
             {published_b}\trefs/heads/B\n\
             {proposal_a}\trefs/heads/gherrit-bases/B\n\
             {published_b}\trefs/tags/gherrit/B/v1\n"
        );
        let observation = observe_many(&ids, root, records);
        let prepared = StructurallyPreparedHistories::prepare_raw(&observation, &local).unwrap();

        assert_eq!(prepared.graph_roots().as_ref(), [proposal_b, published_a2]);
        let roots = prepared.external_roots().collect::<Vec<_>>();
        assert_eq!(
            roots[0].source_refs().collect::<Vec<_>>(),
            ["refs/tags/gherrit/A/v1", "refs/tags/gherrit/A/v3"]
        );
        assert_eq!(roots[1].source_refs().collect::<Vec<_>>(), ["refs/tags/gherrit/A/v2"]);

        let first_missing = ObjectId::from_bytes_or_panic(&[0xc1; 20]);
        let second_missing = ObjectId::from_bytes_or_panic(&[0xc2; 20]);
        assert!(matches!(
            CommitGraphEvidence::load_for_test(&repository.open(), [first_missing, second_missing]),
            Err(TestGraphLoadError::MissingAdvertisedRoot { root }) if root == first_missing
        ));

        let first_parent = ObjectId::from_bytes_or_panic(&[0xd1; 20]);
        let second_parent = ObjectId::from_bytes_or_panic(&[0xd2; 20]);
        let merge = repository.commit("merge", &[first_parent, second_parent]);
        assert!(matches!(
            CommitGraphEvidence::load_for_test(&repository.open(), [merge]),
            Err(TestGraphLoadError::MissingAncestor { oid, root })
                if oid == first_parent && root == merge
        ));

        let shared_missing = ObjectId::from_bytes_or_panic(&[0xd5; 20]);
        let first_causal_root = repository.commit("first causal root", &[shared_missing]);
        let second_causal_root = repository.commit("second causal root", &[shared_missing]);
        assert!(matches!(
            CommitGraphEvidence::load_for_test(
                &repository.open(),
                [first_causal_root, second_causal_root],
            ),
            Err(TestGraphLoadError::MissingAncestor { oid, root })
                if oid == shared_missing && root == first_causal_root
        ));

        let missing_parent = ObjectId::from_bytes_or_panic(&[0xd3; 20]);
        let root_with_missing_parent = repository.commit("root before ancestry", &[missing_parent]);
        let missing_later_root = ObjectId::from_bytes_or_panic(&[0xd4; 20]);
        assert!(matches!(
            CommitGraphEvidence::load_for_test(
                &repository.open(),
                [root_with_missing_parent, missing_later_root],
            ),
            Err(TestGraphLoadError::MissingAdvertisedRoot { root })
                if root == missing_later_root
        ));
    }

    #[test]
    fn sparse_keyed_trailer_loading_matches_unfiltered_git_formatting() {
        const UNFILTERED_FORMAT: &str = "--format=tformat:%H%x00%(trailers:only,unfold)";

        let repository = TestRepository::new();
        let root = repository.commit("root", &[]);
        let messages = [
            "subject\n\ngherrit-pr-id: Gone\n",
            "subject\n\ngherrit-pr-id: Other\n",
            "subject\n\nGherrit-Pr-Id: Gone\n",
            "subject\n\ngherrit-pr-id: Gone\n continuation\n",
            "subject\n\ngherrit-pr-id: Gone\n\nbody after the trailer-looking line\n",
            "subject\n\nReviewed-by: Person <person@example.com>\n",
            "subject\n\nNote: mentions gherrit-pr-id in an unrelated value\n",
            "subject\n\ngherrit-pr-id: Gone\ngherrit-pr-id: Gone\n",
            "subject\n\nReviewed-by: Person <person@example.com>\ngherrit-pr-id: Gone\n",
        ];
        let commits =
            messages.iter().map(|message| repository.commit(message, &[root])).collect::<Vec<_>>();
        let unfiltered = decode_canonical_ids(
            &repository.trailer_output(UNFILTERED_FORMAT, commits.iter().copied()),
            &commits,
        )
        .unwrap();
        let keyed = read_canonical_ids(&repository.open(), commits.iter().copied()).unwrap();
        let graph = CommitGraphEvidence::load_for_test(&repository.open(), commits.iter().copied())
            .unwrap();
        let proposal = repository.commit("proposal\n\ngherrit-pr-id: Gone\n", &[root]);
        let proposed = Revision { head: proposal, first_parent: root };

        for commit in &commits {
            assert_eq!(keyed.get(commit), unfiltered.get(commit), "commit={commit}");
            assert_eq!(graph.canonical_ids(*commit), unfiltered[commit].as_ref());
        }
        assert!(message_may_contain_identity(messages[2].as_bytes()));
        assert!(graph.canonical_ids.contains_key(&commits[2]));
        assert!(!message_may_contain_identity(messages[5].as_bytes()));
        assert!(!graph.canonical_ids.contains_key(&commits[5]));

        let change_id = id("Gone");
        let accepted = [true, false, true, false, false, false, false, false, true];
        for (commit, accepted) in commits.iter().zip(accepted) {
            let result = external_history(&graph, &change_id, *commit, proposed).validate(&graph);
            assert_eq!(result.is_ok(), accepted, "commit={commit}");
        }
    }

    #[test]
    fn canonical_trailer_queries_cover_zero_one_and_batch_boundaries() {
        let commits =
            (1..=121).map(|byte| ObjectId::from_bytes_or_panic(&[byte; 20])).collect::<Vec<_>>();

        for count in [0, 1, 120, 121] {
            let mut calls = Vec::new();
            let observed = read_canonical_ids_with(commits[..count].iter().copied(), |batch| {
                calls.push(batch.to_vec());
                Ok(batch
                    .iter()
                    .map(|oid| format!("{oid}\0gherrit-pr-id: Batch\n\0"))
                    .collect::<String>()
                    .into_bytes())
            })
            .unwrap();
            let expected_calls = commits[..count]
                .chunks(CANONICAL_ID_QUERY_BATCH_LEN)
                .map(<[ObjectId]>::to_vec)
                .collect::<Vec<_>>();

            assert_eq!(calls, expected_calls);
            assert_eq!(observed.len(), count);
            for commit in &commits[..count] {
                assert_eq!(observed[commit].len(), 1);
                assert_eq!(observed[commit][0].as_ref(), b"Batch");
            }
        }
    }

    #[test]
    fn canonical_trailer_decoder_requires_exact_nul_framing_and_coverage() {
        let a = ObjectId::from_bytes_or_panic(&[0xe1; 20]);
        let b = ObjectId::from_bytes_or_panic(&[0xe2; 20]);
        let valid = format!("{a}\0gherrit-pr-id: A\nOther: ignored\n\0{b}\0\0");
        let decoded = decode_canonical_ids(valid.as_bytes(), &[a, b]).unwrap();
        assert_eq!(decoded[&a].len(), 1);
        assert_eq!(decoded[&a][0].as_ref(), b"A");
        assert!(decoded[&b].is_empty());

        let duplicate = format!("{a}\0\0{a}\0\0");
        assert!(
            decode_canonical_ids(duplicate.as_bytes(), &[a])
                .unwrap_err()
                .to_string()
                .contains("duplicate trailer data")
        );
        assert!(
            decode_canonical_ids(valid.as_bytes(), &[a])
                .unwrap_err()
                .to_string()
                .contains("unrequested commit")
        );
        assert!(
            decode_canonical_ids(format!("{a}\0\0").as_bytes(), &[a, b])
                .unwrap_err()
                .to_string()
                .contains("omitted trailer data")
        );
        assert!(
            decode_canonical_ids(format!("{a}\0\0\0").as_bytes(), &[a])
                .unwrap_err()
                .to_string()
                .contains("trailing fields")
        );
        assert!(
            decode_canonical_ids(a.to_string().as_bytes(), &[a])
                .unwrap_err()
                .to_string()
                .contains("omitted trailer data")
        );
        assert!(decode_canonical_ids(b"not-an-oid\0\0", &[a]).is_err());
        assert!(
            decode_canonical_ids(format!("{a}\0\0").as_bytes(), &[a, a])
                .unwrap_err()
                .to_string()
                .contains("duplicate commits")
        );
    }

    #[test]
    fn validation_checks_proper_ancestry_through_every_parent() {
        let repository = TestRepository::new();
        let root = repository.commit("root", &[]);
        let proposal = repository.commit("proposal\n\ngherrit-pr-id: Gone\n", &[root]);
        let first_parent_head = repository.commit("head\n\ngherrit-pr-id: Gone\n", &[proposal]);
        let merge_head = repository.commit("merge\n\ngherrit-pr-id: Gone\n", &[root, proposal]);
        let change_id = id("Gone");
        let local = LocalStack::for_history_test(
            default_branch("main", root),
            [(change_id.clone(), proposal, root)],
        );

        for (head, owned_base) in [(first_parent_head, proposal), (merge_head, root)] {
            let observation =
                observe(&change_id, root, Some(head), Some(owned_base), &[(1, head)], None);
            let prepared =
                StructurallyPreparedHistories::prepare_raw(&observation, &local).unwrap();
            assert_eq!(prepared.graph_roots().as_ref(), [head]);
            let graph =
                CommitGraphEvidence::load_for_test(&repository.open(), prepared.graph_roots())
                    .unwrap();
            let error = prepared.validate(&graph).unwrap_err();
            assert!(error.to_string().contains("Proper ancestry"), "head={head}");
            assert!(error.to_string().contains(&proposal.to_string()));
        }
    }

    #[test]
    fn direct_external_head_identity_error_precedes_ancestry_error() {
        let repository = TestRepository::new();
        let root = repository.commit("root", &[]);
        let duplicate = repository.commit("ancestor\n\ngherrit-pr-id: Gone\n", &[root]);
        let wrong = repository.commit("wrong\n\ngherrit-pr-id: Other\n", &[duplicate]);
        let proposal = repository.commit("proposal\n\ngherrit-pr-id: Gone\n", &[root]);
        let change_id = id("Gone");
        let local = LocalStack::for_history_test(
            default_branch("main", root),
            [(change_id.clone(), proposal, root)],
        );
        let observation =
            observe(&change_id, root, Some(wrong), Some(duplicate), &[(1, wrong)], None);
        let prepared = StructurallyPreparedHistories::prepare_raw(&observation, &local).unwrap();
        assert_eq!(prepared.graph_roots().as_ref(), [wrong]);
        let graph =
            CommitGraphEvidence::load_for_test(&repository.open(), prepared.graph_roots()).unwrap();
        let error = prepared.validate(&graph).unwrap_err();

        assert!(error.to_string().contains("must have exactly one"));
        assert!(error.to_string().contains(&wrong.to_string()));
        assert!(!error.to_string().contains("Proper ancestry"));
    }

    #[test]
    fn union_reachability_matches_all_two_through_five_node_dags() {
        for node_count in 2_usize..=5 {
            let nodes = (1..=node_count)
                .map(|byte| ObjectId::from_bytes_or_panic(&[byte as u8; 20]))
                .collect::<Vec<_>>();
            let edge_count = node_count * (node_count - 1) / 2;
            for edge_mask in 0_usize..1 << edge_count {
                let mut bit = 0;
                let mut parent_indices = vec![Vec::new(); node_count];
                for (child, parents) in parent_indices.iter_mut().enumerate() {
                    for parent in 0..child {
                        if edge_mask & (1 << bit) != 0 {
                            parents.push(parent);
                        }
                        bit += 1;
                    }
                }
                let commits = nodes
                    .iter()
                    .enumerate()
                    .map(|(index, oid)| {
                        let parents =
                            parent_indices[index].iter().map(|parent| nodes[*parent]).collect();
                        (*oid, CommitFacts { parents })
                    })
                    .collect();
                let graph = CommitGraphEvidence { commits, canonical_ids: HashMap::new() };

                let mut closure = vec![vec![false; node_count]; node_count];
                for index in 0..node_count {
                    closure[index][index] = true;
                    for parent in &parent_indices[index] {
                        closure[index][*parent] = true;
                    }
                }
                for via in 0..node_count {
                    for source in 0..node_count {
                        for target in 0..node_count {
                            closure[source][target] |= closure[source][via] && closure[via][target];
                        }
                    }
                }

                for root_mask in 1_usize..1 << node_count {
                    let roots = nodes
                        .iter()
                        .enumerate()
                        .filter(|(index, _)| root_mask & (1 << index) != 0)
                        .map(|(_, oid)| *oid)
                        .collect::<Vec<_>>();
                    for target_mask in 0_usize..1 << node_count {
                        let expected = (0..node_count).any(|source| {
                            root_mask & (1 << source) != 0
                                && (0..node_count).any(|target| {
                                    target_mask & (1 << target) != 0 && closure[source][target]
                                })
                        });
                        let actual = graph
                            .first_reachable(roots.iter().copied(), |oid| {
                                let index = nodes.iter().position(|node| *node == oid).unwrap();
                                target_mask & (1 << index) != 0
                            })
                            .is_some();
                        assert_eq!(
                            actual, expected,
                            "nodes={node_count}, edges={edge_mask:b}, roots={root_mask:b}, targets={target_mask:b}"
                        );
                    }
                }
            }
        }
    }
}

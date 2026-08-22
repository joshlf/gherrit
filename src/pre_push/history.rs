//! Literal commit evidence and normalized publication history.
//!
//! Raw remote refs can be absent, incomplete, or contradictory. This module
//! turns only an absent representation or one complete published history into
//! a domain value. A local value must then be coupled to one literal proposal;
//! an existing nonlocal value must be nonempty. Either path consumes the whole
//! value during validation before planning can inspect any revision. A
//! revision always comes from an actual commit and therefore cannot pair an
//! arbitrary head with an arbitrary first parent.

use std::{
    collections::{HashMap, HashSet},
    fmt,
};

use color_eyre::eyre::{Context as _, Report, Result, bail, eyre};
use gix::ObjectId;

use super::{
    local::{GherritPrId, LocalChange},
    remote::ObservedChangeHistory,
    version::Version,
};
use crate::util::{self, CommandExt as _};

/// One head and the literal first parent encoded by that head commit.
///
/// The fields are private because an arbitrary pair does not describe a real
/// revision. Repository-wide identity and reachability checks are separate:
/// they concern a collection of revisions rather than one commit.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub(super) struct Revision {
    head: ObjectId,
    first_parent: ObjectId,
}

impl Revision {
    fn from_commit(head: ObjectId, commit: &CommitFacts) -> Result<Self> {
        let first_parent = commit
            .parents
            .first()
            .copied()
            .ok_or_else(|| eyre!("Commit {head} has no first parent"))?;
        Ok(Self { head, first_parent })
    }

    pub(super) fn head(self) -> ObjectId {
        self.head
    }

    pub(super) fn first_parent(self) -> ObjectId {
        self.first_parent
    }
}

/// The current entry of a nonempty history.
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

/// A structurally nonempty sequence of complete published revisions.
///
/// Version numbers are not stored. Position zero is v1, position one is v2,
/// and so on. Every observed tag position remains distinct, including
/// adjacent positions which name the same literal revision. The reader must
/// preserve immutable evidence even when the publisher would not create that
/// history from the current proposal.
#[derive(Clone, Debug, Eq, PartialEq)]
struct PublishedHistory {
    first: Revision,
    later: Box<[Revision]>,
}

impl PublishedHistory {
    fn from_revisions(mut revisions: impl Iterator<Item = Revision>) -> Result<Self> {
        let first = revisions.next().ok_or_else(|| eyre!("Published history is empty"))?;
        let later = revisions.collect();
        Ok(Self { first, later })
    }

    fn len(&self) -> usize {
        1 + self.later.len()
    }

    fn iter(&self) -> impl ExactSizeIterator<Item = Revision> + '_ {
        (0..self.len()).map(|index| if index == 0 { self.first } else { self.later[index - 1] })
    }

    fn at(&self, index: usize) -> Revision {
        if index == 0 { self.first } else { self.later[index - 1] }
    }

    fn versioned(&self) -> impl ExactSizeIterator<Item = (Version, Revision)> + '_ {
        self.iter().enumerate().map(|(index, revision)| {
            let version = Version::from_history_index(index)
                .expect("an in-memory history position always fits in u64");
            (version, revision)
        })
    }

    fn current(&self) -> CurrentVersion {
        let index = self.len() - 1;
        let revision = self.later.last().copied().unwrap_or(self.first);
        let number = Version::from_history_index(index)
            .expect("an in-memory history position always fits in u64");
        CurrentVersion { number, revision }
    }
}

/// One complete structurally normalized remote history observation.
///
/// The private optional field distinguishes a genuinely absent history from a
/// nonempty published history. Its production transitions either add a
/// mandatory literal proposal or require the entire published history to
/// exist, so neither path accepts a selected subset.
#[derive(Debug)]
pub(super) struct NormalizedPublishedHistory {
    id: GherritPrId,
    published: Option<PublishedHistory>,
}

impl NormalizedPublishedHistory {
    /// Consumes one complete, destination-bound remote change observation.
    pub(super) fn from_observation(
        observed: ObservedChangeHistory,
        graph: &CommitGraphEvidence,
    ) -> Result<Self> {
        let id = observed.id().clone();
        let head = observed.candidate_head();
        let owned_base = observed.owned_base();
        let tags = observed.versions();
        match (head, owned_base, tags.len() == 0) {
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

        let revisions = tags
            .enumerate()
            .map(|(index, (actual, target))| {
                let expected = Version::from_history_index(index).ok_or_else(|| {
                    eyre!("Remote GHerrit change '{}' has too many versions", id.as_str())
                })?;
                if actual != expected {
                    bail!(
                        "Remote GHerrit change '{}' has noncontiguous version tags: expected v{expected}, observed v{actual}",
                        id.as_str()
                    );
                }
                graph.revision(target).wrap_err_with(|| {
                    format!(
                        "Version v{actual} of GHerrit change '{}' is not a complete commit",
                        id.as_str()
                    )
                })
            })
            .collect::<Result<Vec<_>>>()?;
        let history = PublishedHistory::from_revisions(revisions.into_iter())?;
        let current = history.current().revision();
        if head != Some(current.head()) {
            bail!(
                "Remote GHerrit change '{}' head does not match its latest version tag",
                id.as_str()
            );
        }
        if owned_base != Some(current.first_parent()) {
            bail!(
                "Remote GHerrit change '{}' owned base does not match the latest version's first parent",
                id.as_str()
            );
        }
        Ok(Self { id, published: Some(history) })
    }

    /// Consumes normalized history and couples it to one real proposed commit.
    pub(super) fn with_proposal(
        self,
        change: &LocalChange,
        graph: &CommitGraphEvidence,
    ) -> Result<ChangeHistory> {
        if self.id != *change.id() {
            bail!(
                "Observed GHerrit change '{}' cannot be coupled to local change '{}'",
                self.id.as_str(),
                change.id().as_str()
            );
        }
        let proposed = graph.revision(change.head())?;
        if proposed.first_parent() != change.first_parent() {
            bail!("Local change {} does not retain its literal first parent", change.head());
        }
        Ok(ChangeHistory { id: self.id, published: self.published, proposed })
    }

    /// Consumes and validates all history for an existing nonlocal change.
    pub(super) fn validate_existing(
        self,
        graph: &CommitGraphEvidence,
        default_tip: Option<ObjectId>,
    ) -> Result<ValidatedPublishedHistory> {
        let published = self.published.ok_or_else(|| {
            eyre!("Existing GHerrit change '{}' has no published history", self.id.as_str())
        })?;
        graph.validate_complete_revisions(&self.id, published.iter(), default_tip)?;
        Ok(ValidatedPublishedHistory { id: self.id, published })
    }
}

/// Complete published-only evidence for an existing nonlocal change.
#[derive(Debug)]
pub(super) struct ValidatedPublishedHistory {
    id: GherritPrId,
    published: PublishedHistory,
}

impl ValidatedPublishedHistory {
    pub(super) fn id(&self) -> &GherritPrId {
        &self.id
    }

    pub(super) fn published_len(&self) -> usize {
        self.published.len()
    }

    pub(super) fn published_versions(
        &self,
    ) -> impl ExactSizeIterator<Item = (Version, Revision)> + '_ {
        self.published.versioned()
    }

    pub(super) fn published_current(&self) -> CurrentVersion {
        self.published.current()
    }

    pub(super) fn contains_published_head(&self, head: ObjectId) -> bool {
        self.published.iter().any(|revision| revision.head() == head)
    }

    pub(super) fn contains_published_first_parent(&self, first_parent: ObjectId) -> bool {
        self.published.iter().any(|revision| revision.first_parent() == first_parent)
    }
}

/// Complete, unvalidated history for exactly one change.
///
/// There is deliberately no inspection surface. Validation consumes this
/// value and checks the entire published sequence together with its proposal.
#[derive(Debug)]
pub(super) struct ChangeHistory {
    id: GherritPrId,
    published: Option<PublishedHistory>,
    proposed: Revision,
}

impl ChangeHistory {
    /// Validates the complete history and optionally its exact root base.
    pub(super) fn validate(
        self,
        graph: &CommitGraphEvidence,
        default_tip: Option<ObjectId>,
    ) -> Result<ValidatedChangeHistory> {
        graph.validate_complete_revisions(&self.id, self.revisions(), default_tip)?;
        Ok(ValidatedChangeHistory {
            id: self.id,
            published: self.published,
            proposed: self.proposed,
        })
    }

    fn revisions(&self) -> impl Iterator<Item = Revision> + '_ {
        self.published.iter().flat_map(PublishedHistory::iter).chain(std::iter::once(self.proposed))
    }
}

/// Complete history evidence which is safe for planning to inspect.
///
/// Published-only membership intentionally excludes the proposal. GitHub OIDs
/// observed before publication may be correlated only with already-published
/// evidence. A proposal equal to the current literal revision is retained as
/// intent but does not project an adjacent duplicate version.
#[derive(Debug)]
pub(super) struct ValidatedChangeHistory {
    id: GherritPrId,
    published: Option<PublishedHistory>,
    proposed: Revision,
}

impl ValidatedChangeHistory {
    pub(super) fn id(&self) -> &GherritPrId {
        &self.id
    }

    pub(super) fn published_len(&self) -> usize {
        self.published.as_ref().map_or(0, PublishedHistory::len)
    }

    pub(super) fn published_versions(&self) -> impl Iterator<Item = (Version, Revision)> + '_ {
        self.published.iter().flat_map(PublishedHistory::versioned)
    }

    pub(super) fn published_current(&self) -> Option<CurrentVersion> {
        self.published.as_ref().map(PublishedHistory::current)
    }

    pub(super) fn proposed(&self) -> Revision {
        self.proposed
    }

    pub(super) fn needs_publication(&self) -> bool {
        self.published_current().is_none_or(|current| current.revision() != self.proposed)
    }

    pub(super) fn projected_versions(
        &self,
    ) -> impl DoubleEndedIterator<Item = (Version, Revision)> + ExactSizeIterator + '_ {
        let published_len = self.published_len();
        let projected_len = published_len + usize::from(self.needs_publication());
        (0..projected_len).map(move |index| {
            let version = Version::from_history_index(index)
                .expect("an in-memory history position always fits in u64");
            let revision = if index < published_len {
                self.published
                    .as_ref()
                    .expect("a published position has published history")
                    .at(index)
            } else {
                self.proposed
            };
            (version, revision)
        })
    }

    pub(super) fn projected_current(&self) -> CurrentVersion {
        if self.needs_publication() {
            let number = Version::from_history_index(self.published_len())
                .expect("an in-memory history position always fits in u64");
            CurrentVersion { number, revision: self.proposed }
        } else {
            self.published_current()
                .expect("only a published current revision can equal the proposal")
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

impl GherritPrId {
    /// Constructs identity only from one real commit's canonical trailer data.
    fn from_commit(head: ObjectId, trailer_identities: &[Vec<u8>]) -> Result<Self> {
        let [identity] = trailer_identities else {
            bail!("Head commit {head} must have exactly one gherrit-pr-id trailer");
        };
        Self::from_ref_component(identity)
            .wrap_err_with(|| format!("Head commit {head} has an invalid gherrit-pr-id trailer"))
    }
}

/// Complete literal commit facts reachable from a set of roots.
///
/// Every reachable object is loaded as a commit, every parent edge is retained,
/// and Git's own trailer formatter parses every commit whose raw message can
/// contain the exact identity key. An ancestry traversal owns one temporary
/// visited set and releases it before the next traversal; evidence never keeps
/// one full ancestor set per historical head or parent. One traversal per
/// distinct parent checks all known heads and stops on the first unsafe pair.
/// No head-by-parent relation is retained.
pub(super) struct CommitGraphEvidence {
    commits: HashMap<ObjectId, CommitFacts>,
    trailer_identities: HashMap<ObjectId, Box<[Vec<u8>]>>,
}

/// Distinguishes an object which acquisition may supply from invalid evidence.
#[derive(Debug)]
pub(super) enum GraphLoadError {
    MissingObject { oid: ObjectId },
    Invalid(Report),
}

impl fmt::Display for GraphLoadError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::MissingObject { oid } => write!(formatter, "Commit object {oid} is missing"),
            Self::Invalid(error) => error.fmt(formatter),
        }
    }
}

impl std::error::Error for GraphLoadError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::MissingObject { .. } => None,
            Self::Invalid(error) => error.source(),
        }
    }
}

impl CommitGraphEvidence {
    /// Loads the complete all-parent ancestry of `roots` from the literal ODB.
    pub(super) fn load(
        repository: &util::Repo,
        roots: impl IntoIterator<Item = ObjectId>,
    ) -> std::result::Result<Self, GraphLoadError> {
        let mut pending = roots.into_iter().collect::<Vec<_>>();
        let mut parents_by_commit = HashMap::<ObjectId, Box<[ObjectId]>>::new();
        let mut trailer_candidates = Vec::new();

        while let Some(oid) = pending.pop() {
            if parents_by_commit.contains_key(&oid) {
                continue;
            }
            let object = repository.find_object(oid).map_err(|error| match error {
                gix::object::find::existing::Error::NotFound { oid } => {
                    GraphLoadError::MissingObject { oid }
                }
                error => GraphLoadError::Invalid(Report::new(error)),
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
            let decoded =
                commit.decode().map_err(|error| GraphLoadError::Invalid(Report::new(error)))?;
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
            pending.extend(parents.iter().copied());
            parents_by_commit.insert(oid, parents);
        }

        let trailer_identities = read_commit_trailers(repository, trailer_candidates)
            .map_err(GraphLoadError::Invalid)?
            .into_iter()
            .filter(|(_, identities)| !identities.is_empty())
            .collect();
        let commits = parents_by_commit
            .into_iter()
            .map(|(oid, parents)| (oid, CommitFacts { parents }))
            .collect();
        Ok(Self { commits, trailer_identities })
    }

    /// Derives a revision from a real commit and its literal first parent.
    fn revision(&self, head: ObjectId) -> Result<Revision> {
        let commit = self
            .commits
            .get(&head)
            .ok_or_else(|| eyre!("Commit {head} is absent from complete graph evidence"))?;
        Revision::from_commit(head, commit)
    }

    /// Proves every invariant over one privately supplied complete sequence.
    fn validate_complete_revisions(
        &self,
        id: &GherritPrId,
        revisions: impl IntoIterator<Item = Revision>,
        default_tip: Option<ObjectId>,
    ) -> Result<()> {
        let revisions = self.literal_revisions(revisions)?;
        let expected = id.as_str().as_bytes();
        let heads = revisions.iter().map(|revision| revision.head()).collect::<HashSet<_>>();
        let identity_by_head = heads
            .iter()
            .map(|head| {
                let identities =
                    self.trailer_identities.get(head).map(Box::as_ref).unwrap_or_default();
                Ok((
                    *head,
                    (
                        GherritPrId::from_commit(*head, identities)
                            .is_ok_and(|observed| observed == *id),
                        self.count_identity(*head, expected)?,
                    ),
                ))
            })
            .collect::<Result<HashMap<_, _>>>()?;
        let proof = revisions
            .iter()
            .map(|revision| {
                let (exact_own_identity, ancestry_identity_count) =
                    identity_by_head[&revision.head()];
                ProofRevision {
                    head: revision.head(),
                    first_parent: revision.first_parent(),
                    exact_own_identity,
                    ancestry_identity_count,
                }
            })
            .collect::<Vec<_>>();
        let heads = validate_change_proof(id.as_str(), &proof, |parent, heads| {
            self.first_reachable_target(parent, heads)
        })?;
        if let Some(default_tip) = default_tip {
            validate_root_proof(default_tip, &heads, |tip, heads| {
                self.first_reachable_target(tip, heads)
            })?;
        }
        Ok(())
    }

    fn literal_revisions(
        &self,
        revisions: impl IntoIterator<Item = Revision>,
    ) -> Result<Vec<Revision>> {
        revisions
            .into_iter()
            .map(|revision| {
                let literal = self.revision(revision.head())?;
                if literal != revision {
                    bail!("Revision {} does not have its literal first parent", revision.head());
                }
                Ok(revision)
            })
            .collect()
    }

    fn count_identity(&self, root: ObjectId, expected: &[u8]) -> Result<usize> {
        let mut count = 0;
        self.visit_ancestry(root, |oid| {
            count += self
                .trailer_identities
                .get(&oid)
                .into_iter()
                .flatten()
                .filter(|candidate| candidate.as_slice() == expected)
                .count();
            false
        })?;
        Ok(count)
    }

    fn first_reachable_target(
        &self,
        from: ObjectId,
        targets: &HashSet<ObjectId>,
    ) -> Result<Option<ObjectId>> {
        let mut reachable = None;
        self.visit_ancestry(from, |oid| {
            if targets.contains(&oid) {
                reachable = Some(oid);
            }
            reachable.is_some()
        })?;
        Ok(reachable)
    }

    /// Walks one ancestry with O(number of commits) temporary memory.
    fn visit_ancestry(
        &self,
        root: ObjectId,
        mut stop: impl FnMut(ObjectId) -> bool,
    ) -> Result<bool> {
        let mut pending = vec![root];
        let mut visited = HashSet::new();
        while let Some(oid) = pending.pop() {
            if !visited.insert(oid) {
                continue;
            }
            let commit = self
                .commits
                .get(&oid)
                .ok_or_else(|| eyre!("Commit {oid} is absent from complete graph evidence"))?;
            if stop(oid) {
                return Ok(true);
            }
            pending.extend(commit.parents.iter().copied());
        }
        Ok(false)
    }
}

#[derive(Clone, Copy)]
struct ProofRevision<T> {
    head: T,
    first_parent: T,
    exact_own_identity: bool,
    ancestry_identity_count: usize,
}

/// Validates semantic evidence without depending on Git's traversal strategy.
fn validate_change_proof<T>(
    id: &str,
    revisions: &[ProofRevision<T>],
    mut first_reachable_head: impl FnMut(T, &HashSet<T>) -> Result<Option<T>>,
) -> Result<HashSet<T>>
where
    T: Copy + Eq + std::hash::Hash + fmt::Display,
{
    if revisions.is_empty() {
        bail!("GHerrit change '{id}' has no revisions to validate");
    }
    let mut identities = HashMap::new();
    for revision in revisions {
        let evidence = (revision.exact_own_identity, revision.ancestry_identity_count);
        if let Some(previous) = identities.insert(revision.head, evidence)
            && previous != evidence
        {
            bail!("Conflicting identity evidence for head {}", revision.head);
        }
    }
    let heads = identities.keys().copied().collect::<HashSet<_>>();
    let parents = revisions.iter().map(|revision| revision.first_parent).collect::<HashSet<_>>();
    for parent in parents {
        if let Some(head) = first_reachable_head(parent, &heads)? {
            bail!(
                "Managed head {head} is reachable from historical or proposed owned base {parent}"
            );
        }
    }
    for (head, (exact_own_identity, ancestry_identity_count)) in &identities {
        if !exact_own_identity {
            bail!("Head commit {head} must have exactly one gherrit-pr-id trailer equal to '{id}'");
        }
        if *ancestry_identity_count != 1 {
            bail!(
                "The complete ancestry of head {head} contains {ancestry_identity_count} commits with gherrit-pr-id '{id}'"
            );
        }
    }
    Ok(heads)
}

fn validate_root_proof<T>(
    default_tip: T,
    heads: &HashSet<T>,
    mut first_reachable_head: impl FnMut(T, &HashSet<T>) -> Result<Option<T>>,
) -> Result<()>
where
    T: Copy + Eq + std::hash::Hash + fmt::Display,
{
    if heads.is_empty() {
        bail!("A root change must have at least one revision for validation");
    }
    if let Some(head) = first_reachable_head(default_tip, heads)? {
        bail!("Root managed head {head} is reachable from default tip {default_tip}");
    }
    Ok(())
}

fn message_may_contain_identity(message: &[u8]) -> bool {
    // Git's trailer formatter preserves the key bytes. It cannot emit the
    // exact lowercase key unless those bytes occur in the raw message, so this
    // prefilter can skip ordinary commits without changing trailer semantics.
    const KEY: &[u8] = b"gherrit-pr-id";
    message.windows(KEY.len()).any(|window| window == KEY)
}

fn read_commit_trailers(
    repository: &util::Repo,
    commits: impl IntoIterator<Item = ObjectId>,
) -> Result<HashMap<ObjectId, Box<[Vec<u8>]>>> {
    const QUERY_BATCH_LEN: usize = 120;
    const FORMAT: &str = "--format=tformat:%H%x00%(trailers:only,unfold)";

    let mut commits = commits.into_iter().collect::<Vec<_>>();
    commits.sort_unstable();
    let expected = commits.into_iter().collect::<HashSet<_>>();
    let mut observed = HashMap::with_capacity(expected.len());
    let mut commits = expected.iter().copied().collect::<Vec<_>>();
    commits.sort_unstable();
    for batch in commits.chunks(QUERY_BATCH_LEN) {
        let arguments = [
            "log",
            "--no-walk=unsorted",
            "--no-show-signature",
            "--no-notes",
            "--no-decorate",
            "-z",
            FORMAT,
        ]
        .into_iter()
        .map(ToOwned::to_owned)
        .chain(batch.iter().map(ObjectId::to_string));
        let mut command = util::cmd("git", arguments);
        command.current_dir(repository.workdir().unwrap_or(repository.path()));
        let output = command.checked_output().wrap_err("Failed to parse commit trailers")?;
        let expected_batch = batch.iter().copied().collect::<HashSet<_>>();
        let mut fields = output.stdout.split(|byte| *byte == 0);
        loop {
            let oid = fields.next().ok_or_else(|| eyre!("Git returned malformed trailer data"))?;
            if oid.is_empty() {
                if fields.next().is_some() {
                    bail!("Git returned trailing fields after commit trailer data");
                }
                break;
            }
            let oid =
                ObjectId::from_hex(oid).wrap_err("Git returned an invalid trailer object ID")?;
            if !expected_batch.contains(&oid) {
                bail!("Git returned trailer data for unrequested commit {oid}");
            }
            let trailers =
                fields.next().ok_or_else(|| eyre!("Git omitted trailer data for commit {oid}"))?;
            let identities = trailers
                .split(|byte| *byte == b'\n')
                .filter_map(|line| line.strip_prefix(b"gherrit-pr-id: "))
                .map(<[u8]>::to_vec)
                .collect::<Box<[_]>>();
            if observed.insert(oid, identities).is_some() {
                bail!("Git returned duplicate trailer data for commit {oid}");
            }
        }
        if !expected_batch.iter().all(|oid| observed.contains_key(oid)) {
            bail!("Git omitted trailer data for one or more requested commits");
        }
    }
    if observed.len() != expected.len() {
        bail!("Git omitted trailer data for one or more requested commits");
    }
    Ok(observed)
}

#[cfg(test)]
mod tests {
    use std::{
        collections::{BTreeMap, HashSet},
        fmt::Write as _,
    };

    use gix::ObjectId;
    use tempfile::TempDir;

    use super::*;
    use crate::pre_push::{local::LocalStack, remote};

    fn change_id(value: &str) -> GherritPrId {
        GherritPrId::from_ref_component(value.as_bytes()).expect("valid test change ID")
    }

    fn version(value: u64) -> Version {
        Version::new(value).expect("test versions are positive")
    }

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

        fn commit(&self, subject: &str, parents: &[ObjectId], identities: &[&str]) -> ObjectId {
            let trailers = identities
                .iter()
                .map(|identity| format!("gherrit-pr-id: {identity}"))
                .collect::<Vec<_>>()
                .join("\n");
            let message = if trailers.is_empty() {
                subject.to_owned()
            } else {
                format!("{subject}\n\n{trailers}\n")
            };
            self.commit_with_message(&message, parents)
        }

        fn commit_with_message(&self, message: &str, parents: &[ObjectId]) -> ObjectId {
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

        fn open(&self) -> util::Repo {
            util::Repo::open(self.directory.path().to_str().expect("UTF-8 test path"))
                .expect("open test repository")
        }
    }

    fn load(
        repository: &TestRepository,
        roots: impl IntoIterator<Item = ObjectId>,
    ) -> CommitGraphEvidence {
        CommitGraphEvidence::load(&repository.open(), roots).expect("complete test graph")
    }

    fn tags(entries: impl IntoIterator<Item = (u64, ObjectId)>) -> BTreeMap<Version, ObjectId> {
        entries.into_iter().map(|(number, oid)| (version(number), oid)).collect()
    }

    fn observed_change(
        id: &GherritPrId,
        head: Option<ObjectId>,
        owned_base: Option<ObjectId>,
        tags: &BTreeMap<Version, ObjectId>,
    ) -> remote::ObservedChangeHistory {
        let default = ObjectId::from_bytes_or_panic(&[1; 20]);
        let mut heads =
            format!("ref: refs/heads/main\tHEAD\n{default}\tHEAD\n{default}\trefs/heads/main\n");
        if let Some(head) = head {
            writeln!(heads, "{head}\trefs/heads/{}", id.as_str()).unwrap();
        }
        if let Some(owned_base) = owned_base {
            writeln!(heads, "{owned_base}\trefs/heads/gherrit-bases/{}", id.as_str()).unwrap();
        }
        let versions = tags
            .iter()
            .map(|(version, oid)| format!("{oid}\trefs/tags/gherrit/{}/v{version}\n", id.as_str()))
            .collect::<String>();
        remote::parse_active_change_for_test(id.clone(), heads.as_bytes(), versions.as_bytes())
            .expect("complete test observation")
    }

    fn normalize(
        id: &GherritPrId,
        head: Option<ObjectId>,
        owned_base: Option<ObjectId>,
        tags: &BTreeMap<Version, ObjectId>,
        graph: &CommitGraphEvidence,
    ) -> Result<NormalizedPublishedHistory> {
        NormalizedPublishedHistory::from_observation(
            observed_change(id, head, owned_base, tags),
            graph,
        )
    }

    fn change_history(
        id: &GherritPrId,
        graph: &CommitGraphEvidence,
        published: &[(u64, ObjectId)],
        proposed: ObjectId,
    ) -> ChangeHistory {
        let (head, owned_base) = published.last().map_or((None, None), |(_, head)| {
            let revision = graph.revision(*head).expect("literal published revision");
            (Some(*head), Some(revision.first_parent()))
        });
        let normalized = normalize(id, head, owned_base, &tags(published.iter().copied()), graph)
            .expect("normalized test history");
        let first_parent =
            graph.revision(proposed).expect("literal proposed revision").first_parent();
        let stack = LocalStack::for_test(first_parent, [(id.clone(), proposed)]);
        normalized
            .with_proposal(stack.iter().next().expect("one local change"), graph)
            .expect("literal proposed revision")
    }

    #[test]
    fn normalizes_only_absent_or_complete_history() {
        let repository = TestRepository::new();
        let root = repository.commit("root", &[], &[]);
        let head = repository.commit("head", &[root], &["Gone"]);
        let graph = load(&repository, [head]);
        let id = change_id("Gone");

        for has_head in [false, true] {
            for has_base in [false, true] {
                for has_tags in [false, true] {
                    let observed_head = has_head.then_some(head);
                    let observed_base = has_base.then_some(root);
                    let observed_tags = if has_tags { tags([(1, head)]) } else { BTreeMap::new() };
                    let result =
                        normalize(&id, observed_head, observed_base, &observed_tags, &graph);

                    match (has_head, has_base, has_tags) {
                        (false, false, false) => assert!(
                            result.is_ok_and(|history| history.published.is_none()),
                            "absent evidence must remain absent"
                        ),
                        (true, true, true) => {
                            let normalized = result.expect("complete history");
                            let history = normalized.published.expect("published");
                            assert_eq!(history.current().number(), Version::FIRST);
                            assert_eq!(history.current().revision().head(), head);
                            assert_eq!(history.current().revision().first_parent(), root);
                        }
                        _ => assert!(result.is_err(), "partial tuple unexpectedly normalized"),
                    }
                }
            }
        }
    }

    #[test]
    fn derives_versions_and_preserves_every_tag_position() {
        let repository = TestRepository::new();
        let root = repository.commit("root", &[], &[]);
        let a = repository.commit("A", &[root], &["Gone"]);
        let b = repository.commit("B", &[root], &["Gone"]);
        let graph = load(&repository, [a, b]);
        let id = change_id("Gone");

        let history = change_history(&id, &graph, &[(1, a), (2, a), (3, b), (4, a)], a)
            .validate(&graph, None)
            .expect("valid complete history");
        assert_eq!(
            history
                .published_versions()
                .map(|(version, revision)| (version.get(), revision.head()))
                .collect::<Vec<_>>(),
            [(1, a), (2, a), (3, b), (4, a)]
        );
        assert_eq!(history.published_current().unwrap().number(), version(4));
        assert!(!history.needs_publication());
        assert_eq!(
            history
                .projected_versions()
                .map(|(version, revision)| (version.get(), revision.head()))
                .collect::<Vec<_>>(),
            [(1, a), (2, a), (3, b), (4, a)]
        );
    }

    #[test]
    fn absent_published_history_still_requires_and_validates_one_proposal() {
        let repository = TestRepository::new();
        let root = repository.commit("root", &[], &[]);
        let proposed = repository.commit("proposal", &[root], &["Gone"]);
        let graph = load(&repository, [proposed]);
        let id = change_id("Gone");

        let validated = change_history(&id, &graph, &[], proposed)
            .validate(&graph, None)
            .expect("new change with one mandatory proposal");

        assert_eq!(validated.id(), &id);
        assert_eq!(validated.published_len(), 0);
        assert_eq!(validated.published_current(), None);
        assert_eq!(validated.proposed().head(), proposed);
        assert!(validated.needs_publication());
        assert_eq!(validated.projected_current().number(), Version::FIRST);
        assert_eq!(validated.projected_current().revision().head(), proposed);
        assert_eq!(
            validated
                .projected_versions()
                .map(|(number, revision)| (number.get(), revision.head()))
                .collect::<Vec<_>>(),
            [(1, proposed)]
        );
        assert!(!validated.contains_published_head(proposed));
        assert!(!validated.contains_published_first_parent(root));
    }

    #[test]
    fn opaque_observation_id_cannot_be_replaced_by_a_local_proposal() {
        let repository = TestRepository::new();
        let root = repository.commit("root", &[], &[]);
        let proposed = repository.commit("proposal", &[root], &["Gother"]);
        let graph = load(&repository, [proposed]);
        let observed_id = change_id("Gone");
        let local_id = change_id("Gother");
        let normalized = normalize(&observed_id, None, None, &BTreeMap::new(), &graph)
            .expect("opaque absent observation");
        let stack = LocalStack::for_test(root, [(local_id, proposed)]);

        let error = normalized
            .with_proposal(stack.iter().next().unwrap(), &graph)
            .expect_err("a caller cannot replace the retained observed ID");

        assert!(error.to_string().contains("cannot be coupled"), "{error:?}");
        assert!(error.to_string().contains("Gone"), "{error:?}");
        assert!(error.to_string().contains("Gother"), "{error:?}");
    }

    #[test]
    fn validated_accessors_retain_complete_published_and_projected_history() {
        let repository = TestRepository::new();
        let published_base = repository.commit("published base", &[], &[]);
        let proposed_base = repository.commit("proposed base", &[], &[]);
        let a = repository.commit("A", &[published_base], &["Gone"]);
        let b = repository.commit("B", &[published_base], &["Gone"]);
        let proposed = repository.commit("proposal", &[proposed_base], &["Gone"]);
        let graph = load(&repository, [a, b, proposed]);
        let id = change_id("Gone");

        let validated = change_history(&id, &graph, &[(1, a), (2, b)], proposed)
            .validate(&graph, None)
            .expect("complete history is safe");

        assert_eq!(validated.id(), &id);
        assert_eq!(validated.published_len(), 2);
        assert_eq!(
            validated
                .published_versions()
                .map(|(number, revision)| (number.get(), revision.head()))
                .collect::<Vec<_>>(),
            [(1, a), (2, b)]
        );
        assert_eq!(validated.published_current().unwrap().revision().head(), b);
        assert_eq!(validated.proposed().head(), proposed);
        assert!(validated.needs_publication());
        assert_eq!(
            validated
                .projected_versions()
                .map(|(number, revision)| (number.get(), revision.head()))
                .collect::<Vec<_>>(),
            [(1, a), (2, b), (3, proposed)]
        );
        assert_eq!(validated.projected_current().number(), version(3));
        assert_eq!(validated.projected_current().revision().head(), proposed);
        assert!(validated.contains_published_head(a));
        assert!(validated.contains_published_head(b));
        assert!(!validated.contains_published_head(proposed));
        assert!(validated.contains_published_first_parent(published_base));
        assert!(!validated.contains_published_first_parent(proposed_base));
    }

    #[test]
    fn unsafe_middle_published_revision_cannot_be_omitted_from_validation() {
        let repository = TestRepository::new();
        let root = repository.commit("root", &[], &[]);
        let first = repository.commit("first", &[root], &["Gone"]);
        let unsafe_middle = repository.commit("unsafe middle", &[root], &["Gother"]);
        let current = repository.commit("current", &[root], &["Gone"]);
        let proposed = repository.commit("proposal", &[root], &["Gone"]);
        let graph = load(&repository, [first, unsafe_middle, current, proposed]);
        let id = change_id("Gone");
        let observed = tags([(1, first), (2, unsafe_middle), (3, current)]);

        let normalized = normalize(&id, Some(current), Some(root), &observed, &graph)
            .expect("structurally complete published history");
        assert_eq!(normalized.published.as_ref().unwrap().len(), 3);
        let stack = LocalStack::for_test(root, [(id.clone(), proposed)]);
        let error = normalized
            .with_proposal(stack.iter().next().unwrap(), &graph)
            .expect("literal proposal")
            .validate(&graph, None)
            .expect_err("the sole validation path must include the unsafe middle revision");
        assert!(error.to_string().contains(&unsafe_middle.to_string()), "{error:?}");
    }

    #[test]
    fn existing_nonlocal_validation_rejects_absence_and_checks_full_history() {
        let repository = TestRepository::new();
        let root = repository.commit("root", &[], &[]);
        let a = repository.commit("A", &[root], &["Gone"]);
        let b = repository.commit("B", &[root], &["Gone"]);
        let unsafe_middle = repository.commit("unsafe middle", &[root], &["Gother"]);
        let current = repository.commit("current", &[root], &["Gone"]);
        let graph = load(&repository, [a, b, unsafe_middle, current]);
        let id = change_id("Gone");

        let absent =
            normalize(&id, None, None, &BTreeMap::new(), &graph).expect("genuinely absent history");
        let absent_error = absent
            .validate_existing(&graph, None)
            .expect_err("an existing nonlocal change must have published history");
        assert!(absent_error.to_string().contains("no published history"));

        let unsafe_history = normalize(
            &id,
            Some(current),
            Some(root),
            &tags([(1, a), (2, unsafe_middle), (3, current)]),
            &graph,
        )
        .expect("structurally complete nonlocal history");
        let unsafe_error = unsafe_history
            .validate_existing(&graph, None)
            .expect_err("full nonlocal validation must retain the unsafe middle revision");
        assert!(unsafe_error.to_string().contains(&unsafe_middle.to_string()));

        let validated =
            normalize(&id, Some(a), Some(root), &tags([(1, a), (2, b), (3, a)]), &graph)
                .expect("complete nonlocal history")
                .validate_existing(&graph, Some(root))
                .expect("entire nonlocal history and exact root tip are safe");
        assert_eq!(validated.id(), &id);
        assert_eq!(validated.published_len(), 3);
        assert_eq!(
            validated
                .published_versions()
                .map(|(number, revision)| (number.get(), revision.head()))
                .collect::<Vec<_>>(),
            [(1, a), (2, b), (3, a)]
        );
        assert_eq!(validated.published_current().number(), version(3));
        assert_eq!(validated.published_current().revision().head(), a);
        assert!(validated.contains_published_head(a));
        assert!(validated.contains_published_head(b));
        assert!(!validated.contains_published_head(current));
        assert!(validated.contains_published_first_parent(root));
    }

    #[test]
    fn proposed_revision_identity_and_reachability_are_both_mandatory() {
        let repository = TestRepository::new();
        let root = repository.commit("root", &[], &[]);
        let wrong_identity = repository.commit("wrong proposal", &[root], &["Gother"]);
        let published = repository.commit("published", &[root], &["Gone"]);
        let descendant_base = repository.commit("descendant base", &[published], &[]);
        let unsafe_proposal = repository.commit("unsafe proposal", &[descendant_base], &["Gone"]);
        let graph = load(&repository, [wrong_identity, unsafe_proposal]);
        let id = change_id("Gone");

        let identity_error = change_history(&id, &graph, &[], wrong_identity)
            .validate(&graph, None)
            .expect_err("a proposal is not optional identity evidence");
        assert!(identity_error.to_string().contains("exactly one gherrit-pr-id"));

        let reachability_error = change_history(&id, &graph, &[(1, published)], unsafe_proposal)
            .validate(&graph, None)
            .expect_err("a proposal's owned base cannot contain a published head");
        assert!(
            reachability_error.to_string().contains("reachable from"),
            "{reachability_error:?}"
        );
    }

    #[test]
    fn rejects_every_gap_in_a_bounded_raw_version_domain() {
        let repository = TestRepository::new();
        let root = repository.commit("root", &[], &[]);
        let revisions = (1..=4)
            .map(|index| repository.commit(&format!("head {index}"), &[root], &["Gone"]))
            .collect::<Vec<_>>();
        let graph = load(&repository, revisions.iter().copied());
        let id = change_id("Gone");

        for mask in 1_u8..1 << revisions.len() {
            let observed = revisions
                .iter()
                .enumerate()
                .filter(|(index, _)| mask & (1 << index) != 0)
                .map(|(index, oid)| ((index + 1) as u64, *oid))
                .collect::<Vec<_>>();
            let current = observed.last().expect("nonempty mask").1;
            let normalized =
                normalize(&id, Some(current), Some(root), &tags(observed.iter().copied()), &graph);
            let contiguous = mask == (1 << observed.len()) - 1;
            assert_eq!(normalized.is_ok(), contiguous, "mask={mask:04b}");
        }
    }

    #[test]
    fn rejects_latest_head_and_owned_base_disagreement() {
        let repository = TestRepository::new();
        let root = repository.commit("root", &[], &[]);
        let head = repository.commit("head", &[root], &["Gone"]);
        let other = repository.commit("other", &[root], &["Gone"]);
        let graph = load(&repository, [head, other]);
        let id = change_id("Gone");
        let history = tags([(1, head)]);

        let head_error = normalize(&id, Some(other), Some(root), &history, &graph)
            .expect_err("head disagreement");
        assert!(head_error.to_string().contains("head does not match"));

        let base_error = normalize(&id, Some(head), Some(other), &history, &graph)
            .expect_err("owned-base disagreement");
        assert!(base_error.to_string().contains("owned base does not match"));
    }

    #[test]
    fn distinguishes_missing_objects_and_rejects_noncommits_and_parentless_heads() {
        let repository = TestRepository::new();
        let missing = ObjectId::from_bytes_or_panic(&[0x55; 20]);
        assert!(matches!(
            CommitGraphEvidence::load(&repository.open(), [missing]),
            Err(GraphLoadError::MissingObject { oid }) if oid == missing
        ));
        let incomplete = repository.commit("incomplete", &[missing], &["Gone"]);
        assert!(matches!(
            CommitGraphEvidence::load(&repository.open(), [incomplete]),
            Err(GraphLoadError::MissingObject { oid }) if oid == missing
        ));

        let blob = repository.writer.write_blob(b"not a commit").expect("write blob").detach();
        assert!(matches!(
            CommitGraphEvidence::load(&repository.open(), [blob]),
            Err(GraphLoadError::Invalid(_))
        ));

        let root = repository.commit("parentless", &[], &["Gone"]);
        let graph = load(&repository, [root]);
        assert!(graph.revision(root).is_err());
        assert!(
            normalize(
                &change_id("Gone"),
                Some(missing),
                Some(root),
                &tags([(1, missing)]),
                &graph,
            )
            .is_err()
        );
    }

    #[test]
    fn head_identity_uses_gits_canonical_trailer_semantics() {
        let repository = TestRepository::new();
        let root = repository.commit("root", &[], &[]);
        let valid = repository.commit("valid", &[root], &["Gone"]);
        let wrong = repository.commit("wrong", &[root], &["Gother"]);
        let repeated = repository.commit("repeated", &[root], &["Gone", "Gother"]);
        let body_only = repository.commit_with_message(
            "subject\n\ngherrit-pr-id: Gone\n\nThis is body text, not a trailer.\n",
            &[root],
        );
        let unfolded = repository
            .commit_with_message("subject\n\ngherrit-pr-id: Gone\n continuation\n", &[root]);
        let graph = load(&repository, [valid, wrong, repeated, body_only, unfolded]);
        let id = change_id("Gone");

        change_history(&id, &graph, &[], valid)
            .validate(&graph, None)
            .expect("one exact canonical trailer");
        for head in [wrong, repeated, body_only, unfolded] {
            assert!(
                change_history(&id, &graph, &[], head).validate(&graph, None).is_err(),
                "head={head}"
            );
        }
    }

    #[test]
    fn complete_merge_ancestry_counts_duplicate_identity() {
        let repository = TestRepository::new();
        let root = repository.commit("root", &[], &[]);
        let duplicate = repository.commit("duplicate", &[root], &["Gone"]);
        let head = repository.commit("merge head", &[root, duplicate], &["Gone"]);
        let graph = load(&repository, [head]);
        let id = change_id("Gone");
        let error = change_history(&id, &graph, &[], head)
            .validate(&graph, None)
            .expect_err("non-first-parent duplicate identity");
        assert!(error.to_string().contains("contains 2 commits"), "{error:?}");
    }

    #[test]
    fn direct_reachability_checks_every_historical_head_parent_pair() {
        let repository = TestRepository::new();
        let root = repository.commit("root", &[], &[]);
        let first = repository.commit("first", &[root], &["Gone"]);
        let descendant_parent = repository.commit("later parent", &[first], &[]);
        let unsafe_later = repository.commit("unsafe later", &[descendant_parent], &["Gone"]);
        let safe_later = repository.commit("safe later", &[root], &["Gone"]);
        let graph = load(&repository, [unsafe_later, safe_later]);
        let first = graph.revision(first).unwrap();
        let unsafe_later = graph.revision(unsafe_later).unwrap();
        let safe_later = graph.revision(safe_later).unwrap();

        let proof = |revisions: [Revision; 2]| {
            revisions.map(|revision| ProofRevision {
                head: revision.head(),
                first_parent: revision.first_parent(),
                exact_own_identity: true,
                ancestry_identity_count: 1,
            })
        };
        validate_change_proof("Gone", &proof([first, safe_later]), |from, targets| {
            graph.first_reachable_target(from, targets)
        })
        .expect("parents being reachable from heads is the safe orientation");
        let error =
            validate_change_proof("Gone", &proof([first, unsafe_later]), |from, targets| {
                graph.first_reachable_target(from, targets)
            })
            .expect_err("an older head is reachable from a later owned base");
        assert!(error.to_string().contains("reachable from"), "{error:?}");
    }

    #[test]
    fn root_reachability_uses_the_exact_default_tip() {
        let repository = TestRepository::new();
        let root = repository.commit("root", &[], &[]);
        let head = repository.commit("head", &[root], &["Gone"]);
        let advanced_default = repository.commit("advanced default", &[head], &[]);
        let unrelated_default = repository.commit("other root", &[], &[]);
        let graph = load(&repository, [advanced_default, unrelated_default]);
        let id = change_id("Gone");

        change_history(&id, &graph, &[], head)
            .validate(&graph, None)
            .expect("non-root validation has no default-tip requirement");
        change_history(&id, &graph, &[], head)
            .validate(&graph, Some(root))
            .expect("original default is safe");
        change_history(&id, &graph, &[], head)
            .validate(&graph, Some(unrelated_default))
            .expect("an unrelated exact default is safe");
        let error = change_history(&id, &graph, &[], head)
            .validate(&graph, Some(advanced_default))
            .expect_err("default tip containing the head is unsafe");
        assert!(error.to_string().contains("reachable from default tip"), "{error:?}");
    }

    #[test]
    fn dense_oracle_detects_the_parent_head_reorder_failure() {
        // Old: D-A1-B1. Reordered: D-B2-A2. If B's PR still targets A's
        // mutable head, GitHub can observe B2 against A2 and mark B merged.
        let model = ModelGraph::new(vec![vec![], vec![0], vec![1], vec![0], vec![3]]);
        let (default, _a1, _b1, b2, a2) = (0, 1, 2, 3, 4);
        assert!(model.reaches(a2, b2), "old parent-head base absorbs B2");
        assert!(!model.reaches(default, b2), "B's new owned base remains safe");

        let repository = TestRepository::new();
        let default = repository.commit("D", &[], &[]);
        let a1 = repository.commit("A1", &[default], &["Ga"]);
        let b1 = repository.commit("B1", &[a1], &["Gb"]);
        let b2 = repository.commit("B2", &[default], &["Gb"]);
        let a2 = repository.commit("A2", &[b2], &["Ga"]);
        let evidence = load(&repository, [b1, a2]);
        let id = change_id("Gb");
        change_history(&id, &evidence, &[(1, b1)], b2)
            .validate(&evidence, None)
            .expect("B's own historical first parents are safe across the reorder");
    }

    #[derive(Clone)]
    struct ModelGraph {
        parents: Vec<Vec<usize>>,
        reachable: Vec<Vec<bool>>,
    }

    impl ModelGraph {
        fn new(parents: Vec<Vec<usize>>) -> Self {
            let mut reachable = vec![vec![false; parents.len()]; parents.len()];
            for (commit, commit_parents) in parents.iter().enumerate() {
                reachable[commit][commit] = true;
                for parent in commit_parents {
                    reachable[commit][*parent] = true;
                }
            }
            for through in 0..parents.len() {
                for from in 0..parents.len() {
                    for to in 0..parents.len() {
                        reachable[from][to] |= reachable[from][through] && reachable[through][to];
                    }
                }
            }
            Self { parents, reachable }
        }

        fn reaches(&self, from: usize, target: usize) -> bool {
            self.reachable[from][target]
        }
    }

    #[derive(Default)]
    struct OracleCoverage {
        graphs: [usize; 6],
        graph_history_pairs: usize,
        merge_graph_history_pairs: usize,
        histories: [usize; 4],
        adjacent_repeats: usize,
        nonconsecutive_repeats: usize,
        accepted: usize,
        rejected_identity: usize,
        rejected_reachability: usize,
    }

    fn parent_choices(index: usize) -> Vec<Vec<usize>> {
        std::iter::once(Vec::new())
            .chain((0..index).map(|parent| vec![parent]))
            .chain((0..index).flat_map(|first| {
                (0..index)
                    .filter(move |second| *second != first)
                    .map(move |second| vec![first, second])
            }))
            .collect()
    }

    fn enumerate_graphs(
        node_count: usize,
        parents: &mut Vec<Vec<usize>>,
        visit: &mut impl FnMut(ModelGraph),
    ) {
        if parents.len() == node_count {
            visit(ModelGraph::new(parents.clone()));
            return;
        }
        for choice in parent_choices(parents.len()) {
            parents.push(choice);
            enumerate_graphs(node_count, parents, visit);
            parents.pop();
        }
    }

    fn enumerate_head_sequences(
        eligible: &[usize],
        remaining: usize,
        heads: &mut Vec<usize>,
        visit: &mut impl FnMut(&[usize]),
    ) {
        if remaining == 0 {
            visit(heads);
            return;
        }
        for head in eligible {
            heads.push(*head);
            enumerate_head_sequences(eligible, remaining - 1, heads, visit);
            heads.pop();
        }
    }

    #[test]
    fn bounded_dense_graph_history_oracle_matches_pure_validator() {
        let mut coverage = OracleCoverage::default();
        let mut case_index = 0;

        for node_count in 2..=5 {
            enumerate_graphs(node_count, &mut Vec::new(), &mut |graph| {
                coverage.graphs[node_count] += 1;
                let eligible = graph
                    .parents
                    .iter()
                    .enumerate()
                    .filter_map(|(index, parents)| (!parents.is_empty()).then_some(index))
                    .collect::<Vec<_>>();
                let has_merge = graph.parents.iter().any(|parents| parents.len() == 2);

                for length in 1..=3 {
                    enumerate_head_sequences(&eligible, length, &mut Vec::new(), &mut |heads| {
                        let labeled = heads.iter().copied().collect::<HashSet<_>>();
                        let identity_safe = heads.iter().all(|head| {
                            labeled.iter().filter(|label| graph.reaches(*head, **label)).count()
                                == 1
                        });
                        let reachability_safe = heads.iter().all(|head| {
                            heads.iter().all(|other| {
                                let parent = graph.parents[*other][0];
                                !graph.reaches(parent, *head)
                            })
                        });
                        coverage.graph_history_pairs += 1;
                        coverage.histories[length] += 1;
                        coverage.merge_graph_history_pairs += usize::from(has_merge);
                        coverage.adjacent_repeats +=
                            usize::from(heads.windows(2).any(|pair| pair[0] == pair[1]));
                        coverage.nonconsecutive_repeats += usize::from(
                            heads.len() == 3 && heads[0] == heads[2] && heads[0] != heads[1],
                        );
                        coverage.accepted += usize::from(identity_safe && reachability_safe);
                        coverage.rejected_identity += usize::from(!identity_safe);
                        coverage.rejected_reachability += usize::from(!reachability_safe);
                        let proof = heads
                            .iter()
                            .map(|head| ProofRevision {
                                head: *head,
                                first_parent: graph.parents[*head][0],
                                exact_own_identity: labeled.contains(head),
                                ancestry_identity_count: labeled
                                    .iter()
                                    .filter(|label| graph.reaches(*head, **label))
                                    .count(),
                            })
                            .collect::<Vec<_>>();
                        let validated = validate_change_proof("Gmodel", &proof, |from, targets| {
                            Ok(targets.iter().copied().find(|target| graph.reaches(from, *target)))
                        });
                        assert_eq!(
                            validated.is_ok(),
                            identity_safe && reachability_safe,
                            "case={case_index}, nodes={node_count}, heads={heads:?}, error={:?}",
                            validated.err()
                        );
                        if identity_safe && reachability_safe {
                            let heads = heads.iter().copied().collect::<HashSet<_>>();
                            validate_root_proof(0, &heads, |from, targets| {
                                Ok(targets
                                    .iter()
                                    .copied()
                                    .find(|target| graph.reaches(from, *target)))
                            })
                            .unwrap_or_else(|error| {
                                panic!("root case {case_index}, heads={heads:?}: {error:?}")
                            });
                        }
                        case_index += 1;
                    });
                }
            });
        }

        assert!(coverage.graphs[2..=5].iter().all(|count| *count > 0));
        assert!(coverage.merge_graph_history_pairs > 0);
        assert!(coverage.histories[1..=3].iter().all(|count| *count > 0));
        assert!(coverage.adjacent_repeats > 0);
        assert!(coverage.nonconsecutive_repeats > 0);
        assert!(coverage.accepted > 0);
        assert!(coverage.rejected_identity > 0);
        assert!(coverage.rejected_reachability > 0);
        let summary = format!(
            "graphs by nodes: 2={}, 3={}, 4={}, 5={}\n\
             graph/history pairs: {}\n\
             pairs with merge parents: {}\n\
             histories by length: 1={}, 2={}, 3={}\n\
             histories with adjacent repeats: {}\n\
             nonconsecutive A,B,A histories: {}\n\
             accepted: {}\n\
             rejected by identity: {}\n\
             rejected by reachability: {}",
            coverage.graphs[2],
            coverage.graphs[3],
            coverage.graphs[4],
            coverage.graphs[5],
            coverage.graph_history_pairs,
            coverage.merge_graph_history_pairs,
            coverage.histories[1],
            coverage.histories[2],
            coverage.histories[3],
            coverage.adjacent_repeats,
            coverage.nonconsecutive_repeats,
            coverage.accepted,
            coverage.rejected_identity,
            coverage.rejected_reachability,
        );
        insta::assert_snapshot!(summary, @r###"
graphs by nodes: 2=2, 3=10, 4=100, 5=1700
graph/history pairs: 86482
pairs with merge parents: 81550
histories by length: 1=5574, 2=18274, 3=62634
histories with adjacent repeats: 36548
nonconsecutive A,B,A histories: 12700
accepted: 37608
rejected by identity: 48874
rejected by reachability: 34816
"###);
    }
}

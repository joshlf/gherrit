//! Literal revisions and structurally normalized publication history.
//!
//! Exact remote refs can describe either no state at all or one complete,
//! nonempty publication history. This module rejects every partial shape and
//! replaces each version target with facts read from that literal commit
//! object. It deliberately does not inspect commit messages or traverse
//! ancestry. Complete graph and change-identity validation belongs to #373;
//! #374 later owns acquisition when one of those required graph objects is
//! missing.

use std::collections::HashMap;

use color_eyre::eyre::{Context as _, Result, bail, eyre};
use gix::ObjectId;

use super::{local::GherritPrId, remote::RawExactLocalChange, version::Version};
use crate::util;

/// One commit and the literal first parent recorded in that commit object.
///
/// The fields are private because an arbitrary pair is not literal commit
/// evidence. This boundary intentionally does not require the parent object
/// itself: complete graph validation and missing-object acquisition are later
/// concerns.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub(super) struct Revision {
    head: ObjectId,
    first_parent: ObjectId,
}

impl Revision {
    fn from_literal_commit(repository: &util::Repo, head: ObjectId) -> Result<Self> {
        let object = repository
            .try_find_object(head)
            .wrap_err_with(|| format!("Failed to read object {head}"))?
            .ok_or_else(|| eyre!("Commit object {head} is missing"))?;
        if object.kind != gix::object::Kind::Commit {
            bail!("Object {head} is {}, not a commit", object.kind);
        }
        let commit = object.try_into_commit().map_err(|error| eyre!(error))?;
        let decoded =
            commit.decode().wrap_err_with(|| format!("Commit {head} has malformed encoding"))?;
        let first_parent =
            decoded.parents.first().ok_or_else(|| eyre!("Commit {head} has no first parent"))?;
        let first_parent = ObjectId::from_hex(first_parent)
            .wrap_err_with(|| format!("Commit {head} has an invalid first parent"))?;
        Ok(Self { head, first_parent })
    }

    pub(super) fn head(self) -> ObjectId {
        self.head
    }

    pub(super) fn first_parent(self) -> ObjectId {
        self.first_parent
    }
}

/// Resolves each distinct head once while replaying every immutable slot.
///
/// This cache is deliberately scoped to one normalized change. #373 can lift
/// literal evidence across changes after it owns the complete graph.
fn resolve_version_slots(
    slots: &[(Version, ObjectId)],
    mut load: impl FnMut(Version, ObjectId) -> Result<Revision>,
) -> Result<Vec<Revision>> {
    let mut cache = HashMap::new();
    slots
        .iter()
        .map(|(version, head)| {
            if let Some(revision) = cache.get(head) {
                return Ok(*revision);
            }
            let revision = load(*version, *head)?;
            cache.insert(*head, revision);
            Ok(revision)
        })
        .collect()
}

/// A structurally nonempty sequence of literal published revisions.
///
/// Version numbers are derived from positions: `first` is v1 and position
/// zero in `later` is v2. Every immutable tag position remains distinct,
/// including adjacent and nonadjacent repeats. A validated marker retains the
/// annotated tag object and its mandatory v1 target so later acquisition and
/// marker decoding cannot accidentally exchange either identity.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct PublishedHistory {
    first: Revision,
    later: Box<[Revision]>,
    pull_request_marker: Option<ObservedPullRequestMarker>,
}

/// Exact ref-level marker evidence, before the annotated tag object is loaded.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct ObservedPullRequestMarker {
    tag: ObjectId,
    v1: ObjectId,
}

impl ObservedPullRequestMarker {
    pub(super) fn tag(self) -> ObjectId {
        self.tag
    }

    pub(super) fn v1(self) -> ObjectId {
        self.v1
    }
}

impl PublishedHistory {
    pub(super) fn len(&self) -> usize {
        1 + self.later.len()
    }

    pub(super) fn iter(
        &self,
    ) -> impl DoubleEndedIterator<Item = Revision> + ExactSizeIterator + '_ {
        (0..self.len()).map(|index| if index == 0 { self.first } else { self.later[index - 1] })
    }

    pub(super) fn versioned(
        &self,
    ) -> impl DoubleEndedIterator<Item = (Version, Revision)> + ExactSizeIterator + '_ {
        self.iter().enumerate().map(|(index, revision)| {
            let version = Version::from_history_index(index)
                .expect("an in-memory history position always fits in u64");
            (version, revision)
        })
    }

    pub(super) fn current(&self) -> (Version, Revision) {
        let index = self.len() - 1;
        let version = Version::from_history_index(index)
            .expect("an in-memory history position always fits in u64");
        let revision = self.later.last().copied().unwrap_or(self.first);
        (version, revision)
    }

    pub(super) fn has_pull_request_marker(&self) -> bool {
        self.pull_request_marker.is_some()
    }

    pub(super) fn pull_request_marker(&self) -> Option<ObservedPullRequestMarker> {
        self.pull_request_marker
    }
}

/// One request-bound remote history after structural normalization.
///
/// `None` means the requested head, owned base, version namespace, and marker
/// were all absent. `Some` is necessarily nonempty and complete. The ID is
/// copied from the exact request-derived raw observation rather than inferred
/// from any commit message.
#[derive(Debug, Eq, PartialEq)]
pub(super) struct NormalizedHistory {
    id: GherritPrId,
    published: Option<PublishedHistory>,
}

impl NormalizedHistory {
    /// Borrows and normalizes all raw facts for one exactly requested change.
    pub(super) fn normalize(repository: &util::Repo, raw: &RawExactLocalChange) -> Result<Self> {
        let id = raw.id().clone();
        let candidate_head = raw.candidate_head();
        let owned_base = raw.owned_base();
        let version_count = raw.versions().len();
        let marker = raw.pull_request_marker();

        if version_count == 0 && marker.is_some() {
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

        let slots = raw
            .versions()
            .enumerate()
            .map(|(index, raw_version)| {
                let expected = Version::from_history_index(index).ok_or_else(|| {
                    eyre!("Remote GHerrit change '{}' has too many versions", id.as_str())
                })?;
                let actual = raw_version.version();
                if actual != expected {
                    bail!(
                        "Remote GHerrit change '{}' has noncontiguous version tags: expected v{expected}, observed v{actual}",
                        id.as_str()
                    );
                }
                Ok((expected, raw_version.object_id()))
            })
            .collect::<Result<Vec<_>>>()?;
        let latest_head =
            slots.last().expect("a complete published shape has at least one version").1;
        if candidate_head != Some(latest_head) {
            bail!(
                "Remote GHerrit change '{}' head does not match its latest version tag",
                id.as_str()
            );
        }
        let pull_request_marker = marker
            .map(|marker| {
                let v1 = slots.first().expect("a complete published shape is nonempty").1;
                if marker.v1() != v1 {
                    bail!(
                        "Pull-request marker for GHerrit change '{}' does not peel exactly to v1",
                        id.as_str()
                    );
                }
                Ok(ObservedPullRequestMarker { tag: marker.tag(), v1: marker.v1() })
            })
            .transpose()?;

        let mut revisions = resolve_version_slots(&slots, |version, head| {
            Revision::from_literal_commit(repository, head).wrap_err_with(|| {
                format!(
                    "Version v{version} of GHerrit change '{}' is not a complete literal revision",
                    id.as_str()
                )
            })
        })?
        .into_iter();
        let first = revisions.next().expect("a complete published shape has at least one version");
        let later = revisions.collect::<Box<[_]>>();
        let latest = later.last().copied().unwrap_or(first);
        if owned_base != Some(latest.first_parent()) {
            bail!(
                "Remote GHerrit change '{}' owned base does not match the latest version's first parent",
                id.as_str()
            );
        }

        let published = PublishedHistory { first, later, pull_request_marker };
        Ok(Self { id, published: Some(published) })
    }

    pub(super) fn id(&self) -> &GherritPrId {
        &self.id
    }

    pub(super) fn published(&self) -> Option<&PublishedHistory> {
        self.published.as_ref()
    }
}

#[cfg(test)]
mod tests {
    use gix::{ObjectId, prelude::Write as _};
    use tempfile::TempDir;

    use super::*;
    use crate::pre_push::{
        destination::DefaultBranch,
        remote::{ExactLocalQueryPlan, RawExactLocalObservation},
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
    }

    fn id(value: &str) -> GherritPrId {
        GherritPrId::from_ref_component(value.as_bytes()).expect("valid test change ID")
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
        let default = DefaultBranch::new("main".to_owned(), default_tip).unwrap();
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
            let tag = ObjectId::from_bytes_or_panic(&[0x44; 20]);
            output.push_str(&format!("{tag}\trefs/tags/gherrit/{}/pr\n", id.as_str()));
            output.push_str(&format!("{target}\trefs/tags/gherrit/{}/pr^{{}}\n", id.as_str()));
        }
        ExactLocalQueryPlan::new(default, std::slice::from_ref(id))
            .unwrap()
            .decode([output.as_bytes()])
            .unwrap()
    }

    fn normalize(
        repository: &TestRepository,
        id: &GherritPrId,
        default_tip: ObjectId,
        candidate_head: Option<ObjectId>,
        owned_base: Option<ObjectId>,
        versions: &[(u64, ObjectId)],
        marker_target: Option<ObjectId>,
    ) -> Result<NormalizedHistory> {
        let observed =
            observe(id, default_tip, candidate_head, owned_base, versions, marker_target);
        NormalizedHistory::normalize(&repository.open(), observed.iter().next().unwrap())
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
                            assert_eq!(normalized.id(), &change_id);
                            assert_eq!(normalized.published().is_some(), has_versions);
                            if let Some(history) = normalized.published() {
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
            let history = normalized.published().unwrap();
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
            let (current_version, current_revision) = history.current();
            assert_eq!(current_version.get(), expected.len() as u64);
            assert_eq!(current_revision.head(), *expected.last().unwrap());
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
        assert!(marker.to_string().contains("does not peel exactly to v1"));

        let slots = [(version(1), a), (version(2), a), (version(3), b), (version(4), a)];
        let mut loads = HashMap::<ObjectId, usize>::new();
        let resolved = resolve_version_slots(&slots, |_, head| {
            *loads.entry(head).or_default() += 1;
            Ok(Revision { head, first_parent: root })
        })
        .unwrap();
        assert_eq!(
            resolved.iter().map(|revision| revision.head()).collect::<Vec<_>>(),
            [a, a, b, a]
        );
        assert_eq!(loads, HashMap::from([(a, 1), (b, 1)]));
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
        let history = normalized.published().unwrap();
        assert_eq!(history.current().1, Revision { head, first_parent: root });
    }

    #[test]
    fn marker_must_peel_exactly_to_v1_and_retains_both_ref_objects() {
        let repository = TestRepository::new();
        let root = repository.commit("root", &[]);
        let a = repository.commit("A", &[root]);
        let b = repository.commit("B", &[root]);
        let unrelated = repository.commit("unrelated", &[root]);
        let missing = ObjectId::from_bytes_or_panic(&[0x55; 20]);
        let change_id = id("Gone");
        let versions = [(1, a), (2, b), (3, a)];

        let normalized =
            normalize(&repository, &change_id, root, Some(a), Some(root), &versions, Some(a))
                .expect("marker peels exactly to v1");
        let marker = normalized.published().unwrap().pull_request_marker().unwrap();
        assert_eq!(marker.tag(), ObjectId::from_bytes_or_panic(&[0x44; 20]));
        assert_eq!(marker.v1(), a);

        for marker in [b, root, unrelated, missing] {
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
            assert!(error.to_string().contains("does not peel exactly to v1"));
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
        let missing_error = Revision::from_literal_commit(&repository.open(), missing).unwrap_err();
        assert_eq!(missing_error.to_string(), format!("Commit object {missing} is missing"));

        let corrupt = ObjectId::from_bytes_or_panic(&[0x69; 20]);
        repository.corrupt_loose_object(corrupt);
        let read_error = Revision::from_literal_commit(&repository.open(), corrupt).unwrap_err();
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
    fn reads_only_the_literal_head_without_traversing_its_parent() {
        let repository = TestRepository::new();
        let default_tip = repository.commit("default", &[]);
        let missing_parent = ObjectId::from_bytes_or_panic(&[0x77; 20]);
        let head = repository.commit("head", &[missing_parent]);
        let change_id = id("Gone");
        let normalized = normalize(
            &repository,
            &change_id,
            default_tip,
            Some(head),
            Some(missing_parent),
            &[(1, head)],
            None,
        )
        .expect("graph completeness belongs to later validation");
        assert_eq!(normalized.published().unwrap().current().1.first_parent(), missing_parent);
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
            normalized.published().unwrap().iter().map(Revision::first_parent).collect::<Vec<_>>(),
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
            let first = NormalizedHistory::normalize(&repository.open(), raw).unwrap();
            let second = NormalizedHistory::normalize(&repository.open(), raw).unwrap();
            assert_eq!(first.id(), &requested);
            assert_eq!(second.id(), &requested);
            assert_eq!(raw.id(), &requested, "normalization must borrow raw evidence");
        }
    }
}

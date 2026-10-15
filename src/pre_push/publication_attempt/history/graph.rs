//! Literal graph loading, acquisition causes and reachability.

use std::{collections::VecDeque, fmt};

use super::*;

#[derive(Debug)]
pub(super) struct CommitFacts {
    pub(super) parents: Box<[ObjectId]>,
    pub(super) identities: Box<[GherritIdTrailer]>,
}

/// Complete literal all-parent graph shared by every exact local history.
#[derive(Debug)]
pub(in crate::pre_push::publication_attempt) struct CommitGraphEvidence {
    pub(super) commits: HashMap<ObjectId, CommitFacts>,
}

/// Separates exact acquisition capabilities from invalid local graph evidence.
#[derive(Debug)]
pub(in crate::pre_push::publication_attempt) enum GraphLoadError<'root> {
    MissingAdvertisedRoot(MissingAdvertisedRoot<'root>),
    MissingAncestor(MissingAncestor<'root>),
    MissingMarkerObject(&'root ExternalMarkerObject),
    Invalid(Report),
}

#[derive(Debug)]
pub(in crate::pre_push::publication_attempt) struct MissingAdvertisedRoot<'root> {
    root: &'root ExternalGraphRoot,
}

impl<'root> MissingAdvertisedRoot<'root> {
    #[cfg(test)]
    pub(in crate::pre_push::publication_attempt) fn root(&self) -> &'root ExternalGraphRoot {
        self.root
    }
}

#[derive(Debug)]
pub(in crate::pre_push::publication_attempt) struct MissingAncestor<'root> {
    oid: ObjectId,
    root: &'root ExternalGraphRoot,
}

impl<'root> MissingAncestor<'root> {
    pub(in crate::pre_push::publication_attempt) fn oid(&self) -> ObjectId {
        self.oid
    }

    pub(in crate::pre_push::publication_attempt) fn root(&self) -> &'root ExternalGraphRoot {
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
            Self::MissingMarkerObject(marker) => write!(
                formatter,
                "Advertised pull-request marker tag object {} is missing",
                marker.oid
            ),
            Self::Invalid(error) => error.fmt(formatter),
        }
    }
}

impl std::error::Error for GraphLoadError<'_> {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::MissingAdvertisedRoot(_)
            | Self::MissingAncestor(_)
            | Self::MissingMarkerObject(_) => None,
            Self::Invalid(error) => error.source(),
        }
    }
}

impl GraphLoadError<'_> {
    pub(in crate::pre_push::publication_attempt) fn into_report(self) -> Report {
        match self {
            Self::Invalid(error) => error,
            error => eyre!(error.to_string()),
        }
    }
}

#[cfg(test)]
#[derive(Debug)]
pub(super) enum TestGraphLoadError {
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
pub(super) fn graph_load_report(error: TestGraphLoadError) -> Report {
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
    /// Decode each available commit once, retaining deterministic root and
    /// parent order. Classify every available ancestry before acquisition:
    /// a missing ancestor requires refetch even if another root is absent.
    pub(super) fn load<'root>(
        repository: &util::Repo,
        roots: &'root [ExternalGraphRoot],
    ) -> std::result::Result<Self, GraphLoadError<'root>> {
        // None denotes an advertised root not yet encountered as an ancestor.
        let mut scheduled = HashMap::new();
        let mut pending = roots
            .iter()
            .filter(|root| {
                if scheduled.contains_key(&root.oid()) {
                    return false;
                }
                scheduled.insert(root.oid(), None);
                true
            })
            .map(|root| (root.oid(), root))
            .collect::<VecDeque<_>>();
        let mut commits = HashMap::new();
        let mut missing = Vec::new();

        while let Some((oid, causal_root)) = pending.pop_front() {
            let object = repository.try_find_object(oid).map_err(|error| {
                GraphLoadError::Invalid(
                    Report::new(error).wrap_err(format!("Failed to read object {oid}")),
                )
            })?;
            let Some(object) = object else {
                missing.push((oid, causal_root));
                continue;
            };
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
            let identities = gherrit_id_trailers(decoded.message).into_boxed_slice();
            let parents = decoded
                .parents
                .iter()
                .map(|parent| {
                    ObjectId::from_hex(parent)
                        .map_err(|error| {
                            GraphLoadError::Invalid(
                                Report::new(error)
                                    .wrap_err(format!("Commit {oid} has an invalid parent ID")),
                            )
                        })
                        .and_then(|parent| {
                            if parent.is_null() {
                                Err(GraphLoadError::Invalid(eyre!(
                                    "Commit {oid} has a null parent ID"
                                )))
                            } else {
                                Ok(parent)
                            }
                        })
                })
                .collect::<std::result::Result<Box<[_]>, _>>()?;
            for parent in parents.iter().copied() {
                use std::collections::hash_map::Entry;
                match scheduled.entry(parent) {
                    Entry::Vacant(entry) => {
                        entry.insert(Some(causal_root));
                        pending.push_back((parent, causal_root));
                    }
                    Entry::Occupied(mut entry) => {
                        // An absent advertised root can also be an ancestor
                        // of a present root; that stronger evidence must win.
                        entry.get_mut().get_or_insert(causal_root);
                    }
                }
            }
            assert!(commits.insert(oid, CommitFacts { parents, identities }).is_none());
        }

        for (oid, _) in &missing {
            if let Some(root) = scheduled[oid] {
                return Err(GraphLoadError::MissingAncestor(MissingAncestor { oid: *oid, root }));
            }
        }
        if let Some((_, root)) = missing.first() {
            return Err(GraphLoadError::MissingAdvertisedRoot(MissingAdvertisedRoot { root }));
        }
        Ok(Self { commits })
    }

    #[cfg(test)]
    pub(super) fn load_for_test(
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
            GraphLoadError::MissingMarkerObject(_) => {
                unreachable!("the commit-only test loader has no marker objects")
            }
            GraphLoadError::Invalid(error) => TestGraphLoadError::Invalid(error),
        })
    }

    pub(super) fn revision(&self, head: ObjectId) -> Result<Revision> {
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

    pub(super) fn parents(&self, oid: ObjectId) -> &[ObjectId] {
        &self.commits.get(&oid).expect("complete graph contains every revision").parents
    }

    pub(super) fn identities(&self, oid: ObjectId) -> &[GherritIdTrailer] {
        &self.commits.get(&oid).expect("complete graph contains every revision").identities
    }

    pub(super) fn first_reachable(
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

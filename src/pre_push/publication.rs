use std::{num::NonZeroUsize, slice};

use color_eyre::eyre::{Result, bail, eyre};
use gix::ObjectId;

// Windows command lines are limited to roughly 32 KiB. Each target contributes
// about 200 characters of branch and tag refspecs, so 80 targets leave ample
// headroom.
const PUSH_BATCH_LEN: usize = 80;
// Each commit contributes a branch and wildcard tag query. Limiting batches to
// 120 commits leaves ample headroom under Windows' roughly 32 KiB command-line
// limit, including unusually long remote names.
const REMOTE_QUERY_BATCH_LEN: usize = 120;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct Checkpoint {
    pub object_id: ObjectId,
    pub version: NonZeroUsize,
}

/// A validated, contiguous remote version history.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct RemoteHistory {
    first: ObjectId,
    later: Vec<ObjectId>,
}

impl RemoteHistory {
    pub(super) fn new(first: ObjectId, later: Vec<ObjectId>) -> Self {
        Self { first, later }
    }

    fn checkpoint(&self, version: NonZeroUsize) -> Option<Checkpoint> {
        let object_id = match version.get() {
            1 => Some(self.first),
            version => self.later.get(version - 2).copied(),
        }?;
        Some(Checkpoint { object_id, version })
    }

    fn latest(&self) -> Checkpoint {
        let version = NonZeroUsize::new(self.later.len() + 1)
            .expect("a remote history always contains its first version");
        let object_id = self.later.last().copied().unwrap_or(self.first);
        Checkpoint { object_id, version }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum RemotePublication {
    Unpublished,
    Published(RemoteHistory),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct CheckpointTarget<'a> {
    pub gherrit_id: &'a str,
    pub checkpoint: Checkpoint,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct PushTarget<'a> {
    checkpoint: CheckpointTarget<'a>,
    expected_remote_oid: Option<ObjectId>,
}

impl<'a> PushTarget<'a> {
    fn initial(gherrit_id: &'a str, desired_oid: ObjectId) -> Self {
        Self {
            checkpoint: CheckpointTarget {
                gherrit_id,
                checkpoint: Checkpoint { object_id: desired_oid, version: NonZeroUsize::MIN },
            },
            expected_remote_oid: None,
        }
    }

    fn advance(gherrit_id: &'a str, desired_oid: ObjectId, latest: Checkpoint) -> Result<Self> {
        let version = next_version(Some(latest.version))
            .ok_or_else(|| eyre!("GHerrit version overflow for '{gherrit_id}'"))?;
        Ok(Self {
            checkpoint: CheckpointTarget {
                gherrit_id,
                checkpoint: Checkpoint { object_id: desired_oid, version },
            },
            expected_remote_oid: Some(latest.object_id),
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum PublicationAction<'a> {
    NoPush(CheckpointTarget<'a>),
    Push(PushTarget<'a>),
}

impl PublicationAction<'_> {
    pub(super) fn required_checkpoint(&self) -> CheckpointTarget<'_> {
        match self {
            Self::NoPush(target) => *target,
            Self::Push(target) => target.checkpoint,
        }
    }
}

pub(super) fn parse_version(value: &str) -> Option<NonZeroUsize> {
    let version = value.parse::<NonZeroUsize>().ok()?;
    (version.get().to_string() == value).then_some(version)
}

pub(super) fn next_version(latest: Option<NonZeroUsize>) -> Option<NonZeroUsize> {
    latest.map_or(Some(NonZeroUsize::MIN), |latest| {
        latest.get().checked_add(1).and_then(NonZeroUsize::new)
    })
}

pub(super) fn plan_publication<'a>(
    remote_name: &str,
    gherrit_id: &'a str,
    desired_oid: ObjectId,
    local: Option<Checkpoint>,
    remote: RemotePublication,
) -> Result<PublicationAction<'a>> {
    let target = |checkpoint| CheckpointTarget { gherrit_id, checkpoint };

    let RemotePublication::Published(history) = remote else {
        if let Some(local) = local {
            bail!(
                "Local checkpoint refs/tags/gherrit/{gherrit_id}/v{} has no matching remote publication",
                local.version.get()
            );
        }
        return Ok(PublicationAction::Push(PushTarget::initial(gherrit_id, desired_oid)));
    };

    if let Some(local) = local {
        let Some(remote_checkpoint) = history.checkpoint(local.version) else {
            bail!(
                "Local checkpoint refs/tags/gherrit/{gherrit_id}/v{} is ahead of the remote publication history",
                local.version.get()
            );
        };
        if remote_checkpoint != local {
            bail!(
                "Local checkpoint refs/tags/gherrit/{gherrit_id}/v{} does not match the remote tag",
                local.version.get()
            );
        }
    }

    let latest = history.latest();
    if latest.object_id == desired_oid {
        return Ok(PublicationAction::NoPush(target(latest)));
    }

    if local != Some(latest) {
        bail!(
            "Remote publication for '{gherrit_id}' advanced to v{} since this checkout's last observed version. Run `git fetch {remote_name} 'refs/tags/gherrit/{gherrit_id}/*:refs/tags/gherrit/{gherrit_id}/*'` and rebase before retrying.",
            latest.version.get()
        );
    }

    Ok(PublicationAction::Push(PushTarget::advance(gherrit_id, desired_oid, latest)?))
}

pub(super) fn confirmed_checkpoint(
    required: CheckpointTarget<'_>,
    remote: RemotePublication,
) -> Result<Checkpoint> {
    let CheckpointTarget { gherrit_id, checkpoint: required_checkpoint } = required;
    let RemotePublication::Published(history) = remote else {
        bail!(
            "Remote publication for '{gherrit_id}' disappeared before PR projection. Remote Git writes may already have succeeded, but no PR mutations were attempted; retrying is safe."
        );
    };

    match history.checkpoint(required_checkpoint.version) {
        None => bail!(
            "Remote publication checkpoint v{} for '{gherrit_id}' disappeared before PR projection. Remote Git writes may already have succeeded, but no PR mutations were attempted; retrying is safe.",
            required_checkpoint.version.get()
        ),
        Some(observed) if observed != required_checkpoint => bail!(
            "Remote publication checkpoint v{} for '{gherrit_id}' changed from {} to {} before PR projection. No PR mutations were attempted; fetch and rebase before retrying.",
            required_checkpoint.version.get(),
            required_checkpoint.object_id,
            observed.object_id
        ),
        Some(_) => {}
    }

    let latest = history.latest();
    if latest.object_id != required_checkpoint.object_id {
        bail!(
            "Remote publication for '{gherrit_id}' advanced to v{} ({}) instead of the desired commit {} before PR projection. No PR mutations were attempted; fetch and rebase before retrying.",
            latest.version.get(),
            latest.object_id,
            required_checkpoint.object_id
        );
    }
    Ok(latest)
}

pub(super) fn push_batches<T>(items: &[T]) -> slice::Chunks<'_, T> {
    items.chunks(PUSH_BATCH_LEN)
}

pub(super) fn remote_query_batches<T>(items: &[T]) -> slice::Chunks<'_, T> {
    items.chunks(REMOTE_QUERY_BATCH_LEN)
}

pub(super) fn plan_push(remote: &str, targets: &[PushTarget<'_>]) -> Vec<String> {
    assert!(!targets.is_empty(), "cannot plan an empty push");
    let refspecs = targets.iter().flat_map(|target| {
        let CheckpointTarget { gherrit_id, checkpoint } = target.checkpoint;
        let branch = format!("refs/heads/{gherrit_id}");
        let tag = format!("refs/tags/gherrit/{gherrit_id}/v{}", checkpoint.version.get());
        let expected_remote =
            target.expected_remote_oid.map(|object_id| object_id.to_string()).unwrap_or_default();
        // Branch updates are leased against the observed remote value. A tag
        // lease with an empty expected value requires that the version tag not
        // exist, making it a lock rather than an overwrite.
        [
            format!("{}:{branch}", checkpoint.object_id),
            format!("--force-with-lease={branch}:{expected_remote}"),
            format!("{}:{tag}", checkpoint.object_id),
            format!("--force-with-lease={tag}:"),
        ]
    });
    ["push", "--quiet", "--no-verify", "--atomic", remote]
        .into_iter()
        .map(ToString::to_string)
        .chain(refspecs)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn batch_lengths(batches: slice::Chunks<'_, usize>) -> Vec<usize> {
        batches.map(<[usize]>::len).collect()
    }

    fn object_id(byte: u8) -> ObjectId {
        ObjectId::from_bytes_or_panic(&[byte; 20])
    }

    fn version(value: usize) -> NonZeroUsize {
        NonZeroUsize::new(value).unwrap()
    }

    #[test]
    fn plans_push_batch_boundaries() {
        for (item_count, expected) in [
            (0, vec![]),
            (1, vec![1]),
            (79, vec![79]),
            (80, vec![80]),
            (81, vec![80, 1]),
            (160, vec![80, 80]),
            (161, vec![80, 80, 1]),
        ] {
            let items = (0..item_count).collect::<Vec<_>>();
            assert_eq!(batch_lengths(push_batches(&items)), expected);
        }
    }

    #[test]
    fn plans_remote_query_batch_boundaries() {
        for (item_count, expected) in
            [(0, vec![]), (1, vec![1]), (120, vec![120]), (121, vec![120, 1])]
        {
            let items = (0..item_count).collect::<Vec<_>>();
            assert_eq!(batch_lengths(remote_query_batches(&items)), expected);
        }
    }

    #[test]
    fn plans_atomic_branch_and_tag_leases() {
        let targets = [
            PushTarget::advance(
                "Gone",
                object_id(0x11),
                Checkpoint { object_id: object_id(0x33), version: version(1) },
            )
            .unwrap(),
            PushTarget::initial("Gtwo", object_id(0x22)),
        ];

        let arguments = plan_push("origin", &targets);

        assert_eq!(
            arguments,
            [
                "push".to_string(),
                "--quiet".to_string(),
                "--no-verify".to_string(),
                "--atomic".to_string(),
                "origin".to_string(),
                format!("{}:refs/heads/Gone", object_id(0x11)),
                format!("--force-with-lease=refs/heads/Gone:{}", object_id(0x33)),
                format!("{}:refs/tags/gherrit/Gone/v2", object_id(0x11)),
                "--force-with-lease=refs/tags/gherrit/Gone/v2:".to_string(),
                format!("{}:refs/heads/Gtwo", object_id(0x22)),
                "--force-with-lease=refs/heads/Gtwo:".to_string(),
                format!("{}:refs/tags/gherrit/Gtwo/v1", object_id(0x22)),
                "--force-with-lease=refs/tags/gherrit/Gtwo/v1:".to_string(),
            ]
        );
    }

    #[test]
    fn versions_are_nonzero_canonical_and_checked() {
        assert_eq!(parse_version("1"), Some(version(1)));
        assert_eq!(parse_version(&usize::MAX.to_string()), Some(version(usize::MAX)));

        for invalid in [
            "".to_string(),
            "0".to_string(),
            "01".to_string(),
            "+1".to_string(),
            "-1".to_string(),
            " 1".to_string(),
            "1v999".to_string(),
            "1/child".to_string(),
            ((usize::MAX as u128) + 1).to_string(),
        ] {
            assert_eq!(parse_version(&invalid), None, "value={invalid:?}");
        }

        assert_eq!(next_version(None), Some(version(1)));
        assert_eq!(next_version(Some(version(1))), Some(version(2)));
        assert_eq!(next_version(Some(version(usize::MAX))), None);
    }

    #[test]
    fn publication_actions_model_no_push_push_and_conflict() {
        let old = object_id(0x11);
        let desired = object_id(0x22);
        let v1 = Checkpoint { object_id: old, version: version(1) };
        let v2 = Checkpoint { object_id: desired, version: version(2) };
        let unpublished = RemotePublication::Unpublished;
        let published_v1 = RemotePublication::Published(RemoteHistory::new(old, vec![]));
        let published_v2 = RemotePublication::Published(RemoteHistory::new(old, vec![desired]));

        assert_eq!(
            plan_publication("origin", "Gone", old, None, unpublished.clone()).unwrap(),
            PublicationAction::Push(PushTarget::initial("Gone", old))
        );
        assert!(plan_publication("origin", "Gone", old, Some(v1), unpublished).is_err());
        assert_eq!(
            plan_publication("origin", "Gone", old, Some(v1), published_v1.clone()).unwrap(),
            PublicationAction::NoPush(CheckpointTarget { gherrit_id: "Gone", checkpoint: v1 })
        );
        assert_eq!(
            plan_publication("origin", "Gone", desired, Some(v1), published_v1).unwrap(),
            PublicationAction::Push(PushTarget::advance("Gone", desired, v1).unwrap())
        );
        assert_eq!(
            plan_publication("origin", "Gone", desired, Some(v1), published_v2.clone()).unwrap(),
            PublicationAction::NoPush(CheckpointTarget { gherrit_id: "Gone", checkpoint: v2 })
        );
        assert_eq!(
            plan_publication("origin", "Gone", desired, None, published_v2.clone()).unwrap(),
            PublicationAction::NoPush(CheckpointTarget { gherrit_id: "Gone", checkpoint: v2 })
        );
        assert!(
            plan_publication("origin", "Gone", old, None, published_v2.clone()).is_err(),
            "an uncheckpointed remote advance to different content must conflict"
        );
        assert!(
            plan_publication(
                "origin",
                "Gone",
                desired,
                Some(Checkpoint { object_id: object_id(0x33), version: version(1) }),
                published_v2.clone(),
            )
            .is_err(),
            "a local checkpoint must match its exact historical remote tag"
        );
        assert!(plan_publication("origin", "Gone", old, Some(v1), published_v2).is_err());
    }

    #[test]
    fn confirmation_requires_the_planned_checkpoint_and_desired_latest_oid() {
        let desired = object_id(0x22);
        let v2 = Checkpoint { object_id: desired, version: version(2) };

        assert_eq!(
            confirmed_checkpoint(
                CheckpointTarget { gherrit_id: "Gone", checkpoint: v2 },
                RemotePublication::Published(RemoteHistory::new(object_id(0x11), vec![desired])),
            )
            .unwrap(),
            v2
        );
        assert_eq!(
            confirmed_checkpoint(
                CheckpointTarget { gherrit_id: "Gone", checkpoint: v2 },
                RemotePublication::Published(RemoteHistory::new(
                    object_id(0x11),
                    vec![desired, desired],
                )),
            )
            .unwrap(),
            Checkpoint { object_id: desired, version: version(3) }
        );
        assert!(
            confirmed_checkpoint(
                CheckpointTarget { gherrit_id: "Gone", checkpoint: v2 },
                RemotePublication::Published(RemoteHistory::new(
                    object_id(0x11),
                    vec![desired, object_id(0x33)],
                )),
            )
            .is_err()
        );
        assert!(
            confirmed_checkpoint(
                CheckpointTarget { gherrit_id: "Gone", checkpoint: v2 },
                RemotePublication::Published(RemoteHistory::new(desired, vec![])),
            )
            .is_err(),
            "a rollback must not erase the checkpoint observed during planning"
        );
        assert!(
            confirmed_checkpoint(
                CheckpointTarget { gherrit_id: "Gone", checkpoint: v2 },
                RemotePublication::Published(RemoteHistory::new(
                    object_id(0x11),
                    vec![object_id(0x33), desired],
                )),
            )
            .is_err(),
            "the planned checkpoint must not be rewritten even if latest is desired"
        );
        assert!(
            confirmed_checkpoint(
                CheckpointTarget { gherrit_id: "Gone", checkpoint: v2 },
                RemotePublication::Unpublished,
            )
            .is_err()
        );
    }
}

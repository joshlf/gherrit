use std::slice;

use color_eyre::eyre::{Result, eyre};
use gix::ObjectId;

use super::legacy_remote::RemotePublication;

// Windows command lines are limited to roughly 32 KiB. Each target contributes
// about 200 characters of branch and tag refspecs, so 80 targets leave ample
// headroom.
const PUSH_BATCH_LEN: usize = 80;

#[derive(Debug)]
pub(super) struct PushTarget<'a> {
    object_id: ObjectId,
    gherrit_id: &'a str,
    version: usize,
    expected_remote_head: Option<ObjectId>,
}

/// The publication decision for one local change.
///
/// An unchanged destination head reuses its latest version without creating a
/// tag. An absent or divergent head carries the complete update target.
#[derive(Debug)]
pub(super) enum ChangePublication<'a> {
    Current { version: usize },
    Publish(PushTarget<'a>),
}

impl<'a> ChangePublication<'a> {
    pub(super) fn into_parts(self) -> (usize, Option<PushTarget<'a>>) {
        match self {
            Self::Current { version } => (version, None),
            Self::Publish(target) => (target.version, Some(target)),
        }
    }
}

/// Plans one change solely from the local head and authoritative destination
/// state. Local tags are deliberately not an input.
pub(super) fn plan_change<'a>(
    object_id: ObjectId,
    gherrit_id: &'a str,
    remote: RemotePublication,
) -> Result<ChangePublication<'a>> {
    match remote {
        RemotePublication::Absent => {
            let version = 1;
            let target = PushTarget { object_id, gherrit_id, version, expected_remote_head: None };
            Ok(ChangePublication::Publish(target))
        }
        RemotePublication::Published { head, latest_version } if head == object_id => {
            Ok(ChangePublication::Current { version: latest_version })
        }
        RemotePublication::Published { head, latest_version } => {
            let version = latest_version
                .checked_add(1)
                .ok_or_else(|| eyre!("Remote GHerrit change '{gherrit_id}' has no next version"))?;
            let target =
                PushTarget { object_id, gherrit_id, version, expected_remote_head: Some(head) };
            Ok(ChangePublication::Publish(target))
        }
    }
}

pub(super) struct PushPlan {
    pub options: Vec<String>,
    pub refspecs: Vec<String>,
}

pub(super) fn push_batches<T>(items: &[T]) -> slice::Chunks<'_, T> {
    items.chunks(PUSH_BATCH_LEN)
}

pub(super) fn plan_push(targets: &[PushTarget<'_>]) -> PushPlan {
    assert!(!targets.is_empty(), "cannot plan an empty push");
    let options = ["--quiet", "--atomic"]
        .into_iter()
        .map(str::to_owned)
        .chain(targets.iter().flat_map(|target| {
            let branch = format!("refs/heads/{}", target.gherrit_id);
            let tag = format!("refs/tags/gherrit/{}/v{}", target.gherrit_id, target.version);
            let expected_head =
                target.expected_remote_head.map(|head| head.to_string()).unwrap_or_default();
            [
                format!("--force-with-lease={branch}:{expected_head}"),
                format!("--force-with-lease={tag}:"),
            ]
        }))
        .collect();
    let refspecs = targets
        .iter()
        .flat_map(|target| {
            let branch = format!("refs/heads/{}", target.gherrit_id);
            let tag = format!("refs/tags/gherrit/{}/v{}", target.gherrit_id, target.version);
            // Branch updates are leased against the observed remote value. A tag
            // lease with an empty expected value requires that the version tag not
            // exist, making it a lock rather than an overwrite.
            [format!("{}:{branch}", target.object_id), format!("{}:{tag}", target.object_id)]
        })
        .collect();

    PushPlan { options, refspecs }
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

    #[test]
    fn plans_only_absent_and_divergent_changes() {
        let absent = plan_change(object_id(1), "Gabsent", RemotePublication::Absent).unwrap();
        let current = plan_change(
            object_id(2),
            "Gcurrent",
            RemotePublication::Published { head: object_id(2), latest_version: 7 },
        )
        .unwrap();
        let divergent = plan_change(
            object_id(4),
            "Gdivergent",
            RemotePublication::Published { head: object_id(3), latest_version: 7 },
        )
        .unwrap();

        let (version, target) = absent.into_parts();
        let target = target.unwrap();
        assert_eq!(version, 1);
        assert_eq!(target.version, 1);
        assert_eq!(target.expected_remote_head, None);

        assert!(matches!(current, ChangePublication::Current { version: 7 }));

        let (version, target) = divergent.into_parts();
        let target = target.unwrap();
        assert_eq!(version, 8);
        assert_eq!(target.version, 8);
        assert_eq!(target.expected_remote_head, Some(object_id(3)));
    }

    #[test]
    fn a_current_maximum_version_converges_but_a_divergent_one_rejects() {
        assert!(matches!(
            plan_change(
                object_id(1),
                "Gcurrent",
                RemotePublication::Published { head: object_id(1), latest_version: usize::MAX },
            )
            .unwrap(),
            ChangePublication::Current { version: usize::MAX }
        ));

        let error = plan_change(
            object_id(2),
            "Gdivergent",
            RemotePublication::Published { head: object_id(1), latest_version: usize::MAX },
        )
        .unwrap_err();
        assert_eq!(error.to_string(), "Remote GHerrit change 'Gdivergent' has no next version");
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
    fn plans_atomic_branch_and_tag_leases() {
        let targets = [
            PushTarget {
                object_id: object_id(0x11),
                gherrit_id: "Gone",
                version: 2,
                expected_remote_head: Some(object_id(0x33)),
            },
            PushTarget {
                object_id: object_id(0x22),
                gherrit_id: "Gtwo",
                version: 1,
                expected_remote_head: None,
            },
        ];

        let plan = plan_push(&targets);

        assert_eq!(
            plan.options,
            [
                "--quiet".to_string(),
                "--atomic".to_string(),
                format!("--force-with-lease=refs/heads/Gone:{}", object_id(0x33)),
                "--force-with-lease=refs/tags/gherrit/Gone/v2:".to_string(),
                "--force-with-lease=refs/heads/Gtwo:".to_string(),
                "--force-with-lease=refs/tags/gherrit/Gtwo/v1:".to_string(),
            ]
        );
        assert_eq!(
            plan.refspecs,
            [
                format!("{}:refs/heads/Gone", object_id(0x11)),
                format!("{}:refs/tags/gherrit/Gone/v2", object_id(0x11)),
                format!("{}:refs/heads/Gtwo", object_id(0x22)),
                format!("{}:refs/tags/gherrit/Gtwo/v1", object_id(0x22)),
            ]
        );
    }
}

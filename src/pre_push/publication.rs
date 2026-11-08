//! Builds the arguments and local tag writes for one Git push batch.
//!
//! Each change has a mutable branch and a numbered version tag pointing to the
//! submitted commit. The caller supplies observed branch tips and version
//! numbers chosen from local history, runs the command, and saves local tags
//! only after Git reports success. This module neither reads remote state nor
//! checks a push receipt or recovers from a failed attempt.

use std::slice;

use gix::ObjectId;

use super::{identity::ChangeId, refs};

// Windows command lines are limited to roughly 32 KiB. Each target contributes
// about 200 characters of branch and tag refspecs, so 80 targets leave ample
// headroom.
const PUSH_BATCH_LEN: usize = 80;
// Each queried branch is about 62 characters, making 250 branches roughly
// 15.5 KiB.
const REMOTE_QUERY_BATCH_LEN: usize = 250;

pub(super) struct PushTarget<'a> {
    pub object_id: ObjectId,
    pub gherrit_id: &'a ChangeId,
    pub version: usize,
    /// `None` means the branch was absent in the caller's observation.
    pub expected_remote_tip: Option<ObjectId>,
}

pub(super) struct PersistedTag {
    pub object_id: ObjectId,
    pub gherrit_id: ChangeId,
    pub version: usize,
}

pub(super) struct PushPlan {
    pub arguments: Vec<String>,
    /// Local writes to make after a successful push.
    ///
    /// These values do not prove that the remote push took effect. The caller
    /// runs the push and checks its result.
    pub persisted_tags: Vec<PersistedTag>,
}

pub(super) fn push_batches<T>(items: &[T]) -> slice::Chunks<'_, T> {
    items.chunks(PUSH_BATCH_LEN)
}

pub(super) fn remote_query_batches<T>(items: &[T]) -> slice::Chunks<'_, T> {
    items.chunks(REMOTE_QUERY_BATCH_LEN)
}

pub(super) fn plan_push(remote: &str, targets: &[PushTarget<'_>]) -> PushPlan {
    assert!(!targets.is_empty(), "cannot plan an empty push");
    let refspecs = targets.iter().flat_map(|target| {
        let branch = refs::head(target.gherrit_id.as_str());
        let tag = refs::version_tag(target.gherrit_id.as_str(), target.version);
        let expected = target.expected_remote_tip.map(|id| id.to_string()).unwrap_or_default();
        // Require the observed branch value and a missing version tag. These
        // lightweight refs store commit IDs, not unique attempt IDs. Git may
        // consider a ref with the requested value already up to date, so these
        // checks are not an exclusive lock that only one push can use.
        [
            format!("{}:{branch}", target.object_id),
            format!("--force-with-lease={branch}:{expected}"),
            format!("{}:{tag}", target.object_id),
            format!("--force-with-lease={tag}:"),
        ]
    });
    // The outer pre-push hook is already sending these generated refs.
    // --no-verify avoids running the hook recursively. --atomic covers this
    // batch of branches and tags, not other batches or GitHub PR changes.
    let arguments = ["push", "--quiet", "--no-verify", "--atomic", remote]
        .into_iter()
        .map(ToString::to_string)
        .chain(refspecs)
        .collect();
    let persisted_tags = targets
        .iter()
        .map(|target| PersistedTag {
            object_id: target.object_id,
            gherrit_id: target.gherrit_id.clone(),
            version: target.version,
        })
        .collect();

    PushPlan { arguments, persisted_tags }
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
            [(0, vec![]), (1, vec![1]), (250, vec![250]), (251, vec![250, 1])]
        {
            let items = (0..item_count).collect::<Vec<_>>();
            assert_eq!(batch_lengths(remote_query_batches(&items)), expected);
        }
    }

    #[test]
    fn plans_atomic_branch_and_tag_leases() {
        let targets = [
            PushTarget {
                object_id: object_id(0x11),
                gherrit_id: &ChangeId::parse(b"Gone").unwrap(),
                version: 2,
                expected_remote_tip: Some(object_id(0x33)),
            },
            PushTarget {
                object_id: object_id(0x22),
                gherrit_id: &ChangeId::parse(b"Gtwo").unwrap(),
                version: 1,
                expected_remote_tip: None,
            },
        ];

        let plan = plan_push("origin", &targets);

        assert_eq!(
            plan.arguments,
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
        assert_eq!(plan.persisted_tags.len(), 2);
        assert_eq!(plan.persisted_tags[0].object_id, object_id(0x11));
        assert_eq!(plan.persisted_tags[0].gherrit_id.as_str(), "Gone");
        assert_eq!(plan.persisted_tags[0].version, 2);
        assert_eq!(plan.persisted_tags[1].object_id, object_id(0x22));
        assert_eq!(plan.persisted_tags[1].gherrit_id.as_str(), "Gtwo");
        assert_eq!(plan.persisted_tags[1].version, 1);
    }
}

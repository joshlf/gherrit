use std::slice;

use gix::ObjectId;

// Windows command lines are limited to roughly 32 KiB. Each target contributes
// about 200 characters of branch and tag refspecs, so 80 targets leave ample
// headroom.
const PUSH_BATCH_LEN: usize = 80;
// Each queried branch is about 62 characters, making 250 branches roughly
// 15.5 KiB.
const REMOTE_QUERY_BATCH_LEN: usize = 250;

pub(super) struct PushTarget<'a> {
    pub object_id: ObjectId,
    pub gherrit_id: &'a str,
    pub version: usize,
    pub expected_remote_sha: &'a str,
}

pub(super) struct PersistedTag {
    pub object_id: ObjectId,
    pub gherrit_id: String,
    pub version: usize,
}

pub(super) struct PushPlan {
    pub arguments: Vec<String>,
    pub persisted_tags: Vec<PersistedTag>,
}

pub(super) fn push_batches<T>(items: &[T]) -> slice::Chunks<'_, T> {
    items.chunks(push_batch_len())
}

#[cfg(not(gherrit_test))]
const fn push_batch_len() -> usize {
    PUSH_BATCH_LEN
}

#[cfg(gherrit_test)]
fn push_batch_len() -> usize {
    std::env::var("GHERRIT_TEST_PUSH_BATCH_LEN").map_or(PUSH_BATCH_LEN, |value| {
        value
            .parse::<std::num::NonZeroUsize>()
            .expect("GHERRIT_TEST_PUSH_BATCH_LEN must be a positive integer")
            .get()
    })
}

pub(super) fn remote_query_batches<T>(items: &[T]) -> slice::Chunks<'_, T> {
    items.chunks(REMOTE_QUERY_BATCH_LEN)
}

pub(super) fn plan_push(remote: &str, targets: &[PushTarget<'_>]) -> PushPlan {
    assert!(!targets.is_empty(), "cannot plan an empty push");
    let refspecs = targets.iter().flat_map(|target| {
        let branch = format!("refs/heads/{}", target.gherrit_id);
        let tag = format!("refs/tags/gherrit/{}/v{}", target.gherrit_id, target.version);
        // Branch updates are leased against the observed remote value. A tag
        // lease with an empty expected value requires that the version tag not
        // exist, making it a lock rather than an overwrite.
        [
            format!("{}:{branch}", target.object_id),
            format!("--force-with-lease={branch}:{}", target.expected_remote_sha),
            format!("{}:{tag}", target.object_id),
            format!("--force-with-lease={tag}:"),
        ]
    });
    let arguments = ["push", "--quiet", "--no-verify", "--atomic", remote]
        .into_iter()
        .map(ToString::to_string)
        .chain(refspecs)
        .collect();
    let persisted_tags = targets
        .iter()
        .map(|target| PersistedTag {
            object_id: target.object_id,
            gherrit_id: target.gherrit_id.to_string(),
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
                gherrit_id: "Gone",
                version: 2,
                expected_remote_sha: "abc123",
            },
            PushTarget {
                object_id: object_id(0x22),
                gherrit_id: "Gtwo",
                version: 1,
                expected_remote_sha: "",
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
                "--force-with-lease=refs/heads/Gone:abc123".to_string(),
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
        assert_eq!(plan.persisted_tags[0].gherrit_id, "Gone");
        assert_eq!(plan.persisted_tags[0].version, 2);
        assert_eq!(plan.persisted_tags[1].object_id, object_id(0x22));
        assert_eq!(plan.persisted_tags[1].gherrit_id, "Gtwo");
        assert_eq!(plan.persisted_tags[1].version, 1);
    }
}

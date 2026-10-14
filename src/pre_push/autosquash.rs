use std::{error::Error, fmt};

use super::destination::DefaultBranch;

const RESERVED_PREFIXES: [&str; 3] = ["fixup!", "squash!", "amend!"];

fn is_pending(subject: &str) -> bool {
    RESERVED_PREFIXES.iter().any(|prefix| subject.starts_with(prefix))
}

#[derive(Debug, Eq, PartialEq)]
pub(super) struct PendingAutosquash {
    default_ref: String,
}

impl fmt::Display for PendingAutosquash {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            concat!(
                "Stack contains pending fixup/squash/amend commits.\n",
                "Please squash your history before publishing:\n",
                "    git rebase -i --autosquash {}",
            ),
            self.default_ref,
        )
    }
}

impl Error for PendingAutosquash {}

/// Ensures that a stack contains no commits reserved for autosquashing.
///
/// This policy runs over the entire stack before commit metadata is validated.
/// A temporary commit therefore takes precedence over errors that are only
/// meaningful once the stack is ready to publish, such as a missing GHerrit
/// ID. Guidance names the local ref whose tip was validated against the push
/// repository; the configured remote name need not name that same history.
pub(super) fn ensure_publishable<'a>(
    subjects: impl IntoIterator<Item = &'a str>,
    default_branch: &DefaultBranch,
) -> Result<(), PendingAutosquash> {
    subjects
        .into_iter()
        .any(is_pending)
        .then(|| PendingAutosquash { default_ref: default_branch.full_ref_name() })
        .map_or(Ok(()), Err)
}

#[cfg(test)]
mod tests {
    use gix::ObjectId;

    use super::*;

    fn default_branch(name: &str) -> DefaultBranch {
        DefaultBranch::new(name.to_owned(), ObjectId::from_bytes_or_panic(&[1; 20])).unwrap()
    }

    #[test]
    fn classifies_reserved_subject_prefixes() {
        let suffixes = ["", " subject", "\tsubject", "subject", " 世界"];

        RESERVED_PREFIXES.into_iter().for_each(|prefix| {
            suffixes.into_iter().for_each(|suffix| {
                let subject = format!("{prefix}{suffix}");
                assert!(is_pending(&subject), "subject: {subject:?}");
            });
        });
    }

    #[test]
    fn accepts_near_misses() {
        [
            "",
            "ordinary subject",
            "fixup",
            "squash",
            "amend",
            " fixup! subject",
            "Fixup! subject",
            "SQUASH! subject",
            "Amend! subject",
            "prefix fixup! subject",
            "refixup! subject",
        ]
        .into_iter()
        .for_each(|subject| {
            assert!(!is_pending(subject), "subject: {subject:?}");
        });
    }

    #[test]
    fn rejects_every_prefix_at_every_stack_position() {
        RESERVED_PREFIXES.into_iter().for_each(|prefix| {
            (0..4).for_each(|pending_index| {
                let subjects = (0..4)
                    .map(|index| if index == pending_index { prefix } else { "ordinary" })
                    .collect::<Vec<_>>();

                assert!(
                    ensure_publishable(subjects, &default_branch("main")).is_err(),
                    "prefix: {prefix:?}, position: {pending_index}",
                );
            });
        });
    }

    #[test]
    fn accepts_empty_and_ordinary_stacks() {
        let default_branch = default_branch("main");
        assert_eq!(ensure_publishable([], &default_branch), Ok(()));
        assert_eq!(ensure_publishable(["one", "two", "three"], &default_branch), Ok(()));
    }

    #[test]
    fn reports_the_validated_local_rebase_target() {
        ["main", "master", "release/trunk"].into_iter().for_each(|branch| {
            let error =
                ensure_publishable(["fixup! subject"], &default_branch(branch)).unwrap_err();

            assert_eq!(
                error.to_string(),
                format!(
                    concat!(
                        "Stack contains pending fixup/squash/amend commits.\n",
                        "Please squash your history before publishing:\n",
                        "    git rebase -i --autosquash refs/heads/{}",
                    ),
                    branch,
                ),
            );
        });
    }
}

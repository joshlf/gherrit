use std::{error::Error, fmt};

const RESERVED_PREFIXES: [&str; 3] = ["fixup!", "squash!", "amend!"];

fn is_pending(subject: &str) -> bool {
    RESERVED_PREFIXES.iter().any(|prefix| subject.starts_with(prefix))
}

#[derive(Debug, Eq, PartialEq)]
pub(super) struct PendingAutosquash {
    remote: String,
    default_branch: String,
}

impl fmt::Display for PendingAutosquash {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            concat!(
                "Stack contains pending fixup/squash/amend commits.\n",
                "Please squash your history before syncing:\n",
                "    git rebase -i --autosquash {}/{}",
            ),
            self.remote, self.default_branch,
        )
    }
}

impl Error for PendingAutosquash {}

/// Ensures that a stack contains no commits reserved for autosquashing.
///
/// This policy runs over the entire stack before commit metadata is validated.
/// A temporary commit therefore takes precedence over errors that are only
/// meaningful once the stack is ready to publish, such as a missing GHerrit
/// ID.
pub(super) fn ensure_publishable<'a>(
    subjects: impl IntoIterator<Item = &'a str>,
    remote: &str,
    default_branch: &str,
) -> Result<(), PendingAutosquash> {
    subjects
        .into_iter()
        .any(is_pending)
        .then(|| PendingAutosquash {
            remote: remote.to_owned(),
            default_branch: default_branch.to_owned(),
        })
        .map_or(Ok(()), Err)
}

#[cfg(test)]
mod tests {
    use super::*;

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
                    ensure_publishable(subjects, "origin", "main").is_err(),
                    "prefix: {prefix:?}, position: {pending_index}",
                );
            });
        });
    }

    #[test]
    fn accepts_empty_and_ordinary_stacks() {
        assert_eq!(ensure_publishable([], "origin", "main"), Ok(()));
        assert_eq!(ensure_publishable(["one", "two", "three"], "origin", "main"), Ok(()));
    }

    #[test]
    fn reports_the_configured_rebase_target() {
        [("origin", "main"), ("upstream", "master"), (".", "trunk")].into_iter().for_each(
            |(remote, branch)| {
                let error = ensure_publishable(["fixup! subject"], remote, branch).unwrap_err();

                assert_eq!(
                    error.to_string(),
                    format!(
                        concat!(
                            "Stack contains pending fixup/squash/amend commits.\n",
                            "Please squash your history before syncing:\n",
                            "    git rebase -i --autosquash {}/{}",
                        ),
                        remote, branch,
                    ),
                );
            },
        );
    }
}

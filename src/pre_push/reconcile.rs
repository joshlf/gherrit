use serde::Deserialize;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub(super) enum PullRequestState {
    Open,
    Closed,
    Merged,
}

/// A PR lifecycle observation that forbids mutation.
///
/// `Open` is unrepresentable, so consumers do not rely on a field invariant.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum NonOpenPullRequest {
    Closed { number: u64 },
    Merged { number: u64 },
}

impl NonOpenPullRequest {
    /// Converts a lifecycle observation into rejection evidence.
    ///
    /// Returns `None` exactly when the pull request is open.
    fn from_state(number: u64, state: PullRequestState) -> Option<Self> {
        match state {
            PullRequestState::Open => None,
            PullRequestState::Closed => Some(Self::Closed { number }),
            PullRequestState::Merged => Some(Self::Merged { number }),
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
pub(super) struct NonOpenPullRequests {
    pull_requests: Vec<NonOpenPullRequest>,
}

impl std::fmt::Display for NonOpenPullRequests {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.pull_requests.iter().try_for_each(|pull_request| {
            let (state, number) = match *pull_request {
                NonOpenPullRequest::Closed { number } => ("closed", number),
                NonOpenPullRequest::Merged { number } => ("merged", number),
            };
            writeln!(
                formatter,
                "Cannot push to {state} PR #{number}. Please open a new PR or reopen the existing one."
            )
        })?;
        write!(formatter, "You may want to rebase on the latest changes before pushing.")
    }
}

impl std::error::Error for NonOpenPullRequests {}

/// Rejects a stack containing any closed or merged pull request.
pub(super) fn ensure_pull_requests_open(
    pull_requests: impl IntoIterator<Item = (u64, PullRequestState)>,
) -> Result<(), NonOpenPullRequests> {
    let pull_requests = pull_requests
        .into_iter()
        .filter_map(|(number, state)| NonOpenPullRequest::from_state(number, state))
        .collect::<Vec<_>>();

    if pull_requests.is_empty() { Ok(()) } else { Err(NonOpenPullRequests { pull_requests }) }
}

/// A stack item annotated with the base branch needed to project it as a PR.
#[derive(Debug, PartialEq, Eq)]
pub(super) struct StackEntry<T> {
    pub(super) item: T,
    pub(super) base_branch: String,
}

/// Derives stack topology from items ordered from base to head.
pub(super) fn link_stack<T>(
    base_branch: &str,
    items: impl IntoIterator<Item = T>,
    mut id_of: impl FnMut(&T) -> String,
) -> Vec<StackEntry<T>> {
    let mut previous_id = None;

    items
        .into_iter()
        .map(|item| {
            let id = id_of(&item);
            let base_branch = previous_id.replace(id).unwrap_or_else(|| base_branch.to_owned());

            StackEntry { item, base_branch }
        })
        .collect()
}

/// Metadata currently stored on a PR.
pub(super) struct CurrentPr<'a> {
    pub(super) node_id: &'a str,
    pub(super) title: Option<&'a str>,
    pub(super) body: Option<&'a str>,
    pub(super) base_branch: &'a str,
}

/// Metadata derived from a local commit and its stack position.
pub(super) struct DesiredPr<'a> {
    pub(super) title: &'a str,
    pub(super) body: &'a str,
    pub(super) base_branch: &'a str,
}

/// The fields that must be changed to reconcile a PR.
#[derive(Debug, PartialEq, Eq)]
pub(super) struct PrUpdate {
    /// The global node ID of the PR to update.
    pub(super) node_id: String,
    pub(super) title: Option<String>,
    pub(super) body: Option<String>,
    // Omitting an unchanged base branch is required for PRs in the merge queue:
    // GitHub rejects even a no-op base update for those PRs. See #271.
    pub(super) base_branch: Option<String>,
}

/// Returns the minimal update needed to make `current` match `desired`.
pub(super) fn plan_update(current: CurrentPr<'_>, desired: DesiredPr<'_>) -> Option<PrUpdate> {
    let title = (current.title != Some(desired.title)).then(|| desired.title.to_string());
    let body = current
        .body
        .is_none_or(|body| normalize_body(body) != normalize_body(desired.body))
        .then(|| desired.body.to_string());
    let base_branch =
        (current.base_branch != desired.base_branch).then(|| desired.base_branch.to_string());

    (title.is_some() || body.is_some() || base_branch.is_some()).then(|| PrUpdate {
        node_id: current.node_id.to_string(),
        title,
        body,
        base_branch,
    })
}

fn normalize_body(body: &str) -> String {
    body.replace("\r\n", "\n").trim().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    const PR_STATES: [PullRequestState; 3] =
        [PullRequestState::Open, PullRequestState::Closed, PullRequestState::Merged];

    #[test]
    fn pull_request_lifecycle_policy_covers_every_state() {
        let cases = [
            (PullRequestState::Open, None),
            (
                PullRequestState::Closed,
                Some(
                    "Cannot push to closed PR #42. Please open a new PR or reopen the existing one.\n\
                     You may want to rebase on the latest changes before pushing.",
                ),
            ),
            (
                PullRequestState::Merged,
                Some(
                    "Cannot push to merged PR #42. Please open a new PR or reopen the existing one.\n\
                     You may want to rebase on the latest changes before pushing.",
                ),
            ),
        ];

        cases.into_iter().for_each(|(state, expected_error)| {
            let error =
                ensure_pull_requests_open([(42, state)]).err().map(|error| error.to_string());
            assert_eq!(error.as_deref(), expected_error, "state={state:?}");
        });
    }

    #[test]
    fn pull_request_lifecycle_policy_is_exhaustive_for_two_pr_stacks() {
        PR_STATES.into_iter().for_each(|first_state| {
            PR_STATES.into_iter().for_each(|second_state| {
                let observed = [(11, first_state), (22, second_state)];
                let expected = observed
                    .into_iter()
                    .filter_map(|(number, state)| NonOpenPullRequest::from_state(number, state))
                    .collect::<Vec<_>>();
                let actual = ensure_pull_requests_open(observed)
                    .err()
                    .map(|error| error.pull_requests)
                    .unwrap_or_default();

                assert_eq!(actual, expected, "states=({first_state:?}, {second_state:?})");
            });
        });
    }

    #[test]
    fn pull_request_lifecycle_diagnostic_reports_every_violation_in_order() {
        let error = ensure_pull_requests_open([
            (11, PullRequestState::Merged),
            (22, PullRequestState::Open),
            (33, PullRequestState::Closed),
        ])
        .unwrap_err();

        assert_eq!(
            error.to_string(),
            "Cannot push to merged PR #11. Please open a new PR or reopen the existing one.\n\
             Cannot push to closed PR #33. Please open a new PR or reopen the existing one.\n\
             You may want to rebase on the latest changes before pushing."
        );
    }

    fn link_ids(base_branch: &str, ids: &[&str]) -> Vec<StackEntry<String>> {
        link_stack(base_branch, ids.iter().copied().map(str::to_owned), Clone::clone)
    }

    #[test]
    fn links_an_empty_stack() {
        assert_eq!(link_ids("main", &[]), []);
    }

    #[test]
    fn links_a_single_commit_to_the_base() {
        assert_eq!(
            link_ids("main", &["A"]),
            [StackEntry { item: "A".to_string(), base_branch: "main".to_string() }]
        );
    }

    #[test]
    fn links_each_commit_to_its_parent() {
        assert_eq!(
            link_ids("main", &["A", "B", "C"]),
            [
                StackEntry { item: "A".to_string(), base_branch: "main".to_string() },
                StackEntry { item: "B".to_string(), base_branch: "A".to_string() },
                StackEntry { item: "C".to_string(), base_branch: "B".to_string() },
            ]
        );
    }

    #[test]
    fn respects_a_custom_base() {
        assert_eq!(
            link_ids("release", &["A"]),
            [StackEntry { item: "A".to_string(), base_branch: "release".to_string() }]
        );
    }

    #[test]
    fn preserves_input_order() {
        assert_eq!(
            link_ids("release", &["C", "A", "B"]),
            [
                StackEntry { item: "C".to_string(), base_branch: "release".to_string() },
                StackEntry { item: "A".to_string(), base_branch: "C".to_string() },
                StackEntry { item: "B".to_string(), base_branch: "A".to_string() },
            ]
        );
    }

    #[test]
    fn extracts_each_id_once() {
        let mut calls = 0;
        let _ = link_stack("main", ["A", "B", "C"], |id| {
            calls += 1;
            (*id).to_owned()
        });

        assert_eq!(calls, 3);
    }

    fn current<'a>(
        title: Option<&'a str>,
        body: Option<&'a str>,
        base_branch: &'a str,
    ) -> CurrentPr<'a> {
        CurrentPr { node_id: "PR_node", title, body, base_branch }
    }

    fn desired<'a>(title: &'a str, body: &'a str, base_branch: &'a str) -> DesiredPr<'a> {
        DesiredPr { title, body, base_branch }
    }

    fn update(
        title: Option<&str>,
        body: Option<&str>,
        base_branch: Option<&str>,
    ) -> Option<PrUpdate> {
        Some(PrUpdate {
            node_id: "PR_node".to_string(),
            title: title.map(ToString::to_string),
            body: body.map(ToString::to_string),
            base_branch: base_branch.map(ToString::to_string),
        })
    }

    #[test]
    fn omits_an_update_when_metadata_matches() {
        assert_eq!(
            plan_update(
                current(Some("Title"), Some("Body"), "main"),
                desired("Title", "Body", "main"),
            ),
            None
        );
    }

    #[test]
    fn treats_line_endings_and_outer_whitespace_as_equivalent() {
        assert_eq!(
            plan_update(
                current(Some("Title"), Some(" \r\nBody\r\n "), "main"),
                desired("Title", "Body\n", "main"),
            ),
            None
        );
    }

    #[test]
    fn preserves_meaningful_body_whitespace() {
        assert_eq!(
            plan_update(
                current(Some("Title"), Some("Line one\n\nLine two"), "main"),
                desired("Title", "Line one\nLine two", "main"),
            ),
            update(None, Some("Line one\nLine two"), None)
        );
    }

    #[test]
    fn plans_each_metadata_delta_independently() {
        let cases = [
            (
                current(Some("Old"), Some("Body"), "main"),
                desired("Title", "Body", "main"),
                update(Some("Title"), None, None),
            ),
            (
                current(Some("Title"), Some("Old"), "main"),
                desired("Title", "Body", "main"),
                update(None, Some("Body"), None),
            ),
            (
                current(Some("Title"), Some("Body"), "old-base"),
                desired("Title", "Body", "main"),
                update(None, None, Some("main")),
            ),
            (
                current(Some("Old"), Some("Old"), "old-base"),
                desired("Title", "Body", "main"),
                update(Some("Title"), Some("Body"), Some("main")),
            ),
        ];

        for (current, desired, expected) in cases {
            assert_eq!(plan_update(current, desired), expected);
        }
    }

    #[test]
    fn fills_in_missing_title_and_body() {
        assert_eq!(
            plan_update(current(None, None, "main"), desired("Title", "Body", "main"),),
            update(Some("Title"), Some("Body"), None)
        );
    }

    #[test]
    fn omits_an_unchanged_base_from_other_updates() {
        let update = plan_update(
            current(Some("Old"), Some("Body"), "main"),
            desired("Title", "Body", "main"),
        )
        .unwrap();

        assert_eq!(update.title.as_deref(), Some("Title"));
        assert_eq!(update.base_branch, None);
    }
}

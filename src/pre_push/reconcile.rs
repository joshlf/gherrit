use std::ops::Deref;

use serde::Deserialize;

use super::body::PrBody;

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

/// Local commit state needed to project one pull request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct ProjectionCommit {
    pub(super) gherrit_id: String,
    pub(super) title: String,
    pub(super) commit_body: String,
    pub(super) latest_version: usize,
}

/// The two GitHub identifiers for one pull request.
///
/// Keeping them together prevents diagnostics from naming one PR while a
/// mutation targets another.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct PrIdentity {
    number: u64,
    node_id: String,
}

impl PrIdentity {
    pub(super) fn new(number: u64, node_id: String) -> Self {
        Self { number, node_id }
    }

    pub(super) fn number(&self) -> u64 {
        self.number
    }
}

/// PR state known from an observation or an acknowledged creation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct KnownPr {
    pub(super) identity: PrIdentity,
    pub(super) title: String,
    pub(super) body: String,
    pub(super) base_branch: String,
}

impl KnownPr {
    pub(super) fn new(
        number: u64,
        node_id: String,
        title: String,
        body: String,
        base_branch: String,
    ) -> Self {
        Self { identity: PrIdentity::new(number, node_id), title, body, base_branch }
    }
}

/// One local commit paired with the PR queried for that exact stack position.
///
/// A PR does not repeat its head identity: position establishes that fact.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct ProjectionEntry {
    pub(super) commit: ProjectionCommit,
    pub(super) pull_request: Option<KnownPr>,
}

/// Transport-independent request to create one pull request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct CreatePr {
    pub(super) title: String,
    pub(super) body: String,
    pub(super) base_branch: String,
    pub(super) head_branch: String,
}

/// Context shared by every pull request in a projected stack.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct ProjectionContext<'a> {
    pub(super) base_branch: &'a str,
    pub(super) repo_url: &'a str,
    pub(super) public_branch: Option<&'a str>,
}

/// A collection that is nonempty by construction.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct NonEmpty<T> {
    items: Vec<T>,
}

impl<T> NonEmpty<T> {
    fn collect(items: impl IntoIterator<Item = T>) -> Option<Self> {
        let items = items.into_iter().collect::<Vec<_>>();
        (!items.is_empty()).then_some(Self { items })
    }
}

impl<T> Deref for NonEmpty<T> {
    type Target = [T];

    fn deref(&self) -> &Self::Target {
        &self.items
    }
}

impl<T> IntoIterator for NonEmpty<T> {
    type Item = T;
    type IntoIter = std::vec::IntoIter<T>;

    fn into_iter(self) -> Self::IntoIter {
        self.items.into_iter()
    }
}

/// The next semantic stage required to project a stack.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum ProjectionStep {
    /// Missing pull requests must be created before their assigned numbers can
    /// be included in the final stack bodies.
    Create(NonEmpty<CreatePr>),
    /// Existing pull requests need these minimal metadata patches.
    Update(NonEmpty<UpdatePr>),
    /// Every pull request already matches the desired projection.
    Done,
}

struct DesiredPr<'a> {
    title: &'a str,
    body: &'a str,
    base_branch: &'a str,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct PrPatch {
    title: Option<String>,
    body: Option<String>,
    base_branch: Option<String>,
}

/// Transport-independent, nonempty patch for one pull request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct UpdatePr {
    target: PrIdentity,
    patch: PrPatch,
}

impl UpdatePr {
    pub(super) fn number(&self) -> u64 {
        self.target.number()
    }

    pub(super) fn into_parts(self) -> (String, Option<String>, Option<String>, Option<String>) {
        (self.target.node_id, self.patch.title, self.patch.body, self.patch.base_branch)
    }
}

fn plan_update(current: &KnownPr, desired: DesiredPr<'_>) -> Option<UpdatePr> {
    let title = (current.title != desired.title).then(|| desired.title.to_string());
    let body = (normalize_body(&current.body) != normalize_body(desired.body))
        .then(|| desired.body.to_string());
    // Omitting an unchanged base is required for PRs in the merge queue:
    // GitHub rejects even a no-op base update. See #271.
    let base_branch =
        (current.base_branch != desired.base_branch).then(|| desired.base_branch.to_string());

    (title.is_some() || body.is_some() || base_branch.is_some()).then(|| UpdatePr {
        target: current.identity.clone(),
        patch: PrPatch { title, body, base_branch },
    })
}

fn neighbor_ids(entries: &[ProjectionEntry], index: usize) -> (Option<&str>, Option<&str>) {
    let parent = index
        .checked_sub(1)
        .and_then(|parent| entries.get(parent))
        .map(|entry| entry.commit.gherrit_id.as_str());
    let child = entries.get(index + 1).map(|entry| entry.commit.gherrit_id.as_str());
    (parent, child)
}

/// Derives the next projection stage from a positionally aligned stack.
///
/// The returned actions are ordered but are not transport batches. The caller
/// may commit any prefix, but must reobserve and plan again after an ambiguous
/// write. Assigned numbers from `Create` must be known before updates can be
/// derived.
pub(super) fn plan_projection(
    context: ProjectionContext<'_>,
    entries: &[ProjectionEntry],
) -> ProjectionStep {
    let creations =
        entries.iter().enumerate().filter(|(_, entry)| entry.pull_request.is_none()).map(
            |(index, entry)| {
                let (parent, _) = neighbor_ids(entries, index);
                CreatePr {
                    title: entry.commit.title.clone(),
                    body: entry.commit.commit_body.clone(),
                    base_branch: parent.unwrap_or(context.base_branch).to_string(),
                    head_branch: entry.commit.gherrit_id.clone(),
                }
            },
        );
    if let Some(creations) = NonEmpty::collect(creations) {
        return ProjectionStep::Create(creations);
    }

    let stack_pr_numbers = entries
        .iter()
        .map(|entry| {
            entry
                .pull_request
                .as_ref()
                .expect("missing pull request would have required creation")
                .identity
                .number()
        })
        .collect::<Vec<_>>();
    let updates = entries.iter().enumerate().filter_map(|(index, entry)| {
        let pull_request =
            entry.pull_request.as_ref().expect("missing pull request would have required creation");
        let commit = &entry.commit;
        let (parent, child) = neighbor_ids(entries, index);
        let base_branch = parent.unwrap_or(context.base_branch);
        let body = PrBody {
            commit_body: &commit.commit_body,
            repo_url: context.repo_url,
            public_branch: context.public_branch,
            stack_pr_numbers: &stack_pr_numbers,
            current_pr_number: pull_request.identity.number(),
            latest_version: commit.latest_version,
            base_branch,
            gherrit_id: &commit.gherrit_id,
            parent_id: parent,
            child_id: child,
        }
        .render();

        plan_update(pull_request, DesiredPr { title: &commit.title, body: &body, base_branch })
    });

    NonEmpty::collect(updates).map_or(ProjectionStep::Done, ProjectionStep::Update)
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

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    enum ProjectionCase {
        Absent,
        Converged,
        Drifted,
    }

    const PROJECTION_CASES: [ProjectionCase; 3] =
        [ProjectionCase::Absent, ProjectionCase::Converged, ProjectionCase::Drifted];

    fn projection_context() -> ProjectionContext<'static> {
        ProjectionContext { base_branch: "main", repo_url: "/owner/repo", public_branch: None }
    }

    fn projection_commits() -> Vec<ProjectionCommit> {
        ["A", "B"]
            .into_iter()
            .map(|id| ProjectionCommit {
                gherrit_id: id.to_string(),
                title: format!("Title {id}"),
                commit_body: format!("Body {id}\n\ngherrit-pr-id: {id}"),
                latest_version: 1,
            })
            .collect()
    }

    fn assigned_number(head_branch: &str) -> u64 {
        match head_branch {
            "A" => 37,
            "B" => 11,
            _ => panic!("unknown projected head {head_branch}"),
        }
    }

    fn desired_pr(commits: &[ProjectionCommit], index: usize) -> KnownPr {
        let commit = &commits[index];
        let parent = index.checked_sub(1).map(|parent| commits[parent].gherrit_id.as_str());
        let child = commits.get(index + 1).map(|child| child.gherrit_id.as_str());
        let base_branch = parent.unwrap_or("main");
        let number = assigned_number(&commit.gherrit_id);
        let stack_pr_numbers =
            commits.iter().map(|commit| assigned_number(&commit.gherrit_id)).collect::<Vec<_>>();
        let body = PrBody {
            commit_body: &commit.commit_body,
            repo_url: projection_context().repo_url,
            public_branch: None,
            stack_pr_numbers: &stack_pr_numbers,
            current_pr_number: number,
            latest_version: commit.latest_version,
            base_branch,
            gherrit_id: &commit.gherrit_id,
            parent_id: parent,
            child_id: child,
        }
        .render();

        KnownPr::new(
            number,
            format!("PR_{number}"),
            commit.title.clone(),
            body,
            base_branch.to_string(),
        )
    }

    fn projected_world(cases: [ProjectionCase; 2]) -> Vec<ProjectionEntry> {
        let commits = projection_commits();
        commits
            .iter()
            .cloned()
            .zip(cases)
            .enumerate()
            .map(|(index, (commit, case))| {
                let pull_request = match case {
                    ProjectionCase::Absent => None,
                    ProjectionCase::Converged => Some(desired_pr(&commits, index)),
                    ProjectionCase::Drifted => {
                        let mut pull_request = desired_pr(&commits, index);
                        pull_request.title = format!("Stale {index}");
                        Some(pull_request)
                    }
                };
                ProjectionEntry { commit, pull_request }
            })
            .collect()
    }

    fn apply_creations(entries: &mut [ProjectionEntry], creations: &[CreatePr]) {
        creations.iter().for_each(|create| {
            let entry = entries
                .iter_mut()
                .find(|entry| entry.commit.gherrit_id == create.head_branch)
                .expect("creation must identify a projected commit");
            assert!(entry.pull_request.is_none());
            let number = assigned_number(&create.head_branch);
            entry.pull_request = Some(KnownPr::new(
                number,
                format!("PR_{number}"),
                create.title.clone(),
                create.body.clone(),
                create.base_branch.clone(),
            ));
        });
    }

    fn apply_updates(entries: &mut [ProjectionEntry], updates: &[UpdatePr]) {
        updates.iter().for_each(|update| {
            let pull_request = entries
                .iter_mut()
                .filter_map(|entry| entry.pull_request.as_mut())
                .find(|pull_request| pull_request.identity == update.target)
                .expect("update must identify a known pull request");
            if let Some(title) = &update.patch.title {
                pull_request.title.clone_from(title);
            }
            if let Some(body) = &update.patch.body {
                pull_request.body.clone_from(body);
            }
            if let Some(base_branch) = &update.patch.base_branch {
                pull_request.base_branch.clone_from(base_branch);
            }
        });
    }

    fn apply_projection_step(entries: &mut [ProjectionEntry], step: &ProjectionStep) {
        match step {
            ProjectionStep::Create(creations) => apply_creations(entries, creations),
            ProjectionStep::Update(updates) => apply_updates(entries, updates),
            ProjectionStep::Done => {}
        }
    }

    fn expected_update(entry: &ProjectionEntry) -> UpdatePr {
        let pull_request = entry.pull_request.as_ref().unwrap();
        UpdatePr {
            target: pull_request.identity.clone(),
            patch: PrPatch {
                title: Some(entry.commit.title.clone()),
                body: None,
                base_branch: None,
            },
        }
    }

    #[test]
    fn an_empty_projection_is_already_done() {
        assert_eq!(plan_projection(projection_context(), &[]), ProjectionStep::Done);
    }

    #[test]
    fn projection_planning_exhausts_two_commit_worlds_in_stack_order() {
        let mut cases = 0;

        PROJECTION_CASES.into_iter().for_each(|first| {
            PROJECTION_CASES.into_iter().for_each(|second| {
                cases += 1;
                let states = [first, second];
                let entries = projected_world(states);
                let plan = plan_projection(projection_context(), &entries);
                let creations = entries
                    .iter()
                    .enumerate()
                    .filter(|(index, _)| states[*index] == ProjectionCase::Absent)
                    .map(|(index, entry)| CreatePr {
                        title: entry.commit.title.clone(),
                        body: entry.commit.commit_body.clone(),
                        base_branch: if index == 0 { "main" } else { "A" }.to_string(),
                        head_branch: entry.commit.gherrit_id.clone(),
                    });
                let updates = entries
                    .iter()
                    .enumerate()
                    .filter(|(index, _)| states[*index] == ProjectionCase::Drifted)
                    .map(|(_, entry)| expected_update(entry));
                let expected = NonEmpty::collect(creations)
                    .map(ProjectionStep::Create)
                    .or_else(|| NonEmpty::collect(updates).map(ProjectionStep::Update))
                    .unwrap_or(ProjectionStep::Done);

                assert_eq!(plan, expected, "states={states:?}");
            });
        });

        assert_eq!(cases, 9);
    }

    fn converge(entries: &mut [ProjectionEntry]) {
        for _ in 0..3 {
            let step = plan_projection(projection_context(), entries);
            if step == ProjectionStep::Done {
                return;
            }
            apply_projection_step(entries, &step);
        }
        panic!("projection did not converge: {entries:?}");
    }

    #[test]
    fn applying_projection_stages_converges_idempotently() {
        PROJECTION_CASES.into_iter().for_each(|first| {
            PROJECTION_CASES.into_iter().for_each(|second| {
                let states = [first, second];
                let mut entries = projected_world(states);
                converge(&mut entries);
                assert_eq!(
                    plan_projection(projection_context(), &entries),
                    ProjectionStep::Done,
                    "states={states:?}"
                );
            });
        });
    }

    #[test]
    fn recovers_after_every_committed_creation_prefix() {
        let mut entries = projected_world([ProjectionCase::Absent; 2]);
        let ProjectionStep::Create(creations) = plan_projection(projection_context(), &entries)
        else {
            panic!("an absent stack must begin with creation");
        };

        (0..=creations.len()).for_each(|committed| {
            entries.iter_mut().for_each(|entry| entry.pull_request = None);
            apply_creations(&mut entries, &creations[..committed]);

            let replanned = plan_projection(projection_context(), &entries);
            if committed < creations.len() {
                assert_eq!(
                    replanned,
                    ProjectionStep::Create(
                        NonEmpty::collect(creations[committed..].iter().cloned()).unwrap()
                    ),
                    "committed creation prefix={committed}"
                );
            } else {
                assert!(
                    matches!(replanned, ProjectionStep::Update(_)),
                    "a fully-created stack must advance to final metadata"
                );
            }

            converge(&mut entries);
            assert_eq!(plan_projection(projection_context(), &entries), ProjectionStep::Done);
        });
    }

    #[test]
    fn replans_after_every_committed_update_prefix() {
        let initial = projected_world([ProjectionCase::Drifted; 2]);
        let ProjectionStep::Update(updates) = plan_projection(projection_context(), &initial)
        else {
            panic!("a drifted stack must require updates");
        };

        (0..=updates.len()).for_each(|committed| {
            let mut entries = initial.clone();
            apply_updates(&mut entries, &updates[..committed]);

            let expected = NonEmpty::collect(updates[committed..].iter().cloned())
                .map_or(ProjectionStep::Done, ProjectionStep::Update);
            assert_eq!(
                plan_projection(projection_context(), &entries),
                expected,
                "committed update prefix={committed}"
            );

            converge(&mut entries);
            assert_eq!(plan_projection(projection_context(), &entries), ProjectionStep::Done);
        });
    }

    fn current(title: &str, body: &str, base_branch: &str) -> KnownPr {
        KnownPr::new(
            42,
            "PR_node".to_string(),
            title.to_string(),
            body.to_string(),
            base_branch.to_string(),
        )
    }

    fn desired<'a>(title: &'a str, body: &'a str, base_branch: &'a str) -> DesiredPr<'a> {
        DesiredPr { title, body, base_branch }
    }

    fn update(
        title: Option<&str>,
        body: Option<&str>,
        base_branch: Option<&str>,
    ) -> Option<UpdatePr> {
        Some(UpdatePr {
            target: PrIdentity::new(42, "PR_node".to_string()),
            patch: PrPatch {
                title: title.map(ToString::to_string),
                body: body.map(ToString::to_string),
                base_branch: base_branch.map(ToString::to_string),
            },
        })
    }

    #[test]
    fn omits_an_update_when_metadata_matches() {
        assert_eq!(
            plan_update(&current("Title", "Body", "main"), desired("Title", "Body", "main")),
            None
        );
    }

    #[test]
    fn treats_line_endings_and_outer_whitespace_as_equivalent() {
        assert_eq!(
            plan_update(
                &current("Title", " \r\nBody\r\n ", "main"),
                desired("Title", "Body\n", "main"),
            ),
            None
        );
    }

    #[test]
    fn preserves_meaningful_body_whitespace() {
        assert_eq!(
            plan_update(
                &current("Title", "Line one\n\nLine two", "main"),
                desired("Title", "Line one\nLine two", "main"),
            ),
            update(None, Some("Line one\nLine two"), None)
        );
    }

    #[test]
    fn plans_every_metadata_delta_combination_minimally() {
        let mut cases = 0;

        [false, true].into_iter().for_each(|title_drift| {
            [false, true].into_iter().for_each(|body_drift| {
                [false, true].into_iter().for_each(|base_drift| {
                    cases += 1;
                    let expected = (title_drift || body_drift || base_drift).then(|| {
                        update(
                            title_drift.then_some("Title"),
                            body_drift.then_some("Body"),
                            base_drift.then_some("main"),
                        )
                        .unwrap()
                    });

                    assert_eq!(
                        plan_update(
                            &current(
                                if title_drift { "Old" } else { "Title" },
                                if body_drift { "Old" } else { "Body" },
                                if base_drift { "old-base" } else { "main" },
                            ),
                            desired("Title", "Body", "main"),
                        ),
                        expected,
                        "drift=({title_drift}, {body_drift}, {base_drift})"
                    );
                });
            });
        });

        assert_eq!(cases, 8);
    }

    #[test]
    fn update_targets_keep_number_and_node_id_together() {
        let update =
            plan_update(&current("Old", "Body", "main"), desired("Title", "Body", "main")).unwrap();

        assert_eq!(update.number(), 42);
        assert_eq!(
            update.into_parts(),
            ("PR_node".to_string(), Some("Title".to_string()), None, None)
        );
    }
}

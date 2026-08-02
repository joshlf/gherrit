use super::body::PrBody;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum PullRequestState {
    Open,
    Closed,
    Merged,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct NonOpenPullRequest {
    number: u64,
    state: PullRequestState,
}

#[derive(Debug, PartialEq, Eq)]
pub(super) struct NonOpenPullRequests {
    pull_requests: Vec<NonOpenPullRequest>,
}

impl std::fmt::Display for NonOpenPullRequests {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.pull_requests.iter().try_for_each(|pull_request| {
            let state = match pull_request.state {
                PullRequestState::Open => unreachable!("open pull requests are not rejected"),
                PullRequestState::Closed => "closed",
                PullRequestState::Merged => "merged",
            };
            writeln!(
                formatter,
                "Cannot push to {state} PR #{}. Please open a new PR or reopen the existing one.",
                pull_request.number
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
        .filter(|(_, state)| *state != PullRequestState::Open)
        .map(|(number, state)| NonOpenPullRequest { number, state })
        .collect::<Vec<_>>();

    if pull_requests.is_empty() { Ok(()) } else { Err(NonOpenPullRequests { pull_requests }) }
}

/// A stack item annotated with the relationships needed to project it as a PR.
#[derive(Debug, PartialEq, Eq)]
pub(super) struct StackEntry<T> {
    pub(super) item: T,
    pub(super) base_branch: String,
    pub(super) parent_id: Option<String>,
    pub(super) child_id: Option<String>,
}

/// Derives stack topology from items ordered from base to head.
pub(super) fn link_stack<T>(
    base_branch: &str,
    items: impl IntoIterator<Item = T>,
    mut id_of: impl FnMut(&T) -> String,
) -> Vec<StackEntry<T>> {
    let mut items = items
        .into_iter()
        .map(|item| {
            let id = id_of(&item);
            (item, id)
        })
        .peekable();
    let mut previous_id = None;

    std::iter::from_fn(|| {
        let (item, id) = items.next()?;
        let child_id = items.peek().map(|(_, id)| id.clone());
        let parent_id = previous_id.replace(id);
        let base_branch = parent_id.as_deref().unwrap_or(base_branch).to_owned();

        Some(StackEntry { item, base_branch, parent_id, child_id })
    })
    .collect()
}

/// Local commit state needed to project one pull request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct ProjectionCommit {
    pub(super) gherrit_id: String,
    pub(super) title: String,
    pub(super) commit_body: String,
    pub(super) latest_version: usize,
}

/// GitHub state observed for one pull request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct ObservedPr {
    pub(super) number: u64,
    pub(super) node_id: String,
    pub(super) title: Option<String>,
    pub(super) body: Option<String>,
    pub(super) base_branch: String,
    pub(super) head_branch: String,
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

/// The next semantic stage required to project a stack.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum ProjectionPlan {
    /// Missing pull requests must be created before their assigned numbers can
    /// be included in the final stack bodies.
    Create(Vec<CreatePr>),
    /// Existing pull requests need these minimal metadata patches.
    Update(Vec<UpdatePr>),
    /// Every pull request already matches the desired projection.
    Done,
}

/// Metadata currently stored on a PR.
struct CurrentPr<'a> {
    node_id: &'a str,
    title: Option<&'a str>,
    body: Option<&'a str>,
    base_branch: &'a str,
}

/// Metadata derived from a local commit and its stack position.
struct DesiredPr<'a> {
    title: &'a str,
    body: &'a str,
    base_branch: &'a str,
}

/// Transport-independent minimal patch for one pull request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct UpdatePr {
    pub(super) number: u64,
    /// The global node ID of the PR to update.
    pub(super) node_id: String,
    pub(super) title: Option<String>,
    pub(super) body: Option<String>,
    // Omitting an unchanged base branch is required for PRs in the merge queue:
    // GitHub rejects even a no-op base update for those PRs. See #271.
    pub(super) base_branch: Option<String>,
}

/// Returns the minimal update needed to make `current` match `desired`.
fn plan_update(number: u64, current: CurrentPr<'_>, desired: DesiredPr<'_>) -> Option<UpdatePr> {
    let title = (current.title != Some(desired.title)).then(|| desired.title.to_string());
    let body = current
        .body
        .is_none_or(|body| normalize_body(body) != normalize_body(desired.body))
        .then(|| desired.body.to_string());
    let base_branch =
        (current.base_branch != desired.base_branch).then(|| desired.base_branch.to_string());

    (title.is_some() || body.is_some() || base_branch.is_some()).then(|| UpdatePr {
        number,
        node_id: current.node_id.to_string(),
        title,
        body,
        base_branch,
    })
}

/// Derives the next projection stage from local intent and observed PR state.
///
/// The returned vectors are ordered independent actions, not transport
/// batches. The caller may commit any prefix, but must reobserve and plan again
/// after interruption. Assigned numbers from `Create` must be observed before
/// updates can be derived.
pub(super) fn plan_projection(
    context: ProjectionContext<'_>,
    commits: &[ProjectionCommit],
    pull_requests: &[ObservedPr],
) -> ProjectionPlan {
    let entries = link_stack(context.base_branch, commits, |commit| commit.gherrit_id.clone());
    let matched = entries
        .iter()
        .map(|entry| {
            let pull_request =
                pull_requests.iter().find(|pr| pr.head_branch == entry.item.gherrit_id);
            (entry, pull_request)
        })
        .collect::<Vec<_>>();

    let creations = matched
        .iter()
        .filter(|(_, pull_request)| pull_request.is_none())
        .map(|(entry, _)| CreatePr {
            title: entry.item.title.clone(),
            body: entry.item.commit_body.clone(),
            base_branch: entry.base_branch.clone(),
            head_branch: entry.item.gherrit_id.clone(),
        })
        .collect::<Vec<_>>();
    if !creations.is_empty() {
        return ProjectionPlan::Create(creations);
    }

    let stack_pr_numbers = matched
        .iter()
        .map(|(_, pull_request)| {
            pull_request.expect("missing pull request would have required creation").number
        })
        .collect::<Vec<_>>();
    let updates = matched
        .into_iter()
        .filter_map(|(entry, pull_request)| {
            let pull_request =
                pull_request.expect("missing pull request would have required creation");
            let commit = entry.item;
            let body = PrBody {
                commit_body: &commit.commit_body,
                repo_url: context.repo_url,
                public_branch: context.public_branch,
                stack_pr_numbers: &stack_pr_numbers,
                current_pr_number: pull_request.number,
                latest_version: commit.latest_version,
                base_branch: &entry.base_branch,
                gherrit_id: &commit.gherrit_id,
                parent_id: entry.parent_id.as_deref(),
                child_id: entry.child_id.as_deref(),
            }
            .render();

            plan_update(
                pull_request.number,
                CurrentPr {
                    node_id: &pull_request.node_id,
                    title: pull_request.title.as_deref(),
                    body: pull_request.body.as_deref(),
                    base_branch: &pull_request.base_branch,
                },
                DesiredPr { title: &commit.title, body: &body, base_branch: &entry.base_branch },
            )
        })
        .collect::<Vec<_>>();

    if updates.is_empty() { ProjectionPlan::Done } else { ProjectionPlan::Update(updates) }
}

/// Whether reobservation proves that an ambiguously acknowledged update made
/// progress.
///
/// Replaying an identical patch is unsafe even when assigning the same values
/// is state-idempotent: GitHub can reject a now-no-op base change for a pull
/// request in the merge queue. A changed or absent patch was derived from new
/// state and can be executed as a new action.
pub(super) fn ambiguous_update_made_progress(
    attempted: &UpdatePr,
    replanned: &ProjectionPlan,
) -> bool {
    !matches!(replanned, ProjectionPlan::Update(updates) if updates.contains(attempted))
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
                    .filter(|(_, state)| *state != PullRequestState::Open)
                    .map(|(number, state)| NonOpenPullRequest { number, state })
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
            [StackEntry {
                item: "A".to_string(),
                base_branch: "main".to_string(),
                parent_id: None,
                child_id: None,
            }]
        );
    }

    #[test]
    fn links_each_commit_to_its_neighbors() {
        assert_eq!(
            link_ids("main", &["A", "B", "C"]),
            [
                StackEntry {
                    item: "A".to_string(),
                    base_branch: "main".to_string(),
                    parent_id: None,
                    child_id: Some("B".to_string()),
                },
                StackEntry {
                    item: "B".to_string(),
                    base_branch: "A".to_string(),
                    parent_id: Some("A".to_string()),
                    child_id: Some("C".to_string()),
                },
                StackEntry {
                    item: "C".to_string(),
                    base_branch: "B".to_string(),
                    parent_id: Some("B".to_string()),
                    child_id: None,
                },
            ]
        );
    }

    #[test]
    fn respects_a_custom_base() {
        assert_eq!(
            link_ids("release", &["A"]),
            [StackEntry {
                item: "A".to_string(),
                base_branch: "release".to_string(),
                parent_id: None,
                child_id: None,
            }]
        );
    }

    #[test]
    fn preserves_input_order() {
        assert_eq!(
            link_ids("release", &["C", "A", "B"]),
            [
                StackEntry {
                    item: "C".to_string(),
                    base_branch: "release".to_string(),
                    parent_id: None,
                    child_id: Some("A".to_string()),
                },
                StackEntry {
                    item: "A".to_string(),
                    base_branch: "C".to_string(),
                    parent_id: Some("C".to_string()),
                    child_id: Some("B".to_string()),
                },
                StackEntry {
                    item: "B".to_string(),
                    base_branch: "A".to_string(),
                    parent_id: Some("A".to_string()),
                    child_id: None,
                },
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

    fn assigned_number(commits: &[ProjectionCommit], head_branch: &str) -> u64 {
        let index = commits
            .iter()
            .position(|commit| commit.gherrit_id == head_branch)
            .expect("created PR must identify a projected commit");
        u64::try_from(index + 1).unwrap() * 11
    }

    fn desired_pr(commits: &[ProjectionCommit], index: usize) -> ObservedPr {
        let commit = &commits[index];
        let parent_id = (index == 1).then_some("A");
        let child_id = (index == 0).then_some("B");
        let base_branch = parent_id.unwrap_or("main");
        let number = assigned_number(commits, &commit.gherrit_id);
        let stack_pr_numbers = commits
            .iter()
            .map(|commit| assigned_number(commits, &commit.gherrit_id))
            .collect::<Vec<_>>();
        let body = PrBody {
            commit_body: &commit.commit_body,
            repo_url: projection_context().repo_url,
            public_branch: None,
            stack_pr_numbers: &stack_pr_numbers,
            current_pr_number: number,
            latest_version: commit.latest_version,
            base_branch,
            gherrit_id: &commit.gherrit_id,
            parent_id,
            child_id,
        }
        .render();

        ObservedPr {
            number,
            node_id: format!("PR_{number}"),
            title: Some(commit.title.clone()),
            body: Some(body),
            base_branch: base_branch.to_string(),
            head_branch: commit.gherrit_id.clone(),
        }
    }

    fn observed_world(commits: &[ProjectionCommit], cases: [ProjectionCase; 2]) -> Vec<ObservedPr> {
        cases
            .into_iter()
            .enumerate()
            .filter_map(|(index, case)| match case {
                ProjectionCase::Absent => None,
                ProjectionCase::Converged => Some(desired_pr(commits, index)),
                ProjectionCase::Drifted => {
                    let mut pull_request = desired_pr(commits, index);
                    pull_request.title = Some(format!("Stale {index}"));
                    Some(pull_request)
                }
            })
            .collect()
    }

    fn apply_creations(
        commits: &[ProjectionCommit],
        pull_requests: &mut Vec<ObservedPr>,
        creations: &[CreatePr],
    ) {
        creations.iter().for_each(|create| {
            assert!(
                pull_requests.iter().all(|pr| pr.head_branch != create.head_branch),
                "planner emitted a duplicate creation for {}",
                create.head_branch
            );
            let number = assigned_number(commits, &create.head_branch);
            pull_requests.push(ObservedPr {
                number,
                node_id: format!("PR_{number}"),
                title: Some(create.title.clone()),
                body: Some(create.body.clone()),
                base_branch: create.base_branch.clone(),
                head_branch: create.head_branch.clone(),
            });
        });
    }

    fn apply_updates(pull_requests: &mut [ObservedPr], updates: &[UpdatePr]) {
        updates.iter().for_each(|update| {
            let pull_request = pull_requests
                .iter_mut()
                .find(|pull_request| pull_request.node_id == update.node_id)
                .expect("update must identify an observed pull request");
            if let Some(title) = &update.title {
                pull_request.title = Some(title.clone());
            }
            if let Some(body) = &update.body {
                pull_request.body = Some(body.clone());
            }
            if let Some(base_branch) = &update.base_branch {
                pull_request.base_branch.clone_from(base_branch);
            }
        });
    }

    fn apply_projection_plan(
        commits: &[ProjectionCommit],
        pull_requests: &mut Vec<ObservedPr>,
        plan: &ProjectionPlan,
    ) {
        match plan {
            ProjectionPlan::Create(creations) => {
                apply_creations(commits, pull_requests, creations);
            }
            ProjectionPlan::Update(updates) => apply_updates(pull_requests, updates),
            ProjectionPlan::Done => {}
        }
    }

    #[test]
    fn an_empty_projection_is_already_done() {
        assert_eq!(plan_projection(projection_context(), &[], &[]), ProjectionPlan::Done);
    }

    #[test]
    fn projection_planning_exhausts_two_commit_worlds_in_stack_order() {
        let commits = projection_commits();
        let mut cases = 0;

        PROJECTION_CASES.into_iter().for_each(|first| {
            PROJECTION_CASES.into_iter().for_each(|second| {
                cases += 1;
                let states = [first, second];
                let pull_requests = observed_world(&commits, states);
                let plan = plan_projection(projection_context(), &commits, &pull_requests);

                let expected_creations = states
                    .into_iter()
                    .enumerate()
                    .filter(|(_, state)| *state == ProjectionCase::Absent)
                    .map(|(index, _)| CreatePr {
                        title: commits[index].title.clone(),
                        body: commits[index].commit_body.clone(),
                        base_branch: if index == 0 { "main" } else { "A" }.to_string(),
                        head_branch: commits[index].gherrit_id.clone(),
                    })
                    .collect::<Vec<_>>();
                let expected_updates = states
                    .into_iter()
                    .enumerate()
                    .filter(|(_, state)| *state == ProjectionCase::Drifted)
                    .map(|(index, _)| UpdatePr {
                        number: assigned_number(&commits, &commits[index].gherrit_id),
                        node_id: format!(
                            "PR_{}",
                            assigned_number(&commits, &commits[index].gherrit_id)
                        ),
                        title: Some(commits[index].title.clone()),
                        body: None,
                        base_branch: None,
                    })
                    .collect::<Vec<_>>();
                let expected = if !expected_creations.is_empty() {
                    ProjectionPlan::Create(expected_creations)
                } else if !expected_updates.is_empty() {
                    ProjectionPlan::Update(expected_updates)
                } else {
                    ProjectionPlan::Done
                };

                assert_eq!(plan, expected, "states={states:?}");
            });
        });

        assert_eq!(cases, 9);
    }

    #[test]
    fn applying_projection_stages_converges_idempotently() {
        let commits = projection_commits();

        PROJECTION_CASES.into_iter().for_each(|first| {
            PROJECTION_CASES.into_iter().for_each(|second| {
                let states = [first, second];
                let mut pull_requests = observed_world(&commits, states);
                let first_plan = plan_projection(projection_context(), &commits, &pull_requests);
                apply_projection_plan(&commits, &mut pull_requests, &first_plan);
                let second_plan = plan_projection(projection_context(), &commits, &pull_requests);
                apply_projection_plan(&commits, &mut pull_requests, &second_plan);
                let final_plan = plan_projection(projection_context(), &commits, &pull_requests);

                assert_eq!(final_plan, ProjectionPlan::Done, "states={states:?}");
                [first_plan, second_plan]
                    .iter()
                    .find(|plan| matches!(plan, ProjectionPlan::Update(_)))
                    .into_iter()
                    .for_each(|plan| apply_projection_plan(&commits, &mut pull_requests, plan));
                assert_eq!(
                    plan_projection(projection_context(), &commits, &pull_requests),
                    ProjectionPlan::Done,
                    "reapplying a successful stage changed convergence for states={states:?}"
                );
            });
        });
    }

    fn converge(commits: &[ProjectionCommit], pull_requests: &mut Vec<ObservedPr>) {
        for _ in 0..3 {
            let plan = plan_projection(projection_context(), commits, pull_requests);
            if plan == ProjectionPlan::Done {
                return;
            }
            apply_projection_plan(commits, pull_requests, &plan);
        }
        panic!("projection did not converge: {pull_requests:?}");
    }

    #[test]
    fn recovers_after_every_committed_creation_prefix() {
        let commits = projection_commits();
        let ProjectionPlan::Create(creations) =
            plan_projection(projection_context(), &commits, &[])
        else {
            panic!("an absent stack must begin with creation");
        };

        (0..=creations.len()).for_each(|committed| {
            let mut pull_requests = Vec::new();
            apply_creations(&commits, &mut pull_requests, &creations[..committed]);

            let replanned = plan_projection(projection_context(), &commits, &pull_requests);
            if committed < creations.len() {
                assert_eq!(
                    replanned,
                    ProjectionPlan::Create(creations[committed..].to_vec()),
                    "committed creation prefix={committed}"
                );
            } else {
                assert!(
                    matches!(replanned, ProjectionPlan::Update(_)),
                    "a fully-created stack must advance to updates after a lost acknowledgement"
                );
            }

            converge(&commits, &mut pull_requests);
            assert_eq!(pull_requests.len(), commits.len());
            assert_eq!(
                plan_projection(projection_context(), &commits, &pull_requests),
                ProjectionPlan::Done
            );
        });
    }

    #[test]
    fn replans_after_every_committed_update_prefix() {
        let commits = projection_commits();
        let initial = observed_world(&commits, [ProjectionCase::Drifted; 2]);
        let ProjectionPlan::Update(updates) =
            plan_projection(projection_context(), &commits, &initial)
        else {
            panic!("a drifted stack must require updates");
        };

        (0..=updates.len()).for_each(|committed| {
            let mut pull_requests = initial.clone();
            apply_updates(&mut pull_requests, &updates[..committed]);

            let expected = if committed == updates.len() {
                ProjectionPlan::Done
            } else {
                ProjectionPlan::Update(updates[committed..].to_vec())
            };
            assert_eq!(
                plan_projection(projection_context(), &commits, &pull_requests),
                expected,
                "committed update prefix={committed}"
            );

            converge(&commits, &mut pull_requests);
            assert_eq!(
                plan_projection(projection_context(), &commits, &pull_requests),
                ProjectionPlan::Done
            );
        });
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
    ) -> Option<UpdatePr> {
        Some(UpdatePr {
            number: 42,
            node_id: "PR_node".to_string(),
            title: title.map(ToString::to_string),
            body: body.map(ToString::to_string),
            base_branch: base_branch.map(ToString::to_string),
        })
    }

    #[test]
    fn ambiguous_update_requires_observed_progress_before_retry() {
        let attempted = update(Some("Title"), Some("Body"), Some("main")).unwrap();
        assert!(!ambiguous_update_made_progress(
            &attempted,
            &ProjectionPlan::Update(vec![attempted.clone()]),
        ));

        let remaining = update(None, Some("Body"), None).unwrap();
        for replanned in [
            ProjectionPlan::Update(vec![remaining]),
            ProjectionPlan::Create(Vec::new()),
            ProjectionPlan::Done,
        ] {
            assert!(ambiguous_update_made_progress(&attempted, &replanned));
        }
    }

    #[test]
    fn omits_an_update_when_metadata_matches() {
        assert_eq!(
            plan_update(
                42,
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
                42,
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
                42,
                current(Some("Title"), Some("Line one\n\nLine two"), "main"),
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
                            42,
                            current(
                                Some(if title_drift { "Old" } else { "Title" }),
                                Some(if body_drift { "Old" } else { "Body" }),
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
    fn fills_in_missing_title_and_body() {
        assert_eq!(
            plan_update(42, current(None, None, "main"), desired("Title", "Body", "main"),),
            update(Some("Title"), Some("Body"), None)
        );
    }

    #[test]
    fn omits_an_unchanged_base_from_other_updates() {
        let update = plan_update(
            42,
            current(Some("Old"), Some("Body"), "main"),
            desired("Title", "Body", "main"),
        )
        .unwrap();

        assert_eq!(update.title.as_deref(), Some("Title"));
        assert_eq!(update.base_branch, None);
    }
}

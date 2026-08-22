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

/// Metadata currently stored on a PR.
pub(super) struct CurrentPr<'a> {
    pub(super) node_id: &'a str,
    pub(super) title: &'a str,
    pub(super) body: &'a str,
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
    let title = (current.title != desired.title).then(|| desired.title.to_string());
    let body = (normalize_body(current.body) != normalize_body(desired.body))
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

    fn current<'a>(title: &'a str, body: &'a str, base_branch: &'a str) -> CurrentPr<'a> {
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
            plan_update(current("Title", "Body", "main"), desired("Title", "Body", "main"),),
            None
        );
    }

    #[test]
    fn treats_line_endings_and_outer_whitespace_as_equivalent() {
        assert_eq!(
            plan_update(
                current("Title", " \r\nBody\r\n ", "main"),
                desired("Title", "Body\n", "main"),
            ),
            None
        );
    }

    #[test]
    fn preserves_meaningful_body_whitespace() {
        assert_eq!(
            plan_update(
                current("Title", "Line one\n\nLine two", "main"),
                desired("Title", "Line one\nLine two", "main"),
            ),
            update(None, Some("Line one\nLine two"), None)
        );
    }

    #[test]
    fn plans_each_metadata_delta_independently() {
        let cases = [
            (
                current("Old", "Body", "main"),
                desired("Title", "Body", "main"),
                update(Some("Title"), None, None),
            ),
            (
                current("Title", "Old", "main"),
                desired("Title", "Body", "main"),
                update(None, Some("Body"), None),
            ),
            (
                current("Title", "Body", "old-base"),
                desired("Title", "Body", "main"),
                update(None, None, Some("main")),
            ),
            (
                current("Old", "Old", "old-base"),
                desired("Title", "Body", "main"),
                update(Some("Title"), Some("Body"), Some("main")),
            ),
        ];

        for (current, desired, expected) in cases {
            assert_eq!(plan_update(current, desired), expected);
        }
    }

    #[test]
    fn omits_an_unchanged_base_from_other_updates() {
        let update =
            plan_update(current("Old", "Body", "main"), desired("Title", "Body", "main")).unwrap();

        assert_eq!(update.title.as_deref(), Some("Title"));
        assert_eq!(update.base_branch, None);
    }
}

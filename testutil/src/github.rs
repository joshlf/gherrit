//! Github.

use super::*;

pub struct MockGithub<'a> {
    pub(super) context: &'a TestContext,
}

impl MockGithub<'_> {
    pub fn repository(&self) -> (String, String) {
        self.context.inspect_mock_state(|state| (state.repo_owner.clone(), state.repo_name.clone()))
    }

    pub fn pull_requests(&self) -> Vec<PullRequestSnapshot> {
        self.context
            .inspect_mock_state(|state| state.prs.iter().map(PullRequestSnapshot::from).collect())
    }

    pub fn requests(&self) -> Vec<Vec<GraphQlOperation>> {
        self.context.inspect_mock_state(|state| {
            state.graphql_requests.iter().map(|request| request.operations.clone()).collect()
        })
    }

    /// Returns each validated physical GraphQL document in arrival order.
    pub fn request_documents(&self) -> Vec<String> {
        self.context.inspect_mock_state(|state| {
            state.graphql_requests.iter().map(|request| request.document.clone()).collect()
        })
    }

    pub fn redirect_trap_requests(&self) -> usize {
        self.context.inspect_mock_state(|state| state.graphql_redirect_trap_requests)
    }

    /// Overrides GitHub's view of the default branch without changing Git.
    pub fn set_default_branch(&self, name: &str, object_id: &str) {
        self.context.mutate_mock_state(|state| {
            state.github_default_branch = Some((name.to_owned(), object_id.to_owned()));
        });
    }

    pub fn seed_pull_request(&self, seed: PullRequestSeed) {
        let PullRequestSeed { number, title, body, head, base, is_draft } = seed;
        self.context.mutate_mock_state(|state| {
            let mut pr = mock_server::PrEntry::mock(mock_server::MockPrArgs {
                number: usize::try_from(number.get())
                    .expect("the test target must represent GraphQL pull request numbers"),
                title,
                body,
                head,
                base,
            });
            pr.is_draft = is_draft;
            state.add_pr(pr);
        });
    }

    /// Inserts a pull request before the existing connection rows.
    pub fn seed_earlier_pull_request(&self, seed: PullRequestSeed) {
        let PullRequestSeed { number, title, body, head, base, is_draft } = seed;
        self.context.mutate_mock_state(|state| {
            let mut pr = mock_server::PrEntry::mock(mock_server::MockPrArgs {
                number: usize::try_from(number.get())
                    .expect("the test target must represent GraphQL pull request numbers"),
                title,
                body,
                head,
                base,
            });
            pr.is_draft = is_draft;
            state.prs.insert(0, pr);
        });
    }

    /// Seeds an intentionally malformed GraphQL identity for rejection tests.
    pub fn seed_pull_request_with_invalid_number(
        &self,
        number: usize,
        title: impl Into<String>,
        body: impl Into<String>,
        head: impl Into<String>,
        base: impl Into<String>,
    ) {
        assert!(
            valid_pull_request_number(number).is_none(),
            "ordinary pull request numbers must use seed_pull_request"
        );
        self.context.mutate_mock_state(|state| {
            state.add_pr(mock_server::PrEntry::mock(mock_server::MockPrArgs {
                number,
                title: title.into(),
                body: body.into(),
                head: head.into(),
                base: base.into(),
            }));
        });
    }

    pub fn seed_cross_repository_pull_request(
        &self,
        seed: PullRequestSeed,
        head_oid: &str,
        base_oid: &str,
    ) {
        for (kind, oid) in [("head", head_oid), ("base", base_oid)] {
            assert!(
                oid.len() == 40 && oid.bytes().all(|byte| byte.is_ascii_hexdigit()),
                "cross-repository {kind} object ID must be 40 hexadecimal digits"
            );
        }
        let PullRequestSeed { number, title, body, head, base, is_draft } = seed;
        self.context.mutate_mock_state(|state| {
            let number = usize::try_from(number.get())
                .expect("the test target must represent GraphQL pull request numbers");
            let mut pr = mock_server::PrEntry::mock(mock_server::MockPrArgs {
                number,
                title,
                body,
                head,
                base,
            });
            pr.is_draft = is_draft;
            pr.head.oid = head_oid.to_owned();
            pr.base.oid = base_oid.to_owned();
            state.add_pr(pr);
            state.cross_repository_prs.insert(number);
        });
    }

    pub fn set_pull_request_landing_automation(
        &self,
        number: usize,
        auto_merge: bool,
        in_merge_queue: bool,
    ) {
        self.context.mutate_mock_state(|state| {
            let pr = state
                .prs
                .iter_mut()
                .find(|pr| pr.number == number)
                .unwrap_or_else(|| panic!("pull request #{number} does not exist"));
            pr.auto_merge = auto_merge;
            pr.in_merge_queue = in_merge_queue;
        });
    }

    pub fn set_pull_request_state(&self, number: usize, new_state: PullRequestState) {
        self.context.mutate_mock_state(|state| {
            let pr = state
                .prs
                .iter_mut()
                .find(|pr| pr.number == number)
                .unwrap_or_else(|| panic!("pull request #{number} does not exist"));
            pr.state = new_state;
        });
    }

    pub fn set_pull_request_draft(&self, number: usize, is_draft: bool) {
        self.context.mutate_mock_state(|state| {
            let pr = state
                .prs
                .iter_mut()
                .find(|pr| pr.number == number)
                .unwrap_or_else(|| panic!("pull request #{number} does not exist"));
            pr.is_draft = is_draft;
        });
    }

    pub fn set_pull_request_title(&self, number: usize, new_title: &str) {
        self.context.mutate_mock_state(|state| {
            let pr = state
                .prs
                .iter_mut()
                .find(|pr| pr.number == number)
                .unwrap_or_else(|| panic!("pull request #{number} does not exist"));
            pr.title = new_title.to_owned();
        });
    }
}

use super::{
    mock_server, GraphQlOperation, PullRequestSeed, PullRequestSnapshot, PullRequestState,
    TestContext,
};

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

    /// HTTP arrivals counted before JSON extraction or schema validation.
    pub fn http_arrivals(&self) -> usize {
        self.context.inspect_mock_state(|state| state.http_arrivals)
    }

    pub fn requests(&self) -> Vec<Vec<GraphQlOperation>> {
        self.context.inspect_mock_state(|state| state.graphql_requests.clone())
    }

    pub fn seed_pull_request(&self, seed: PullRequestSeed) {
        self.context.mutate_mock_state(|state| {
            let pr = mock_server::PrEntry::mock(mock_server::MockPrArgs {
                id: u64::try_from(seed.number).expect("pull request number does not fit in u64"),
                title: seed.title,
                body: seed.body,
                head: seed.head,
                base: seed.base,
                repo_owner: &state.repo_owner,
                repo_name: &state.repo_name,
            });
            state.add_pr(pr);
        });
    }

    pub fn set_pull_request_state(&self, number: usize, new_state: PullRequestState) {
        self.context.mutate_mock_state(|state| {
            let pr = state
                .prs
                .iter_mut()
                .find(|pr| pr.number == number)
                .unwrap_or_else(|| panic!("pull request #{number} does not exist"));
            pr.state = new_state.as_str().to_string();
        });
    }
}

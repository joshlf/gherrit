//! Pull-request evidence for the exact local stack.
//!
//! Each local change has one completely paginated GitHub connection covering
//! every pull-request lifecycle state. Correlation consumes those connections
//! without interpreting pull-request bodies or searching unrelated pull
//! requests.

use std::{collections::HashSet, num::NonZeroU32};

use color_eyre::eyre::{Context as _, Result, bail, eyre};
use gix::ObjectId;

use super::{
    bounded_diagnostic_detail,
    destination::DefaultBranch,
    github::{CompleteLocalPullRequests, ObservedPullRequest, PullRequestState},
    local::GherritPrId,
};

const MAX_TERMINAL_DIAGNOSTIC_ROWS: usize = 20;

/// A positive pull-request number in GitHub's GraphQL `Int` range.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub(super) struct PullRequestNumber(NonZeroU32);

impl PullRequestNumber {
    pub(super) fn new(value: u64) -> Result<Self> {
        let value = u32::try_from(value)
            .ok()
            .and_then(NonZeroU32::new)
            .filter(|value| value.get() <= i32::MAX as u32)
            .ok_or_else(|| eyre!("GitHub reported invalid pull request number {value}"))?;
        Ok(Self(value))
    }

    pub(super) fn get(self) -> u32 {
        self.0.get()
    }
}

/// A nonempty opaque GraphQL node ID.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub(super) struct PullRequestNodeId(Box<str>);

impl PullRequestNodeId {
    pub(super) fn new(value: String) -> Result<Self> {
        if value.is_empty() {
            bail!("GitHub reported an empty pull request node ID");
        }
        Ok(Self(value.into_boxed_str()))
    }

    pub(super) fn as_str(&self) -> &str {
        &self.0
    }
}

/// The two values which together identify one GitHub pull request.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub(super) struct PullRequestIdentity {
    number: PullRequestNumber,
    node_id: PullRequestNodeId,
}

impl PullRequestIdentity {
    pub(super) fn new(number: u64, node_id: String) -> Result<Self> {
        Ok(Self {
            number: PullRequestNumber::new(number)?,
            node_id: PullRequestNodeId::new(node_id)?,
        })
    }

    pub(super) fn number(&self) -> PullRequestNumber {
        self.number
    }

    pub(super) fn node_id(&self) -> &PullRequestNodeId {
        &self.node_id
    }
}

/// The exact identity namespace observed while exhausting all local heads.
///
/// This is deliberately not a repository-wide claim. It retains every pull
/// request returned for the exact local ID set, including terminal and fork
/// rows which do not otherwise survive correlation. Create acknowledgements
/// must not collide with this observation or with one another.
#[derive(Debug)]
pub(super) struct ExactLocalPullRequestIdentities {
    numbers: HashSet<PullRequestNumber>,
    node_ids: HashSet<PullRequestNodeId>,
}

impl ExactLocalPullRequestIdentities {
    pub(super) fn new<'a>(
        identities: impl IntoIterator<Item = &'a PullRequestIdentity>,
    ) -> Result<Self> {
        let mut numbers = HashSet::new();
        let mut node_ids = HashSet::new();
        for identity in identities {
            if !numbers.insert(identity.number()) {
                bail!(
                    "exact local observation repeated pull request number {}",
                    identity.number().get()
                );
            }
            if !node_ids.insert(identity.node_id().clone()) {
                bail!("exact local observation repeated a pull request node ID");
            }
        }
        Ok(Self { numbers, node_ids })
    }

    pub(super) fn into_sets(self) -> (HashSet<PullRequestNumber>, HashSet<PullRequestNodeId>) {
        (self.numbers, self.node_ids)
    }
}

/// The only two supported base names for a managed pull request.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum BaseKind {
    Default,
    Owned,
}

impl BaseKind {
    /// Derives the complete base name from repository context and the change.
    pub(super) fn branch_name(self, default: &str, id: &GherritPrId) -> String {
        match self {
            Self::Default => default.to_owned(),
            Self::Owned => owned_base_name(id),
        }
    }
}

/// A classified base name coupled to the object GitHub observed for it.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct ObservedBase {
    kind: BaseKind,
    oid: ObjectId,
}

impl ObservedBase {
    pub(super) fn kind(&self) -> BaseKind {
        self.kind
    }

    pub(super) fn oid(&self) -> ObjectId {
        self.oid
    }
}

/// Same-repository OPEN evidence returned for one exact local change.
///
/// The object IDs are observation evidence, not validated publication state.
/// History validation later establishes whether the observed head and base
/// belong to the change's published history.
#[derive(Debug)]
pub(super) struct ManagedOpenPullRequest {
    id: GherritPrId,
    identity: PullRequestIdentity,
    head_oid: ObjectId,
    base: ObservedBase,
    title: Box<str>,
    body: Box<str>,
    has_landing_automation: bool,
}

pub(super) struct ManagedOpenParts {
    id: GherritPrId,
    identity: PullRequestIdentity,
    observed_base: ObservedBase,
    title: Box<str>,
    body: Box<str>,
}

impl ManagedOpenParts {
    pub(super) fn into_parts(
        self,
    ) -> (GherritPrId, PullRequestIdentity, ObservedBase, Box<str>, Box<str>) {
        (self.id, self.identity, self.observed_base, self.title, self.body)
    }
}

impl ManagedOpenPullRequest {
    pub(super) fn id(&self) -> &GherritPrId {
        &self.id
    }

    pub(super) fn identity(&self) -> &PullRequestIdentity {
        &self.identity
    }

    pub(super) fn head_oid(&self) -> ObjectId {
        self.head_oid
    }

    pub(super) fn base(&self) -> ObservedBase {
        self.base
    }

    #[cfg(test)]
    pub(super) fn title(&self) -> &str {
        &self.title
    }

    #[cfg(test)]
    pub(super) fn body(&self) -> &str {
        &self.body
    }

    pub(super) fn has_landing_automation(&self) -> bool {
        self.has_landing_automation
    }

    pub(super) fn into_validated_parts(self) -> ManagedOpenParts {
        ManagedOpenParts {
            id: self.id,
            identity: self.identity,
            observed_base: self.base,
            title: self.title,
            body: self.body,
        }
    }

    /// Constructs already-correlated OPEN evidence for semantic tests.
    #[cfg(test)]
    pub(super) fn from_typed_for_test(
        id: GherritPrId,
        identity: PullRequestIdentity,
        head_oid: ObjectId,
        base_kind: BaseKind,
        base_oid: ObjectId,
        title: String,
        body: String,
    ) -> Self {
        Self {
            id,
            identity,
            head_oid,
            base: ObservedBase { kind: base_kind, oid: base_oid },
            title: title.into_boxed_str(),
            body: body.into_boxed_str(),
            has_landing_automation: false,
        }
    }

    /// Sets the already-decoded landing-automation fact in pure policy tests.
    #[cfg(test)]
    pub(super) fn with_landing_automation_for_test(mut self, value: bool) -> Self {
        self.has_landing_automation = value;
        self
    }
}

/// Correlated state for one local change, in exact local stack order.
#[derive(Debug)]
pub(super) enum LocalPullRequestObservation {
    Open(ManagedOpenPullRequest),
    Absent(AbsentPullRequest),
}

impl LocalPullRequestObservation {
    pub(super) fn id(&self) -> &GherritPrId {
        match self {
            Self::Open(pull_request) => pull_request.id(),
            Self::Absent(absent) => absent.id(),
        }
    }
}

/// Proof that one exhausted exact all-state connection contained no
/// same-repository pull request.
///
/// Production code can construct this value only by exhausting the connection
/// associated with its retained change ID. The connection may have contained
/// cross-repository rows; those cannot prevent same-repository creation.
#[derive(Debug)]
pub(super) struct AbsentPullRequest {
    id: GherritPrId,
}

impl AbsentPullRequest {
    fn after_exhaustion(id: GherritPrId) -> Self {
        Self { id }
    }

    #[cfg(test)]
    pub(super) fn for_test(id: GherritPrId) -> Self {
        Self::after_exhaustion(id)
    }

    pub(super) fn id(&self) -> &GherritPrId {
        &self.id
    }
}

/// Complete correlation output in exact local stack order.
#[derive(Debug)]
pub(super) struct CorrelatedPullRequests {
    local: Box<[LocalPullRequestObservation]>,
    identities: ExactLocalPullRequestIdentities,
}

impl CorrelatedPullRequests {
    #[cfg(test)]
    pub(super) fn local(&self) -> &[LocalPullRequestObservation] {
        &self.local
    }

    pub(super) fn into_parts(
        self,
    ) -> (Box<[LocalPullRequestObservation]>, ExactLocalPullRequestIdentities) {
        (self.local, self.identities)
    }

    /// Constructs already-correlated planner input without GitHub wire data.
    #[cfg(test)]
    pub(super) fn from_typed_for_test(local: Vec<LocalPullRequestObservation>) -> Result<Self> {
        let mut ids = HashSet::with_capacity(local.len());
        for observation in &local {
            if !ids.insert(observation.id()) {
                bail!("literal correlated test input repeats a change ID");
            }
        }
        let identities =
            ExactLocalPullRequestIdentities::new(local.iter().filter_map(|observation| {
                match observation {
                    LocalPullRequestObservation::Open(pull_request) => {
                        Some(pull_request.identity())
                    }
                    LocalPullRequestObservation::Absent(_) => None,
                }
            }))?;
        Ok(Self { local: local.into_boxed_slice(), identities })
    }
}

/// Correlates completely exhausted all-state connections for the local stack.
///
/// Cross-repository rows are ignored. For each local change, one OPEN row wins
/// over any historical rows, more than one OPEN row is ambiguous, and terminal
/// rows reject the push only when no OPEN row exists. Rejections for terminal
/// rows are accumulated across the stack so the user can address them at once.
pub(super) fn correlate_local(
    default_branch: &DefaultBranch,
    observed: CompleteLocalPullRequests,
) -> Result<CorrelatedPullRequests> {
    let (entries, identities) = observed.into_parts();
    correlate_local_entries_with_identities(default_branch.name(), entries, identities)
}

#[cfg(test)]
fn correlate_local_entries(
    default_branch: &str,
    entries: Box<[(GherritPrId, Box<[ObservedPullRequest]>)]>,
) -> Result<CorrelatedPullRequests> {
    let identities = ExactLocalPullRequestIdentities::new(
        entries.iter().flat_map(|(_, rows)| rows).map(|row| &row.identity),
    )?;
    correlate_local_entries_with_identities(default_branch, entries, identities)
}

fn correlate_local_entries_with_identities(
    default_branch: &str,
    entries: Box<[(GherritPrId, Box<[ObservedPullRequest]>)]>,
    identities: ExactLocalPullRequestIdentities,
) -> Result<CorrelatedPullRequests> {
    let mut local = Vec::with_capacity(entries.len());
    let mut terminal_rows = Vec::new();
    let mut terminal_row_count = 0_usize;

    for (id, pull_requests) in entries.into_vec() {
        let mut open = None;
        let mut terminals = Vec::new();

        for pull_request in pull_requests {
            if pull_request.is_cross_repository {
                continue;
            }
            if pull_request.head_branch != id.as_str() {
                bail!("local pull request evidence for '{}' returned another head", id.as_str());
            }
            match pull_request.state {
                PullRequestState::Open if open.is_some() => bail!(
                    "GitHub has more than one same-repository OPEN pull request for '{}'",
                    id.as_str()
                ),
                PullRequestState::Open => open = Some(pull_request),
                PullRequestState::Closed | PullRequestState::Merged => {
                    terminals.push(pull_request);
                }
            }
        }

        if let Some(pull_request) = open {
            local.push(LocalPullRequestObservation::Open(correlate_open(
                default_branch,
                id,
                pull_request,
            )?));
        } else if terminals.is_empty() {
            local
                .push(LocalPullRequestObservation::Absent(AbsentPullRequest::after_exhaustion(id)));
        } else {
            for pull_request in terminals {
                terminal_row_count = terminal_row_count.saturating_add(1);
                if terminal_rows.len() < MAX_TERMINAL_DIAGNOSTIC_ROWS {
                    terminal_rows.push((
                        id.clone(),
                        pull_request.identity.number(),
                        pull_request.state,
                    ));
                }
            }
        }
    }

    if terminal_row_count != 0 {
        let displayed_row_count = terminal_rows.len();
        let mut message = terminal_rows
            .into_iter()
            .map(|(id, number, state)| {
                let id = bounded_diagnostic_detail(id.as_str());
                let recovery = match state {
                    PullRequestState::Closed => format!(
                        "PR #{} is closed. Reopen it or change the commit's gherrit-pr-id to start a new review.",
                        number.get()
                    ),
                    PullRequestState::Merged => format!(
                        "PR #{} is merged. Change the commit's gherrit-pr-id to start a new review.",
                        number.get()
                    ),
                    PullRequestState::Open => unreachable!("OPEN rows were separated above"),
                };
                format!("Cannot push GHerrit change '{id}' because {recovery}\n")
            })
            .collect::<String>();
        let omitted = terminal_row_count - displayed_row_count;
        if omitted != 0 {
            message.push_str(&format!("... and {omitted} more terminal pull request(s).\n"));
        }
        message.pop();
        bail!(message);
    }

    Ok(CorrelatedPullRequests { local: local.into_boxed_slice(), identities })
}

fn correlate_open(
    default_branch: &str,
    id: GherritPrId,
    pull_request: ObservedPullRequest,
) -> Result<ManagedOpenPullRequest> {
    let identity = pull_request.identity;
    let base = classify_base(default_branch, &id, &pull_request.base_branch, pull_request.base_oid)
        .wrap_err_with(|| {
            format!(
                "GitHub pull request #{} for '{}' has an unsupported base",
                identity.number().get(),
                id.as_str()
            )
        })?;
    Ok(ManagedOpenPullRequest {
        id,
        identity,
        head_oid: pull_request.head_oid,
        base,
        title: pull_request.title.into_boxed_str(),
        body: pull_request.body.into_boxed_str(),
        has_landing_automation: pull_request.has_auto_merge_request
            || pull_request.is_in_merge_queue,
    })
}

fn classify_base(
    default_branch: &str,
    id: &GherritPrId,
    observed_name: &str,
    oid: ObjectId,
) -> Result<ObservedBase> {
    let kind = if observed_name == default_branch {
        BaseKind::Default
    } else if observed_name == owned_base_name(id) {
        BaseKind::Owned
    } else {
        let default_branch = bounded_diagnostic_detail(default_branch);
        let owned_base = bounded_diagnostic_detail(&owned_base_name(id));
        let observed_name = bounded_diagnostic_detail(observed_name);
        bail!("expected '{default_branch}' or '{owned_base}', found '{observed_name}'");
    };
    Ok(ObservedBase { kind, oid })
}

pub(super) fn owned_base_name(id: &GherritPrId) -> String {
    format!("gherrit-bases/{}", id.as_str())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn object_id(byte: u8) -> ObjectId {
        ObjectId::from_bytes_or_panic(&[byte; 20])
    }

    fn id(value: &str) -> GherritPrId {
        GherritPrId::from_ref_component(value.as_bytes()).unwrap()
    }

    #[allow(clippy::too_many_arguments)]
    fn observed(
        number: u64,
        node_id: &str,
        head: &str,
        base: &str,
        body: &str,
        state: PullRequestState,
        cross_repository: bool,
    ) -> ObservedPullRequest {
        ObservedPullRequest {
            identity: PullRequestIdentity::new(number, node_id.to_owned()).unwrap(),
            title: format!("title for {head}"),
            body: body.to_owned(),
            base_branch: base.to_owned(),
            head_branch: head.to_owned(),
            base_oid: object_id(2),
            head_oid: object_id(3),
            state,
            is_cross_repository: cross_repository,
            has_auto_merge_request: false,
            is_in_merge_queue: false,
        }
    }

    fn entries(
        entries: impl IntoIterator<Item = (&'static str, Vec<ObservedPullRequest>)>,
    ) -> Box<[(GherritPrId, Box<[ObservedPullRequest]>)]> {
        entries
            .into_iter()
            .map(|(value, pull_requests)| (id(value), pull_requests.into_boxed_slice()))
            .collect()
    }

    fn kinds(correlated: &CorrelatedPullRequests) -> Vec<(&str, &'static str)> {
        correlated
            .local()
            .iter()
            .map(|observation| {
                (
                    observation.id().as_str(),
                    match observation {
                        LocalPullRequestObservation::Open(_) => "open",
                        LocalPullRequestObservation::Absent(_) => "absent",
                    },
                )
            })
            .collect()
    }

    #[test]
    fn pull_request_identity_wrappers_enforce_exact_graphql_bounds() {
        for number in [1, i32::MAX as u64] {
            assert_eq!(
                PullRequestNumber::new(number).unwrap().get(),
                u32::try_from(number).unwrap()
            );
        }
        for number in [0, i32::MAX as u64 + 1, u64::MAX] {
            assert!(PullRequestNumber::new(number).is_err(), "number={number}");
        }

        assert_eq!(PullRequestNodeId::new("node".to_owned()).unwrap().as_str(), "node");
        assert_eq!(PullRequestNodeId::new(" ".to_owned()).unwrap().as_str(), " ");
        assert!(PullRequestNodeId::new(String::new()).is_err());
    }

    #[test]
    fn exact_local_correlation_preserves_order_and_seals_absence() {
        let correlated = correlate_local_entries(
            "main",
            entries([
                (
                    "A",
                    vec![observed(7, "PR_A", "A", "main", "opaque", PullRequestState::Open, false)],
                ),
                ("B", Vec::new()),
            ]),
        )
        .unwrap();

        assert_eq!(kinds(&correlated), [("A", "open"), ("B", "absent")]);
        let LocalPullRequestObservation::Absent(b) = &correlated.local()[1] else {
            panic!("B must be proven absent");
        };
        assert_eq!(b.id().as_str(), "B");
    }

    #[test]
    fn one_open_wins_over_history_and_body_is_opaque() {
        let body = "<!-- gherrit-meta: this is ordinary text -->";
        let mut open =
            observed(9, "PR_G", "G", "gherrit-bases/G", body, PullRequestState::Open, false);
        open.is_in_merge_queue = true;
        let correlated = correlate_local_entries(
            "main",
            entries([(
                "G",
                vec![
                    observed(
                        3,
                        "OLD_CLOSED",
                        "G",
                        "main",
                        "ignored",
                        PullRequestState::Closed,
                        false,
                    ),
                    open,
                    observed(
                        4,
                        "OLD_MERGED",
                        "G",
                        "main",
                        "ignored",
                        PullRequestState::Merged,
                        false,
                    ),
                ],
            )]),
        )
        .unwrap();

        let LocalPullRequestObservation::Open(open) = &correlated.local()[0] else {
            panic!("the current OPEN pull request must win");
        };
        assert_eq!(open.identity().number().get(), 9);
        assert_eq!(open.base().kind(), BaseKind::Owned);
        assert_eq!(open.base().oid(), object_id(2));
        assert_eq!(open.head_oid(), object_id(3));
        assert_eq!(open.title(), "title for G");
        assert_eq!(open.body(), body);
        assert!(open.has_landing_automation());
    }

    #[test]
    fn correlation_retains_identities_from_open_terminal_and_fork_rows() {
        let observed = CompleteLocalPullRequests::for_test(vec![(
            id("G"),
            vec![
                observed(1, "PR_OPEN", "G", "main", "", PullRequestState::Open, false),
                observed(2, "PR_CLOSED", "G", "main", "", PullRequestState::Closed, false),
                observed(3, "PR_FORK", "G", "main", "", PullRequestState::Merged, true),
            ],
        )])
        .unwrap();
        let default = DefaultBranch::new("main".to_owned(), object_id(1)).unwrap();
        let (_, identities) = correlate_local(&default, observed).unwrap().into_parts();
        let (numbers, node_ids) = identities.into_sets();

        assert_eq!(
            numbers.into_iter().map(PullRequestNumber::get).collect::<HashSet<_>>(),
            HashSet::from([1, 2, 3])
        );
        assert_eq!(
            node_ids.into_iter().map(|node_id| node_id.as_str().to_owned()).collect::<HashSet<_>>(),
            HashSet::from(["PR_OPEN".to_owned(), "PR_CLOSED".to_owned(), "PR_FORK".to_owned()])
        );
    }

    #[test]
    fn more_than_one_same_repository_open_is_ambiguous() {
        let result = correlate_local_entries(
            "main",
            entries([(
                "G",
                vec![
                    observed(1, "ONE", "G", "main", "", PullRequestState::Open, false),
                    observed(2, "TWO", "G", "main", "", PullRequestState::Open, false),
                ],
            )]),
        );

        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("more than one same-repository OPEN pull request")
        );
    }

    #[test]
    fn terminal_only_connections_are_reported_together() {
        let error = correlate_local_entries(
            "main",
            entries([
                (
                    "A",
                    vec![observed(1, "A_CLOSED", "A", "main", "", PullRequestState::Closed, false)],
                ),
                (
                    "B",
                    vec![
                        observed(2, "B_CLOSED", "B", "main", "", PullRequestState::Closed, false),
                        observed(3, "B_MERGED", "B", "main", "", PullRequestState::Merged, false),
                    ],
                ),
            ]),
        )
        .unwrap_err()
        .to_string();

        assert!(error.contains("change 'A' because PR #1 is closed"));
        assert!(error.contains("change 'B' because PR #2 is closed"));
        assert!(error.contains("change 'B' because PR #3 is merged"));
        assert!(!error.contains("rebase"));
    }

    #[test]
    fn terminal_diagnostics_are_bounded_and_prescribe_state_specific_recovery() {
        let entries = (0..MAX_TERMINAL_DIAGNOSTIC_ROWS + 7)
            .map(|index| {
                let id = format!("G{}", "x".repeat(1_000 + index));
                let state =
                    if index == 0 { PullRequestState::Closed } else { PullRequestState::Merged };
                (
                    GherritPrId::from_ref_component(id.as_bytes()).unwrap(),
                    vec![observed(
                        u64::try_from(index + 1).unwrap(),
                        &format!("PR_{index}"),
                        &id,
                        "main",
                        "",
                        state,
                        false,
                    )]
                    .into_boxed_slice(),
                )
            })
            .collect();

        let error = correlate_local_entries("main", entries).unwrap_err().to_string();

        assert!(error.contains("PR #1 is closed. Reopen it or change the commit's gherrit-pr-id"));
        assert!(error.contains("PR #2 is merged. Change the commit's gherrit-pr-id"));
        assert!(error.contains("... and 7 more terminal pull request(s)."));
        assert!(!error.contains("PR #21"));
        assert!(error.len() < 16_000, "diagnostic was {} bytes", error.len());
    }

    #[test]
    fn cross_repository_rows_are_ignored_without_interpreting_their_fields() {
        let correlated = correlate_local_entries(
            "main",
            entries([(
                "G",
                vec![observed(
                    1,
                    "FORK",
                    "not-the-requested-head",
                    "unsupported-base",
                    "<!-- gherrit-meta: malformed -->",
                    PullRequestState::Merged,
                    true,
                )],
            )]),
        )
        .unwrap();

        assert_eq!(kinds(&correlated), [("G", "absent")]);
    }

    #[test]
    fn same_repository_rows_must_match_their_exact_requested_head() {
        let error = correlate_local_entries(
            "main",
            entries([(
                "G",
                vec![observed(1, "WRONG", "H", "main", "", PullRequestState::Open, false)],
            )]),
        )
        .unwrap_err()
        .to_string();

        assert!(error.contains("evidence for 'G' returned another head"));
    }

    #[test]
    fn managed_bases_are_exactly_default_or_the_changes_owned_base() {
        for (base, expected) in [
            ("main", Some(BaseKind::Default)),
            ("gherrit-bases/G", Some(BaseKind::Owned)),
            ("gherrit-bases/H", None),
            ("feature", None),
        ] {
            let result = correlate_local_entries(
                "main",
                entries([(
                    "G",
                    vec![observed(1, "PR_G", "G", base, "", PullRequestState::Open, false)],
                )]),
            );
            match expected {
                Some(expected) => {
                    let correlated = result.unwrap();
                    let LocalPullRequestObservation::Open(open) = &correlated.local()[0] else {
                        panic!("G must be open");
                    };
                    assert_eq!(open.base().kind(), expected);
                    assert_eq!(open.base().kind().branch_name("main", open.id()), base);
                }
                None => assert!(result.is_err(), "base={base}"),
            }
        }
    }

    #[test]
    fn response_derived_base_diagnostics_are_terminal_safe_and_bounded() {
        let returned = format!("{}\nnot-disclosed", "x".repeat(1_000));
        let error =
            classify_base("main", &id("G"), &returned, object_id(1)).unwrap_err().to_string();

        assert!(!error.contains('\n'));
        assert!(!error.contains("not-disclosed"));
        assert!(error.len() < 400);
    }
}

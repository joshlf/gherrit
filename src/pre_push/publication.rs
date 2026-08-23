//! Pure publication-state normalization and ref-update planning.
//!
//! Observed remote heads and active histories are authoritative. Local tags
//! are deliberately not inputs: a fresh clone must derive the same next
//! version as the repository which originally published the change.

use std::{
    collections::{BTreeMap, BTreeSet},
    fmt,
    process::ExitStatus,
};

use color_eyre::eyre::{Result, bail, eyre};
use gix::ObjectId;

use super::{
    destination::PushDestination,
    history::ValidatedChangeHistory,
    local::{GherritPrId, LocalChange},
    remote::{ObservedChange, ObservedStack},
    subprocess,
    version::Version,
};

const FIXED_PUSH_OPTIONS: [&str; 7] = [
    "--porcelain",
    "--atomic",
    "--no-verify",
    "--no-follow-tags",
    "--recurse-submodules=no",
    "--no-signed",
    "--no-force-if-includes",
];
// Windows command lines are limited to roughly 32 KiB. All variable push
// arguments are ASCII, so their byte lengths equal their UTF-16 code-unit
// lengths before the platform's quoting. Limiting those arguments to 16 KiB,
// including one separator per argument, leaves half of the limit for the Git
// executable, private-remote adapter configuration, fixed push arguments,
// reserved remote name, quoting, and terminating NUL. It also bounds POSIX
// argv encoding conservatively.
const PUSH_VARIABLE_ARGV_BUDGET_BYTES: usize = 16 * 1024;

#[derive(Clone, Copy)]
enum LeaseExpectation {
    Absent,
    At(ObjectId),
}

impl LeaseExpectation {
    fn render(self) -> String {
        match self {
            Self::Absent => String::new(),
            Self::At(oid) => oid.to_string(),
        }
    }

    fn receipt_transition(self) -> ExpectedRefTransition {
        match self {
            Self::Absent => ExpectedRefTransition::CreateOrAlreadyDesired,
            Self::At(_) => ExpectedRefTransition::UpdateOrAlreadyDesired,
        }
    }
}

struct OwnedPushTupleArguments {
    options: [String; 3],
    refspecs: [String; 3],
    expected_receipts: [(String, ExpectedRefReceipt); 3],
}

impl OwnedPushTupleArguments {
    fn from_history(history: &ValidatedChangeHistory) -> Result<Option<Self>> {
        if !history.needs_publication() {
            return Ok(None);
        }
        let id = history.id().as_str();
        let proposed = history.proposed();
        let current = history.published_current();
        let head_expectation = current
            .map(|current| LeaseExpectation::At(current.revision().head()))
            .unwrap_or(LeaseExpectation::Absent);
        let base_expectation = current
            .map(|current| LeaseExpectation::At(current.revision().first_parent()))
            .unwrap_or(LeaseExpectation::Absent);
        let head = format!("refs/heads/{id}");
        let base = format!("refs/heads/gherrit-bases/{id}");
        let tag = format!("refs/tags/gherrit/{id}/v{}", history.projected_current().number());
        let options = [
            format!("--force-with-lease={head}:{}", head_expectation.render()),
            format!("--force-with-lease={base}:{}", base_expectation.render()),
            format!("--force-with-lease={tag}:"),
        ];
        let desired_head = proposed.head().to_string();
        let desired_base = proposed.first_parent().to_string();
        let refspecs = [
            format!("{desired_head}:{head}"),
            format!("{desired_base}:{base}"),
            format!("{desired_head}:{tag}"),
        ];
        let expected_receipts = [
            (
                head,
                ExpectedRefReceipt::new(
                    desired_head.clone(),
                    head_expectation.receipt_transition(),
                ),
            ),
            (base, ExpectedRefReceipt::new(desired_base, base_expectation.receipt_transition())),
            (
                tag,
                ExpectedRefReceipt::new(
                    desired_head,
                    ExpectedRefTransition::CreateOrAlreadyDesired,
                ),
            ),
        ];
        Ok(Some(Self { options, refspecs, expected_receipts }))
    }

    fn encoded_argv_bytes(&self) -> usize {
        self.options.iter().chain(&self.refspecs).map(|argument| argument.len() + 1).sum()
    }
}

struct BudgetedOwnedPushTuple {
    arguments: OwnedPushTupleArguments,
    encoded_argv_bytes: usize,
}

/// Plans exact, tuple-indivisible owned-base publication from validated history.
pub(super) fn plan_owned_base_pushes<'destination>(
    destination: &'destination PushDestination,
    histories: &[&ValidatedChangeHistory],
) -> Result<Option<PreparedPushes<'destination>>> {
    let requests =
        plan_owned_base_requests_with_budget(histories, PUSH_VARIABLE_ARGV_BUDGET_BYTES)?;
    PreparedPushes::new(destination, requests)
}

/// One required missing marker, derived only from validated local history.
#[derive(Clone, Debug)]
pub(super) struct MissingPullRequestMarker {
    id: GherritPrId,
    target: ObjectId,
}

impl MissingPullRequestMarker {
    pub(super) fn from_history(history: &ValidatedChangeHistory) -> Option<Self> {
        history.pull_request_marker().is_none().then(|| Self {
            id: history.id().clone(),
            target: history.projected_current().revision().head(),
        })
    }
}

struct MarkerPushArguments {
    option: String,
    refspec: String,
    expected_receipt: (String, ExpectedRefReceipt),
}

impl MarkerPushArguments {
    fn new(marker: &MissingPullRequestMarker) -> Self {
        let destination = format!("refs/tags/gherrit/{}/pr", marker.id.as_str());
        let source = marker.target.to_string();
        Self {
            option: format!("--force-with-lease={destination}:"),
            refspec: format!("{source}:{destination}"),
            expected_receipt: (
                destination,
                ExpectedRefReceipt::new(source, ExpectedRefTransition::CreateOrAlreadyDesired),
            ),
        }
    }

    fn encoded_argv_bytes(&self) -> usize {
        self.option.len() + self.refspec.len() + 2
    }
}

/// Plans one absent-leased durable marker per unmarked local pull request.
pub(super) fn plan_marker_pushes<'destination>(
    destination: &'destination PushDestination,
    markers: &[MissingPullRequestMarker],
) -> Result<Option<PreparedPushes<'destination>>> {
    let requests = plan_marker_requests_with_budget(markers, PUSH_VARIABLE_ARGV_BUDGET_BYTES)?;
    PreparedPushes::new(destination, requests)
}

fn plan_marker_requests_with_budget(
    markers: &[MissingPullRequestMarker],
    budget: usize,
) -> Result<Box<[PushRequest]>> {
    let arguments = markers.iter().map(MarkerPushArguments::new).collect::<Vec<_>>();
    ExpectedReceipts::new(arguments.iter().map(|value| value.expected_receipt.clone()))?;

    let mut requests = Vec::new();
    let mut current = Vec::new();
    let mut current_bytes = 0;
    for (index, arguments) in arguments.into_iter().enumerate() {
        let encoded = arguments.encoded_argv_bytes();
        if encoded > budget {
            bail!(
                "Git marker target {index} requires {encoded} bytes of variable push arguments, which exceeds the {budget}-byte variable-argument budget"
            );
        }
        if !current.is_empty() && current_bytes > budget - encoded {
            requests.push(marker_request(std::mem::take(&mut current))?);
            current_bytes = 0;
        }
        current_bytes += encoded;
        current.push(arguments);
    }
    if !current.is_empty() {
        requests.push(marker_request(current)?);
    }
    Ok(requests.into_boxed_slice())
}

fn marker_request(arguments: Vec<MarkerPushArguments>) -> Result<PushRequest> {
    let mut options = FIXED_PUSH_OPTIONS.map(str::to_owned).to_vec();
    let mut refspecs = Vec::with_capacity(arguments.len());
    let mut receipts = Vec::with_capacity(arguments.len());
    for arguments in arguments {
        options.push(arguments.option);
        refspecs.push(arguments.refspec);
        receipts.push(arguments.expected_receipt);
    }
    Ok(PushRequest { options, refspecs, expected: ExpectedReceipts::new(receipts)? })
}

fn plan_owned_base_requests_with_budget(
    histories: &[&ValidatedChangeHistory],
    budget: usize,
) -> Result<Box<[PushRequest]>> {
    let arguments = histories
        .iter()
        .map(|history| OwnedPushTupleArguments::from_history(history))
        .collect::<Result<Vec<_>>>()?;
    let tuples = arguments
        .into_iter()
        .flatten()
        .enumerate()
        .map(|(index, arguments)| {
            let encoded_argv_bytes = arguments.encoded_argv_bytes();
            if encoded_argv_bytes > budget {
                bail!(
                    "Git publication target {index} requires {encoded_argv_bytes} bytes of variable push arguments, which exceeds the {budget}-byte variable-argument budget"
                );
            }
            Ok(BudgetedOwnedPushTuple { arguments, encoded_argv_bytes })
        })
        .collect::<Result<Vec<_>>>()?;
    ExpectedReceipts::new(
        tuples.iter().flat_map(|tuple| tuple.arguments.expected_receipts.iter().cloned()),
    )?;

    let mut batches = Vec::<Vec<BudgetedOwnedPushTuple>>::new();
    let mut current = Vec::new();
    let mut current_bytes = 0;
    for tuple in tuples {
        if !current.is_empty() && current_bytes > budget - tuple.encoded_argv_bytes {
            batches.push(std::mem::take(&mut current));
            current_bytes = 0;
        }
        current_bytes += tuple.encoded_argv_bytes;
        current.push(tuple);
    }
    if !current.is_empty() {
        batches.push(current);
    }

    batches
        .into_iter()
        .map(|batch| {
            let mut options = FIXED_PUSH_OPTIONS.map(str::to_owned).to_vec();
            let mut refspecs = Vec::with_capacity(batch.len() * 3);
            let mut receipts = Vec::with_capacity(batch.len() * 3);
            for tuple in batch {
                options.extend(tuple.arguments.options);
                refspecs.extend(tuple.arguments.refspecs);
                receipts.extend(tuple.arguments.expected_receipts);
            }
            Ok(PushRequest { options, refspecs, expected: ExpectedReceipts::new(receipts)? })
        })
        .collect::<Result<Box<[_]>>>()
}

#[cfg(test)]
fn plan_owned_base_requests(histories: &[&ValidatedChangeHistory]) -> Result<Box<[PushRequest]>> {
    plan_owned_base_requests_with_budget(histories, PUSH_VARIABLE_ARGV_BUDGET_BYTES)
}

#[derive(Clone, Debug)]
enum PushTarget {
    First {
        id: GherritPrId,
        desired_head: ObjectId,
    },
    Advance {
        id: GherritPrId,
        desired_head: ObjectId,
        expected_head: ObjectId,
        next_version: Version,
    },
}

impl PushTarget {
    fn first(id: GherritPrId, desired_head: ObjectId) -> Self {
        Self::First { id, desired_head }
    }

    fn advance(
        id: GherritPrId,
        desired_head: ObjectId,
        expected_head: ObjectId,
        latest_version: Version,
    ) -> Result<Self> {
        let next_version = latest_version
            .next()
            .ok_or_else(|| eyre!("Remote GHerrit change '{}' has no next version", id.as_str()))?;
        Ok(Self::Advance { id, desired_head, expected_head, next_version })
    }

    fn id(&self) -> &GherritPrId {
        match self {
            Self::First { id, .. } | Self::Advance { id, .. } => id,
        }
    }

    fn desired_head(&self) -> ObjectId {
        match self {
            Self::First { desired_head, .. } | Self::Advance { desired_head, .. } => *desired_head,
        }
    }

    fn version(&self) -> Version {
        match self {
            Self::First { .. } => Version::FIRST,
            Self::Advance { next_version, .. } => *next_version,
        }
    }

    fn expected_head(&self) -> Option<ObjectId> {
        match self {
            Self::First { .. } => None,
            Self::Advance { expected_head, .. } => Some(*expected_head),
        }
    }
}

#[derive(Debug)]
struct PushPlan {
    first: BudgetedPushTuple,
    rest: Vec<BudgetedPushTuple>,
}

impl PushPlan {
    fn new(first: BudgetedPushTuple) -> Self {
        Self { first, rest: Vec::new() }
    }

    fn push(&mut self, tuple: BudgetedPushTuple) {
        self.rest.push(tuple);
    }

    #[cfg(test)]
    fn tuples(&self) -> impl Iterator<Item = &BudgetedPushTuple> {
        std::iter::once(&self.first).chain(&self.rest)
    }

    #[cfg(test)]
    fn arguments(&self) -> (Vec<String>, Vec<String>) {
        let tuple_count = self.tuples().count();
        let mut options = FIXED_PUSH_OPTIONS.map(str::to_owned).to_vec();
        let mut refspecs = Vec::with_capacity(tuple_count * 2);
        options.reserve(tuple_count * 2);
        for tuple in self.tuples() {
            options.extend(tuple.arguments.options.iter().cloned());
            refspecs.extend(tuple.arguments.refspecs.iter().cloned());
        }
        (options, refspecs)
    }

    fn into_request(self) -> Result<PushRequest> {
        let tuple_count = 1 + self.rest.len();
        let mut options = FIXED_PUSH_OPTIONS.map(str::to_owned).to_vec();
        let mut refspecs = Vec::with_capacity(tuple_count * 2);
        let mut expected_receipts = Vec::with_capacity(tuple_count * 2);
        options.reserve(tuple_count * 2);
        for tuple in std::iter::once(self.first).chain(self.rest) {
            options.extend(tuple.arguments.options);
            refspecs.extend(tuple.arguments.refspecs);
            expected_receipts.extend(tuple.arguments.expected_receipts);
        }
        Ok(PushRequest { options, refspecs, expected: ExpectedReceipts::new(expected_receipts)? })
    }

    #[cfg(test)]
    fn into_arguments(self) -> (Vec<String>, Vec<String>) {
        let request = self.into_request().expect("a planned request has unique destinations");
        (request.options, request.refspecs)
    }
}

/// The immutable boundary between publication planning and a `git push`
/// invocation. Receipt expectations are rendered while the plan is built,
/// rather than reconstructed from later mutable state or process output.
#[derive(Debug)]
struct PushRequest {
    options: Vec<String>,
    refspecs: Vec<String>,
    expected: ExpectedReceipts,
}

impl PushRequest {
    fn options(&self) -> impl Iterator<Item = String> + '_ {
        self.options.iter().cloned()
    }

    fn refspecs(&self) -> impl Iterator<Item = String> + '_ {
        self.refspecs.iter().cloned()
    }

    fn outcome(&self, status: &ExitStatus, stdout: &[u8]) -> PushOutcome {
        if status.code() == Some(0) {
            classify_push_receipts(&self.expected, stdout)
        } else {
            PushOutcome::Indeterminate
        }
    }
}

#[derive(Debug)]
struct ExpectedReceipts {
    refs: BTreeMap<String, ExpectedRefReceipt>,
}

impl ExpectedReceipts {
    fn new(receipts: impl IntoIterator<Item = (String, ExpectedRefReceipt)>) -> Result<Self> {
        let mut refs = BTreeMap::new();
        for (destination, receipt) in receipts {
            if refs.insert(destination.clone(), receipt).is_some() {
                bail!("Git publication plans destination '{destination}' more than once");
            }
        }
        Ok(Self { refs })
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct ExpectedRefReceipt {
    source: String,
    transition: ExpectedRefTransition,
}

impl ExpectedRefReceipt {
    fn new(source: String, transition: ExpectedRefTransition) -> Self {
        Self { source, transition }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ExpectedRefTransition {
    CreateOrAlreadyDesired,
    UpdateOrAlreadyDesired,
}

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
enum PushOutcome {
    AcknowledgedSuccess,
    Indeterminate,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ReceiptStatus {
    FastForward,
    Forced,
    New,
    AlreadyDesired,
    Failed,
}

impl ReceiptStatus {
    fn satisfies(self, expected: ExpectedRefTransition) -> bool {
        matches!(
            (expected, self),
            (ExpectedRefTransition::CreateOrAlreadyDesired, Self::New | Self::AlreadyDesired)
                | (
                    ExpectedRefTransition::UpdateOrAlreadyDesired,
                    Self::FastForward | Self::Forced | Self::AlreadyDesired
                )
        )
    }
}

/// Classifies `git push --porcelain` standard output without allowing its
/// human-readable destination header to escape into a diagnostic.
fn classify_push_receipts(expected: &ExpectedReceipts, stdout: &[u8]) -> PushOutcome {
    let Some(receipts) = parse_push_receipts(expected, stdout) else {
        return PushOutcome::Indeterminate;
    };
    if receipts.iter().all(|(expected, status)| status.satisfies(*expected)) {
        PushOutcome::AcknowledgedSuccess
    } else {
        PushOutcome::Indeterminate
    }
}

fn parse_push_receipts(
    expected: &ExpectedReceipts,
    stdout: &[u8],
) -> Option<Vec<(ExpectedRefTransition, ReceiptStatus)>> {
    let output = std::str::from_utf8(stdout).ok()?;
    let (output, line_ending) = output
        .strip_suffix("\r\n")
        .map(|output| (output, "\r\n"))
        .or_else(|| output.strip_suffix('\n').map(|output| (output, "\n")))?;
    let lines = output.split(line_ending).collect::<Vec<_>>();
    let (header, body) = lines.split_first()?;
    let (footer, status_lines) = body.split_last()?;
    let displayed_destination = header.strip_prefix("To ")?;
    if displayed_destination.is_empty()
        || displayed_destination.chars().any(char::is_control)
        || *footer != "Done"
    {
        return None;
    }

    let mut receipts = Vec::with_capacity(expected.refs.len());
    let mut seen = BTreeSet::new();
    for line in status_lines {
        if line.is_empty() {
            return None;
        }
        let mut fields = line.split('\t');
        let flag = fields.next()?;
        let refs = fields.next()?;
        let summary = fields.next()?;
        if fields.next().is_some() {
            return None;
        }
        let status = match flag.as_bytes() {
            [b' '] => ReceiptStatus::FastForward,
            [b'+'] => ReceiptStatus::Forced,
            [b'*'] => ReceiptStatus::New,
            [b'='] => ReceiptStatus::AlreadyDesired,
            [b'!'] => ReceiptStatus::Failed,
            _ => return None,
        };
        let (source, destination) = refs.split_once(':')?;
        let expected_ref = expected.refs.get(destination)?;
        if source.is_empty()
            || destination.is_empty()
            || summary.is_empty()
            || !destination.starts_with("refs/")
            || [source, destination, summary]
                .into_iter()
                .any(|field| field.chars().any(char::is_control))
            || source != expected_ref.source
            || !seen.insert(destination)
        {
            return None;
        }
        receipts.push((expected_ref.transition, status));
    }
    (seen.len() == expected.refs.len()).then_some(receipts)
}

/// One nonempty sequence of fully preflighted pushes bound to the destination
/// which supplied its publication evidence.
///
/// Both publication planners use this boundary. In particular, the active
/// legacy path now deliberately has the same finite remote-command deadline as
/// the dormant owned-base path instead of waiting forever for a hung push.
pub(super) struct PreparedPushes<'destination> {
    destination: &'destination PushDestination,
    first: PushRequest,
    rest: Box<[PushRequest]>,
}

impl fmt::Debug for PreparedPushes<'_> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("PreparedPushes")
            .field("batch_count", &(1 + self.rest.len()))
            .finish()
    }
}

impl<'destination> PreparedPushes<'destination> {
    fn new(
        destination: &'destination PushDestination,
        requests: impl IntoIterator<Item = PushRequest>,
    ) -> Result<Option<Self>> {
        let mut requests = requests.into_iter().collect::<Vec<_>>();
        if requests.is_empty() {
            return Ok(None);
        }
        if requests.iter().any(|request| request.expected.refs.is_empty()) {
            bail!("Git publication contains an empty push request");
        }
        // Validate the complete cross-batch destination set before the first
        // request can escape to execution. A duplicate in a later batch must
        // not be discovered after an acknowledged prefix has landed.
        ExpectedReceipts::new(requests.iter().flat_map(|request| {
            request
                .expected
                .refs
                .iter()
                .map(|(destination, receipt)| (destination.clone(), receipt.clone()))
        }))?;

        let first = requests.remove(0);
        Ok(Some(Self { destination, first, rest: requests.into_boxed_slice() }))
    }

    /// Executes every batch in order and acknowledges only exact receipts.
    pub(super) async fn publish(self) -> Result<()> {
        self.publish_with_timeout(subprocess::REMOTE_GIT_EXECUTION_TIMEOUT).await
    }

    async fn publish_with_timeout(self, timeout: std::time::Duration) -> Result<()> {
        for request in std::iter::once(self.first).chain(self.rest) {
            log::info!("Pushing chunk to remote...");
            let output = subprocess::output(
                self.destination.push(request.options(), request.refspecs()),
                timeout,
            )
            .await
            .map_err(|error| {
                eyre!(
                    "Could not execute or acknowledge `git push` for GHerrit remote '{}'; remote refs may or may not have changed. Run GHerrit again to observe them before continuing: {error}",
                    self.destination.configured_remote()
                )
            })?;
            if request.outcome(output.status(), output.stdout()) == PushOutcome::Indeterminate {
                bail!(
                    "Could not acknowledge `git push` for GHerrit remote '{}'; remote refs may or may not have changed. Run GHerrit again to observe them before continuing.",
                    self.destination.configured_remote()
                );
            }
        }
        Ok(())
    }

    #[cfg(test)]
    pub(super) fn arguments_for_test(&self) -> Vec<(Vec<String>, Vec<String>)> {
        std::iter::once(&self.first)
            .chain(self.rest.iter())
            .map(|request| {
                (request.options().collect::<Vec<_>>(), request.refspecs().collect::<Vec<_>>())
            })
            .collect()
    }

    #[cfg(test)]
    async fn publish_with_timeout_for_test(self, timeout: std::time::Duration) -> Result<()> {
        self.publish_with_timeout(timeout).await
    }
}

/// Every Git publication decision and ready push batch for one local stack.
#[derive(Debug)]
pub(super) struct GitPublicationPlan<'stack, 'destination> {
    pushes: Option<PreparedPushes<'destination>>,
    changes: PlannedChanges<'stack>,
}

impl<'stack> GitPublicationPlan<'stack, '_> {
    /// Releases planned changes only after every required push is acknowledged.
    pub(super) async fn publish(self) -> Result<PlannedChanges<'stack>> {
        if let Some(pushes) = self.pushes {
            pushes.publish().await?;
        }
        Ok(self.changes)
    }

    #[cfg(test)]
    async fn publish_with_timeout_for_test(
        self,
        timeout: std::time::Duration,
    ) -> Result<PlannedChanges<'stack>> {
        if let Some(pushes) = self.pushes {
            pushes.publish_with_timeout_for_test(timeout).await?;
        }
        Ok(self.changes)
    }
}

/// One publication outcome for every local change, in local stack order.
#[derive(Debug)]
pub(super) struct PlannedChanges<'stack>(Vec<VersionedChange<'stack>>);

impl<'stack> IntoIterator for PlannedChanges<'stack> {
    type Item = VersionedChange<'stack>;
    type IntoIter = std::vec::IntoIter<Self::Item>;

    fn into_iter(self) -> Self::IntoIter {
        self.0.into_iter()
    }
}

impl PlannedChanges<'_> {
    #[cfg(test)]
    fn version(&self, id: &str) -> Option<Version> {
        self.0
            .iter()
            .find(|change| change.change().id().as_str() == id)
            .map(|change| change.version())
    }

    #[cfg(test)]
    fn len(&self) -> usize {
        self.0.len()
    }

    #[cfg(test)]
    fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

#[derive(Clone, Copy, Debug)]
pub(super) struct VersionedChange<'a> {
    change: &'a LocalChange,
    version: Version,
}

impl<'a> VersionedChange<'a> {
    pub(super) fn change(self) -> &'a LocalChange {
        self.change
    }

    pub(super) fn version(self) -> Version {
        self.version
    }
}

pub(super) fn plan_git_publication<'stack, 'destination>(
    observed: &ObservedStack<'stack, 'destination>,
) -> Result<GitPublicationPlan<'stack, 'destination>> {
    // Collect every result before constructing the plan. In particular, an
    // invalid change late in a large stack cannot be discovered after an
    // earlier push batch has already committed.
    let planned = observed
        .iter()
        .map(|observed| plan_change(observed).map(|publication| (observed.change(), publication)))
        .collect::<Result<Vec<_>>>()?;
    let requests =
        plan_push_batches(planned.iter().filter_map(|(_, publication)| publication.target()))?
            .into_iter()
            .map(PushPlan::into_request)
            .collect::<Result<Vec<_>>>()?;
    let pushes = PreparedPushes::new(observed.destination(), requests)?;
    let changes = planned
        .into_iter()
        .map(|(change, publication)| VersionedChange { change, version: publication.version() })
        .collect();
    Ok(GitPublicationPlan { pushes, changes: PlannedChanges(changes) })
}

#[derive(Debug)]
enum PlannedChange {
    Current(Version),
    Publish(PushTarget),
}

impl PlannedChange {
    fn version(&self) -> Version {
        match self {
            Self::Current(version) => *version,
            Self::Publish(target) => target.version(),
        }
    }

    fn target(&self) -> Option<&PushTarget> {
        match self {
            Self::Current(_) => None,
            Self::Publish(target) => Some(target),
        }
    }
}

fn plan_change(observed: &ObservedChange<'_>) -> Result<PlannedChange> {
    let change = observed.change();
    let id = change.id();
    let desired_head = change.head();
    match normalize_remote_publication(
        id,
        observed.head(),
        observed.owned_base(),
        observed.versions(),
    )? {
        RemotePublication::Absent => {
            Ok(PlannedChange::Publish(PushTarget::first(id.clone(), desired_head)))
        }
        RemotePublication::Published { current_head, latest_version } => {
            if current_head == desired_head {
                // This is a true no-op, not a concurrency guard. Git elides
                // an up-to-date refspec without sending an update, so adding
                // an exact lease would not make it a compare-and-swap. A
                // later read would only narrow, not close, the race before
                // unconditioned GitHub mutations. The publication protocol
                // therefore assumes one publisher at a time.
                Ok(PlannedChange::Current(latest_version))
            } else {
                Ok(PlannedChange::Publish(PushTarget::advance(
                    id.clone(),
                    desired_head,
                    current_head,
                    latest_version,
                )?))
            }
        }
    }
}

#[derive(Debug)]
enum RemotePublication {
    Absent,
    Published { current_head: ObjectId, latest_version: Version },
}

fn normalize_remote_publication(
    id: &GherritPrId,
    head: Option<ObjectId>,
    owned_base: Option<ObjectId>,
    tags: &std::collections::BTreeMap<Version, ObjectId>,
) -> Result<RemotePublication> {
    if owned_base.is_some() {
        bail!(
            "Remote GHerrit change '{}' has an owned base from the new publication representation; this client cannot publish mixed representations",
            id.as_str()
        );
    }
    match (head, tags.is_empty()) {
        (None, true) => return Ok(RemotePublication::Absent),
        (Some(_), true) => {
            bail!("Remote GHerrit change '{}' has a managed head but no version tags", id.as_str())
        }
        (None, false) => {
            bail!("Remote GHerrit change '{}' has version tags but no managed head", id.as_str())
        }
        (Some(_), false) => {}
    }

    tags.iter().enumerate().try_for_each(|(index, (actual, _))| {
        let expected = Version::from_history_index(index)
            .ok_or_else(|| {
                eyre!("Remote GHerrit change '{}' has too many versions", id.as_str())
            })?;
        if *actual != expected {
            bail!(
                "Remote GHerrit change '{}' has noncontiguous version tags: expected v{expected}, observed v{actual}",
                id.as_str()
            );
        }
        Ok(())
    })?;
    let (&latest_version, &current_head) = tags
        .last_key_value()
        .ok_or_else(|| eyre!("Remote GHerrit change '{}' has no version records", id.as_str()))?;
    if head != Some(current_head) {
        bail!("Remote GHerrit change '{}' head does not match its latest version tag", id.as_str());
    }
    Ok(RemotePublication::Published { current_head, latest_version })
}

#[derive(Debug)]
struct PushTupleArguments {
    options: [String; 2],
    refspecs: [String; 2],
    expected_receipts: [(String, ExpectedRefReceipt); 2],
}

/// One exact rendered change tuple which has passed the variable-argument
/// budget used to construct its push batch.
#[derive(Debug)]
struct BudgetedPushTuple {
    arguments: PushTupleArguments,
    encoded_argv_bytes: usize,
}

impl BudgetedPushTuple {
    fn new(target_index: usize, target: &PushTarget, budget: usize) -> Result<Self> {
        let arguments = PushTupleArguments::new(target);
        let encoded_argv_bytes = arguments.encoded_argv_bytes();
        if encoded_argv_bytes > budget {
            bail!(
                "Git publication target {target_index} has a {}-byte change ID and requires {encoded_argv_bytes} bytes of variable push arguments, which exceeds the {budget}-byte variable-argument budget",
                target.id().as_str().len()
            );
        }
        Ok(Self { arguments, encoded_argv_bytes })
    }
}

impl PushTupleArguments {
    fn new(target: &PushTarget) -> Self {
        let branch = format!("refs/heads/{}", target.id().as_str());
        let tag = format!("refs/tags/gherrit/{}/v{}", target.id().as_str(), target.version());
        let expected = target.expected_head().map(|object| object.to_string()).unwrap_or_default();
        // Branch updates are leased against the observed remote value. A tag
        // lease with an empty expected value requires that the version tag not
        // exist, making it a lock rather than an overwrite.
        let options = [
            format!("--force-with-lease={branch}:{expected}"),
            format!("--force-with-lease={tag}:"),
        ];
        let desired_head = target.desired_head();
        let refspecs = [format!("{desired_head}:{branch}"), format!("{desired_head}:{tag}")];
        let branch_transition = if target.expected_head().is_some() {
            ExpectedRefTransition::UpdateOrAlreadyDesired
        } else {
            ExpectedRefTransition::CreateOrAlreadyDesired
        };
        let expected_receipts = [
            (branch, ExpectedRefReceipt::new(desired_head.to_string(), branch_transition)),
            (
                tag,
                ExpectedRefReceipt::new(
                    desired_head.to_string(),
                    ExpectedRefTransition::CreateOrAlreadyDesired,
                ),
            ),
        ];
        Self { options, refspecs, expected_receipts }
    }

    fn encoded_argv_bytes(&self) -> usize {
        self.options.iter().chain(&self.refspecs).map(|argument| argument.len() + 1).sum()
    }
}

fn plan_push_batches<'a>(
    targets: impl IntoIterator<Item = &'a PushTarget>,
) -> Result<Vec<PushPlan>> {
    plan_push_batches_with_budget(targets, PUSH_VARIABLE_ARGV_BUDGET_BYTES)
}

fn plan_push_batches_with_budget<'a>(
    targets: impl IntoIterator<Item = &'a PushTarget>,
    budget: usize,
) -> Result<Vec<PushPlan>> {
    // Render and size every per-change tuple before constructing the first
    // batch. A late oversized target therefore rejects the complete
    // publication plan; no prefix can escape to the push adapter.
    let tuples = targets
        .into_iter()
        .enumerate()
        .map(|(index, target)| BudgetedPushTuple::new(index, target, budget))
        .collect::<Result<Vec<_>>>()?;
    // Validate destinations across the complete plan before constructing its
    // first executable batch. A repeated destination in different batches is
    // just as invalid as a repeat within one atomic push.
    ExpectedReceipts::new(
        tuples.iter().flat_map(|tuple| tuple.arguments.expected_receipts.iter().cloned()),
    )?;

    let mut batches = Vec::new();
    let mut current = None::<PushPlan>;
    let mut current_bytes = 0;
    for tuple in tuples {
        let tuple_bytes = tuple.encoded_argv_bytes;
        if current.is_some() && current_bytes > budget - tuple_bytes {
            batches.push(current.take().expect("a full push batch exists"));
            current_bytes = 0;
        }
        current_bytes += tuple_bytes;
        match &mut current {
            Some(batch) => batch.push(tuple),
            None => current = Some(PushPlan::new(tuple)),
        }
    }
    if let Some(current) = current {
        batches.push(current);
    }

    Ok(batches)
}

#[cfg(test)]
mod tests {
    use std::{collections::BTreeMap, fmt::Write as _};
    #[cfg(unix)]
    use std::{
        fs,
        os::unix::fs::PermissionsExt as _,
        path::PathBuf,
        time::{Duration, Instant},
    };

    use tempfile::TempDir;

    use super::*;
    use crate::{
        pre_push::{
            history::{CommitGraphEvidence, NormalizedPublishedHistory, ValidatedChangeHistory},
            local::LocalStack,
            remote::{self, ObservedStack},
        },
        util,
    };

    fn object_id(byte: u8) -> ObjectId {
        ObjectId::from_bytes_or_panic(&[byte; 20])
    }

    fn version(value: u64) -> Version {
        Version::new(value).expect("test version must be nonzero")
    }

    fn change_id(value: &str) -> GherritPrId {
        GherritPrId::from_ref_component(value.as_bytes()).expect("valid test change ID")
    }

    fn versions(values: &[(u64, u8)]) -> BTreeMap<Version, ObjectId> {
        values.iter().map(|(value, byte)| (version(*value), object_id(*byte))).collect()
    }

    fn stack(changes: impl IntoIterator<Item = (GherritPrId, ObjectId)>) -> LocalStack {
        LocalStack::for_test(object_id(0xff), changes)
    }

    fn observed<'stack>(
        stack: &'stack LocalStack,
        heads: &[(&str, u8)],
        tags: &[(&str, &[(u64, u8)])],
    ) -> ObservedStack<'stack, 'static> {
        ObservedStack::for_test(
            stack,
            stack.iter().map(|change| {
                let id = change.id().as_str();
                let head = heads
                    .iter()
                    .find_map(|(candidate, byte)| (*candidate == id).then(|| object_id(*byte)));
                let history = tags
                    .iter()
                    .find_map(|(candidate, values)| (*candidate == id).then(|| versions(values)))
                    .unwrap_or_default();
                (head, None, history)
            }),
        )
    }

    #[cfg(unix)]
    fn observed_at<'stack, 'destination>(
        destination: &'destination PushDestination,
        stack: &'stack LocalStack,
        heads: &[(&str, u8)],
        tags: &[(&str, &[(u64, u8)])],
    ) -> ObservedStack<'stack, 'destination> {
        ObservedStack::for_test_at(
            destination,
            stack,
            stack.iter().map(|change| {
                let id = change.id().as_str();
                let head = heads
                    .iter()
                    .find_map(|(candidate, byte)| (*candidate == id).then(|| object_id(*byte)));
                let history = tags
                    .iter()
                    .find_map(|(candidate, values)| (*candidate == id).then(|| versions(values)))
                    .unwrap_or_default();
                (head, None, history)
            }),
        )
    }

    #[cfg(unix)]
    fn fake_git_destination(script: &str) -> (TempDir, PushDestination, PathBuf) {
        let directory = tempfile::tempdir().unwrap();
        let executable = directory.path().join("git");
        let argument_log = directory.path().join("arguments");
        fs::write(&executable, script).unwrap();
        let mut permissions = fs::metadata(&executable).unwrap().permissions();
        permissions.set_mode(0o700);
        fs::set_permissions(&executable, permissions).unwrap();
        let environment = vec![
            (std::ffi::OsString::from("PATH"), directory.path().as_os_str().to_owned()),
            (std::ffi::OsString::from("GHERRIT_TEST_ARGV"), argument_log.as_os_str().to_owned()),
        ];
        let destination = PushDestination::for_test(
            "origin",
            "https://github.com/owner/repository.git",
            environment,
        )
        .unwrap();
        (directory, destination, argument_log)
    }

    #[cfg(unix)]
    const SUCCESSFUL_PUSH: &str = r#"#!/bin/sh
: > "$GHERRIT_TEST_ARGV"
for argument in "$@"; do
    printf '%s\n' "$argument" >> "$GHERRIT_TEST_ARGV"
done
printf 'To private-destination\n'
for argument in "$@"; do
    case "$argument" in
        *:refs/heads/*|*:refs/tags/*)
            printf '*\t%s\t[new reference]\n' "$argument"
            ;;
    esac
done
printf 'Done\n'
"#;

    #[cfg(unix)]
    const HANGING_PUSH: &str = r#"#!/bin/sh
: > "$GHERRIT_TEST_ARGV"
/bin/sleep 10
"#;

    #[cfg(unix)]
    const ACKNOWLEDGED_PREFIX: &str = r#"#!/bin/sh
count=0
if test -f "$GHERRIT_TEST_ARGV"; then
    read -r count < "$GHERRIT_TEST_ARGV"
fi
count=$((count + 1))
printf '%s\n' "$count" > "$GHERRIT_TEST_ARGV"
printf 'To private-destination\n'
if test "$count" -eq 1; then
    for argument in "$@"; do
        case "$argument" in
            *:refs/heads/*|*:refs/tags/*)
                printf '*\t%s\t[new reference]\n' "$argument"
                ;;
        esac
    done
fi
printf 'Done\n'
"#;

    fn push_target(
        id: &str,
        object_id: ObjectId,
        version: Version,
        expected_remote: Option<ObjectId>,
    ) -> PushTarget {
        match expected_remote {
            None => {
                assert_eq!(version, Version::FIRST);
                PushTarget::first(change_id(id), object_id)
            }
            Some(expected_head) => {
                let previous = Version::new(version.get() - 1).expect("advanced test version");
                PushTarget::advance(change_id(id), object_id, expected_head, previous).unwrap()
            }
        }
    }

    fn batch_tuple_count(batch: &PushPlan) -> usize {
        batch.tuples().count()
    }

    struct HistoryRepository {
        directory: TempDir,
        writer: gix::Repository,
    }

    impl HistoryRepository {
        fn new() -> Self {
            let directory = tempfile::tempdir().unwrap();
            let writer = gix::init_bare(directory.path()).unwrap();
            Self { directory, writer }
        }

        fn commit(&self, subject: &str, parents: &[ObjectId], id: Option<&str>) -> ObjectId {
            let message = id.map_or_else(
                || subject.to_owned(),
                |id| format!("{subject}\n\ngherrit-pr-id: {id}\n"),
            );
            let signature = gix::actor::Signature {
                name: "GHerrit test".into(),
                email: "test@example.com".into(),
                time: gix::actor::date::Time::new(0, 0),
            };
            self.writer
                .write_object(&gix::objs::Commit {
                    tree: ObjectId::empty_tree(self.writer.object_hash()),
                    parents: parents.iter().copied().collect(),
                    author: signature.clone(),
                    committer: signature,
                    encoding: None,
                    message: message.into(),
                    extra_headers: Vec::new(),
                })
                .unwrap()
                .detach()
        }

        fn graph(&self, roots: impl IntoIterator<Item = ObjectId>) -> CommitGraphEvidence {
            let repository = util::Repo::open(self.directory.path().to_str().unwrap()).unwrap();
            CommitGraphEvidence::load(&repository, roots).unwrap()
        }
    }

    struct ValidatedFixture {
        history: ValidatedChangeHistory,
        published: Option<(ObjectId, ObjectId)>,
        proposed: (ObjectId, ObjectId),
    }

    fn validated_history(id: &str, published: bool, advances: bool) -> ValidatedFixture {
        assert!(published || advances, "an absent history always has a proposal to publish");
        let repository = HistoryRepository::new();
        let published_revision = published.then(|| {
            let base = repository.commit(&format!("{id} published base"), &[], None);
            let head = repository.commit(&format!("{id} published"), &[base], Some(id));
            (head, base)
        });
        let proposed = if advances {
            let base = repository.commit(&format!("{id} proposed base"), &[], None);
            let head = repository.commit(&format!("{id} proposed"), &[base], Some(id));
            (head, base)
        } else {
            published_revision.expect("a current proposal must already be published")
        };
        let graph = repository.graph(
            published_revision.into_iter().map(|(head, _)| head).chain(std::iter::once(proposed.0)),
        );
        let id = change_id(id);
        let default = published_revision.map_or(proposed.1, |(_, base)| base);
        let mut heads =
            format!("ref: refs/heads/main\tHEAD\n{default}\tHEAD\n{default}\trefs/heads/main\n");
        let mut tags = String::new();
        if let Some((head, base)) = published_revision {
            writeln!(heads, "{head}\trefs/heads/{}", id.as_str()).unwrap();
            writeln!(heads, "{base}\trefs/heads/gherrit-bases/{}", id.as_str()).unwrap();
            writeln!(tags, "{head}\trefs/tags/gherrit/{}/v1", id.as_str()).unwrap();
        }
        let observed =
            remote::parse_active_change_for_test(id.clone(), heads.as_bytes(), tags.as_bytes())
                .unwrap();
        let normalized = NormalizedPublishedHistory::from_observation(observed, &graph).unwrap();
        let stack = LocalStack::for_test(proposed.1, [(id, proposed.0)]);
        let history = normalized
            .with_proposal(stack.iter().next().unwrap(), &graph)
            .unwrap()
            .validate(&graph, None)
            .unwrap();
        ValidatedFixture { history, published: published_revision, proposed }
    }

    #[test]
    fn owned_base_publication_plans_one_exact_three_ref_tuple() {
        let fixture = validated_history("Gone", true, true);
        let requests = plan_owned_base_requests(&[&fixture.history]).unwrap();
        let (published_head, published_base) = fixture.published.unwrap();
        let (proposed_head, proposed_base) = fixture.proposed;

        assert_eq!(requests.len(), 1);
        assert_eq!(
            requests[0].options,
            [
                "--porcelain".to_owned(),
                "--atomic".to_owned(),
                "--no-verify".to_owned(),
                "--no-follow-tags".to_owned(),
                "--recurse-submodules=no".to_owned(),
                "--no-signed".to_owned(),
                "--no-force-if-includes".to_owned(),
                format!("--force-with-lease=refs/heads/Gone:{published_head}"),
                format!("--force-with-lease=refs/heads/gherrit-bases/Gone:{published_base}"),
                "--force-with-lease=refs/tags/gherrit/Gone/v2:".to_owned(),
            ]
        );
        assert_eq!(
            requests[0].refspecs,
            [
                format!("{proposed_head}:refs/heads/Gone"),
                format!("{proposed_base}:refs/heads/gherrit-bases/Gone"),
                format!("{proposed_head}:refs/tags/gherrit/Gone/v2"),
            ]
        );
        assert_eq!(requests[0].expected.refs.len(), 3);
        assert_eq!(
            requests[0].expected.refs["refs/heads/Gone"].transition,
            ExpectedRefTransition::UpdateOrAlreadyDesired
        );
        assert_eq!(
            requests[0].expected.refs["refs/heads/gherrit-bases/Gone"].transition,
            ExpectedRefTransition::UpdateOrAlreadyDesired
        );
        assert_eq!(
            requests[0].expected.refs["refs/tags/gherrit/Gone/v2"].transition,
            ExpectedRefTransition::CreateOrAlreadyDesired
        );
    }

    #[test]
    fn marker_publication_uses_exact_absent_leases_and_indivisible_byte_batches() {
        let first = MissingPullRequestMarker { id: change_id("Gone"), target: object_id(0x11) };
        let second = MissingPullRequestMarker { id: change_id("Gtwo"), target: object_id(0x22) };
        let encoded = MarkerPushArguments::new(&first).encoded_argv_bytes();

        let exact =
            plan_marker_requests_with_budget(std::slice::from_ref(&first), encoded).unwrap();
        assert_eq!(exact.len(), 1);
        assert_eq!(
            exact[0].options,
            [
                "--porcelain".to_owned(),
                "--atomic".to_owned(),
                "--no-verify".to_owned(),
                "--no-follow-tags".to_owned(),
                "--recurse-submodules=no".to_owned(),
                "--no-signed".to_owned(),
                "--no-force-if-includes".to_owned(),
                "--force-with-lease=refs/tags/gherrit/Gone/pr:".to_owned(),
            ]
        );
        assert_eq!(exact[0].refspecs, [format!("{}:refs/tags/gherrit/Gone/pr", object_id(0x11))]);
        assert_eq!(
            exact[0].expected.refs["refs/tags/gherrit/Gone/pr"].transition,
            ExpectedRefTransition::CreateOrAlreadyDesired
        );
        assert!(
            plan_marker_requests_with_budget(std::slice::from_ref(&first), encoded - 1).is_err()
        );

        let batches = plan_marker_requests_with_budget(&[first.clone(), second], encoded).unwrap();
        assert_eq!(batches.len(), 2);
        assert!(batches.iter().all(|batch| batch.refspecs.len() == 1));
        assert!(plan_marker_requests_with_budget(&[first.clone(), first], usize::MAX).is_err());
    }

    #[test]
    fn marker_receipts_require_exact_source_destination_coverage_and_create_transition() {
        let marker = MissingPullRequestMarker { id: change_id("Gone"), target: object_id(0x11) };
        let mut requests =
            plan_marker_requests_with_budget(&[marker], usize::MAX).unwrap().into_vec();
        let request = requests.pop().unwrap();
        let source = object_id(0x11);
        for flag in ["*", "="] {
            let output =
                format!("To private\n{flag}\t{source}:refs/tags/gherrit/Gone/pr\tstatus\nDone\n");
            assert_eq!(
                classify_push_receipts(&request.expected, output.as_bytes()),
                PushOutcome::AcknowledgedSuccess
            );
        }
        for output in [
            format!("To private\n+\t{source}:refs/tags/gherrit/Gone/pr\tstatus\nDone\n"),
            format!("To private\n!\t{source}:refs/tags/gherrit/Gone/pr\trejected\nDone\n"),
            "To private\nDone\n".to_owned(),
            format!(
                "To private\n*\t{source}:refs/tags/gherrit/Gone/pr\tstatus\n*\t{source}:refs/tags/gherrit/Gextra/pr\tstatus\nDone\n"
            ),
            format!(
                "To private\n*\t{source}:refs/tags/gherrit/Gone/pr\tstatus\n=\t{source}:refs/tags/gherrit/Gone/pr\tstatus\nDone\n"
            ),
            format!("To private\n*\t{}:refs/tags/gherrit/Gone/pr\tstatus\nDone\n", object_id(0x22)),
            format!("To private\n* {source}:refs/tags/gherrit/Gone/pr status\nDone\n"),
        ] {
            assert_eq!(
                classify_push_receipts(&request.expected, output.as_bytes()),
                PushOutcome::Indeterminate,
                "output={output:?}"
            );
        }
    }

    #[test]
    fn owned_base_publication_uses_absence_leases_for_first_publication() {
        let fixture = validated_history("Gnew", false, true);
        let requests = plan_owned_base_requests(&[&fixture.history]).unwrap();

        assert_eq!(requests.len(), 1);
        assert_eq!(
            &requests[0].options[FIXED_PUSH_OPTIONS.len()..],
            [
                "--force-with-lease=refs/heads/Gnew:",
                "--force-with-lease=refs/heads/gherrit-bases/Gnew:",
                "--force-with-lease=refs/tags/gherrit/Gnew/v1:",
            ]
        );
        assert!(
            requests[0]
                .expected
                .refs
                .values()
                .all(|receipt| receipt.transition == ExpectedRefTransition::CreateOrAlreadyDesired)
        );
    }

    #[test]
    fn current_owned_base_history_is_a_git_no_op() {
        let fixture = validated_history("Gone", true, false);
        assert!(plan_owned_base_requests(&[&fixture.history]).unwrap().is_empty());

        let destination = PushDestination::for_test(
            "origin",
            "https://github.com/owner/repository.git",
            Vec::new(),
        )
        .unwrap();
        assert!(plan_owned_base_pushes(&destination, &[&fixture.history]).unwrap().is_none());
    }

    #[test]
    fn mixed_current_and_changed_histories_emit_only_the_changed_tuple() {
        let current = validated_history("Gone", true, false);
        let changed = validated_history("Gtwo", true, true);
        let requests = plan_owned_base_requests(&[&current.history, &changed.history]).unwrap();

        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].refspecs.len(), 3);
        assert!(requests[0].refspecs.iter().all(|refspec| refspec.contains("Gtwo")));
        assert!(requests[0].refspecs.iter().all(|refspec| !refspec.contains("Gone")));
    }

    #[test]
    fn owned_base_batching_never_splits_a_three_ref_tuple() {
        let first = validated_history("Gone", false, true);
        let second = validated_history("Gtwo", false, true);
        let first_bytes = OwnedPushTupleArguments::from_history(&first.history)
            .unwrap()
            .unwrap()
            .encoded_argv_bytes();
        let second_bytes = OwnedPushTupleArguments::from_history(&second.history)
            .unwrap()
            .unwrap()
            .encoded_argv_bytes();

        assert_eq!(
            plan_owned_base_requests_with_budget(
                &[&first.history, &second.history],
                first_bytes + second_bytes,
            )
            .unwrap()
            .len(),
            1
        );
        let split = plan_owned_base_requests_with_budget(
            &[&first.history, &second.history],
            first_bytes + second_bytes - 1,
        )
        .unwrap();
        assert_eq!(split.len(), 2);
        assert!(split.iter().all(|request| request.refspecs.len() == 3));
    }

    #[test]
    fn normalizes_only_absent_or_complete_contiguous_publications() {
        assert!(matches!(
            normalize_remote_publication(&change_id("Gone"), None, None, &versions(&[])).unwrap(),
            RemotePublication::Absent
        ));

        let repeated = versions(&[(1, 2), (2, 2), (3, 3)]);
        let RemotePublication::Published { current_head, latest_version } =
            normalize_remote_publication(&change_id("Gone"), Some(object_id(3)), None, &repeated)
                .unwrap()
        else {
            panic!("complete history must be published");
        };
        assert_eq!(current_head, object_id(3));
        assert_eq!(latest_version, version(3));

        for (head, tags, message) in [
            (Some(object_id(2)), versions(&[]), "head but no version tags"),
            (None, versions(&[(1, 2)]), "version tags but no managed head"),
            (Some(object_id(3)), versions(&[(1, 2), (3, 3)]), "noncontiguous version tags"),
            (
                Some(object_id(3)),
                versions(&[(1, 2), (2, 2)]),
                "does not match its latest version tag",
            ),
        ] {
            let error =
                normalize_remote_publication(&change_id("Gone"), head, None, &tags).unwrap_err();
            assert!(error.to_string().contains(message), "error={error:?}");
        }
    }

    #[test]
    fn rejects_an_owned_base_before_planning_any_push() {
        let stack = stack([(change_id("Gone"), object_id(2))]);
        let observed =
            ObservedStack::for_test(&stack, [(None, Some(object_id(1)), BTreeMap::new())]);

        let error = plan_git_publication(&observed).unwrap_err();
        assert!(error.to_string().contains("mixed representations"), "error={error:?}");
    }

    #[test]
    fn unchanged_heads_are_no_ops_and_changed_heads_advance_remote_history() {
        let stack = stack([
            (change_id("Gone"), object_id(3)),
            (change_id("Gtwo"), object_id(6)),
            (change_id("Gnew"), object_id(7)),
        ]);
        let observed = observed(
            &stack,
            &[("Gone", 3), ("Gtwo", 5)],
            &[("Gone", &[(1, 2), (2, 3)]), ("Gtwo", &[(1, 5)])],
        );
        let plan = plan_git_publication(&observed).unwrap();

        assert_eq!(plan.changes.version("Gone"), Some(version(2)));
        assert_eq!(plan.changes.version("Gtwo"), Some(version(2)));
        assert_eq!(plan.changes.version("Gnew"), Some(Version::FIRST));
        let batches = plan.pushes.as_ref().unwrap().arguments_for_test();
        assert_eq!(batches.len(), 1);
        let (options, refspecs) = &batches[0];
        assert_eq!(refspecs.len() / 2, 2);
        assert!(options.iter().all(|argument| !argument.contains("Gone")));
        assert!(refspecs.iter().all(|argument| !argument.contains("Gone")));
        assert!(options.contains(&format!("--force-with-lease=refs/heads/Gtwo:{}", object_id(5))));
        assert!(refspecs.contains(&format!("{}:refs/tags/gherrit/Gnew/v1", object_id(7))));
    }

    #[test]
    fn stacks_larger_than_the_removed_query_batch_are_planned_together() {
        let ids = (0..251).map(|index| change_id(&format!("G{index}"))).collect::<Vec<_>>();
        let stack = stack(ids.iter().cloned().map(|id| (id, object_id(2))));
        let observed = observed(&stack, &[], &[]);
        let plan = plan_git_publication(&observed).unwrap();

        assert_eq!(plan.changes.len(), 251);
        let batches = plan.pushes.as_ref().unwrap().arguments_for_test();
        assert_eq!(batches.iter().map(|(_, refspecs)| refspecs.len() / 2).sum::<usize>(), 251);
        assert!(
            batches
                .iter()
                .flat_map(|(_, refspecs)| refspecs)
                .filter(|refspec| refspec.contains("refs/tags/gherrit/"))
                .all(|refspec| refspec.ends_with("/v1"))
        );
    }

    #[test]
    fn every_local_change_is_validated_before_a_plan_exists() {
        let stack = stack([(change_id("Gvalid"), object_id(2)), (change_id("Gbad"), object_id(4))]);
        let observed = observed(&stack, &[("Gbad", 4)], &[("Gbad", &[(1, 3)])]);
        let error = plan_git_publication(&observed).unwrap_err();

        assert!(error.to_string().contains("latest version tag"), "error={error:?}");
    }

    #[test]
    fn an_empty_publication_has_no_push_batches() {
        let stack = stack(std::iter::empty());
        let observed = observed(&stack, &[], &[]);
        let plan = plan_git_publication(&observed).unwrap();

        assert!(plan.pushes.is_none());
        assert!(plan.changes.is_empty());
    }

    #[cfg(unix)]
    #[tokio::test(flavor = "current_thread")]
    async fn legacy_no_op_has_no_executable_action_and_releases_changes() {
        let (_directory, destination, argument_log) = fake_git_destination(SUCCESSFUL_PUSH);
        let stack = stack([(change_id("Gone"), object_id(3))]);
        let observed = observed_at(&destination, &stack, &[("Gone", 3)], &[("Gone", &[(1, 3)])]);
        let plan = plan_git_publication(&observed).unwrap();

        assert!(plan.pushes.is_none());
        let changes = plan.publish_with_timeout_for_test(Duration::from_millis(100)).await.unwrap();

        assert_eq!(changes.version("Gone"), Some(Version::FIRST));
        assert!(!argument_log.exists(), "a no-op plan must not start Git");
    }

    #[cfg(unix)]
    #[tokio::test(flavor = "current_thread")]
    async fn legacy_plan_executes_only_its_observed_destination_and_releases_exact_receipts() {
        let (_directory, destination, argument_log) = fake_git_destination(SUCCESSFUL_PUSH);
        let stack = stack([(change_id("Gone"), object_id(7))]);
        let observed = observed_at(&destination, &stack, &[], &[]);
        let plan = plan_git_publication(&observed).unwrap();
        let expected = plan.pushes.as_ref().unwrap().arguments_for_test();
        assert_eq!(expected.len(), 1);
        let expected_options = expected[0].0.clone();
        let expected_refspecs = expected[0].1.clone();

        let changes = plan.publish_with_timeout_for_test(Duration::from_secs(10)).await.unwrap();

        assert_eq!(changes.version("Gone"), Some(Version::FIRST));
        let arguments = fs::read_to_string(argument_log)
            .unwrap()
            .lines()
            .map(str::to_owned)
            .collect::<Vec<_>>();
        let push = arguments.iter().position(|argument| argument == "push").unwrap();
        let separator = arguments[push + 1..]
            .iter()
            .position(|argument| argument == "--")
            .map(|offset| push + 1 + offset)
            .unwrap();
        assert_eq!(&arguments[push + 1..separator], expected_options);
        assert_eq!(arguments[separator + 1], "gherrit-publication");
        assert_eq!(&arguments[separator + 2..], expected_refspecs);
    }

    #[cfg(unix)]
    #[tokio::test(flavor = "current_thread")]
    async fn legacy_hung_push_uses_the_bounded_executor_and_releases_nothing() {
        let (_directory, destination, argument_log) = fake_git_destination(HANGING_PUSH);
        let stack = stack([(change_id("Gone"), object_id(7))]);
        let observed = observed_at(&destination, &stack, &[], &[]);
        let plan = plan_git_publication(&observed).unwrap();

        let started = Instant::now();
        let error = plan.publish_with_timeout_for_test(Duration::from_secs(5)).await.unwrap_err();

        assert!(error.to_string().contains("timed out"), "error={error:?}");
        assert!(argument_log.exists(), "the hanging fake Git process must have started");
        assert!(started.elapsed() < Duration::from_secs(12));
    }

    #[cfg(unix)]
    #[tokio::test(flavor = "current_thread")]
    async fn acknowledged_legacy_prefix_then_indeterminate_batch_releases_nothing() {
        let (_directory, destination, invocation_count) = fake_git_destination(ACKNOWLEDGED_PREFIX);
        let first = change_id(&format!("G{}", "a".repeat(2_000)));
        let second = change_id(&format!("G{}", "b".repeat(2_000)));
        let stack = stack([(first, object_id(2)), (second, object_id(3))]);
        let observed = observed_at(&destination, &stack, &[], &[]);
        let plan = plan_git_publication(&observed).unwrap();
        assert_eq!(plan.pushes.as_ref().unwrap().arguments_for_test().len(), 2);

        let error = plan.publish_with_timeout_for_test(Duration::from_secs(10)).await.unwrap_err();

        assert!(error.to_string().contains("Could not acknowledge `git push`"), "error={error:?}");
        assert_eq!(fs::read_to_string(invocation_count).unwrap().trim(), "2");
    }

    #[cfg(unix)]
    #[tokio::test(flavor = "current_thread")]
    async fn acknowledged_marker_prefix_then_indeterminate_batch_is_not_success() {
        let (_directory, destination, invocation_count) = fake_git_destination(ACKNOWLEDGED_PREFIX);
        let markers = [
            MissingPullRequestMarker {
                id: change_id(&format!("G{}", "a".repeat(5_000))),
                target: object_id(2),
            },
            MissingPullRequestMarker {
                id: change_id(&format!("G{}", "b".repeat(5_000))),
                target: object_id(3),
            },
        ];
        let pushes = plan_marker_pushes(&destination, &markers).unwrap().unwrap();
        assert_eq!(pushes.arguments_for_test().len(), 2);

        let error =
            pushes.publish_with_timeout_for_test(Duration::from_secs(10)).await.unwrap_err();

        assert!(error.to_string().contains("Could not acknowledge `git push`"));
        assert_eq!(fs::read_to_string(invocation_count).unwrap().trim(), "2");
    }

    #[test]
    fn push_batch_planning_accepts_the_exact_encoded_argv_boundary() {
        let target = push_target("Gone", object_id(2), version(2), Some(object_id(1)));
        let exact_bytes = PushTupleArguments::new(&target).encoded_argv_bytes();

        let exact = plan_push_batches_with_budget([&target], exact_bytes).unwrap();
        assert_eq!(exact.len(), 1);
        assert_eq!(batch_tuple_count(&exact[0]), 1);

        let error = plan_push_batches_with_budget([&target], exact_bytes - 1).unwrap_err();
        assert!(error.to_string().contains(&format!("requires {exact_bytes} bytes")));
        assert!(
            error
                .to_string()
                .contains(&format!("{}-byte variable-argument budget", exact_bytes - 1))
        );
    }

    #[test]
    fn push_batch_planning_splits_just_before_the_byte_boundary() {
        let first = push_target("Gone", object_id(2), version(2), Some(object_id(1)));
        let second = push_target("Gtwo", object_id(3), Version::FIRST, None);
        let first_bytes = PushTupleArguments::new(&first).encoded_argv_bytes();
        let second_bytes = PushTupleArguments::new(&second).encoded_argv_bytes();
        let combined_bytes = first_bytes + second_bytes;

        assert_eq!(
            plan_push_batches_with_budget([&first, &second], combined_bytes)
                .unwrap()
                .iter()
                .map(batch_tuple_count)
                .collect::<Vec<_>>(),
            [2]
        );
        assert_eq!(
            plan_push_batches_with_budget([&first, &second], combined_bytes - 1)
                .unwrap()
                .iter()
                .map(batch_tuple_count)
                .collect::<Vec<_>>(),
            [1, 1]
        );
    }

    #[test]
    fn a_late_oversized_target_rejects_the_complete_push_plan() {
        let first = push_target("Gone", object_id(2), Version::FIRST, None);
        let oversized =
            push_target(&format!("G{}", "x".repeat(100)), object_id(3), Version::FIRST, None);
        let budget = PushTupleArguments::new(&first).encoded_argv_bytes();

        let error = plan_push_batches_with_budget([&first, &oversized], budget)
            .expect_err("a later oversized tuple must reject rather than return a prefix");
        assert!(error.to_string().contains("target 1"), "error={error:?}");
    }

    #[test]
    fn long_ids_split_by_rendered_bytes_instead_of_target_count() {
        let first_id = format!("G{}", "a".repeat(2_000));
        let second_id = format!("G{}", "b".repeat(2_000));
        let first = push_target(&first_id, object_id(2), Version::FIRST, None);
        let second = push_target(&second_id, object_id(3), version(2), Some(object_id(2)));
        let batches = plan_push_batches([&first, &second]).unwrap();

        assert_eq!(batches.iter().map(batch_tuple_count).collect::<Vec<_>>(), [1, 1]);
        assert!(batches.iter().all(|batch| {
            let (options, refspecs) = batch.arguments();
            options
                .iter()
                .skip(FIXED_PUSH_OPTIONS.len())
                .chain(&refspecs)
                .map(|argument| argument.len() + 1)
                .sum::<usize>()
                <= PUSH_VARIABLE_ARGV_BUDGET_BYTES
        }));
    }

    #[test]
    fn branch_and_tag_arguments_are_never_split_between_batches() {
        let ids = [
            format!("G{}", "a".repeat(2_000)),
            format!("G{}", "b".repeat(2_000)),
            "Gshort".to_owned(),
        ];
        let targets = ids
            .iter()
            .enumerate()
            .map(|(index, id)| push_target(id, object_id(index as u8 + 2), Version::FIRST, None))
            .collect::<Vec<_>>();
        let batches = plan_push_batches(&targets).unwrap();

        for target in &targets {
            let tuple = PushTupleArguments::new(target);
            let memberships = batches
                .iter()
                .map(|batch| {
                    let (options, refspecs) = batch.arguments();
                    let option_count =
                        tuple.options.iter().filter(|item| options.contains(item)).count();
                    let refspec_count =
                        tuple.refspecs.iter().filter(|item| refspecs.contains(item)).count();
                    assert!(
                        (option_count == 0 && refspec_count == 0)
                            || (option_count == 2 && refspec_count == 2),
                        "a change tuple was split across push batches"
                    );
                    usize::from(option_count == 2)
                })
                .sum::<usize>();
            assert_eq!(memberships, 1, "each change tuple must appear in exactly one batch");
        }
    }

    #[test]
    fn plans_atomic_branch_and_tag_leases() {
        let targets = [
            push_target("Gone", object_id(0x11), version(2), Some(object_id(0x33))),
            push_target("Gtwo", object_id(0x22), Version::FIRST, None),
        ];
        let mut plans = plan_push_batches(&targets).unwrap();
        assert_eq!(plans.len(), 1);
        let plan = plans.pop().unwrap();
        let (options, refspecs) = plan.into_arguments();

        assert_eq!(
            options,
            [
                "--porcelain".to_string(),
                "--atomic".to_string(),
                "--no-verify".to_string(),
                "--no-follow-tags".to_string(),
                "--recurse-submodules=no".to_string(),
                "--no-signed".to_string(),
                "--no-force-if-includes".to_string(),
                format!("--force-with-lease=refs/heads/Gone:{}", object_id(0x33)),
                "--force-with-lease=refs/tags/gherrit/Gone/v2:".to_string(),
                "--force-with-lease=refs/heads/Gtwo:".to_string(),
                "--force-with-lease=refs/tags/gherrit/Gtwo/v1:".to_string(),
            ]
        );
        assert_eq!(
            refspecs,
            [
                format!("{}:refs/heads/Gone", object_id(0x11)),
                format!("{}:refs/tags/gherrit/Gone/v2", object_id(0x11)),
                format!("{}:refs/heads/Gtwo", object_id(0x22)),
                format!("{}:refs/tags/gherrit/Gtwo/v1", object_id(0x22)),
            ]
        );
    }

    fn expected_receipts(receipts: &[(&str, ExpectedRefTransition)]) -> ExpectedReceipts {
        ExpectedReceipts::new(receipts.iter().map(|(destination, transition)| {
            ((*destination).to_owned(), ExpectedRefReceipt::new("object".to_owned(), *transition))
        }))
        .unwrap()
    }

    #[test]
    fn push_receipts_accept_every_reachable_success_flag_and_line_ending() {
        let expected = expected_receipts(&[
            ("refs/heads/fast-forward", ExpectedRefTransition::UpdateOrAlreadyDesired),
            ("refs/heads/forced", ExpectedRefTransition::UpdateOrAlreadyDesired),
            ("refs/heads/new", ExpectedRefTransition::CreateOrAlreadyDesired),
            ("refs/heads/unchanged", ExpectedRefTransition::CreateOrAlreadyDesired),
        ]);
        let success = concat!(
            "To private destination\n",
            "=\tobject:refs/heads/unchanged\t[up to date]\n",
            "*\tobject:refs/heads/new\t[new branch]\n",
            " \tobject:refs/heads/fast-forward\told..new\n",
            "+\tobject:refs/heads/forced\told...new (forced update)\n",
            "Done\n",
        );
        for output in [success.to_owned(), success.replace('\n', "\r\n")] {
            assert_eq!(
                classify_push_receipts(&expected, output.as_bytes()),
                PushOutcome::AcknowledgedSuccess
            );
        }
    }

    #[test]
    fn push_receipts_enforce_transition_source_and_remote_failure() {
        for (transition, accepted) in [
            (ExpectedRefTransition::CreateOrAlreadyDesired, ["*", "="]),
            (ExpectedRefTransition::UpdateOrAlreadyDesired, [" ", "+"]),
        ] {
            let expected = expected_receipts(&[("refs/heads/Gone", transition)]);
            for flag in [" ", "+", "*", "=", "!"] {
                for source in ["object", "wrong-object"] {
                    let receipt =
                        format!("To private\n{flag}\t{source}:refs/heads/Gone\tstatus\nDone\n");
                    let outcome = if source == "object" && (accepted.contains(&flag) || flag == "=")
                    {
                        PushOutcome::AcknowledgedSuccess
                    } else {
                        PushOutcome::Indeterminate
                    };
                    assert_eq!(classify_push_receipts(&expected, receipt.as_bytes()), outcome);
                }
            }
        }
    }

    #[test]
    fn push_receipts_reject_bad_framing_records_and_coverage() {
        let expected = expected_receipts(&[
            ("refs/heads/Gone", ExpectedRefTransition::CreateOrAlreadyDesired),
            ("refs/tags/gherrit/Gone/v1", ExpectedRefTransition::CreateOrAlreadyDesired),
        ]);
        for output in [
            "",
            "To private\n*\tobject:refs/heads/Gone\t[new branch]\nDone",
            "To \n*\tobject:refs/heads/Gone\t[new branch]\nDone\n",
            "To private\n*\tobject:refs/heads/Gone\t[new branch]\nComplete\n",
            "To private\n*\tobject:refs/heads/Gone\t[new branch]\nDone\n\n",
            "To private\r\n*\tobject:refs/heads/Gone\t[new branch]\nDone\r\n",
            "To private\n*\tobject:refs/heads/Gone\t[new branch]\n=\tobject:refs/heads/Gone\t[up to date]\nDone\n",
            "To private\n*\tobject:refs/heads/Gone\t[new branch]\n*\tobject:refs/tags/gherrit/Gone/v2\t[new tag]\nDone\n",
            "To private\n* object:refs/heads/Gone [new branch]\nDone\n",
            "To private\n?\tobject:refs/heads/Gone\tstatus\nDone\n",
        ] {
            assert_eq!(
                classify_push_receipts(&expected, output.as_bytes()),
                PushOutcome::Indeterminate
            );
        }
        assert_eq!(
            classify_push_receipts(&expected, b"To private\n\xff\nDone\n"),
            PushOutcome::Indeterminate
        );
    }

    #[cfg(unix)]
    #[test]
    fn only_a_zero_exit_status_can_acknowledge_on_unix() {
        use std::os::unix::process::ExitStatusExt as _;
        let target = push_target("Gone", object_id(2), Version::FIRST, None);
        let request = plan_push_batches([&target]).unwrap().pop().unwrap().into_request().unwrap();
        let receipt = format!(
            "To private\n*\t{}:refs/heads/Gone\t[new branch]\n*\t{}:refs/tags/gherrit/Gone/v1\t[new tag]\nDone\n",
            object_id(2),
            object_id(2)
        );
        assert_eq!(
            request.outcome(&std::process::ExitStatus::from_raw(0), receipt.as_bytes()),
            PushOutcome::AcknowledgedSuccess
        );
        assert_eq!(
            request.outcome(&std::process::ExitStatus::from_raw(1 << 8), receipt.as_bytes()),
            PushOutcome::Indeterminate
        );
        assert_eq!(
            request.outcome(&std::process::ExitStatus::from_raw(9), receipt.as_bytes()),
            PushOutcome::Indeterminate
        );
    }

    #[test]
    fn duplicate_planned_destinations_are_rejected_before_batching() {
        let target = push_target("Gone", object_id(2), Version::FIRST, None);
        let error = plan_push_batches([&target, &target]).unwrap_err();
        assert!(error.to_string().contains("refs/heads/Gone"));
    }

    #[test]
    fn prepared_pushes_revalidate_duplicate_destinations_across_batches() {
        let destination = PushDestination::for_test(
            "origin",
            "https://github.com/owner/repository.git",
            Vec::new(),
        )
        .unwrap();
        let target = push_target("Gone", object_id(2), Version::FIRST, None);
        let first = plan_push_batches([&target]).unwrap().pop().unwrap().into_request().unwrap();
        let second = plan_push_batches([&target]).unwrap().pop().unwrap().into_request().unwrap();

        let error = PreparedPushes::new(&destination, [first, second]).unwrap_err();

        assert!(error.to_string().contains("refs/heads/Gone"));
    }
}

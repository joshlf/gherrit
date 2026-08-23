//! Owned-base and pull-request-marker publication.
//!
//! Every push is fully planned, destination-bound, atomically tupled, and
//! receipt-checked before a later publication stage can be released.

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
    plan::{AuthorizedMarkerPushes, AuthorizedTuplePushes, MarkerTarget},
    subprocess,
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
    #[cfg(test)]
    effect: super::test_effect::TupleEffect,
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
        #[cfg(test)]
        let effect = super::test_effect::TupleEffect {
            id: history.id().clone(),
            previous: current.map(|current| super::test_effect::RevisionEffect {
                head: current.revision().head(),
                first_parent: current.revision().first_parent(),
            }),
            desired: super::test_effect::RevisionEffect {
                head: proposed.head(),
                first_parent: proposed.first_parent(),
            },
            version: history.projected_current().number().get(),
        };
        Ok(Some(Self {
            options,
            refspecs,
            expected_receipts,
            #[cfg(test)]
            effect,
        }))
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
pub(super) fn preflight_tuple_pushes<'destination>(
    destination: &'destination PushDestination,
    histories: &[&ValidatedChangeHistory],
) -> Result<Option<TuplePushPreflight<'destination>>> {
    let requests =
        plan_owned_base_requests_with_budget(histories, PUSH_VARIABLE_ARGV_BUDGET_BYTES)?;
    PushSequence::new(destination, requests)
        .map(|pushes| pushes.map(|sequence| TuplePushPreflight { sequence }))
}

struct MarkerPushArguments {
    option: String,
    refspec: String,
    expected_receipt: (String, ExpectedRefReceipt),
    #[cfg(test)]
    effect: super::test_effect::MarkerEffect,
}

impl MarkerPushArguments {
    fn new(marker: &MarkerTarget) -> Self {
        let destination = format!("refs/tags/gherrit/{}/pr", marker.id().as_str());
        let source = marker.target().to_string();
        Self {
            option: format!("--force-with-lease={destination}:"),
            refspec: format!("{source}:{destination}"),
            expected_receipt: (
                destination,
                ExpectedRefReceipt::new(source, ExpectedRefTransition::CreateOrAlreadyDesired),
            ),
            #[cfg(test)]
            effect: super::test_effect::MarkerEffect {
                id: marker.id().clone(),
                target: marker.target(),
            },
        }
    }

    fn encoded_argv_bytes(&self) -> usize {
        self.option.len() + self.refspec.len() + 2
    }
}

/// Plans one absent-leased durable marker per unmarked local pull request.
pub(super) fn preflight_marker_pushes<'destination>(
    destination: &'destination PushDestination,
    markers: &[MarkerTarget],
) -> Result<Option<MarkerPushPreflight<'destination>>> {
    let requests = plan_marker_requests_with_budget(markers, PUSH_VARIABLE_ARGV_BUDGET_BYTES)?;
    let target_keys =
        markers.iter().map(|marker| (marker.id().clone(), marker.target())).collect::<Box<[_]>>();
    PushSequence::new(destination, requests)
        .map(|pushes| pushes.map(|sequence| MarkerPushPreflight { sequence, target_keys }))
}

fn plan_marker_requests_with_budget(
    markers: &[MarkerTarget],
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
    #[cfg(test)]
    let mut effects = Vec::with_capacity(arguments.len());
    for arguments in arguments {
        #[cfg(test)]
        effects.push(super::test_effect::GitEffect::Marker(arguments.effect.clone()));
        options.push(arguments.option);
        refspecs.push(arguments.refspec);
        receipts.push(arguments.expected_receipt);
    }
    Ok(PushRequest {
        options,
        refspecs,
        expected: ExpectedReceipts::new(receipts)?,
        #[cfg(test)]
        effects,
    })
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
            #[cfg(test)]
            let mut effects = Vec::with_capacity(batch.len());
            for tuple in batch {
                #[cfg(test)]
                effects.push(super::test_effect::GitEffect::Tuple(tuple.arguments.effect.clone()));
                options.extend(tuple.arguments.options);
                refspecs.extend(tuple.arguments.refspecs);
                receipts.extend(tuple.arguments.expected_receipts);
            }
            Ok(PushRequest {
                options,
                refspecs,
                expected: ExpectedReceipts::new(receipts)?,
                #[cfg(test)]
                effects,
            })
        })
        .collect::<Result<Box<[_]>>>()
}

#[cfg(test)]
fn plan_owned_base_requests(histories: &[&ValidatedChangeHistory]) -> Result<Box<[PushRequest]>> {
    plan_owned_base_requests_with_budget(histories, PUSH_VARIABLE_ARGV_BUDGET_BYTES)
}

/// The immutable boundary between publication planning and a `git push`
/// invocation. Receipt expectations are rendered while the plan is built,
/// rather than reconstructed from later mutable state or process output.
#[derive(Debug)]
struct PushRequest {
    options: Vec<String>,
    refspecs: Vec<String>,
    expected: ExpectedReceipts,
    #[cfg(test)]
    effects: Vec<super::test_effect::GitEffect>,
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
/// Both owned-tuple and marker publication use the same finite remote-command
/// deadline and exact receipt policy.
struct PushSequence<'destination> {
    destination: &'destination PushDestination,
    first: PushRequest,
    rest: Box<[PushRequest]>,
}

impl fmt::Debug for PushSequence<'_> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.debug_struct("PushSequence").field("batch_count", &(1 + self.rest.len())).finish()
    }
}

impl<'destination> PushSequence<'destination> {
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

    /// Returns semantic operations in their sequential push-request batches.
    #[cfg(test)]
    pub(super) fn effect_batches_for_test(
        &self,
    ) -> super::test_effect::EffectBatches<super::test_effect::GitEffect> {
        std::iter::once(&self.first)
            .chain(self.rest.iter())
            .map(|request| request.effects.clone().into_boxed_slice())
            .collect()
    }

    #[cfg(test)]
    async fn publish_with_timeout_for_test(self, timeout: std::time::Duration) -> Result<()> {
        self.publish_with_timeout(timeout).await
    }
}

/// Completely validated tuple wire data which is not executable on its own.
///
/// Only the planner can turn this value into [`AuthorizedTuplePushes`]. This
/// keeps request construction and its local failure modes before the first
/// write without treating validated bytes as publication authority.
pub(super) struct TuplePushPreflight<'destination> {
    sequence: PushSequence<'destination>,
}

/// Completely validated marker wire data which is not executable on its own.
///
/// A missing marker is not sufficient authority to publish it. The planner
/// converts this value into an executable stage only by consuming authority
/// from an already-validated OPEN pull request or exact create receipts.
pub(super) struct MarkerPushPreflight<'destination> {
    sequence: PushSequence<'destination>,
    target_keys: Box<[(super::local::GherritPrId, ObjectId)]>,
}

impl fmt::Debug for TuplePushPreflight<'_> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.sequence.fmt(formatter)
    }
}

impl fmt::Debug for MarkerPushPreflight<'_> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("MarkerPushPreflight")
            .field("pushes", &self.sequence)
            .field("target_count", &self.target_keys.len())
            .finish()
    }
}

impl MarkerPushPreflight<'_> {
    /// Checks the exact ordered evidence set whose wire data was preflighted.
    pub(super) fn matches_targets(&self, targets: &[MarkerTarget]) -> bool {
        self.target_keys
            .iter()
            .map(|(id, target)| (id, *target))
            .eq(targets.iter().map(|target| (target.id(), target.target())))
    }
}

#[cfg(test)]
impl TuplePushPreflight<'_> {
    pub(super) fn effect_batches_for_test(
        &self,
    ) -> super::test_effect::EffectBatches<super::test_effect::GitEffect> {
        self.sequence.effect_batches_for_test()
    }
}

#[cfg(test)]
impl MarkerPushPreflight<'_> {
    pub(super) fn arguments_for_test(&self) -> Vec<(Vec<String>, Vec<String>)> {
        self.sequence.arguments_for_test()
    }

    pub(super) fn effect_batches_for_test(
        &self,
    ) -> super::test_effect::EffectBatches<super::test_effect::GitEffect> {
        self.sequence.effect_batches_for_test()
    }

    async fn publish_with_timeout_for_test(self, timeout: std::time::Duration) -> Result<()> {
        self.sequence.publish_with_timeout_for_test(timeout).await
    }
}

/// Executes tuple wire data only after the planner has authorized the complete
/// publication plan.
pub(super) async fn publish_tuples(pushes: AuthorizedTuplePushes<'_>) -> Result<()> {
    pushes.into_preflight().sequence.publish().await
}

/// Executes marker wire data only after the planner has authorized every
/// marker in the complete stage.
pub(super) async fn publish_markers(pushes: AuthorizedMarkerPushes<'_>) -> Result<()> {
    pushes.into_preflight().sequence.publish().await
}

#[cfg(test)]
mod tests {
    use std::fmt::Write as _;
    #[cfg(unix)]
    use std::{fs, os::unix::fs::PermissionsExt as _, path::PathBuf, time::Duration};

    use tempfile::TempDir;

    use super::*;
    use crate::{
        pre_push::{
            destination::DefaultBranch,
            history::{CommitGraphEvidence, NormalizedPublishedHistory, ValidatedChangeHistory},
            local::{GherritPrId, LocalStack},
            remote,
        },
        util,
    };

    fn object_id(byte: u8) -> ObjectId {
        ObjectId::from_bytes_or_panic(&[byte; 20])
    }

    fn change_id(value: &str) -> GherritPrId {
        GherritPrId::from_ref_component(value.as_bytes()).expect("valid test change ID")
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
        let mut local = format!("{default}\trefs/heads/main\n");
        if let Some((head, base)) = published_revision {
            writeln!(local, "{head}\trefs/heads/{}", id.as_str()).unwrap();
            writeln!(local, "{base}\trefs/heads/gherrit-bases/{}", id.as_str()).unwrap();
            writeln!(local, "{head}\trefs/tags/gherrit/{}/v1", id.as_str()).unwrap();
        }
        let observed = remote::parse_active_change_for_test(
            id.clone(),
            DefaultBranch::new("main".to_owned(), default).unwrap(),
            local.as_bytes(),
        )
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
        let first = MarkerTarget::for_test(change_id("Gone"), object_id(0x11));
        let second = MarkerTarget::for_test(change_id("Gtwo"), object_id(0x22));
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
        let marker = MarkerTarget::for_test(change_id("Gone"), object_id(0x11));
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
        assert!(preflight_tuple_pushes(&destination, &[&fixture.history]).unwrap().is_none());
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

    #[cfg(unix)]
    #[tokio::test(flavor = "current_thread")]
    async fn acknowledged_marker_prefix_then_indeterminate_batch_is_not_success() {
        let (_directory, destination, invocation_count) = fake_git_destination(ACKNOWLEDGED_PREFIX);
        let markers = [
            MarkerTarget::for_test(change_id("Gone"), object_id(2)),
            MarkerTarget::for_test(change_id("Gtwo"), object_id(3)),
        ];
        let argument_bytes = markers
            .iter()
            .map(MarkerPushArguments::new)
            .map(|arguments| arguments.encoded_argv_bytes())
            .collect::<Vec<_>>();
        let requests =
            plan_marker_requests_with_budget(&markers, argument_bytes.iter().sum::<usize>() - 1)
                .unwrap();
        let sequence = PushSequence::new(&destination, requests).unwrap().unwrap();
        let target_keys =
            markers.iter().map(|marker| (marker.id().clone(), marker.target())).collect();
        let pushes = MarkerPushPreflight { sequence, target_keys };
        assert_eq!(pushes.arguments_for_test().len(), 2);

        let error =
            pushes.publish_with_timeout_for_test(Duration::from_secs(10)).await.unwrap_err();

        assert!(error.to_string().contains("Could not acknowledge `git push`"));
        assert_eq!(fs::read_to_string(invocation_count).unwrap().trim(), "2");
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
        let marker = MarkerTarget::for_test(change_id("Gone"), object_id(2));
        let request = plan_marker_requests_with_budget(&[marker], usize::MAX)
            .unwrap()
            .into_vec()
            .pop()
            .unwrap();
        let receipt =
            format!("To private\n*\t{}:refs/tags/gherrit/Gone/pr\t[new tag]\nDone\n", object_id(2));
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
    fn push_sequences_revalidate_duplicate_destinations_across_batches() {
        let destination = PushDestination::for_test(
            "origin",
            "https://github.com/owner/repository.git",
            Vec::new(),
        )
        .unwrap();
        let request = || {
            let marker = MarkerTarget::for_test(change_id("Gone"), object_id(2));
            plan_marker_requests_with_budget(&[marker], usize::MAX)
                .unwrap()
                .into_vec()
                .pop()
                .unwrap()
        };
        let first = request();
        let second = request();

        let error = PushSequence::new(&destination, [first, second]).unwrap_err();

        assert!(error.to_string().contains("refs/tags/gherrit/Gone/pr"));
    }
}

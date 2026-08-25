//! Exact Git ref transitions and their porcelain acknowledgement protocol.
//!
//! This module renders already-chosen publication transitions as immutable,
//! destination-bound actions and executes their bounded Git subprocesses. It
//! does not decide which local changes need publication. A complete, exact
//! porcelain acknowledgement is the only result which releases the caller.
//!
//! A change publication is one indivisible three-ref tuple: candidate head,
//! owned base, and a new immutable version tag. A pull-request marker is a
//! separate create-only transition. Every mutable ref carries the exact lease
//! implied by the transition, and every new tag carries an absence lease.
//! Batches remain atomic and are acknowledged only by one complete matching
//! `git push --porcelain` response from a normally exiting zero-status process.

use std::{
    collections::{BTreeMap, BTreeSet, HashSet},
    process::ExitStatus,
};

use color_eyre::eyre::{Context as _, Result, bail, eyre};
use gix::ObjectId;

use super::{
    destination::{PublicationTarget, PushDestination},
    local::GherritPrId,
    subprocess::{self, REMOTE_GIT_EXECUTION_TIMEOUT},
    version::Version,
};

// Hook recursion is an execution-boundary concern, not a reason to bypass
// repository hooks here. Exact leases make `--no-force-if-includes`
// unnecessary and avoid depending on the Git version which introduced it.
const FIXED_PUSH_OPTIONS: [&str; 5] =
    ["--porcelain", "--atomic", "--no-follow-tags", "--recurse-submodules=no", "--no-signed"];

// Windows command lines are limited to roughly 32 KiB. The variable arguments
// are ASCII, so byte lengths equal their UTF-16 code-unit lengths before
// quoting. Reserving half the limit leaves room for the executable, private
// remote configuration, fixed arguments, quoting, and the terminating NUL. It
// also bounds POSIX argument encoding conservatively.
const PUSH_VARIABLE_ARGV_BUDGET_BYTES: usize = 16 * 1024;

/// One non-null candidate-head and owned-base pair.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct PublicationRevision {
    head: ObjectId,
    owned_base: ObjectId,
}

impl PublicationRevision {
    pub(super) fn new(head: ObjectId, owned_base: ObjectId) -> Result<Self> {
        if head.is_null() || owned_base.is_null() {
            bail!("Git publication revisions require non-null object IDs");
        }
        if head == owned_base {
            bail!("A published commit cannot be its own owned base");
        }
        Ok(Self { head, owned_base })
    }

    pub(super) fn head(self) -> ObjectId {
        self.head
    }

    pub(super) fn owned_base(self) -> ObjectId {
        self.owned_base
    }
}

/// One complete change-tuple transition selected by a later semantic planner.
///
/// The variants mirror the only two transitions which Git publication can
/// receive. First publication always creates v1. Advancement always replaces
/// both mutable refs from one exact observed revision and creates the version
/// immediately following the observed latest version.
#[derive(Debug)]
pub(super) struct TupleTransition(TupleTransitionKind);

#[derive(Debug)]
enum TupleTransitionKind {
    Create {
        id: GherritPrId,
        desired: PublicationRevision,
    },
    Advance {
        id: GherritPrId,
        expected: PublicationRevision,
        desired: PublicationRevision,
        version: Version,
    },
}

impl TupleTransition {
    pub(super) fn create(id: GherritPrId, desired: PublicationRevision) -> Self {
        Self(TupleTransitionKind::Create { id, desired })
    }

    pub(super) fn advance(
        id: GherritPrId,
        expected: PublicationRevision,
        desired: PublicationRevision,
        latest_version: Version,
    ) -> Result<Self> {
        if expected.head == desired.head {
            bail!("Git publication cannot advance an already-current change revision");
        }
        let version = latest_version
            .next()
            .ok_or_else(|| eyre!("GHerrit change '{}' has no next version", id.as_str()))?;
        Ok(Self(TupleTransitionKind::Advance { id, expected, desired, version }))
    }

    fn id(&self) -> &GherritPrId {
        match &self.0 {
            TupleTransitionKind::Create { id, .. } | TupleTransitionKind::Advance { id, .. } => id,
        }
    }

    fn desired(&self) -> PublicationRevision {
        match &self.0 {
            TupleTransitionKind::Create { desired, .. }
            | TupleTransitionKind::Advance { desired, .. } => *desired,
        }
    }

    fn expected(&self) -> Option<PublicationRevision> {
        match &self.0 {
            TupleTransitionKind::Create { .. } => None,
            TupleTransitionKind::Advance { expected, .. } => Some(*expected),
        }
    }

    fn version(&self) -> Version {
        match &self.0 {
            TupleTransitionKind::Create { .. } => Version::FIRST,
            TupleTransitionKind::Advance { version, .. } => *version,
        }
    }
}

/// One absent pull-request marker to create at an exact published head.
///
/// No update variant exists because marker tags are immutable. A caller which
/// observes an existing marker must validate it rather than overwrite it.
#[derive(Clone, Debug)]
pub(super) struct MarkerTransition {
    id: GherritPrId,
    target: ObjectId,
}

impl MarkerTransition {
    pub(super) fn create(id: GherritPrId, target: ObjectId) -> Result<Self> {
        if target.is_null() {
            bail!("Git pull-request markers require a non-null target");
        }
        Ok(Self { id, target })
    }
}

#[derive(Clone, Copy)]
enum LeaseExpectation {
    Absent,
    At(ObjectId),
}

impl LeaseExpectation {
    fn render(self) -> String {
        match self {
            Self::Absent => String::new(),
            Self::At(object_id) => object_id.to_string(),
        }
    }

    fn receipt_transition(self) -> ExpectedRefTransition {
        match self {
            Self::Absent => ExpectedRefTransition::CreateOrAlreadyDesired,
            Self::At(_) => ExpectedRefTransition::UpdateOrAlreadyDesired,
        }
    }
}

struct AtomicUnit {
    options: Box<[String]>,
    refspecs: Box<[String]>,
    expected: Box<[(String, ExpectedRefReceipt)]>,
    encoded_argv_bytes: usize,
}

impl AtomicUnit {
    fn tuple(transition: &TupleTransition) -> Self {
        let id = transition.id().as_str();
        let desired = transition.desired();
        let expected = transition.expected();
        let head = format!("refs/heads/{id}");
        let base = format!("refs/heads/gherrit-bases/{id}");
        let tag = format!("refs/tags/gherrit/{id}/v{}", transition.version());
        let head_expectation = expected
            .map_or(LeaseExpectation::Absent, |revision| LeaseExpectation::At(revision.head));
        let base_expectation = expected
            .map_or(LeaseExpectation::Absent, |revision| LeaseExpectation::At(revision.owned_base));
        let options = [
            format!("--force-with-lease={head}:{}", head_expectation.render()),
            format!("--force-with-lease={base}:{}", base_expectation.render()),
            format!("--force-with-lease={tag}:"),
        ];
        let refspecs = [
            format!("{}:{head}", desired.head),
            format!("{}:{base}", desired.owned_base),
            format!("{}:{tag}", desired.head),
        ];
        let expected = [
            (head, ExpectedRefReceipt::new(desired.head, head_expectation.receipt_transition())),
            (
                base,
                ExpectedRefReceipt::new(desired.owned_base, base_expectation.receipt_transition()),
            ),
            (
                tag,
                ExpectedRefReceipt::new(
                    desired.head,
                    ExpectedRefTransition::CreateOrAlreadyDesired,
                ),
            ),
        ];
        Self::new(options, refspecs, expected)
    }

    fn marker(transition: &MarkerTransition) -> Self {
        let destination = format!("refs/tags/gherrit/{}/pr", transition.id.as_str());
        let option = format!("--force-with-lease={destination}:");
        let refspec = format!("{}:{destination}", transition.target);
        let expected = (
            destination,
            ExpectedRefReceipt::new(
                transition.target,
                ExpectedRefTransition::CreateOrAlreadyDesired,
            ),
        );
        Self::new([option], [refspec], [expected])
    }

    fn new<const OPTIONS: usize, const REFSPECS: usize, const RECEIPTS: usize>(
        options: [String; OPTIONS],
        refspecs: [String; REFSPECS],
        expected: [(String, ExpectedRefReceipt); RECEIPTS],
    ) -> Self {
        let encoded_argv_bytes =
            options.iter().chain(&refspecs).map(|argument| argument.len() + 1).sum();
        Self {
            options: options.into(),
            refspecs: refspecs.into(),
            expected: expected.into(),
            encoded_argv_bytes,
        }
    }
}

/// Fully rendered, bounded push requests bound to one exact destination.
pub(super) struct PreparedPushes {
    target: PublicationTarget,
    batches: Box<[PushBatch]>,
}

impl PreparedPushes {
    /// Executes each preflighted atomic batch from the exact repository which
    /// validated this publication. No command output or partial outcome
    /// escapes the execution boundary.
    pub(super) async fn execute(self, destination: &PushDestination) -> Result<()> {
        if self.target != destination.publication_target() {
            bail!("Git publication action belongs to a different repository or push destination");
        }
        for batch in self.batches.into_vec() {
            let command =
                destination.push(batch.options.iter().cloned(), batch.refspecs.iter().cloned());
            let output = subprocess::output(command, REMOTE_GIT_EXECUTION_TIMEOUT)
                .await
                .wrap_err("Git publication process failed; its remote effects are indeterminate")?;
            let outcome = batch.outcome(output.status(), output.stdout());
            let diagnostic = match outcome {
                PushOutcome::Acknowledged => None,
                PushOutcome::Rejected | PushOutcome::Indeterminate => {
                    output.child_diagnostic(destination)
                }
            };
            require_acknowledgement(outcome, diagnostic)?;
        }
        Ok(())
    }

    #[cfg(test)]
    pub(super) fn batches(&self) -> impl ExactSizeIterator<Item = &PushBatch> {
        self.batches.iter()
    }

    #[cfg(test)]
    pub(super) fn into_batches(self) -> Box<[PushBatch]> {
        self.batches
    }
}

fn require_acknowledgement(outcome: PushOutcome, diagnostic: Option<String>) -> Result<()> {
    let message = match outcome {
        PushOutcome::Acknowledged => return Ok(()),
        PushOutcome::Rejected => {
            "Git publication was rejected without changing the requested refs; stop this attempt and run GHerrit again after resolving the conflict"
        }
        PushOutcome::Indeterminate => {
            "Git publication acknowledgement is indeterminate; requested refs may or may not have changed, so stop this attempt and run GHerrit again to reobserve them"
        }
    };
    Err(diagnostic.map_or_else(
        || eyre!(message),
        |diagnostic| {
            eyre!(
                "{message}\n\nInternal push diagnostic (untrusted and not publication evidence):\n{diagnostic}"
            )
        },
    ))
}

/// Renders complete, indivisible change tuples into bounded atomic batches.
pub(super) fn prepare_tuple_pushes(
    destination: &PushDestination,
    transitions: &[TupleTransition],
) -> Result<PreparedPushes> {
    prepare_tuple_pushes_with_budget(destination, transitions, PUSH_VARIABLE_ARGV_BUDGET_BYTES)
}

fn prepare_tuple_pushes_with_budget(
    destination: &PushDestination,
    transitions: &[TupleTransition],
    budget: usize,
) -> Result<PreparedPushes> {
    prepare_units(
        destination,
        transitions.iter().map(AtomicUnit::tuple).collect(),
        budget,
        "change tuple",
    )
}

/// Renders create-only marker transitions into bounded atomic batches.
pub(super) fn prepare_marker_pushes(
    destination: &PushDestination,
    transitions: &[MarkerTransition],
) -> Result<PreparedPushes> {
    prepare_marker_pushes_with_budget(destination, transitions, PUSH_VARIABLE_ARGV_BUDGET_BYTES)
}

fn prepare_marker_pushes_with_budget(
    destination: &PushDestination,
    transitions: &[MarkerTransition],
    budget: usize,
) -> Result<PreparedPushes> {
    prepare_units(
        destination,
        transitions.iter().map(AtomicUnit::marker).collect(),
        budget,
        "pull-request marker",
    )
}

fn prepare_units(
    destination: &PushDestination,
    units: Vec<AtomicUnit>,
    budget: usize,
    kind: &str,
) -> Result<PreparedPushes> {
    // Render and size every indivisible unit before the first batch exists. A
    // late error can therefore reject only the whole candidate, never return
    // a prefix which an executor could expose.
    for (index, unit) in units.iter().enumerate() {
        if unit.encoded_argv_bytes > budget {
            bail!(
                "Git {kind} {index} requires {} bytes of variable push arguments, which exceeds the {budget}-byte variable-argument budget",
                unit.encoded_argv_bytes
            );
        }
    }
    validate_unique_destinations(
        units.iter().flat_map(|unit| unit.expected.iter().map(|(destination, _)| destination)),
    )?;

    let mut groups = Vec::<Vec<AtomicUnit>>::new();
    let mut current = Vec::new();
    let mut current_bytes = 0;
    for unit in units {
        if !current.is_empty() && current_bytes > budget - unit.encoded_argv_bytes {
            groups.push(std::mem::take(&mut current));
            current_bytes = 0;
        }
        current_bytes += unit.encoded_argv_bytes;
        current.push(unit);
    }
    if !current.is_empty() {
        groups.push(current);
    }

    let batches = groups
        .into_iter()
        .map(PushBatch::from_units)
        .collect::<Result<Vec<_>>>()?
        .into_boxed_slice();
    Ok(PreparedPushes { target: destination.publication_target(), batches })
}

fn validate_unique_destinations<'destination>(
    destinations: impl IntoIterator<Item = &'destination String>,
) -> Result<()> {
    let mut seen = HashSet::new();
    for destination in destinations {
        if !seen.insert(destination) {
            bail!("Git publication plans destination '{destination}' more than once");
        }
    }
    Ok(())
}

/// One nonempty atomic push request and its complete receipt contract.
#[derive(Debug)]
pub(super) struct PushBatch {
    options: Box<[String]>,
    refspecs: Box<[String]>,
    expected: ExpectedReceipts,
}

impl PushBatch {
    fn from_units(units: Vec<AtomicUnit>) -> Result<Self> {
        if units.is_empty() {
            bail!("Git publication cannot construct an empty push batch");
        }
        let mut options = FIXED_PUSH_OPTIONS.map(str::to_owned).to_vec();
        let mut refspecs = Vec::new();
        let mut expected = Vec::new();
        for unit in units {
            options.extend(unit.options);
            refspecs.extend(unit.refspecs);
            expected.extend(unit.expected);
        }
        Ok(Self {
            options: options.into_boxed_slice(),
            refspecs: refspecs.into_boxed_slice(),
            expected: ExpectedReceipts::new(expected)?,
        })
    }

    #[cfg(test)]
    pub(super) fn options(&self) -> impl ExactSizeIterator<Item = &str> {
        self.options.iter().map(String::as_str)
    }

    #[cfg(test)]
    pub(super) fn refspecs(&self) -> impl ExactSizeIterator<Item = &str> {
        self.refspecs.iter().map(String::as_str)
    }

    /// Acknowledges only a normally exiting zero-status process with one
    /// complete exact porcelain response for this batch. A complete ordinary
    /// nonzero response which claims no ref mutation is a known rejection;
    /// every other result is operationally indeterminate.
    fn outcome(&self, status: &ExitStatus, stdout: &[u8]) -> PushOutcome {
        let Some(receipts) = parse_push_receipts(&self.expected, stdout) else {
            return PushOutcome::Indeterminate;
        };
        match status.code() {
            Some(0)
                if receipts.iter().all(|(transition, receipt)| receipt.satisfies(*transition)) =>
            {
                PushOutcome::Acknowledged
            }
            Some(0) | None => PushOutcome::Indeterminate,
            Some(_) if receipts.iter().all(|(_, receipt)| receipt.claims_no_mutation()) => {
                PushOutcome::Rejected
            }
            Some(_) => PushOutcome::Indeterminate,
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
        if refs.is_empty() {
            bail!("Git publication cannot expect receipts for an empty push batch");
        }
        Ok(Self { refs })
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct ExpectedRefReceipt {
    source: ObjectId,
    transition: ExpectedRefTransition,
}

impl ExpectedRefReceipt {
    fn new(source: ObjectId, transition: ExpectedRefTransition) -> Self {
        Self { source, transition }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ExpectedRefTransition {
    CreateOrAlreadyDesired,
    UpdateOrAlreadyDesired,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum PushOutcome {
    /// Every expected ref reported an allowed success or exact no-op.
    Acknowledged,
    /// A normal nonzero process reported only failures and exact no-ops.
    Rejected,
    /// The process or receipt stream did not establish either other outcome.
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

    fn claims_no_mutation(self) -> bool {
        matches!(self, Self::AlreadyDesired | Self::Failed)
    }
}

/// Decodes `git push --porcelain` stdout without retaining or reporting its
/// human-readable destination header.
#[cfg(test)]
fn receipts_acknowledge(expected: &ExpectedReceipts, stdout: &[u8]) -> bool {
    let Some(receipts) = parse_push_receipts(expected, stdout) else {
        return false;
    };
    receipts.iter().all(|(transition, status)| status.satisfies(*transition))
}

fn parse_push_receipts(
    expected: &ExpectedReceipts,
    stdout: &[u8],
) -> Option<Vec<(ExpectedRefTransition, ReceiptStatus)>> {
    fn split_last<'input>(
        input: &'input [u8],
        delimiter: &[u8],
    ) -> Option<(&'input [u8], &'input [u8])> {
        let start = input.windows(delimiter.len()).rposition(|window| window == delimiter)?;
        Some((&input[..start], &input[start + delimiter.len()..]))
    }

    // A composite pre-push hook runs before Git writes its porcelain result
    // and may itself write to stdout. Decode the exact final Git block rather
    // than requiring it to own the whole stream. The expected receipt count
    // fixes the suffix length, so earlier hook output—even a forged complete
    // porcelain block—cannot add, remove, or replace a receipt.
    let (output, line_ending) = stdout
        .strip_suffix(b"\r\n")
        .map(|output| (output, b"\r\n".as_slice()))
        .or_else(|| stdout.strip_suffix(b"\n").map(|output| (output, b"\n".as_slice())))?;
    let (mut body, footer) = split_last(output, line_ending)?;
    let footer = str::from_utf8(footer).ok()?;

    let mut status_lines = Vec::with_capacity(expected.refs.len());
    for _ in 0..expected.refs.len() {
        let (remaining, line) = split_last(body, line_ending)?;
        status_lines.push(str::from_utf8(line).ok()?);
        body = remaining;
    }
    status_lines.reverse();

    // Git does not insert a separator between an earlier hook's stdout and
    // its own header. Find the final header introducer instead of assuming
    // that the independent producer ended with LF. The fixed receipt suffix
    // still makes any extra status record part of the would-be header, where
    // the framing checks below reject it. An earlier forged block cannot
    // displace the real final header.
    let header_start = body.windows(b"To ".len()).rposition(|window| window == b"To ")?;
    let header = &body[header_start..];
    let header = str::from_utf8(header).ok()?;
    parse_push_receipt_lines(expected, header, footer, status_lines)
}

fn parse_push_receipt_lines<'line>(
    expected: &ExpectedReceipts,
    header: &str,
    footer: &str,
    status_lines: impl IntoIterator<Item = &'line str>,
) -> Option<Vec<(ExpectedRefTransition, ReceiptStatus)>> {
    let displayed_destination = header.strip_prefix("To ")?;
    if displayed_destination.is_empty()
        || displayed_destination.chars().any(char::is_control)
        || footer != "Done"
    {
        return None;
    }

    let mut receipts = Vec::with_capacity(expected.refs.len());
    let mut seen = BTreeSet::new();
    for line in status_lines {
        // The subprocess boundary retains at most 64 MiB, but an adversarial
        // newline storm could still contain millions of slices. Stop after
        // the first impossible extra record instead of collecting or draining
        // the rest of the response.
        if receipts.len() == expected.refs.len() {
            return None;
        }
        if line.is_empty() {
            return None;
        }
        let mut fields = line.split('\t');
        let (Some(flag), Some(refs), Some(summary), None) =
            (fields.next(), fields.next(), fields.next(), fields.next())
        else {
            return None;
        };
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
        if source != expected_ref.source.to_string()
            || summary.is_empty()
            || !destination.starts_with("refs/")
            || [source, destination, summary]
                .into_iter()
                .any(|field| field.chars().any(char::is_control))
            || !seen.insert(destination)
        {
            return None;
        }
        receipts.push((expected_ref.transition, status));
    }
    (seen.len() == expected.refs.len()).then_some(receipts)
}

#[cfg(test)]
mod tests {
    use std::env;

    use super::*;

    fn object_id(byte: u8) -> ObjectId {
        ObjectId::from_bytes_or_panic(&[byte; 20])
    }

    fn id(value: &str) -> GherritPrId {
        GherritPrId::from_ref_component(value.as_bytes()).unwrap()
    }

    fn destination() -> PushDestination {
        PushDestination::for_test()
    }

    fn revision(head: u8, base: u8) -> PublicationRevision {
        PublicationRevision::new(object_id(head), object_id(base)).unwrap()
    }

    fn create(value: &str, head: u8, base: u8) -> TupleTransition {
        TupleTransition::create(id(value), revision(head, base))
    }

    fn advance(
        value: &str,
        expected_head: u8,
        expected_base: u8,
        desired_head: u8,
        desired_base: u8,
        latest_version: u64,
    ) -> TupleTransition {
        TupleTransition::advance(
            id(value),
            revision(expected_head, expected_base),
            revision(desired_head, desired_base),
            Version::new(latest_version).unwrap(),
        )
        .unwrap()
    }

    fn one_tuple_batch(transition: TupleTransition) -> PushBatch {
        prepare_tuple_pushes(&destination(), &[transition])
            .unwrap()
            .into_batches()
            .into_vec()
            .pop()
            .unwrap()
    }

    fn one_marker_batch(transition: MarkerTransition) -> PushBatch {
        prepare_marker_pushes(&destination(), &[transition])
            .unwrap()
            .into_batches()
            .into_vec()
            .pop()
            .unwrap()
    }

    #[test]
    fn first_publication_is_one_exact_three_ref_creation_tuple() {
        let transition = create("Gone", 0x22, 0x11);
        assert_eq!(transition.desired().head(), object_id(0x22));
        assert_eq!(transition.desired().owned_base(), object_id(0x11));
        let batch = one_tuple_batch(transition);

        assert_eq!(
            batch.options().collect::<Vec<_>>(),
            [
                "--porcelain",
                "--atomic",
                "--no-follow-tags",
                "--recurse-submodules=no",
                "--no-signed",
                "--force-with-lease=refs/heads/Gone:",
                "--force-with-lease=refs/heads/gherrit-bases/Gone:",
                "--force-with-lease=refs/tags/gherrit/Gone/v1:",
            ]
        );
        assert_eq!(
            batch.refspecs().collect::<Vec<_>>(),
            [
                format!("{}:refs/heads/Gone", object_id(0x22)),
                format!("{}:refs/heads/gherrit-bases/Gone", object_id(0x11)),
                format!("{}:refs/tags/gherrit/Gone/v1", object_id(0x22)),
            ]
        );
        assert!(!batch.options().any(|option| option == "--no-force-if-includes"));
        assert!(!batch.options().any(|option| option == "--no-verify"));
    }

    #[test]
    fn advancement_leases_both_mutable_refs_and_creates_only_the_next_tag() {
        let batch = one_tuple_batch(advance("Gone", 0x22, 0x11, 0x44, 0x11, 7));
        assert_eq!(
            &batch.options().collect::<Vec<_>>()[FIXED_PUSH_OPTIONS.len()..],
            [
                format!("--force-with-lease=refs/heads/Gone:{}", object_id(0x22)),
                format!("--force-with-lease=refs/heads/gherrit-bases/Gone:{}", object_id(0x11)),
                "--force-with-lease=refs/tags/gherrit/Gone/v8:".to_owned(),
            ]
        );
        assert_eq!(
            batch.refspecs().collect::<Vec<_>>(),
            [
                format!("{}:refs/heads/Gone", object_id(0x44)),
                format!("{}:refs/heads/gherrit-bases/Gone", object_id(0x11)),
                format!("{}:refs/tags/gherrit/Gone/v8", object_id(0x44)),
            ]
        );
    }

    #[test]
    fn transition_types_reject_states_which_semantic_planning_cannot_emit() {
        let null = ObjectId::null(gix::hash::Kind::Sha1);
        assert!(PublicationRevision::new(null, object_id(1)).is_err());
        assert!(PublicationRevision::new(object_id(1), null).is_err());
        assert!(PublicationRevision::new(object_id(1), object_id(1)).is_err());
        assert!(MarkerTransition::create(id("Gone"), null).is_err());
        assert!(
            TupleTransition::advance(id("Gone"), revision(2, 1), revision(2, 1), Version::FIRST,)
                .is_err()
        );
        assert!(
            TupleTransition::advance(
                id("Gone"),
                revision(2, 1),
                revision(3, 1),
                Version::new(u64::MAX).unwrap(),
            )
            .is_err()
        );
        assert!(
            TupleTransition::advance(id("Gone"), revision(2, 1), revision(2, 3), Version::FIRST,)
                .is_err()
        );
    }

    #[test]
    fn marker_publication_has_only_a_create_transition() {
        let marker = MarkerTransition::create(id("Gone"), object_id(0x22)).unwrap();
        let batch = one_marker_batch(marker);
        assert_eq!(
            &batch.options().collect::<Vec<_>>()[FIXED_PUSH_OPTIONS.len()..],
            ["--force-with-lease=refs/tags/gherrit/Gone/pr:"]
        );
        assert_eq!(
            batch.refspecs().collect::<Vec<_>>(),
            [format!("{}:refs/tags/gherrit/Gone/pr", object_id(0x22))]
        );
    }

    #[test]
    fn tuple_batching_is_exactly_bounded_and_never_splits_a_tuple() {
        let first = create("Gone", 2, 1);
        let second = create("Gtwo", 4, 3);
        let first_bytes = AtomicUnit::tuple(&first).encoded_argv_bytes;
        let second_bytes = AtomicUnit::tuple(&second).encoded_argv_bytes;

        let exact = prepare_tuple_pushes_with_budget(
            &destination(),
            &[create("Gone", 2, 1), create("Gtwo", 4, 3)],
            first_bytes + second_bytes,
        )
        .unwrap();
        assert_eq!(exact.batches().len(), 1);
        assert_eq!(exact.batches().next().unwrap().refspecs().len(), 6);

        let split = prepare_tuple_pushes_with_budget(
            &destination(),
            &[create("Gone", 2, 1), create("Gtwo", 4, 3)],
            first_bytes + second_bytes - 1,
        )
        .unwrap();
        assert_eq!(split.batches().len(), 2);
        assert!(split.batches().all(|batch| batch.refspecs().len() == 3));

        let exact_single =
            prepare_tuple_pushes_with_budget(&destination(), &[create("Gone", 2, 1)], first_bytes)
                .unwrap();
        assert_eq!(exact_single.batches().len(), 1);
        assert!(
            prepare_tuple_pushes_with_budget(
                &destination(),
                &[create("Gone", 2, 1)],
                first_bytes - 1,
            )
            .is_err()
        );
    }

    #[test]
    fn a_late_oversized_or_duplicate_tuple_rejects_the_complete_candidate() {
        let first = create("Gone", 2, 1);
        let budget = AtomicUnit::tuple(&first).encoded_argv_bytes;
        let oversized = create(&format!("G{}", "x".repeat(100)), 4, 3);
        assert!(
            prepare_tuple_pushes_with_budget(
                &destination(),
                &[create("Gone", 2, 1), oversized],
                budget,
            )
            .is_err()
        );
        assert!(
            prepare_tuple_pushes(&destination(), &[create("Gone", 2, 1), create("Gone", 4, 3)],)
                .is_err()
        );
    }

    #[test]
    fn marker_batches_are_bounded_create_only_and_globally_unique() {
        let first = MarkerTransition::create(id("Gone"), object_id(2)).unwrap();
        let second = MarkerTransition::create(id("Gtwo"), object_id(3)).unwrap();
        let encoded = AtomicUnit::marker(&first).encoded_argv_bytes;
        let batches =
            prepare_marker_pushes_with_budget(&destination(), &[first.clone(), second], encoded)
                .unwrap();
        assert_eq!(batches.batches().len(), 2);
        assert!(batches.batches().all(|batch| batch.refspecs().len() == 1));
        assert!(
            prepare_marker_pushes_with_budget(
                &destination(),
                std::slice::from_ref(&first),
                encoded - 1,
            )
            .is_err()
        );
        assert!(prepare_marker_pushes(&destination(), &[first.clone(), first]).is_err());
    }

    #[test]
    fn empty_transition_sets_produce_no_batches() {
        assert_eq!(prepare_tuple_pushes(&destination(), &[]).unwrap().batches().len(), 0);
        assert_eq!(prepare_marker_pushes(&destination(), &[]).unwrap().batches().len(), 0);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn an_empty_push_action_crosses_without_starting_git() {
        let destination = destination();
        prepare_tuple_pushes(&destination, &[]).unwrap().execute(&destination).await.unwrap();
    }

    #[tokio::test(flavor = "current_thread")]
    async fn a_push_action_cannot_be_relabelled_to_another_destination() {
        let repository = crate::util::Repo::open(".").unwrap();
        let https =
            PushDestination::for_test_url_in(&repository, "https://github.com/owner/repo.git");
        let ssh = PushDestination::for_test_url_in(&repository, "git@github.com:owner/repo.git");
        let error = prepare_tuple_pushes(&https, &[]).unwrap().execute(&ssh).await.unwrap_err();

        assert_eq!(
            error.to_string(),
            "Git publication action belongs to a different repository or push destination"
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn a_push_action_cannot_be_moved_to_another_local_repository() {
        let first = tempfile::tempdir().unwrap();
        let second = tempfile::tempdir().unwrap();
        gix::init_bare(first.path()).unwrap();
        gix::init_bare(second.path()).unwrap();
        let first = crate::util::Repo::open(first.path().to_str().unwrap()).unwrap();
        let second = crate::util::Repo::open(second.path().to_str().unwrap()).unwrap();
        let first = PushDestination::for_test_in(&first);
        let second = PushDestination::for_test_in(&second);

        let error = prepare_tuple_pushes(&first, &[]).unwrap().execute(&second).await.unwrap_err();

        assert_eq!(
            error.to_string(),
            "Git publication action belongs to a different repository or push destination"
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn a_prepared_tuple_executes_against_its_bound_real_repository() {
        let context = testutil::TestContextBuilder::new(env::current_exe().unwrap())
            .with_remote()
            .with_initial_commit()
            .build();
        let base = ObjectId::from_hex(context.head_oid().as_bytes()).unwrap();
        context.commit("publication head");
        let head = ObjectId::from_hex(context.head_oid().as_bytes()).unwrap();
        let repository = crate::util::Repo::open(context.repo_path.to_str().unwrap()).unwrap();
        let destination =
            PushDestination::resolve(&repository, repository.default_remote_name().unwrap())
                .unwrap();

        prepare_tuple_pushes(
            &destination,
            &[TupleTransition::create(
                id("Gpublication"),
                PublicationRevision::new(head, base).unwrap(),
            )],
        )
        .unwrap()
        .execute(&destination)
        .await
        .unwrap();

        assert_eq!(context.remote_ref_oid("refs/heads/Gpublication"), Some(head.to_string()));
        assert_eq!(
            context.remote_ref_oid("refs/heads/gherrit-bases/Gpublication"),
            Some(base.to_string())
        );
        assert_eq!(
            context.remote_ref_oid("refs/tags/gherrit/Gpublication/v1"),
            Some(head.to_string())
        );
    }

    #[test]
    fn only_exact_acknowledgement_releases_the_next_stage() {
        require_acknowledgement(PushOutcome::Acknowledged, None).unwrap();

        for (outcome, classification) in [
            (PushOutcome::Rejected, "without changing the requested refs"),
            (PushOutcome::Indeterminate, "may or may not have changed"),
        ] {
            let plain = require_acknowledgement(outcome, None).unwrap_err().to_string();
            assert!(plain.contains(classification));
            assert!(!plain.contains("Internal push diagnostic"));

            let diagnostic =
                require_acknowledgement(outcome, Some("safe local policy explanation".to_owned()))
                    .unwrap_err()
                    .to_string();
            assert!(diagnostic.contains(classification));
            assert!(diagnostic.contains(
                "Internal push diagnostic (untrusted and not publication evidence):\n\
                 safe local policy explanation"
            ));
        }
    }

    fn expected_receipts(receipts: &[(&str, ObjectId, ExpectedRefTransition)]) -> ExpectedReceipts {
        ExpectedReceipts::new(receipts.iter().map(|(destination, source, transition)| {
            ((*destination).to_owned(), ExpectedRefReceipt::new(*source, *transition))
        }))
        .unwrap()
    }

    #[test]
    fn receipts_accept_every_produced_success_flag_in_arbitrary_order() {
        let source = object_id(2);
        let expected = expected_receipts(&[
            ("refs/heads/fast", source, ExpectedRefTransition::UpdateOrAlreadyDesired),
            ("refs/heads/forced", source, ExpectedRefTransition::UpdateOrAlreadyDesired),
            ("refs/heads/new", source, ExpectedRefTransition::CreateOrAlreadyDesired),
            ("refs/heads/current", source, ExpectedRefTransition::CreateOrAlreadyDesired),
        ]);
        let output = format!(
            concat!(
                "To private destination\n",
                "=\t{source}:refs/heads/current\t[up to date]\n",
                "*\t{source}:refs/heads/new\t[new branch]\n",
                " \t{source}:refs/heads/fast\told..new\n",
                "+\t{source}:refs/heads/forced\told...new (forced update)\n",
                "Done\n",
            ),
            source = source
        );
        for output in [output.clone(), output.replace('\n', "\r\n")] {
            assert!(receipts_acknowledge(&expected, output.as_bytes()));
        }
    }

    #[test]
    fn exact_already_desired_is_a_no_op_but_lease_rejection_is_not_success() {
        let desired_head = object_id(4);
        let desired_base = object_id(1);
        let batch = one_tuple_batch(advance("Gone", 2, 1, 4, 1, 1));
        let already = format!(
            concat!(
                "To private\n",
                "=\t{desired_head}:refs/heads/Gone\t[up to date]\n",
                "=\t{desired_base}:refs/heads/gherrit-bases/Gone\t[up to date]\n",
                "=\t{desired_head}:refs/tags/gherrit/Gone/v2\t[up to date]\n",
                "Done\n",
            ),
            desired_head = desired_head,
            desired_base = desired_base
        );
        assert!(receipts_acknowledge(&batch.expected, already.as_bytes()));

        let wrong_source =
            already.replacen(&desired_head.to_string(), &object_id(3).to_string(), 1);
        assert!(!receipts_acknowledge(&batch.expected, wrong_source.as_bytes()));

        let stale_lease = format!(
            concat!(
                "To private\n",
                "!\t{desired_head}:refs/heads/Gone\t[rejected] (stale info)\n",
                "=\t{desired_base}:refs/heads/gherrit-bases/Gone\t[up to date]\n",
                "!\t{desired_head}:refs/tags/gherrit/Gone/v2\t[rejected] (atomic push failed)\n",
                "Done\n",
            ),
            desired_head = desired_head,
            desired_base = desired_base
        );
        assert!(!receipts_acknowledge(&batch.expected, stale_lease.as_bytes()));
    }

    #[test]
    fn receipt_flags_must_match_the_exact_planned_transition() {
        let source = object_id(2);
        for (transition, accepted) in [
            (ExpectedRefTransition::CreateOrAlreadyDesired, ["*", "="]),
            (ExpectedRefTransition::UpdateOrAlreadyDesired, [" ", "+"]),
        ] {
            let expected = expected_receipts(&[("refs/heads/Gone", source, transition)]);
            for flag in [" ", "+", "*", "=", "!"] {
                let output =
                    format!("To private\n{flag}\t{source}:refs/heads/Gone\tstatus\nDone\n");
                let acknowledged = accepted.contains(&flag) || flag == "=";
                assert_eq!(receipts_acknowledge(&expected, output.as_bytes()), acknowledged);
            }
        }
    }

    #[test]
    fn receipt_decoder_rejects_bad_framing_records_and_coverage() {
        let source = object_id(2);
        let expected = expected_receipts(&[
            ("refs/heads/Gone", source, ExpectedRefTransition::CreateOrAlreadyDesired),
            ("refs/tags/gherrit/Gone/v1", source, ExpectedRefTransition::CreateOrAlreadyDesired),
        ]);
        for output in [
            String::new(),
            format!("To private\n*\t{source}:refs/heads/Gone\t[new branch]\nDone"),
            format!("To \n*\t{source}:refs/heads/Gone\t[new branch]\nDone\n"),
            format!("To private\n*\t{source}:refs/heads/Gone\t[new branch]\nComplete\n"),
            format!("To private\n*\t{source}:refs/heads/Gone\t[new branch]\nDone\n\n"),
            format!("To private\r\n*\t{source}:refs/heads/Gone\t[new branch]\nDone\r\n"),
            format!(
                "To private\n*\t{source}:refs/heads/Gone\t[new branch]\n=\t{source}:refs/heads/Gone\t[up to date]\nDone\n"
            ),
            format!(
                "To private\n*\t{source}:refs/heads/Gone\t[new branch]\n*\t{source}:refs/tags/gherrit/Gone/v2\t[new tag]\nDone\n"
            ),
            format!("To private\n* {source}:refs/heads/Gone [new branch]\nDone\n"),
            format!("To private\n?\t{source}:refs/heads/Gone\tstatus\nDone\n"),
            format!("To private\n*\t{source}:refs/heads/Gone\t\nDone\n"),
            format!("To private\n*\t{source}:refs/heads/Gone\tbad\rsummary\nDone\n"),
        ] {
            assert!(!receipts_acknowledge(&expected, output.as_bytes()), "accepted {output:?}");
        }
        assert!(!receipts_acknowledge(&expected, b"To private\n\xff\nDone\n"));
    }

    #[test]
    fn receipt_decoder_stops_after_the_first_impossible_extra_record() {
        let source = object_id(2);
        let expected = expected_receipts(&[(
            "refs/heads/Gone",
            source,
            ExpectedRefTransition::CreateOrAlreadyDesired,
        )]);
        let status = format!("*\t{source}:refs/heads/Gone\t[new branch]");
        let mut polls = 0;
        let poison = std::iter::from_fn(|| {
            polls += 1;
            match polls {
                1 | 2 => Some(status.as_str()),
                _ => panic!("receipt parsing consumed past the first extra status record"),
            }
        });

        assert!(parse_push_receipt_lines(&expected, "To private", "Done", poison).is_none());
    }

    #[test]
    fn receipt_decoder_uses_the_exact_final_block_after_composite_hook_output() {
        let source = object_id(2);
        let expected = expected_receipts(&[(
            "refs/heads/Gone",
            source,
            ExpectedRefTransition::CreateOrAlreadyDesired,
        )]);
        let receipt = format!("*\t{source}:refs/heads/Gone\t[new branch]");

        for prefix_ending in ["", "\n", "\r\n"] {
            for git_ending in ["\n", "\r\n"] {
                let output = format!(
                    "policy check passed{prefix_ending}To private{git_ending}{receipt}{git_ending}Done{git_ending}"
                );
                assert!(receipts_acknowledge(&expected, output.as_bytes()), "rejected {output:?}");
            }
        }

        for prefix in [
            "policy check passed".to_owned(),
            "policy check passed\n".to_owned(),
            format!("To forged\n{receipt}\nDone"),
        ] {
            let output = format!("{prefix}To private\n{receipt}\nDone\n");
            assert!(receipts_acknowledge(&expected, output.as_bytes()), "rejected {output:?}");
        }

        let mut binary_prefix = b"policy:\xff".to_vec();
        binary_prefix.extend(format!("To private\n{receipt}\nDone\n").as_bytes());
        assert!(receipts_acknowledge(&expected, &binary_prefix));

        let no_git_header = format!("policy check passedprivate\n{receipt}\nDone\n");
        assert!(!receipts_acknowledge(&expected, no_git_header.as_bytes()));
    }

    #[test]
    fn receipt_decoder_does_not_treat_trailing_output_as_a_prefix() {
        let source = object_id(2);
        let expected = expected_receipts(&[(
            "refs/heads/Gone",
            source,
            ExpectedRefTransition::CreateOrAlreadyDesired,
        )]);
        let output = format!(
            "To private\n*\t{source}:refs/heads/Gone\t[new branch]\nDone\nlate hook output\n"
        );
        assert!(!receipts_acknowledge(&expected, output.as_bytes()));
    }

    #[cfg(unix)]
    #[test]
    fn process_status_distinguishes_acknowledgement_rejection_and_ambiguity() {
        use std::os::unix::process::ExitStatusExt as _;

        let source = object_id(2);
        let batch = one_marker_batch(MarkerTransition::create(id("Gone"), source).unwrap());
        let output =
            format!("To private\n*\t{source}:refs/tags/gherrit/Gone/pr\t[new tag]\nDone\n");
        assert_eq!(
            batch.outcome(&ExitStatus::from_raw(0), output.as_bytes()),
            PushOutcome::Acknowledged
        );
        assert_eq!(
            batch.outcome(&ExitStatus::from_raw(1 << 8), output.as_bytes()),
            PushOutcome::Indeterminate
        );
        assert_eq!(
            batch.outcome(&ExitStatus::from_raw(9), output.as_bytes()),
            PushOutcome::Indeterminate
        );

        let rejected = format!(
            "To private\n!\t{source}:refs/tags/gherrit/Gone/pr\t[rejected] (stale info)\nDone\n"
        );
        assert_eq!(
            batch.outcome(&ExitStatus::from_raw(1 << 8), rejected.as_bytes()),
            PushOutcome::Rejected
        );
        assert_eq!(
            batch.outcome(&ExitStatus::from_raw(0), rejected.as_bytes()),
            PushOutcome::Indeterminate
        );

        let already =
            format!("To private\n=\t{source}:refs/tags/gherrit/Gone/pr\t[up to date]\nDone\n");
        assert_eq!(
            batch.outcome(&ExitStatus::from_raw(0), already.as_bytes()),
            PushOutcome::Acknowledged
        );
        assert_eq!(
            batch.outcome(&ExitStatus::from_raw(1 << 8), already.as_bytes()),
            PushOutcome::Rejected
        );

        for flag in [" ", "+", "*"] {
            let claims_mutation =
                format!("To private\n{flag}\t{source}:refs/tags/gherrit/Gone/pr\tstatus\nDone\n");
            assert_eq!(
                batch.outcome(&ExitStatus::from_raw(1 << 8), claims_mutation.as_bytes()),
                PushOutcome::Indeterminate
            );
        }

        assert_eq!(
            batch.outcome(&ExitStatus::from_raw(1 << 8), b"malformed"),
            PushOutcome::Indeterminate
        );

        let tuple = one_tuple_batch(advance("Gtwo", 2, 1, 4, 1, 1));
        let desired_head = object_id(4);
        let desired_base = object_id(1);
        let rejected_atomic = format!(
            concat!(
                "To private\n",
                "!\t{desired_head}:refs/heads/Gtwo\t[rejected] (stale info)\n",
                "=\t{desired_base}:refs/heads/gherrit-bases/Gtwo\t[up to date]\n",
                "!\t{desired_head}:refs/tags/gherrit/Gtwo/v2\t[rejected] (atomic push failed)\n",
                "Done\n",
            ),
            desired_head = desired_head,
            desired_base = desired_base
        );
        assert_eq!(
            tuple.outcome(&ExitStatus::from_raw(1 << 8), rejected_atomic.as_bytes()),
            PushOutcome::Rejected
        );
    }
}

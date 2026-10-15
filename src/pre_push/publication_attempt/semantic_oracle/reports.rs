use super::*;

pub(super) type EffectBatches<T> = Vec<Box<[T]>>;

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct TupleEffect {
    pub(super) id: GherritPrId,
    pub(super) expected: Option<LiteralRevision>,
    pub(super) desired: LiteralRevision,
    pub(super) version: u64,
    pub(super) publication: ObjectId,
}

/// One semantic effect in the attempt's first atomic ref stage.
///
/// The optional public projection remains distinct from each indivisible
/// change tuple even when both share one atomic batch.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) enum InitialRefEffect {
    Tuple(TupleEffect),
    PublicBranch { branch: PublicBranchName, expected: Option<ObjectId>, desired: ObjectId },
}

impl InitialRefEffect {
    pub(super) fn tuple(&self) -> &TupleEffect {
        match self {
            Self::Tuple(tuple) => tuple,
            Self::PublicBranch { .. } => panic!("the requested initial-ref effect is not a tuple"),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct MarkerEffect {
    pub(super) id: GherritPrId,
    pub(super) marker: MarkerIdentity,
    pub(super) object: ObjectId,
}

/// One GitHub update request annotated only after resolving its node ID.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct UpdateEffect {
    pub(super) resolved_id: Option<GherritPrId>,
    pub(super) operation: TestUpdate,
}

/// One draft-conversion request after resolving its node ID in durable state.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct DraftEffect {
    pub(super) resolved_id: Option<GherritPrId>,
    pub(super) operation: TestDraft,
}

/// One literal operation in a mixed final-projection request.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) enum ProjectionEffect {
    Close { resolved_id: Option<GherritPrId>, identity: PullRequestIdentity },
    Update(UpdateEffect),
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(super) struct AttemptTrace {
    pub(super) draft_conversions: EffectBatches<DraftEffect>,
    pub(super) initial_refs: EffectBatches<InitialRefEffect>,
    pub(super) creates: EffectBatches<TestCreate>,
    pub(super) markers: EffectBatches<MarkerEffect>,
    pub(super) projections: EffectBatches<ProjectionEffect>,
    pub(super) alias_competitions: Vec<CompetitionBoundary>,
}

impl AttemptTrace {
    pub(super) fn is_empty(&self) -> bool {
        self.draft_conversions.is_empty()
            && self.initial_refs.is_empty()
            && self.creates.is_empty()
            && self.markers.is_empty()
            && self.projections.is_empty()
            && self.alias_competitions.is_empty()
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum AttemptOutcome {
    Acknowledged,
    Stopped(StopReason),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum StopReason {
    Indeterminate,
    Rejected,
}

/// The only process outcomes which deliberately stop a modeled Git stage.
///
/// A known rejection proves that the atomic batch did not land. An
/// indeterminate result carries the two durable worlds a lost process result
/// permits: the whole batch landed or none of it did.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum GitStop {
    Rejected,
    Indeterminate { applied: bool },
}

#[derive(Clone, Debug)]
pub(super) enum Interruption {
    Draft { batch: usize, applied_aliases: Box<[usize]> },
    InitialRefs { batch: usize, stop: GitStop },
    Create { batch: usize, applied_aliases: Box<[usize]> },
    Marker { batch: usize, stop: GitStop },
    Projection { batch: usize, applied_aliases: Box<[usize]> },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct AttemptReport {
    pub(super) outcome: AttemptOutcome,
    pub(super) trace: AttemptTrace,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum EffectStage {
    Draft,
    InitialRefs,
    Create,
    Marker,
    Projection,
}

/// A point where another complete publisher may run while the primary keeps
/// its already-planned, attempt-local authority.
///
/// Draft conversion, initial refs, creates, and markers release later
/// authority. A between-batches point is reached after the named batch is
/// acknowledged and only when a later serialized batch remains.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum CompetitionBoundary {
    AfterDraftConversions,
    AfterInitialRefs,
    AfterCreates,
    AfterMarkers,
    BetweenBatches { stage: EffectStage, completed_batch: usize },
    // Reached after one indivisible GraphQL alias, before its later sibling.
    BetweenAliases { stage: EffectStage, batch: usize, completed_alias: usize },
}

pub(super) enum Competition {
    Pending {
        boundary: CompetitionBoundary,
        plan: PlannedPublication,
        interruption: Option<Interruption>,
    },
    Complete(AttemptReport),
}

pub(super) enum DriverReport {
    Solo(AttemptReport),
    Competition { primary: AttemptReport, competitor: AttemptReport },
}

impl DriverReport {
    pub(super) fn into_solo(self) -> AttemptReport {
        let Self::Solo(report) = self else {
            panic!("this execution expected one publisher but received two reports")
        };
        report
    }

    pub(super) fn into_competition(self) -> (AttemptReport, AttemptReport) {
        let Self::Competition { primary, competitor } = self else {
            panic!("a scheduled driver must produce both reports")
        };
        (primary, competitor)
    }
}

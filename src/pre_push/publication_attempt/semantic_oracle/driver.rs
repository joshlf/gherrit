use super::*;

/// Executes the real consuming publication plan directly against literal
/// durable state. The trace is evidence of calls the production stage machine
/// actually reached; it is never replayed to drive another attempt.
pub(super) struct WorldDriver<'world> {
    pub(super) world: &'world mut DurableWorld,
    pub(super) interruption: Option<Interruption>,
    pub(super) interruption_consumed: bool,
    pub(super) failure: Option<StopReason>,
    pub(super) trace: AttemptTrace,
    pub(super) competition: Option<Competition>,
}

impl<'world> WorldDriver<'world> {
    pub(super) fn new(world: &'world mut DurableWorld, interruption: Option<Interruption>) -> Self {
        Self {
            world,
            interruption,
            interruption_consumed: false,
            failure: None,
            trace: AttemptTrace::default(),
            competition: None,
        }
    }

    pub(super) fn with_competition(
        world: &'world mut DurableWorld,
        boundary: CompetitionBoundary,
        plan: PlannedPublication,
        interruption: Option<Interruption>,
    ) -> Self {
        let mut driver = Self::new(world, None);
        driver.competition = Some(Competition::Pending { boundary, plan, interruption });
        driver
    }

    pub(super) fn take_interruption(
        &mut self,
        stage: EffectStage,
        batch: usize,
    ) -> Option<Interruption> {
        let matches = match self.interruption.as_ref() {
            Some(Interruption::Draft { batch: stopped, .. }) => {
                stage == EffectStage::Draft && *stopped == batch
            }
            Some(Interruption::InitialRefs { batch: stopped, .. }) => {
                stage == EffectStage::InitialRefs && *stopped == batch
            }
            Some(Interruption::Create { batch: stopped, .. }) => {
                stage == EffectStage::Create && *stopped == batch
            }
            Some(Interruption::Marker { batch: stopped, .. }) => {
                stage == EffectStage::Marker && *stopped == batch
            }
            Some(Interruption::Projection { batch: stopped, .. }) => {
                stage == EffectStage::Projection && *stopped == batch
            }
            None => false,
        };
        if !matches {
            return None;
        }
        assert!(!self.interruption_consumed, "one attempt has only one stop point");
        self.interruption_consumed = true;
        self.interruption.clone()
    }

    pub(super) fn stop<T>(&mut self, reason: StopReason) -> Result<T> {
        assert!(self.failure.replace(reason).is_none(), "one attempt stops only once");
        bail!("injected or observed external interruption")
    }

    pub(super) fn assert_target_draft_safety(&self, target: ExternalTarget) {
        if let ExternalTarget::Change(id) = target {
            assert!(
                self.world.local_open_rows_are_draft_safe(&id),
                "each reached local effect prefix must retain draft safety; trace={:#?}; world={:#?}",
                self.trace,
                self.world
            );
        }
    }

    pub(super) fn apply_effect(&mut self, effect: ExternalEffect) -> EffectOutcome {
        let target = effect.target();
        let outcome = self.world.apply_effect(&effect);
        self.assert_target_draft_safety(target);
        outcome
    }

    pub(super) fn assert_git_batch_draft_safety<T: Clone>(
        &self,
        effects: &[T],
        wrap: impl Fn(T) -> ExternalEffect,
    ) {
        for effect in effects.iter().cloned().map(wrap) {
            self.assert_target_draft_safety(effect.target());
        }
    }

    pub(super) fn apply_git_batch<T: Clone>(
        &mut self,
        effects: &[T],
        wrap: impl Fn(T) -> ExternalEffect + Copy,
    ) -> EffectOutcome {
        let outcome = apply_atomic_git_batch(self.world, effects, wrap);
        self.assert_git_batch_draft_safety(effects, wrap);
        outcome
    }

    pub(super) fn stop_git_batch<T: Clone>(
        &mut self,
        stop: GitStop,
        effects: &[T],
        wrap: impl Fn(T) -> ExternalEffect + Copy,
    ) -> Result<()> {
        match stop {
            GitStop::Rejected => {
                self.assert_git_batch_draft_safety(effects, wrap);
                self.stop(StopReason::Rejected)
            }
            GitStop::Indeterminate { applied: false } => {
                self.assert_git_batch_draft_safety(effects, wrap);
                self.stop(StopReason::Indeterminate)
            }
            GitStop::Indeterminate { applied: true } => {
                let outcome = self.apply_git_batch(effects, wrap);
                assert_eq!(
                    outcome,
                    EffectOutcome::Acknowledged,
                    "the selected fully-applied world requires the whole atomic batch to apply"
                );
                self.stop(StopReason::Indeterminate)
            }
        }
    }

    /// Runs an independently planned publisher at one reached production
    /// boundary. Both publishers consume their real plans through the same
    /// driver implementation; the schedule never extracts or replays effects.
    pub(super) async fn run_competition_at(&mut self, boundary: CompetitionBoundary) -> Result<()> {
        let Some(competition) = self.competition.take() else {
            return Ok(());
        };
        let (plan, interruption) = match competition {
            Competition::Complete(report) => {
                self.competition = Some(Competition::Complete(report));
                return Ok(());
            }
            Competition::Pending { boundary: scheduled, plan, interruption }
                if scheduled == boundary =>
            {
                (plan, interruption)
            }
            pending @ Competition::Pending { .. } => {
                self.competition = Some(pending);
                return Ok(());
            }
        };
        if matches!(boundary, CompetitionBoundary::BetweenAliases { .. }) {
            self.trace.alias_competitions.push(boundary);
        }
        let mut driver = WorldDriver::new(self.world, interruption);
        let result = Box::pin(plan.execute_with(&mut driver)).await;
        let report = driver.finish(result)?.into_solo();
        self.competition = Some(Competition::Complete(report));
        Ok(())
    }

    pub(super) fn finish_attempt(&self, result: Result<()>) -> Result<AttemptOutcome> {
        if self.interruption.is_some() {
            assert!(self.interruption_consumed, "the configured interruption was not reached");
        }
        let outcome = match (result, self.failure) {
            (Ok(()), None) => AttemptOutcome::Acknowledged,
            (Err(_), Some(outcome)) => AttemptOutcome::Stopped(outcome),
            (Err(error), None) => return Err(error),
            (Ok(()), Some(_)) => panic!("a stopped driver cannot release a later stage"),
        };
        Ok(outcome)
    }

    pub(super) fn finish(self, result: Result<()>) -> Result<DriverReport> {
        let outcome = self.finish_attempt(result)?;
        let primary = AttemptReport { outcome, trace: self.trace };
        Ok(match self.competition {
            None => DriverReport::Solo(primary),
            Some(Competition::Complete(competitor)) => {
                DriverReport::Competition { primary, competitor }
            }
            Some(Competition::Pending { .. }) => {
                panic!("the configured competition boundary was not reached")
            }
        })
    }
}

pub(super) async fn execute_with_competition(
    world: &mut DurableWorld,
    primary: PlannedPublication,
    competing: PlannedPublication,
    boundary: CompetitionBoundary,
    competing_interruption: Option<Interruption>,
) -> Result<(AttemptReport, AttemptReport)> {
    let mut driver =
        WorldDriver::with_competition(world, boundary, competing, competing_interruption);
    let result = primary.execute_with(&mut driver).await;
    Ok(driver.finish(result)?.into_competition())
}

pub(super) fn validated_aliases(aliases: &[usize], batch_len: usize) -> HashSet<usize> {
    let selected = aliases.iter().copied().collect::<HashSet<_>>();
    assert_eq!(selected.len(), aliases.len(), "an interruption cannot repeat an alias");
    assert!(selected.iter().all(|index| *index < batch_len), "interrupted alias is in range");
    selected
}

impl EffectDriver for WorldDriver<'_> {
    async fn convert_pull_requests_to_draft(
        &mut self,
        conversions: PreparedDraftConversions,
    ) -> Result<()> {
        let batch_count = conversions.batches_for_test().len();
        for (index, batch) in conversions.batches_for_test().enumerate() {
            assert!(
                batch.iter().all(|operation| self.world.marker_authorizes_draft(operation)),
                "a planned draft conversion requires exact immutable-marker authority"
            );
            let batch = batch
                .iter()
                .cloned()
                .map(|operation| self.world.resolve_draft(operation))
                .collect::<Box<[_]>>();
            self.trace.draft_conversions.push(batch.clone());
            let interruption = self.take_interruption(EffectStage::Draft, index);
            let selected = match &interruption {
                Some(Interruption::Draft { applied_aliases, .. }) => {
                    Some(validated_aliases(applied_aliases, batch.len()))
                }
                _ => None,
            };
            let mut exact = true;
            for (alias, operation) in batch.iter().enumerate() {
                if selected.as_ref().is_some_and(|selected| !selected.contains(&alias)) {
                    continue;
                }
                exact &= self.apply_effect(ExternalEffect::Draft(operation.clone()))
                    == EffectOutcome::Acknowledged;
                if alias + 1 < batch.len() {
                    self.run_competition_at(CompetitionBoundary::BetweenAliases {
                        stage: EffectStage::Draft,
                        batch: index,
                        completed_alias: alias,
                    })
                    .await?;
                }
            }
            for operation in &batch {
                self.assert_target_draft_safety(ExternalEffect::Draft(operation.clone()).target());
            }
            if interruption.is_some() || !exact {
                // Mutation aliases are independent, but an incomplete or
                // mismatched response cannot release the Git continuation.
                return self.stop(StopReason::Indeterminate);
            }
            if index + 1 < batch_count {
                self.run_competition_at(CompetitionBoundary::BetweenBatches {
                    stage: EffectStage::Draft,
                    completed_batch: index,
                })
                .await?;
            }
        }
        self.run_competition_at(CompetitionBoundary::AfterDraftConversions).await
    }

    async fn publish_initial_refs(&mut self, pushes: PreparedPushes) -> Result<()> {
        let batch_count = pushes.batches().len();
        for (index, batch) in pushes.batches().enumerate() {
            let mut publications = batch.version_objects_for_test();
            let effects = batch
                .semantic_effects_for_test()
                .iter()
                .map(|effect| match effect {
                    TestPushEffect::PublicBranch { branch, expected, desired } => {
                        InitialRefEffect::PublicBranch {
                            branch: PublicBranchName::new(branch.clone())
                                .expect("production emitted a checked public branch"),
                            expected: *expected,
                            desired: *desired,
                        }
                    }
                    TestPushEffect::Tuple { id, expected, desired, version } => {
                        InitialRefEffect::Tuple(TupleEffect {
                            id: id.clone(),
                            expected: expected.map(literal_revision),
                            desired: literal_revision(*desired),
                            version: *version,
                            publication: publications.next().expect("one version object per tuple"),
                        })
                    }
                    TestPushEffect::Marker { .. } => {
                        panic!("the initial-ref stage cannot contain marker effects")
                    }
                })
                .collect::<Box<[_]>>();
            self.trace.initial_refs.push(effects.clone());
            if let Some(Interruption::InitialRefs { stop, .. }) =
                self.take_interruption(EffectStage::InitialRefs, index)
            {
                return self.stop_git_batch(stop, &effects, ExternalEffect::InitialRef);
            }
            let outcome = self.apply_git_batch(&effects, ExternalEffect::InitialRef);
            if outcome != EffectOutcome::Acknowledged {
                return self.stop(outcome.stop_reason());
            }
            if index + 1 < batch_count {
                self.run_competition_at(CompetitionBoundary::BetweenBatches {
                    stage: EffectStage::InitialRefs,
                    completed_batch: index,
                })
                .await?;
            }
        }
        self.run_competition_at(CompetitionBoundary::AfterInitialRefs).await
    }

    async fn create_pull_requests(
        &mut self,
        creates: PreparedCreates,
    ) -> Result<CompleteCreateReceipts> {
        assert_eq!(creates.repository_id_for_test(), REPOSITORY_ID);
        let mut receipts = Vec::with_capacity(creates.operations_for_test().len());
        let batch_count = creates.batches_for_test().len();
        for (index, batch) in creates.batches_for_test().enumerate() {
            let batch = batch.to_vec().into_boxed_slice();
            self.trace.creates.push(batch.clone());
            let interruption = self.take_interruption(EffectStage::Create, index);
            let selected = match &interruption {
                Some(Interruption::Create { applied_aliases, .. }) => {
                    Some(validated_aliases(applied_aliases, batch.len()))
                }
                _ => None,
            };
            let mut exact = true;
            for (alias, create) in batch.iter().enumerate() {
                if selected.as_ref().is_some_and(|selected| !selected.contains(&alias)) {
                    continue;
                }
                let outcome = self.apply_effect(ExternalEffect::Create(create.clone()));
                exact &= outcome == EffectOutcome::Acknowledged;
                if outcome == EffectOutcome::Acknowledged {
                    receipts.push((create.id.clone(), self.world.latest_identity(&create.id)));
                }
                if alias + 1 < batch.len() {
                    self.run_competition_at(CompetitionBoundary::BetweenAliases {
                        stage: EffectStage::Create,
                        batch: index,
                        completed_alias: alias,
                    })
                    .await?;
                }
            }
            for create in &batch {
                self.assert_target_draft_safety(ExternalTarget::Change(create.id.clone()));
            }
            if interruption.is_some() || !exact {
                // A GraphQL response with any missing, duplicate, or mismatched
                // alias is indeterminate even when some siblings durably land.
                return self.stop(StopReason::Indeterminate);
            }
            if index + 1 < batch_count {
                self.run_competition_at(CompetitionBoundary::BetweenBatches {
                    stage: EffectStage::Create,
                    completed_batch: index,
                })
                .await?;
            }
        }
        let receipts = CompleteCreateReceipts::for_plan_test(receipts);
        self.run_competition_at(CompetitionBoundary::AfterCreates).await?;
        Ok(receipts)
    }

    async fn publish_markers(&mut self, markers: Box<[MarkerTemplate]>) -> Result<()> {
        let prepared = markers
            .into_vec()
            .into_iter()
            .map(|template| {
                let (id, v1, number) = template.test_parts();
                let id = id.clone();
                let identity = marker_identity(v1, number);
                let marker = template.prepare()?;
                let effect = MarkerEffect { id, marker: identity, object: marker.tag() };
                Ok((marker, effect))
            })
            .collect::<Result<Vec<_>>>()?;
        let transitions = prepared
            .iter()
            .map(|(marker, _)| MarkerTransition::from_prepared(marker))
            .collect::<Vec<_>>();
        let destination = PushDestination::for_test();
        let pushes = prepare_marker_pushes(&destination, &transitions)?;
        let batch_count = pushes.batches().len();
        for (index, batch) in pushes.batches().enumerate() {
            let effects = batch
                .semantic_effects_for_test()
                .iter()
                .map(|effect| match effect {
                    TestPushEffect::Marker { id, target } => prepared
                        .iter()
                        .find(|(marker, _)| marker.id() == id && marker.tag() == *target)
                        .map(|(_, effect)| effect.clone())
                        .expect("each prepared marker push retains its canonical identity"),
                    TestPushEffect::Tuple { .. } => {
                        panic!("the marker stage cannot contain tuple effects")
                    }
                    TestPushEffect::PublicBranch { .. } => {
                        panic!("the marker stage cannot contain public-branch effects")
                    }
                })
                .collect::<Box<[_]>>();
            self.trace.markers.push(effects.clone());
            if let Some(Interruption::Marker { stop, .. }) =
                self.take_interruption(EffectStage::Marker, index)
            {
                return self.stop_git_batch(stop, &effects, ExternalEffect::Marker);
            }
            let outcome = self.apply_git_batch(&effects, ExternalEffect::Marker);
            if outcome != EffectOutcome::Acknowledged {
                return self.stop(outcome.stop_reason());
            }
            if index + 1 < batch_count {
                self.run_competition_at(CompetitionBoundary::BetweenBatches {
                    stage: EffectStage::Marker,
                    completed_batch: index,
                })
                .await?;
            }
        }
        self.run_competition_at(CompetitionBoundary::AfterMarkers).await
    }

    async fn project_pull_requests(
        &mut self,
        projection: PreparedPullRequestProjection,
    ) -> Result<()> {
        let batch_count = projection.batches_for_test().len();
        for (index, batch) in projection.batches_for_test().enumerate() {
            let batch = batch
                .iter()
                .cloned()
                .map(|operation| self.world.resolve_projection(operation))
                .collect::<Box<[_]>>();
            assert!(
                batch.iter().all(|effect| {
                    let (resolved_id, identity, canonical) = match effect {
                        ProjectionEffect::Close { resolved_id, identity } => {
                            (resolved_id, identity, false)
                        }
                        ProjectionEffect::Update(UpdateEffect { resolved_id, operation }) => {
                            (resolved_id, &operation.identity, true)
                        }
                    };
                    resolved_id.as_ref().is_some_and(|id| {
                        self.world.published(id).and_then(|change| change.marker).is_some_and(
                            |marker| (identity.number() == marker.number()) == canonical,
                        )
                    })
                }),
                "projection authority must match the durable marker identity"
            );
            self.trace.projections.push(batch.clone());
            let interruption = self.take_interruption(EffectStage::Projection, index);
            let selected = match &interruption {
                Some(Interruption::Projection { applied_aliases, .. }) => {
                    Some(validated_aliases(applied_aliases, batch.len()))
                }
                _ => None,
            };
            let mut exact = true;
            for (alias, operation) in batch.iter().enumerate() {
                if selected.as_ref().is_some_and(|selected| !selected.contains(&alias)) {
                    continue;
                }
                exact &= self.apply_effect(ExternalEffect::Projection(operation.clone()))
                    == EffectOutcome::Acknowledged;
                if alias + 1 < batch.len() {
                    self.run_competition_at(CompetitionBoundary::BetweenAliases {
                        stage: EffectStage::Projection,
                        batch: index,
                        completed_alias: alias,
                    })
                    .await?;
                }
            }
            for operation in &batch {
                self.assert_target_draft_safety(
                    ExternalEffect::Projection(operation.clone()).target(),
                );
            }
            if interruption.is_some() || !exact {
                return self.stop(StopReason::Indeterminate);
            }
            if index + 1 < batch_count {
                self.run_competition_at(CompetitionBoundary::BetweenBatches {
                    stage: EffectStage::Projection,
                    completed_batch: index,
                })
                .await?;
            }
        }
        Ok(())
    }
}

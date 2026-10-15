use super::*;

#[tokio::test]
async fn every_small_attempt_prefix_restarts_from_literal_durable_state() {
    let intent = two_change_intent();
    let initial = DurableWorld::for_intents(oid(10), &[&intent]);
    let mut completed = initial.clone();
    let report = completed.run_attempt(&intent, &QueryVisibility::default(), None).await.unwrap();
    assert_attempt_outcome!(report, AttemptOutcome::Acknowledged);
    assert_eq!(report.trace.initial_refs.iter().map(|batch| batch.len()).collect::<Vec<_>>(), [2]);
    assert_eq!(report.trace.creates.iter().map(|batch| batch.len()).collect::<Vec<_>>(), [2]);
    assert_eq!(report.trace.markers.iter().map(|batch| batch.len()).collect::<Vec<_>>(), [2]);
    assert_eq!(report.trace.projections.iter().map(|batch| batch.len()).collect::<Vec<_>>(), [2]);
    assert_model_snapshot!("two_change_provisional_creates", create_snapshot(&report.trace));
    assert_model_snapshot!("two_change_final_updates", update_snapshot(&report.trace));

    let interruptions = [
        Interruption::InitialRefs { batch: 0, stop: GitStop::Rejected },
        Interruption::InitialRefs { batch: 0, stop: GitStop::Indeterminate { applied: false } },
        Interruption::InitialRefs { batch: 0, stop: GitStop::Indeterminate { applied: true } },
        Interruption::Create { batch: 0, applied_aliases: Box::new([]) },
        Interruption::Create { batch: 0, applied_aliases: Box::new([0]) },
        Interruption::Create { batch: 0, applied_aliases: Box::new([1]) },
        Interruption::Create { batch: 0, applied_aliases: Box::new([0, 1]) },
        Interruption::Marker { batch: 0, stop: GitStop::Rejected },
        Interruption::Marker { batch: 0, stop: GitStop::Indeterminate { applied: false } },
        Interruption::Marker { batch: 0, stop: GitStop::Indeterminate { applied: true } },
        Interruption::Projection { batch: 0, applied_aliases: Box::new([]) },
        Interruption::Projection { batch: 0, applied_aliases: Box::new([0]) },
        Interruption::Projection { batch: 0, applied_aliases: Box::new([1]) },
        Interruption::Projection { batch: 0, applied_aliases: Box::new([0, 1]) },
    ];
    for (index, interruption) in interruptions.into_iter().enumerate() {
        let expected_stop = match &interruption {
            Interruption::InitialRefs { stop: GitStop::Rejected, .. }
            | Interruption::Marker { stop: GitStop::Rejected, .. } => StopReason::Rejected,
            _ => StopReason::Indeterminate,
        };
        let mut interrupted = initial.clone();
        let stopped = interrupted
            .run_attempt(&intent, &QueryVisibility::default(), Some(interruption.clone()))
            .await
            .unwrap();
        assert_attempt_outcome!(stopped, AttemptOutcome::Stopped(expected_stop));
        if let Interruption::Create { batch, applied_aliases } = &interruption {
            assert!(interrupted.changes.values().all(|change| { change.marker.is_none() }));
            assert!(stopped.trace.markers.is_empty() && stopped.trace.projections.is_empty());
            assert_interrupted_create_state(
                &interrupted,
                &intent,
                &stopped.trace.creates[*batch],
                applied_aliases,
            );
        }
        let label = format!("prefix-{index}");
        let retry = run_acknowledged_retry(&mut interrupted, &intent, &label).await;
        if let Interruption::Create { batch, applied_aliases } = &interruption {
            assert!(retry.trace.initial_refs.is_empty());
            assert_eq!(
                flatten(&retry.trace.creates),
                unapplied_request_suffix(&report.trace.creates, *batch, applied_aliases),
                "{label}: retry must create exactly the aliases which are not durable"
            );
            assert_create_retry_finishes_projection(&retry, &interrupted, &intent, &label);
        }
        if let Interruption::Projection { batch, applied_aliases } = &interruption {
            assert!(retry.trace.initial_refs.is_empty());
            assert!(retry.trace.creates.is_empty());
            assert!(retry.trace.markers.is_empty());
            assert_eq!(
                flatten(&retry.trace.projections),
                unapplied_request_suffix(&report.trace.projections, *batch, applied_aliases),
                "{label}: retry must omit exactly the update aliases already durable"
            );
        }
        assert_quiescent(&mut interrupted, &intent, &label).await;
    }

    let done = completed.run_attempt(&intent, &QueryVisibility::default(), None).await.unwrap();
    assert!(done.trace.is_empty());
}

#[tokio::test]
async fn multibatch_draft_interruption_replans_only_remaining_conversions_before_git() {
    let intent = many_bounded_ids_intent(66, 16);
    let mut initial = DurableWorld::for_intents(oid(1), &[&intent]);
    for local in intent.iter() {
        initial.publish_for_setup(&local.id, local.desired);
        initial.open_for_setup(&local.id, &local.title, "stale body");
        initial.mark_ready_on_default_for_setup(&local.id, local.desired.head);
    }
    let mut completed = initial.clone();
    let complete = completed.run_attempt(&intent, &QueryVisibility::default(), None).await.unwrap();
    assert_attempt_outcome!(complete, AttemptOutcome::Acknowledged);
    assert_eq!(
        complete.trace.draft_conversions.iter().map(|batch| batch.len()).collect::<Vec<_>>(),
        [64, 1]
    );

    let interruptions = [
        (0, Vec::new()),
        (0, vec![0, 17, 63]),
        (0, (0..64).collect()),
        (1, Vec::new()),
        (1, vec![0]),
    ];
    for (batch, applied_aliases) in interruptions {
        let mut world = initial.clone();

        let stopped = world
            .run_attempt(
                &intent,
                &QueryVisibility::default(),
                Some(Interruption::Draft {
                    batch,
                    applied_aliases: applied_aliases.clone().into_boxed_slice(),
                }),
            )
            .await
            .unwrap();
        assert_attempt_outcome!(stopped, AttemptOutcome::Stopped(StopReason::Indeterminate));
        assert_eq!(stopped.trace.draft_conversions, complete.trace.draft_conversions[..=batch]);
        assert!(stopped.trace.initial_refs.is_empty());
        assert!(stopped.trace.creates.is_empty());
        assert!(stopped.trace.markers.is_empty());
        assert!(stopped.trace.projections.is_empty());

        let label = format!("draft batch {batch} retry after {applied_aliases:?}");
        let retry = run_acknowledged_retry(&mut world, &intent, &label).await;
        assert_eq!(
            flatten(&retry.trace.draft_conversions),
            unapplied_request_suffix(&complete.trace.draft_conversions, batch, &applied_aliases),
            "{label}: retry must convert exactly the aliases not known durable"
        );
        assert!(retry.trace.initial_refs.is_empty());
        assert_eq!(flatten(&retry.trace.projections).len(), 66);
        assert_quiescent(&mut world, &intent, &label).await;
    }
}

fn ready_departing_child_world(intent: &LocalIntent) -> DurableWorld {
    let mut world = DurableWorld::for_intents(oid(10), &[intent]);
    let root = &intent.first;
    world.publish_for_setup(&root.id, root.desired);
    world.open_for_setup(&root.id, &root.title, "root body");
    world.mark_for_setup(&root.id, root.desired.head, BaseKind::Default);

    let child = &intent.later[0];
    let old_child = LiteralRevision { head: oid(30), first_parent: root.desired.head };
    world.publish_for_setup(&child.id, old_child);
    world.open_for_setup(&child.id, &child.title, "child body");
    world.mark_ready_on_default_for_setup(&child.id, old_child.head);

    world
}

#[tokio::test]
async fn stale_identical_draft_conversion_with_lost_receipt_stops_before_git() {
    let intent = two_change_intent();
    for applied_aliases in [Vec::new(), vec![0]] {
        let mut world = ready_departing_child_world(&intent);
        let primary = world.plan(&intent, &QueryVisibility::default()).unwrap();
        let stale = world.plan(&intent, &QueryVisibility::default()).unwrap();
        let (winner, loser) = execute_with_competition(
            &mut world,
            primary,
            stale,
            CompetitionBoundary::AfterDraftConversions,
            Some(Interruption::Draft {
                batch: 0,
                applied_aliases: applied_aliases.into_boxed_slice(),
            }),
        )
        .await
        .unwrap();
        assert_attempt_outcome!(winner, AttemptOutcome::Acknowledged);
        assert_attempt_outcome!(loser, AttemptOutcome::Stopped(StopReason::Indeterminate));
        assert_eq!(flatten(&winner.trace.draft_conversions).len(), 1);
        assert_eq!(flatten(&winner.trace.initial_refs).len(), 1);
        assert_eq!(flatten(&loser.trace.draft_conversions).len(), 1);
        assert!(loser.trace.initial_refs.is_empty());
        assert!(loser.trace.creates.is_empty());
        assert!(loser.trace.markers.is_empty());
        assert!(loser.trace.projections.is_empty());
        assert_quiescent(&mut world, &intent, "stale lost draft receipt").await;
    }
}

#[tokio::test]
async fn exact_already_draft_receipt_releases_stale_publisher_to_cas_convergence() {
    let intent = two_change_intent();
    let mut world = ready_departing_child_world(&intent);
    let primary = world.plan(&intent, &QueryVisibility::default()).unwrap();
    let stale = world.plan(&intent, &QueryVisibility::default()).unwrap();
    let (primary, stale) = execute_with_competition(
        &mut world,
        primary,
        stale,
        CompetitionBoundary::AfterDraftConversions,
        None,
    )
    .await
    .unwrap();
    assert_attempt_outcome!(stale, AttemptOutcome::Acknowledged);
    assert_attempt_outcome!(primary, AttemptOutcome::Stopped(StopReason::Rejected));
    assert_eq!(flatten(&stale.trace.draft_conversions).len(), 1);
    assert_eq!(flatten(&stale.trace.initial_refs).len(), 1);
    assert!(primary.trace.projections.is_empty());
    assert_quiescent(&mut world, &intent, "exact stale draft conversion").await;
}

#[tokio::test]
async fn git_restarts_expose_only_complete_atomic_batch_prefixes() {
    let tuple_intent = many_bounded_ids_intent(30, 128);
    let tuple_initial = DurableWorld::for_intents(oid(1), &[&tuple_intent]);
    let mut tuple_complete = tuple_initial.clone();
    let tuple_report =
        tuple_complete.run_attempt(&tuple_intent, &QueryVisibility::default(), None).await.unwrap();
    assert!(
        tuple_report.trace.initial_refs.len() > 1,
        "the fixture crosses the initial-ref push budget"
    );
    for stopped_batch in 0..tuple_report.trace.initial_refs.len() {
        for applied in [false, true] {
            let mut world = tuple_initial.clone();
            let stopped = world
                .run_attempt(
                    &tuple_intent,
                    &QueryVisibility::default(),
                    Some(Interruption::InitialRefs {
                        batch: stopped_batch,
                        stop: GitStop::Indeterminate { applied },
                    }),
                )
                .await
                .unwrap();
            assert_eq!(
                initial_shape(&stopped.trace.initial_refs),
                initial_shape(&tuple_report.trace.initial_refs[..=stopped_batch])
            );
            let label = format!("initial-ref-batch-{stopped_batch}-applied-{applied}");
            let retry = run_acknowledged_retry(&mut world, &tuple_intent, &label).await;
            assert_eq!(
                submitted_shape(&flatten(&retry.trace.initial_refs)),
                submitted_shape(&flatten(
                    &tuple_report.trace.initial_refs[stopped_batch + usize::from(applied)..]
                )),
                "an initial-ref retry retains the exact unpublished atomic-batch suffix"
            );
            assert_complete_create_stage(&retry, &world, &tuple_intent, &label);
            assert_create_retry_finishes_projection(&retry, &world, &tuple_intent, &label);
            assert_quiescent(&mut world, &tuple_intent, &label).await;
        }
    }

    let marker_intent = many_bounded_ids_intent(90, 128);
    let mut before_markers = DurableWorld::for_intents(oid(1), &[&marker_intent]);
    for local in marker_intent.iter() {
        before_markers.publish_for_setup(&local.id, local.desired);
        before_markers.open_for_setup(&local.id, &local.title, "provisional");
    }
    let mut marker_complete = before_markers.clone();
    let marker_report = marker_complete
        .run_attempt(&marker_intent, &QueryVisibility::default(), None)
        .await
        .unwrap();
    assert!(marker_report.trace.initial_refs.is_empty());
    assert!(marker_report.trace.creates.is_empty());
    assert!(marker_report.trace.markers.len() > 1, "the fixture crosses the marker push budget");
    for stopped_batch in 0..marker_report.trace.markers.len() {
        for applied in [false, true] {
            let mut world = before_markers.clone();
            let stopped = world
                .run_attempt(
                    &marker_intent,
                    &QueryVisibility::default(),
                    Some(Interruption::Marker {
                        batch: stopped_batch,
                        stop: GitStop::Indeterminate { applied },
                    }),
                )
                .await
                .unwrap();
            assert_eq!(stopped.trace.markers, marker_report.trace.markers[..=stopped_batch]);
            let label = format!("marker-batch-{stopped_batch}-applied-{applied}");
            let retry = run_acknowledged_retry(&mut world, &marker_intent, &label).await;
            assert!(retry.trace.initial_refs.is_empty());
            assert!(retry.trace.creates.is_empty());
            assert_eq!(
                flatten(&retry.trace.markers),
                flatten(&marker_report.trace.markers[stopped_batch + usize::from(applied)..]),
                "a marker retry retains the exact unpublished atomic-batch suffix"
            );
            assert_retry_finishes_projection(&retry, &world, &marker_intent, &label);
            assert_quiescent(&mut world, &marker_intent, &label).await;
        }
    }

    let mixed = LocalIntent::new(
        oid(1),
        [
            local_change(
                "GleaseA",
                LiteralRevision { head: oid(300), first_parent: oid(1) },
                "A",
                "A",
            ),
            local_change(
                "GleaseB",
                LiteralRevision { head: oid(301), first_parent: oid(300) },
                "B",
                "B",
            ),
            local_change(
                "GleaseC",
                LiteralRevision { head: oid(302), first_parent: oid(301) },
                "C",
                "C",
            ),
        ],
    );
    let initial = DurableWorld::for_intents(oid(1), &[&mixed]);
    let stale_plan = initial.plan(&mixed, &QueryVisibility::default()).unwrap();
    let mut raced = initial;
    let conflicting = LiteralRevision { head: oid(399), first_parent: oid(300) };
    raced.publish_for_setup(&id("GleaseB"), conflicting);
    let before = raced.clone();
    let rejected = raced.execute_plan(stale_plan, None).await.unwrap();
    assert_attempt_outcome!(rejected, AttemptOutcome::Stopped(StopReason::Rejected));
    assert_eq!(raced, before, "one conflicting lease rolls back the whole mixed Git batch");
    assert!(raced.published(&id("GleaseA")).is_none());
    assert!(raced.published(&id("GleaseC")).is_none());
}

#[tokio::test]
async fn known_public_batch_rejection_retains_only_earlier_atomic_batches() {
    let intent = public_multibatch_intent();
    let initial = DurableWorld::for_intents(oid(1), &[&intent]);
    let mut completed = initial.clone();
    let complete = completed.run_attempt(&intent, &QueryVisibility::default(), None).await.unwrap();
    assert_attempt_outcome!(complete, AttemptOutcome::Acknowledged);
    let (final_batch_index, final_batch) = mixed_final_batch(&complete.trace);
    let final_batch = final_batch.to_vec().into_boxed_slice();

    let mut rejected_world = initial.clone();
    let rejected = rejected_world
        .run_attempt(
            &intent,
            &QueryVisibility::default(),
            Some(Interruption::InitialRefs { batch: final_batch_index, stop: GitStop::Rejected }),
        )
        .await
        .unwrap();
    assert_attempt_outcome!(rejected, AttemptOutcome::Stopped(StopReason::Rejected));
    assert_eq!(
        initial_shape(&rejected.trace.initial_refs),
        initial_shape(&complete.trace.initial_refs)
    );
    assert!(
        rejected.trace.creates.is_empty()
            && rejected.trace.markers.is_empty()
            && rejected.trace.projections.is_empty()
    );
    assert_tuple_effects_published(
        &rejected_world,
        &flatten(&complete.trace.initial_refs[..final_batch_index]),
    );
    for effect in &final_batch {
        match effect {
            InitialRefEffect::Tuple(tuple) => {
                assert!(rejected_world.published(&tuple.id).is_none())
            }
            InitialRefEffect::PublicBranch { branch, .. } => {
                assert_eq!(branch.as_str(), PUBLIC_BRANCH);
                assert!(rejected_world.public_branch_target(branch).is_none());
            }
        }
    }

    let retry =
        rejected_world.run_attempt(&intent, &QueryVisibility::default(), None).await.unwrap();
    assert_attempt_outcome!(retry, AttemptOutcome::Acknowledged);
    assert_eq!(
        initial_shape(&retry.trace.initial_refs),
        initial_shape(std::slice::from_ref(&final_batch))
    );
    assert_eq!(
        public_branch_target(&rejected_world),
        Some(intent.later.last().unwrap().desired.head)
    );
    assert!(
        rejected_world
            .run_attempt(&intent, &QueryVisibility::default(), None)
            .await
            .unwrap()
            .trace
            .is_empty()
    );
}

#[tokio::test]
async fn lost_public_batch_acknowledgement_replans_no_initial_refs() {
    let intent = public_multibatch_intent();
    let initial = DurableWorld::for_intents(oid(1), &[&intent]);
    let mut completed = initial.clone();
    let complete = completed.run_attempt(&intent, &QueryVisibility::default(), None).await.unwrap();
    let (final_batch_index, _) = mixed_final_batch(&complete.trace);
    let mut lost_world = initial.clone();
    let lost = lost_world
        .run_attempt(
            &intent,
            &QueryVisibility::default(),
            Some(Interruption::InitialRefs {
                batch: final_batch_index,
                stop: GitStop::Indeterminate { applied: true },
            }),
        )
        .await
        .unwrap();
    assert_attempt_outcome!(lost, AttemptOutcome::Stopped(StopReason::Indeterminate));
    assert_eq!(
        initial_shape(&lost.trace.initial_refs),
        initial_shape(&complete.trace.initial_refs)
    );
    assert_eq!(public_branch_target(&lost_world), Some(intent.later.last().unwrap().desired.head));
    assert!(intent.iter().all(|local| {
        lost_world.published(&local.id).is_some_and(|published| {
            published.history.last() == local.desired && published.pull_requests.is_empty()
        })
    }));
    let lost_retry =
        lost_world.run_attempt(&intent, &QueryVisibility::default(), None).await.unwrap();
    assert_attempt_outcome!(lost_retry, AttemptOutcome::Acknowledged);
    assert!(
        lost_retry.trace.initial_refs.is_empty(),
        "fresh observation skips every initial ref whose lost request landed"
    );
    assert!(
        lost_world
            .run_attempt(&intent, &QueryVisibility::default(), None)
            .await
            .unwrap()
            .trace
            .is_empty()
    );
}

#[tokio::test]
async fn stale_public_lease_rolls_back_its_sibling_tuples() {
    let intent = public_multibatch_intent();
    let initial = DurableWorld::for_intents(oid(1), &[&intent]);
    let mut raced_world = initial;
    raced_world.set_public_branch(&public_branch_name(PUBLIC_BRANCH), oid(900));
    raced_world.assert_well_formed();
    let stale_plan = raced_world.plan(&intent, &QueryVisibility::default()).unwrap();
    let mut uncontended = raced_world.clone();
    let expected =
        uncontended.run_attempt(&intent, &QueryVisibility::default(), None).await.unwrap();
    let expected_final_index = expected.trace.initial_refs.len() - 1;
    let expected_final = &expected.trace.initial_refs[expected_final_index];
    assert!(expected_final.iter().any(|effect| matches!(effect, InitialRefEffect::Tuple(_))));
    assert!(expected_final.iter().any(|effect| {
        matches!(
            effect,
            InitialRefEffect::PublicBranch { expected: Some(value), .. } if *value == oid(900)
        )
    }));

    raced_world.set_public_branch(&public_branch_name(PUBLIC_BRANCH), oid(901));
    let stale = raced_world.execute_plan(stale_plan, None).await.unwrap();
    assert_attempt_outcome!(stale, AttemptOutcome::Stopped(StopReason::Rejected));
    assert_eq!(
        public_branch_target(&raced_world),
        Some(oid(901)),
        "a stale public lease preserves the competing branch value"
    );
    for effect in expected_final {
        if let InitialRefEffect::Tuple(tuple) = effect {
            assert!(
                raced_world.published(&tuple.id).is_none(),
                "the stale public lease rolls back sibling tuples in its atomic batch"
            );
        }
    }
    assert_tuple_effects_published(
        &raced_world,
        &flatten(&expected.trace.initial_refs[..expected_final_index]),
    );

    let raced_retry =
        raced_world.run_attempt(&intent, &QueryVisibility::default(), None).await.unwrap();
    assert_attempt_outcome!(raced_retry, AttemptOutcome::Acknowledged);
    assert_eq!(raced_retry.trace.initial_refs.len(), 1);
    let expected_retry = expected_public_retry_batch(expected_final, oid(901));
    assert_eq!(
        submitted_shape(&raced_retry.trace.initial_refs[0]),
        submitted_shape(&expected_retry)
    );
    assert_eq!(public_branch_target(&raced_world), Some(intent.later.last().unwrap().desired.head));
    assert!(
        raced_world
            .run_attempt(&intent, &QueryVisibility::default(), None)
            .await
            .unwrap()
            .trace
            .is_empty()
    );
}

#[tokio::test]
#[should_panic(expected = "selected fully-applied world requires the whole atomic batch")]
async fn indeterminate_applied_world_cannot_select_a_rejected_batch() {
    let intent = root_intent(
        oid(1),
        "Gappliedworld",
        LiteralRevision { head: oid(980), first_parent: oid(1) },
    )
    .with_public_branch(PUBLIC_BRANCH);
    let mut world = DurableWorld::for_intents(oid(1), &[&intent]);
    world.set_public_branch(&public_branch_name(PUBLIC_BRANCH), oid(900));
    let stale_plan = world.plan(&intent, &QueryVisibility::default()).unwrap();
    world.set_public_branch(&public_branch_name(PUBLIC_BRANCH), oid(901));
    let _ = world
        .execute_plan(
            stale_plan,
            Some(Interruption::InitialRefs {
                batch: 0,
                stop: GitStop::Indeterminate { applied: true },
            }),
        )
        .await;
}

#[tokio::test]
async fn competing_public_publisher_wins_between_primary_initial_ref_batches() {
    let primary_intent = public_multibatch_intent();
    let competitor_intent = root_intent(
        oid(1),
        "Gpubliccompetitor",
        LiteralRevision { head: oid(975), first_parent: oid(1) },
    )
    .with_public_branch(PUBLIC_BRANCH);
    let competitor_tip = competitor_intent.first.desired.head;
    let mut initial = DurableWorld::for_intents(oid(1), &[&primary_intent, &competitor_intent]);
    initial.set_public_branch(&public_branch_name(PUBLIC_BRANCH), oid(900));
    initial.assert_well_formed();

    let primary_plan = initial.plan(&primary_intent, &QueryVisibility::default()).unwrap();
    let competitor_plan = initial.plan(&competitor_intent, &QueryVisibility::default()).unwrap();
    let mut reference_world = initial.clone();
    let reference = reference_world
        .run_attempt(&primary_intent, &QueryVisibility::default(), None)
        .await
        .unwrap();
    let (final_index, final_batch) = mixed_final_batch(&reference.trace);
    assert!(final_batch.iter().any(|effect| {
        matches!(
            effect,
            InitialRefEffect::PublicBranch { expected: Some(value), .. } if *value == oid(900)
        )
    }));

    let mut world = initial;
    let (primary, competitor) = execute_with_competition(
        &mut world,
        primary_plan,
        competitor_plan,
        CompetitionBoundary::BetweenBatches { stage: EffectStage::InitialRefs, completed_batch: 0 },
        None,
    )
    .await
    .unwrap();
    assert_attempt_outcome!(competitor, AttemptOutcome::Acknowledged);
    assert_eq!(competitor.trace.initial_refs.len(), 1);
    assert_eq!(flatten(&competitor.trace.creates).len(), 1);
    assert_eq!(flatten(&competitor.trace.markers).len(), 1);
    assert_eq!(flatten(&competitor.trace.projections).len(), 1);
    assert_attempt_outcome!(primary, AttemptOutcome::Stopped(StopReason::Rejected));
    assert_eq!(
        initial_shape(&primary.trace.initial_refs),
        initial_shape(&reference.trace.initial_refs)
    );
    assert!(
        primary.trace.creates.is_empty()
            && primary.trace.markers.is_empty()
            && primary.trace.projections.is_empty(),
        "the stale public lease cannot release any primary GitHub stage"
    );
    assert_eq!(public_branch_target(&world), Some(competitor_tip));
    assert_tuple_effects_published(&world, &flatten(&reference.trace.initial_refs[..final_index]));
    for effect in final_batch {
        if let InitialRefEffect::Tuple(tuple) = effect {
            assert!(
                world.published(&tuple.id).is_none(),
                "the stale public lease rolls back sibling primary tuples"
            );
        }
    }

    let competitor_change = world.published(&competitor_intent.first.id).cloned().unwrap();
    let retry =
        world.run_attempt(&primary_intent, &QueryVisibility::default(), None).await.unwrap();
    assert_attempt_outcome!(retry, AttemptOutcome::Acknowledged);
    assert_eq!(retry.trace.initial_refs.len(), 1);
    assert_eq!(
        submitted_shape(&retry.trace.initial_refs[0]),
        submitted_shape(&expected_public_retry_batch(final_batch, competitor_tip))
    );
    assert_eq!(
        public_branch_target(&world),
        Some(primary_intent.later.last().unwrap().desired.head)
    );
    assert_eq!(world.published(&competitor_intent.first.id), Some(&competitor_change));
    assert!(
        world
            .run_attempt(&primary_intent, &QueryVisibility::default(), None)
            .await
            .unwrap()
            .trace
            .is_empty()
    );
}

#[tokio::test]
async fn preplanned_public_effects_accept_already_desired_receipts() {
    let duplicate_intent = root_intent(
        oid(1),
        "Gpublicduplicate",
        LiteralRevision { head: oid(950), first_parent: oid(1) },
    )
    .with_public_branch(PUBLIC_BRANCH);
    let duplicate_initial = DurableWorld::for_intents(oid(1), &[&duplicate_intent]);
    let first_plan =
        duplicate_initial.plan(&duplicate_intent, &QueryVisibility::default()).unwrap();
    let duplicate_plan =
        duplicate_initial.plan(&duplicate_intent, &QueryVisibility::default()).unwrap();
    let mut duplicate_world = duplicate_initial;
    let first = duplicate_world
        .execute_plan(
            first_plan,
            Some(Interruption::InitialRefs {
                batch: 0,
                stop: GitStop::Indeterminate { applied: true },
            }),
        )
        .await
        .unwrap();
    assert_attempt_outcome!(first, AttemptOutcome::Stopped(StopReason::Indeterminate));
    let duplicate = duplicate_world.execute_plan(duplicate_plan, None).await.unwrap();
    assert_attempt_outcome!(duplicate, AttemptOutcome::Stopped(StopReason::Rejected));
    assert!(duplicate.trace.creates.is_empty());
    run_acknowledged_retry(&mut duplicate_world, &duplicate_intent, "fresh-after-unique-fence")
        .await;
    assert_quiescent(&mut duplicate_world, &duplicate_intent, "fresh-after-unique-fence").await;
}

#[tokio::test]
async fn graphql_restarts_stop_after_an_indeterminate_current_request() {
    let intent = multi_request_intent();
    let initial = DurableWorld::for_intents(oid(10), &[&intent]);
    let mut complete = initial.clone();
    let completed = complete.run_attempt(&intent, &QueryVisibility::default(), None).await.unwrap();
    let creates = &completed.trace.creates;
    assert_eq!(
        creates.iter().map(|batch| batch.len()).collect::<Vec<_>>(),
        [1, 1, 1],
        "the fixture uses the production serialized-byte request boundary"
    );
    assert_eq!(
        completed.trace.projections.iter().map(|batch| batch.len()).collect::<Vec<_>>(),
        [1, 1, 1],
        "create and update serialization preserve the same byte boundary"
    );

    let applied_alias_schedules: [&[usize]; 2] = [&[], &[0]];
    for applied_aliases in applied_alias_schedules {
        let label = format!("multi-request-create-applied-{applied_aliases:?}");
        let mut interrupted_create = initial.clone();
        let stopped = interrupted_create
            .run_attempt(
                &intent,
                &QueryVisibility::default(),
                Some(Interruption::Create { batch: 1, applied_aliases: applied_aliases.into() }),
            )
            .await
            .unwrap();
        assert_attempt_outcome!(stopped, AttemptOutcome::Stopped(StopReason::Indeterminate));
        assert_eq!(
            stopped.trace.creates.iter().map(|batch| batch.len()).collect::<Vec<_>>(),
            [1, 1],
            "an earlier request is acknowledged, the current response is lost, and the tail is unsent"
        );
        assert!(stopped.trace.markers.is_empty() && stopped.trace.projections.is_empty());
        assert_interrupted_create_request_prefix(
            &interrupted_create,
            &intent,
            creates,
            1,
            applied_aliases,
        );
        let retry = run_acknowledged_retry(&mut interrupted_create, &intent, &label).await;
        assert!(retry.trace.initial_refs.is_empty());
        assert_eq!(
            flatten(&retry.trace.creates),
            unapplied_request_suffix(creates, 1, applied_aliases),
            "{label}: retry must contain exactly the unapplied create-request suffix"
        );
        assert_create_retry_finishes_projection(&retry, &interrupted_create, &intent, &label);
        assert_quiescent(&mut interrupted_create, &intent, &label).await;
    }

    for applied_aliases in applied_alias_schedules {
        let label = format!("multi-request-update-applied-{applied_aliases:?}");
        let mut before_updates = DurableWorld::for_intents(oid(10), &[&intent]);
        for (index, local) in intent.iter().enumerate() {
            establish_marked(
                &mut before_updates,
                local,
                if index == 0 { BaseKind::Default } else { BaseKind::Owned },
                "provisional",
            );
        }
        let stopped = before_updates
            .run_attempt(
                &intent,
                &QueryVisibility::default(),
                Some(Interruption::Projection {
                    batch: 1,
                    applied_aliases: applied_aliases.into(),
                }),
            )
            .await
            .unwrap();
        assert_eq!(
            stopped.trace.projections.iter().map(|batch| batch.len()).collect::<Vec<_>>(),
            [1, 1]
        );
        let durable_updates =
            only_updates(&applied_request_prefix(&completed.trace.projections, 1, applied_aliases));
        for local in intent.iter() {
            let actual = &before_updates
                .selected_open_pull_request(&local.id)
                .unwrap()
                .open_fields()
                .unwrap()
                .body;
            let expected = durable_updates
                .iter()
                .find(|update| update.resolved_id.as_ref() == Some(&local.id))
                .map(|update| {
                    update.operation.body.as_deref().expect("the fixture changes every body")
                })
                .unwrap_or("provisional");
            assert!(
                actual == expected,
                "{label}: durable body for {} differs from its applied prefix ({} != {} bytes)",
                local.id.as_str(),
                actual.len(),
                expected.len()
            );
        }
        let retry = run_acknowledged_retry(&mut before_updates, &intent, &label).await;
        assert!(retry.trace.initial_refs.is_empty());
        assert!(retry.trace.creates.is_empty());
        assert!(retry.trace.markers.is_empty());
        assert_eq!(
            flatten(&retry.trace.projections),
            unapplied_request_suffix(&completed.trace.projections, 1, applied_aliases),
            "{label}: retry must contain exactly the unapplied update-request suffix"
        );
        assert_final_projection_state(&before_updates, &intent, &label);
        if applied_aliases == [0] {
            let snapshot = MultiRequestUpdateRestartSnapshot {
                before_retry: multi_request_update_snapshot(&stopped.trace),
                retry: multi_request_update_snapshot(&retry.trace),
            };
            assert_model_snapshot!("multi_request_update_restart", snapshot);
        }
        assert_quiescent(&mut before_updates, &intent, &label).await;
    }

    let duplicate_intent = two_change_intent();
    let duplicate_initial = DurableWorld::for_intents(oid(10), &[&duplicate_intent]);
    let stale_plan =
        duplicate_initial.plan(&duplicate_intent, &QueryVisibility::default()).unwrap();
    let mut duplicate_world = duplicate_initial;
    let first = duplicate_world
        .run_attempt(
            &duplicate_intent,
            &QueryVisibility::default(),
            Some(Interruption::Create { batch: 0, applied_aliases: Box::new([0]) }),
        )
        .await
        .unwrap();
    assert_attempt_outcome!(first, AttemptOutcome::Stopped(StopReason::Indeterminate));
    let duplicate_creates = &first.trace.creates;
    let retained_identity = duplicate_world.identity(&duplicate_creates[0][0].id);
    let next_before = duplicate_world.next_identity;
    let before = duplicate_world.clone();
    let duplicate = duplicate_world.execute_plan(stale_plan, None).await.unwrap();
    assert_attempt_outcome!(duplicate, AttemptOutcome::Stopped(StopReason::Rejected));
    assert_eq!(duplicate_world, before);
    assert_eq!(duplicate_world.next_identity, next_before);
    assert_eq!(duplicate_world.identity(&duplicate_creates[0][0].id), retained_identity);
    assert_restart_converges(duplicate_world, &duplicate_intent, "mixed-duplicate-create").await;
}

#[tokio::test]
async fn hidden_open_rows_retry_only_the_unmarked_stable_create_key() {
    let default = oid(1);
    let revision = LiteralRevision { head: oid(2), first_parent: default };
    let intent = root_intent(default, "Ghidden", revision);
    let mut world = DurableWorld::for_intents(default, &[&intent]);
    let first = world
        .run_attempt(
            &intent,
            &QueryVisibility::default(),
            Some(Interruption::Create { batch: 0, applied_aliases: Box::new([0]) }),
        )
        .await
        .unwrap();
    assert_attempt_outcome!(first, AttemptOutcome::Stopped(StopReason::Indeterminate));

    let hidden = QueryVisibility::hiding(&world, [id("Ghidden")]);
    let before = world.clone();
    let rejected = world
        .run_attempt(
            &intent,
            &hidden,
            Some(Interruption::Create { batch: 0, applied_aliases: Box::new([]) }),
        )
        .await
        .unwrap();
    assert_attempt_outcome!(rejected, AttemptOutcome::Stopped(StopReason::Indeterminate));
    assert_eq!(world, before, "a provider rejection retains every row and identity allocation");
    assert!(rejected.trace.markers.is_empty());
    assert!(rejected.trace.projections.is_empty());
    let duplicate = world.run_attempt(&intent, &hidden, None).await.unwrap();
    assert!(duplicate.trace.initial_refs.is_empty());
    assert_eq!(
        duplicate.trace.creates, first.trace.creates,
        "the stable create payload is identical"
    );
    assert_attempt_outcome!(
        duplicate,
        AttemptOutcome::Acknowledged,
        "the literal service accepts a same-key duplicate create"
    );
    assert_eq!(world.open_pull_requests(&id("Ghidden")).count(), 2);
    assert!(world.published(&id("Ghidden")).unwrap().marker.is_some());

    let world = assert_restart_converges(world, &intent, "hidden-unmarked").await;
    let hidden_marked = QueryVisibility::hiding(&world, [id("Ghidden")]);
    let error = world.plan(&intent, &hidden_marked).err().unwrap();
    assert!(error.to_string().contains("marker"));
    assert!(error.to_string().contains("no OPEN pull request"));
}

#[tokio::test]
async fn stale_open_oids_and_body_bytes_do_not_advance_with_durable_writes() {
    let default = oid(1);
    let old_root = LiteralRevision { head: oid(10), first_parent: default };
    let old_child = LiteralRevision { head: oid(20), first_parent: old_root.head };
    let new_root = LiteralRevision { head: oid(11), first_parent: default };
    let new_child = LiteralRevision { head: oid(21), first_parent: new_root.head };
    let old_intent = LocalIntent::new(
        default,
        [
            local_change("Groot", old_root, "Old root", "Old root body"),
            local_change("Gchild", old_child, "Old child", "Old child body"),
        ],
    );
    let new_intent = LocalIntent::new(
        default,
        [
            local_change("Groot", new_root, "Amended root", "New root body"),
            local_change("Gchild", new_child, "Rebased child", "New child body"),
        ],
    );
    let mut world = DurableWorld::for_intents(default, &[&old_intent, &new_intent]);
    for (index, local) in old_intent.iter().enumerate() {
        world.publish_for_setup(&local.id, local.desired);
        world.open_for_setup(&local.id, &local.title, &local.commit_body);
        world.mark_for_setup(
            &local.id,
            local.desired.head,
            if index == 0 { BaseKind::Default } else { BaseKind::Owned },
        );
    }
    let stale = QueryVisibility::stale(&world, [id("Groot"), id("Gchild")]);
    let attempt = world.run_attempt(&new_intent, &stale, None).await.unwrap();
    assert_attempt_outcome!(attempt, AttemptOutcome::Acknowledged);
    assert_eq!(
        flatten(&attempt.trace.initial_refs)
            .iter()
            .map(|effect| {
                let tuple = effect.tuple();
                (tuple.id.clone(), tuple.version)
            })
            .collect::<Vec<_>>(),
        [(id("Groot"), 2), (id("Gchild"), 2)]
    );
    assert!(attempt.trace.markers.is_empty(), "older durable markers remain authoritative");
    assert!(
        update_effects(&attempt.trace).iter().all(|update| update.operation.base_branch.is_none())
    );
    assert_model_snapshot!("stale_amend_rebase_updates", update_snapshot(&attempt.trace));

    let stale_retry = world.run_attempt(&new_intent, &stale, None).await.unwrap();
    assert!(stale_retry.trace.initial_refs.is_empty());
    assert_eq!(
        flatten(&stale_retry.trace.projections),
        flatten(&attempt.trace.projections),
        "neither tuple publication nor updates mutate an already-returned query view"
    );
    assert!(
        world
            .run_attempt(&new_intent, &QueryVisibility::default(), None)
            .await
            .unwrap()
            .trace
            .is_empty(),
        "an explicitly current observation sees convergence"
    );
}

#[tokio::test]
async fn independently_lagged_head_and_base_history_slots_are_safe() {
    let default = oid(1);
    let old_root = LiteralRevision { head: oid(40), first_parent: default };
    let old_child = LiteralRevision { head: oid(41), first_parent: old_root.head };
    let new_root = LiteralRevision { head: oid(42), first_parent: default };
    let new_child = LiteralRevision { head: oid(43), first_parent: new_root.head };
    let old_intent = LocalIntent::new(
        default,
        [
            local_change("Gslotroot", old_root, "Old root", "Old root body"),
            local_change("Gslotchild", old_child, "Old child", "Old child body"),
        ],
    );
    let new_intent = LocalIntent::new(
        default,
        [
            local_change("Gslotroot", new_root, "New root", "New root body"),
            local_change("Gslotchild", new_child, "New child", "New child body"),
        ],
    );
    let mut initial = DurableWorld::for_intents(default, &[&old_intent, &new_intent]);
    for (old, new, base) in [
        (&old_intent.first, &new_intent.first, BaseKind::Default),
        (&old_intent.later[0], &new_intent.later[0], BaseKind::Owned),
    ] {
        initial.publish_for_setup(&old.id, old.desired);
        initial.publish_for_setup(&new.id, new.desired);
        initial.open_for_setup(&old.id, &old.title, "provisional");
        initial.mark_for_setup(&old.id, old.desired.head, base);
    }

    for head_slot in 0..2 {
        for base_slot in 0..2 {
            let visibility =
                QueryVisibility::historical_slots(&initial, id("Gslotchild"), head_slot, base_slot);
            let mut world = initial.clone();
            let report = world.run_attempt(&new_intent, &visibility, None).await.unwrap();
            assert_attempt_outcome!(report, AttemptOutcome::Acknowledged);
            assert!(report.trace.initial_refs.is_empty());
            assert!(report.trace.creates.is_empty());
            assert!(report.trace.markers.is_empty());
            assert_eq!(
                world.published(&id("Gslotchild")).unwrap().history,
                initial.published(&id("Gslotchild")).unwrap().history,
                "query lag cannot create immutable history"
            );
            assert!(
                world
                    .run_attempt(&new_intent, &QueryVisibility::default(), None)
                    .await
                    .unwrap()
                    .trace
                    .is_empty()
            );
        }
    }
}

#[tokio::test]
async fn reorder_keeps_old_markers_and_projects_exact_new_stack_positions() {
    let default = oid(1);
    let old_root = LiteralRevision { head: oid(20), first_parent: default };
    let new_root = LiteralRevision { head: oid(10), first_parent: default };
    let moved_child = LiteralRevision { head: oid(21), first_parent: new_root.head };
    let old_intent = root_intent(default, "Gmove", old_root);
    let new_intent = LocalIntent::new(
        default,
        [
            local_change("Gnew", new_root, "New root", "New root body"),
            local_change("Gmove", moved_child, "Moved child", "Moved child body"),
        ],
    );
    let mut world = DurableWorld::for_intents(default, &[&old_intent, &new_intent]);
    world.publish_for_setup(&id("Gmove"), old_root);
    world.open_for_setup(&id("Gmove"), "Old root", "Old root body");
    world.mark_for_setup(&id("Gmove"), old_root.head, BaseKind::Default);
    let old_marker = world.published(&id("Gmove")).unwrap().marker;
    let stale = QueryVisibility::stale(&world, [id("Gmove")]);

    let attempt = world.run_attempt(&new_intent, &stale, None).await.unwrap();
    assert_eq!(
        flatten(&attempt.trace.initial_refs)
            .iter()
            .map(|effect| effect.tuple().version)
            .collect::<Vec<_>>(),
        [1, 2]
    );
    assert_eq!(
        attempt.trace.creates.iter().flatten().map(|create| create.id.clone()).collect::<Vec<_>>(),
        [id("Gnew")]
    );
    assert_eq!(
        flatten(&attempt.trace.markers).iter().map(|marker| marker.id.clone()).collect::<Vec<_>>(),
        [id("Gnew")]
    );
    assert_model_snapshot!("reorder_exact_final_updates", update_snapshot(&attempt.trace));
    assert_eq!(
        world.published(&id("Gmove")).unwrap().marker,
        old_marker,
        "the immutable canonical marker remains byte-for-byte unchanged"
    );
    assert!(
        world
            .run_attempt(&new_intent, &QueryVisibility::default(), None)
            .await
            .unwrap()
            .trace
            .is_empty()
    );
}

#[tokio::test]
async fn stale_nonroot_visibility_can_project_a_root_without_touching_its_old_parent() {
    let default = oid(1);
    let old_parent = LiteralRevision { head: oid(30), first_parent: default };
    let old_child = LiteralRevision { head: oid(31), first_parent: old_parent.head };
    let moved_root = LiteralRevision { head: oid(32), first_parent: default };
    let old_intent = LocalIntent::new(
        default,
        [
            local_change("Gparent", old_parent, "Old parent", "Parent body"),
            local_change("Gmove", old_child, "Old child", "Old child body"),
        ],
    );
    let new_intent = LocalIntent::new(
        default,
        [local_change("Gmove", moved_root, "Moved root", "Moved root body")],
    );
    let mut world = DurableWorld::for_intents(default, &[&old_intent, &new_intent]);
    world.publish_for_setup(&id("Gparent"), old_parent);
    world.publish_for_setup(&id("Gmove"), old_child);
    world.open_for_setup(&id("Gmove"), "Old child", "Old child body");
    world.mark_for_setup(&id("Gmove"), old_child.head, BaseKind::Owned);
    let parent_before = world.published(&id("Gparent")).cloned();
    let stale = QueryVisibility::stale(&world, [id("Gmove")]);

    let attempt = world.run_attempt(&new_intent, &stale, None).await.unwrap();
    assert_eq!(flatten(&attempt.trace.initial_refs).len(), 1);
    assert!(attempt.trace.creates.is_empty());
    assert!(attempt.trace.markers.is_empty());
    let updates = update_effects(&attempt.trace);
    assert_eq!(updates.len(), 1);
    assert_eq!(updates[0].resolved_id, Some(id("Gmove")));
    assert_eq!(updates[0].operation.title.as_deref(), Some("Moved root"));
    assert_eq!(updates[0].operation.base_branch.as_deref(), Some(DEFAULT_BRANCH));
    assert_eq!(world.published(&id("Gparent")), parent_before.as_ref());

    let stale_retry = world.run_attempt(&new_intent, &stale, None).await.unwrap();
    assert_eq!(
        update_effects(&stale_retry.trace),
        updates,
        "durable effects cannot advance an already-returned stale observation"
    );
    assert!(
        world
            .run_attempt(&new_intent, &QueryVisibility::default(), None)
            .await
            .unwrap()
            .trace
            .is_empty()
    );
}

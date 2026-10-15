use super::*;

#[tokio::test]
async fn competing_publishers_resume_across_every_authority_barrier() {
    let default = oid(1);
    let old_revision = LiteralRevision { head: oid(10), first_parent: default };
    let new_revision = LiteralRevision { head: oid(11), first_parent: default };
    let old_intent = root_intent(default, "Gtuplecreate", old_revision);
    let new_intent = root_intent(default, "Gtuplecreate", new_revision);
    let initial = DurableWorld::for_intents(default, &[&old_intent, &new_intent]);

    let primary = initial.plan(&old_intent, &QueryVisibility::default()).unwrap();
    let mut post_primary_initial_refs = initial.clone();
    let preview = post_primary_initial_refs
        .run_attempt(
            &old_intent,
            &QueryVisibility::default(),
            Some(Interruption::Create { batch: 0, applied_aliases: Box::new([]) }),
        )
        .await
        .unwrap();
    assert_eq!(flatten(&preview.trace.initial_refs).len(), 1);
    let competing =
        post_primary_initial_refs.plan(&new_intent, &QueryVisibility::default()).unwrap();

    let mut world = initial;
    let (primary, competing) = execute_with_competition(
        &mut world,
        primary,
        competing,
        CompetitionBoundary::AfterInitialRefs,
        Some(Interruption::Create { batch: 0, applied_aliases: Box::new([]) }),
    )
    .await
    .unwrap();
    assert_attempt_outcome!(competing, AttemptOutcome::Stopped(StopReason::Indeterminate));
    assert_eq!(flatten(&competing.trace.initial_refs).len(), 1);
    assert!(competing.trace.markers.is_empty() && competing.trace.projections.is_empty());
    assert_attempt_outcome!(
        primary,
        AttemptOutcome::Stopped(StopReason::Indeterminate),
        "the suspended create lands, but its stale object IDs cannot release a marker"
    );
    assert!(primary.trace.markers.is_empty() && primary.trace.projections.is_empty());
    assert_eq!(
        world.published(&id("Gtuplecreate")).unwrap().history.iter().copied().collect::<Vec<_>>(),
        [old_revision, new_revision]
    );
    assert_eq!(world.published(&id("Gtuplecreate")).unwrap().marker, None);
    assert_restart_converges(world, &new_intent, "tuple-create-interleaving").await;

    let intent = root_intent(
        default,
        "Gcreatemarker",
        LiteralRevision { head: oid(20), first_parent: default },
    );
    let mut initial = DurableWorld::for_intents(default, &[&intent]);
    initial.publish_for_setup(&intent.first.id, intent.first.desired);
    let primary = initial.plan(&intent, &QueryVisibility::default()).unwrap();
    let competing = initial.plan(&intent, &QueryVisibility::default()).unwrap();
    let mut world = initial;
    let (primary, competing) = execute_with_competition(
        &mut world,
        primary,
        competing,
        CompetitionBoundary::AfterCreates,
        None,
    )
    .await
    .unwrap();
    assert_attempt_outcome!(competing, AttemptOutcome::Acknowledged);
    assert_attempt_outcome!(
        primary,
        AttemptOutcome::Stopped(StopReason::Rejected),
        "only the competing create's different marker can win the absence lease"
    );
    assert!(primary.trace.projections.is_empty());
    assert_eq!(world.open_pull_requests(&id("Gcreatemarker")).count(), 2);
    let winner = world.published(&id("Gcreatemarker")).unwrap().marker.unwrap().number();
    assert_eq!(winner, world.latest_identity(&id("Gcreatemarker")).number());
    let retry = run_acknowledged_retry(&mut world, &intent, "different-create-markers").await;
    assert!(matches!(
        flatten(&retry.trace.projections).as_ref(),
        [ProjectionEffect::Close { identity, .. }] if identity.number().get() == 100
    ));
    assert_quiescent(&mut world, &intent, "different-create-markers").await;

    let root = LiteralRevision { head: oid(30), first_parent: default };
    let a_child = LiteralRevision { head: oid(31), first_parent: root.head };
    let b_child = LiteralRevision { head: oid(32), first_parent: root.head };
    let root_change = local_change("Gbarrierroot", root, "Root", "Root body");
    let a_intent = LocalIntent::new(
        default,
        [root_change.clone(), local_change("Gbarriera", a_child, "A", "A body")],
    );
    let b_intent =
        LocalIntent::new(default, [root_change, local_change("Gbarrierb", b_child, "B", "B body")]);
    let mut world = DurableWorld::for_intents(default, &[&a_intent, &b_intent]);
    for change in [&a_intent.first, &a_intent.later[0], &b_intent.later[0]] {
        world.publish_for_setup(&change.id, change.desired);
        world.open_for_setup(&change.id, &change.title, "provisional");
    }
    let primary = world.plan(&a_intent, &QueryVisibility::default()).unwrap();
    let competing = world.plan(&b_intent, &QueryVisibility::default()).unwrap();
    let (primary, competing) = execute_with_competition(
        &mut world,
        primary,
        competing,
        CompetitionBoundary::AfterMarkers,
        None,
    )
    .await
    .unwrap();
    assert_attempt_outcome!(primary, AttemptOutcome::Acknowledged);
    assert_attempt_outcome!(competing, AttemptOutcome::Acknowledged);
    assert_eq!(flatten(&primary.trace.markers).len(), 2);
    assert_eq!(flatten(&competing.trace.markers).len(), 2);
    let primary_root_body = update_effects(&primary.trace)
        .iter()
        .find(|update| update.resolved_id.as_ref() == Some(&id("Gbarrierroot")))
        .and_then(|update| update.operation.body.clone())
        .unwrap();
    assert_eq!(
        &world.selected_open_pull_request(&id("Gbarrierroot")).unwrap().open_fields().unwrap().body,
        &primary_root_body,
        "the resumed primary projection is the last complete alias"
    );
    assert!(
        world
            .run_attempt(&a_intent, &QueryVisibility::default(), None)
            .await
            .unwrap()
            .trace
            .is_empty()
    );
}

#[tokio::test]
async fn fresh_competitor_marks_and_updates_after_primary_creates() {
    let primary_intent = two_change_intent();
    let competing_intent = LocalIntent::new(oid(10), std::iter::once(primary_intent.first.clone()));
    let initial = DurableWorld::for_intents(oid(10), &[&primary_intent, &competing_intent]);
    let primary = initial.plan(&primary_intent, &QueryVisibility::default()).unwrap();

    // Build only the observation from the post-create checkpoint. The actual
    // schedule below starts again from `initial`; no durable preview state or
    // attempt-local receipt is shared between publishers.
    let mut post_create = initial.clone();
    let preview = post_create
        .run_attempt(
            &primary_intent,
            &QueryVisibility::default(),
            Some(Interruption::Marker {
                batch: 0,
                stop: GitStop::Indeterminate { applied: false },
            }),
        )
        .await
        .unwrap();
    assert_attempt_outcome!(preview, AttemptOutcome::Stopped(StopReason::Indeterminate));
    assert_eq!(flatten(&preview.trace.creates).len(), 2);
    assert!(preview.trace.projections.is_empty());
    let competing = post_create.plan(&competing_intent, &QueryVisibility::default()).unwrap();

    let mut world = initial;
    let (primary, competing) = execute_with_competition(
        &mut world,
        primary,
        competing,
        CompetitionBoundary::AfterCreates,
        None,
    )
    .await
    .unwrap();
    assert_attempt_outcome!(primary, AttemptOutcome::Acknowledged);
    assert_attempt_outcome!(competing, AttemptOutcome::Acknowledged);
    assert!(competing.trace.initial_refs.is_empty());
    assert!(competing.trace.creates.is_empty());
    assert_eq!(flatten(&competing.trace.markers).len(), 1);
    assert_eq!(flatten(&competing.trace.projections).len(), 1);
    assert_eq!(flatten(&primary.trace.markers).len(), 2);
    assert_eq!(flatten(&primary.trace.projections).len(), 2);

    let root_id = &primary_intent.first.id;
    let primary_updates = update_effects(&primary.trace);
    let primary_body = primary_updates
        .iter()
        .find(|update| update.resolved_id.as_ref() == Some(root_id))
        .and_then(|update| update.operation.body.as_ref())
        .expect("the resumed two-change publisher updates the shared root body");
    let competing_updates = update_effects(&competing.trace);
    let competing_body = competing_updates
        .iter()
        .find(|update| update.resolved_id.as_ref() == Some(root_id))
        .and_then(|update| update.operation.body.as_ref())
        .expect("the fresh root-only publisher updates the shared root body");
    assert_ne!(primary_body, competing_body);
    assert_eq!(
        &world.selected_open_pull_request(root_id).unwrap().open_fields().unwrap().body,
        primary_body,
        "the primary resumes after its competitor and is the observable last writer"
    );
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
async fn competing_publishers_interleave_between_initial_ref_batches() {
    let mut competing_tuple_intent = many_bounded_ids_intent(30, 128);
    let primary_tuple_intent = competing_tuple_intent.clone();
    let competing_last = &mut competing_tuple_intent.later.last_mut().unwrap().desired;
    competing_last.head = oid(500);
    let initial =
        DurableWorld::for_intents(oid(1), &[&primary_tuple_intent, &competing_tuple_intent]);
    let primary = initial.plan(&primary_tuple_intent, &QueryVisibility::default()).unwrap();
    let competing = initial.plan(&competing_tuple_intent, &QueryVisibility::default()).unwrap();
    let mut world = initial;
    let (primary, competing) = execute_with_competition(
        &mut world,
        primary,
        competing,
        CompetitionBoundary::BetweenBatches { stage: EffectStage::InitialRefs, completed_batch: 0 },
        None,
    )
    .await
    .unwrap();
    assert_attempt_outcome!(competing, AttemptOutcome::Stopped(StopReason::Rejected));
    assert_attempt_outcome!(primary, AttemptOutcome::Acknowledged);
    assert_eq!(primary.trace.initial_refs.len(), 2);
    assert!(!primary.trace.creates.is_empty());
    let last_id = &primary_tuple_intent.later.last().unwrap().id;
    assert_eq!(
        world.published(last_id).unwrap().history.last().head,
        primary_tuple_intent.later.last().unwrap().desired.head
    );
    assert_restart_converges(world, &primary_tuple_intent, "between-initial-ref-batches").await;
}

#[tokio::test]
async fn competing_revisions_share_v1_markers_between_batches() {
    let old_marker_intent = many_bounded_ids_intent(90, 128);
    let last_v1 = old_marker_intent.later.last().unwrap().desired.head;
    let mut new_marker_intent = old_marker_intent.clone();
    let new_marker_revision = {
        let revision = &mut new_marker_intent.later.last_mut().unwrap().desired;
        revision.head = oid(600);
        *revision
    };
    let mut initial = DurableWorld::for_intents(oid(1), &[&old_marker_intent, &new_marker_intent]);
    for local in old_marker_intent.iter() {
        initial.publish_for_setup(&local.id, local.desired);
        initial.open_for_setup(&local.id, &local.title, "provisional");
    }
    let primary = initial.plan(&old_marker_intent, &QueryVisibility::default()).unwrap();
    let mut world = initial;
    let last_id = &new_marker_intent.later.last().unwrap().id;
    world.publish_for_setup(last_id, new_marker_revision);
    let competing = world.plan(&new_marker_intent, &QueryVisibility::default()).unwrap();
    let (primary, competing) = execute_with_competition(
        &mut world,
        primary,
        competing,
        CompetitionBoundary::BetweenBatches { stage: EffectStage::Marker, completed_batch: 0 },
        None,
    )
    .await
    .unwrap();
    assert_attempt_outcome!(competing, AttemptOutcome::Acknowledged);
    assert_attempt_outcome!(primary, AttemptOutcome::Stopped(StopReason::Indeterminate));
    assert_eq!(primary.trace.markers.len(), 2);
    assert!(!primary.trace.projections.is_empty());
    assert_eq!(
        world.published(last_id).unwrap().marker,
        Some(marker_identity(last_v1, world.identity(last_id).number()))
    );
    assert_restart_converges(world, &old_marker_intent, "between-marker-batches").await;
}

#[tokio::test]
async fn stale_competitor_loses_completed_create_request_and_primary_resumes() {
    let create_intent = multi_request_intent();
    let mut initial = DurableWorld::for_intents(oid(10), &[&create_intent]);
    for change in create_intent.iter() {
        initial.publish_for_setup(&change.id, change.desired);
    }
    for completed_batch in 0..2 {
        let primary = initial.plan(&create_intent, &QueryVisibility::default()).unwrap();
        let competing = initial.plan(&create_intent, &QueryVisibility::default()).unwrap();
        let mut world = initial.clone();
        let (primary, competing) = execute_with_competition(
            &mut world,
            primary,
            competing,
            CompetitionBoundary::BetweenBatches { stage: EffectStage::Create, completed_batch },
            None,
        )
        .await
        .unwrap();
        assert_attempt_outcome!(competing, AttemptOutcome::Acknowledged);
        assert_attempt_outcome!(primary, AttemptOutcome::Stopped(StopReason::Rejected));
        assert_eq!(
            primary.trace.creates.iter().map(|batch| batch.len()).collect::<Vec<_>>(),
            [1, 1, 1]
        );
        assert_eq!(competing.trace.creates.len(), 3);
        assert_eq!(flatten(&competing.trace.markers).len(), 3);
        assert_eq!(update_effects(&competing.trace).len(), 3);
        assert!(primary.trace.projections.is_empty());
        let retry = run_acknowledged_retry(
            &mut world,
            &create_intent,
            &format!("stale-create-batches-{completed_batch}"),
        )
        .await;
        assert_eq!(
            flatten(&retry.trace.projections)
                .iter()
                .filter(|effect| matches!(effect, ProjectionEffect::Close { .. }))
                .count(),
            3
        );
        assert_quiescent(
            &mut world,
            &create_intent,
            &format!("stale-create-batches-{completed_batch}"),
        )
        .await;
    }
}

#[tokio::test]
async fn fresh_competitor_wins_future_create_requests_and_stale_primary_stops() {
    // A publisher which starts after a non-final request observes and retains
    // every completed identity, then creates the tail before the original
    // publisher resumes its stale plan.
    let create_intent = multi_request_intent();
    let initial = DurableWorld::for_intents(oid(10), &[&create_intent]);
    for completed_batch in 0..2 {
        let mut after_completed_requests = initial.clone();
        let preview = after_completed_requests
            .run_attempt(
                &create_intent,
                &QueryVisibility::default(),
                Some(Interruption::Create {
                    batch: completed_batch + 1,
                    applied_aliases: Box::new([]),
                }),
            )
            .await
            .unwrap();
        assert_attempt_outcome!(preview, AttemptOutcome::Stopped(StopReason::Indeterminate));
        assert_eq!(preview.trace.creates.len(), completed_batch + 2);

        let primary = initial.plan(&create_intent, &QueryVisibility::default()).unwrap();
        let competing =
            after_completed_requests.plan(&create_intent, &QueryVisibility::default()).unwrap();
        let mut world = initial.clone();
        let (primary, competing) = execute_with_competition(
            &mut world,
            primary,
            competing,
            CompetitionBoundary::BetweenBatches { stage: EffectStage::Create, completed_batch },
            None,
        )
        .await
        .unwrap();
        assert_attempt_outcome!(competing, AttemptOutcome::Acknowledged);
        assert_attempt_outcome!(primary, AttemptOutcome::Stopped(StopReason::Rejected));
        assert_eq!(primary.trace.creates.len(), 3);
        assert_eq!(
            competing.trace.creates.iter().map(|batch| batch.len()).collect::<Vec<_>>(),
            vec![1; 2 - completed_batch]
        );
        assert!(!primary.trace.markers.is_empty() && primary.trace.projections.is_empty());
        assert_eq!(flatten(&competing.trace.markers).len(), 3);
        assert_eq!(flatten(&competing.trace.projections).len(), 3);
        run_acknowledged_retry(
            &mut world,
            &create_intent,
            &format!("fresh-create-batches-{completed_batch}"),
        )
        .await;
        assert_quiescent(
            &mut world,
            &create_intent,
            &format!("fresh-create-batches-{completed_batch}"),
        )
        .await;
    }
}

#[tokio::test]
async fn competing_publishers_interleave_between_update_requests() {
    let shared = multi_request_intent();
    let shared_tip = shared.later.last().unwrap().desired.head;
    let a_intent = LocalIntent::new(
        oid(10),
        shared.iter().cloned().chain([local_change(
            "Grequesta",
            LiteralRevision { head: oid(700), first_parent: shared_tip },
            "A child",
            "A body",
        )]),
    );
    let b_intent = LocalIntent::new(
        oid(10),
        shared.iter().cloned().chain([local_change(
            "Grequestb",
            LiteralRevision { head: oid(701), first_parent: shared_tip },
            "B child",
            "B body",
        )]),
    );
    let initial = {
        let mut world = DurableWorld::for_intents(oid(10), &[&a_intent, &b_intent]);
        for change in a_intent.iter().chain(std::iter::once(b_intent.later.last().unwrap())) {
            establish_marked(
                &mut world,
                change,
                if change.id == a_intent.first.id { BaseKind::Default } else { BaseKind::Owned },
                "provisional",
            );
        }
        world
    };

    for completed_batch in 0..2 {
        let primary = initial.plan(&a_intent, &QueryVisibility::default()).unwrap();
        let competing = initial.plan(&b_intent, &QueryVisibility::default()).unwrap();
        let mut world = initial.clone();
        let (primary, competing) = execute_with_competition(
            &mut world,
            primary,
            competing,
            CompetitionBoundary::BetweenBatches { stage: EffectStage::Projection, completed_batch },
            None,
        )
        .await
        .unwrap();
        assert_attempt_outcome!(primary, AttemptOutcome::Acknowledged);
        assert_attempt_outcome!(competing, AttemptOutcome::Acknowledged);
        assert_eq!(
            primary.trace.projections.iter().map(|batch| batch.len()).collect::<Vec<_>>(),
            [1, 1, 2]
        );
        assert_eq!(
            competing.trace.projections.iter().map(|batch| batch.len()).collect::<Vec<_>>(),
            [1, 1, 2]
        );

        let primary_count =
            primary.trace.projections.iter().map(|batch| batch.len()).sum::<usize>();
        let primary_writes = primary
            .trace
            .projections
            .iter()
            .enumerate()
            .flat_map(|(batch, projections)| {
                projections.iter().filter_map(move |projection| {
                    let ProjectionEffect::Update(update) = projection else {
                        return None;
                    };
                    let id = update
                        .resolved_id
                        .clone()
                        .expect("every primary update has an exact target");
                    Some((id, (batch, update.clone())))
                })
            })
            .collect::<HashMap<_, _>>();
        assert_eq!(primary_writes.len(), primary_count);
        let competing_count =
            competing.trace.projections.iter().map(|batch| batch.len()).sum::<usize>();
        let competing_writes = update_effects(&competing.trace)
            .iter()
            .cloned()
            .map(|update| {
                let id =
                    update.resolved_id.clone().expect("every competing update has an exact target");
                (id, update)
            })
            .collect::<HashMap<_, _>>();
        assert_eq!(competing_writes.len(), competing_count);

        let primary_ids = primary_writes.keys().cloned().collect::<HashSet<_>>();
        let competing_ids = competing_writes.keys().cloned().collect::<HashSet<_>>();
        assert_eq!(
            primary_ids,
            a_intent.iter().map(|change| change.id.clone()).collect::<HashSet<_>>()
        );
        assert_eq!(
            competing_ids,
            b_intent.iter().map(|change| change.id.clone()).collect::<HashSet<_>>()
        );
        for shared_id in primary_ids.intersection(&competing_ids) {
            let primary_body = primary_writes[shared_id]
                .1
                .operation
                .body
                .as_ref()
                .expect("every shared primary update replaces its body");
            let competing_body = competing_writes[shared_id]
                .operation
                .body
                .as_ref()
                .expect("every shared competing update replaces its body");
            assert_ne!(
                primary_body, competing_body,
                "the last-writer assertion for {shared_id:?} must be observable"
            );
        }
        let ids =
            primary_writes.keys().chain(competing_writes.keys()).cloned().collect::<HashSet<_>>();
        for id in ids {
            let expected = match (primary_writes.get(&id), competing_writes.get(&id)) {
                (Some((batch, _)), Some(competing)) if *batch <= completed_batch => competing,
                (Some((_, primary)), _) => primary,
                (None, Some(competing)) => competing,
                (None, None) => unreachable!("the ID came from one writer map"),
            };
            assert_projection_matches_update(&world, expected);
        }

        assert_restart_converges(
            world,
            &a_intent,
            &format!("between-update-request-{completed_batch}"),
        )
        .await;
    }
}

#[tokio::test]
async fn disjoint_and_identical_publishers_have_only_protocol_outcomes() {
    let default = oid(1);
    let a_revision = LiteralRevision { head: oid(10), first_parent: default };
    let b_revision = LiteralRevision { head: oid(20), first_parent: default };
    let a_intent = root_intent(default, "GA", a_revision);
    let b_intent = root_intent(default, "GB", b_revision);
    let initial = DurableWorld::for_intents(default, &[&a_intent, &b_intent]);

    let mut ab = initial.clone();
    let a = initial.plan(&a_intent, &QueryVisibility::default()).unwrap();
    let b = initial.plan(&b_intent, &QueryVisibility::default()).unwrap();
    assert_eq!(ab.execute_plan(a, None).await.unwrap().outcome, AttemptOutcome::Acknowledged);
    assert!(ab.published(&id("GB")).is_none(), "publisher A cannot publish B's change");
    assert_eq!(ab.execute_plan(b, None).await.unwrap().outcome, AttemptOutcome::Acknowledged);

    let mut ba = initial.clone();
    let b = initial.plan(&b_intent, &QueryVisibility::default()).unwrap();
    let a = initial.plan(&a_intent, &QueryVisibility::default()).unwrap();
    assert_eq!(ba.execute_plan(b, None).await.unwrap().outcome, AttemptOutcome::Acknowledged);
    assert!(ba.published(&id("GA")).is_none(), "publisher B cannot publish A's change");
    assert_eq!(ba.execute_plan(a, None).await.unwrap().outcome, AttemptOutcome::Acknowledged);
    assert_ne!(ab.identity(&id("GA")), ba.identity(&id("GA")), "allocation order is durable");
    assert_disjoint_results_commute(&ab, &ba);
    assert!(
        ab.run_attempt(&a_intent, &QueryVisibility::default(), None)
            .await
            .unwrap()
            .trace
            .is_empty()
    );
    assert!(
        ab.run_attempt(&b_intent, &QueryVisibility::default(), None)
            .await
            .unwrap()
            .trace
            .is_empty()
    );
    assert!(
        ba.run_attempt(&a_intent, &QueryVisibility::default(), None)
            .await
            .unwrap()
            .trace
            .is_empty()
    );
    assert!(
        ba.run_attempt(&b_intent, &QueryVisibility::default(), None)
            .await
            .unwrap()
            .trace
            .is_empty()
    );

    let same_revision = LiteralRevision { head: oid(30), first_parent: default };
    let same_intent = root_intent(default, "Gsame", same_revision);
    let same_initial = DurableWorld::for_intents(default, &[&same_intent]);
    let same_a = same_initial.plan(&same_intent, &QueryVisibility::default()).unwrap();
    let same_b = same_initial.plan(&same_intent, &QueryVisibility::default()).unwrap();
    let mut same_world = same_initial;
    assert_eq!(
        same_world.execute_plan(same_a, None).await.unwrap().outcome,
        AttemptOutcome::Acknowledged
    );
    let after_first = same_world.clone();
    assert_eq!(
        same_world.execute_plan(same_b, None).await.unwrap().outcome,
        AttemptOutcome::Stopped(StopReason::Rejected),
        "the second create's different marker loses the immutable lease"
    );
    assert_eq!(same_world, after_first, "distinct publication token rejects before creation");
    assert_eq!(same_world.open_pull_requests(&id("Gsame")).count(), 1);
    assert_quiescent(&mut same_world, &same_intent, "identical-create-race").await;

    let marker_revision = LiteralRevision { head: oid(40), first_parent: default };
    let marker_intent = root_intent(default, "Gmarker", marker_revision);
    let mut marker_world = DurableWorld::for_intents(default, &[&marker_intent]);
    marker_world.publish_for_setup(&id("Gmarker"), marker_revision);
    marker_world.open_for_setup(&id("Gmarker"), "provisional", "provisional");
    let marker_a = marker_world.plan(&marker_intent, &QueryVisibility::default()).unwrap();
    let marker_b = marker_world.plan(&marker_intent, &QueryVisibility::default()).unwrap();
    let marker_a = marker_world.execute_plan(marker_a, None).await.unwrap();
    assert!(marker_a.trace.initial_refs.is_empty() && marker_a.trace.creates.is_empty());
    assert_eq!(flatten(&marker_a.trace.markers).len(), 1);
    let after_marker = marker_world.clone();
    assert_eq!(
        marker_world.execute_plan(marker_b, None).await.unwrap().outcome,
        AttemptOutcome::Acknowledged,
        "the same immutable marker and projection are idempotent"
    );
    assert_eq!(marker_world, after_marker);
    assert!(
        marker_world
            .run_attempt(&marker_intent, &QueryVisibility::default(), None)
            .await
            .unwrap()
            .trace
            .is_empty()
    );
}

#[tokio::test]
async fn conflicting_publishers_retry_from_the_winner_and_indeterminate_create() {
    let default = oid(1);
    let v1 = LiteralRevision { head: oid(10), first_parent: default };
    let a_revision = LiteralRevision { head: oid(11), first_parent: default };
    let b_revision = LiteralRevision { head: oid(12), first_parent: default };
    let a_intent = root_intent(default, "Gconflict", a_revision);
    let b_intent = root_intent(default, "Gconflict", b_revision);
    let mut initial = DurableWorld::for_intents(default, &[&a_intent, &b_intent]);
    initial.publish_for_setup(&id("Gconflict"), v1);
    for (winner_intent, loser_intent, winner, loser) in [
        (&a_intent, &b_intent, a_revision, b_revision),
        (&b_intent, &a_intent, b_revision, a_revision),
    ] {
        let mut world = initial.clone();
        let winner_attempt = initial.plan(winner_intent, &QueryVisibility::default()).unwrap();
        let loser_attempt = initial.plan(loser_intent, &QueryVisibility::default()).unwrap();
        assert_eq!(
            world.execute_plan(winner_attempt, None).await.unwrap().outcome,
            AttemptOutcome::Acknowledged
        );
        let after_winner = world.clone();
        assert_eq!(
            world.execute_plan(loser_attempt, None).await.unwrap().outcome,
            AttemptOutcome::Stopped(StopReason::Rejected),
            "a stale tuple lease stops every later stage of that attempt"
        );
        assert_eq!(world, after_winner);

        let retry =
            world.run_attempt(loser_intent, &QueryVisibility::default(), None).await.unwrap();
        assert_eq!(
            flatten(&retry.trace.initial_refs).as_ref(),
            &[InitialRefEffect::Tuple(TupleEffect {
                id: id("Gconflict"),
                expected: Some(winner),
                desired: loser,
                version: 3,
                publication: retry.trace.initial_refs[0][0].tuple().publication,
            })]
        );
        assert_attempt_outcome!(retry, AttemptOutcome::Acknowledged);
        assert_eq!(
            world.published(&id("Gconflict")).unwrap().history.iter().copied().collect::<Vec<_>>(),
            [v1, winner, loser]
        );
        assert!(
            world
                .run_attempt(loser_intent, &QueryVisibility::default(), None)
                .await
                .unwrap()
                .trace
                .is_empty()
        );
    }

    let old_revision = LiteralRevision { head: oid(20), first_parent: default };
    let new_revision = LiteralRevision { head: oid(21), first_parent: default };
    let old_intent = root_intent(default, "Gcreate", old_revision);
    let new_intent = root_intent(default, "Gcreate", new_revision);
    let mut world = DurableWorld::for_intents(default, &[&old_intent, &new_intent]);
    let first = world
        .run_attempt(
            &old_intent,
            &QueryVisibility::default(),
            Some(Interruption::Create { batch: 0, applied_aliases: Box::new([]) }),
        )
        .await
        .unwrap();
    assert_eq!(flatten(&first.trace.initial_refs).len(), 1);
    let old_create_attempt = world.plan(&old_intent, &QueryVisibility::default()).unwrap();
    let concurrent = world
        .run_attempt(
            &new_intent,
            &QueryVisibility::default(),
            Some(Interruption::Create { batch: 0, applied_aliases: Box::new([]) }),
        )
        .await
        .unwrap();
    assert_eq!(flatten(&concurrent.trace.initial_refs).len(), 1);
    let indeterminate = world.execute_plan(old_create_attempt, None).await.unwrap();
    assert_attempt_outcome!(
        indeterminate,
        AttemptOutcome::Stopped(StopReason::Indeterminate),
        "the stable-key create may land after its observed branch OIDs move"
    );
    assert!(indeterminate.trace.initial_refs.is_empty());
    assert_eq!(indeterminate.trace.creates.len(), 1);
    assert!(
        world.published(&id("Gcreate")).unwrap().marker.is_none(),
        "an indeterminate create receipt cannot authorize later attempt stages"
    );
    let world = assert_restart_converges(world, &new_intent, "indeterminate-create").await;
    assert_eq!(
        world.published(&id("Gcreate")).unwrap().history.iter().copied().collect::<Vec<_>>(),
        [old_revision, new_revision]
    );
}

#[tokio::test]
async fn divergent_navigation_is_last_writer_wins_then_freshly_repairable() {
    let default = oid(1);
    let root = LiteralRevision { head: oid(10), first_parent: default };
    let a_child = LiteralRevision { head: oid(11), first_parent: root.head };
    let b_child = LiteralRevision { head: oid(12), first_parent: root.head };
    let root_change = local_change("Groot", root, "Shared root", "Root body");
    let a_intent = LocalIntent::new(
        default,
        [root_change.clone(), local_change("GA", a_child, "A child", "A body")],
    );
    let b_intent =
        LocalIntent::new(default, [root_change, local_change("GB", b_child, "B child", "B body")]);
    let mut initial = DurableWorld::for_intents(default, &[&a_intent, &b_intent]);
    for (change, base) in [
        (&a_intent.first, BaseKind::Default),
        (&a_intent.later[0], BaseKind::Owned),
        (&b_intent.later[0], BaseKind::Owned),
    ] {
        establish_marked(&mut initial, change, base, "stale body");
    }

    let mut a_preview = initial.clone();
    let a_attempt =
        a_preview.run_attempt(&a_intent, &QueryVisibility::default(), None).await.unwrap();
    let mut b_preview = initial.clone();
    let b_attempt =
        b_preview.run_attempt(&b_intent, &QueryVisibility::default(), None).await.unwrap();
    assert!(a_attempt.trace.initial_refs.is_empty() && a_attempt.trace.markers.is_empty());
    assert!(b_attempt.trace.initial_refs.is_empty() && b_attempt.trace.markers.is_empty());
    assert_model_snapshot!(
        "divergent_child_exact_update_alternatives",
        UpdateAlternatives {
            publisher_a: update_snapshot(&a_attempt.trace),
            publisher_b: update_snapshot(&b_attempt.trace),
        }
    );

    let a_updates = update_effects(&a_attempt.trace);
    let b_updates = update_effects(&b_attempt.trace);
    assert_eq!((a_updates.len(), b_updates.len()), (2, 2));
    let a_root_body = a_updates
        .iter()
        .find(|update| update.resolved_id.as_ref() == Some(&id("Groot")))
        .and_then(|update| update.operation.body.clone())
        .unwrap();
    let b_root_body = b_updates
        .iter()
        .find(|update| update.resolved_id.as_ref() == Some(&id("Groot")))
        .and_then(|update| update.operation.body.clone())
        .unwrap();
    assert_ne!(a_root_body, b_root_body);

    for (first_intent, second_intent, expected, repair_intent, nonlocal_id) in [
        (&a_intent, &b_intent, &b_root_body, &a_intent, id("GB")),
        (&b_intent, &a_intent, &a_root_body, &b_intent, id("GA")),
    ] {
        let mut world = initial.clone();
        let first = initial.plan(first_intent, &QueryVisibility::default()).unwrap();
        let second = initial.plan(second_intent, &QueryVisibility::default()).unwrap();
        assert_eq!(
            world.execute_plan(first, None).await.unwrap().outcome,
            AttemptOutcome::Acknowledged
        );
        assert_eq!(
            world.execute_plan(second, None).await.unwrap().outcome,
            AttemptOutcome::Acknowledged
        );
        assert_eq!(
            &world.selected_open_pull_request(&id("Groot")).unwrap().open_fields().unwrap().body,
            expected,
            "the later stale update request wins at the stage boundary"
        );
        let nonlocal = world.published(&nonlocal_id).cloned();
        let stabilized = assert_restart_converges(world, repair_intent, "navigation-repair").await;
        assert_eq!(stabilized.published(&nonlocal_id), nonlocal.as_ref());
    }
}

#[tokio::test]
async fn stale_precomputed_projection_lands_then_revision_receipt_stops_and_fresh_intent_repairs_it()
 {
    let default = oid(1);
    let old_revision = LiteralRevision { head: oid(10), first_parent: default };
    let new_revision = LiteralRevision { head: oid(11), first_parent: default };
    let old_intent =
        LocalIntent::new(default, [local_change("Gstale", old_revision, "Old title", "Old body")]);
    let new_intent =
        LocalIntent::new(default, [local_change("Gstale", new_revision, "New title", "New body")]);
    let mut initial = DurableWorld::for_intents(default, &[&old_intent, &new_intent]);
    establish_marked(&mut initial, &old_intent.first, BaseKind::Default, "outdated body");
    let stale_attempt = initial.plan(&old_intent, &QueryVisibility::default()).unwrap();
    let new_attempt = initial.plan(&new_intent, &QueryVisibility::default()).unwrap();

    let mut world = initial;
    assert_eq!(
        world.execute_plan(new_attempt, None).await.unwrap().outcome,
        AttemptOutcome::Acknowledged
    );
    let stale = world.execute_plan(stale_attempt, None).await.unwrap();
    assert_attempt_outcome!(stale, AttemptOutcome::Stopped(StopReason::Indeterminate));
    let stale_updates = update_effects(&stale.trace);
    assert_eq!(stale_updates.len(), 1);
    assert_eq!(stale_updates[0].resolved_id, Some(id("Gstale")));
    assert_eq!(
        world.selected_open_pull_request(&id("Gstale")).unwrap().open_fields().unwrap().body,
        stale_updates[0].operation.body.clone().unwrap()
    );
    let world = assert_restart_converges(world, &new_intent, "stale-projection").await;
    assert_eq!(
        world.published(&id("Gstale")).unwrap().history.last(),
        new_revision,
        "repair never rewrites the winning immutable tuple"
    );
}

#[tokio::test]
async fn three_retained_publishers_cannot_overwrite_after_revision_aba() {
    let default = oid(1);
    let a = LiteralRevision { head: oid(2), first_parent: default };
    let b = LiteralRevision { head: oid(3), first_parent: default };
    let intent_a = root_intent(default, "Gaba", a);
    let intent_b = root_intent(default, "Gaba", b);
    let mut world = DurableWorld::for_intents(default, &[&intent_a, &intent_b]);
    run_acknowledged_retry(&mut world, &intent_a, "initial A").await;
    let x = world.plan(&intent_b, &QueryVisibility::default()).unwrap();
    let y = world.plan(&intent_b, &QueryVisibility::default()).unwrap();
    assert_eq!(world.execute_plan(y, None).await.unwrap().outcome, AttemptOutcome::Acknowledged);
    let z = world.plan(&intent_a, &QueryVisibility::default()).unwrap();
    assert_eq!(world.execute_plan(z, None).await.unwrap().outcome, AttemptOutcome::Acknowledged);
    let before = world.clone();
    assert_eq!(
        world.execute_plan(x, None).await.unwrap().outcome,
        AttemptOutcome::Stopped(StopReason::Rejected)
    );
    assert_eq!(world.git_refs, before.git_refs, "the delayed attempt changes no literal ref");
    assert_eq!(
        world.published(&id("Gaba")).unwrap().history.iter().copied().collect::<Vec<_>>(),
        [a, b, a]
    );
    assert_quiescent(&mut world, &intent_a, "ABA loser").await;
}

#[tokio::test]
async fn competing_create_runs_between_sibling_aliases_before_marker_arbitration() {
    let intent = two_change_intent();
    let mut world = DurableWorld::for_intents(oid(10), &[&intent]);
    for change in intent.iter() {
        world.publish_for_setup(&change.id, change.desired);
    }
    let primary = world.plan(&intent, &QueryVisibility::default()).unwrap();
    let competing = world.plan(&intent, &QueryVisibility::default()).unwrap();
    let boundary = CompetitionBoundary::BetweenAliases {
        stage: EffectStage::Create,
        batch: 0,
        completed_alias: 0,
    };
    let (primary, competing) =
        execute_with_competition(&mut world, primary, competing, boundary, None).await.unwrap();
    assert_eq!(primary.trace.alias_competitions, [boundary]);
    assert_eq!(primary.trace.creates[0].len(), 2);
    assert_attempt_outcome!(competing, AttemptOutcome::Acknowledged);
    assert_attempt_outcome!(primary, AttemptOutcome::Stopped(StopReason::Rejected));
    assert!(primary.trace.projections.is_empty());
    assert_eq!(world.published(&intent.first.id).unwrap().marker.unwrap().number().get(), 101);
    assert_eq!(world.published(&intent.later[0].id).unwrap().marker.unwrap().number().get(), 102);
    for change in intent.iter() {
        assert_eq!(world.open_pull_requests(&change.id).count(), 2);
    }
    assert_restart_converges(world, &intent, "between-create-aliases").await;
}

#[tokio::test]
async fn competing_revision_runs_between_projection_aliases_and_stale_receipt_stops() {
    let old = two_change_intent();
    let mut new = old.clone();
    new.first.desired.head = oid(70);
    new.first.title = "New root".into();
    new.later[0].desired = LiteralRevision { head: oid(71), first_parent: oid(70) };
    new.later[0].title = "New child".into();
    let mut world = DurableWorld::for_intents(oid(10), &[&old, &new]);
    for (index, change) in old.iter().enumerate() {
        world.publish_for_setup(&change.id, change.desired);
        world.open_for_setup(&change.id, &change.title, "unprojected");
        world.mark_for_setup(
            &change.id,
            change.desired.head,
            if index == 0 { BaseKind::Default } else { BaseKind::Owned },
        );
    }
    let primary = world.plan(&old, &QueryVisibility::default()).unwrap();
    let competing = world.plan(&new, &QueryVisibility::default()).unwrap();
    let boundary = CompetitionBoundary::BetweenAliases {
        stage: EffectStage::Projection,
        batch: 0,
        completed_alias: 0,
    };
    let (primary, competing) =
        execute_with_competition(&mut world, primary, competing, boundary, None).await.unwrap();
    assert_eq!(primary.trace.alias_competitions, [boundary]);
    assert_eq!(primary.trace.projections[0].len(), 2);
    assert_attempt_outcome!(competing, AttemptOutcome::Acknowledged);
    assert_attempt_outcome!(primary, AttemptOutcome::Stopped(StopReason::Indeterminate));
    // Receipts detect the stale revision after the second alias applies. The
    // service does not roll it back: these fields have different last writers.
    assert_eq!(
        world.selected_open_pull_request(&old.first.id).unwrap().open_fields().unwrap().title,
        new.first.title
    );
    assert_eq!(
        world.selected_open_pull_request(&old.later[0].id).unwrap().open_fields().unwrap().title,
        old.later[0].title
    );
    assert_restart_converges(world, &new, "between-projection-aliases").await;
}

#[tokio::test]
async fn competing_publication_runs_between_draft_aliases_before_primary_git() {
    let intent = many_bounded_ids_intent(3, 8);
    let prior = intent
        .later
        .iter()
        .enumerate()
        .map(|(index, change)| {
            root_intent(
                oid(1),
                change.id.as_str(),
                LiteralRevision { head: oid(90 + index as u16), first_parent: oid(1) },
            )
        })
        .collect::<Vec<_>>();
    let mut intents = vec![&intent];
    intents.extend(prior.iter());
    let mut world = DurableWorld::for_intents(oid(1), &intents);
    for old in &prior {
        world.publish_for_setup(&old.first.id, old.first.desired);
        world.open_for_setup(&old.first.id, &old.first.title, "old root");
        world.mark_ready_on_default_for_setup(&old.first.id, old.first.desired.head);
    }
    let primary = world.plan(&intent, &QueryVisibility::default()).unwrap();
    let competing = world.plan(&intent, &QueryVisibility::default()).unwrap();
    let boundary = CompetitionBoundary::BetweenAliases {
        stage: EffectStage::Draft,
        batch: 0,
        completed_alias: 0,
    };
    let (primary, competing) =
        execute_with_competition(&mut world, primary, competing, boundary, None).await.unwrap();
    assert_eq!(primary.trace.alias_competitions, [boundary]);
    assert_eq!(primary.trace.draft_conversions[0].len(), 2);
    assert_attempt_outcome!(competing, AttemptOutcome::Acknowledged);
    assert_attempt_outcome!(primary, AttemptOutcome::Stopped(StopReason::Indeterminate));
    assert!(primary.trace.initial_refs.is_empty());
    assert!(primary.trace.creates.is_empty() && primary.trace.markers.is_empty());
    assert_restart_converges(world, &intent, "between-draft-aliases").await;
}

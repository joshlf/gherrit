use super::*;

#[test]
fn marker_service_enforces_v1_and_immutability_without_github_coupling() {
    let default = oid(1);
    let first = LiteralRevision { head: oid(2), first_parent: default };
    let amended = LiteralRevision { head: oid(3), first_parent: default };
    let local = local_change("Gmarker", amended, "Marker", "Marker body");
    let intent = LocalIntent::new(default, [local.clone()]);
    let mut world = DurableWorld::for_intents(default, &[&intent]);
    world.publish_for_setup(&local.id, first);
    world.publish_for_setup(&local.id, amended);
    world.open_for_setup(&local.id, &local.title, "provisional");

    let number = world.identity(&local.id).number();
    let absent_number = MarkerEffect {
        id: local.id.clone(),
        marker: marker_identity(first.head, PullRequestNumber::for_test(number.get() + 1)),
    };
    assert_eq!(
        world.apply_effect(&ExternalEffect::Marker(absent_number.clone())),
        EffectOutcome::Acknowledged,
        "Git accepts a semantic marker without consulting GitHub rows"
    );

    let wrong_v1 =
        MarkerEffect { id: local.id.clone(), marker: marker_identity(amended.head, number) };
    let mut wrong_v1_world = world.clone();
    wrong_v1_world.published_mut(&local.id).unwrap().marker = None;
    assert_eq!(
        wrong_v1_world.apply_effect(&ExternalEffect::Marker(wrong_v1)),
        EffectOutcome::Rejected
    );

    let correct =
        MarkerEffect { id: local.id.clone(), marker: marker_identity(first.head, number) };
    assert_eq!(
        world.apply_effect(&ExternalEffect::Marker(correct)),
        EffectOutcome::Rejected,
        "a different immutable marker cannot replace the winner"
    );
    assert_eq!(
        world.apply_effect(&ExternalEffect::Marker(absent_number.clone())),
        EffectOutcome::Acknowledged,
        "the identical semantic marker is an acknowledged no-op"
    );
    assert_eq!(world.published(&local.id).unwrap().marker, Some(absent_number.marker));
}

#[test]
#[should_panic(expected = "directory/file conflict")]
fn durable_public_branches_cannot_represent_d_f_conflicts() {
    let intent =
        root_intent(oid(1), "Gdfinvariant", LiteralRevision { head: oid(2), first_parent: oid(1) });
    let mut world = DurableWorld::for_intents(oid(1), &[&intent]);
    world.public_branches.insert(public_branch_name("release-"), oid(10));
    world.public_branches.insert(public_branch_name("release-/child"), oid(11));
    world.assert_well_formed();
}

#[test]
fn public_create_lease_rejects_a_d_f_conflict_without_mutation() {
    let intent =
        root_intent(oid(1), "Gdflease", LiteralRevision { head: oid(2), first_parent: oid(1) });
    let mut world = DurableWorld::for_intents(oid(1), &[&intent]);
    world.public_branches.insert(public_branch_name("release-"), oid(10));
    world.assert_well_formed();
    let before = world.clone();
    let effect = ExternalEffect::InitialRef(InitialRefEffect::PublicBranch {
        branch: public_branch_name("release-/child"),
        expected: None,
        desired: oid(11),
    });
    assert_eq!(world.apply_effect(&effect), EffectOutcome::Rejected);
    assert_eq!(world, before);
}

fn exact_draft_operation(world: &DurableWorld, id: &GherritPrId) -> TestDraft {
    TestDraft {
        id: id.clone(),
        identity: world.identity(id),
        head_oid: world.published(id).unwrap().history.last().head,
        default_branch: DefaultBranch::new(DEFAULT_BRANCH.to_owned(), world.default_tip).unwrap(),
    }
}

fn draft_routing_world() -> (DurableWorld, GherritPrId, GherritPrId) {
    let default = oid(1);
    let target_intent = root_intent(
        default,
        "Gdraftroute",
        LiteralRevision { head: oid(2), first_parent: default },
    );
    let sibling_intent = root_intent(
        default,
        "Gdraftsibling",
        LiteralRevision { head: oid(3), first_parent: default },
    );
    let mut world = DurableWorld::for_intents(default, &[&target_intent, &sibling_intent]);
    for intent in [&target_intent, &sibling_intent] {
        world.publish_for_setup(&intent.first.id, intent.first.desired);
        world.open_for_setup(&intent.first.id, &intent.first.title, "old body");
        world.mark_ready_on_default_for_setup(&intent.first.id, intent.first.desired.head);
    }
    (world, target_intent.first.id, sibling_intent.first.id)
}

#[test]
fn draft_authority_and_safety_predicates_are_locally_scoped() {
    let (mut world, target_id, sibling_id) = draft_routing_world();
    let exact = exact_draft_operation(&world, &target_id);
    assert!(world.marker_authorizes_draft(&exact));
    assert!(world.local_open_rows_are_draft_safe(&target_id));

    world.published_mut(&target_id).unwrap().marker = None;
    assert!(!world.marker_authorizes_draft(&exact));
    let v1 = world.published(&target_id).unwrap().history.first.head;
    world.published_mut(&target_id).unwrap().marker =
        Some(marker_identity(v1, exact.identity.number()));

    world.open_for_setup(&target_id, "duplicate", "duplicate body");
    let duplicate = TestDraft { identity: world.latest_identity(&target_id), ..exact.clone() };
    assert!(!world.marker_authorizes_draft(&duplicate));

    let sibling = world.identity(&sibling_id);
    world
        .published_mut(&sibling_id)
        .unwrap()
        .pull_requests
        .iter_mut()
        .find(|row| row.identity == sibling)
        .unwrap()
        .open_fields_mut()
        .unwrap()
        .base = BaseKind::Owned;
    assert!(world.local_open_rows_are_draft_safe(&target_id));
    assert!(!world.local_open_rows_are_draft_safe(&sibling_id));
}

fn prepared_draft_conversion(operation: &TestDraft) -> PreparedDraftConversions {
    PreparedDraftConversions::prepare(vec![DraftPullRequest::from_ready_default(
        operation.id.clone(),
        operation.identity.clone(),
        operation.head_oid,
        operation.default_branch.clone(),
    )])
    .unwrap()
}

#[tokio::test]
#[should_panic(expected = "a planned draft conversion requires exact immutable-marker authority")]
async fn semantic_driver_rejects_a_fabricated_unmarked_draft_conversion() {
    let (mut world, target_id, _) = draft_routing_world();
    let operation = exact_draft_operation(&world, &target_id);
    world.published_mut(&target_id).unwrap().marker = None;
    let conversions = prepared_draft_conversion(&operation);
    let mut driver = WorldDriver::new(&mut world, None);
    driver.convert_pull_requests_to_draft(conversions).await.unwrap();
}

#[tokio::test]
#[should_panic(expected = "each reached local effect prefix must retain draft safety")]
async fn semantic_driver_checks_draft_safety_at_an_interrupted_owned_prefix() {
    let (mut world, target_id, _) = draft_routing_world();
    let operation = exact_draft_operation(&world, &target_id);
    let target_identity = operation.identity.clone();
    world
        .published_mut(&target_id)
        .unwrap()
        .pull_requests
        .iter_mut()
        .find(|row| row.identity == target_identity)
        .unwrap()
        .open_fields_mut()
        .unwrap()
        .base = BaseKind::Owned;
    let conversions = prepared_draft_conversion(&operation);
    let interruption = Interruption::Draft { batch: 0, applied_aliases: Box::new([]) };
    let mut driver = WorldDriver::new(&mut world, Some(interruption));
    driver.convert_pull_requests_to_draft(conversions).await.unwrap();
}

fn assert_indeterminate_draft_converts_real_target(
    world: &mut DurableWorld,
    target_id: &GherritPrId,
    sibling_id: &GherritPrId,
    operation: TestDraft,
) {
    let sibling = world.published(sibling_id).unwrap().clone();
    let effect = world.resolve_draft(operation);
    assert_eq!(effect.resolved_id.as_ref(), Some(target_id));
    assert_eq!(world.apply_effect(&ExternalEffect::Draft(effect)), EffectOutcome::Indeterminate);
    assert!(
        world.selected_open_pull_request(target_id).unwrap().open_fields().unwrap().is_draft,
        "the service mutates the node target before the receipt mismatch is known"
    );
    assert_eq!(world.published(sibling_id), Some(&sibling));
}

#[test]
fn drafts_route_by_node_id_and_validate_complete_receipt_evidence_after_mutation() {
    let (initial, target_id, sibling_id) = draft_routing_world();
    let exact = exact_draft_operation(&initial, &target_id);

    let mut unknown_world = initial.clone();
    let unknown = TestDraft {
        identity: PullRequestIdentity::for_plan_test(999, "UNKNOWN_NODE"),
        ..exact.clone()
    };
    let unknown = unknown_world.resolve_draft(unknown);
    assert_eq!(unknown.resolved_id, None);
    let before_unknown = unknown_world.clone();
    assert_eq!(
        unknown_world.apply_effect(&ExternalEffect::Draft(unknown)),
        EffectOutcome::Indeterminate
    );
    assert_eq!(unknown_world, before_unknown, "an unknown node ID cannot select a durable row");

    let mut wrong_number_world = initial.clone();
    let wrong_number = TestDraft {
        identity: PullRequestIdentity::for_plan_test(
            exact.identity.number().get() + 1,
            exact.identity.node_id_for_test(),
        ),
        ..exact.clone()
    };
    assert_indeterminate_draft_converts_real_target(
        &mut wrong_number_world,
        &target_id,
        &sibling_id,
        wrong_number,
    );

    let mut wrong_change_world = initial.clone();
    assert_indeterminate_draft_converts_real_target(
        &mut wrong_change_world,
        &target_id,
        &sibling_id,
        TestDraft { id: sibling_id.clone(), ..exact.clone() },
    );

    let mut wrong_head_world = initial.clone();
    assert_indeterminate_draft_converts_real_target(
        &mut wrong_head_world,
        &target_id,
        &sibling_id,
        TestDraft { head_oid: oid(99), ..exact.clone() },
    );

    let mut wrong_base_world = initial.clone();
    let target_identity = wrong_base_world.identity(&target_id);
    wrong_base_world
        .published_mut(&target_id)
        .unwrap()
        .pull_requests
        .iter_mut()
        .find(|row| row.identity == target_identity)
        .unwrap()
        .open_fields_mut()
        .unwrap()
        .base = BaseKind::Owned;
    assert_indeterminate_draft_converts_real_target(
        &mut wrong_base_world,
        &target_id,
        &sibling_id,
        exact.clone(),
    );

    for default_branch in [
        DefaultBranch::new("trunk".to_owned(), initial.default_tip).unwrap(),
        DefaultBranch::new(DEFAULT_BRANCH.to_owned(), oid(99)).unwrap(),
    ] {
        let mut wrong_default_world = initial.clone();
        assert_indeterminate_draft_converts_real_target(
            &mut wrong_default_world,
            &target_id,
            &sibling_id,
            TestDraft { default_branch, ..exact.clone() },
        );
    }

    for merged in [false, true] {
        let mut terminal_world = initial.clone();
        if merged {
            terminal_world.merge_for_setup(&exact.identity);
        } else {
            terminal_world.close_for_setup(&exact.identity);
        }
        let before = terminal_world.clone();
        let terminal = terminal_world.resolve_draft(exact.clone());
        assert_eq!(terminal.resolved_id.as_ref(), Some(&target_id));
        assert_eq!(
            terminal_world.apply_effect(&ExternalEffect::Draft(terminal)),
            EffectOutcome::Indeterminate
        );
        assert_eq!(terminal_world, before, "a terminal target and every sibling stay unchanged");
    }
}

#[test]
fn updates_route_by_node_id_and_validate_the_complete_receipt_after_mutation() {
    let default = oid(1);
    let revision = LiteralRevision { head: oid(2), first_parent: default };
    let intent = root_intent(default, "Groute", revision);
    let mut world = DurableWorld::for_intents(default, &[&intent]);
    establish_marked(&mut world, &intent.first, BaseKind::Owned, "old body");

    let unknown = world.resolve_projection(TestPullRequestProjection::Update(TestUpdate {
        identity: PullRequestIdentity::for_plan_test(999, "UNKNOWN_NODE"),
        title: Some("not applied".to_owned()),
        body: Some("not applied".to_owned()),
        base_branch: Some(DEFAULT_BRANCH.to_owned()),
    }));
    let ProjectionEffect::Update(unknown) = unknown else { unreachable!() };
    assert_eq!(unknown.resolved_id, None);
    let before_unknown = world.clone();
    assert_eq!(
        world.apply_effect(&ExternalEffect::Projection(ProjectionEffect::Update(unknown))),
        EffectOutcome::Indeterminate
    );
    assert_eq!(world, before_unknown, "an unknown node ID cannot select a durable row");

    let identity = world.identity(&id("Groute"));
    let invalid_base = world.resolve_projection(TestPullRequestProjection::Update(TestUpdate {
        identity: identity.clone(),
        title: Some("not applied".to_owned()),
        body: Some("not applied".to_owned()),
        base_branch: Some(owned_base_name(&id("Gother"))),
    }));
    let ProjectionEffect::Update(invalid_base) = invalid_base else { unreachable!() };
    assert_eq!(invalid_base.resolved_id, Some(id("Groute")));
    let before_invalid_base = world.clone();
    assert_eq!(
        world.apply_effect(&ExternalEffect::Projection(ProjectionEffect::Update(invalid_base))),
        EffectOutcome::Indeterminate
    );
    assert_eq!(world, before_invalid_base, "base validation precedes every field mutation");

    let mismatched_receipt =
        world.resolve_projection(TestPullRequestProjection::Update(TestUpdate {
            identity: PullRequestIdentity::for_plan_test(
                identity.number().get() + 1,
                identity.node_id_for_test(),
            ),
            title: Some("new title".to_owned()),
            body: Some("new body".to_owned()),
            base_branch: Some(DEFAULT_BRANCH.to_owned()),
        }));
    let ProjectionEffect::Update(mismatched_receipt) = mismatched_receipt else { unreachable!() };
    assert_eq!(mismatched_receipt.resolved_id, Some(id("Groute")));
    assert_eq!(
        world.apply_effect(&ExternalEffect::Projection(ProjectionEffect::Update(
            mismatched_receipt
        ))),
        EffectOutcome::Indeterminate,
        "a wrong expected number makes the receipt indeterminate"
    );
    assert!(world.published(&id("Groute")).unwrap().marker.is_some());
    let fields = world.selected_open_pull_request(&id("Groute")).unwrap().open_fields().unwrap();
    assert_eq!(fields.base, BaseKind::Default);
    assert_eq!(fields.title, "new title");
    assert_eq!(fields.body, "new body");
}

#[test]
fn literal_services_do_not_enforce_cross_backend_stage_policy() {
    let default = oid(1);
    let revision = LiteralRevision { head: oid(2), first_parent: default };
    let intent = root_intent(default, "Gunmarked", revision);
    let mut world = DurableWorld::for_intents(default, &[&intent]);
    world.publish_for_setup(&intent.first.id, revision);
    world.open_for_setup(&intent.first.id, "old title", "old body");
    let operation = TestUpdate {
        identity: world.identity(&intent.first.id),
        title: Some("new title".to_owned()),
        body: None,
        base_branch: None,
    };
    let effect = world.resolve_projection(TestPullRequestProjection::Update(operation));
    assert_eq!(
        world.apply_effect(&ExternalEffect::Projection(effect)),
        EffectOutcome::Acknowledged,
        "GitHub can update an exact OPEN row without observing a Git marker"
    );
    assert_eq!(
        world.selected_open_pull_request(&intent.first.id).unwrap().open_fields().unwrap().title,
        "new title"
    );

    let marker_only = root_intent(
        default,
        "Gmarkerwithoutopen",
        LiteralRevision { head: oid(3), first_parent: default },
    );
    let mut marker_world = DurableWorld::for_intents(default, &[&marker_only]);
    marker_world.publish_for_setup(&marker_only.first.id, marker_only.first.desired);
    let marker = marker_identity(marker_only.first.desired.head, PullRequestNumber::for_test(999));
    assert_eq!(
        marker_world.apply_effect(&ExternalEffect::Marker(MarkerEffect {
            id: marker_only.first.id.clone(),
            marker,
        })),
        EffectOutcome::Acknowledged,
        "Git can publish a marker without observing GitHub lifecycle"
    );
    assert_eq!(marker_world.published(&marker_only.first.id).unwrap().marker, Some(marker));
}

#[tokio::test]
async fn every_small_attempt_prefix_restarts_from_literal_durable_state() {
    let intent = two_change_intent();
    let initial = DurableWorld::for_intents(oid(10), &[&intent]);
    let mut completed = initial.clone();
    let report = completed.run_attempt(&intent, &QueryVisibility::default(), None).await.unwrap();
    assert_eq!(report.outcome, AttemptOutcome::Acknowledged);
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
        assert_eq!(stopped.outcome, AttemptOutcome::Stopped(expected_stop));
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
    assert_eq!(complete.outcome, AttemptOutcome::Acknowledged);
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
        assert_eq!(stopped.outcome, AttemptOutcome::Stopped(StopReason::Indeterminate));
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
        assert_eq!(winner.outcome, AttemptOutcome::Acknowledged);
        assert_eq!(loser.outcome, AttemptOutcome::Stopped(StopReason::Indeterminate));
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
    for report in [&primary, &stale] {
        assert_eq!(report.outcome, AttemptOutcome::Acknowledged);
        assert_eq!(flatten(&report.trace.draft_conversions).len(), 1);
        assert_eq!(flatten(&report.trace.initial_refs).len(), 1);
        assert_eq!(flatten(&report.trace.projections).len(), 2);
    }
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
                stopped.trace.initial_refs,
                tuple_report.trace.initial_refs[..=stopped_batch]
            );
            let label = format!("initial-ref-batch-{stopped_batch}-applied-{applied}");
            let retry = run_acknowledged_retry(&mut world, &tuple_intent, &label).await;
            assert_eq!(
                flatten(&retry.trace.initial_refs),
                flatten(&tuple_report.trace.initial_refs[stopped_batch + usize::from(applied)..]),
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
    assert_eq!(rejected.outcome, AttemptOutcome::Stopped(StopReason::Rejected));
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
    assert_eq!(complete.outcome, AttemptOutcome::Acknowledged);
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
    assert_eq!(rejected.outcome, AttemptOutcome::Stopped(StopReason::Rejected));
    assert_eq!(rejected.trace.initial_refs, complete.trace.initial_refs);
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
                assert!(!rejected_world.public_branches.contains_key(branch));
            }
        }
    }

    let retry =
        rejected_world.run_attempt(&intent, &QueryVisibility::default(), None).await.unwrap();
    assert_eq!(retry.outcome, AttemptOutcome::Acknowledged);
    assert_eq!(retry.trace.initial_refs.as_slice(), std::slice::from_ref(&final_batch));
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
    assert_eq!(lost.outcome, AttemptOutcome::Stopped(StopReason::Indeterminate));
    assert_eq!(lost.trace.initial_refs, complete.trace.initial_refs);
    assert_eq!(public_branch_target(&lost_world), Some(intent.later.last().unwrap().desired.head));
    assert!(intent.iter().all(|local| {
        lost_world.published(&local.id).is_some_and(|published| {
            published.history.last() == local.desired && published.pull_requests.is_empty()
        })
    }));
    let lost_retry =
        lost_world.run_attempt(&intent, &QueryVisibility::default(), None).await.unwrap();
    assert_eq!(lost_retry.outcome, AttemptOutcome::Acknowledged);
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
    raced_world.public_branches.insert(public_branch_name(PUBLIC_BRANCH), oid(900));
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

    raced_world.public_branches.insert(public_branch_name(PUBLIC_BRANCH), oid(901));
    let stale = raced_world.execute_plan(stale_plan, None).await.unwrap();
    assert_eq!(stale.outcome, AttemptOutcome::Stopped(StopReason::Rejected));
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
    assert_eq!(raced_retry.outcome, AttemptOutcome::Acknowledged);
    assert_eq!(raced_retry.trace.initial_refs.len(), 1);
    let expected_retry = expected_public_retry_batch(expected_final, oid(901));
    assert_eq!(raced_retry.trace.initial_refs[0], expected_retry);
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
    world.public_branches.insert(public_branch_name(PUBLIC_BRANCH), oid(900));
    let stale_plan = world.plan(&intent, &QueryVisibility::default()).unwrap();
    world.public_branches.insert(public_branch_name(PUBLIC_BRANCH), oid(901));
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
    initial.public_branches.insert(public_branch_name(PUBLIC_BRANCH), oid(900));
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
    assert_eq!(competitor.outcome, AttemptOutcome::Acknowledged);
    assert_eq!(competitor.trace.initial_refs.len(), 1);
    assert_eq!(flatten(&competitor.trace.creates).len(), 1);
    assert_eq!(flatten(&competitor.trace.markers).len(), 1);
    assert_eq!(flatten(&competitor.trace.projections).len(), 1);
    assert_eq!(primary.outcome, AttemptOutcome::Stopped(StopReason::Rejected));
    assert_eq!(primary.trace.initial_refs, reference.trace.initial_refs);
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
    assert_eq!(retry.outcome, AttemptOutcome::Acknowledged);
    assert_eq!(retry.trace.initial_refs.len(), 1);
    assert_eq!(
        retry.trace.initial_refs[0],
        expected_public_retry_batch(final_batch, competitor_tip)
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
    assert_eq!(first.outcome, AttemptOutcome::Stopped(StopReason::Indeterminate));
    let duplicate = duplicate_world.execute_plan(duplicate_plan, None).await.unwrap();
    assert_eq!(duplicate.outcome, AttemptOutcome::Acknowledged);
    assert_eq!(duplicate.trace.initial_refs, first.trace.initial_refs);
    assert!(
        duplicate_world
            .run_attempt(&duplicate_intent, &QueryVisibility::default(), None)
            .await
            .unwrap()
            .trace
            .is_empty(),
        "an already-desired public ref receipt releases the preplanned duplicate"
    );
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
        assert_eq!(stopped.outcome, AttemptOutcome::Stopped(StopReason::Indeterminate));
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
    assert_eq!(first.outcome, AttemptOutcome::Stopped(StopReason::Indeterminate));
    let duplicate_creates = &first.trace.creates;
    let retained_identity = duplicate_world.identity(&duplicate_creates[0][0].id);
    let next_before = duplicate_world.next_identity;
    let duplicate = duplicate_world.execute_plan(stale_plan, None).await.unwrap();
    assert_eq!(duplicate.outcome, AttemptOutcome::Acknowledged);
    assert_eq!(duplicate.trace.creates.len(), 1);
    assert!(!duplicate.trace.markers.is_empty() && !duplicate.trace.projections.is_empty());
    assert_ne!(duplicate_world.identity(&duplicate_creates[0][0].id), retained_identity);
    assert!(duplicate_world.open_pull_requests(&duplicate_creates[0][1].id).next().is_some());
    assert_eq!(
        duplicate_world.next_identity,
        next_before + 2,
        "both stale create aliases append literal durable rows"
    );
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
    assert_eq!(first.outcome, AttemptOutcome::Stopped(StopReason::Indeterminate));

    let hidden = QueryVisibility::hiding(&world, [id("Ghidden")]);
    let duplicate = world.run_attempt(&intent, &hidden, None).await.unwrap();
    assert!(duplicate.trace.initial_refs.is_empty());
    assert_eq!(
        duplicate.trace.creates, first.trace.creates,
        "the stable create payload is identical"
    );
    assert_eq!(
        duplicate.outcome,
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
    assert_eq!(attempt.outcome, AttemptOutcome::Acknowledged);
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
            assert_eq!(report.outcome, AttemptOutcome::Acknowledged);
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
    assert_eq!(competing.outcome, AttemptOutcome::Stopped(StopReason::Indeterminate));
    assert_eq!(flatten(&competing.trace.initial_refs).len(), 1);
    assert!(competing.trace.markers.is_empty() && competing.trace.projections.is_empty());
    assert_eq!(
        primary.outcome,
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
    let initial = DurableWorld::for_intents(default, &[&intent]);
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
    assert_eq!(competing.outcome, AttemptOutcome::Acknowledged);
    assert_eq!(
        primary.outcome,
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
    assert_eq!(primary.outcome, AttemptOutcome::Acknowledged);
    assert_eq!(competing.outcome, AttemptOutcome::Acknowledged);
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
    assert_eq!(preview.outcome, AttemptOutcome::Stopped(StopReason::Indeterminate));
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
    assert_eq!(primary.outcome, AttemptOutcome::Acknowledged);
    assert_eq!(competing.outcome, AttemptOutcome::Acknowledged);
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
        Some(Interruption::Create { batch: 0, applied_aliases: Box::new([]) }),
    )
    .await
    .unwrap();
    assert_eq!(competing.outcome, AttemptOutcome::Stopped(StopReason::Indeterminate));
    assert_eq!(primary.outcome, AttemptOutcome::Stopped(StopReason::Rejected));
    assert_eq!(primary.trace.initial_refs.len(), 2);
    assert!(primary.trace.creates.is_empty());
    let last_id = &primary_tuple_intent.later.last().unwrap().id;
    assert_eq!(world.published(last_id).unwrap().history.last().head, oid(500));
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
    assert_eq!(competing.outcome, AttemptOutcome::Acknowledged);
    assert_eq!(primary.outcome, AttemptOutcome::Acknowledged);
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
    let initial = DurableWorld::for_intents(oid(10), &[&create_intent]);
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
        assert_eq!(competing.outcome, AttemptOutcome::Acknowledged);
        assert_eq!(primary.outcome, AttemptOutcome::Stopped(StopReason::Rejected));
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
        assert_eq!(preview.outcome, AttemptOutcome::Stopped(StopReason::Indeterminate));
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
        assert_eq!(competing.outcome, AttemptOutcome::Acknowledged);
        assert_eq!(primary.outcome, AttemptOutcome::Stopped(StopReason::Rejected));
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
        assert_eq!(primary.outcome, AttemptOutcome::Acknowledged);
        assert_eq!(competing.outcome, AttemptOutcome::Acknowledged);
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
    assert_ne!(same_world, after_first, "the second create appends a durable duplicate row");
    assert_eq!(same_world.open_pull_requests(&id("Gsame")).count(), 2);
    let retry =
        run_acknowledged_retry(&mut same_world, &same_intent, "identical-create-race").await;
    assert!(matches!(
        flatten(&retry.trace.projections).as_ref(),
        [ProjectionEffect::Close { identity, .. }] if identity.number().get() == 101
    ));
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
            })]
        );
        assert_eq!(retry.outcome, AttemptOutcome::Acknowledged);
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
    assert_eq!(
        indeterminate.outcome,
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
async fn stale_precomputed_projection_lands_safely_and_fresh_intent_repairs_it() {
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
    assert_eq!(stale.outcome, AttemptOutcome::Acknowledged);
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

fn three_open_root(marker_index: usize) -> (DurableWorld, LocalIntent, [PullRequestIdentity; 3]) {
    let default = oid(1);
    let revision = LiteralRevision { head: oid(2), first_parent: default };
    let intent = root_intent(default, "Gduplicates", revision);
    let mut world = DurableWorld::for_intents(default, &[&intent]);
    world.publish_for_setup(&intent.first.id, revision);
    for suffix in ["first", "second", "third"] {
        world.open_for_setup(&intent.first.id, &format!("stale {suffix}"), "stale body");
    }
    let identities: [PullRequestIdentity; 3] = world
        .open_pull_requests(&intent.first.id)
        .map(|row| row.identity.clone())
        .collect::<Vec<_>>()
        .try_into()
        .unwrap();
    world.mark_identity_for_setup(
        &intent.first.id,
        revision.head,
        &identities[marker_index],
        BaseKind::Default,
    );
    (world, intent, identities)
}

#[derive(serde::Serialize)]
struct AliasSubsetRecoverySnapshot {
    applied_aliases: Vec<usize>,
    durable_open_numbers: Vec<u32>,
    retry: Vec<Vec<ProjectionSnapshot>>,
}

#[tokio::test]
async fn mixed_projection_recovers_from_all_eight_durable_alias_subsets() {
    let (initial, intent, identities) = three_open_root(0);
    let mut completed = initial.clone();
    let complete = completed.run_attempt(&intent, &QueryVisibility::default(), None).await.unwrap();
    assert_eq!(complete.trace.projections.len(), 1);
    assert_eq!(complete.trace.projections[0].len(), 3);
    assert_model_snapshot!("mixed_duplicate_projection", projection_snapshot(&complete.trace));

    let mut snapshots = Vec::new();
    for mask in 0_u8..8 {
        let applied_aliases = (0..3).filter(|alias| mask & (1 << alias) != 0).collect::<Vec<_>>();
        let selected = validated_aliases(&applied_aliases, 3);
        let expected_retry = complete.trace.projections[0]
            .iter()
            .enumerate()
            .filter(|(alias, _)| !selected.contains(alias))
            .map(|(_, effect)| effect.clone())
            .collect::<Box<[_]>>();
        let mut world = initial.clone();
        let stopped = world
            .run_attempt(
                &intent,
                &QueryVisibility::default(),
                Some(Interruption::Projection {
                    batch: 0,
                    applied_aliases: applied_aliases.clone().into(),
                }),
            )
            .await
            .unwrap();
        assert_eq!(stopped.outcome, AttemptOutcome::Stopped(StopReason::Indeterminate));
        assert_eq!(stopped.trace.projections, complete.trace.projections);
        let durable_open_numbers = world
            .open_pull_requests(&intent.first.id)
            .map(|row| row.identity.number().get())
            .collect::<Vec<_>>();
        let retry = run_acknowledged_retry(&mut world, &intent, &format!("alias-{mask:03b}")).await;
        assert_eq!(flatten(&retry.trace.projections), expected_retry);
        snapshots.push(AliasSubsetRecoverySnapshot {
            applied_aliases,
            durable_open_numbers,
            retry: projection_snapshot(&retry.trace),
        });
        assert_eq!(world.open_pull_requests(&intent.first.id).count(), 1);
        assert_eq!(world.identity(&intent.first.id), identities[0]);
        assert_quiescent(&mut world, &intent, &format!("alias-{mask:03b}")).await;
    }
    assert_model_snapshot!("mixed_projection_alias_subsets", snapshots);
}

#[tokio::test]
async fn mixed_projection_multibatch_prefixes_replan_only_literal_remaining_work() {
    let default = oid(1);
    let revision = LiteralRevision { head: oid(2), first_parent: default };
    let intent = root_intent(default, "Gmanyduplicates", revision);
    let mut initial = DurableWorld::for_intents(default, &[&intent]);
    initial.publish_for_setup(&intent.first.id, revision);
    for index in 0..66 {
        initial.open_for_setup(&intent.first.id, &format!("stale {index}"), "stale body");
    }
    initial.mark_for_setup(&intent.first.id, revision.head, BaseKind::Default);
    let mut completed = initial.clone();
    let complete = completed.run_attempt(&intent, &QueryVisibility::default(), None).await.unwrap();
    assert_eq!(
        complete.trace.projections.iter().map(|batch| batch.len()).collect::<Vec<_>>(),
        [64, 2]
    );

    let cases = [
        ("first-none", 0, Box::new([]) as Box<[usize]>),
        ("first-partial", 0, Box::new([0, 31, 63])),
        ("first-all", 0, (0..64).collect()),
        ("second-none", 1, Box::new([])),
        ("second-close", 1, Box::new([0])),
        ("second-update", 1, Box::new([1])),
        ("second-all", 1, Box::new([0, 1])),
    ];
    for (label, batch, applied_aliases) in cases {
        let expected =
            unapplied_request_suffix(&complete.trace.projections, batch, &applied_aliases);
        let mut world = initial.clone();
        let stopped = world
            .run_attempt(
                &intent,
                &QueryVisibility::default(),
                Some(Interruption::Projection { batch, applied_aliases }),
            )
            .await
            .unwrap();
        assert_eq!(stopped.outcome, AttemptOutcome::Stopped(StopReason::Indeterminate));
        assert_eq!(stopped.trace.projections.len(), batch + 1, "{label}");
        let retry = run_acknowledged_retry(&mut world, &intent, label).await;
        assert_eq!(flatten(&retry.trace.projections), expected, "{label}");
        assert_quiescent(&mut world, &intent, label).await;
    }
}

#[tokio::test]
async fn different_create_contenders_release_only_the_marker_winners_projection() {
    let default = oid(1);
    let revision = LiteralRevision { head: oid(2), first_parent: default };
    let intent = root_intent(default, "Gmarkerrace", revision);
    let initial = DurableWorld::for_intents(default, &[&intent]);
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
    assert_eq!(competing.outcome, AttemptOutcome::Acknowledged);
    assert_eq!(primary.outcome, AttemptOutcome::Stopped(StopReason::Rejected));
    assert!(primary.trace.projections.is_empty());
    assert_eq!(world.open_pull_requests(&intent.first.id).count(), 2);
    assert_eq!(world.published(&intent.first.id).unwrap().marker.unwrap().number().get(), 101);
    assert_model_snapshot!(
        "winning_marker_contender_projection",
        projection_snapshot(&competing.trace)
    );

    let retry = run_acknowledged_retry(&mut world, &intent, "different-marker-contenders").await;
    assert!(matches!(
        flatten(&retry.trace.projections).as_ref(),
        [ProjectionEffect::Close { identity, .. }] if identity.number().get() == 100
    ));
    assert_quiescent(&mut world, &intent, "different-marker-contenders").await;
}

#[tokio::test]
async fn marker_selects_a_higher_row_and_closes_every_visible_lower_duplicate() {
    let (mut world, intent, identities) = three_open_root(2);
    let report = world.run_attempt(&intent, &QueryVisibility::default(), None).await.unwrap();
    assert_eq!(report.outcome, AttemptOutcome::Acknowledged);
    assert!(matches!(
        flatten(&report.trace.projections).as_ref(),
        [
            ProjectionEffect::Close { identity: first, .. },
            ProjectionEffect::Close { identity: second, .. },
            ProjectionEffect::Update(update),
        ] if first == &identities[0]
            && second == &identities[1]
            && update.operation.identity == identities[2]
    ));
    assert_eq!(world.identity(&intent.first.id), identities[2]);
    assert_quiescent(&mut world, &intent, "higher-marker-row").await;
}

#[tokio::test]
async fn captured_open_results_never_authorize_closing_rows_they_did_not_return() {
    let (mut world, intent, _identities) = three_open_root(2);
    let captured = QueryVisibility::stale(&world, [intent.first.id.clone()]);
    world.open_for_setup(&intent.first.id, "later duplicate", "later body");
    let later = world.latest_identity(&intent.first.id);

    let stale = world.run_attempt(&intent, &captured, None).await.unwrap();
    assert_eq!(stale.outcome, AttemptOutcome::Acknowledged);
    assert!(flatten(&stale.trace.projections).iter().all(|effect| match effect {
        ProjectionEffect::Close { identity, .. } => identity != &later,
        ProjectionEffect::Update(_) => true,
    }));
    assert!(world.exact_open_pull_request(&later).is_some());
    let repair = run_acknowledged_retry(&mut world, &intent, "later-unseen-row").await;
    assert!(matches!(
        flatten(&repair.trace.projections).as_ref(),
        [ProjectionEffect::Close { identity, .. }] if identity == &later
    ));
    assert_quiescent(&mut world, &intent, "later-unseen-row").await;

    let (mut world, intent, identities) = three_open_root(2);
    let captured = QueryVisibility::hiding_identities(&world, [identities[0].clone()]);
    world.run_attempt(&intent, &captured, None).await.unwrap();
    assert!(world.exact_open_pull_request(&identities[0]).is_some());
    let repair = run_acknowledged_retry(&mut world, &intent, "hidden-lower-row").await;
    assert!(matches!(
        flatten(&repair.trace.projections).as_ref(),
        [ProjectionEffect::Close { identity, .. }] if identity == &identities[0]
    ));
    assert_quiescent(&mut world, &intent, "hidden-lower-row").await;
}

#[tokio::test]
async fn delayed_create_after_canonical_projection_becomes_a_repairable_duplicate() {
    let default = oid(1);
    let revision = LiteralRevision { head: oid(2), first_parent: default };
    let intent = root_intent(default, "Gdelayed", revision);
    let initial = DurableWorld::for_intents(default, &[&intent]);
    let delayed = initial.plan(&intent, &QueryVisibility::default()).unwrap();
    let mut world = initial;
    world.run_attempt(&intent, &QueryVisibility::default(), None).await.unwrap();
    assert_eq!(world.identity(&intent.first.id).number().get(), 100);

    let stopped = world.execute_plan(delayed, None).await.unwrap();
    assert_eq!(stopped.outcome, AttemptOutcome::Stopped(StopReason::Rejected));
    assert!(stopped.trace.projections.is_empty());
    assert_eq!(world.open_pull_requests(&intent.first.id).count(), 2);
    let retry = run_acknowledged_retry(&mut world, &intent, "delayed-create").await;
    assert!(matches!(
        flatten(&retry.trace.projections).as_ref(),
        [ProjectionEffect::Close { identity, .. }] if identity.number().get() == 101
    ));
    assert_quiescent(&mut world, &intent, "delayed-create").await;
}

#[tokio::test]
async fn terminal_rows_are_absent_from_open_observation_and_never_duplicate_authority() {
    let (mut world, intent, identities) = three_open_root(0);
    world.close_for_setup(&identities[1]);
    world.merge_for_setup(&identities[2]);
    let terminal = [
        world.exact_pull_request(&identities[1]).unwrap().clone(),
        world.exact_pull_request(&identities[2]).unwrap().clone(),
    ];
    let observed = world.observed_open_rows(&intent.first.id, &QueryVisibility::default());
    assert_eq!(
        observed.iter().map(|row| row.identity.clone()).collect::<Vec<_>>(),
        [identities[0].clone()]
    );
    let report = world.run_attempt(&intent, &QueryVisibility::default(), None).await.unwrap();
    assert_eq!(report.outcome, AttemptOutcome::Acknowledged);
    assert!(
        flatten(&report.trace.projections)
            .iter()
            .all(|effect| matches!(effect, ProjectionEffect::Update(_)))
    );
    assert_eq!(world.exact_pull_request(&identities[1]), Some(&terminal[0]));
    assert_eq!(world.exact_pull_request(&identities[2]), Some(&terminal[1]));
    assert_quiescent(&mut world, &intent, "terminal-duplicates").await;
}

#[tokio::test]
async fn a_terminal_duplicate_makes_only_its_stale_projection_alias_indeterminate() {
    for merged in [false, true] {
        let (mut world, intent, identities) = three_open_root(0);
        let stale = world.plan(&intent, &QueryVisibility::default()).unwrap();
        if merged {
            world.merge_for_setup(&identities[1]);
        } else {
            world.close_for_setup(&identities[1]);
        }
        let terminal = world.exact_pull_request(&identities[1]).unwrap().clone();

        let stopped = world.execute_plan(stale, None).await.unwrap();
        assert_eq!(stopped.outcome, AttemptOutcome::Stopped(StopReason::Indeterminate));
        assert!(matches!(
            flatten(&stopped.trace.projections).as_ref(),
            [
                ProjectionEffect::Close { identity: first, .. },
                ProjectionEffect::Close { identity: second, .. },
                ProjectionEffect::Update(update),
            ] if first == &identities[1]
                && second == &identities[2]
                && update.operation.identity == identities[0]
        ));
        assert_eq!(world.exact_pull_request(&identities[1]), Some(&terminal));
        assert!(matches!(
            world.exact_pull_request(&identities[2]).unwrap().state,
            PullRequestState::Closed
        ));
        let retry = run_acknowledged_retry(&mut world, &intent, "terminal-duplicate-race").await;
        assert!(retry.trace.is_empty());
        assert_quiescent(&mut world, &intent, "terminal-duplicate-race").await;
    }
}

#[tokio::test]
async fn unusable_marker_selected_rows_fail_closed_without_promoting_a_duplicate() {
    for merged in [false, true] {
        let (mut world, intent, identities) = three_open_root(0);
        if merged {
            world.merge_for_setup(&identities[0]);
        } else {
            world.close_for_setup(&identities[0]);
        }
        let before = world.clone();
        let error =
            world.run_attempt(&intent, &QueryVisibility::default(), None).await.unwrap_err();
        assert!(error.to_string().contains("marker"));
        assert_eq!(world, before);
        assert_eq!(world.open_pull_requests(&intent.first.id).count(), 2);
    }

    let (mut world, intent, identities) = three_open_root(0);
    let hidden = QueryVisibility::hiding_identities(&world, [identities[0].clone()]);
    let before = world.clone();
    let error = world.run_attempt(&intent, &hidden, None).await.unwrap_err();
    assert!(error.to_string().contains("marker"));
    assert_eq!(world, before);
    assert_eq!(world.open_pull_requests(&intent.first.id).count(), 3);

    let default = oid(1);
    let revision = LiteralRevision { head: oid(2), first_parent: default };
    let intent = root_intent(default, "Gabsentmarker", revision);
    let mut world = DurableWorld::for_intents(default, &[&intent]);
    world.publish_for_setup(&intent.first.id, revision);
    world.set_marker_for_setup(&intent.first.id, revision.head, PullRequestNumber::for_test(999));
    assert!(world.plan(&intent, &QueryVisibility::default()).is_err());
}

#[tokio::test]
async fn unmarked_terminal_rows_grant_no_authority_but_do_not_block_a_new_contender() {
    for merged in [false, true] {
        let default = oid(1);
        let revision = LiteralRevision { head: oid(2), first_parent: default };
        let intent = root_intent(default, "Gunmarkedterminal", revision);
        let mut world = DurableWorld::for_intents(default, &[&intent]);
        world.publish_for_setup(&intent.first.id, revision);
        world.open_for_setup(&intent.first.id, "terminal", "terminal body");
        let terminal = world.identity(&intent.first.id);
        if merged {
            world.merge_for_setup(&terminal);
        } else {
            world.close_for_setup(&terminal);
        }

        let report = world.run_attempt(&intent, &QueryVisibility::default(), None).await.unwrap();
        assert_eq!(report.outcome, AttemptOutcome::Acknowledged);
        assert_eq!(flatten(&report.trace.creates).len(), 1);
        assert_eq!(world.identity(&intent.first.id).number().get(), 101);
        assert_eq!(world.published(&intent.first.id).unwrap().marker.unwrap().number().get(), 101);
        assert_quiescent(&mut world, &intent, "unmarked-terminal").await;
    }
}

#[tokio::test]
async fn an_exact_closed_canonical_can_reopen_without_changing_marker_identity() {
    let default = oid(1);
    let revision = LiteralRevision { head: oid(2), first_parent: default };
    let intent = root_intent(default, "Greopen", revision);
    let mut world = DurableWorld::for_intents(default, &[&intent]);
    establish_marked(&mut world, &intent.first, BaseKind::Default, "closed body");
    let identity = world.identity(&intent.first.id);
    let marker = world.published(&intent.first.id).unwrap().marker.unwrap();
    world.close_for_setup(&identity);
    assert!(world.plan(&intent, &QueryVisibility::default()).is_err());

    world.reopen_for_setup(&identity, "reopened", "reopened body", BaseKind::Owned);
    let report = world.run_attempt(&intent, &QueryVisibility::default(), None).await.unwrap();
    assert_eq!(report.outcome, AttemptOutcome::Acknowledged);
    assert_eq!(world.identity(&intent.first.id), identity);
    assert_eq!(world.published(&intent.first.id).unwrap().marker, Some(marker));
    assert_quiescent(&mut world, &intent, "closed-canonical-reopen").await;
}

#[tokio::test]
async fn stale_canonical_updates_cannot_mutate_rows_which_closed_or_merged() {
    for merged in [false, true] {
        let default = oid(1);
        let revision = LiteralRevision { head: oid(2), first_parent: default };
        let intent = root_intent(default, "Gterminalupdate", revision);
        let mut world = DurableWorld::for_intents(default, &[&intent]);
        establish_marked(&mut world, &intent.first, BaseKind::Default, "stale body");
        let stale_plan = world.plan(&intent, &QueryVisibility::default()).unwrap();
        let stale_visibility = QueryVisibility::stale(&world, [intent.first.id.clone()]);
        let identity = world.identity(&intent.first.id);
        if merged {
            world.merge_for_setup(&identity);
        } else {
            let close = world.resolve_close(identity.clone());
            assert_eq!(
                world.apply_effect(&ExternalEffect::Projection(close)),
                EffectOutcome::Acknowledged
            );
        }
        let terminal = world.clone();

        let stopped = world.execute_plan(stale_plan, None).await.unwrap();
        assert_eq!(stopped.outcome, AttemptOutcome::Stopped(StopReason::Indeterminate));
        assert_eq!(world, terminal);
        let stopped = world.run_attempt(&intent, &stale_visibility, None).await.unwrap();
        assert_eq!(stopped.outcome, AttemptOutcome::Stopped(StopReason::Indeterminate));
        assert_eq!(world, terminal);
        assert!(world.plan(&intent, &QueryVisibility::default()).is_err());
    }
}

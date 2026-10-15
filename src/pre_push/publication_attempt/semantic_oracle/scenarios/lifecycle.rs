use super::*;

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
        repository_id: REPOSITORY_ID.to_owned(),
        identity: PullRequestIdentity::for_plan_test(999, "UNKNOWN_NODE"),
        title: Some("not applied".to_owned()),
        body: Some("not applied".to_owned()),
        base_branch: Some(DEFAULT_BRANCH.to_owned()),
        expected_base: DEFAULT_BRANCH.to_owned(),
        expected_revision: super::super::super::refs::PublicationRevision::new(
            revision.head,
            revision.first_parent,
        )
        .unwrap(),
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
        repository_id: REPOSITORY_ID.to_owned(),
        identity: identity.clone(),
        title: Some("not applied".to_owned()),
        body: Some("not applied".to_owned()),
        base_branch: Some(owned_base_name(&id("Gother"))),
        expected_base: owned_base_name(&id("Gother")),
        expected_revision: super::super::super::refs::PublicationRevision::new(
            revision.head,
            revision.first_parent,
        )
        .unwrap(),
    }));
    let ProjectionEffect::Update(invalid_base) = invalid_base else { unreachable!() };
    assert_eq!(invalid_base.resolved_id, Some(id("Groute")));
    let before_invalid_base = world.clone();
    assert_eq!(
        world.apply_effect(&ExternalEffect::Projection(ProjectionEffect::Update(invalid_base))),
        EffectOutcome::Indeterminate
    );
    assert_eq!(world, before_invalid_base, "base validation precedes every field mutation");

    let wrong_repository =
        world.resolve_projection(TestPullRequestProjection::Update(TestUpdate {
            repository_id: "OTHER_REPOSITORY".to_owned(),
            identity: identity.clone(),
            title: Some("repository mismatch title".to_owned()),
            body: Some("repository mismatch body".to_owned()),
            base_branch: None,
            expected_base: owned_base_name(&id("Groute")),
            expected_revision: super::super::super::refs::PublicationRevision::new(
                revision.head,
                revision.first_parent,
            )
            .unwrap(),
        }));
    assert_eq!(
        world.apply_effect(&ExternalEffect::Projection(wrong_repository)),
        EffectOutcome::Indeterminate
    );
    let fields = world.selected_open_pull_request(&id("Groute")).unwrap().open_fields().unwrap();
    assert_eq!(fields.title, "repository mismatch title");
    assert_eq!(fields.body, "repository mismatch body");

    let mismatched_receipt =
        world.resolve_projection(TestPullRequestProjection::Update(TestUpdate {
            repository_id: REPOSITORY_ID.to_owned(),
            identity: PullRequestIdentity::for_plan_test(
                identity.number().get() + 1,
                identity.node_id_for_test(),
            ),
            title: Some("new title".to_owned()),
            body: Some("new body".to_owned()),
            base_branch: Some(DEFAULT_BRANCH.to_owned()),
            expected_base: DEFAULT_BRANCH.to_owned(),
            expected_revision: super::super::super::refs::PublicationRevision::new(
                revision.head,
                revision.first_parent,
            )
            .unwrap(),
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
        repository_id: REPOSITORY_ID.to_owned(),
        identity: world.identity(&intent.first.id),
        title: Some("new title".to_owned()),
        body: None,
        base_branch: None,
        expected_base: owned_base_name(&intent.first.id),
        expected_revision: super::super::super::refs::PublicationRevision::new(
            revision.head,
            revision.first_parent,
        )
        .unwrap(),
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
        marker_world
            .apply_effect(&ExternalEffect::Marker(marker_effect(&marker_only.first.id, marker))),
        EffectOutcome::Acknowledged,
        "Git can publish a marker without observing GitHub lifecycle"
    );
    assert_eq!(marker_world.published(&marker_only.first.id).unwrap().marker, Some(marker));
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
        assert_attempt_outcome!(stopped, AttemptOutcome::Stopped(StopReason::Indeterminate));
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
        assert_attempt_outcome!(stopped, AttemptOutcome::Stopped(StopReason::Indeterminate));
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
    assert_attempt_outcome!(primary, AttemptOutcome::Stopped(StopReason::Rejected));
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
    assert_attempt_outcome!(report, AttemptOutcome::Acknowledged);
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
    assert_attempt_outcome!(stale, AttemptOutcome::Acknowledged);
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
    let mut initial = DurableWorld::for_intents(default, &[&intent]);
    initial.publish_for_setup(&intent.first.id, intent.first.desired);
    let delayed = initial.plan(&intent, &QueryVisibility::default()).unwrap();
    let mut world = initial;
    world.run_attempt(&intent, &QueryVisibility::default(), None).await.unwrap();
    assert_eq!(world.identity(&intent.first.id).number().get(), 100);

    let stopped = world.execute_plan(delayed, None).await.unwrap();
    assert_attempt_outcome!(stopped, AttemptOutcome::Stopped(StopReason::Rejected));
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
    assert_attempt_outcome!(report, AttemptOutcome::Acknowledged);
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
        assert_attempt_outcome!(stopped, AttemptOutcome::Stopped(StopReason::Indeterminate));
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
        assert_attempt_outcome!(report, AttemptOutcome::Acknowledged);
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
    assert_attempt_outcome!(report, AttemptOutcome::Acknowledged);
    assert_eq!(world.identity(&intent.first.id), identity);
    assert_eq!(world.published(&intent.first.id).unwrap().marker, Some(marker));
    assert_quiescent(&mut world, &intent, "closed-canonical-reopen").await;
}

#[tokio::test]
async fn terminal_mutations_may_apply_but_unusable_receipts_stop_later_effects() {
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
        assert_attempt_outcome!(stopped, AttemptOutcome::Stopped(StopReason::Indeterminate));
        let row = world.exact_pull_request(&identity).unwrap();
        assert_eq!(row.state, terminal.exact_pull_request(&identity).unwrap().state);
        assert_eq!(
            row.terminal_fields.as_ref().unwrap().body,
            update_effects(&stopped.trace)[0].operation.body.clone().unwrap()
        );
        let after = world.clone();
        let stopped = world.run_attempt(&intent, &stale_visibility, None).await.unwrap();
        assert_attempt_outcome!(stopped, AttemptOutcome::Stopped(StopReason::Indeterminate));
        assert_eq!(world, after);
        assert!(world.plan(&intent, &QueryVisibility::default()).is_err());
    }
}

#[test]
fn field_only_receipt_rejects_an_unexpected_current_base_after_applying_fields() {
    let default = oid(1);
    let revision = LiteralRevision { head: oid(2), first_parent: default };
    let intent = root_intent(default, "Gretarget", revision);
    let mut world = DurableWorld::for_intents(default, &[&intent]);
    establish_marked(&mut world, &intent.first, BaseKind::Default, "old body");
    let effect = world.resolve_projection(TestPullRequestProjection::Update(TestUpdate {
        repository_id: REPOSITORY_ID.to_owned(),
        identity: world.identity(&intent.first.id),
        title: Some("new title".to_owned()),
        body: None,
        base_branch: None,
        expected_base: owned_base_name(&intent.first.id),
        expected_revision: super::super::super::refs::PublicationRevision::new(
            revision.head,
            revision.first_parent,
        )
        .unwrap(),
    }));
    assert_eq!(
        world.apply_effect(&ExternalEffect::Projection(effect)),
        EffectOutcome::Indeterminate
    );
    let fields = world.selected_open_pull_request(&intent.first.id).unwrap().open_fields().unwrap();
    assert_eq!(fields.title, "new title");
    assert_eq!(fields.base, BaseKind::Default);
}

#[test]
fn update_receipts_read_literal_refs_after_applying_the_patch() {
    let default = oid(1);
    let revision = LiteralRevision { head: oid(2), first_parent: default };
    let intent = root_intent(default, "Greceipt", revision);
    for changed_ref in ["refs/heads/Greceipt", "refs/heads/gherrit-bases/Greceipt"] {
        let mut world = DurableWorld::for_intents(default, &[&intent]);
        establish_marked(&mut world, &intent.first, BaseKind::Owned, "old body");
        let effect = world.resolve_projection(TestPullRequestProjection::Update(TestUpdate {
            repository_id: REPOSITORY_ID.to_owned(),
            identity: world.identity(&intent.first.id),
            title: Some("applied title".to_owned()),
            body: None,
            base_branch: None,
            expected_base: owned_base_name(&intent.first.id),
            expected_revision: PublicationRevision::new(revision.head, revision.first_parent)
                .unwrap(),
        }));
        // Deliberately change the service's literal ref without rewriting the
        // separate history model; the receipt must observe the actual ref.
        world.git_refs.0.insert(changed_ref.to_owned(), oid(99).to_string());
        let ProjectionEffect::Update(effect) = effect else { unreachable!() };
        assert_eq!(world.apply_update(&effect), EffectOutcome::Indeterminate);
        assert_eq!(
            world
                .selected_open_pull_request(&intent.first.id)
                .unwrap()
                .open_fields()
                .unwrap()
                .title,
            "applied title"
        );
    }
}

#[tokio::test]
async fn updates_overwrite_concurrent_edits_to_formerly_equal_content() {
    let default = oid(1);
    let revision = LiteralRevision { head: oid(2), first_parent: default };
    let intent = root_intent(default, "Gcontent", revision);
    let mut initial = DurableWorld::for_intents(default, &[&intent]);
    establish_marked(&mut initial, &intent.first, BaseKind::Default, "old body");
    let initial = assert_restart_converges(initial, &intent, "initial content").await;
    let identity = initial.identity(&intent.first.id);
    let desired_body = initial
        .selected_open_pull_request(&intent.first.id)
        .unwrap()
        .open_fields()
        .unwrap()
        .body
        .clone();
    for equal_field in ["title", "body"] {
        let mut world = initial.clone();
        {
            let fields = world
                .published_mut(&intent.first.id)
                .unwrap()
                .pull_requests
                .iter_mut()
                .find(|row| row.identity == identity)
                .unwrap()
                .open_fields_mut()
                .unwrap();
            if equal_field == "title" {
                fields.body = "stale body".to_owned();
            } else {
                fields.title = "stale title".to_owned();
            }
        }
        let plan = world.plan(&intent, &QueryVisibility::default()).unwrap();
        {
            let fields = world
                .published_mut(&intent.first.id)
                .unwrap()
                .pull_requests
                .iter_mut()
                .find(|row| row.identity == identity)
                .unwrap()
                .open_fields_mut()
                .unwrap();
            if equal_field == "title" {
                fields.title = "concurrent title".to_owned();
            } else {
                fields.body = "concurrent body".to_owned();
            }
        }
        let run = world.execute_plan(plan, None).await.unwrap();
        assert_attempt_outcome!(run, AttemptOutcome::Acknowledged);
        let updates = update_effects(&run.trace);
        assert_eq!(updates.len(), 1);
        assert_eq!(updates[0].operation.title.as_deref(), Some(intent.first.title.as_str()));
        assert_eq!(updates[0].operation.body.as_deref(), Some(desired_body.as_str()));
        let fields =
            world.selected_open_pull_request(&intent.first.id).unwrap().open_fields().unwrap();
        assert_eq!(fields.title, intent.first.title);
        assert_eq!(fields.body, desired_body);
    }
}

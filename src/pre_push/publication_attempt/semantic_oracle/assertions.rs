use super::*;

pub(super) fn owned_base_name(id: &GherritPrId) -> String {
    format!("gherrit-bases/{}", id.as_str())
}

pub(super) fn apply_atomic_git_batch<T: Clone>(
    world: &mut DurableWorld,
    batch: &[T],
    wrap: impl Fn(T) -> ExternalEffect,
) -> EffectOutcome {
    let before = world.clone();
    for effect in batch.iter().cloned().map(wrap) {
        if world.apply_effect(&effect) != EffectOutcome::Acknowledged {
            *world = before;
            return EffectOutcome::Rejected;
        }
    }
    EffectOutcome::Acknowledged
}

pub(super) fn flatten<T: Clone>(batches: &[Box<[T]>]) -> Box<[T]> {
    batches.iter().flat_map(|batch| batch.iter().cloned()).collect()
}

pub(super) fn update_effects(trace: &AttemptTrace) -> Box<[UpdateEffect]> {
    only_updates(&flatten(&trace.projections))
}

pub(super) fn only_updates(effects: &[ProjectionEffect]) -> Box<[UpdateEffect]> {
    effects
        .iter()
        .filter_map(|effect| match effect {
            ProjectionEffect::Close { .. } => None,
            ProjectionEffect::Update(update) => Some(update.clone()),
        })
        .collect()
}

pub(super) async fn assert_restart_converges(
    mut world: DurableWorld,
    intent: &LocalIntent,
    label: &str,
) -> DurableWorld {
    run_acknowledged_retry(&mut world, intent, label).await;
    assert_quiescent(&mut world, intent, label).await;
    world
}

/// Runs the exact first attempt after an interruption and returns its trace.
///
/// Keeping this attempt observable lets restart tests prove which durable
/// aliases were omitted, rather than checking only eventual convergence.
pub(super) async fn run_acknowledged_retry(
    world: &mut DurableWorld,
    intent: &LocalIntent,
    label: &str,
) -> AttemptReport {
    let retry = world
        .run_attempt(intent, &QueryVisibility::default(), None)
        .await
        .unwrap_or_else(|error| panic!("{label}: fresh planning failed: {error}"));
    assert_eq!(
        retry.outcome,
        AttemptOutcome::Acknowledged,
        "{label}: a fresh stable-intent attempt must receive usable acknowledgements"
    );
    retry
}

pub(super) async fn assert_quiescent(world: &mut DurableWorld, intent: &LocalIntent, label: &str) {
    let done = world
        .run_attempt(intent, &QueryVisibility::default(), None)
        .await
        .unwrap_or_else(|error| panic!("{label}: final observation failed: {error}"));
    assert_eq!(
        done.outcome,
        AttemptOutcome::Acknowledged,
        "{label}: a quiescent attempt must still complete with usable acknowledgements"
    );
    assert!(done.trace.is_empty(), "{label}: the next fresh attempt must have no durable work");
}

pub(super) fn two_change_intent() -> LocalIntent {
    let default = oid(10);
    let root = LiteralRevision { head: oid(20), first_parent: default };
    let child = LiteralRevision { head: oid(21), first_parent: root.head };
    LocalIntent::new(
        default,
        [
            local_change("Groot", root, "Root title", "Root body"),
            local_change("Gchild", child, "Child title", "Child body"),
        ],
    )
}

pub(super) fn many_bounded_ids_intent(change_count: usize, id_bytes: usize) -> LocalIntent {
    assert!((8..=128).contains(&id_bytes));
    let default = oid(1);
    let mut first_parent = default;
    LocalIntent::new(
        default,
        (0..change_count).map(|index| {
            let head = oid(u16::try_from(index + 2).unwrap());
            let prefix = format!("G{index:04}");
            let suffix = "a".repeat(id_bytes - prefix.len());
            let change = local_change(
                &format!("{prefix}{suffix}"),
                LiteralRevision { head, first_parent },
                &format!("Change {index}"),
                &format!("Body for change {index}"),
            );
            first_parent = head;
            change
        }),
    )
}

pub(super) fn public_multibatch_intent() -> LocalIntent {
    many_bounded_ids_intent(30, 128).with_public_branch(PUBLIC_BRANCH)
}

pub(super) fn public_branch_target(world: &DurableWorld) -> Option<ObjectId> {
    world.public_branch_target(&public_branch_name(PUBLIC_BRANCH))
}

pub(super) fn mixed_final_batch(trace: &AttemptTrace) -> (usize, &[InitialRefEffect]) {
    assert!(trace.initial_refs.len() > 1, "the fixture crosses the initial-ref push budget");
    let final_index = trace.initial_refs.len() - 1;
    let final_batch = &trace.initial_refs[final_index];
    assert!(final_batch.iter().any(|effect| matches!(effect, InitialRefEffect::Tuple(_))));
    assert!(
        final_batch.iter().any(|effect| matches!(effect, InitialRefEffect::PublicBranch { .. })),
        "the public projection shares the last tuple batch"
    );
    assert_eq!(
        flatten(&trace.initial_refs)
            .iter()
            .filter(|effect| matches!(effect, InitialRefEffect::PublicBranch { .. }))
            .count(),
        1,
        "only the final initial-ref batch contains the public projection"
    );
    (final_index, final_batch)
}

pub(super) fn assert_tuple_effects_published(world: &DurableWorld, effects: &[InitialRefEffect]) {
    for effect in effects {
        let InitialRefEffect::Tuple(tuple) = effect else {
            panic!("the public projection is ordered after every earlier tuple batch")
        };
        assert_eq!(world.published(&tuple.id).unwrap().history.last(), tuple.desired);
    }
}

pub(super) fn expected_public_retry_batch(
    final_batch: &[InitialRefEffect],
    current: ObjectId,
) -> Box<[InitialRefEffect]> {
    final_batch
        .iter()
        .cloned()
        .map(|effect| match effect {
            InitialRefEffect::PublicBranch { branch, desired, .. } => {
                InitialRefEffect::PublicBranch { branch, expected: Some(current), desired }
            }
            tuple @ InitialRefEffect::Tuple(_) => tuple,
        })
        .collect()
}

const MULTI_REQUEST_BODY_BYTES_BEFORE_JSON_ESCAPING: usize = 90_000;

pub(super) fn multi_request_intent() -> LocalIntent {
    let default = oid(10);
    let body = "\u{1}".repeat(MULTI_REQUEST_BODY_BYTES_BEFORE_JSON_ESCAPING);
    let mut first_parent = default;
    LocalIntent::new(
        default,
        (0..3).map(|index| {
            let head = oid(u16::try_from(index + 20).unwrap());
            let change = local_change(
                &format!("G{index}"),
                LiteralRevision { head, first_parent },
                &format!("Change {index}"),
                &body,
            );
            first_parent = head;
            change
        }),
    )
}

#[derive(serde::Serialize)]
pub(super) struct CreateSnapshot {
    pub(super) id: String,
    pub(super) title: String,
    pub(super) head_oid: String,
    pub(super) base_oid: String,
    pub(super) body: Box<[String]>,
}

pub(super) fn create_snapshot(trace: &AttemptTrace) -> Vec<CreateSnapshot> {
    flatten(&trace.creates)
        .iter()
        .map(|create| CreateSnapshot {
            id: create.id.as_str().to_owned(),
            title: create.title.clone(),
            head_oid: create.head_oid.to_string(),
            base_oid: create.base_oid.to_string(),
            // `split` keeps a final empty element when the body ends in LF,
            // so the readable line sequence still preserves exact bytes.
            body: create.body.split('\n').map(str::to_owned).collect(),
        })
        .collect()
}

#[derive(serde::Serialize)]
pub(super) struct UpdateSnapshot {
    pub(super) id: String,
    pub(super) number: u32,
    pub(super) node_id: String,
    pub(super) title: Option<String>,
    pub(super) body: Option<Box<[String]>>,
    pub(super) base_branch: Option<String>,
}

pub(super) fn update_snapshot(trace: &AttemptTrace) -> Vec<UpdateSnapshot> {
    update_effects(trace)
        .iter()
        .map(|update| UpdateSnapshot {
            id: update
                .resolved_id
                .as_ref()
                .expect("a planned update resolves to its observed OPEN row")
                .as_str()
                .to_owned(),
            number: update.operation.identity.number().get(),
            node_id: update.operation.identity.node_id_for_test().to_owned(),
            title: update.operation.title.clone(),
            body: update
                .operation
                .body
                .as_deref()
                .map(|body| body.split('\n').map(str::to_owned).collect()),
            base_branch: update.operation.base_branch.clone(),
        })
        .collect()
}

#[derive(serde::Serialize)]
#[serde(tag = "operation", rename_all = "snake_case")]
pub(super) enum ProjectionSnapshot {
    Close { id: String, number: u32, node_id: String },
    Update(UpdateSnapshot),
}

pub(super) fn projection_snapshot(trace: &AttemptTrace) -> Vec<Vec<ProjectionSnapshot>> {
    trace
        .projections
        .iter()
        .map(|batch| {
            batch
                .iter()
                .map(|effect| match effect {
                    ProjectionEffect::Close { resolved_id, identity } => {
                        ProjectionSnapshot::Close {
                            id: resolved_id
                                .as_ref()
                                .expect("a planned close resolves to its observed OPEN row")
                                .as_str()
                                .to_owned(),
                            number: identity.number().get(),
                            node_id: identity.node_id_for_test().to_owned(),
                        }
                    }
                    ProjectionEffect::Update(update) => {
                        ProjectionSnapshot::Update(UpdateSnapshot {
                            id: update
                                .resolved_id
                                .as_ref()
                                .expect("a planned update resolves to its observed OPEN row")
                                .as_str()
                                .to_owned(),
                            number: update.operation.identity.number().get(),
                            node_id: update.operation.identity.node_id_for_test().to_owned(),
                            title: update.operation.title.clone(),
                            body: update.operation.body.as_deref().map(|body| {
                                body.split('\n').map(str::to_owned).collect::<Box<[_]>>()
                            }),
                            base_branch: update.operation.base_branch.clone(),
                        })
                    }
                })
                .collect()
        })
        .collect()
}

#[derive(serde::Serialize)]
pub(super) struct MultiRequestUpdateRestartSnapshot {
    pub(super) before_retry: Vec<UpdateSnapshot>,
    pub(super) retry: Vec<UpdateSnapshot>,
}

pub(super) fn multi_request_update_snapshot(trace: &AttemptTrace) -> Vec<UpdateSnapshot> {
    const PAYLOAD_PLACEHOLDER: &str = "{ repeated: U+0001, count: 90000 }";

    let payload = "\u{1}".repeat(MULTI_REQUEST_BODY_BYTES_BEFORE_JSON_ESCAPING);
    update_snapshot(trace)
        .into_iter()
        .map(|mut update| {
            let body = update
                .body
                .take()
                .expect("every multi-request update carries its rendered body")
                .join("\n");
            assert_eq!(
                body.match_indices(&payload).count(),
                1,
                "the exact large fixture payload occurs once in each rendered body"
            );
            let body = body.replacen(&payload, PAYLOAD_PLACEHOLDER, 1);
            assert!(
                !body.contains('\u{1}'),
                "no repeated fixture byte remains outside the exact replaced payload"
            );
            update.body = Some(body.split('\n').map(str::to_owned).collect());
            update
        })
        .collect()
}

/// Returns exactly the request aliases which a fresh observation must still
/// plan after one response became indeterminate.
pub(super) fn unapplied_request_suffix<T: Clone>(
    complete: &[Box<[T]>],
    interrupted_batch: usize,
    applied_aliases: &[usize],
) -> Box<[T]> {
    let interrupted =
        complete.get(interrupted_batch).expect("the interruption selects a complete request batch");
    let applied = validated_aliases(applied_aliases, interrupted.len());
    complete
        .iter()
        .enumerate()
        .skip(interrupted_batch)
        .flat_map(|(batch_index, batch)| {
            let applied = &applied;
            batch
                .iter()
                .enumerate()
                .filter(move |(alias, _)| {
                    batch_index != interrupted_batch || !applied.contains(alias)
                })
                .map(|(_, effect)| effect.clone())
        })
        .collect()
}

/// Returns exactly the request aliases which were durable before an
/// indeterminate response stopped one request.
pub(super) fn applied_request_prefix<T: Clone>(
    complete: &[Box<[T]>],
    interrupted_batch: usize,
    applied_aliases: &[usize],
) -> Box<[T]> {
    let interrupted =
        complete.get(interrupted_batch).expect("the interruption selects a complete request batch");
    let applied = validated_aliases(applied_aliases, interrupted.len());
    complete
        .iter()
        .enumerate()
        .take(interrupted_batch + 1)
        .flat_map(|(batch_index, batch)| {
            let applied = &applied;
            batch
                .iter()
                .enumerate()
                .filter(move |(alias, _)| {
                    batch_index != interrupted_batch || applied.contains(alias)
                })
                .map(|(_, effect)| effect.clone())
        })
        .collect()
}

pub(super) fn assert_interrupted_create_state(
    world: &DurableWorld,
    intent: &LocalIntent,
    batch: &[TestCreate],
    applied_aliases: &[usize],
) {
    let applied = validated_aliases(applied_aliases, batch.len());
    for (alias, create) in batch.iter().enumerate() {
        let local = intent
            .iter()
            .find(|local| local.id == create.id)
            .expect("every planned create belongs to the local intent");
        assert_eq!(create.title, local.title);
        assert_eq!(create.head_oid, local.desired.head);
        assert_eq!(create.base_oid, local.desired.first_parent);
        let published = world
            .published(&create.id)
            .expect("the create stage follows durable tuple publication");
        let current = published.history.last();
        assert_eq!(current.head, create.head_oid);
        assert_eq!(current.first_parent, create.base_oid);
        let open = world.open_pull_requests(&create.id).collect::<Vec<_>>();
        if applied.contains(&alias) {
            assert_eq!(open.len(), 1, "a selected create alias establishes one OPEN row");
            let fields = open[0].open_fields().unwrap();
            assert_eq!(fields.title, create.title);
            assert!(fields.is_draft, "every created pull request must be durably draft");
            assert!(
                fields.body == create.body,
                "durable create body for {} differs from its applied create ({} != {} bytes)",
                create.id.as_str(),
                fields.body.len(),
                create.body.len()
            );
        } else {
            assert!(open.is_empty(), "an unselected create alias cannot establish an OPEN row");
        }
    }
}

pub(super) fn assert_interrupted_create_request_prefix(
    world: &DurableWorld,
    intent: &LocalIntent,
    complete: &[Box<[TestCreate]>],
    interrupted_batch: usize,
    applied_aliases: &[usize],
) {
    let applied = applied_request_prefix(complete, interrupted_batch, applied_aliases)
        .into_iter()
        .map(|create| (create.id.clone(), create))
        .collect::<HashMap<_, _>>();
    assert_eq!(
        applied.len(),
        complete.iter().take(interrupted_batch).map(|batch| batch.len()).sum::<usize>()
            + applied_aliases.len(),
        "one interrupted create prefix cannot repeat a local change"
    );
    for create in flatten(complete) {
        let selected = applied.get(&create.id);
        assert_interrupted_create_state(
            world,
            intent,
            std::slice::from_ref(&create),
            if selected.is_some() { &[0] } else { &[] },
        );
    }
}

pub(super) fn body_recipes(world: &DurableWorld, intent: &LocalIntent) -> StackBodyRecipes {
    let default = DefaultBranch::new(DEFAULT_BRANCH.to_owned(), world.default_tip).unwrap();
    let stack = LocalStack::for_plan_test(
        default,
        intent.iter().map(|local| {
            (
                local.id.clone(),
                local.desired.head,
                local.desired.first_parent,
                local.title.clone(),
                local.commit_body.clone(),
            )
        }),
    );
    let histories = intent
        .iter()
        .map(|local| {
            let published = world.published(&local.id).expect("every local change is published");
            validated_history(
                local.id.clone(),
                &published
                    .history
                    .iter()
                    .map(|revision| (revision.head, revision.first_parent))
                    .collect::<Vec<_>>(),
                (local.desired.head, local.desired.first_parent),
                published.marker.map(MarkerIdentity::number),
            )
        })
        .collect();
    StackBodyRecipes::new(&PushDestination::for_test(), None, stack, histories).unwrap()
}

pub(super) fn assert_complete_create_stage(
    retry: &AttemptReport,
    world: &DurableWorld,
    intent: &LocalIntent,
    label: &str,
) {
    let expected_bodies = body_recipes(world, intent)
        .provisional_bodies()
        .into_iter()
        .map(|body| body.into_parts())
        .map(|(id, body)| (id, body.into_string()))
        .collect::<HashMap<_, _>>();
    let creates = flatten(&retry.trace.creates);
    assert_eq!(
        creates.len(),
        intent.iter().count(),
        "{label}: retry must create every pull request after the Git barrier"
    );
    for (local, create) in intent.iter().zip(creates) {
        assert_eq!(create.id, local.id, "{label}: each create targets its exact local change");
        assert_eq!(create.title, local.title, "{label}: each create carries the exact title");
        assert_eq!(
            create.head_oid, local.desired.head,
            "{label}: each create uses the acknowledged local head"
        );
        assert_eq!(
            create.base_oid, local.desired.first_parent,
            "{label}: each create uses the acknowledged owned base"
        );
        assert_eq!(
            Some(&create.body),
            expected_bodies.get(&local.id),
            "{label}: each create carries its exact provisional body"
        );
    }
}

pub(super) fn assert_create_retry_finishes_projection(
    retry: &AttemptReport,
    world: &DurableWorld,
    intent: &LocalIntent,
    label: &str,
) {
    let expected_markers =
        intent.iter().map(|local| expected_marker_effect(world, local)).collect::<Box<[_]>>();
    assert_eq!(
        flatten(&retry.trace.markers),
        expected_markers,
        "{label}: retry must mark every durable OPEN row at its exact local head"
    );

    assert_retry_finishes_projection(retry, world, intent, label);
}

pub(super) fn assert_retry_finishes_projection(
    retry: &AttemptReport,
    world: &DurableWorld,
    intent: &LocalIntent,
    label: &str,
) {
    // Body rendering has its own exact snapshot tests. Re-rendering here from
    // the durable identities checks the stage boundary independently: the
    // retry must route every corresponding final body into an update and make
    // that exact value durable.
    let expected_bodies = expected_final_bodies(world, intent);

    let actual_updates = update_effects(&retry.trace);
    assert_eq!(
        actual_updates.len(),
        intent.iter().count(),
        "{label}: retry must finalize every provisional OPEN row"
    );

    for (index, (local, update)) in intent.iter().zip(actual_updates).enumerate() {
        assert_eq!(
            update.resolved_id.as_ref(),
            Some(&local.id),
            "{label}: each final update targets its exact local change"
        );
        assert_eq!(
            update.operation.identity,
            world.identity(&local.id),
            "{label}: each final update uses the durable OPEN identity"
        );
        let expected_body = expected_bodies.get(&local.id).unwrap();
        assert_eq!(
            update.operation.body.as_ref(),
            Some(expected_body),
            "{label}: each final update carries its exact rendered body"
        );
        assert_eq!(
            update.operation.base_branch.as_deref(),
            (index == 0).then_some(DEFAULT_BRANCH),
            "{label}: only the root update retargets from its owned base"
        );
    }

    assert_final_projection_state(world, intent, label);
}

pub(super) fn expected_final_bodies(
    world: &DurableWorld,
    intent: &LocalIntent,
) -> HashMap<GherritPrId, String> {
    let assignments = intent
        .iter()
        .map(|local| (local.id.clone(), world.identity(&local.id).number()))
        .collect::<Vec<_>>();
    body_recipes(world, intent)
        .final_bodies(&assignments)
        .unwrap()
        .into_iter()
        .map(|body| body.into_parts())
        .map(|(id, body)| (id, body.into_string()))
        .collect()
}

pub(super) fn expected_marker_effect(world: &DurableWorld, local: &LocalChange) -> MarkerEffect {
    let published = world.published(&local.id).expect("every marked local change is published");
    let v1 = published.history.first.head;
    let number = world.identity(&local.id).number();
    MarkerEffect { id: local.id.clone(), marker: marker_identity(v1, number) }
}

pub(super) fn assert_final_projection_state(
    world: &DurableWorld,
    intent: &LocalIntent,
    label: &str,
) {
    let expected_bodies = expected_final_bodies(world, intent);
    for (index, local) in intent.iter().enumerate() {
        let published = world.published(&local.id).expect("every local change is published");
        assert_eq!(
            published.history.last(),
            local.desired,
            "{label}: each durable tuple reaches the exact local revision"
        );
        let marker = published
            .marker
            .unwrap_or_else(|| panic!("{label}: every durable change must be marked after retry"));
        let pull_request = world
            .selected_open_pull_request(&local.id)
            .unwrap_or_else(|| panic!("{label}: the marker-selected row must remain OPEN"));
        let fields = pull_request.open_fields().unwrap();
        assert_eq!(
            marker,
            expected_marker_effect(world, local).marker,
            "{label}: the marker retains the exact canonical v1 and pull request identity"
        );
        assert_eq!(
            fields.base,
            if index == 0 { BaseKind::Default } else { BaseKind::Owned },
            "{label}: the finalized pull request has its exact local base kind"
        );
        assert_eq!(fields.title, local.title, "{label}: the finalized title is exact");
        let expected_body = expected_bodies.get(&local.id).unwrap();
        assert_eq!(&fields.body, expected_body, "{label}: each exact final body becomes durable");
    }
}

#[derive(serde::Serialize)]
pub(super) struct UpdateAlternatives {
    pub(super) publisher_a: Vec<UpdateSnapshot>,
    pub(super) publisher_b: Vec<UpdateSnapshot>,
}

pub(super) fn establish_marked(
    world: &mut DurableWorld,
    change: &LocalChange,
    base: BaseKind,
    body: &str,
) {
    world.publish_for_setup(&change.id, change.desired);
    world.open_for_setup(&change.id, &change.title, body);
    world.mark_for_setup(&change.id, change.desired.head, base);
}

pub(super) fn assert_projection_matches_update(world: &DurableWorld, update: &UpdateEffect) {
    let id =
        update.resolved_id.as_ref().expect("a planned update resolves to one durable OPEN row");
    let pull_request = world
        .exact_open_pull_request(&update.operation.identity)
        .expect("the update target remains OPEN");
    let fields = pull_request.open_fields().unwrap();
    if let Some(title) = &update.operation.title {
        assert_eq!(&fields.title, title);
    }
    if let Some(body) = &update.operation.body {
        assert_eq!(&fields.body, body);
    }
    if let Some(base) = &update.operation.base_branch {
        let expected = if base == DEFAULT_BRANCH {
            BaseKind::Default
        } else {
            assert_eq!(base, &owned_base_name(id));
            BaseKind::Owned
        };
        assert_eq!(fields.base, expected);
    }
}

/// Compares the facts which commute when disjoint publishers allocate pull
/// request identities in different orders. Generated navigation bodies and
/// canonical markers embed those identities, so neither value is expected to
/// match literally. Each marker must instead bind its own world's exact OPEN
/// identity while retaining the same immutable v1.
pub(super) fn assert_disjoint_results_commute(left: &DurableWorld, right: &DurableWorld) {
    assert_eq!(left.default_tip, right.default_tip);
    assert_eq!(
        left.git_refs
            .0
            .iter()
            .filter(|(name, _)| name.starts_with("refs/heads/"))
            .collect::<Vec<_>>(),
        right
            .git_refs
            .0
            .iter()
            .filter(|(name, _)| name.starts_with("refs/heads/"))
            .collect::<Vec<_>>()
    );
    assert_eq!(left.changes.len(), right.changes.len());
    for (id, left_change) in &left.changes {
        let right_change = right.changes.get(id).expect("both worlds publish the same IDs");
        assert_eq!(left_change.history, right_change.history);
        match (left_change.marker, right_change.marker) {
            (None, None) => assert!(
                left_change.pull_requests.is_empty() && right_change.pull_requests.is_empty(),
                "unpublished pull request state commutes literally"
            ),
            (Some(left_marker), Some(right_marker)) => {
                let left = left
                    .selected_open_pull_request(id)
                    .expect("a completed left publication has its canonical OPEN row");
                let right = right
                    .selected_open_pull_request(id)
                    .expect("a completed right publication has its canonical OPEN row");
                let left_fields = left.open_fields().unwrap();
                let right_fields = right.open_fields().unwrap();
                assert_eq!(left_marker.v1, right_marker.v1);
                assert_eq!(left_marker.number(), left.identity.number());
                assert_eq!(right_marker.number(), right.identity.number());
                assert_eq!(left_fields.base, right_fields.base);
                assert_eq!(left_fields.title, right_fields.title);
                // Generated navigation embeds dynamically allocated PR
                // numbers, so bodies need not commute literally.
            }
            _ => panic!("both worlds have the same OPEN-row presence"),
        }
    }
}

/// Compare submitted work from independently planned attempts. Their opaque
/// publication identities must differ; shape comparisons deliberately retain
/// every change/revision/version and ref destination, but erase those tokens.
pub(super) fn submitted_shape(effects: &[InitialRefEffect]) -> Vec<InitialRefEffect> {
    effects
        .iter()
        .cloned()
        .map(|mut effect| {
            if let InitialRefEffect::Tuple(tuple) = &mut effect {
                tuple.publication = oid(0);
            }
            effect
        })
        .collect()
}
pub(super) fn initial_shape(batches: &[Box<[InitialRefEffect]>]) -> Vec<Vec<InitialRefEffect>> {
    batches.iter().map(|batch| submitted_shape(batch)).collect()
}

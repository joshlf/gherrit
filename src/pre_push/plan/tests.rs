//! Focused tables for the planner's exact-local policy and typed gates.
//!
//! These fixtures start after Git and GitHub adapters have produced their
//! validated domain values. Adapter encoding, terminal classification, and
//! unrelated repository state belong to their own test modules.

use gix::ObjectId;

use super::*;
use crate::pre_push::{
    github::CorrelatedRepository,
    pull_request::{AbsentPullRequest, LocalPullRequestObservation},
    remote::{ActiveRemoteChanges, ObservedChangeHistory},
    test_effect::{EffectBatches, Stage},
};

/// Flattens typed transport batches only in planner-policy assertions which
/// neither execute nor model interruption. The restart oracle retains every
/// batch boundary.
fn flatten_batches<T: Clone>(batches: &EffectBatches<T>) -> Box<[T]> {
    batches.iter().flat_map(|batch| batch.iter().cloned()).collect()
}

fn id(value: &str) -> GherritPrId {
    GherritPrId::from_ref_component(value.as_bytes()).expect("valid test change ID")
}

fn oid(byte: u8) -> ObjectId {
    ObjectId::from_bytes_or_panic(&[byte; 20])
}

fn identity(number: u64) -> PullRequestIdentity {
    PullRequestIdentity::new(number, format!("PR_{number}")).expect("valid test identity")
}

fn destination() -> super::super::destination::PushDestination {
    super::super::destination::PushDestination::for_test(
        "origin",
        "https://github.com/owner/repository.git",
        Vec::new(),
    )
    .expect("valid test destination")
}

#[allow(clippy::too_many_arguments)]
fn open(
    id: GherritPrId,
    number: u64,
    head: ObjectId,
    base: BaseKind,
    base_oid: ObjectId,
    title: &str,
    body: &str,
    landing: bool,
) -> ManagedOpenPullRequest {
    ManagedOpenPullRequest::from_typed_for_test(
        id,
        identity(number),
        head,
        base,
        base_oid,
        title.to_owned(),
        body.to_owned(),
    )
    .with_landing_automation_for_test(landing)
}

#[test]
fn open_marker_base_position_and_landing_policy_is_exact() {
    let change = id("Gone");
    let default = DefaultBranch::new("main".to_owned(), oid(1)).unwrap();

    for desired in [BaseKind::Default, BaseKind::Owned] {
        for observed in [BaseKind::Default, BaseKind::Owned] {
            for marked in [false, true] {
                for landing in [false, true] {
                    let base_oid = match observed {
                        BaseKind::Default => default.tip(),
                        BaseKind::Owned => oid(2),
                    };
                    let pull_request = open(
                        change.clone(),
                        7,
                        oid(3),
                        observed,
                        base_oid,
                        "Title",
                        "Body",
                        landing,
                    );
                    let result = validate_pull_request(
                        &change,
                        &pull_request,
                        true,
                        true,
                        marked,
                        Some(desired),
                        &default,
                    );
                    let expected = (marked || observed == BaseKind::Owned)
                        && !(landing
                            && (observed == BaseKind::Owned || desired == BaseKind::Owned));

                    assert_eq!(
                        result.is_ok(),
                        expected,
                        "desired={desired:?}, observed={observed:?}, marked={marked}, landing={landing}, error={:?}",
                        result.err()
                    );
                }
            }
        }
    }
}

#[derive(Clone, Copy)]
enum OneChangeObservation {
    Absent,
    Open { head: ObjectId, base: BaseKind, base_oid: ObjectId },
}

fn one_change_plan(
    published: &[(ObjectId, ObjectId)],
    marker: Option<ObjectId>,
    observation: OneChangeObservation,
) -> Result<Stage> {
    let change = id("Gone");
    let default_tip = oid(1);
    let proposal = oid(12);
    let graph = CommitGraphEvidence::from_literal_commits_for_test([
        (default_tip, Vec::new(), Vec::new()),
        (oid(2), Vec::new(), Vec::new()),
        (oid(3), Vec::new(), Vec::new()),
        (oid(10), vec![oid(2)], vec![change.clone()]),
        (oid(11), vec![oid(3)], vec![change.clone()]),
        (proposal, vec![default_tip], vec![change.clone()]),
        (oid(20), vec![default_tip], vec![id("Other")]),
    ])?;
    let default = DefaultBranch::new("main".to_owned(), default_tip)?;
    let stack = LocalStack::for_test_with_content(
        default_tip,
        [(change.clone(), proposal, "Title".to_owned(), "Commit body".to_owned())],
    )?;
    let destination = destination();
    let remote = ActiveRemoteChanges::from_typed_for_test(
        &destination,
        default.clone(),
        vec![ObservedChangeHistory::from_typed_for_test(change.clone(), published, marker)?],
    );
    let observation = match observation {
        OneChangeObservation::Absent => {
            LocalPullRequestObservation::Absent(AbsentPullRequest::for_test(change.clone()))
        }
        OneChangeObservation::Open { head, base, base_oid } => LocalPullRequestObservation::Open(
            open(change.clone(), 7, head, base, base_oid, "Title", "stale body", false),
        ),
    };
    let correlated = CorrelatedRepository::from_typed_for_test(
        &destination,
        "R_repository".to_owned(),
        default,
        vec![observation],
    )?;
    let context = BodyLinkContext::from_destination(&destination, None)?;

    plan_local_publication(context, stack, correlated, remote, &graph)
        .map(|plan| plan.first_stage_for_test())
}

#[test]
fn absence_and_marker_are_the_exact_creation_authority() {
    let current = (oid(12), oid(1));
    let Stage::Creates(creates) =
        one_change_plan(&[current], None, OneChangeObservation::Absent).unwrap()
    else {
        panic!("published markerless absence must create")
    };
    assert_eq!(creates.len(), 1, "the one-operation fixture has one request");
    let creates = flatten_batches(&creates);
    assert_eq!(creates.len(), 1);
    assert_eq!(creates[0].id.as_str(), "Gone");
    assert_eq!(creates[0].base_branch, "gherrit-bases/Gone");
    assert_eq!(creates[0].head_oid, oid(12));
    assert_eq!(creates[0].base_oid, oid(1));

    let error =
        one_change_plan(&[current], Some(current.0), OneChangeObservation::Absent).unwrap_err();
    assert!(error.to_string().contains("marker but no same-repository pull request"));
}

#[test]
fn open_head_and_base_oids_must_belong_to_published_history() {
    let published = [(oid(10), oid(2)), (oid(11), oid(3))];

    for head in [oid(10), oid(11)] {
        for base_oid in [oid(2), oid(3)] {
            one_change_plan(
                &published,
                Some(oid(10)),
                OneChangeObservation::Open { head, base: BaseKind::Owned, base_oid },
            )
            .unwrap_or_else(|error| {
                panic!("published head={head} and base={base_oid} must be independent: {error:?}")
            });
        }
    }

    one_change_plan(
        &published,
        Some(oid(10)),
        OneChangeObservation::Open { head: oid(10), base: BaseKind::Default, base_oid: oid(1) },
    )
    .expect("the exact default object ID is valid");

    for (label, observation, diagnostic) in [
        (
            "proposal head",
            OneChangeObservation::Open { head: oid(12), base: BaseKind::Owned, base_oid: oid(2) },
            "head not present in published history",
        ),
        (
            "unrelated head",
            OneChangeObservation::Open { head: oid(20), base: BaseKind::Owned, base_oid: oid(2) },
            "head not present in published history",
        ),
        (
            "wrong default",
            OneChangeObservation::Open { head: oid(10), base: BaseKind::Default, base_oid: oid(2) },
            "wrong default-branch object ID",
        ),
        (
            "proposal parent",
            OneChangeObservation::Open { head: oid(10), base: BaseKind::Owned, base_oid: oid(1) },
            "owned-base object ID not present in published history",
        ),
        (
            "unrelated base",
            OneChangeObservation::Open { head: oid(10), base: BaseKind::Owned, base_oid: oid(20) },
            "owned-base object ID not present in published history",
        ),
        (
            "published head used as base",
            OneChangeObservation::Open { head: oid(10), base: BaseKind::Owned, base_oid: oid(11) },
            "owned-base object ID not present in published history",
        ),
    ] {
        let error = one_change_plan(&published, Some(oid(10)), observation).unwrap_err();
        assert!(error.to_string().contains(diagnostic), "case={label}: {error:?}");
    }

    let error = one_change_plan(
        &[],
        None,
        OneChangeObservation::Open { head: oid(12), base: BaseKind::Default, base_oid: oid(1) },
    )
    .unwrap_err();
    assert!(error.to_string().contains("OPEN pull request but no published history"));
}

#[test]
fn existing_projection_emits_every_exact_field_difference_mask() {
    for mask in 0_u8..8 {
        let projection = ExistingProjection {
            id: id("Gone"),
            identity: identity(7),
            observed_body: if mask & 2 == 0 { "new body" } else { "old body" }.into(),
            title_update: (mask & 1 != 0).then(|| "new title".to_owned()),
            base_update: (mask & 4 != 0).then(|| "gherrit-bases/Gone".to_owned()),
        };
        let update = projection.into_update(GeneratedBody::for_test("new body")).unwrap();

        if mask == 0 {
            assert!(update.is_none(), "mask={mask:03b}");
            continue;
        }
        let (actual_identity, title, body, base) = update.unwrap().into_parts();
        assert_eq!(actual_identity, identity(7), "mask={mask:03b}");
        assert_eq!(title.as_deref(), (mask & 1 != 0).then_some("new title"), "mask={mask:03b}");
        assert_eq!(body.as_deref(), (mask & 2 != 0).then_some("new body"), "mask={mask:03b}");
        assert_eq!(
            base.as_deref(),
            (mask & 4 != 0).then_some("gherrit-bases/Gone"),
            "mask={mask:03b}"
        );
    }
}

#[test]
fn body_comparison_normalizes_only_crlf_pairs() {
    for (label, observed, desired, equal) in [
        ("empty", "", "", true),
        ("exact", "a\nb", "a\nb", true),
        ("observed CRLF", "a\r\nb", "a\nb", true),
        ("desired CRLF", "a\nb", "a\r\nb", true),
        ("mixed line endings", "a\r\nb\nc\r\n", "a\nb\r\nc\n", true),
        ("leading space", " body", "body", false),
        ("trailing space", "body ", "body", false),
        ("terminal newline", "body\n", "body", false),
        ("lone CR", "a\rb", "a\nb", false),
        ("extra blank line", "a\n\nb", "a\nb", false),
        ("changed content", "a\nx", "a\ny", false),
    ] {
        assert_eq!(bodies_equal(observed, desired), equal, "case={label}");
    }
}

#[test]
fn create_stage_requires_one_exact_nonempty_ordered_join() {
    let one = id("Gone");
    let two = id("Gtwo");
    let three = id("Gthree");
    assert!(
        validate_create_stage_ids(
            &[one.clone(), two.clone()],
            &[one.clone(), two.clone()],
            &[one.clone(), two.clone()],
        )
        .is_ok()
    );

    for (label, planned, projected, pending) in [
        ("empty", Vec::new(), Vec::new(), Vec::new()),
        (
            "short projection",
            vec![one.clone(), two.clone()],
            vec![one.clone()],
            vec![one.clone(), two.clone()],
        ),
        (
            "long projection",
            vec![one.clone(), two.clone()],
            vec![one.clone(), two.clone(), three.clone()],
            vec![one.clone(), two.clone()],
        ),
        (
            "duplicate",
            vec![one.clone(), one.clone()],
            vec![one.clone(), one.clone()],
            vec![one.clone(), one.clone()],
        ),
        (
            "reordered projection",
            vec![one.clone(), two.clone()],
            vec![two.clone(), one.clone()],
            vec![one.clone(), two.clone()],
        ),
        (
            "short marker evidence",
            vec![one.clone(), two.clone()],
            vec![one.clone(), two.clone()],
            vec![one.clone()],
        ),
        (
            "long marker evidence",
            vec![one.clone(), two.clone()],
            vec![one.clone(), two.clone()],
            vec![one.clone(), two.clone(), three.clone()],
        ),
    ] {
        assert!(validate_create_stage_ids(&planned, &projected, &pending).is_err(), "case={label}");
    }
}

#[test]
fn mixed_create_receipt_releases_one_ordered_marker_gate_and_final_projection() {
    let default_tip = oid(1);
    let created_id = id("Gcreated");
    let marked_id = id("Gmarked");
    let unmarked_id = id("Gunmarked");
    let created_head = oid(10);
    let marked_head = oid(11);
    let unmarked_head = oid(12);
    let graph = CommitGraphEvidence::from_literal_commits_for_test([
        (default_tip, Vec::new(), Vec::new()),
        (created_head, vec![default_tip], vec![created_id.clone()]),
        (marked_head, vec![created_head], vec![marked_id.clone()]),
        (unmarked_head, vec![marked_head], vec![unmarked_id.clone()]),
    ])
    .unwrap();
    let default = DefaultBranch::new("main".to_owned(), default_tip).unwrap();
    let stack = LocalStack::for_test_with_content(
        default_tip,
        [
            (
                created_id.clone(),
                created_head,
                "Created root".to_owned(),
                "Created body".to_owned(),
            ),
            (marked_id.clone(), marked_head, "Marked child".to_owned(), "Marked body".to_owned()),
            (
                unmarked_id.clone(),
                unmarked_head,
                "Unmarked child".to_owned(),
                "Unmarked body".to_owned(),
            ),
        ],
    )
    .unwrap();
    let destination = destination();
    let remote = ActiveRemoteChanges::from_typed_for_test(
        &destination,
        default.clone(),
        vec![
            ObservedChangeHistory::from_typed_for_test(
                created_id.clone(),
                &[(created_head, default_tip)],
                None,
            )
            .unwrap(),
            ObservedChangeHistory::from_typed_for_test(
                marked_id.clone(),
                &[(marked_head, created_head)],
                Some(marked_head),
            )
            .unwrap(),
            ObservedChangeHistory::from_typed_for_test(
                unmarked_id.clone(),
                &[(unmarked_head, marked_head)],
                None,
            )
            .unwrap(),
        ],
    );
    let correlated = CorrelatedRepository::from_typed_for_test(
        &destination,
        "R_repository".to_owned(),
        default,
        vec![
            LocalPullRequestObservation::Absent(AbsentPullRequest::for_test(created_id.clone())),
            LocalPullRequestObservation::Open(open(
                marked_id.clone(),
                101,
                marked_head,
                BaseKind::Owned,
                created_head,
                "Marked child",
                "stale marked body",
                false,
            )),
            LocalPullRequestObservation::Open(open(
                unmarked_id.clone(),
                102,
                unmarked_head,
                BaseKind::Owned,
                marked_head,
                "Unmarked child",
                "stale unmarked body",
                false,
            )),
        ],
    )
    .unwrap();
    let context = BodyLinkContext::from_destination(&destination, None).unwrap();
    let plan = plan_local_publication(context, stack, correlated, remote, &graph).unwrap();
    let (creates, projection) = plan.into_create_stage_for_test();
    let create_batches = creates.effect_batches_for_test();
    assert_eq!(create_batches.len(), 1, "the one-operation fixture has one request");
    let create_effects = flatten_batches(&create_batches);
    assert_eq!(create_effects.len(), 1);
    assert_eq!(create_effects[0].id, created_id);
    assert_eq!(create_effects[0].base_branch, "gherrit-bases/Gcreated");
    assert_eq!(create_effects[0].head_oid, created_head);
    assert_eq!(create_effects[0].base_oid, default_tip);

    let receipts = creates.complete_for_test(vec![(created_id.clone(), identity(103))]).unwrap();
    let markers = projection.complete(receipts).unwrap();
    assert_eq!(
        flatten_batches(&markers.effect_batches_for_test())
            .iter()
            .map(|effect| (effect.id.as_str(), effect.target))
            .collect::<Vec<_>>(),
        [("Gcreated", created_head), ("Gunmarked", unmarked_head)]
    );

    let Stage::Updates(updates) = markers.final_projection.stage_for_test() else {
        panic!("the marker gate must retain one complete final projection")
    };
    let updates = flatten_batches(&updates);
    assert_eq!(
        updates
            .iter()
            .map(|update| (
                update.identity.number().get(),
                update.title.is_some(),
                update.body.is_some(),
                update.base_branch.as_deref(),
            ))
            .collect::<Vec<_>>(),
        [(103, false, true, Some("main")), (101, false, true, None), (102, false, true, None),]
    );
}

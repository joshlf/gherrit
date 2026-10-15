use super::*;

#[test]
fn public_projection_types_exclude_managed_ref_namespace_conflicts() {
    // These names cannot reach the driver/world: the public branch type
    // excludes exact change-ID names and the owned-base namespace.
    for name in ["Groot", "gherrit-bases", "gherrit-bases/Groot"] {
        assert!(crate::manage::PublicBranchName::new(name.to_owned()).is_err(), "{name}");
    }
}

#[test]
fn marker_storage_enforces_exact_object_leases_without_github_coupling() {
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
    let absent_number = marker_effect(
        &local.id,
        marker_identity(first.head, PullRequestNumber::for_test(number.get() + 1)),
    );
    assert_eq!(
        world.apply_effect(&ExternalEffect::Marker(absent_number.clone())),
        EffectOutcome::Acknowledged,
        "Git accepts a semantic marker without consulting GitHub rows"
    );

    let wrong_v1 = marker_effect(&local.id, marker_identity(amended.head, number));
    let mut wrong_v1_world = world.clone();
    assert_eq!(
        wrong_v1_world.apply_effect(&ExternalEffect::Marker(wrong_v1)),
        EffectOutcome::Rejected
    );

    let correct = marker_effect(&local.id, marker_identity(first.head, number));
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
    world.set_public_branch(&public_branch_name("release-"), oid(10));
    world.set_public_branch(&public_branch_name("release-/child"), oid(11));
    world.assert_well_formed();
}

#[test]
fn public_create_lease_rejects_a_d_f_conflict_without_mutation() {
    let intent =
        root_intent(oid(1), "Gdflease", LiteralRevision { head: oid(2), first_parent: oid(1) });
    let mut world = DurableWorld::for_intents(oid(1), &[&intent]);
    world.set_public_branch(&public_branch_name("release-"), oid(10));
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

#[test]
fn public_ref_effects_share_raw_store_leases_and_directory_constraints() {
    let intent =
        root_intent(oid(1), "Grawpublic", LiteralRevision { head: oid(2), first_parent: oid(1) });
    for (raw_name, raw_target, accepted) in [
        ("refs/heads/release-/child", oid(10), false),
        ("refs/heads/release-", oid(10), false),
        ("refs/heads/release-other/child", oid(10), true),
        ("refs/heads/release-", oid(11), true),
    ] {
        let mut world = DurableWorld::for_intents(oid(1), &[&intent]);
        world.git_refs.0.insert(raw_name.to_owned(), raw_target.to_string());
        world.assert_well_formed();
        let before = world.clone();
        let outcome =
            world.apply_effect(&ExternalEffect::InitialRef(InitialRefEffect::PublicBranch {
                branch: public_branch_name("release-"),
                expected: None,
                desired: oid(11),
            }));
        assert_eq!(
            outcome,
            if accepted { EffectOutcome::Acknowledged } else { EffectOutcome::Rejected },
            "{raw_name}"
        );
        if !accepted {
            assert_eq!(world, before);
        }
    }
    let mut world = DurableWorld::for_intents(oid(1), &[&intent]);
    let branch = public_branch_name("release-");
    world.set_public_branch(&branch, oid(10));
    assert_eq!(world.public_branch_target(&branch), Some(oid(10)));
    assert_eq!(
        world.apply_public_branch(&branch, Some(oid(10)), oid(11)),
        EffectOutcome::Acknowledged
    );
    assert_eq!(world.public_branch_target(&branch), Some(oid(11)));
}

#[test]
fn retained_marker_commands_obey_raw_conflicts_and_equal_oid_elision() {
    let intent = root_intent(
        oid(1),
        "Gmarkerstorage",
        LiteralRevision { head: oid(2), first_parent: oid(1) },
    );
    let mut initial = DurableWorld::for_intents(oid(1), &[&intent]);
    initial.publish_for_setup(&intent.first.id, intent.first.desired);
    let marker = marker_identity(oid(2), PullRequestNumber::for_test(999));
    let effect = marker_effect(&intent.first.id, marker);
    let name = "refs/tags/gherrit/Gmarkerstorage/pr";
    for (raw_name, target, acknowledged) in [
        (format!("{name}/child"), oid(99), false),
        (name.to_owned(), oid(99), false),
        (name.to_owned(), effect.object, true),
        (format!("{name}-other"), oid(99), true),
    ] {
        let mut world = initial.clone();
        world.git_refs.0.insert(raw_name.clone(), target.to_string());
        let before = world.clone();
        let outcome = world.apply_effect(&ExternalEffect::Marker(effect.clone()));
        assert_eq!(outcome == EffectOutcome::Acknowledged, acknowledged, "{raw_name}");
        if acknowledged {
            assert_eq!(world.git_refs.0[name], effect.object.to_string());
            assert_eq!(world.published(&intent.first.id).unwrap().marker, Some(marker));
            if raw_name != name {
                assert_eq!(world.git_refs.0[&raw_name], target.to_string());
            }
        } else {
            assert_eq!(world, before);
        }
    }
}

#[test]
#[should_panic(expected = "an immutable marker targets exactly v1")]
fn native_marker_acceptance_does_not_authorize_a_wrong_v1() {
    let intent =
        root_intent(oid(1), "Gbadmarkerv1", LiteralRevision { head: oid(2), first_parent: oid(1) });
    let mut world = DurableWorld::for_intents(oid(1), &[&intent]);
    world.publish_for_setup(&intent.first.id, intent.first.desired);
    let effect =
        marker_effect(&intent.first.id, marker_identity(oid(3), PullRequestNumber::for_test(999)));
    world.apply_effect(&ExternalEffect::Marker(effect));
}

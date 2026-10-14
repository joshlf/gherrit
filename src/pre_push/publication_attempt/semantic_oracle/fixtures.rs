use super::*;

pub(super) fn id(value: &str) -> GherritPrId {
    GherritPrId::from_ref_component(value.as_bytes()).unwrap()
}

pub(super) fn public_branch_name(value: &str) -> PublicBranchName {
    PublicBranchName::new(value.to_owned()).unwrap()
}

pub(super) fn oid(value: u16) -> ObjectId {
    let mut bytes = [0_u8; 20];
    bytes[18..].copy_from_slice(&value.to_be_bytes());
    ObjectId::from_bytes_or_panic(&bytes)
}

pub(super) fn validated_history(
    id: GherritPrId,
    published: &[(ObjectId, ObjectId)],
    proposal: (ObjectId, ObjectId),
    pull_request_number: Option<PullRequestNumber>,
) -> ValidatedChangeHistory {
    ValidatedChangeHistory::for_plan_test(id, published, proposal, pull_request_number)
}

pub(super) fn marker_identity(v1: ObjectId, number: PullRequestNumber) -> MarkerIdentity {
    MarkerIdentity { v1, marker: PullRequestMarker::new(number) }
}

pub(super) fn local_change(
    name: &str,
    desired: LiteralRevision,
    title: &str,
    commit_body: &str,
) -> LocalChange {
    LocalChange {
        id: id(name),
        desired,
        title: title.to_owned(),
        commit_body: commit_body.to_owned(),
    }
}

pub(super) fn root_intent(
    default_tip: ObjectId,
    name: &str,
    revision: LiteralRevision,
) -> LocalIntent {
    LocalIntent::new(
        default_tip,
        [local_change(name, revision, &format!("Title for {name}"), &format!("Body for {name}"))],
    )
}

/// Independent literal framing for the storage oracle's expected marker OID.
pub(super) fn marker_object(id: &GherritPrId, marker: MarkerIdentity) -> ObjectId {
    let bytes = format!(
        "object {}\ntype commit\ntag gherrit/{}/pr\ntagger GHerrit <gherrit@invalid> 0 +0000\n\ngherrit-canonical-pr-v1 {}\n",
        marker.v1,
        id.as_str(),
        marker.number().get()
    );
    gix::objs::compute_hash(gix::hash::Kind::Sha1, gix::object::Kind::Tag, bytes.as_bytes())
        .unwrap()
}

pub(super) fn marker_effect(id: &GherritPrId, marker: MarkerIdentity) -> MarkerEffect {
    MarkerEffect { id: id.clone(), marker, object: marker_object(id, marker) }
}

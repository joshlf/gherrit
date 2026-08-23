//! Semantic effects exposed only to the restart model.
//!
//! These values are captured before Git and GraphQL serialization. Protocol
//! tests cover that serialization independently.

use gix::ObjectId;

use super::{local::GherritPrId, pull_request::PullRequestIdentity};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct RevisionEffect {
    pub(super) head: ObjectId,
    pub(super) first_parent: ObjectId,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct TupleEffect {
    pub(super) id: GherritPrId,
    pub(super) previous: Option<RevisionEffect>,
    pub(super) desired: RevisionEffect,
    pub(super) version: u64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct CreateEffect {
    pub(super) id: GherritPrId,
    pub(super) repository_id: String,
    pub(super) base_branch: String,
    pub(super) title: String,
    pub(super) body: String,
    pub(super) head_oid: ObjectId,
    pub(super) base_oid: ObjectId,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct MarkerEffect {
    pub(super) id: GherritPrId,
    pub(super) target: ObjectId,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct UpdateEffect {
    pub(super) identity: PullRequestIdentity,
    pub(super) title: Option<String>,
    pub(super) body: Option<String>,
    pub(super) base_branch: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) enum GitEffect {
    Tuple(TupleEffect),
    Marker(MarkerEffect),
}

pub(super) type EffectBatches<T> = Box<[Box<[T]>]>;

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) enum Stage {
    Tuples(EffectBatches<TupleEffect>),
    Creates(EffectBatches<CreateEffect>),
    Markers(EffectBatches<MarkerEffect>),
    Updates(EffectBatches<UpdateEffect>),
    Done,
}

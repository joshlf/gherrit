//! Restart and concurrency checks over literal durable state.
//!
//! This model deliberately knows nothing about refspec text, GraphQL
//! documents, JSON, HTTP, or process output. Adapter tests own those
//! encodings. It preserves only the request and alias boundaries which
//! determine what can be durable after an interrupted attempt.
//!
//! A local intent, durable world, and one query's visibility are separate
//! values. Production planning consumes observations built from those values,
//! and a test-only effect driver executes the real consuming plan directly
//! against the world. It records only batches the production stage machine
//! actually reaches. Restart tests then throw every attempt-local value away
//! and construct a fresh observation and plan.

use std::collections::{HashMap, HashSet};

use color_eyre::eyre::{Result, bail};
use gix::ObjectId;

use super::{
    ObservedLocalPublication,
    body::StackBodyRecipes,
    github::{
        AbsentPullRequest, BaseKind, CompleteCreateReceipts, CompleteLocalPullRequests,
        DraftPullRequest, LocalPullRequestObservation, ManagedOpenPullRequests, ObservedBase,
        PreparedCreates, PreparedDraftConversions, PreparedPullRequestProjection,
        PullRequestIdentity, PullRequestNumber, TestCreate, TestDraft, TestPullRequestProjection,
        TestUpdate,
    },
    history::ValidatedChangeHistory,
    marker::{MarkerTemplate, PullRequestMarker},
    plan::{EffectDriver, PlannedPublication, plan_effects},
    refs::{
        MarkerTransition, PreparedPushes, PublicationRevision, TestPushEffect,
        prepare_marker_pushes,
    },
};
use crate::{
    manage::PublicBranchName,
    pre_push::{
        destination::{
            DefaultBranch, ObservedPublicBranch, PushDestination, RemoteBranchState,
            RepositoryCoordinates,
        },
        local::{GherritPrId, LocalStack},
    },
};

const DEFAULT_BRANCH: &str = "main";
const PUBLIC_BRANCH: &str = "release-candidate";
const REPOSITORY_ID: &str = "REPOSITORY_NODE";

// Preserve existing snapshot identities and their common directory while
// moving scenario functions into a private module.
macro_rules! assert_model_snapshot {
    ($name:literal, $value:expr $(,)?) => {
        insta::with_settings!({
            snapshot_path => concat!(env!("CARGO_MANIFEST_DIR"), "/src/pre_push/publication_attempt/snapshots"),
            prepend_module_to_snapshot => false,
        }, {
            insta::assert_yaml_snapshot!(
                concat!("gherrit__pre_push__publication_attempt__semantic_oracle__", $name),
                $value
            );
        });
    };
}

mod world;
use world::*;
mod visibility;
use visibility::*;
mod fixtures;
use fixtures::*;
mod reports;
use reports::*;
mod driver;
use driver::*;
mod assertions;
use assertions::*;
mod scenarios;

mod performance;

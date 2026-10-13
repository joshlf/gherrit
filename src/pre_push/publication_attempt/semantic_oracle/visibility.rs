use super::*;

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct ObservedOpenFields {
    pub(super) head_oid: ObjectId,
    pub(super) base: BaseKind,
    pub(super) base_oid: ObjectId,
    pub(super) title: String,
    pub(super) body: String,
    pub(super) is_draft: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct ObservedOpenPullRequest {
    pub(super) identity: PullRequestIdentity,
    pub(super) fields: ObservedOpenFields,
}

/// Complete OPEN-only connection results captured before planning.
///
/// An absent change ID observes current durable OPEN rows. A present entry is
/// the whole result captured earlier, including membership, fields, and an
/// intentionally empty result. A capture can retain a row which later becomes
/// terminal, but it can never gain a row created after the capture.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(super) struct QueryVisibility {
    pub(super) captured: HashMap<GherritPrId, Box<[ObservedOpenPullRequest]>>,
}

impl QueryVisibility {
    pub(super) fn hiding_identities(
        world: &DurableWorld,
        identities: impl IntoIterator<Item = PullRequestIdentity>,
    ) -> Self {
        let identities = identities.into_iter().collect::<Vec<_>>();
        let hidden = identities.iter().cloned().collect::<HashSet<_>>();
        assert_eq!(hidden.len(), identities.len(), "one query hides an identity at most once");
        assert!(
            hidden.iter().all(|identity| world.exact_pull_request(identity).is_some()),
            "only a durable row can be hidden"
        );
        let captured = world
            .changes
            .iter()
            .filter(|(_, change)| {
                change.pull_requests.iter().any(|row| hidden.contains(&row.identity))
            })
            .map(|(id, _)| {
                let rows = world
                    .current_open_rows(id)
                    .into_iter()
                    .filter(|row| !hidden.contains(&row.identity))
                    .collect::<Vec<_>>()
                    .into_boxed_slice();
                (id.clone(), rows)
            })
            .collect();
        Self { captured }
    }

    pub(super) fn hiding(world: &DurableWorld, ids: impl IntoIterator<Item = GherritPrId>) -> Self {
        let identities = ids
            .into_iter()
            .flat_map(|id| {
                let identities = world
                    .open_pull_requests(&id)
                    .map(|row| row.identity.clone())
                    .collect::<Vec<_>>();
                assert!(!identities.is_empty(), "only an existing OPEN result can hide");
                identities
            })
            .collect::<Vec<_>>();
        Self::hiding_identities(world, identities)
    }

    pub(super) fn stale(world: &DurableWorld, ids: impl IntoIterator<Item = GherritPrId>) -> Self {
        let mut visibility = Self::default();
        for id in ids {
            assert!(
                visibility
                    .captured
                    .insert(id.clone(), world.current_open_rows(&id).into_boxed_slice())
                    .is_none(),
                "one query captures one OPEN connection per change"
            );
        }
        visibility
    }

    /// Captures independently lagged Git head and base-ref observations.
    ///
    /// GitHub can expose the two refs from different immutable history slots.
    /// The OPEN row's non-ref fields are captured at construction time, so a
    /// later durable write cannot retroactively advance this query result.
    pub(super) fn historical_slots(
        world: &DurableWorld,
        id: GherritPrId,
        head_slot: usize,
        base_slot: usize,
    ) -> Self {
        let mut rows = world.current_open_rows(&id);
        let fields = rows
            .iter_mut()
            .min_by_key(|row| row.identity.number().get())
            .expect("a historical observation contains an OPEN row");
        let change = world.published(&id).expect("an OPEN row has published history");
        fields.fields.head_oid =
            change.history.get(head_slot).expect("valid historical head slot").head;
        let historical_base =
            change.history.get(base_slot).expect("valid historical base slot").first_parent;
        fields.fields.base_oid = match fields.fields.base {
            BaseKind::Default => world.default_tip,
            BaseKind::Owned => historical_base,
        };
        Self { captured: HashMap::from([(id, rows.into_boxed_slice())]) }
    }

    pub(super) fn rows(&self, id: &GherritPrId) -> Option<&[ObservedOpenPullRequest]> {
        self.captured.get(id).map(AsRef::as_ref)
    }
}

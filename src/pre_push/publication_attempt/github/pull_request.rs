//! GitHub pull request identity scalars.
//!
//! Their wire constructors are visible only inside the private GitHub adapter.
//! Completed exact-local evidence lives in [`super::observation`], where
//! pagination authority is established.

use std::{collections::HashSet, num::NonZeroU32};

use color_eyre::eyre::{Result, bail, eyre};

/// A positive pull request number in GitHub's GraphQL `Int` range.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub(in crate::pre_push::publication_attempt) struct PullRequestNumber(NonZeroU32);

impl PullRequestNumber {
    pub(in crate::pre_push::publication_attempt) const MAX: Self =
        Self(NonZeroU32::new(i32::MAX as u32).expect("GraphQL Int maximum is nonzero"));

    fn new(value: u64) -> Result<Self> {
        let value = u32::try_from(value)
            .ok()
            .and_then(NonZeroU32::new)
            .filter(|value| value.get() <= i32::MAX as u32)
            .ok_or_else(|| eyre!("GitHub reported invalid pull request number {value}"))?;
        Ok(Self(value))
    }

    pub(in crate::pre_push::publication_attempt) fn get(self) -> u32 {
        self.0.get()
    }

    #[cfg(test)]
    pub(in crate::pre_push::publication_attempt) fn for_test(value: u32) -> Self {
        Self::new(u64::from(value)).expect("valid test pull request number")
    }
}

/// A nonempty opaque GraphQL node ID.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub(super) struct PullRequestNodeId(Box<str>);

impl PullRequestNodeId {
    pub(super) fn new(value: String) -> Result<Self> {
        if value.is_empty() {
            bail!("GitHub reported an empty pull request node ID");
        }
        Ok(Self(value.into_boxed_str()))
    }

    pub(super) fn as_str(&self) -> &str {
        &self.0
    }
}

/// The two coupled values which identify one GitHub pull request.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub(in crate::pre_push::publication_attempt) struct PullRequestIdentity {
    number: PullRequestNumber,
    node_id: PullRequestNodeId,
}

impl PullRequestIdentity {
    pub(super) fn new(number: u64, node_id: String) -> Result<Self> {
        Ok(Self {
            number: PullRequestNumber::new(number)?,
            node_id: PullRequestNodeId::new(node_id)?,
        })
    }

    pub(in crate::pre_push::publication_attempt) fn number(&self) -> PullRequestNumber {
        self.number
    }

    pub(super) fn node_id(&self) -> &PullRequestNodeId {
        &self.node_id
    }

    #[cfg(test)]
    pub(in crate::pre_push::publication_attempt) fn for_plan_test(
        number: u32,
        node_id: &str,
    ) -> Self {
        Self::new(u64::from(number), node_id.to_owned()).expect("valid plan-test identity")
    }

    #[cfg(test)]
    pub(in crate::pre_push::publication_attempt) fn node_id_for_test(&self) -> &str {
        self.node_id.as_str()
    }
}

/// Both pull request identity namespaces retained for one publication attempt.
///
/// The registry is initially populated by exact-local observation and then
/// moved through create execution. This is not a repository-wide uniqueness
/// claim. Numbers and node IDs remain independent because matching one
/// component does not make a create receipt safe.
#[derive(Debug, Default)]
pub(super) struct PullRequestIdentityRegistry {
    numbers: HashSet<PullRequestNumber>,
    node_ids: HashSet<PullRequestNodeId>,
}

#[derive(Clone, Copy)]
enum IdentityEvidence {
    Observation,
    CreateReceipt,
}

impl PullRequestIdentityRegistry {
    /// Atomically registers both components of an identity.
    ///
    /// Neither namespace is changed unless both components are new. The
    /// evidence kind controls only the bounded diagnostic; it does not change
    /// the uniqueness rule.
    fn insert(&mut self, identity: &PullRequestIdentity, evidence: IdentityEvidence) -> Result<()> {
        if self.numbers.contains(&identity.number()) {
            match evidence {
                IdentityEvidence::Observation => bail!(
                    "Exact-local observation repeats pull request number {}",
                    identity.number().get()
                ),
                IdentityEvidence::CreateReceipt => {
                    bail!("A createPullRequest receipt reuses a retained pull request number")
                }
            }
        }
        if self.node_ids.contains(identity.node_id()) {
            match evidence {
                IdentityEvidence::Observation => {
                    bail!("Exact-local observation repeats a pull request node ID")
                }
                IdentityEvidence::CreateReceipt => {
                    bail!("A createPullRequest receipt reuses a retained pull request node ID")
                }
            }
        }
        assert!(self.numbers.insert(identity.number()));
        assert!(self.node_ids.insert(identity.node_id().clone()));
        Ok(())
    }

    pub(super) fn insert_observation(&mut self, identity: &PullRequestIdentity) -> Result<()> {
        self.insert(identity, IdentityEvidence::Observation)
    }

    pub(super) fn insert_create_receipt(&mut self, identity: &PullRequestIdentity) -> Result<()> {
        self.insert(identity, IdentityEvidence::CreateReceipt)
    }

    #[cfg(test)]
    pub(super) fn lengths(&self) -> (usize, usize) {
        (self.numbers.len(), self.node_ids.len())
    }

    #[cfg(test)]
    pub(super) fn number_values(&self) -> HashSet<u32> {
        self.numbers.iter().map(|number| number.get()).collect()
    }

    #[cfg(test)]
    pub(super) fn node_id_values(&self) -> HashSet<&str> {
        self.node_ids.iter().map(|node_id| node_id.as_str()).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identity_wrappers_enforce_graphql_bounds() {
        for number in [1, i32::MAX as u64] {
            assert_eq!(
                PullRequestNumber::new(number).unwrap().get(),
                u32::try_from(number).unwrap()
            );
        }
        for number in [0, i32::MAX as u64 + 1, u64::MAX] {
            assert!(PullRequestNumber::new(number).is_err(), "number={number}");
        }

        assert_eq!(PullRequestNodeId::new("node".to_owned()).unwrap().as_str(), "node");
        assert_eq!(PullRequestNodeId::new(" ".to_owned()).unwrap().as_str(), " ");
        assert!(PullRequestNodeId::new(String::new()).is_err());
    }

    #[test]
    fn identity_namespaces_are_independently_unique() {
        let one = PullRequestIdentity::new(1, "NODE_ONE".to_owned()).unwrap();
        let same_number = PullRequestIdentity::new(1, "NODE_TWO".to_owned()).unwrap();
        let same_node = PullRequestIdentity::new(2, "NODE_ONE".to_owned()).unwrap();
        let mut numbers = PullRequestIdentityRegistry::default();
        numbers.insert_observation(&one).unwrap();
        assert!(numbers.insert_observation(&same_number).is_err());

        let mut nodes = PullRequestIdentityRegistry::default();
        nodes.insert_observation(&one).unwrap();
        assert!(nodes.insert_observation(&same_node).is_err());
    }

    #[test]
    fn failed_identity_insertions_do_not_claim_the_other_component() {
        let one = PullRequestIdentity::new(1, "NODE_ONE".to_owned()).unwrap();
        let number_two_same_node = PullRequestIdentity::new(2, "NODE_ONE".to_owned()).unwrap();
        let number_two = PullRequestIdentity::new(2, "NODE_TWO".to_owned()).unwrap();
        let same_number_node_three = PullRequestIdentity::new(1, "NODE_THREE".to_owned()).unwrap();
        let number_three = PullRequestIdentity::new(3, "NODE_THREE".to_owned()).unwrap();
        let swapped = PullRequestIdentity::new(1, "NODE_TWO".to_owned()).unwrap();

        let mut identities = PullRequestIdentityRegistry::default();
        identities.insert_observation(&one).unwrap();
        assert!(identities.insert_observation(&number_two_same_node).is_err());
        identities.insert_observation(&number_two).unwrap();
        assert!(identities.insert_observation(&same_number_node_three).is_err());
        identities.insert_observation(&number_three).unwrap();
        assert!(identities.insert_observation(&swapped).is_err());
        assert_eq!(identities.lengths(), (3, 3));
    }
}

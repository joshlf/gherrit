//! GitHub pull request identity scalars.
//!
//! Their wire constructors are visible only inside the private GitHub adapter.
//! Completed exact-local evidence lives in [`super::observation`], where
//! pagination authority is established.

use std::{collections::HashSet, num::NonZeroU32};

use color_eyre::eyre::{Result, bail, eyre};

/// A positive pull request number in GitHub's GraphQL `Int` range.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub(in crate::pre_push) struct PullRequestNumber(NonZeroU32);

impl PullRequestNumber {
    pub(super) fn new(value: u64) -> Result<Self> {
        let value = u32::try_from(value)
            .ok()
            .and_then(NonZeroU32::new)
            .filter(|value| value.get() <= i32::MAX as u32)
            .ok_or_else(|| eyre!("GitHub reported invalid pull request number {value}"))?;
        Ok(Self(value))
    }

    pub(in crate::pre_push) fn get(self) -> u32 {
        self.0.get()
    }
}

/// A nonempty opaque GraphQL node ID.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub(in crate::pre_push) struct PullRequestNodeId(Box<str>);

impl PullRequestNodeId {
    pub(super) fn new(value: String) -> Result<Self> {
        if value.is_empty() {
            bail!("GitHub reported an empty pull request node ID");
        }
        Ok(Self(value.into_boxed_str()))
    }

    pub(in crate::pre_push) fn as_str(&self) -> &str {
        &self.0
    }
}

/// The two coupled values which identify one GitHub pull request.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub(in crate::pre_push) struct PullRequestIdentity {
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

    pub(in crate::pre_push) fn number(&self) -> PullRequestNumber {
        self.number
    }

    pub(in crate::pre_push) fn node_id(&self) -> &PullRequestNodeId {
        &self.node_id
    }
}

/// Both identity namespaces returned by one exact-local observation.
///
/// This is not a repository-wide uniqueness claim. Numbers and node IDs remain
/// independent because matching one component does not make a create receipt
/// safe. The registry stays coupled to the completed observation.
#[derive(Debug, Default)]
pub(super) struct ExactLocalPullRequestIdentities {
    numbers: HashSet<PullRequestNumber>,
    node_ids: HashSet<PullRequestNodeId>,
}

impl ExactLocalPullRequestIdentities {
    pub(super) fn insert(&mut self, identity: &PullRequestIdentity) -> Result<()> {
        if self.numbers.contains(&identity.number()) {
            bail!(
                "Exact local observation repeats pull request number {}",
                identity.number().get()
            );
        }
        if self.node_ids.contains(identity.node_id()) {
            bail!("Exact local observation repeats a pull request node ID");
        }
        assert!(self.numbers.insert(identity.number()));
        assert!(self.node_ids.insert(identity.node_id().clone()));
        Ok(())
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
        let mut numbers = ExactLocalPullRequestIdentities::default();
        numbers.insert(&one).unwrap();
        assert!(numbers.insert(&same_number).is_err());

        let mut nodes = ExactLocalPullRequestIdentities::default();
        nodes.insert(&one).unwrap();
        assert!(nodes.insert(&same_node).is_err());
    }

    #[test]
    fn failed_identity_insertions_do_not_claim_the_other_component() {
        let one = PullRequestIdentity::new(1, "NODE_ONE".to_owned()).unwrap();
        let number_two_same_node = PullRequestIdentity::new(2, "NODE_ONE".to_owned()).unwrap();
        let number_two = PullRequestIdentity::new(2, "NODE_TWO".to_owned()).unwrap();
        let same_number_node_three = PullRequestIdentity::new(1, "NODE_THREE".to_owned()).unwrap();
        let number_three = PullRequestIdentity::new(3, "NODE_THREE".to_owned()).unwrap();
        let swapped = PullRequestIdentity::new(1, "NODE_TWO".to_owned()).unwrap();

        let mut identities = ExactLocalPullRequestIdentities::default();
        identities.insert(&one).unwrap();
        assert!(identities.insert(&number_two_same_node).is_err());
        identities.insert(&number_two).unwrap();
        assert!(identities.insert(&same_number_node_three).is_err());
        identities.insert(&number_three).unwrap();
        assert!(identities.insert(&swapped).is_err());
        assert_eq!(identities.lengths(), (3, 3));
    }
}

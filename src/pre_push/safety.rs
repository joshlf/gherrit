use std::collections::{HashMap, HashSet};

use color_eyre::eyre::{Result, bail, eyre};

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct PrSafetyInput {
    pub(super) number: u64,
    pub(super) node_id: String,
    pub(super) head_branch: String,
    pub(super) current_base: String,
    pub(super) head_oids: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum StagingReason {
    CurrentBase,
    DesiredBase,
    NearestSafeAncestor,
    DefaultBranch,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct StagingBase {
    pub(super) number: u64,
    pub(super) node_id: String,
    pub(super) head_branch: String,
    pub(super) current_base: String,
    pub(super) staging_base: String,
    pub(super) desired_base: String,
    pub(super) reason: StagingReason,
}

pub(super) fn plan_staging_bases(
    default_branch: &str,
    desired_order: &[String],
    prs: &[PrSafetyInput],
    ref_trajectories: &HashMap<String, Vec<String>>,
    mut is_ancestor: impl FnMut(&str, &str) -> Result<bool>,
) -> Result<Vec<StagingBase>> {
    let desired_parents = parent_map(default_branch, desired_order)?;
    let current_parents = prs
        .iter()
        .map(|pr| (pr.head_branch.clone(), pr.current_base.clone()))
        .collect::<HashMap<_, _>>();
    let desired_ids = desired_order.iter().cloned().collect::<HashSet<_>>();

    let mut plans = Vec::with_capacity(prs.len());
    for pr in prs {
        let desired_base = desired_parents.get(&pr.head_branch).ok_or_else(|| {
            eyre!("PR #{} head `{}` is not present in the desired stack", pr.number, pr.head_branch)
        })?;

        let mut candidates = Vec::new();
        push_candidate(&mut candidates, &pr.current_base, StagingReason::CurrentBase);
        push_candidate(&mut candidates, desired_base, StagingReason::DesiredBase);

        let current_ancestors =
            ancestor_chain(&pr.current_base, default_branch, &current_parents, &desired_ids)?
                .into_iter()
                .collect::<HashSet<_>>();
        for ancestor in
            ancestor_chain(desired_base, default_branch, &desired_parents, &desired_ids)?
        {
            if ancestor != *desired_base
                && ancestor != pr.current_base
                && ancestor != default_branch
                && current_ancestors.contains(&ancestor)
            {
                push_candidate(&mut candidates, &ancestor, StagingReason::NearestSafeAncestor);
            }
        }
        push_candidate(&mut candidates, default_branch, StagingReason::DefaultBranch);

        let mut selected = None;
        for (candidate, reason) in candidates {
            if candidate_is_safe(pr, &candidate, ref_trajectories, &mut is_ancestor)? {
                selected = Some((candidate, reason));
                break;
            }
        }
        let Some((staging_base, reason)) = selected else {
            bail!(
                "No reachability-safe staging base exists for PR #{} ({})",
                pr.number,
                pr.head_branch
            );
        };

        plans.push(StagingBase {
            number: pr.number,
            node_id: pr.node_id.clone(),
            head_branch: pr.head_branch.clone(),
            current_base: pr.current_base.clone(),
            staging_base,
            desired_base: desired_base.clone(),
            reason,
        });
    }

    Ok(plans)
}

fn parent_map(default_branch: &str, order: &[String]) -> Result<HashMap<String, String>> {
    let mut parents = HashMap::new();
    let mut parent = default_branch.to_string();
    for id in order {
        if parents.insert(id.clone(), parent).is_some() {
            bail!("Desired stack contains duplicate GHerrit ID `{id}`");
        }
        parent = id.clone();
    }
    Ok(parents)
}

fn ancestor_chain(
    first: &str,
    default_branch: &str,
    parents: &HashMap<String, String>,
    managed_ids: &HashSet<String>,
) -> Result<Vec<String>> {
    let mut chain = Vec::new();
    let mut current = first.to_string();
    let mut seen = HashSet::new();

    loop {
        if !seen.insert(current.clone()) {
            bail!("Stack topology contains a cycle at `{current}`");
        }
        chain.push(current.clone());
        if current == default_branch || !managed_ids.contains(&current) {
            break;
        }
        let Some(parent) = parents.get(&current) else {
            break;
        };
        current = parent.clone();
    }

    Ok(chain)
}

fn push_candidate(
    candidates: &mut Vec<(String, StagingReason)>,
    branch: &str,
    reason: StagingReason,
) {
    if candidates.iter().all(|(candidate, _)| candidate != branch) {
        candidates.push((branch.to_string(), reason));
    }
}

fn candidate_is_safe(
    pr: &PrSafetyInput,
    candidate: &str,
    ref_trajectories: &HashMap<String, Vec<String>>,
    is_ancestor: &mut impl FnMut(&str, &str) -> Result<bool>,
) -> Result<bool> {
    if candidate == pr.head_branch {
        return Ok(false);
    }
    let Some(base_oids) = ref_trajectories.get(candidate) else {
        // A PR cannot be retargeted before publication to a branch that does
        // not yet exist on the remote.
        return Ok(false);
    };
    if base_oids.is_empty() || pr.head_oids.is_empty() {
        return Ok(false);
    }

    for head_oid in &pr.head_oids {
        for base_oid in base_oids {
            if head_oid == base_oid || is_ancestor(head_oid, base_oid)? {
                return Ok(false);
            }
        }
    }
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn trajectories(entries: &[(&str, &[&str])]) -> HashMap<String, Vec<String>> {
        entries
            .iter()
            .map(|(branch, oids)| {
                ((*branch).to_string(), oids.iter().map(|oid| (*oid).to_string()).collect())
            })
            .collect()
    }

    fn pr(number: u64, head: &str, base: &str, oids: &[&str]) -> PrSafetyInput {
        PrSafetyInput {
            number,
            node_id: format!("PR_{number}"),
            head_branch: head.to_string(),
            current_base: base.to_string(),
            head_oids: oids.iter().map(|oid| (*oid).to_string()).collect(),
        }
    }

    fn ancestry(edges: &[(&str, &str)]) -> impl FnMut(&str, &str) -> Result<bool> + '_ {
        move |ancestor, descendant| {
            Ok(edges.iter().any(|(a, d)| *a == ancestor && *d == descendant))
        }
    }

    #[test]
    fn keeps_a_safe_current_base_before_considering_the_desired_base() {
        let order = vec!["A".to_string(), "C".to_string(), "B".to_string()];
        let refs = trajectories(&[
            ("main", &["M"]),
            ("A", &["A0", "A1"]),
            ("B", &["B0", "B1"]),
            ("C", &["C0", "C1"]),
        ]);
        let plans = plan_staging_bases(
            "main",
            &order,
            &[pr(2, "B", "A", &["B0", "B1"])],
            &refs,
            ancestry(&[]),
        )
        .unwrap();

        assert_eq!(plans[0].staging_base, "A");
        assert_eq!(plans[0].reason, StagingReason::CurrentBase);
        assert_eq!(plans[0].desired_base, "C");
    }

    #[test]
    fn moves_directly_to_a_safe_desired_base() {
        let order = vec!["B".to_string(), "A".to_string()];
        let refs = trajectories(&[("main", &["M"]), ("A", &["A0", "A1"]), ("B", &["B0", "B1"])]);
        let plans = plan_staging_bases(
            "main",
            &order,
            &[pr(2, "B", "A", &["B0", "B1"])],
            &refs,
            ancestry(&[("B1", "A1")]),
        )
        .unwrap();

        assert_eq!(plans[0].staging_base, "main");
        assert_eq!(plans[0].reason, StagingReason::DesiredBase);
    }

    #[test]
    fn chooses_the_nearest_safe_common_ancestor() {
        let order = vec!["A".to_string(), "D".to_string(), "C".to_string(), "B".to_string()];
        let refs = trajectories(&[
            ("main", &["M"]),
            ("A", &["A0", "A1"]),
            ("B", &["B0", "B1"]),
            ("C", &["C0", "C1"]),
            ("D", &["D0", "D1"]),
        ]);
        let prs = [
            pr(1, "A", "main", &["A0", "A1"]),
            pr(2, "B", "A", &["B0", "B1"]),
            pr(3, "C", "B", &["C0", "C1"]),
            pr(4, "D", "C", &["D0", "D1"]),
        ];
        let plans = plan_staging_bases(
            "main",
            &order,
            &prs,
            &refs,
            ancestry(&[("C1", "B1"), ("C0", "D0")]),
        )
        .unwrap();
        let c = plans.iter().find(|plan| plan.head_branch == "C").unwrap();

        assert_eq!(c.staging_base, "A");
        assert_eq!(c.reason, StagingReason::NearestSafeAncestor);
    }

    #[test]
    fn falls_back_to_the_default_branch() {
        let order = vec!["C".to_string(), "B".to_string(), "A".to_string()];
        let refs = trajectories(&[
            ("main", &["M"]),
            ("A", &["A0", "A1"]),
            ("B", &["B0", "B1"]),
            ("C", &["C0", "C1"]),
        ]);
        let prs = [
            pr(1, "A", "main", &["A0", "A1"]),
            pr(2, "B", "A", &["B0", "B1"]),
            pr(3, "C", "B", &["C0", "C1"]),
        ];
        let plans = plan_staging_bases(
            "main",
            &order,
            &prs,
            &refs,
            ancestry(&[("B1", "A1"), ("B0", "C0")]),
        )
        .unwrap();
        let b = plans.iter().find(|plan| plan.head_branch == "B").unwrap();

        assert_eq!(b.staging_base, "main");
        assert_eq!(b.reason, StagingReason::DefaultBranch);
    }

    #[test]
    fn checks_every_old_and_new_oid_combination() {
        let order = vec!["A".to_string(), "B".to_string()];
        let refs = trajectories(&[("main", &["M"]), ("A", &["A0", "A1"])]);
        let plans = plan_staging_bases(
            "main",
            &order,
            &[pr(2, "B", "A", &["B0", "B1"])],
            &refs,
            ancestry(&[("B1", "A0")]),
        )
        .unwrap();

        assert_eq!(plans[0].staging_base, "main");
    }

    #[test]
    fn rejects_a_missing_candidate_branch() {
        let order = vec!["A".to_string(), "B".to_string()];
        let refs = trajectories(&[("main", &["M"])]);
        let plans = plan_staging_bases(
            "main",
            &order,
            &[pr(2, "B", "A", &["B0", "B1"])],
            &refs,
            ancestry(&[]),
        )
        .unwrap();

        assert_eq!(plans[0].staging_base, "main");
    }

    #[test]
    fn fails_when_even_the_default_branch_contains_the_head() {
        let order = vec!["A".to_string()];
        let refs = trajectories(&[("main", &["M"]), ("A", &["A0", "A1"])]);
        let error = plan_staging_bases(
            "main",
            &order,
            &[pr(1, "A", "main", &["A0", "A1"])],
            &refs,
            ancestry(&[("A1", "M")]),
        )
        .unwrap_err();

        assert!(error.to_string().contains("No reachability-safe staging base"));
    }

    #[test]
    fn rejects_cycles_in_the_observed_topology() {
        let order = vec!["A".to_string(), "B".to_string()];
        let refs = trajectories(&[("main", &["M"]), ("A", &["A0"]), ("B", &["B0"])]);
        let error = plan_staging_bases(
            "main",
            &order,
            &[pr(1, "A", "B", &["A0"]), pr(2, "B", "A", &["B0"])],
            &refs,
            ancestry(&[]),
        )
        .unwrap_err();

        assert!(error.to_string().contains("cycle"));
    }
}

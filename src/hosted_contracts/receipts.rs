//! Receipts.

use super::*;

pub(super) fn field(value: &Value, key: &str) -> Result<String> {
    if key == "sha"
        && !value[key].as_str().is_some_and(|value| {
            value.len() == 40
                && value.bytes().all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
        })
    {
        bail!("Invalid hosted object receipt");
    }
    value[key]
        .as_str()
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
        .ok_or_else(|| eyre!("Missing hosted receipt field {key}"))
}
/// Identity and revision from this run's checked creation receipt.
pub(super) struct OwnedReview {
    pub(super) number: u64,
    pub(super) node_id: String,
    pub(super) head: String,
}

impl OwnedReview {
    pub(super) fn from_receipt(
        value: &Value,
        repository_id: u64,
        branch: &str,
        head: &str,
        base: &str,
        previous: &[Self],
    ) -> Result<Self> {
        let number = value["number"]
            .as_u64()
            .filter(|number| *number > 0)
            .ok_or_else(|| eyre!("PR creation acknowledgement is indeterminate"))?;
        let node_id = field(value, "node_id")?;
        if previous.iter().any(|review| review.number == number || review.node_id == node_id)
            || value["head"]["ref"] != branch
            || value["head"]["sha"] != head
            || value["base"]["ref"] != base
            || value["draft"] != true
            || value["state"] != "open"
            || value["head"]["repo"]["id"] != repository_id
            || value["base"]["repo"]["id"] != repository_id
        {
            bail!("Draft creation acknowledgement is indeterminate");
        }
        Ok(Self { number, node_id, head: head.to_owned() })
    }
}

/// Only membership in the exact newly created chain establishes ownership.
pub(super) fn owned_stack_number(
    value: &Value,
    reviews: &[OwnedReview],
    base: &str,
) -> Result<u64> {
    let number = value["number"]
        .as_u64()
        .filter(|number| *number > 0)
        .ok_or_else(|| eyre!("Native stack acknowledgement has no valid number"))?;
    if value["base"]["ref"] != base || value["open"] != true {
        bail!("Native stack acknowledgement has an unexpected base or lifecycle");
    }
    let members = value["pull_requests"]
        .as_array()
        .ok_or_else(|| eyre!("Native stack acknowledgement has no members"))?;
    if reviews.is_empty() || members.len() != reviews.len() {
        bail!("Native stack acknowledgement has an unexpected membership count");
    }
    for (member, review) in members.iter().zip(reviews) {
        if member["number"] != review.number
            || member["node_id"] != review.node_id
            || member["head"]["sha"] != review.head
        {
            bail!("Native stack acknowledgement does not identify the owned ordered chain");
        }
    }
    Ok(number)
}

/// A pending receipt authorizes only polling its exact operation, never a
/// claim that the merge or restack completed. Terminal failure stops the run.
pub(super) fn merge_receipt_uuid(
    value: &Value,
    head: &str,
    polling: Option<&str>,
) -> Result<Option<String>> {
    let details =
        value["details"].as_object().ok_or_else(|| eyre!("Missing merge receipt details"))?;
    if !details.get("message").is_some_and(Value::is_string) {
        bail!("Missing merge receipt message");
    }
    match value["status"].as_str() {
        Some("pending") => {
            let uuid = details
                .get("uuid")
                .and_then(Value::as_str)
                .filter(|uuid| {
                    !uuid.is_empty()
                        && uuid.bytes().all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
                })
                .ok_or_else(|| eyre!("Invalid merge receipt identity"))?;
            if polling.is_some_and(|expected| uuid != expected)
                || details.get("expected_head_sha").and_then(Value::as_str) != Some(head)
                || !matches!(details.get("merge_method").and_then(Value::as_str), Some("merge"))
                || !matches!(
                    details.get("merge_action").and_then(Value::as_str),
                    Some("default" | "merge_queue" | "direct_merge")
                )
                || details.get("bypass_rules").is_some_and(|value| value.as_bool() != Some(false))
            {
                bail!("Pending merge receipt does not identify the submitted operation");
            }
            Ok(Some(uuid.to_owned()))
        }
        Some("merged") => {
            let sha = details
                .get("sha")
                .and_then(Value::as_str)
                .ok_or_else(|| eyre!("Missing merged receipt SHA"))?;
            if !matches!(sha.len(), 40 | 64) || !sha.bytes().all(|byte| byte.is_ascii_hexdigit()) {
                bail!("Invalid merged receipt SHA");
            }
            Ok(None)
        }
        Some("enqueued") => Ok(None),
        Some("failed") => bail!("Asynchronous merge failed; inspect its journaled receipt"),
        _ => bail!("Unrecognized asynchronous merge status"),
    }
}

pub(super) fn workflow_runs_for_pr(runs: &Value, number: u64) -> Result<Vec<Value>> {
    let rows =
        runs["workflow_runs"].as_array().ok_or_else(|| eyre!("Missing workflow run rows"))?;
    if rows.len() > 100 {
        bail!("Workflow observation exceeds its page bound");
    }
    Ok(rows
        .iter()
        .filter(|run| {
            run["pull_requests"]
                .as_array()
                .is_some_and(|prs| prs.iter().any(|pr| pr["number"] == number))
        })
        .cloned()
        .collect())
}

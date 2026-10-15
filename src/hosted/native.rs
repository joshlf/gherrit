//! Native.

use super::*;

pub(super) fn required<'a>(value: &'a Value, key: &str) -> Result<&'a Value> {
    value.get(key).ok_or_else(|| eyre!("Native resource omitted required field '{key}'"))
}

pub(super) fn string_field<'a>(value: &'a Value, key: &str) -> Result<&'a str> {
    required(value, key)?.as_str().ok_or_else(|| eyre!("Invalid native string field '{key}'"))
}

pub(super) fn positive_field(value: &Value, key: &str) -> Result<u64> {
    required(value, key)?
        .as_u64()
        .filter(|value| *value > 0)
        .ok_or_else(|| eyre!("Invalid native identity field '{key}'"))
}

pub(super) fn url_field(value: &Value, key: &str) -> Result<()> {
    url::Url::parse(string_field(value, key)?)
        .map(|_| ())
        .map_err(|_| eyre!("Invalid native URI field '{key}'"))
}

pub(super) fn timestamp_field(value: &Value, key: &str, nullable: bool) -> Result<()> {
    if nullable && required(value, key)?.is_null() {
        return Ok(());
    }
    chrono::DateTime::parse_from_rfc3339(string_field(value, key)?)
        .map(|_| ())
        .map_err(|_| eyre!("Invalid native timestamp field '{key}'"))
}

pub(super) fn validate_native_user(value: &Value) -> Result<()> {
    if value.is_null() {
        return Ok(());
    }
    positive_field(value, "id")?;
    for key in ["login", "node_id", "type"] {
        if string_field(value, key)?.is_empty() {
            bail!("Empty native user identity");
        }
    }
    if !required(value, "site_admin")?.is_boolean() {
        bail!("Invalid native user site_admin");
    }
    for key in [
        "avatar_url",
        "url",
        "html_url",
        "followers_url",
        "following_url",
        "gists_url",
        "starred_url",
        "subscriptions_url",
        "organizations_url",
        "repos_url",
        "events_url",
        "received_events_url",
    ] {
        url_field(value, key)?;
    }
    let gravatar = required(value, "gravatar_id")?;
    if !gravatar.is_null() && !gravatar.is_string() {
        bail!("Invalid native gravatar_id");
    }
    for key in ["name", "email"] {
        if value.get(key).is_some_and(|value| !value.is_null() && !value.is_string()) {
            bail!("Invalid native optional user field '{key}'");
        }
    }
    if value.get("user_view_type").is_some_and(|value| !value.is_string()) {
        bail!("Invalid native user_view_type");
    }
    if value.get("starred_at").is_some() {
        timestamp_field(value, "starred_at", false)?;
    }
    Ok(())
}

pub(super) fn validate_native_revision(value: &Value) -> Result<()> {
    if string_field(value, "ref")?.is_empty() || !oid(string_field(value, "sha")?) {
        bail!("Invalid native revision");
    }
    let repository = required(value, "repo")?;
    positive_field(repository, "id")?;
    if string_field(repository, "name")?.is_empty() {
        bail!("Empty native repository name");
    }
    url_field(repository, "url")
}

/// Validate required fields from the pinned GitHub Stacks REST resource.
/// Extra provider fields remain intact in the original reported JSON.
pub(super) fn validate_stack(
    value: &Value,
    number: u64,
    node_id: &str,
    identities: &mut PullRequestIdentities,
    stack_rest_ids: &mut HashSet<u64>,
) -> Result<()> {
    if positive_field(value, "number")? != number
        || string_field(value, "node_id")? != node_id
        || node_id.is_empty()
        || !required(value, "open")?.is_boolean()
    {
        bail!("Native stack identity disagrees with observed membership");
    }
    if !stack_rest_ids.insert(positive_field(value, "id")?) {
        bail!("Repeated native stack REST identity");
    }
    url_field(value, "url")?;
    timestamp_field(value, "created_at", false)?;
    if string_field(required(value, "base")?, "ref")?.is_empty() {
        bail!("Empty native stack base");
    }
    let members = required(value, "pull_requests")?
        .as_array()
        .ok_or_else(|| eyre!("Missing native stack members"))?;
    if members.is_empty() || members.len() > 100 {
        bail!("Native stack exceeds member budget");
    }
    for member in members {
        let member_number = positive_field(member, "number")?;
        let id = positive_field(member, "id")?;
        let node = string_field(member, "node_id")?;
        if node.is_empty()
            || !matches!(string_field(member, "state")?, "open" | "closed")
            || !required(member, "draft")?.is_boolean()
        {
            bail!("Incomplete or repeated native stack member");
        }
        identities.observe_member(member_number, node, id)?;
        string_field(member, "title")?;
        url_field(member, "url")?;
        url_field(member, "html_url")?;
        timestamp_field(member, "merged_at", true)?;
        validate_native_user(required(member, "user")?)?;
        validate_native_revision(required(member, "head")?)?;
        validate_native_revision(required(member, "base")?)?;
        if !member["merged_at"].is_null() && member["state"] != "closed" {
            bail!("Merged native member is not closed");
        }
    }
    if value["open"] != members.iter().any(|member| member["state"] == "open") {
        bail!("Native stack open state disagrees with its members");
    }
    Ok(())
}

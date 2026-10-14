//! Factual GitHub reads. No value in this module permits a publication effect.
//! The publisher retains its consuming OPEN-only admissibility decoder.

use std::{
    collections::{BTreeMap, HashSet},
    io::Write as _,
    time::{Duration, Instant},
};

use eyre::{Result, WrapErr, bail, eyre};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::{
    identity::GherritPrId,
    pre_push::{json::UniqueJson, subprocess},
    util,
};

const MAX_PAGES: usize = 32;
const MAX_ROWS: usize = 1000;
const QUERY: &str = "query($owner:String!,$repo:String!,$head:String!,$after:String){repository(owner:$owner,name:$repo){id nameWithOwner pullRequests(headRefName:$head,states:[OPEN,CLOSED,MERGED],first:100,after:$after){nodes{number id state isDraft headRefName headRefOid baseRefName baseRefOid headRepository{id nameWithOwner} baseRepository{id nameWithOwner} stack{id number size baseRefName} stackEntry{position stack{id}}} pageInfo{hasNextPage endCursor}}}}";

#[derive(Clone, Debug, Deserialize, Serialize, Eq, PartialEq)]
#[serde(rename_all = "camelCase")]
pub(crate) struct RepositoryIdentity {
    id: String,
    name_with_owner: String,
}

#[derive(Debug, Deserialize, Serialize, Eq, PartialEq)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
enum Lifecycle {
    Open,
    Closed,
    Merged,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
struct NativeSummary {
    id: String,
    number: u64,
    size: usize,
    base_ref_name: String,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
struct NativeEntry {
    position: usize,
    stack: NativeIdentity,
}
#[derive(Debug, Deserialize, Serialize)]
struct NativeIdentity {
    id: String,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct PullRequestFacts {
    number: u64,
    id: String,
    state: Lifecycle,
    is_draft: bool,
    head_ref_name: String,
    head_ref_oid: String,
    base_ref_name: String,
    base_ref_oid: String,
    head_repository: Option<RepositoryIdentity>,
    base_repository: RepositoryIdentity,
    stack: Option<NativeSummary>,
    stack_entry: Option<NativeEntry>,
}

#[derive(Debug, Serialize)]
pub(crate) struct HeadFacts {
    repository: RepositoryIdentity,
    head: String,
    pages: usize,
    pull_requests: Vec<PullRequestFacts>,
    // Complete exhaustion means no matching PRs is observed absence. Failure
    // returns no partial HeadFacts and therefore cannot imply absence.
}

fn oid(value: &str) -> bool {
    matches!(value.len(), 40 | 64)
        && value.bytes().all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
}

fn data(response: Value) -> Result<Value> {
    if response
        .get("errors")
        .is_some_and(|errors| errors.as_array().is_none_or(|errors| !errors.is_empty()))
    {
        bail!("GitHub factual query returned errors");
    }
    response
        .get("data")
        .filter(|value| value.is_object())
        .cloned()
        .ok_or_else(|| eyre!("GitHub factual query has no complete data"))
}

fn read_head(
    owner: &str,
    repo: &str,
    id: &GherritPrId,
    call: &mut impl FnMut(Value) -> Result<Value>,
) -> Result<HeadFacts> {
    let mut cursor: Option<String> = None;
    let mut cursors = HashSet::new();
    let mut numbers = HashSet::new();
    let mut nodes = HashSet::new();
    let mut identity: Option<RepositoryIdentity> = None;
    let mut rows = Vec::new();
    for page in 1..=MAX_PAGES {
        let response = data(call(
            json!({"query": QUERY, "variables":{"owner":owner,"repo":repo,"head":id.as_str(),"after":cursor}}),
        )?)?;
        let repository = response
            .get("repository")
            .filter(|value| value.is_object())
            .ok_or_else(|| eyre!("Repository is unavailable"))?;
        let observed: RepositoryIdentity = serde_json::from_value(repository.clone())?;
        if observed.id.is_empty()
            || !observed.name_with_owner.eq_ignore_ascii_case(&format!("{owner}/{repo}"))
            || identity.as_ref().is_some_and(|identity| *identity != observed)
        {
            bail!("Repository identity changed or disagrees with requested repository");
        }
        let connection = repository
            .get("pullRequests")
            .ok_or_else(|| eyre!("Missing pull request connection"))?;
        let values = connection
            .get("nodes")
            .and_then(Value::as_array)
            .ok_or_else(|| eyre!("Missing pull request rows"))?;
        if values.len() > 100 || rows.len() + values.len() > MAX_ROWS {
            bail!("Factual query exceeds row budget");
        }
        for value in values {
            // serde Option alone cannot distinguish an omitted provider field
            // from a witnessed null membership/deleted head repository.
            if ["stack", "stackEntry", "headRepository"].iter().any(|key| value.get(key).is_none())
            {
                bail!("Incomplete factual pull request row");
            }
            let row: PullRequestFacts = serde_json::from_value(value.clone())?;
            if row.number == 0
                || row.number > i32::MAX as u64
                || row.id.is_empty()
                || !numbers.insert(row.number)
                || !nodes.insert(row.id.clone())
                || row.head_ref_name != id.as_str()
                || row.base_repository != observed
                || !oid(&row.head_ref_oid)
                || !oid(&row.base_ref_oid)
            {
                bail!(
                    "Pull request identity, exact head, repository, or revision disagrees with query"
                );
            }
            match (&row.stack, &row.stack_entry) {
                (None, None) => {}
                (Some(stack), Some(entry))
                    if !stack.id.is_empty()
                        && stack.number > 0
                        && (1..=100).contains(&stack.size)
                        && (1..=stack.size).contains(&entry.position)
                        && entry.stack.id == stack.id => {}
                _ => bail!("Native membership is incomplete or inconsistent"),
            }
            rows.push(row);
        }
        identity = Some(observed);
        let info = connection.get("pageInfo").ok_or_else(|| eyre!("Missing page information"))?;
        let more = info
            .get("hasNextPage")
            .and_then(Value::as_bool)
            .ok_or_else(|| eyre!("Missing pagination completion"))?;
        if !more {
            return Ok(HeadFacts {
                repository: identity.unwrap(),
                head: id.as_str().to_owned(),
                pages: page,
                pull_requests: rows,
            });
        }
        let next = info
            .get("endCursor")
            .and_then(Value::as_str)
            .filter(|cursor| !cursor.is_empty())
            .ok_or_else(|| eyre!("Missing progressing cursor"))?;
        if !cursors.insert(next.to_owned()) {
            bail!("Repeated factual query cursor");
        }
        cursor = Some(next.to_owned());
    }
    bail!("Factual query exceeds page budget")
}

/// Bounded read-only gh adapter; auth is explicit and mutations are not an API.
struct ReadApi {
    token: String,
    deadline: Instant,
}
impl ReadApi {
    fn new() -> Result<Self> {
        Ok(Self {
            token: util::get_github_token()?,
            deadline: Instant::now() + Duration::from_secs(120),
        })
    }
    fn call(&mut self, route: &str, request: Option<Value>) -> Result<Value> {
        let remaining = self
            .deadline
            .checked_duration_since(Instant::now())
            .ok_or_else(|| eyre!("GitHub inspection deadline exceeded"))?;
        let mut command = util::cmd(
            "gh",
            [
                "api",
                "--hostname",
                "github.com",
                "--method",
                if request.is_some() { "POST" } else { "GET" },
                route,
                "-H",
                "Accept: application/vnd.github+json",
                "-H",
                "X-GitHub-Api-Version: 2026-03-10",
            ],
        );
        command.env("GH_TOKEN", &self.token).env("GH_PROMPT_DISABLED", "1");
        let input = if let Some(request) = request {
            // The only POST route is the read-only GraphQL document above.
            if route != "graphql" || request["query"] != QUERY {
                bail!("Read-only inspection rejects this request");
            }
            command.args(["--input", "-"]);
            let mut input = subprocess::RegularFileStdinBuilder::new()?;
            input.write_all(&serde_json::to_vec(&request)?)?;
            Some(input.finish()?)
        } else {
            None
        };
        let output = subprocess::local_output(
            &mut command,
            input,
            remaining.min(Duration::from_secs(60)),
            4 * 1024 * 1024,
        )?;
        if !output.status.success() {
            bail!(
                "GitHub inspection request failed; check credentials, permissions, and service availability"
            );
        }
        Ok(UniqueJson::decode(&output.stdout).wrap_err("Malformed factual response")?.into_value())
    }
}

pub(crate) fn observe(owner: &str, repository: &str, ids: &[GherritPrId]) -> Result<Value> {
    if ids.is_empty() {
        return Ok(
            json!({"state":"observed", "heads": [], "native_stacks": [], "publication_authority":false}),
        );
    }
    let mut api = ReadApi::new()?;
    let mut heads = Vec::new();
    let mut memberships = BTreeMap::new();
    for id in ids {
        let head =
            read_head(owner, repository, id, &mut |request| api.call("graphql", Some(request)))?;
        for pr in &head.pull_requests {
            if let Some(stack) = &pr.stack {
                if memberships.get(&stack.number).is_some_and(|old| *old != stack.id) {
                    bail!("Native stack identity changed during inspection");
                }
                memberships.entry(stack.number).or_insert_with(|| stack.id.clone());
            }
        }
        heads.push(head);
    }
    let mut stacks = Vec::new();
    for (number, node_id) in memberships {
        let stack = api.call(&format!("repos/{owner}/{repository}/stacks/{number}"), None)?;
        validate_stack(&stack, number, &node_id)?;
        // Report the complete ordered native resource as provider facts.
        stacks.push(stack);
    }
    Ok(
        json!({"state":"observed", "heads":heads, "native_stacks":stacks, "publication_authority":false,
        "basis":"sequential read observations; not an atomic snapshot"}),
    )
}

fn validate_stack(value: &Value, number: u64, node_id: &str) -> Result<()> {
    if value["number"] != number || value["node_id"] != node_id || !value["open"].is_boolean() {
        bail!("Native stack identity disagrees with observed membership");
    }
    let members =
        value["pull_requests"].as_array().ok_or_else(|| eyre!("Missing native stack members"))?;
    if members.is_empty() || members.len() > 100 {
        bail!("Native stack exceeds member budget");
    }
    let mut numbers = HashSet::new();
    for member in members {
        let number = member["number"]
            .as_u64()
            .filter(|number| *number > 0)
            .ok_or_else(|| eyre!("Invalid native member identity"))?;
        if !numbers.insert(number)
            || !matches!(member["state"].as_str(), Some("open" | "closed"))
            || member["head"]["ref"].as_str().is_none()
            || !member["head"]["sha"].as_str().is_some_and(oid)
        {
            bail!("Incomplete or repeated native stack member");
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    fn id() -> GherritPrId {
        GherritPrId::from_ref_component(b"Gone").unwrap()
    }
    fn row(number: u64, state: &str) -> Value {
        json!({"number":number,"id":format!("PR{number}"),"state":state,"isDraft":false,
        "headRefName":"Gone","headRefOid":"1111111111111111111111111111111111111111","baseRefName":"main","baseRefOid":"2222222222222222222222222222222222222222",
        "headRepository":{"id":"R","nameWithOwner":"owner/repo"},"baseRepository":{"id":"R","nameWithOwner":"owner/repo"},"stack":null,"stackEntry":null})
    }
    fn page(rows: Vec<Value>, more: bool, cursor: Option<&str>) -> Value {
        json!({"data":{"repository":{"id":"R","nameWithOwner":"owner/repo",
        "pullRequests":{"nodes":rows,"pageInfo":{"hasNextPage":more,"endCursor":cursor}}}}})
    }
    #[test]
    fn terminal_facts_are_complete_only_after_exhaustion() {
        let mut requests = Vec::new();
        let mut responses = [
            page(vec![row(7, "CLOSED")], true, Some("opaque")),
            page(vec![row(8, "MERGED"), row(9, "OPEN")], false, None),
        ]
        .into_iter();
        let facts = read_head("owner", "repo", &id(), &mut |request| {
            requests.push(request);
            Ok(responses.next().unwrap())
        })
        .unwrap();
        assert_eq!(facts.pages, 2);
        assert_eq!(facts.pull_requests.iter().map(|pr| pr.number).collect::<Vec<_>>(), [7, 8, 9]);
        assert_eq!(requests[0]["variables"]["after"], Value::Null);
        assert_eq!(requests[1]["variables"]["after"], "opaque");
        assert!(!serde_json::to_string(&facts).unwrap().contains("authority"));
    }
    #[test]
    fn malformed_scope_pagination_and_unknown_membership_never_become_absence() {
        for field in ["number", "id", "headRefName", "baseRepository", "headRefOid", "stackEntry"] {
            let mut bad = row(7, "CLOSED");
            bad.as_object_mut().unwrap().remove(field);
            assert!(
                read_head("owner", "repo", &id(), &mut |_| Ok(page(
                    vec![bad.clone()],
                    false,
                    None
                )))
                .is_err(),
                "{field}"
            );
        }
        let mut missing = row(7, "OPEN");
        missing.as_object_mut().unwrap().remove("stack");
        assert!(
            read_head("owner", "repo", &id(), &mut |_| Ok(page(
                vec![missing.clone()],
                false,
                None
            )))
            .is_err()
        );
        assert!(
            read_head("owner", "repo", &id(), &mut |_| Ok(page(vec![], true, Some("repeat"))))
                .is_err()
        );
        assert!(read_head("owner", "repo", &id(), &mut |_| Ok(page(vec![], true, None))).is_err());
        assert!(
            read_head("owner", "repo", &id(), &mut |_| Ok(page(
                vec![row(7, "OPEN"), row(7, "OPEN")],
                false,
                None
            )))
            .is_err()
        );
        assert!(read_head("owner", "repo", &id(), &mut |_| Ok(json!({"data":null}))).is_err());
        assert!(
            read_head("owner", "repo", &id(), &mut |_| Ok(json!({"data":{},"errors":[{}]})))
                .is_err()
        );
        assert!(read_head("owner", "repo", &id(), &mut |_| Err(eyre!("unavailable"))).is_err());
    }

    #[test]
    fn valid_but_wrong_scope_or_lost_second_page_has_no_complete_result() {
        for wrong in ["head", "base"] {
            let mut value = row(7, "OPEN");
            if wrong == "head" {
                value["headRefName"] = json!("Gother");
            } else {
                value["baseRepository"]["id"] = json!("other repository");
            }
            assert!(
                read_head("owner", "repo", &id(), &mut |_| Ok(page(
                    vec![value.clone()],
                    false,
                    None
                )))
                .is_err()
            );
        }
        let mut count = 0;
        assert!(
            read_head("owner", "repo", &id(), &mut |_| {
                count += 1;
                if count == 1 {
                    Ok(page(vec![row(7, "CLOSED")], true, Some("next")))
                } else {
                    Err(eyre!("lost second page"))
                }
            })
            .is_err()
        );
        assert_eq!(count, 2);
    }
    #[test]
    fn explicit_empty_connection_and_deleted_or_fork_head_are_facts() {
        let facts =
            read_head("owner", "repo", &id(), &mut |_| Ok(page(vec![], false, None))).unwrap();
        assert!(facts.pull_requests.is_empty());
        for head in [Value::Null, json!({"id":"fork","nameWithOwner":"other/repo"})] {
            let mut value = row(7, "CLOSED");
            value["headRepository"] = head;
            assert_eq!(
                read_head("owner", "repo", &id(), &mut |_| Ok(page(
                    vec![value.clone()],
                    false,
                    None
                )))
                .unwrap()
                .pull_requests
                .len(),
                1
            );
        }
    }
    #[test]
    fn native_order_and_resource_identity_are_independent_checks() {
        let member = |number| json!({"number":number,"state":"open","head":{"ref":format!("G{number}"),"sha":"1111111111111111111111111111111111111111"}});
        let mut stack =
            json!({"number":3,"node_id":"S","open":true,"pull_requests":[member(9),member(7)]});
        validate_stack(&stack, 3, "S").unwrap();
        assert_eq!(stack["pull_requests"][0]["number"], 9);
        assert!(validate_stack(&stack, 4, "S").is_err());
        assert!(validate_stack(&stack, 3, "other").is_err());
        stack["pull_requests"] = json!([member(7), member(7)]);
        assert!(validate_stack(&stack, 3, "S").is_err());
    }
}

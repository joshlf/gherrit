//! Factual GitHub reads. No value in this module permits a publication effect.
//! The publisher retains its consuming OPEN-only admissibility decoder.

use std::{
    collections::{BTreeMap, HashMap, HashSet},
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
const MAX_RESPONSE_BYTES: usize = 4 * 1024 * 1024;
const MAX_OBSERVATION_BYTES: usize = 16 * 1024 * 1024;
const MAX_OBSERVATION_ROWS: usize = 10_000;

/// Whole-observation bounds complement each response/connection's limit.
struct ReadBudget {
    remaining_bytes: usize,
    remaining_rows: usize,
}

impl Default for ReadBudget {
    fn default() -> Self {
        Self { remaining_bytes: MAX_OBSERVATION_BYTES, remaining_rows: MAX_OBSERVATION_ROWS }
    }
}

impl ReadBudget {
    fn response_limit(&self) -> Result<usize> {
        if self.remaining_bytes == 0 {
            bail!("Hosted observation exhausted its response byte budget");
        }
        Ok(self.remaining_bytes.min(MAX_RESPONSE_BYTES))
    }

    fn decode(&mut self, bytes: &[u8]) -> Result<Value> {
        self.remaining_bytes = self
            .remaining_bytes
            .checked_sub(bytes.len())
            .ok_or_else(|| eyre!("Hosted observation exceeds its response byte budget"))?;
        Ok(UniqueJson::decode(bytes).wrap_err("Malformed factual response")?.into_value())
    }

    fn retain_rows(&mut self, count: usize) -> Result<()> {
        self.remaining_rows = self
            .remaining_rows
            .checked_sub(count)
            .ok_or_else(|| eyre!("Hosted observation exceeds its aggregate row budget"))?;
        Ok(())
    }
}

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

#[derive(Default)]
struct PullRequestIdentities {
    numbers: HashMap<u64, String>,
    nodes: HashSet<String>,
    native_numbers: HashSet<u64>,
    native_ids: HashSet<u64>,
}

impl PullRequestIdentities {
    fn observe_head(&mut self, number: u64, node: &str) -> Result<()> {
        if self.numbers.contains_key(&number) || !self.nodes.insert(node.to_owned()) {
            bail!("Repeated factual pull request identity");
        }
        self.numbers.insert(number, node.to_owned());
        Ok(())
    }

    fn observe_member(&mut self, number: u64, node: &str, rest_id: u64) -> Result<()> {
        if !self.native_numbers.insert(number) || !self.native_ids.insert(rest_id) {
            bail!("Repeated native pull request identity");
        }
        // A PR can occur once in each representation. Its immutable identity
        // must agree across them, while mutable fields are sequential facts.
        if let Some(known) = self.numbers.get(&number) {
            if known != node {
                bail!("Native pull request number and node ID disagree with prior facts");
            }
        } else {
            if !self.nodes.insert(node.to_owned()) {
                bail!("Native pull request node ID is repeated under different numbers");
            }
            self.numbers.insert(number, node.to_owned());
        }
        Ok(())
    }
}

#[derive(Default)]
struct NativeStackIdentities {
    by_number: BTreeMap<u64, String>,
    nodes: HashSet<String>,
}

impl NativeStackIdentities {
    fn register(&mut self, stack: &NativeSummary) -> Result<()> {
        if let Some(old) = self.by_number.get(&stack.number) {
            if *old != stack.id {
                bail!("Native stack identity changed during inspection");
            }
            return Ok(());
        }
        if !self.nodes.insert(stack.id.clone()) {
            bail!("Native stack node ID is repeated under different numbers");
        }
        self.by_number.insert(stack.number, stack.id.clone());
        Ok(())
    }
}

fn read_head(
    owner: &str,
    repo: &str,
    id: &GherritPrId,
    call: &mut impl FnMut(Value) -> Result<Value>,
    identities: &mut PullRequestIdentities,
) -> Result<HeadFacts> {
    let mut cursor: Option<String> = None;
    let mut cursors = HashSet::new();
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
                || identities.observe_head(row.number, &row.id).is_err()
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
    fn call(
        &mut self,
        route: &str,
        request: Option<Value>,
        budget: &mut ReadBudget,
    ) -> Result<Value> {
        let response_limit = budget.response_limit()?;
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
            response_limit,
        )?;
        if !output.status.success() {
            bail!(
                "GitHub inspection request failed; check credentials, permissions, and service availability"
            );
        }
        budget.decode(&output.stdout)
    }
}

pub(crate) fn observe(owner: &str, repository: &str, ids: &[GherritPrId]) -> Result<Value> {
    if ids.is_empty() {
        return Ok(
            json!({"state":"observed", "heads": [], "native_stacks": [], "publication_authority":false}),
        );
    }
    let mut api = ReadApi::new()?;
    observe_with(
        owner,
        repository,
        ids,
        &mut ReadBudget::default(),
        &mut |route, request, budget| api.call(route, request, budget),
    )
}

fn observe_with(
    owner: &str,
    repository: &str,
    ids: &[GherritPrId],
    budget: &mut ReadBudget,
    call: &mut impl FnMut(&str, Option<Value>, &mut ReadBudget) -> Result<Value>,
) -> Result<Value> {
    let mut heads = Vec::new();
    let mut memberships = NativeStackIdentities::default();
    let mut identities = PullRequestIdentities::default();
    for id in ids {
        let head = read_head(
            owner,
            repository,
            id,
            &mut |request| call("graphql", Some(request), budget),
            &mut identities,
        )?;
        budget.retain_rows(head.pull_requests.len())?;
        for pr in &head.pull_requests {
            if let Some(stack) = &pr.stack {
                memberships.register(stack)?;
            }
        }
        heads.push(head);
    }
    let mut stacks = Vec::new();
    for (number, node_id) in memberships.by_number {
        let stack = call(&format!("repos/{owner}/{repository}/stacks/{number}"), None, budget)?;
        validate_stack(&stack, number, &node_id, &mut identities)?;
        budget.retain_rows(
            stack["pull_requests"].as_array().expect("validated stack members").len(),
        )?;
        // Report the complete ordered native resource as provider facts.
        stacks.push(stack);
    }
    Ok(
        json!({"state":"observed", "heads":heads, "native_stacks":stacks, "publication_authority":false,
        "basis":"sequential read observations; not an atomic snapshot"}),
    )
}

fn required<'a>(value: &'a Value, key: &str) -> Result<&'a Value> {
    value.get(key).ok_or_else(|| eyre!("Native resource omitted required field '{key}'"))
}

fn string_field<'a>(value: &'a Value, key: &str) -> Result<&'a str> {
    required(value, key)?.as_str().ok_or_else(|| eyre!("Invalid native string field '{key}'"))
}

fn positive_field(value: &Value, key: &str) -> Result<u64> {
    required(value, key)?
        .as_u64()
        .filter(|value| *value > 0)
        .ok_or_else(|| eyre!("Invalid native identity field '{key}'"))
}

fn url_field(value: &Value, key: &str) -> Result<()> {
    url::Url::parse(string_field(value, key)?)
        .map(|_| ())
        .map_err(|_| eyre!("Invalid native URI field '{key}'"))
}

fn timestamp_field(value: &Value, key: &str, nullable: bool) -> Result<()> {
    if nullable && required(value, key)?.is_null() {
        return Ok(());
    }
    chrono::DateTime::parse_from_rfc3339(string_field(value, key)?)
        .map(|_| ())
        .map_err(|_| eyre!("Invalid native timestamp field '{key}'"))
}

fn validate_native_user(value: &Value) -> Result<()> {
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

fn validate_native_revision(value: &Value) -> Result<()> {
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
fn validate_stack(
    value: &Value,
    number: u64,
    node_id: &str,
    identities: &mut PullRequestIdentities,
) -> Result<()> {
    if positive_field(value, "number")? != number
        || string_field(value, "node_id")? != node_id
        || node_id.is_empty()
        || !required(value, "open")?.is_boolean()
    {
        bail!("Native stack identity disagrees with observed membership");
    }
    positive_field(value, "id")?;
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

#[cfg(test)]
mod tests {
    use super::*;
    fn validate_stack(value: &Value, number: u64, node: &str) -> Result<()> {
        super::validate_stack(value, number, node, &mut PullRequestIdentities::default())
    }

    fn read_head(
        owner: &str,
        repo: &str,
        id: &GherritPrId,
        call: &mut impl FnMut(Value) -> Result<Value>,
    ) -> Result<HeadFacts> {
        super::read_head(owner, repo, id, call, &mut PullRequestIdentities::default())
    }
    #[test]
    fn identities_repeated_across_heads_make_observation_unavailable() {
        for duplicate_number in [false, true] {
            let mut identities = PullRequestIdentities::default();
            super::read_head(
                "owner",
                "repo",
                &id(),
                &mut |_| Ok(page(vec![row(7, "OPEN")], false, None)),
                &mut identities,
            )
            .unwrap();
            let other = GherritPrId::from_ref_component(b"Gtwo").unwrap();
            let mut renamed = row(if duplicate_number { 7 } else { 8 }, "OPEN");
            renamed["headRefName"] = json!("Gtwo");
            if !duplicate_number {
                renamed["id"] = json!("PR7");
            }
            assert!(
                super::read_head(
                    "owner",
                    "repo",
                    &other,
                    &mut |_| Ok(page(vec![renamed.clone()], false, None)),
                    &mut identities
                )
                .is_err()
            );
        }
    }
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
    fn native_member(number: u64) -> Value {
        let revision = |name: &str| {
            json!({"ref":name,"sha":"1111111111111111111111111111111111111111",
            "repo":{"id":1,"url":"https://api.github.com/repos/owner/repo","name":"repo"}})
        };
        json!({"id":number+100,"number":number,"node_id":format!("PR{number}"),"title":"Title",
            "state":"open","draft":false,"merged_at":null,"user":null,
            "url":format!("https://api.github.com/repos/owner/repo/pulls/{number}"),
            "html_url":format!("https://github.com/owner/repo/pull/{number}"),
            "head":revision(&format!("G{number}")),"base":revision("main")})
    }

    fn native_stack() -> Value {
        json!({"id":13,"number":3,"node_id":"S","url":"https://api.github.com/repos/owner/repo/stacks/3",
            "created_at":"2026-10-01T12:00:00Z","base":{"ref":"main"},"open":true,
            "pull_requests":[native_member(9),native_member(7)],"provider_extra":{"retained":true}})
    }

    #[test]
    fn native_order_and_resource_identity_are_independent_checks() {
        let mut stack = native_stack();
        validate_stack(&stack, 3, "S").unwrap();
        assert_eq!(stack["pull_requests"][0]["number"], 9);
        assert_eq!(stack["provider_extra"]["retained"], true);
        assert!(validate_stack(&stack, 4, "S").is_err());
        assert!(validate_stack(&stack, 3, "other").is_err());
        for key in ["id", "number", "node_id"] {
            let mut repeated = stack.clone();
            repeated["pull_requests"][1][key] = repeated["pull_requests"][0][key].clone();
            assert!(validate_stack(&repeated, 3, "S").is_err(), "accepted duplicate {key}");
        }
        stack["pull_requests"] = json!([native_member(7), native_member(7)]);
        assert!(validate_stack(&stack, 3, "S").is_err());
    }

    #[test]
    fn native_required_fields_are_not_complete_when_absent_or_malformed() {
        let valid = native_stack();
        for pointer in [
            "",
            "/base",
            "/pull_requests/0",
            "/pull_requests/0/head",
            "/pull_requests/0/base",
            "/pull_requests/0/head/repo",
            "/pull_requests/0/base/repo",
        ] {
            for key in valid.pointer(pointer).unwrap().as_object().unwrap().keys() {
                if key == "provider_extra" {
                    continue;
                }
                let mut incomplete = valid.clone();
                incomplete.pointer_mut(pointer).unwrap().as_object_mut().unwrap().remove(key);
                assert!(
                    validate_stack(&incomplete, 3, "S").is_err(),
                    "accepted absent {pointer}/{key}"
                );
            }
        }
        for (pointer, value) in [
            ("/created_at", json!("not a timestamp")),
            ("/url", json!("relative")),
            ("/pull_requests/0/url", Value::Null),
            ("/pull_requests/0/draft", json!("false")),
            ("/pull_requests/0/head/sha", json!("no oid")),
            ("/pull_requests/0/base/repo", Value::Null),
            ("/pull_requests/0/merged_at", json!("yesterday")),
            ("/pull_requests/0/user", json!({})),
        ] {
            let mut malformed = valid.clone();
            *malformed.pointer_mut(pointer).unwrap() = value;
            assert!(validate_stack(&malformed, 3, "S").is_err(), "accepted malformed {pointer}");
        }
    }

    #[test]
    fn native_nullable_user_requires_complete_fields_when_present() {
        let mut user = json!({"id":1,"login":"reviewer","node_id":"U1","type":"User",
            "site_admin":false,"gravatar_id":null,"name":null,"email":null});
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
            user[key] = json!(format!("https://api.github.com/users/reviewer/{key}"));
        }
        let mut stack = native_stack();
        stack["pull_requests"][0]["user"] = user.clone();
        validate_stack(&stack, 3, "S").unwrap();
        for key in user.as_object().unwrap().keys() {
            if ["name", "email"].contains(&key.as_str()) {
                continue;
            }
            let mut incomplete = stack.clone();
            incomplete["pull_requests"][0]["user"].as_object_mut().unwrap().remove(key);
            assert!(validate_stack(&incomplete, 3, "S").is_err(), "accepted missing user {key}");
        }
    }
    fn factual_response(request: &Value, number: u64, stack: Option<(u64, &str)>) -> Value {
        let mut value = row(number, "OPEN");
        value["headRefName"] = request["variables"]["head"].clone();
        if let Some((number, node)) = stack {
            value["stack"] = json!({"id":node,"number":number,"size":2,"baseRefName":"main"});
            value["stackEntry"] = json!({"position":1,"stack":{"id":node}});
        }
        page(vec![value], false, None)
    }

    #[test]
    fn aggregate_rows_span_heads_and_allow_the_exact_limit() {
        let ids = [
            id(),
            GherritPrId::from_ref_component(b"Gtwo").unwrap(),
            GherritPrId::from_ref_component(b"Gthree").unwrap(),
        ];
        for head_count in [2, 3] {
            let mut budget = ReadBudget { remaining_rows: 2, ..ReadBudget::default() };
            let mut calls = 0;
            let result = observe_with(
                "owner",
                "repo",
                &ids[..head_count],
                &mut budget,
                &mut |route, request, budget| {
                    assert_eq!(route, "graphql");
                    calls += 1;
                    budget.decode(
                        &serde_json::to_vec(&factual_response(&request.unwrap(), calls, None))
                            .unwrap(),
                    )
                },
            );
            assert_eq!(result.is_ok(), head_count == 2);
            assert_eq!(budget.remaining_rows, 0);
            assert_eq!(calls, head_count as u64);
        }
    }

    #[test]
    fn native_resources_share_the_row_and_raw_byte_budgets() {
        let head = factual_response(&json!({"variables":{"head":"Gone"}}), 7, Some((3, "S")));
        let total = serde_json::to_vec(&head).unwrap().len()
            + serde_json::to_vec(&native_stack()).unwrap().len();
        for (rows, bytes, success) in [(3, total, true), (2, total, false), (3, total - 1, false)] {
            let mut budget = ReadBudget { remaining_rows: rows, remaining_bytes: bytes };
            let mut calls = 0;
            let result = observe_with(
                "owner",
                "repo",
                &[id()],
                &mut budget,
                &mut |route, request, budget| {
                    calls += 1;
                    let value = if route == "graphql" {
                        factual_response(&request.unwrap(), 7, Some((3, "S")))
                    } else {
                        assert_eq!(route, "repos/owner/repo/stacks/3");
                        native_stack()
                    };
                    budget.decode(&serde_json::to_vec(&value).unwrap())
                },
            );
            assert_eq!(result.is_ok(), success);
            assert_eq!(calls, 2);
            if !success {
                let error = result.unwrap_err().to_string();
                assert!(error.contains(if rows == 2 { "row budget" } else { "byte budget" }));
            }
        }
    }

    #[test]
    fn raw_response_budget_is_checked_before_decode_and_caps_the_child() {
        let mut budget = ReadBudget { remaining_bytes: 2, ..ReadBudget::default() };
        assert_eq!(budget.response_limit().unwrap(), 2);
        assert_eq!(budget.decode(b"{}").unwrap(), json!({}));
        assert!(budget.response_limit().is_err());
        assert!(budget.decode(b" ").unwrap_err().to_string().contains("byte budget"));
        let mut budget = ReadBudget::default();
        assert_eq!(budget.response_limit().unwrap(), MAX_RESPONSE_BYTES);
        assert!(budget.decode(b"{}").is_ok());
        assert_eq!(budget.remaining_bytes, MAX_OBSERVATION_BYTES - 2);
    }

    #[test]
    fn native_stack_identities_are_one_to_one_across_heads() {
        let ids = [id(), GherritPrId::from_ref_component(b"Gtwo").unwrap()];
        for memberships in [[(3, "S"), (4, "S")], [(3, "S"), (3, "T")]] {
            let mut calls = 0;
            let result = observe_with(
                "owner",
                "repo",
                &ids,
                &mut ReadBudget::default(),
                &mut |route, request, budget| {
                    assert_eq!(route, "graphql", "contradictory identity must stop before REST");
                    let membership = memberships[calls];
                    calls += 1;
                    budget.decode(
                        &serde_json::to_vec(&factual_response(
                            &request.unwrap(),
                            calls as u64,
                            Some(membership),
                        ))
                        .unwrap(),
                    )
                },
            );
            assert!(result.is_err());
            assert_eq!(calls, 2);
        }
        let mut identities = NativeStackIdentities::default();
        let stack = NativeSummary {
            id: "S".to_owned(),
            number: 3,
            size: 2,
            base_ref_name: "main".to_owned(),
        };
        identities.register(&stack).unwrap();
        identities.register(&stack).unwrap();
        assert_eq!(identities.by_number.len(), 1);
    }
    #[test]
    fn member_identity_is_unique_across_native_resources_and_agrees_with_graphql() {
        let first = native_stack();
        for key in ["number", "node_id", "id"] {
            let mut identities = PullRequestIdentities::default();
            super::validate_stack(&first, 3, "S", &mut identities).unwrap();
            let mut second = first.clone();
            second["number"] = json!(4);
            second["node_id"] = json!("S4");
            second["pull_requests"] = json!([native_member(42)]);
            second["pull_requests"][0][key] = first["pull_requests"][0][key].clone();
            assert!(super::validate_stack(&second, 4, "S4", &mut identities).is_err(), "{key}");
        }
        let member = &first["pull_requests"][0];
        let mut identities = PullRequestIdentities::default();
        identities.observe_head(9, member["node_id"].as_str().unwrap()).unwrap();
        super::validate_stack(&first, 3, "S", &mut identities).unwrap();
        for (number, node) in [(9, "different"), (42, member["node_id"].as_str().unwrap())] {
            let mut identities = PullRequestIdentities::default();
            identities.observe_head(number, node).unwrap();
            assert!(super::validate_stack(&first, 3, "S", &mut identities).is_err());
        }
    }
}

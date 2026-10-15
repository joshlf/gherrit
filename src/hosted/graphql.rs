//! Graphql.

use super::*;

pub(super) const QUERY: &str = "query($owner:String!,$repo:String!,$head:String!,$after:String){repository(owner:$owner,name:$repo){id nameWithOwner pullRequests(headRefName:$head,states:[OPEN,CLOSED,MERGED],first:100,after:$after){nodes{number id state isDraft headRefName headRefOid baseRefName baseRefOid headRepository{id nameWithOwner} baseRepository{id nameWithOwner} stack{id number size baseRefName} stackEntry{position stack{id}}} pageInfo{hasNextPage endCursor}}}}";

#[derive(Clone, Debug, Deserialize, Serialize, Eq, PartialEq)]
#[serde(rename_all = "camelCase")]
pub(crate) struct RepositoryIdentity {
    pub(super) id: String,
    pub(super) name_with_owner: String,
}

#[derive(Debug, Deserialize, Serialize, Eq, PartialEq)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub(super) enum Lifecycle {
    Open,
    Closed,
    Merged,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct NativeSummary {
    pub(super) id: String,
    pub(super) number: u64,
    pub(super) size: usize,
    pub(super) base_ref_name: String,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct NativeEntry {
    pub(super) position: usize,
    pub(super) stack: NativeIdentity,
}
#[derive(Debug, Deserialize, Serialize)]
pub(super) struct NativeIdentity {
    pub(super) id: String,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct PullRequestFacts {
    pub(super) number: u64,
    pub(super) id: String,
    pub(super) state: Lifecycle,
    pub(super) is_draft: bool,
    pub(super) head_ref_name: String,
    pub(super) head_ref_oid: String,
    pub(super) base_ref_name: String,
    pub(super) base_ref_oid: String,
    pub(super) head_repository: Option<RepositoryIdentity>,
    pub(super) base_repository: RepositoryIdentity,
    pub(super) stack: Option<NativeSummary>,
    pub(super) stack_entry: Option<NativeEntry>,
}

#[derive(Debug, Serialize)]
pub(crate) struct HeadFacts {
    pub(super) repository: RepositoryIdentity,
    pub(super) head: String,
    pub(super) pages: usize,
    pub(super) pull_requests: Vec<PullRequestFacts>,
    // Complete exhaustion means no matching PRs is observed absence. Failure
    // returns no partial HeadFacts and therefore cannot imply absence.
}

pub(super) fn oid(value: &str) -> bool {
    matches!(value.len(), 40 | 64)
        && !value.bytes().all(|byte| byte == b'0')
        && value.bytes().all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
}

pub(super) fn data(response: Value) -> Result<Value> {
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
pub(super) struct PullRequestIdentities {
    pub(super) numbers: HashMap<u64, String>,
    pub(super) nodes: HashSet<String>,
    pub(super) native_numbers: HashSet<u64>,
    pub(super) native_ids: HashSet<u64>,
}

impl PullRequestIdentities {
    pub(super) fn observe_head(&mut self, number: u64, node: &str) -> Result<()> {
        if self.numbers.contains_key(&number) || !self.nodes.insert(node.to_owned()) {
            bail!("Repeated factual pull request identity");
        }
        self.numbers.insert(number, node.to_owned());
        Ok(())
    }

    pub(super) fn observe_member(&mut self, number: u64, node: &str, rest_id: u64) -> Result<()> {
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
pub(super) struct NativeStackIdentities {
    pub(super) by_number: BTreeMap<u64, String>,
    pub(super) nodes: HashSet<String>,
    pub(super) rest_ids: HashSet<u64>,
}

impl NativeStackIdentities {
    pub(super) fn register(&mut self, stack: &NativeSummary) -> Result<()> {
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

pub(super) fn read_head(
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
                || row.base_ref_name.is_empty()
                || row.base_repository != observed
                || row
                    .head_repository
                    .as_ref()
                    .is_some_and(|head| head.id.is_empty() || head.name_with_owner.is_empty())
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
                        && !stack.base_ref_name.is_empty()
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

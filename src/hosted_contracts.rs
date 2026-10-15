//! Explicit disposable-target contract probes, outside the hermetic suite.
use std::{
    fs::{File, OpenOptions},
    io::Write as _,
    path::PathBuf,
    process::ExitCode,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use clap::{Parser, ValueEnum};
use eyre::{Result, bail, eyre};
use serde_json::{Value, json};

use crate::{json::UniqueJson, subprocess, util};

const MAX_REQUESTS: usize = 40;
#[derive(Clone, Copy, Debug, ValueEnum)]
enum Scenario {
    DraftLifecycle,
    NativeRestack,
    WorkflowDelivery,
    All,
}

#[derive(Debug, Parser)]
#[command(
    about = "Probe GitHub contracts in an explicitly authorized disposable repository. Native restack/all merges the new bottom PR into its default branch."
)]
struct Options {
    #[arg(long)]
    repository: String,
    /// Authorize creation of owned refs/PRs/stacks and selected probe mutations.
    #[arg(long, required = true)]
    allow_mutations: bool,
    #[arg(long, value_enum)]
    scenario: Scenario,
    /// New JSONL journal file; never overwrites or resumes a prior run.
    #[arg(long)]
    output: PathBuf,
}

fn target(value: &str) -> Result<()> {
    let parts: Vec<_> = value.split('/').collect();
    if parts.len() != 2
        || parts.iter().any(|part| {
            part.is_empty()
                || part.len() > 100
                || !part.bytes().all(|byte| byte.is_ascii_alphanumeric() || b"-_.".contains(&byte))
        })
    {
        bail!("Specify an exact owner/repository target");
    }
    Ok(())
}

fn redact_journal(value: &mut Value, credential: &str) {
    match value {
        Value::Object(fields) => {
            for (key, value) in fields {
                if [
                    "temp_clone_token",
                    "token",
                    "access_token",
                    "refresh_token",
                    "id_token",
                    "client_secret",
                    "authorization",
                    "password",
                ]
                .iter()
                .any(|sensitive| key.eq_ignore_ascii_case(sensitive))
                {
                    *value = json!("<redacted>");
                } else {
                    redact_journal(value, credential);
                }
            }
        }
        Value::Array(values) => {
            for value in values {
                redact_journal(value, credential);
            }
        }
        Value::String(value) if !credential.is_empty() => {
            *value = value.replace(credential, "<redacted>")
        }
        _ => {}
    }
}

struct Api {
    token: String,
    journal: File,
    deadline: Instant,
    requests: usize,
    #[cfg(test)]
    test_now: std::cell::RefCell<std::collections::VecDeque<Instant>>,
}
impl Api {
    fn record(&mut self, mut event: Value) -> Result<()> {
        redact_journal(&mut event, &self.token);
        serde_json::to_writer(&mut self.journal, &event)?;
        self.journal.write_all(b"\n")?;
        self.journal.flush()?;
        self.journal.sync_all()?;
        Ok(())
    }
    fn finish_run(&mut self, result: Result<()>) -> Result<()> {
        if let Err(error) = &result {
            // Receipt validation occurs after a response has been persisted.
            // Its rejection must survive stderr loss, without implying undo.
            self.record(json!({"event":"failure","stage":"run","after_sequence":self.requests,
                "outcome":"stopped","reason":error.to_string(),"automatic_retry":false}))?;
        }
        result
    }

    fn remaining_request_time(&self) -> Result<Duration> {
        #[cfg(not(test))]
        let now = Instant::now();
        #[cfg(test)]
        let now = self.test_now.borrow_mut().pop_front().unwrap_or_else(Instant::now);
        self.deadline
            .checked_duration_since(now)
            .filter(|remaining| !remaining.is_zero())
            .map(|remaining| remaining.min(Duration::from_secs(30)))
            .ok_or_else(|| eyre!("Hosted probe exhausted its 180-second deadline"))
    }

    fn next_poll_delay(&self, polls_left: usize) -> Result<Option<Duration>> {
        if polls_left == 0 {
            return Ok(None);
        }
        Ok(Some(self.remaining_request_time()?.min(Duration::from_secs(1))))
    }

    fn call(
        &mut self,
        stage: &str,
        method: &str,
        route: &str,
        request: Option<Value>,
    ) -> Result<Value> {
        if self.requests == MAX_REQUESTS {
            bail!("Hosted probe exhausted its 40-request budget");
        }
        self.remaining_request_time()?;
        self.requests += 1;
        let sequence = self.requests;
        // Persist intent before any effect. A missing receipt never proves no
        // write; failed mutation calls stop the run and are never replayed.
        self.record(json!({"event":"request","sequence":sequence,"stage":stage,"method":method,"route":route,"body":request}))?;
        let mut command = util::cmd(
            "gh",
            [
                "api",
                "--hostname",
                "github.com",
                "--method",
                method,
                route,
                "-H",
                "Accept: application/vnd.github+json",
                "-H",
                "X-GitHub-Api-Version: 2026-03-10",
            ],
        );
        command.env("GH_TOKEN", &self.token).env("GH_PROMPT_DISABLED", "1");
        let input = if let Some(request) = request {
            command.args(["--input", "-"]);
            let mut input = subprocess::RegularFileStdinBuilder::new()?;
            input.write_all(&serde_json::to_vec(&request)?)?;
            Some(input.finish()?)
        } else {
            None
        };
        let remaining = self.remaining_request_time()?;
        let result = subprocess::local_output(&mut command, input, remaining, 4 * 1024 * 1024);
        let response = match result {
            Ok(output) if output.status.success() => {
                UniqueJson::decode(&output.stdout).ok().map(UniqueJson::into_value)
            }
            _ => None,
        };
        match response {
            Some(response) => {
                self.record(
                    json!({"event":"response","sequence":sequence,"stage":stage,"body":response}),
                )?;
                Ok(response)
            }
            None => {
                let outcome = if method == "GET" { "unavailable" } else { "indeterminate" };
                self.record(json!({"event":"failure","sequence":sequence,"stage":stage,"outcome":outcome,"automatic_retry":false}))?;
                bail!(
                    "Hosted {stage} outcome is {outcome}; inspect the journal and remote state before another run"
                );
            }
        }
    }
    fn graphql(&mut self, stage: &str, query: &str, id: &str) -> Result<Value> {
        let response = self.call(
            stage,
            "POST",
            "graphql",
            Some(json!({"query":query,"variables":{"id":id}})),
        )?;
        if response
            .get("errors")
            .is_some_and(|errors| errors.as_array().is_none_or(|errors| !errors.is_empty()))
            || !response["data"].is_object()
        {
            bail!("Hosted lifecycle acknowledgement is indeterminate");
        }
        Ok(response)
    }
}

fn field(value: &Value, key: &str) -> Result<String> {
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
struct OwnedReview {
    number: u64,
    node_id: String,
    head: String,
}

impl OwnedReview {
    fn from_receipt(
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
fn owned_stack_number(value: &Value, reviews: &[OwnedReview], base: &str) -> Result<u64> {
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

fn ready(api: &mut Api, review: &OwnedReview) -> Result<()> {
    let id = review.node_id.as_str();
    let response=api.graphql("ready","mutation($id:ID!){markPullRequestReadyForReview(input:{pullRequestId:$id}){pullRequest{id isDraft state}}}",id)?;
    let pr = &response["data"]["markPullRequestReadyForReview"]["pullRequest"];
    if pr["id"] != id || pr["isDraft"] != false || pr["state"] != "OPEN" {
        bail!("Ready acknowledgement is indeterminate");
    }
    Ok(())
}

fn run(options: Options) -> Result<()> {
    if !options.allow_mutations {
        bail!("Explicit --allow-mutations is required");
    }
    target(&options.repository)?;
    let journal = OpenOptions::new().write(true).create_new(true).open(&options.output)?;
    let run = format!(
        "{}-{}",
        SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs(),
        data_encoding::BASE32.encode(&rand::random::<[u8; 10]>()).to_ascii_lowercase()
    );
    let prefix = format!("gherrit-contracts/{run}");
    let refs = [format!("{prefix}/bottom"), format!("{prefix}/top")];
    let mut api = Api {
        token: String::new(),
        journal,
        deadline: Instant::now() + Duration::from_secs(180),
        requests: 0,
        #[cfg(test)]
        test_now: Default::default(),
    };
    api.record(json!({"event":"run","schema_version":1,"repository":options.repository,"run":run,"scenario":format!("{:?}",options.scenario),"owned_refs":refs,"cleanup":"none; retain receipts and inspect created resources"}))?;
    let result = (|| {
        api.token = util::get_github_token()?;
        let root = format!("repos/{}", options.repository);
        let repository = api.call("repository", "GET", &root, None)?;
        let repository_id = repository["id"]
            .as_u64()
            .filter(|id| *id > 0)
            .ok_or_else(|| eyre!("Missing repository identity"))?;
        if !repository["full_name"]
            .as_str()
            .is_some_and(|name| name.eq_ignore_ascii_case(&options.repository))
        {
            bail!("Disposable target identity disagrees with requested repository");
        }
        let default = field(&repository, "default_branch")?;
        let ref_path =
            percent_encoding::utf8_percent_encode(&default, percent_encoding::NON_ALPHANUMERIC)
                .to_string();
        let base =
            api.call("default-ref", "GET", &format!("{root}/git/ref/heads/{ref_path}"), None)?;
        let mut parent = field(&base["object"], "sha")?;
        let commit =
            api.call("default-commit", "GET", &format!("{root}/git/commits/{parent}"), None)?;
        let mut tree = field(&commit["tree"], "sha")?;
        let mut prs = Vec::new();
        for (index, name) in refs.iter().enumerate() {
            let value=api.call("create-tree","POST",&format!("{root}/git/trees"),Some(json!({"base_tree":tree,"tree":[{"path":format!(".gherrit-contracts/{run}-{index}.txt"),"mode":"100644","type":"blob","content":format!("Owned contract run {run}, change {index}\n")}]})))?;
            tree = field(&value, "sha")?;
            let value=api.call("create-commit","POST",&format!("{root}/git/commits"),Some(json!({"message":format!("GHerrit contract {run} change {index}"),"tree":tree,"parents":[parent]})))?;
            parent = field(&value, "sha")?;
            let value = api.call(
                "create-ref",
                "POST",
                &format!("{root}/git/refs"),
                Some(json!({"ref":format!("refs/heads/{name}"),"sha":parent})),
            )?;
            if value["ref"] != format!("refs/heads/{name}") || value["object"]["sha"] != parent {
                bail!("Ref creation acknowledgement is indeterminate");
            }
            let base = if index == 0 { default.as_str() } else { refs[0].as_str() };
            let value=api.call("create-draft","POST",&format!("{root}/pulls"),Some(json!({"head":name,"base":base,"draft":true,"title":format!("GHerrit contract {run} {index}"),"body":format!("Owned disposable contract run {run}. Receipt journal must be retained.")})))?;
            let review =
                OwnedReview::from_receipt(&value, repository_id, name, &parent, base, &prs)?;
            let number = review.number;
            prs.push(review);
            api.record(json!({"event":"owned-pr","number":number,"ref":name,"head":parent}))?;
        }
        let stack = api.call(
            "create-native-stack",
            "POST",
            &format!("{root}/stacks"),
            Some(json!({"pull_requests":[prs[0].number,prs[1].number]})),
        )?;
        let stack_number = owned_stack_number(&stack, &prs, &default)?;
        api.record(json!({"event":"owned-stack","number":stack_number}))?;
        api.call("native-stack", "GET", &format!("{root}/stacks/{stack_number}"), None)?;
        if matches!(options.scenario, Scenario::DraftLifecycle | Scenario::All) {
            ready(&mut api, &prs[0])?;
            let response=api.graphql("draft","mutation($id:ID!){convertPullRequestToDraft(input:{pullRequestId:$id}){pullRequest{id isDraft state}}}",&prs[0].node_id)?;
            let pr = &response["data"]["convertPullRequestToDraft"]["pullRequest"];
            if pr["id"] != prs[0].node_id || pr["isDraft"] != true || pr["state"] != "OPEN" {
                bail!("Draft acknowledgement is indeterminate");
            }
        }
        if matches!(options.scenario, Scenario::NativeRestack | Scenario::All) {
            ready(&mut api, &prs[0])?;
            let response=api.call("async-bottom-merge","PUT",&format!("{root}/pulls/{}/merge-async",prs[0].number),Some(json!({"sha":prs[0].head,"merge_method":"merge","merge_action":"default","bypass_rules":false})))?;
            if let Some(uuid) = merge_receipt_uuid(&response, &prs[0].head, None)? {
                const POLLS: usize = 6;
                for attempt in 0..POLLS {
                    let status = api.call(
                        "merge-status",
                        "GET",
                        &format!("{root}/pulls/{}/merge-async/{uuid}", prs[0].number),
                        None,
                    )?;
                    if merge_receipt_uuid(&status, &prs[0].head, Some(&uuid))?.is_none() {
                        break;
                    }
                    if let Some(delay) = api.next_poll_delay(POLLS - attempt - 1)? {
                        std::thread::sleep(delay);
                    }
                }
            }
            api.call("restacked-top", "GET", &format!("{root}/pulls/{}", prs[1].number), None)?;
            api.call(
                "restacked-native-stack",
                "GET",
                &format!("{root}/stacks/{stack_number}"),
                None,
            )?;
            api.record(json!({"event":"restack-observation","original_top_head":prs[1].head,"interpretation":"Compare recorded head/base/member receipts; pending or queued merge is not proof of restack failure"}))?;
        }
        if matches!(options.scenario, Scenario::WorkflowDelivery | Scenario::All) {
            for review in &prs {
                let head = &review.head;
                let runs = api.call(
                    "workflow-runs",
                    "GET",
                    &format!("{root}/actions/runs?event=pull_request&per_page=100"),
                    None,
                )?;
                let matching = workflow_runs_for_pr(&runs, review.number)?;
                api.record(json!({"event":"workflow-observation","pull_request":review.number,"submitted_head":head,"matching_runs":matching,"visible_count":matching.len(),"basis":"first 100 pull_request runs correlated by PR number; run head_sha may be a synthetic merge commit","interpretation":"No visible matching runs within this bounded observation is not proof of missing event delivery; inspect workflow configuration, permissions, event payloads, pagination, and delays"}))?;
            }
        }
        api.record(json!({"event":"complete","requests":api.requests,"owned_prs":prs.iter().map(|review|review.number).collect::<Vec<_>>(),"owned_stack":stack_number}))?;
        println!("Recorded contract run {run} in {}", options.output.display());
        Ok(())
    })();
    api.finish_run(result)
}

/// A pending receipt authorizes only polling its exact operation, never a
/// claim that the merge or restack completed. Terminal failure stops the run.
fn merge_receipt_uuid(value: &Value, head: &str, polling: Option<&str>) -> Result<Option<String>> {
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

fn workflow_runs_for_pr(runs: &Value, number: u64) -> Result<Vec<Value>> {
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

pub fn main() -> ExitCode {
    match run(Options::parse()) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("{error}");
            ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn journals_redact_nested_clone_tokens_and_the_api_credential() {
        let mut value = json!({"pull_request":{"repository":{"temp_clone_token":"clone-secret"}},"nested":[{"access_token":"access-secret"}],"url":"prefix-api-secret-suffix","number":7});
        redact_journal(&mut value, "api-secret");
        let encoded = value.to_string();
        for secret in ["clone-secret", "access-secret", "api-secret"] {
            assert!(!encoded.contains(secret));
        }
        assert_eq!(value["number"], 7);
    }

    #[test]
    fn workflow_delivery_correlates_prs_independently_of_run_sha() {
        let runs = json!({"workflow_runs":[
            {"id":1,"head_sha":"synthetic-merge","pull_requests":[{"number":7}]},
            {"id":2,"head_sha":"submitted-head","pull_requests":[{"number":8}]}
        ]});
        assert_eq!(workflow_runs_for_pr(&runs, 7).unwrap()[0]["id"], 1);
        assert!(workflow_runs_for_pr(&runs, 9).unwrap().is_empty());
        assert!(workflow_runs_for_pr(&json!({}), 7).is_err());
    }

    #[test]
    fn hosted_mutation_requires_explicit_target_scenario_output_and_authorization() {
        let valid = [
            "contracts",
            "--repository",
            "owner/repo",
            "--allow-mutations",
            "--scenario",
            "all",
            "--output",
            "receipts.jsonl",
        ];
        assert!(Options::try_parse_from(valid).is_ok());
        for omitted in ["--repository", "--allow-mutations", "--scenario", "--output"] {
            let mut args = Vec::new();
            let mut skip = false;
            for arg in valid {
                if skip {
                    skip = false;
                    continue;
                }
                if arg == omitted {
                    skip = omitted != "--allow-mutations";
                    continue;
                }
                args.push(arg);
            }
            assert!(Options::try_parse_from(args).is_err(), "{omitted}");
        }
        for invalid in ["", "repo", "owner/repo/extra", "owner/repo?query", "owner/$(command)"] {
            assert!(target(invalid).is_err());
        }
    }
    #[test]
    fn owned_review_rejects_mismatched_and_reused_receipts() {
        let head = "a".repeat(40);
        let receipt = json!({"number":1,"node_id":"PR_one","head":{"ref":"owned","sha":head,"repo":{"id":9}},"base":{"ref":"main","repo":{"id":9}},"draft":true,"state":"open"});
        let owned = OwnedReview::from_receipt(&receipt, 9, "owned", &head, "main", &[]).unwrap();
        for (pointer, replacement) in [
            ("/head/ref", json!("foreign")),
            ("/head/sha", json!("b".repeat(40))),
            ("/base/ref", json!("other")),
            ("/head/repo/id", json!(10)),
            ("/base/repo/id", json!(10)),
            ("/draft", json!(false)),
            ("/state", json!("closed")),
            ("/number", json!(0)),
            ("/node_id", json!("")),
        ] {
            let mut changed = receipt.clone();
            *changed.pointer_mut(pointer).unwrap() = replacement;
            assert!(
                OwnedReview::from_receipt(&changed, 9, "owned", &head, "main", &[]).is_err(),
                "{pointer}"
            );
        }
        for (number, node_id) in [(1, "PR_two"), (2, "PR_one")] {
            let mut reused = receipt.clone();
            reused["number"] = json!(number);
            reused["node_id"] = json!(node_id);
            assert!(
                OwnedReview::from_receipt(
                    &reused,
                    9,
                    "owned",
                    &head,
                    "main",
                    std::slice::from_ref(&owned)
                )
                .is_err()
            );
        }
    }

    #[test]
    fn exhausted_request_budget_stops_before_any_child_or_effect() {
        let file = tempfile::tempfile().unwrap();
        let mut api = Api {
            token: "must-never-be-logged".to_owned(),
            journal: file,
            deadline: Instant::now() + Duration::from_secs(1),
            requests: MAX_REQUESTS,
            test_now: Default::default(),
        };
        assert!(api.call("fixture", "POST", "must-not-run", Some(json!({}))).is_err());
        assert_eq!(api.journal.metadata().unwrap().len(), 0);
    }
    #[test]
    fn async_merge_receipts_require_a_complete_correlated_pending_operation() {
        let head = "1".repeat(40);
        let pending = json!({"status":"pending","details":{"message":"Pending","uuid":"operation-1","expected_head_sha":head,"merge_method":"merge","merge_action":"default","bypass_rules":false}});
        assert_eq!(
            merge_receipt_uuid(&pending, &head, None).unwrap().as_deref(),
            Some("operation-1")
        );
        assert!(merge_receipt_uuid(&pending, &head, Some("operation-1")).unwrap().is_some());
        assert!(merge_receipt_uuid(&pending, &head, Some("other")).is_err());
        assert!(merge_receipt_uuid(&pending, &"2".repeat(40), None).is_err());
        for key in ["message", "uuid", "expected_head_sha", "merge_method", "merge_action"] {
            let mut missing = pending.clone();
            missing["details"].as_object_mut().unwrap().remove(key);
            assert!(merge_receipt_uuid(&missing, &head, None).is_err(), "{key}");
        }
        for (field, value) in [("merge_method", json!("squash")), ("bypass_rules", json!(true))] {
            let mut wrong = pending.clone();
            wrong["details"][field] = value;
            assert!(merge_receipt_uuid(&wrong, &head, None).is_err());
        }
        for uuid in ["", "../escape", "bad?query"] {
            let mut bad = pending.clone();
            bad["details"]["uuid"] = json!(uuid);
            assert!(merge_receipt_uuid(&bad, &head, None).is_err());
        }
        for valid in [
            json!({"status":"merged","details":{"message":"Merged","sha":head}}),
            json!({"status":"enqueued","details":{"message":"Queued"}}),
        ] {
            assert!(merge_receipt_uuid(&valid, &head, None).unwrap().is_none());
        }
        for bad in [
            json!({}),
            json!({"status":"unknown","details":{"message":"Unknown"}}),
            json!({"status":"failed","details":{"message":"Failed"}}),
            json!({"status":"merged","details":{"message":"Merged"}}),
        ] {
            assert!(merge_receipt_uuid(&bad, &head, None).is_err());
        }
    }
    #[test]
    fn stack_creation_receipt_must_identify_the_exact_owned_order_and_base() {
        let reviews = vec![
            OwnedReview { number: 7, node_id: "P7".to_owned(), head: "1".repeat(40) },
            OwnedReview { number: 8, node_id: "P8".to_owned(), head: "2".repeat(40) },
        ];
        let valid = json!({"number":3,"open":true,"base":{"ref":"main"},"pull_requests":[{"number":7,"node_id":"P7","head":{"sha":reviews[0].head}},{"number":8,"node_id":"P8","head":{"sha":reviews[1].head}}]});
        assert_eq!(owned_stack_number(&valid, &reviews, "main").unwrap(), 3);
        for pointer in [
            "/number",
            "/base/ref",
            "/open",
            "/pull_requests",
            "/pull_requests/0/number",
            "/pull_requests/0/node_id",
            "/pull_requests/0/head/sha",
        ] {
            let mut bad = valid.clone();
            *bad.pointer_mut(pointer).unwrap() = Value::Null;
            assert!(owned_stack_number(&bad, &reviews, "main").is_err(), "{pointer}");
        }
        let mut reversed = valid.clone();
        reversed["pull_requests"].as_array_mut().unwrap().reverse();
        assert!(owned_stack_number(&reversed, &reviews, "main").is_err());
        let mut extra = valid.clone();
        extra["pull_requests"].as_array_mut().unwrap().push(json!({"number":9}));
        assert!(owned_stack_number(&extra, &reviews, "main").is_err());
        assert!(owned_stack_number(&valid, &reviews, "other").is_err());
    }
    #[test]
    fn deadline_is_rechecked_after_synced_file_intent_and_input_preparation() {
        let start = Instant::now();
        let mut api = Api {
            token: String::new(),
            journal: tempfile::tempfile().unwrap(),
            deadline: start + Duration::from_secs(1),
            requests: 0,
            test_now: std::cell::RefCell::new([start, start + Duration::from_secs(2)].into()),
        };
        let error =
            api.call("fixture", "POST", "--help", Some(json!({"body":"prepared"}))).unwrap_err();
        assert!(error.to_string().contains("exhausted its 180-second deadline"), "{error}");
        assert_eq!(api.requests, 1);
        assert!(api.journal.metadata().unwrap().len() > 0);
        assert!(api.test_now.borrow().is_empty());
    }
    #[test]
    fn merge_polling_respects_the_remaining_run_budget_and_final_attempt() {
        let start = Instant::now();
        let api = Api {
            token: String::new(),
            journal: tempfile::tempfile().unwrap(),
            deadline: start + Duration::from_secs(2),
            requests: 0,
            test_now: std::cell::RefCell::new(
                [start, start + Duration::from_millis(1999), start + Duration::from_secs(2)].into(),
            ),
        };
        assert_eq!(api.next_poll_delay(0).unwrap(), None);
        assert_eq!(
            api.test_now.borrow().len(),
            3,
            "the final poll never sleeps or consults the clock"
        );
        assert_eq!(api.next_poll_delay(5).unwrap(), Some(Duration::from_secs(1)));
        assert_eq!(api.next_poll_delay(1).unwrap(), Some(Duration::from_millis(1)));
        assert!(api.next_poll_delay(1).is_err());
    }

    #[test]
    fn terminal_receipt_rejections_are_journaled_and_redacted() {
        use std::io::{Read as _, Seek as _};
        let head = "a".repeat(40);
        let errors = [
            OwnedReview::from_receipt(&json!({}), 9, "owned", &head, "main", &[]).map(|_| ()),
            owned_stack_number(&json!({}), &[], "main").map(|_| ()),
            merge_receipt_uuid(&json!({}), &head, None).map(|_| ()),
            Err(eyre!("contradictory receipt containing api-secret")),
        ];
        for result in errors {
            let reason = result.as_ref().unwrap_err().to_string();
            let mut api = Api {
                token: "api-secret".to_owned(),
                journal: tempfile::tempfile().unwrap(),
                deadline: Instant::now() + Duration::from_secs(1),
                requests: 7,
                test_now: Default::default(),
            };
            api.record(json!({"event":"response","sequence":7,"stage":"create-draft","body":{}}))
                .unwrap();
            assert!(api.finish_run(result).is_err());
            api.journal.rewind().unwrap();
            let mut journal = String::new();
            api.journal.read_to_string(&mut journal).unwrap();
            assert!(!journal.contains("api-secret"));
            let events = journal
                .lines()
                .map(|line| serde_json::from_str::<Value>(line).unwrap())
                .collect::<Vec<_>>();
            assert_eq!(events.len(), 2);
            assert_eq!(events[0]["event"], "response");
            assert_eq!(events[1]["event"], "failure");
            assert_eq!(events[1]["after_sequence"], 7);
            assert_eq!(events[1]["automatic_retry"], false);
            assert_eq!(events[1]["reason"], reason.replace("api-secret", "<redacted>"));
            let before = api.journal.metadata().unwrap().len();
            api.finish_run(Ok(())).unwrap();
            assert_eq!(api.journal.metadata().unwrap().len(), before);
            assert_eq!(api.requests, 7, "finishing never retries a request");
        }
    }
}

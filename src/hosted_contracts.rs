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

use crate::{pre_push::json::UniqueJson, subprocess, util};

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

struct Api {
    token: String,
    journal: File,
    deadline: Instant,
    requests: usize,
}
impl Api {
    fn record(&mut self, event: Value) -> Result<()> {
        serde_json::to_writer(&mut self.journal, &event)?;
        self.journal.write_all(b"\n")?;
        self.journal.flush()?;
        self.journal.sync_all()?;
        Ok(())
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
        let remaining = self
            .deadline
            .checked_duration_since(Instant::now())
            .ok_or_else(|| eyre!("Hosted probe exhausted its 180-second deadline"))?;
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
        let result = subprocess::local_output(
            &mut command,
            input,
            remaining.min(Duration::from_secs(30)),
            4 * 1024 * 1024,
        );
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
    };
    api.record(json!({"event":"run","schema_version":1,"repository":options.repository,"run":run,"scenario":format!("{:?}",options.scenario),"owned_refs":refs,"cleanup":"none; retain receipts and inspect created resources"}))?;
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
    let base = api.call("default-ref", "GET", &format!("{root}/git/ref/heads/{ref_path}"), None)?;
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
        let review = OwnedReview::from_receipt(&value, repository_id, name, &parent, base, &prs)?;
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
    let stack_number = stack["number"]
        .as_u64()
        .filter(|number| *number > 0)
        .ok_or_else(|| eyre!("Native stack acknowledgement is indeterminate"))?;
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
        if let Some(uuid) = response["details"]["uuid"].as_str() {
            if uuid.is_empty()
                || !uuid.bytes().all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
            {
                bail!("Invalid merge receipt identity");
            }
            for _ in 0..6 {
                let status = api.call(
                    "merge-status",
                    "GET",
                    &format!("{root}/pulls/{}/merge-async/{uuid}", prs[0].number),
                    None,
                )?;
                if status["status"] != "pending" {
                    break;
                }
                std::thread::sleep(Duration::from_secs(1));
            }
        }
        api.call("restacked-top", "GET", &format!("{root}/pulls/{}", prs[1].number), None)?;
        api.call("restacked-native-stack", "GET", &format!("{root}/stacks/{stack_number}"), None)?;
        api.record(json!({"event":"restack-observation","original_top_head":prs[1].head,"interpretation":"Compare recorded head/base/member receipts; pending or queued merge is not proof of restack failure"}))?;
    }
    if matches!(options.scenario, Scenario::WorkflowDelivery | Scenario::All) {
        for review in &prs {
            let head = &review.head;
            let runs = api.call(
                "workflow-runs",
                "GET",
                &format!("{root}/actions/runs?head_sha={head}&per_page=10"),
                None,
            )?;
            api.record(json!({"event":"workflow-observation","submitted_head":head,"visible_count":runs["workflow_runs"].as_array().map(Vec::len),"interpretation":"No visible matching runs within this observation is not proof of missing event delivery; inspect workflow configuration, permissions, event payloads, and delays"}))?;
        }
    }
    api.record(json!({"event":"complete","requests":api.requests,"owned_prs":prs.iter().map(|review|review.number).collect::<Vec<_>>(),"owned_stack":stack_number}))?;
    println!("Recorded contract run {run} in {}", options.output.display());
    Ok(())
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
        };
        assert!(api.call("fixture", "POST", "must-not-run", Some(json!({}))).is_err());
        assert_eq!(api.journal.metadata().unwrap().len(), 0);
    }
}

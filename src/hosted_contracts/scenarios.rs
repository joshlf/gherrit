//! Scenarios.

use super::*;

pub(super) fn ready(api: &mut Api, review: &OwnedReview) -> Result<()> {
    let id = review.node_id.as_str();
    let response=api.graphql("ready","mutation($id:ID!){markPullRequestReadyForReview(input:{pullRequestId:$id}){pullRequest{id isDraft state}}}",id)?;
    let pr = &response["data"]["markPullRequestReadyForReview"]["pullRequest"];
    if pr["id"] != id || pr["isDraft"] != false || pr["state"] != "OPEN" {
        bail!("Ready acknowledgement is indeterminate");
    }
    Ok(())
}

pub(super) fn run(options: Options) -> Result<()> {
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

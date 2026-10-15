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

mod api;
use api::*;
mod journal;
#[cfg(test)]
use journal::*;
mod receipts;
use receipts::*;
mod scenarios;
use scenarios::*;

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

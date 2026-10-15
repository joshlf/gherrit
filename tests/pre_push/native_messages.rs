//! Literal whole-message witnesses across the native hook and collector.
use std::collections::BTreeMap;

use predicates::prelude::*;
use serde::Deserialize;

#[derive(Deserialize)]
struct NativeCase {
    name: String,
    message: String,
    config: BTreeMap<String, String>,
    cleanup: Option<String>,
    hook_preserves: bool,
    publication: Publication,
    committed: String,
}

#[derive(Deserialize)]
#[serde(untagged)]
enum Publication {
    Outcome(String),
    ByNativeTrailers(BTreeMap<String, String>),
}

#[test]
fn native_message_and_configuration_corpus() {
    let cases: Vec<NativeCase> =
        serde_json::from_str(include_str!("../fixtures/native-message-cases.json")).unwrap();
    for case in cases {
        let ctx = testutil::test_context!().with_initial_commit().build();
        ctx.checkout_managed_private("native-message");
        for (key, value) in &case.config {
            ctx.set_config(key, Some(value));
        }
        let hook_file = ctx.repo_path.join("hook-message");
        std::fs::write(&hook_file, &case.message).unwrap();
        ctx.hook_cmd("commit-msg").arg(&hook_file).assert().success();
        assert_eq!(
            std::fs::read(&hook_file).unwrap() == case.message.as_bytes(),
            case.hook_preserves,
            "{}: hook preservation",
            case.name
        );

        // Publication classifies the original message after native commit
        // cleanup, independently of any identity the hook would insert.
        let original = ctx.repo_path.join("original-message");
        std::fs::write(&original, &case.message).unwrap();
        let mut commit = ctx.git_cmd();
        commit.args(["commit", "--allow-empty", "--no-verify", "-F"]).arg(&original);
        if let Some(cleanup) = &case.cleanup {
            commit.arg(format!("--cleanup={cleanup}"));
        }
        commit.assert().success();
        let raw = ctx.git_cmd().args(["cat-file", "commit", "HEAD"]).output().unwrap();
        assert!(raw.status.success());
        let start = raw.stdout.windows(2).position(|bytes| bytes == b"\n\n").unwrap() + 2;
        assert_eq!(
            &raw.stdout[start..],
            case.committed.as_bytes(),
            "{}: native cleanup",
            case.name
        );
        let outcome = match &case.publication {
            Publication::Outcome(outcome) => outcome,
            Publication::ByNativeTrailers(outcomes) => {
                // Git versions differ in how configured separators are
                // formatted. Each literal native output owns its expected
                // admission result; an unfamiliar output must fail the test.
                let output = ctx
                    .git_cmd()
                    .args(["log", "-1", "--format=%(trailers:only,unfold)"])
                    .output()
                    .unwrap();
                assert!(output.status.success());
                let trailers = String::from_utf8(output.stdout).unwrap();
                outcomes.get(&trailers).unwrap_or_else(|| {
                    panic!("{}: unexpected native trailers: {trailers:?}", case.name)
                })
            }
        };
        let diagnostic = match outcome.as_str() {
            // This test driver has no API endpoint. Reaching that guard means
            // a nonempty stack passed committed-message collection.
            "ok" => "cannot sync PRs without a configured GitHub endpoint",
            "missing" => "missing gherrit-pr-id trailer",
            "multiple" => "multiple gherrit-pr-id trailers",
            "invalid" => "invalid gherrit-pr-id trailer",
            other => panic!("unknown corpus outcome: {other}"),
        };
        ctx.hook_cmd("pre-push").assert().code(1).stderr(predicate::str::contains(diagnostic));
    }
}

use predicates::prelude::*;

fn stack_with_raw_commit_message(message: &str) -> testutil::TestContext {
    let ctx = testutil::test_context!()
        .with_remote()
        .with_initial_commit()
        .with_mock_github()
        .with_git_interceptor()
        .build();
    ctx.checkout_managed_private("invalid-id");
    ctx.run_git(&["commit", "--allow-empty", "--no-verify", "--cleanup=verbatim", "-m", message]);
    ctx
}

fn assert_identity_failure_before_external_io(ctx: testutil::TestContext, diagnostic: &str) {
    ctx.hook_cmd("pre-push").assert().failure().stderr(predicate::str::contains(diagnostic));

    assert_eq!(ctx.github().http_arrivals(), 0);
    assert!(ctx.github().requests().is_empty());
    assert!(ctx.recorded_pushes().is_empty());
}

#[test]
fn test_empty_stack_id_fails_before_external_io() {
    let ctx = stack_with_raw_commit_message("Work\n\ngherrit-pr-id: ");

    assert_identity_failure_before_external_io(ctx, "missing gherrit-pr-id trailer");
}

#[test]
fn test_multiple_stack_ids_fail_before_external_io() {
    let ctx = stack_with_raw_commit_message("Work\n\ngherrit-pr-id: Gone\ngherrit-pr-id: Gtwo");

    assert_identity_failure_before_external_io(ctx, "multiple gherrit-pr-id trailers");
}

#[test]
fn test_body_lookalike_is_not_a_stack_id() {
    let ctx = stack_with_raw_commit_message(
        "Work\n\ngherrit-pr-id: Gexample\n\nThis final paragraph is not a trailer.",
    );

    assert_identity_failure_before_external_io(ctx, "missing gherrit-pr-id trailer");
}

#[test]
fn test_continued_stack_id_fails_before_external_io() {
    let ctx = stack_with_raw_commit_message("Work\n\ngherrit-pr-id: Gone\n continuation");

    assert_identity_failure_before_external_io(ctx, "invalid gherrit-pr-id trailer");
}

#[test]
fn test_empty_and_valid_stack_ids_are_multiple() {
    let ctx = stack_with_raw_commit_message("Work\n\ngherrit-pr-id: \ngherrit-pr-id: Gvalid");

    assert_identity_failure_before_external_io(ctx, "multiple gherrit-pr-id trailers");
}

#[test]
fn test_duplicate_stack_ids_fail_before_external_io() {
    let ctx = testutil::test_context!()
        .with_remote()
        .with_initial_commit()
        .with_mock_github()
        .with_git_interceptor()
        .build();
    ctx.checkout_managed_private("duplicate-ids");
    ctx.commit_with_explicit_gherrit_id("First", "Gduplicate");
    ctx.commit_with_explicit_gherrit_id("Second", "Gduplicate");

    ctx.hook_cmd("pre-push")
        .assert()
        .failure()
        .stderr(predicate::str::contains("multiple commits with gherrit-pr-id 'Gduplicate'"));

    assert_eq!(ctx.github().http_arrivals(), 0);
    assert!(ctx.github().requests().is_empty());
    assert!(ctx.recorded_pushes().is_empty());
}

#[test]
fn test_unavailable_remote_observation_failure() {
    let ctx = testutil::test_context!().repository("missing", "repo").with_mock_github().build();
    ctx.commit("Init");

    ctx.checkout_managed_private("feature-fail");
    ctx.commit_with_gherrit_id("Work to push");

    // Exercise the production Git adapter against a real unavailable remote.
    ctx.run_git(&["remote", "add", "broken-remote", "missing/repo.git"]);
    ctx.run_git(&["config", "gherrit.remote", "broken-remote"]);

    testutil::assert_failure_snapshot!(
        ctx,
        ctx.hook_cmd("pre-push"),
        "pre_push_failure_broken_remote"
    );
}

#[test]
fn test_pre_push_edit_failure() {
    let ctx =
        testutil::test_context!().with_remote().with_initial_commit().with_mock_github().build();

    // Setup: Create PR first
    ctx.checkout_managed_private("feature-edit-fail");
    ctx.commit_with_gherrit_id("Initial Work");
    // Initial push creates PR
    ctx.gherrit_cmd().args(["hook", "pre-push"]).assert().success();

    // Amend commit to trigger update (edit)
    ctx.amend_with_message("Initial Work (Updated)");

    // Run hook with failure injection
    ctx.inject_failure(testutil::FailureKind::UpdatePr);

    ctx.gherrit_cmd()
        .args(["hook", "pre-push"])
        .assert()
        .failure()
        .stderr(predicates::str::contains("Injected UpdatePr failure"));
    ctx.assert_failure_consumed();
    let requests = ctx.github().requests();
    assert_eq!(requests.last(), Some(&vec![testutil::GraphQlOperation::UpdatePr]));
    let prs = ctx.github().pull_requests();
    assert_eq!(prs.len(), 1);
    assert_eq!(prs[0].title.as_deref(), Some("Initial Work"));

    let gherrit_id = ctx.gherrit_id("HEAD").unwrap();
    let remote_ref = format!("refs/heads/{gherrit_id}");
    assert_eq!(ctx.remote_ref_oid(&remote_ref).as_deref(), Some(ctx.head_oid().as_str()));
}

#[test]
fn test_pre_push_ls_remote_failure() {
    let ctx = testutil::test_context!()
        .with_remote()
        .with_initial_commit()
        .with_mock_github()
        .with_git_interceptor()
        .build();
    // Manage branch
    ctx.checkout_managed_private("feature-ls-remote-fail");
    ctx.commit_with_gherrit_id("Work");

    let refs_before = ctx.remote_refs("refs");
    ctx.expect_git_failure(testutil::GitOperation::LsRemote);
    testutil::assert_failure_snapshot!(
        ctx,
        ctx.hook_cmd("pre-push"),
        "ls_remote_observation_failure"
    );

    ctx.assert_failure_consumed();
    assert_eq!(ctx.remote_refs("refs"), refs_before);
    assert!(ctx.recorded_pushes().is_empty());
    assert!(ctx.github().pull_requests().is_empty());
    assert_eq!(ctx.github().requests(), vec![vec![testutil::GraphQlOperation::Query]]);
}

#[test]
fn test_pre_push_pr_list_failure() {
    let ctx = testutil::test_context!()
        .with_remote()
        .with_initial_commit()
        .with_mock_github()
        .with_git_interceptor()
        .build();
    ctx.checkout_managed_private("feature-pr-list-fail");
    ctx.commit_with_gherrit_id("Work");

    // Trigger hook
    ctx.inject_failure(testutil::FailureKind::GraphQl);

    ctx.gherrit_cmd()
        .args(["hook", "pre-push"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("Injected GraphQl failure"));
    ctx.assert_failure_consumed();
    assert_eq!(ctx.github().requests(), vec![vec![testutil::GraphQlOperation::Query]]);
    assert!(ctx.github().pull_requests().is_empty());
    assert!(ctx.recorded_pushes().is_empty());
}

#[test]
fn test_pre_push_pr_create_failure() {
    let ctx = testutil::test_context!()
        .with_remote()
        .with_initial_commit()
        .with_mock_github()
        .with_git_interceptor()
        .build();
    ctx.checkout_managed_private("feature-pr-create-fail");
    ctx.commit_with_gherrit_id("Work");

    // Trigger hook
    ctx.inject_failure(testutil::FailureKind::CreatePr);

    ctx.gherrit_cmd()
        .args(["hook", "pre-push"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("Injected CreatePr failure"));
    ctx.assert_failure_consumed();
    let requests = ctx.github().requests();
    assert_eq!(requests.last(), Some(&vec![testutil::GraphQlOperation::CreatePr]));
    assert!(ctx.github().pull_requests().is_empty());
    assert_eq!(ctx.recorded_pushes().iter().filter(|push| push.succeeded()).count(), 1);
}

#[test]
fn non_utf8_committed_body_is_an_error_before_publication() {
    let ctx = stack_with_raw_commit_message("Valid\n\ngherrit-pr-id: Gvalid");
    let parent = ctx.head_oid();
    let tree = ctx.git_cmd().args(["show", "-s", "--format=%T", "HEAD"]).output().unwrap();
    let tree = String::from_utf8(tree.stdout).unwrap();
    let mut raw = format!("tree {}\nparent {parent}\nauthor Fixture <fixture@example.com> 0 +0000\ncommitter Fixture <fixture@example.com> 0 +0000\n\n", tree.trim()).into_bytes();
    raw.extend_from_slice(b"Subject\n\nInvalid body: \xff\n\ngherrit-pr-id: GinvalidBody\n");
    let path = ctx.repo_path.join("raw-commit");
    std::fs::write(&path, raw).unwrap();
    let output =
        ctx.git_cmd().args(["hash-object", "-w", "-t", "commit"]).arg(&path).output().unwrap();
    assert!(output.status.success());
    let oid = String::from_utf8(output.stdout).unwrap();
    ctx.run_git(&["update-ref", "HEAD", oid.trim()]);
    ctx.hook_cmd("pre-push")
        .assert()
        .code(1)
        .stderr(predicate::str::contains(format!("Commit {} body is not UTF-8", oid.trim())));
    assert_eq!(ctx.github().http_arrivals(), 0);
    assert!(ctx.github().requests().is_empty());
    assert!(ctx.recorded_pushes().is_empty());
}

#[test]
fn malformed_graphql_is_recorded_before_schema_decoding() {
    let ctx = testutil::test_context!().with_mock_github().build();
    let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
    let _entered = runtime.enter();
    let client = octocrab::Octocrab::builder()
        .personal_token("fixture-token".to_string())
        .base_uri(ctx.mock_server_url())
        .unwrap()
        .build()
        .unwrap();
    let response: Result<serde_json::Value, _> =
        runtime.block_on(client.graphql(&serde_json::json!({
            "query": "query { fieldThatDoesNotExist }"
        })));
    assert!(response.is_err());
    assert_eq!(ctx.github().http_arrivals(), 1);
    assert!(ctx.github().requests().is_empty());
}

#[test]
fn invalid_json_http_arrives_before_extractor_rejection() {
    use std::{
        io::{Read as _, Write as _},
        time::Duration,
    };
    let ctx = testutil::test_context!().with_mock_github().build();
    let address = ctx.mock_server_url().strip_prefix("http://").unwrap().parse().unwrap();
    let mut socket =
        std::net::TcpStream::connect_timeout(&address, Duration::from_secs(5)).unwrap();
    socket.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    socket.set_write_timeout(Some(Duration::from_secs(5))).unwrap();
    socket.write_all(b"POST /graphql HTTP/1.1\r\nHost: localhost\r\nContent-Type: application/json\r\nContent-Length: 1\r\nConnection: close\r\n\r\n{").unwrap();
    let mut response = String::new();
    socket.read_to_string(&mut response).unwrap();
    assert!(response.starts_with("HTTP/1.1 400"), "{response}");
    assert_eq!(ctx.github().http_arrivals(), 1);
    assert!(ctx.github().requests().is_empty());
}

#[test]
fn unreadable_version_ref_stops_before_publication() {
    let ctx = stack_with_raw_commit_message("Work\n\ngherrit-pr-id: GversionRead");
    let path = ctx.repo_path.join(".git/refs/tags/gherrit/broken");
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, "not an object ID\n").unwrap();
    ctx.hook_cmd("pre-push").assert().failure().stderr(predicate::str::contains(
        "Failed to read local version refs; no publication batch was pushed",
    ));
    assert!(ctx.recorded_pushes().is_empty());
    assert!(ctx.github().pull_requests().is_empty());
}

#[test]
fn exhausted_version_counter_stops_before_publication() {
    let ctx = stack_with_raw_commit_message("Work\n\ngherrit-pr-id: GversionMax");
    ctx.run_git(&["update-ref", &format!("refs/tags/gherrit/GversionMax/v{}", usize::MAX), "HEAD"]);
    ctx.hook_cmd("pre-push").assert().failure().stderr(predicate::str::contains(
        "Local version counter exhausted for 'GversionMax'; no publication batch was pushed",
    ));
    assert!(ctx.recorded_pushes().is_empty());
    assert!(ctx.github().pull_requests().is_empty());
}

#[test]
fn local_version_write_failure_reports_applied_remote_batch() {
    let ctx = stack_with_raw_commit_message("Work\n\ngherrit-pr-id: GversionLock");
    let path = ctx.repo_path.join(".git/refs/tags/gherrit/GversionLock/v1.lock");
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, "fixture lock").unwrap();
    ctx.hook_cmd("pre-push")
        .assert()
        .failure()
        .stderr(predicate::str::contains("Remote publication batch succeeded"))
        .stderr(predicate::str::contains("Remote changes are already applied"));
    let tip = ctx.head_oid();
    assert_eq!(ctx.remote_ref_oid("refs/heads/GversionLock").as_deref(), Some(tip.as_str()));
    assert_eq!(
        ctx.remote_ref_oid("refs/tags/gherrit/GversionLock/v1").as_deref(),
        Some(tip.as_str())
    );
    assert!(
        !ctx.git_cmd()
            .args(["show-ref", "--verify", "refs/tags/gherrit/GversionLock/v1"])
            .output()
            .unwrap()
            .status
            .success()
    );
    assert_eq!(ctx.recorded_pushes().iter().filter(|push| push.succeeded()).count(), 1);
    assert!(ctx.github().pull_requests().is_empty());
}

#[test]
fn invalid_visibility_config_fails_before_external_io() {
    use std::io::Write as _;
    let ctx = stack_with_raw_commit_message("Work\n\ngherrit-pr-id: Gvisibility");
    std::fs::OpenOptions::new()
        .append(true)
        .open(ctx.repo_path.join(".git/config"))
        .unwrap()
        .write_all(b"\n[branch \"invalid-id\"]\n pushRemote = \xff\n")
        .unwrap();
    ctx.hook_cmd("pre-push")
        .assert()
        .failure()
        .stderr(predicate::str::contains("Failed to read stack visibility"));
    assert_eq!(ctx.github().http_arrivals(), 0);
    assert!(ctx.recorded_pushes().is_empty());
    assert!(ctx.github().pull_requests().is_empty());
}

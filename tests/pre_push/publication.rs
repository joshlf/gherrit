use std::{fs, path::Path};

fn installed_git_version(ctx: &testutil::TestContext) -> (u64, u64) {
    let output = ctx.git_cmd().arg("--version").assert().success().get_output().stdout.clone();
    let version =
        std::str::from_utf8(&output).unwrap().trim().strip_prefix("git version ").unwrap();
    let mut components = version.split('.');
    (components.next().unwrap().parse().unwrap(), components.next().unwrap().parse().unwrap())
}

fn locally_stored_objects(ctx: &testutil::TestContext, repository: &Path) -> Vec<String> {
    let output = ctx
        .git_cmd()
        .current_dir(repository)
        .args(["cat-file", "--batch-all-objects", "--batch-check=%(objectname)"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    String::from_utf8(output).unwrap().lines().map(ToOwned::to_owned).collect()
}

fn configured_remote_url(ctx: &testutil::TestContext, remote: &str) -> String {
    let output = ctx
        .git_cmd()
        .args(["remote", "get-url", remote])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    String::from_utf8(output).unwrap().trim_end().to_owned()
}

fn bare_ref_oid(ctx: &testutil::TestContext, repository: &Path, ref_name: &str) -> Option<String> {
    let git_dir = format!("--git-dir={}", repository.display());
    let output = ctx
        .git_cmd()
        .args([git_dir.as_str(), "rev-parse", "--verify", "--quiet", ref_name])
        .output()
        .expect("failed to inspect alternate bare repository");
    match output.status.code() {
        Some(0) => Some(String::from_utf8(output.stdout).unwrap().trim().to_owned()),
        Some(1) => None,
        code => panic!("git rev-parse failed with exit code {code:?}"),
    }
}

fn remote_annotated_tag(ctx: &testutil::TestContext, ref_name: &str) -> String {
    let bytes = ctx
        .remote_git_cmd()
        .args(["cat-file", "tag", ref_name])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    String::from_utf8(bytes).unwrap()
}

fn assert_remote_pr_marker(
    ctx: &testutil::TestContext,
    id: &str,
    v1: &str,
    pull_request_number: usize,
) {
    let marker_ref = format!("refs/tags/gherrit/{id}/pr");
    assert_eq!(
        ctx.remote_ref_oid(&format!("{marker_ref}^{{}}")).as_deref(),
        Some(v1),
        "the marker must peel exactly to v1"
    );
    assert_eq!(
        remote_annotated_tag(ctx, &marker_ref),
        format!(
            "object {v1}\ntype commit\ntag gherrit/{id}/pr\ntagger GHerrit <gherrit@invalid> 0 +0000\n\ngherrit-canonical-pr-v1 {pull_request_number}\n"
        ),
        "the marker must use the one canonical typed encoding"
    );
}

fn github_operation_count(
    ctx: &testutil::TestContext,
    operation: testutil::GraphQlOperation,
) -> usize {
    ctx.github().requests().into_iter().flatten().filter(|observed| *observed == operation).count()
}

#[derive(Debug, PartialEq, Eq)]
struct ConvergedAttemptEvidence {
    exit_code: Option<i32>,
    stdout: Vec<u8>,
    stderr: Vec<u8>,
    graphql_documents: Vec<String>,
    graphql_operations: Vec<Vec<testutil::GraphQlOperation>>,
    git_operations: Vec<testutil::GitOperation>,
    pushes: Vec<testutil::PushRecord>,
    pull_requests: Vec<testutil::PullRequestSnapshot>,
    remote_refs: Vec<(String, Option<String>)>,
}

fn run_converged_attempt(ctx: &testutil::TestContext, id: &str) -> ConvergedAttemptEvidence {
    let document_offset = ctx.github().request_documents().len();
    let request_offset = ctx.github().requests().len();
    let operation_offset = ctx.recorded_git_operations().len();
    let push_offset = ctx.recorded_pushes().len();

    let output = ctx.hook_cmd("pre-push").output().expect("publication attempt must finish");
    assert!(
        output.status.success(),
        "publication attempt failed:\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    let relevant_refs = [
        format!("refs/heads/{id}"),
        format!("refs/heads/gherrit-bases/{id}"),
        format!("refs/tags/gherrit/{id}/v1"),
        format!("refs/tags/gherrit/{id}/pr"),
    ];
    ConvergedAttemptEvidence {
        exit_code: output.status.code(),
        stdout: output.stdout,
        stderr: output.stderr,
        graphql_documents: ctx.github().request_documents()[document_offset..].to_vec(),
        graphql_operations: ctx.github().requests()[request_offset..].to_vec(),
        git_operations: ctx.recorded_git_operations()[operation_offset..].to_vec(),
        pushes: ctx.recorded_pushes()[push_offset..].to_vec(),
        pull_requests: ctx
            .github()
            .pull_requests()
            .into_iter()
            .filter(|pull_request| pull_request.head == id)
            .collect(),
        remote_refs: relevant_refs
            .into_iter()
            .map(|reference| {
                let oid = ctx.remote_ref_oid(&reference);
                (reference, oid)
            })
            .collect(),
    }
}

#[test]
fn exact_git_and_local_github_observations_start_concurrently() {
    const ID: &str = "Gconcurrentobservation";
    let ctx = testutil::test_context!()
        .with_remote()
        .with_initial_commit()
        .with_observation_overlap(ID, "main")
        .build();
    ctx.checkout_managed_private("concurrent-observation");
    ctx.commit_with_explicit_gherrit_id("Observe both bounded authorities", ID);

    // Each fake transport waits in the same two-party rendezvous before
    // entering mutable fake state. Awaiting either observation before
    // starting the other turns this success into a bounded process failure.
    let output = ctx.hook_cmd("pre-push").output().unwrap();

    assert!(
        output.status.success(),
        "publication failed after observation rendezvous:\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    ctx.observation_overlap().assert_consumed();
    let requests = ctx.github().requests();
    assert_eq!(requests.first(), Some(&vec![testutil::GraphQlOperation::Query]));
    assert_eq!(
        requests
            .iter()
            .flatten()
            .filter(|operation| **operation == testutil::GraphQlOperation::Query)
            .count(),
        1,
        "the publication attempt must make one bounded GitHub observation"
    );
    assert_eq!(
        ctx.recorded_git_operations()
            .into_iter()
            .filter(|operation| *operation == testutil::GitOperation::LsRemote)
            .count(),
        2,
        "one initial and one exact Git observation must complete"
    );
}

#[test]
fn unrequested_open_pull_requests_do_not_expand_publication_work() {
    const ID: &str = "Gboundedpopulation";
    const UNREQUESTED_PULL_REQUESTS: usize = 512;

    let ctx = testutil::test_context!()
        .with_remote()
        .with_initial_commit()
        .with_mock_github()
        .with_git_interceptor()
        .build();
    ctx.checkout_managed_private("bounded-population");
    ctx.commit_with_explicit_gherrit_id("Keep observation bounded", ID);
    ctx.hook_cmd("pre-push").assert().success();

    let established = ctx
        .github()
        .pull_requests()
        .into_iter()
        .find(|pull_request| pull_request.head == ID)
        .expect("setup publication must establish the marker-bound pull request");
    assert_remote_pr_marker(&ctx, ID, &ctx.head_oid(), established.number);

    let baseline = run_converged_attempt(&ctx, ID);
    assert_eq!(baseline.graphql_documents.len(), 1, "one physical GitHub request");
    assert_eq!(
        baseline.graphql_operations,
        [vec![testutil::GraphQlOperation::Query]],
        "a marker-bound current change needs observation but no mutation"
    );
    assert!(baseline.pushes.is_empty(), "a converged attempt must not push");

    for index in 0..UNREQUESTED_PULL_REQUESTS {
        ctx.github().seed_pull_request(testutil::PullRequestSeed::new(
            10_000 + index,
            format!("Unrequested pull request {index}"),
            "Unrelated body",
            format!("Gunrequested{index:04}"),
            "main",
        ));
    }

    let populated = run_converged_attempt(&ctx, ID);
    assert_eq!(
        populated, baseline,
        "unrequested OPEN heads must not alter documents, physical operations, pushes, output, or relevant pull request and ref state"
    );
}

#[test]
fn direct_pre_push_publishes_a_complete_stack() {
    let ctx = testutil::test_context!()
        .with_remote()
        .with_initial_commit()
        .with_mock_github()
        .with_git_interceptor()
        .build();

    ctx.checkout_managed_private("feature-stack");

    ctx.commit_with_gherrit_id("Commit A");
    let commit_a_id = ctx.gherrit_id("HEAD").unwrap();
    let commit_a_oid = ctx.head_oid();

    ctx.commit_with_gherrit_id("Commit B");
    let commit_b_id = ctx.gherrit_id("HEAD").unwrap();
    let commit_b_oid = ctx.head_oid();

    // The direct hidden command isolates publication behavior from the
    // separately tested installed-hook boundary.
    testutil::assert_success_snapshot!(
        ctx,
        ctx.gherrit_cmd().args(["hook", "pre-push"]),
        "full_stack_lifecycle_push"
    );

    // Both the bare Git destination and the GitHub fake hold durable state.
    testutil::assert_pr_snapshot!(ctx, "full_stack_lifecycle_state");
    let pull_requests = ctx.github().pull_requests();

    let pushes = ctx.recorded_pushes();
    assert_eq!(pushes.len(), 2, "first publication requires a tuple and marker push");
    assert!(pushes.iter().all(testutil::PushRecord::succeeded));
    assert_eq!(
        ctx.remote_ref_oid(&format!("refs/heads/{commit_a_id}")).as_deref(),
        Some(commit_a_oid.as_str())
    );
    assert_eq!(
        ctx.remote_ref_oid(&format!("refs/heads/{commit_b_id}")).as_deref(),
        Some(commit_b_oid.as_str())
    );
    for (id, head, base) in [
        (&commit_a_id, &commit_a_oid, ctx.remote_ref_oid("refs/heads/main").unwrap()),
        (&commit_b_id, &commit_b_oid, commit_a_oid.clone()),
    ] {
        assert_eq!(
            ctx.remote_ref_oid(&format!("refs/heads/gherrit-bases/{id}")).as_deref(),
            Some(base.as_str())
        );
        assert_eq!(
            ctx.remote_ref_oid(&format!("refs/tags/gherrit/{id}/v1")).as_deref(),
            Some(head.as_str())
        );
        let pull_request = pull_requests
            .iter()
            .find(|pull_request| pull_request.head == id.as_str())
            .expect("every published change must have one pull request");
        assert!(pull_request.is_draft, "every newly created pull request must remain a draft");
        assert_remote_pr_marker(&ctx, id, head, pull_request.number);
    }
}

#[test]
fn lost_initial_push_receipt_recovers_from_the_durable_tuple() {
    let ctx = testutil::test_context!()
        .with_remote()
        .with_initial_commit()
        .with_mock_github()
        .with_git_interceptor()
        .build();
    ctx.checkout_managed_private("lost-initial-receipt");
    let id = ctx.commit_with_gherrit_id("Recover a durable tuple");
    let v1 = ctx.head_oid();
    let default = ctx.remote_ref_oid("refs/heads/main").unwrap();
    ctx.inject_failure(testutil::FailureKind::LosePublicationPushReceipt(
        testutil::PublicationPushStage::Initial,
    ));

    ctx.hook_cmd("pre-push").assert().failure();

    ctx.assert_failure_consumed();
    assert_eq!(ctx.remote_ref_oid(&format!("refs/heads/{id}")).as_deref(), Some(v1.as_str()));
    assert_eq!(
        ctx.remote_ref_oid(&format!("refs/heads/gherrit-bases/{id}")).as_deref(),
        Some(default.as_str())
    );
    assert_eq!(
        ctx.remote_ref_oid(&format!("refs/tags/gherrit/{id}/v1")).as_deref(),
        Some(v1.as_str())
    );
    assert!(ctx.remote_ref_oid(&format!("refs/tags/gherrit/{id}/pr")).is_none());
    assert!(ctx.github().pull_requests().is_empty());
    assert_eq!(ctx.recorded_pushes().len(), 1);
    assert_eq!(github_operation_count(&ctx, testutil::GraphQlOperation::CreatePr), 0);

    ctx.hook_cmd("pre-push").assert().success();

    let pull_requests = ctx.github().pull_requests();
    assert_eq!(pull_requests.len(), 1);
    assert!(pull_requests[0].is_draft);
    assert_eq!(pull_requests[0].base, "main");
    assert_remote_pr_marker(&ctx, &id, &v1, pull_requests[0].number);
    assert_eq!(
        ctx.recorded_pushes().len(),
        2,
        "recovery must acknowledge the existing tuple and push only the marker"
    );
    assert_eq!(github_operation_count(&ctx, testutil::GraphQlOperation::CreatePr), 1);
}

#[test]
fn lost_marker_push_receipt_recovers_by_projecting_the_durable_identity() {
    let ctx = testutil::test_context!()
        .with_remote()
        .with_initial_commit()
        .with_mock_github()
        .with_git_interceptor()
        .build();
    ctx.checkout_managed_private("lost-marker-receipt");
    let id = ctx.commit_with_gherrit_id("Recover a durable marker");
    let v1 = ctx.head_oid();
    ctx.inject_failure(testutil::FailureKind::LosePublicationPushReceipt(
        testutil::PublicationPushStage::Marker,
    ));

    ctx.hook_cmd("pre-push").assert().failure();

    ctx.assert_failure_consumed();
    let provisional = ctx.github().pull_requests();
    assert_eq!(provisional.len(), 1);
    assert!(provisional[0].is_draft);
    assert_eq!(provisional[0].base, format!("gherrit-bases/{id}"));
    assert!(
        provisional[0].body.contains("Stacked PRs enabled by [GHerrit]"),
        "the create barrier must retain a meaningful provisional body"
    );
    assert_remote_pr_marker(&ctx, &id, &v1, provisional[0].number);
    assert_eq!(ctx.recorded_pushes().len(), 2);
    assert_eq!(github_operation_count(&ctx, testutil::GraphQlOperation::CreatePr), 1);
    assert_eq!(github_operation_count(&ctx, testutil::GraphQlOperation::UpdatePr), 0);

    ctx.hook_cmd("pre-push").assert().success();

    let projected = ctx.github().pull_requests();
    assert_eq!(projected.len(), 1);
    assert!(projected[0].is_draft);
    assert_eq!(projected[0].base, "main");
    assert_remote_pr_marker(&ctx, &id, &v1, projected[0].number);
    assert_eq!(
        ctx.recorded_pushes().len(),
        2,
        "recovery must perform only final projection after observing the durable marker"
    );
    assert_eq!(github_operation_count(&ctx, testutil::GraphQlOperation::CreatePr), 1);
    assert_eq!(github_operation_count(&ctx, testutil::GraphQlOperation::UpdatePr), 1);
}

#[test]
fn lost_create_acknowledgement_recovers_without_a_duplicate_pull_request() {
    let ctx = testutil::test_context!()
        .with_remote()
        .with_initial_commit()
        .with_mock_github()
        .with_git_interceptor()
        .build();
    ctx.checkout_managed_private("lost-create-ack");
    let id = ctx.commit_with_gherrit_id("Recover a created pull request");
    let v1 = ctx.head_oid();
    ctx.inject_failure(testutil::FailureKind::CreatePrApplyThenDisconnect);

    ctx.hook_cmd("pre-push")
        .assert()
        .failure()
        .stderr(predicates::str::contains("acknowledgement is indeterminate"));

    ctx.assert_failure_consumed();
    assert_eq!(github_operation_count(&ctx, testutil::GraphQlOperation::CreatePr), 1);
    let pull_requests = ctx.github().pull_requests();
    assert_eq!(pull_requests.len(), 1);
    assert_eq!(pull_requests[0].base, format!("gherrit-bases/{id}"));
    assert!(pull_requests[0].is_draft, "an unmarked provisional pull request must be a draft");
    assert!(
        pull_requests[0].body.contains("Stacked PRs enabled by [GHerrit]"),
        "the create barrier must retain a meaningful provisional body"
    );
    assert!(ctx.remote_ref_oid(&format!("refs/tags/gherrit/{id}/pr")).is_none());
    assert_eq!(ctx.recorded_pushes().len(), 1, "only the tuple barrier ran");

    ctx.hook_cmd("pre-push").assert().success();

    let pull_requests = ctx.github().pull_requests();
    assert_eq!(pull_requests.len(), 1, "stable-key retry must not create a duplicate");
    assert_eq!(pull_requests[0].base, "main");
    assert!(pull_requests[0].is_draft, "final projection must preserve the created draft state");
    assert_remote_pr_marker(&ctx, &id, &v1, pull_requests[0].number);
    assert_eq!(ctx.recorded_pushes().len(), 2, "retry publishes only the missing marker");
    assert_eq!(
        github_operation_count(&ctx, testutil::GraphQlOperation::CreatePr),
        1,
        "neither the indeterminate attempt nor recovery may replay the create mutation"
    );
}

#[test]
fn lost_update_acknowledgement_recovers_without_replaying_the_mutation() {
    let ctx = testutil::test_context!()
        .with_remote()
        .with_initial_commit()
        .with_mock_github()
        .with_git_interceptor()
        .build();
    ctx.checkout_managed_private("lost-update-ack");
    let id = ctx.commit_with_gherrit_id("Recover a projected pull request");
    let v1 = ctx.head_oid();
    ctx.hook_cmd("pre-push").assert().success();
    let initial = ctx.github().pull_requests();
    assert_eq!(initial.len(), 1);
    assert!(initial[0].is_draft);
    assert_remote_pr_marker(&ctx, &id, &v1, initial[0].number);
    let updates_before = github_operation_count(&ctx, testutil::GraphQlOperation::UpdatePr);
    let pushes_before = ctx.recorded_pushes().len();

    ctx.amend();
    ctx.inject_failure(testutil::FailureKind::UpdatePrApplyThenDisconnect);
    ctx.hook_cmd("pre-push")
        .assert()
        .failure()
        .stderr(predicates::str::contains("acknowledgement is indeterminate"));

    ctx.assert_failure_consumed();
    assert_eq!(
        github_operation_count(&ctx, testutil::GraphQlOperation::UpdatePr),
        updates_before + 1
    );
    assert_eq!(ctx.recorded_pushes().len(), pushes_before + 1);
    let projected = ctx.github().pull_requests();
    assert!(projected[0].is_draft);
    assert!(
        projected[0].body.contains("**Latest Update:** v2"),
        "the disconnected update must have applied before its acknowledgement was lost"
    );

    ctx.hook_cmd("pre-push").assert().success();

    assert_eq!(
        github_operation_count(&ctx, testutil::GraphQlOperation::UpdatePr),
        updates_before + 1,
        "recovery must observe the desired projection without replaying its mutation"
    );
    assert_eq!(ctx.recorded_pushes().len(), pushes_before + 1);
    assert_eq!(ctx.github().pull_requests(), projected);
    assert_remote_pr_marker(&ctx, &id, &v1, projected[0].number);
}

#[test]
fn duplicate_repair_recovers_after_losing_the_mixed_projection_response() {
    let ctx = testutil::test_context!()
        .with_remote()
        .with_initial_commit()
        .with_mock_github()
        .with_git_interceptor()
        .build();
    ctx.checkout_managed_private("duplicate-repair");
    let id = ctx.commit_with_gherrit_id("Initial canonical pull request");
    let v1 = ctx.head_oid();
    ctx.hook_cmd("pre-push").assert().success();
    let canonical = ctx.github().pull_requests().pop().unwrap();
    assert!(canonical.is_draft);
    assert_remote_pr_marker(&ctx, &id, &v1, canonical.number);

    ctx.github().seed_pull_request(
        testutil::PullRequestSeed::new(
            12,
            "Delayed duplicate",
            "",
            id.clone(),
            format!("gherrit-bases/{id}"),
        )
        .draft(),
    );
    let duplicate = ctx
        .github()
        .pull_requests()
        .into_iter()
        .find(|pull_request| pull_request.number == 12)
        .unwrap();
    assert!(duplicate.is_draft, "an unmarked duplicate must be a draft");
    assert_eq!(duplicate.base, format!("gherrit-bases/{id}"));

    ctx.amend_with_message("Repair duplicate pull requests");
    let requests_before_repair = ctx.github().requests().len();
    ctx.inject_failure(testutil::FailureKind::ClosePrApplyThenDisconnect);

    ctx.hook_cmd("pre-push")
        .assert()
        .failure()
        .stderr(predicates::str::contains("acknowledgement is indeterminate"));
    ctx.assert_failure_consumed();

    let repaired = ctx.github().pull_requests();
    let repaired_canonical =
        repaired.iter().find(|pull_request| pull_request.number == canonical.number).unwrap();
    let repaired_duplicate =
        repaired.iter().find(|pull_request| pull_request.number == 12).unwrap();
    assert_eq!(repaired_canonical.state, testutil::PullRequestState::Open);
    assert_eq!(repaired_canonical.title, "Repair duplicate pull requests");
    assert!(repaired_canonical.is_draft);
    assert_eq!(repaired_duplicate.state, testutil::PullRequestState::Closed);
    assert_remote_pr_marker(&ctx, &id, &v1, canonical.number);
    assert_eq!(
        &ctx.github().requests()[requests_before_repair..],
        [
            vec![testutil::GraphQlOperation::Query],
            vec![testutil::GraphQlOperation::Query],
            vec![testutil::GraphQlOperation::ClosePr, testutil::GraphQlOperation::UpdatePr],
        ],
        "duplicate closure and canonical update share one ordered projection request"
    );

    let requests_before_retry = ctx.github().requests().len();
    let pushes_before_retry = ctx.recorded_pushes().len();
    ctx.hook_cmd("pre-push").assert().success();

    assert_eq!(ctx.github().pull_requests(), repaired);
    assert_eq!(ctx.recorded_pushes().len(), pushes_before_retry);
    assert_eq!(
        &ctx.github().requests()[requests_before_retry..],
        &[vec![testutil::GraphQlOperation::Query]],
        "OPEN-only recovery must not revisit or replay the closed duplicate"
    );
    assert_remote_pr_marker(&ctx, &id, &v1, canonical.number);
}

#[test]
fn departing_ready_root_becomes_draft_before_the_restack_publishes() {
    let ctx = testutil::test_context!()
        .with_remote()
        .with_initial_commit()
        .with_mock_github()
        .with_git_interceptor()
        .build();
    ctx.checkout_managed_private("draft-conversion");
    let a_id = ctx.commit_with_gherrit_id("Original root");
    let a_v1 = ctx.head_oid();
    ctx.hook_cmd("pre-push").assert().success();

    let a_pr = ctx.github().pull_requests().pop().unwrap();
    assert!(a_pr.is_draft);
    assert_eq!(a_pr.base, "main");
    assert_remote_pr_marker(&ctx, &a_id, &a_v1, a_pr.number);
    ctx.github().set_pull_request_draft(a_pr.number, false);
    assert!(!ctx.github().pull_requests()[0].is_draft);

    ctx.run_git(&["checkout", "-b", "new-root", "main"]);
    let b_id = ctx.commit_with_gherrit_id("New root");
    let b_v1 = ctx.head_oid();
    ctx.run_git(&["checkout", "draft-conversion"]);
    ctx.run_git(&["rebase", "--onto", &b_v1, "main"]);
    let a_v2 = ctx.head_oid();
    assert_ne!(a_v2, a_v1);
    assert_eq!(ctx.gherrit_id("HEAD").unwrap(), a_id);

    let requests_before = ctx.github().requests().len();
    let pushes_before = ctx.recorded_pushes().len();
    ctx.hook_cmd("pre-push").assert().success();

    assert_eq!(
        &ctx.github().requests()[requests_before..],
        [
            vec![testutil::GraphQlOperation::Query],
            vec![testutil::GraphQlOperation::DraftPr],
            vec![testutil::GraphQlOperation::CreatePr],
            vec![testutil::GraphQlOperation::UpdatePr, testutil::GraphQlOperation::UpdatePr],
        ],
        "the ready root must cross the draft barrier before create and final projection"
    );
    assert_eq!(
        ctx.recorded_pushes().len(),
        pushes_before + 2,
        "the restack needs one initial-ref push and one marker push"
    );
    assert_eq!(ctx.remote_ref_oid(&format!("refs/heads/{a_id}")).as_deref(), Some(a_v2.as_str()));
    assert_eq!(
        ctx.remote_ref_oid(&format!("refs/tags/gherrit/{a_id}/v2")).as_deref(),
        Some(a_v2.as_str())
    );

    let pull_requests = ctx.github().pull_requests();
    let a_projected =
        pull_requests.iter().find(|pull_request| pull_request.number == a_pr.number).unwrap();
    let b_projected = pull_requests.iter().find(|pull_request| pull_request.head == b_id).unwrap();
    assert!(a_projected.is_draft);
    assert_eq!(a_projected.base, format!("gherrit-bases/{a_id}"));
    assert!(b_projected.is_draft);
    assert_eq!(b_projected.base, "main");
    assert_remote_pr_marker(&ctx, &a_id, &a_v1, a_projected.number);
    assert_remote_pr_marker(&ctx, &b_id, &b_v1, b_projected.number);
}

#[test]
fn mixed_established_and_new_stack_publishes_only_the_new_change() {
    let ctx = testutil::test_context!()
        .with_remote()
        .with_initial_commit()
        .with_mock_github()
        .with_git_interceptor()
        .build();
    ctx.checkout_managed_private("mixed-established-new");
    let established = ctx.commit_with_gherrit_id("Established root");
    ctx.hook_cmd("pre-push").assert().success();
    let pushes_before = ctx.recorded_pushes().len();
    let requests_before = ctx.github().requests().len();

    let new = ctx.commit_with_gherrit_id("New child");
    ctx.hook_cmd("pre-push").assert().success();

    let requests = ctx.github().requests();
    let second_attempt_requests = &requests[requests_before..];
    assert_eq!(
        &second_attempt_requests[..1],
        &[vec![testutil::GraphQlOperation::Query]],
        "one OPEN query must observe both established and absent heads"
    );
    assert!(
        second_attempt_requests[1..]
            .iter()
            .flatten()
            .all(|operation| *operation != testutil::GraphQlOperation::Query)
    );

    let pushes = ctx.recorded_pushes();
    let second_attempt = &pushes[pushes_before..];
    assert_eq!(second_attempt.len(), 2, "new change needs one tuple and one marker push");
    for namespace in [
        format!("refs/heads/{established}"),
        format!("refs/heads/gherrit-bases/{established}"),
        format!("refs/tags/gherrit/{established}/"),
    ] {
        assert!(
            second_attempt
                .iter()
                .all(|push| push.arguments().iter().all(|argument| !argument.contains(&namespace))),
            "second attempt republished established namespace {namespace}"
        );
    }
    let pull_requests = ctx.github().pull_requests();
    assert_eq!(pull_requests.len(), 2);
    assert_eq!(pull_requests[0].head, established);
    assert_eq!(pull_requests[0].base, "main");
    assert_eq!(pull_requests[1].head, new);
    assert_eq!(pull_requests[1].base, format!("gherrit-bases/{new}"));
}

#[test]
fn cross_repository_pull_request_does_not_claim_the_local_change() {
    let ctx = testutil::test_context!()
        .with_remote()
        .with_initial_commit()
        .with_mock_github()
        .with_git_interceptor()
        .build();
    ctx.checkout_managed_private("foreign-head");
    let id = ctx.commit_with_gherrit_id("Create the local review");
    ctx.github().seed_cross_repository_pull_request(
        testutil::PullRequestSeed::new(41, "Foreign review", "Foreign body", id.clone(), "main"),
        &"1".repeat(40),
        &"2".repeat(40),
    );

    ctx.hook_cmd("pre-push").assert().success();

    let pull_requests = ctx.github().pull_requests();
    assert_eq!(pull_requests.len(), 2);
    assert_eq!(pull_requests[0].title, "Foreign review");
    assert_eq!(pull_requests[0].body, "Foreign body");
    assert_eq!(pull_requests[1].title, "Create the local review");
    assert_eq!(pull_requests[0].head, id);
    assert_eq!(pull_requests[1].head, pull_requests[0].head);
    assert_eq!(github_operation_count(&ctx, testutil::GraphQlOperation::CreatePr), 1);
}

#[test]
fn test_remote_default_ahead_of_local_tracking_ref_fails_before_publication() {
    let ctx = testutil::test_context!()
        .with_remote()
        .with_initial_commit()
        .with_mock_github()
        .with_git_interceptor()
        .build();
    let initial = ctx.head_oid();
    ctx.checkout_managed_private("stale-default");
    ctx.commit_with_gherrit_id("Stack change");

    ctx.run_git(&["checkout", "main"]);
    ctx.commit("Advance remote default");
    ctx.run_git(&["push", "--quiet", "--no-verify", "origin", "main:main"]);
    ctx.run_git(&["update-ref", "refs/heads/main", &initial]);
    ctx.run_git(&["update-ref", "refs/remotes/origin/main", &initial]);
    ctx.run_git(&["checkout", "stale-default"]);
    let fixture_pushes = ctx.recorded_pushes();

    ctx.hook_cmd("pre-push").assert().failure().stderr(predicates::str::contains(
        "Local default branch 'main' does not match the push repository",
    ));
    assert!(ctx.github().requests().is_empty());
    assert_eq!(ctx.recorded_pushes(), fixture_pushes);
}

#[test]
fn test_first_parent_stack_excludes_commits_reachable_only_through_a_merge() {
    let ctx = testutil::test_context!()
        .with_remote()
        .with_initial_commit()
        .with_mock_github()
        .with_git_interceptor()
        .build();
    ctx.checkout_managed_private("first-parent-merge");
    ctx.commit_with_gherrit_id("Stack change");
    let stack_id = ctx.gherrit_id("HEAD").unwrap();

    ctx.run_git(&["checkout", "-b", "side", "main"]);
    ctx.commit_with_gherrit_id("Side change");
    let side_id = ctx.gherrit_id("HEAD").unwrap();
    ctx.run_git(&["checkout", "first-parent-merge"]);
    ctx.run_git(&["merge", "--no-ff", "side", "-m", "Merge side\n\ngherrit-pr-id: Gmerge"]);

    ctx.hook_cmd("pre-push").assert().success();

    assert!(ctx.remote_ref_oid(&format!("refs/heads/{stack_id}")).is_some());
    assert!(ctx.remote_ref_oid("refs/heads/Gmerge").is_some());
    assert!(ctx.remote_ref_oid(&format!("refs/heads/{side_id}")).is_none());
    let pull_requests = ctx.github().pull_requests();
    assert_eq!(
        pull_requests.iter().map(|pr| (pr.head.as_str(), pr.base.as_str())).collect::<Vec<_>>(),
        [(stack_id.as_str(), "main"), ("Gmerge", "gherrit-bases/Gmerge"),]
    );
}

#[test]
fn test_stack_id_comes_only_from_the_trailer_block() {
    let ctx = testutil::test_context!()
        .with_remote()
        .with_initial_commit()
        .with_mock_github()
        .with_git_interceptor()
        .build();
    ctx.checkout_managed_private("trailer-block");
    ctx.run_git(&[
        "commit",
        "--allow-empty",
        "--no-verify",
        "--cleanup=verbatim",
        "-m",
        "Document an example\n\ngherrit-pr-id: Gexample\n\nExplanation.\n\nGherrit-Pr-Id: Greal",
    ]);

    ctx.hook_cmd("pre-push").assert().success();

    assert!(ctx.remote_ref_oid("refs/heads/Gexample").is_none());
    assert_eq!(ctx.remote_ref_oid("refs/heads/Greal").as_deref(), Some(ctx.head_oid().as_str()));
    let pull_requests = ctx.github().pull_requests();
    assert_eq!(pull_requests.len(), 1);
    let body = &pull_requests[0].body;
    assert!(body.contains("gherrit-pr-id: Gexample"));
    assert!(!body.contains("\nGherrit-Pr-Id: Greal\n"));
}

#[test]
fn test_unrelated_continued_trailer_does_not_hide_stack_id() {
    let ctx = testutil::test_context!()
        .with_remote()
        .with_initial_commit()
        .with_mock_github()
        .with_git_interceptor()
        .build();
    ctx.checkout_managed_private("continued-trailer");
    ctx.run_git(&[
        "commit",
        "--allow-empty",
        "--no-verify",
        "--cleanup=verbatim",
        "-m",
        "Work\n\nReviewed-by: First\n continuation\ngherrit-pr-id: Gone",
    ]);

    ctx.hook_cmd("pre-push").assert().success();
    assert!(ctx.remote_ref_oid("refs/heads/Gone").is_some());
}

#[test]
fn test_replacement_ref_is_ignored_even_with_gix_075_false_polarity() {
    let ctx = testutil::test_context!()
        .with_remote()
        .with_initial_commit()
        .with_mock_github()
        .with_git_interceptor()
        .build();
    ctx.checkout_managed_private("replacement-ref");
    let id = ctx.commit_with_gherrit_id("Literal commit");
    let original = ctx.head_oid();
    let tree = String::from_utf8(
        ctx.git_cmd()
            .args(["rev-parse", "HEAD^{tree}"])
            .assert()
            .success()
            .get_output()
            .stdout
            .clone(),
    )
    .unwrap();
    let parent = String::from_utf8(
        ctx.git_cmd().args(["rev-parse", "HEAD^"]).assert().success().get_output().stdout.clone(),
    )
    .unwrap();
    let replacement = String::from_utf8(
        ctx.git_cmd()
            .arg("commit-tree")
            .arg(tree.trim())
            .arg("-p")
            .arg(parent.trim())
            .args(["-m", "Replacement without a GHerrit ID"])
            .assert()
            .success()
            .get_output()
            .stdout
            .clone(),
    )
    .unwrap();
    ctx.git_cmd().arg("replace").arg(&original).arg(replacement.trim()).assert().success();
    ctx.run_git(&["config", "core.useReplaceRefs", "false"]);
    ctx.git_cmd()
        .args(["show-ref", "--verify"])
        .arg(format!("refs/replace/{original}"))
        .assert()
        .success();

    ctx.hook_cmd("pre-push").env("GIT_NO_REPLACE_OBJECTS", "0").assert().success();

    assert_eq!(ctx.remote_ref_oid(&format!("refs/heads/{id}")).as_deref(), Some(original.as_str()));
    let pull_requests = ctx.github().pull_requests();
    assert_eq!(pull_requests.len(), 1);
    assert_eq!(pull_requests[0].title, "Literal commit");
    let pushes = ctx.recorded_pushes();
    let arguments = pushes[0].arguments();
    assert_eq!(&arguments[..2], ["git", "--no-replace-objects"]);
    assert!(arguments.iter().any(|argument| argument == "push"));
    assert!(!arguments.iter().any(|argument| argument == "--no-verify"));
}

#[test]
fn test_real_partial_clone_does_not_lazy_fetch_an_omitted_blob() {
    let ctx =
        testutil::test_context!().with_remote().with_initial_commit().with_mock_github().build();
    fs::write(ctx.repo_path.join("omitted.txt"), "This blob must remain remote-only.\n").unwrap();
    ctx.run_git(&["add", "omitted.txt"]);
    ctx.commit("Add a blob for the partial clone");
    let omitted_blob = String::from_utf8(
        ctx.git_cmd()
            .args(["rev-parse", "HEAD:omitted.txt"])
            .assert()
            .success()
            .get_output()
            .stdout
            .clone(),
    )
    .unwrap();
    let omitted_blob = omitted_blob.trim();
    ctx.run_git(&["push", "--no-verify", "origin", "main"]);
    ctx.remote_git_cmd().args(["config", "uploadpack.allowFilter", "true"]).assert().success();

    let origin = String::from_utf8(
        ctx.git_cmd()
            .args(["remote", "get-url", "origin"])
            .assert()
            .success()
            .get_output()
            .stdout
            .clone(),
    )
    .unwrap();
    let origin = origin.trim();
    let filtered = ctx.dir.path().join("filtered");
    ctx.git_cmd()
        .current_dir(ctx.dir.path())
        .args(["clone", "--filter=blob:none", "--no-checkout", "--no-local", origin])
        .arg(&filtered)
        .assert()
        .success();

    ctx.git_cmd()
        .current_dir(&filtered)
        .args(["remote", "rename", "origin", "promisor"])
        .assert()
        .success();
    let unavailable_promisor = ctx.dir.path().join("unavailable-promisor.git");
    ctx.git_cmd()
        .current_dir(&filtered)
        .args(["remote", "set-url", "promisor"])
        .arg(&unavailable_promisor)
        .assert()
        .success();
    ctx.git_cmd()
        .current_dir(&filtered)
        .args(["remote", "add", "origin", origin])
        .assert()
        .success();

    let tree = String::from_utf8(
        ctx.git_cmd()
            .current_dir(&filtered)
            .args(["rev-parse", "main^{tree}"])
            .assert()
            .success()
            .get_output()
            .stdout
            .clone(),
    )
    .unwrap();
    let head = String::from_utf8(
        ctx.git_cmd()
            .current_dir(&filtered)
            .arg("commit-tree")
            .arg(tree.trim())
            .args(["-p", "main", "-m", "Locally available work\n\ngherrit-pr-id: Gpartial"])
            .assert()
            .success()
            .get_output()
            .stdout
            .clone(),
    )
    .unwrap();
    let head = head.trim();
    ctx.git_cmd()
        .current_dir(&filtered)
        .args(["update-ref", "refs/heads/partial-feature", head])
        .assert()
        .success();
    ctx.git_cmd()
        .current_dir(&filtered)
        .args(["symbolic-ref", "HEAD", "refs/heads/partial-feature"])
        .assert()
        .success();
    for (suffix, value) in [
        ("gherritManaged", testutil::MANAGED_PRIVATE),
        ("pushRemote", "."),
        ("remote", "."),
        ("merge", "refs/heads/partial-feature"),
    ] {
        ctx.git_cmd()
            .current_dir(&filtered)
            .args(["config", &format!("branch.partial-feature.{suffix}"), value])
            .assert()
            .success();
    }

    assert!(!locally_stored_objects(&ctx, &filtered).iter().any(|oid| oid == omitted_blob));
    let output =
        ctx.gherrit_cmd().current_dir(&filtered).args(["hook", "pre-push"]).output().unwrap();

    if installed_git_version(&ctx) >= (2, 45) {
        assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
        assert_eq!(ctx.remote_ref_oid("refs/heads/Gpartial").as_deref(), Some(head));
        let pull_requests = ctx.github().pull_requests();
        assert_eq!(pull_requests.len(), 1);
        assert_eq!(pull_requests[0].title, "Locally available work");
    } else {
        assert!(!output.status.success());
        assert!(
            String::from_utf8_lossy(&output.stderr)
                .contains("requires Git 2.45 or newer for a promisor repository")
        );
        assert!(ctx.remote_ref_oid("refs/heads/Gpartial").is_none());
        assert!(ctx.github().requests().is_empty());
    }
    assert!(!locally_stored_objects(&ctx, &filtered).iter().any(|oid| oid == omitted_blob));
}

#[test]
fn amend_adds_one_immutable_version_without_republishing_v1() {
    let ctx = testutil::test_context!()
        .with_remote()
        .with_initial_commit()
        .with_mock_github()
        .with_git_interceptor()
        .build();

    ctx.checkout_managed_private("feat-versioning");
    ctx.commit_with_gherrit_id("Feature Commit");
    let gherrit_id = ctx.gherrit_id("HEAD").unwrap();
    let v1_oid = ctx.head_oid();
    let owned_head_ref = format!("refs/heads/{gherrit_id}");
    let v1_ref = format!("refs/tags/gherrit/{gherrit_id}/v1");
    let v2_ref = format!("refs/tags/gherrit/{gherrit_id}/v2");

    testutil::assert_success_snapshot!(ctx, ctx.hook_cmd("pre-push"), "version_increment_v1");

    assert_eq!(ctx.remote_ref_oid(&owned_head_ref).as_deref(), Some(v1_oid.as_str()));
    assert_eq!(ctx.remote_ref_oid(&v1_ref).as_deref(), Some(v1_oid.as_str()));

    ctx.amend();
    let v2_oid = ctx.head_oid();
    assert_ne!(v2_oid, v1_oid);

    testutil::assert_success_snapshot!(ctx, ctx.hook_cmd("pre-push"), "version_increment_v2");

    assert_eq!(ctx.remote_ref_oid(&owned_head_ref).as_deref(), Some(v2_oid.as_str()));
    assert_eq!(ctx.remote_ref_oid(&v1_ref).as_deref(), Some(v1_oid.as_str()));
    assert_eq!(ctx.remote_ref_oid(&v2_ref).as_deref(), Some(v2_oid.as_str()));

    let pushes = ctx.recorded_pushes();
    assert_eq!(
        pushes.len(),
        3,
        "the first publication uses tuple and marker pushes; advancement uses one tuple push"
    );
    assert!(
        pushes[2].arguments().iter().all(|argument| !argument.contains(&v1_ref)),
        "advancement must not republish the immutable v1 tag: {:?}",
        pushes[2].arguments()
    );
}

#[test]
fn test_versions_come_only_from_the_push_destination() {
    let ctx = testutil::test_context!()
        .with_remote()
        .with_initial_commit()
        .with_mock_github()
        .with_git_interceptor()
        .build();

    ctx.checkout_managed_private("feat-versioning");
    ctx.commit_with_gherrit_id("Feature Commit");
    let gherrit_id = ctx.gherrit_id("HEAD").unwrap();
    let v1_oid = ctx.head_oid();
    let owned_head_ref = format!("refs/heads/{gherrit_id}");
    let v1_ref = format!("refs/tags/gherrit/{gherrit_id}/v1");
    let v2_ref = format!("refs/tags/gherrit/{gherrit_id}/v2");
    let v3_ref = format!("refs/tags/gherrit/{gherrit_id}/v3");
    let local_high_ref = format!("refs/tags/gherrit/{gherrit_id}/v99");
    let fetch_high_ref = format!("refs/tags/gherrit/{gherrit_id}/v123");

    // Neither local tags nor the fetch URL describe the publication
    // destination. Give both misleading high version histories before the
    // first publication; the empty push destination must still begin at v1.
    ctx.run_git(&["tag", &format!("gherrit/{gherrit_id}/v99")]);
    let fetch_repository = ctx.dir.path().join("divergent-fetch.git");
    ctx.git_cmd()
        .current_dir(ctx.dir.path())
        .args(["clone", "--bare"])
        .arg(&ctx.repo_path)
        .arg(&fetch_repository)
        .assert()
        .success();
    let fetch_git_dir = format!("--git-dir={}", fetch_repository.display());
    ctx.git_cmd()
        .args([fetch_git_dir.as_str(), "update-ref", "-d", &local_high_ref])
        .assert()
        .success();
    ctx.git_cmd()
        .args([fetch_git_dir.as_str(), "update-ref", &fetch_high_ref, &v1_oid])
        .assert()
        .success();
    let push_destination = configured_remote_url(&ctx, "origin");
    let fetch_destination =
        fetch_repository.to_string_lossy().replace(std::path::MAIN_SEPARATOR, "/");
    ctx.set_config("remote.origin.url", Some(&fetch_destination));
    ctx.set_config("remote.origin.pushurl", Some(&push_destination));

    ctx.git_cmd().args(["show-ref", "--verify", &local_high_ref]).assert().success();
    assert_eq!(ctx.remote_ref_oid(&owned_head_ref), None);
    assert_eq!(ctx.remote_ref_oid(&v1_ref), None);
    assert_eq!(ctx.remote_ref_oid(&local_high_ref).as_deref(), None);
    assert_eq!(bare_ref_oid(&ctx, &fetch_repository, &local_high_ref), None);
    assert_eq!(
        bare_ref_oid(&ctx, &fetch_repository, &fetch_high_ref).as_deref(),
        Some(v1_oid.as_str())
    );

    ctx.hook_cmd("pre-push").assert().success();

    assert_eq!(ctx.remote_ref_oid(&owned_head_ref).as_deref(), Some(v1_oid.as_str()));
    assert_eq!(ctx.remote_ref_oid(&v1_ref).as_deref(), Some(v1_oid.as_str()));
    assert_eq!(ctx.remote_ref_oid(&v2_ref), None);
    assert_eq!(ctx.remote_ref_oid(&local_high_ref), None);
    assert_eq!(ctx.remote_ref_oid(&fetch_high_ref), None);

    // Publication does not create local tags. Remove the unrelated local tag
    // as well, then prove the remote v1 is sufficient to select v2.
    ctx.run_git(&["tag", "-d", &format!("gherrit/{gherrit_id}/v99")]);
    ctx.git_cmd().args(["show-ref", "--verify", &v1_ref]).assert().failure();
    ctx.git_cmd().args(["show-ref", "--verify", &local_high_ref]).assert().failure();
    ctx.amend();
    let v2_oid = ctx.head_oid();
    assert_ne!(v2_oid, v1_oid);

    ctx.hook_cmd("pre-push").assert().success();

    assert_eq!(ctx.remote_ref_oid(&owned_head_ref).as_deref(), Some(v2_oid.as_str()));
    assert_eq!(ctx.remote_ref_oid(&v1_ref).as_deref(), Some(v1_oid.as_str()));
    assert_eq!(ctx.remote_ref_oid(&v2_ref).as_deref(), Some(v2_oid.as_str()));

    let pushes_before_retry = ctx.recorded_pushes();
    let refs_before_retry = ctx.remote_refs("refs");
    assert_eq!(
        pushes_before_retry.len(),
        3,
        "initial tuple and marker pushes precede the amended tuple push"
    );
    assert!(
        pushes_before_retry[2].arguments().iter().all(|argument| !argument.contains(&v1_ref)),
        "advancement must not republish the immutable v1 tag: {:?}",
        pushes_before_retry[2].arguments()
    );

    // Retrying an unchanged head may reconcile GitHub, but it must not queue
    // another Git push or allocate a new version tag.
    ctx.hook_cmd("pre-push").assert().success();

    assert_eq!(ctx.recorded_pushes(), pushes_before_retry);
    assert_eq!(ctx.remote_refs("refs"), refs_before_retry);
    assert_eq!(ctx.remote_ref_oid(&v3_ref), None);
}

#[test]
fn test_observation_advances_past_a_preexisting_remote_version() {
    let ctx = testutil::test_context!()
        .with_remote()
        .with_initial_commit()
        .with_mock_github()
        .with_git_interceptor()
        .build();

    ctx.checkout_managed_private("feature-observed-version");
    ctx.commit_with_gherrit_id("Commit V1");

    ctx.hook_cmd("pre-push").assert().success();

    let gherrit_id = ctx.gherrit_id("HEAD").unwrap();
    let managed_ref = format!("refs/heads/{gherrit_id}");
    let v1_ref = format!("refs/tags/gherrit/{gherrit_id}/v1");
    let v2_ref = format!("refs/tags/gherrit/{gherrit_id}/v2");
    let v3_ref = format!("refs/tags/gherrit/{gherrit_id}/v3");
    let v1_oid = ctx.remote_ref_oid(&managed_ref).expect("Managed ref was not pushed");

    // This tag exists before the next observation, so it is authoritative
    // destination state rather than a race with the subsequent push.
    ctx.remote_git_cmd()
        .args(["tag", &format!("gherrit/{gherrit_id}/v2"), &managed_ref])
        .assert()
        .success();
    assert_eq!(ctx.remote_ref_oid(&v2_ref).as_deref(), Some(v1_oid.as_str()));

    let new_msg = format!("Commit V1 (Amended)\n\ngherrit-pr-id: {gherrit_id}");
    ctx.amend_with_message(&new_msg);
    let v3_oid = ctx.head_oid();

    ctx.hook_cmd("pre-push").assert().success();

    let pushes = ctx.recorded_pushes();
    assert_eq!(pushes.len(), 3, "initial tuple and marker pushes precede the amended tuple push");
    assert!(pushes.iter().all(testutil::PushRecord::succeeded));
    assert_eq!(ctx.remote_ref_oid(&managed_ref).as_deref(), Some(v3_oid.as_str()));
    assert_eq!(ctx.remote_ref_oid(&v1_ref).as_deref(), Some(v1_oid.as_str()));
    assert_eq!(ctx.remote_ref_oid(&v2_ref).as_deref(), Some(v1_oid.as_str()));
    assert_eq!(ctx.remote_ref_oid(&v3_ref).as_deref(), Some(v3_oid.as_str()));
}

#[test]
fn next_version_absence_lease_rejects_the_complete_tuple() {
    let ctx = testutil::test_context!()
        .with_remote()
        .with_initial_commit()
        .with_mock_github()
        .with_git_interceptor()
        .build();

    ctx.checkout_managed_private("feature-conflict");
    ctx.commit_with_gherrit_id("Commit V1");

    testutil::assert_success_snapshot!(ctx, ctx.hook_cmd("pre-push"), "next_version_lease_v1");

    let gherrit_id = ctx.gherrit_id("HEAD").unwrap();
    let owned_head_ref = format!("refs/heads/{gherrit_id}");
    let published_head =
        ctx.remote_ref_oid(&owned_head_ref).expect("the change-owned head must be published");

    // Change the message so the amended commit has a distinct object ID while
    // retaining the stable GHerrit ID of the same logical change.
    let new_msg = format!("Commit V1 (Amended)\n\ngherrit-pr-id: {}", gherrit_id);
    ctx.amend_with_message(&new_msg);

    // Another publisher creates the exact next tag after this attempt has
    // observed absence but before its atomic push reaches the remote.
    let v2_ref = format!("refs/tags/gherrit/{gherrit_id}/v2");
    ctx.update_remote_ref_before_push(&v2_ref, &published_head);

    testutil::assert_failure_snapshot!(
        ctx,
        ctx.hook_cmd("pre-push"),
        "next_version_lease_conflict"
    );

    let pushes = ctx.recorded_pushes();
    assert_eq!(pushes.len(), 3, "two established barriers precede the failed advancement");
    assert!(pushes[0].succeeded(), "the initial tuple push succeeds");
    assert!(pushes[1].succeeded(), "the initial marker push succeeds");
    assert!(!pushes[2].succeeded(), "the conflicting tuple push fails");
    assert_eq!(
        ctx.remote_ref_oid(&owned_head_ref).as_deref(),
        Some(published_head.as_str()),
        "the rejected atomic push must not update the change-owned head"
    );
    assert_eq!(ctx.remote_ref_oid(&v2_ref).as_deref(), Some(published_head.as_str()));
}

#[test]
fn concurrent_public_branch_creation_rejects_the_whole_initial_ref_batch() {
    let ctx = testutil::test_context!()
        .with_remote()
        .with_initial_commit()
        .with_mock_github()
        .with_git_interceptor()
        .build();
    ctx.checkout_managed_public("public-lease");
    let id = ctx.commit_with_gherrit_id("Publish with a public branch lease");
    let competing = ctx.remote_ref_oid("refs/heads/main").unwrap();
    ctx.update_remote_ref_before_push("refs/heads/public-lease", &competing);

    ctx.hook_cmd("pre-push").assert().failure();

    assert_eq!(ctx.remote_ref_oid("refs/heads/public-lease").as_deref(), Some(competing.as_str()));
    assert!(ctx.remote_ref_oid(&format!("refs/heads/{id}")).is_none());
    assert!(ctx.remote_ref_oid(&format!("refs/heads/gherrit-bases/{id}")).is_none());
    assert!(ctx.remote_ref_oid(&format!("refs/tags/gherrit/{id}/v1")).is_none());
    assert!(ctx.github().pull_requests().is_empty());
    assert_eq!(github_operation_count(&ctx, testutil::GraphQlOperation::CreatePr), 0);
    assert_eq!(github_operation_count(&ctx, testutil::GraphQlOperation::UpdatePr), 0);
    let pushes = ctx.recorded_pushes();
    assert_eq!(pushes.len(), 1);
    assert!(!pushes[0].succeeded());
}

#[test]
fn concurrent_creation_at_the_desired_public_tip_is_acknowledged_as_current() {
    let ctx = testutil::test_context!()
        .with_remote()
        .with_initial_commit()
        .with_git_interceptor()
        .build();
    ctx.checkout_managed_public("public-desired-race");
    let desired = ctx.remote_ref_oid("refs/heads/main").unwrap();
    ctx.update_remote_ref_before_push("refs/heads/public-desired-race", &desired);

    ctx.hook_cmd("pre-push").assert().success();

    assert_eq!(
        ctx.remote_ref_oid("refs/heads/public-desired-race").as_deref(),
        Some(desired.as_str())
    );
    let pushes = ctx.recorded_pushes();
    assert_eq!(pushes.len(), 1);
    assert!(pushes[0].succeeded());
}

#[test]
fn empty_public_stack_advances_once_then_needs_no_git_or_github_work() {
    let ctx = testutil::test_context!()
        .with_remote()
        .with_initial_commit()
        .with_git_interceptor()
        .build();
    ctx.checkout_new("temporary-public-source");
    ctx.commit("Divergent public value");
    let divergent = ctx.head_oid();
    ctx.run_git(&["push", "--no-verify", "origin", "HEAD:refs/heads/empty-public-state"]);
    ctx.run_git(&["checkout", "main"]);
    ctx.checkout_managed_public("empty-public-state");
    let desired = ctx.head_oid();
    assert_ne!(desired, divergent);
    let fixture_pushes = ctx.recorded_pushes().len();
    let observation_count = || {
        ctx.recorded_git_operations()
            .into_iter()
            .filter(|operation| *operation == testutil::GitOperation::LsRemote)
            .count()
    };
    let fixture_observations = observation_count();

    ctx.hook_cmd("pre-push").assert().success();

    assert_eq!(
        ctx.remote_ref_oid("refs/heads/empty-public-state").as_deref(),
        Some(desired.as_str())
    );
    assert_eq!(ctx.recorded_pushes().len(), fixture_pushes + 1);
    let first_attempt_observations = observation_count();
    assert_eq!(
        first_attempt_observations,
        fixture_observations + 2,
        "a divergent empty public stack observes symbolic HEAD/public state, then the exact named default"
    );

    ctx.hook_cmd("pre-push").assert().success();
    assert_eq!(
        ctx.recorded_pushes().len(),
        fixture_pushes + 1,
        "an already-current empty public stack has no Git effect"
    );
    let current_attempt_observations = observation_count();
    assert_eq!(
        current_attempt_observations,
        first_attempt_observations + 1,
        "an already-current empty public stack needs only its initial observation"
    );

    ctx.run_git(&["checkout", "main"]);
    ctx.checkout_managed_private("empty-private-state");
    ctx.hook_cmd("pre-push").assert().success();
    let private_attempt_observations = observation_count();
    assert_eq!(
        private_attempt_observations,
        current_attempt_observations + 1,
        "an empty private stack needs only its initial observation"
    );
    assert_eq!(ctx.recorded_pushes().len(), fixture_pushes + 1);
}

#[test]
fn ordinary_ref_directory_file_conflicts_reject_public_projection_before_mutation() {
    for (public, conflicting) in [
        ("release-v1", "refs/heads/release-v1/child"),
        ("release-v1/work", "refs/heads/release-v1"),
    ] {
        let ctx = testutil::test_context!()
            .with_remote()
            .with_initial_commit()
            .with_mock_github()
            .with_git_interceptor()
            .build();
        let default = ctx.remote_ref_oid("refs/heads/main").unwrap();
        ctx.remote_git_cmd().args(["update-ref", conflicting, &default]).assert().success();
        ctx.checkout_managed_public(public);
        let id = ctx.commit_with_gherrit_id("Reject an ordinary ref namespace conflict");

        ctx.hook_cmd("pre-push").assert().failure();

        assert_eq!(ctx.remote_ref_oid(conflicting).as_deref(), Some(default.as_str()));
        assert!(ctx.remote_ref_oid(&format!("refs/heads/{public}")).is_none());
        assert!(ctx.remote_ref_oid(&format!("refs/heads/{id}")).is_none());
        assert!(ctx.remote_ref_oid(&format!("refs/heads/gherrit-bases/{id}")).is_none());
        assert!(ctx.remote_ref_oid(&format!("refs/tags/gherrit/{id}/v1")).is_none());
        assert!(ctx.github().pull_requests().is_empty());
        assert_eq!(github_operation_count(&ctx, testutil::GraphQlOperation::CreatePr), 0);
        assert_eq!(github_operation_count(&ctx, testutil::GraphQlOperation::UpdatePr), 0);
        let pushes = ctx.recorded_pushes();
        assert_eq!(pushes.len(), 1);
        assert!(!pushes[0].succeeded());
    }
}

#[test]
fn public_projection_replaces_the_exact_divergent_value_it_observed() {
    let ctx = testutil::test_context!()
        .with_remote()
        .with_initial_commit()
        .with_mock_github()
        .with_git_interceptor()
        .build();
    let observed = ctx.remote_ref_oid("refs/heads/main").unwrap();
    ctx.remote_git_cmd()
        .args(["update-ref", "refs/heads/release-v2", &observed])
        .assert()
        .success();
    ctx.checkout_managed_public("release-v2");
    ctx.commit_with_gherrit_id("Replace the owned public projection");
    let desired = ctx.head_oid();

    ctx.hook_cmd("pre-push").assert().success();

    assert_eq!(ctx.remote_ref_oid("refs/heads/release-v2").as_deref(), Some(desired.as_str()));
    assert!(ctx.recorded_pushes().iter().any(|push| {
        push.arguments().iter().any(|argument| {
            argument == &format!("--force-with-lease=refs/heads/release-v2:{observed}")
        })
    }));
}

#[test]
fn a_public_move_after_its_barrier_cannot_change_pull_request_comparison_refs() {
    let ctx = testutil::test_context!()
        .with_remote()
        .with_initial_commit()
        .with_mock_github()
        .with_git_interceptor()
        .build();
    ctx.checkout_managed_public("release-v3");
    let id = ctx.commit_with_gherrit_id("Keep comparison refs independent of the public branch");
    let desired = ctx.head_oid();
    let default = ctx.remote_ref_oid("refs/heads/main").unwrap();
    ctx.update_remote_ref_before_push("refs/heads/unrelated-race-step", &default);
    ctx.update_remote_ref_before_push("refs/heads/release-v3", &default);

    ctx.hook_cmd("pre-push").assert().success();

    assert_eq!(ctx.remote_ref_oid("refs/heads/release-v3").as_deref(), Some(default.as_str()));
    let pull_requests = ctx.github().pull_requests();
    assert_eq!(pull_requests.len(), 1);
    assert_eq!(pull_requests[0].head, id);
    assert_eq!(pull_requests[0].base, "main");
    assert_eq!(
        ctx.remote_ref_oid(&format!("refs/heads/{}", pull_requests[0].head)).as_deref(),
        Some(desired.as_str())
    );

    let pushes_before = ctx.recorded_pushes().len();
    ctx.hook_cmd("pre-push").assert().success();
    assert_eq!(ctx.remote_ref_oid("refs/heads/release-v3").as_deref(), Some(desired.as_str()));
    assert_eq!(ctx.recorded_pushes().len(), pushes_before + 1);
}

#[test]
fn making_an_established_private_stack_public_only_adds_the_public_projection() {
    let ctx = testutil::test_context!()
        .with_remote()
        .with_initial_commit()
        .with_mock_github()
        .with_git_interceptor()
        .build();
    ctx.checkout_managed_private("release-v1");
    let id = ctx.commit_with_gherrit_id("Make an established stack public");
    let head = ctx.head_oid();
    ctx.hook_cmd("pre-push").assert().success();
    let tuple_ref = format!("refs/tags/gherrit/{id}/v1");
    assert_eq!(ctx.remote_ref_oid(&tuple_ref).as_deref(), Some(head.as_str()));

    ctx.gherrit_cmd().args(["manage", "--public", "--force"]).assert().success();
    ctx.hook_cmd("pre-push").assert().success();

    assert_eq!(ctx.remote_ref_oid("refs/heads/release-v1").as_deref(), Some(head.as_str()));
    assert_eq!(ctx.remote_ref_oid(&tuple_ref).as_deref(), Some(head.as_str()));
    let pull_requests = ctx.github().pull_requests();
    assert_eq!(pull_requests.len(), 1);
    assert!(pull_requests[0].body.contains("[release\\-v1](/owner/repo/tree/release-v1)"));
    let pushes = ctx.recorded_pushes();
    assert_eq!(pushes.len(), 3, "the transition adds one public-branch-only Git barrier");
    assert!(
        pushes[2]
            .arguments()
            .iter()
            .any(|argument| argument == &format!("{head}:refs/heads/release-v1"))
    );

    let pushes_before = pushes.len();
    ctx.hook_cmd("pre-push").assert().success();
    assert_eq!(
        ctx.recorded_pushes().len(),
        pushes_before,
        "an already-current public projection needs no Git write"
    );

    ctx.gherrit_cmd().args(["manage", "--private"]).assert().success();
    ctx.hook_cmd("pre-push").assert().success();
    assert_eq!(
        ctx.remote_ref_oid("refs/heads/release-v1").as_deref(),
        Some(head.as_str()),
        "making the stack private must not delete its former public projection"
    );
    assert!(
        !ctx.github().pull_requests()[0]
            .body
            .contains("[release\\-v1](/owner/repo/tree/release-v1)"),
        "the desired private body must not retain a public-branch link"
    );
}

#[test]
fn renaming_a_public_stack_adds_a_projection_without_deleting_the_old_one() {
    let ctx = testutil::test_context!()
        .with_remote()
        .with_initial_commit()
        .with_mock_github()
        .with_git_interceptor()
        .build();
    ctx.checkout_managed_public("release-old");
    let id = ctx.commit_with_gherrit_id("Rename the public stack");
    let head = ctx.head_oid();
    ctx.hook_cmd("pre-push").assert().success();

    ctx.run_git(&["branch", "--move", "release-new"]);
    ctx.gherrit_cmd().args(["manage", "--public", "--force"]).assert().success();
    ctx.hook_cmd("pre-push").assert().success();

    assert_eq!(
        ctx.remote_ref_oid("refs/heads/release-old").as_deref(),
        Some(head.as_str()),
        "renaming must not delete the former public projection"
    );
    assert_eq!(ctx.remote_ref_oid("refs/heads/release-new").as_deref(), Some(head.as_str()));
    let pull_requests = ctx.github().pull_requests();
    assert_eq!(pull_requests.len(), 1);
    assert_eq!(pull_requests[0].head, id);
    let body = &pull_requests[0].body;
    assert!(body.contains("[release\\-new](/owner/repo/tree/release-new)"));
    assert!(!body.contains("[release\\-old](/owner/repo/tree/release-old)"));
}

#[test]
fn test_graphql_batch_backoff() {
    let ctx = testutil::test_context!()
        .with_remote()
        .with_initial_commit()
        .with_mock_github()
        .with_git_interceptor()
        .build();

    ctx.limit_graphql_query_operations_per_request(2);
    ctx.checkout_managed_private("batch-backoff");

    for i in 1..=4 {
        ctx.commit_with_gherrit_id(&format!("Commit {i}"));
    }

    testutil::assert_success_snapshot!(ctx, ctx.hook_cmd("pre-push"), "graphql_batch_backoff");

    assert_eq!(
        ctx.recorded_pushes().iter().filter(|push| push.succeeded()).count(),
        2,
        "GraphQL backoff must not alter the tuple and marker publication barriers"
    );
    assert_eq!(ctx.github().pull_requests().len(), 4, "Expected every commit to have a PR");
    let requests = ctx.github().requests();
    insta::assert_debug_snapshot!("graphql_batch_backoff_trace", requests);
    assert!(
        ctx.github()
            .requests()
            .iter()
            .any(|request| { request == &vec![testutil::GraphQlOperation::CreatePr; 4] }),
        "query backoff must not impose its learned limit on mutation batches"
    );

    let v1_refs = ctx
        .remote_refs("refs/tags/gherrit")
        .into_iter()
        .filter(|ref_name| ref_name.ends_with("/v1"))
        .count();
    assert_eq!(v1_refs, 4, "Expected every v1 tag on the remote");
}

#[test]
fn checked_management_intent_controls_public_branch_links_despite_push_remote_drift() {
    for (branch, state, drifted_push_remote, expected_link) in [
        ("private-intent", testutil::MANAGED_PRIVATE, "origin", None),
        (
            "public-intent",
            testutil::MANAGED_PUBLIC,
            ".",
            Some("This PR is on branch [public\\-intent](/owner/repo/tree/public-intent)."),
        ),
    ] {
        let ctx = testutil::test_context!()
            .with_remote()
            .with_initial_commit()
            .with_mock_github()
            .with_git_interceptor()
            .build();
        match state {
            testutil::MANAGED_PRIVATE => ctx.checkout_managed_private(branch),
            testutil::MANAGED_PUBLIC => ctx.checkout_managed_public(branch),
            _ => unreachable!("test covers the two managed states"),
        }
        ctx.set_config(&format!("branch.{branch}.pushRemote"), Some(drifted_push_remote));
        ctx.commit_with_gherrit_id("Retain checked privacy intent");

        ctx.hook_cmd("pre-push").assert().success();

        ctx.assert_config(&format!("branch.{branch}.gherritManaged"), Some(state));
        ctx.assert_config(&format!("branch.{branch}.pushRemote"), Some(drifted_push_remote));
        let pull_requests = ctx.github().pull_requests();
        let body = &pull_requests[0].body;
        let links = body
            .lines()
            .filter(|line| line.starts_with("This PR is on branch ["))
            .collect::<Vec<_>>();
        assert_eq!(links, expected_link.into_iter().collect::<Vec<_>>(), "state={state}");
    }
}

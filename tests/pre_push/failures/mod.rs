use std::fs;

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

fn stack_with_raw_commit_message_bytes(message: &[u8]) -> testutil::TestContext {
    let ctx = testutil::test_context!()
        .with_remote()
        .with_initial_commit()
        .with_mock_github()
        .with_git_interceptor()
        .build();
    ctx.checkout_managed_private("invalid-message");

    let repository = gix::open(&ctx.repo_path).unwrap();
    let parent = repository.head_commit().unwrap();
    let signature = gix::actor::Signature {
        name: "GHerrit test".into(),
        email: "test@example.com".into(),
        time: gix::actor::date::Time::new(0, 0),
    };
    let commit = repository
        .write_object(&gix::objs::Commit {
            tree: parent.tree_id().unwrap().detach(),
            parents: [parent.id].into_iter().collect(),
            author: signature.clone(),
            committer: signature,
            encoding: None,
            message: message.into(),
            extra_headers: Vec::new(),
        })
        .unwrap()
        .detach();
    drop(parent);
    drop(repository);
    ctx.run_git(&["reset", "--hard", &commit.to_string()]);
    ctx
}

fn unpublished_managed_commit(branch: &str) -> testutil::TestContext {
    let ctx = testutil::test_context!()
        .with_remote()
        .with_initial_commit()
        .with_mock_github()
        .with_git_interceptor()
        .build();
    ctx.checkout_managed_private(branch);
    ctx.commit_with_gherrit_id("Work");
    ctx
}

fn open_head_query(ctx: &testutil::TestContext) -> testutil::GraphQlOperation {
    testutil::GraphQlOperation::open_query([ctx.gherrit_id("HEAD").unwrap()], true)
}

fn terminal_head_query(ctx: &testutil::TestContext) -> testutil::GraphQlOperation {
    testutil::GraphQlOperation::terminal_query([ctx.gherrit_id("HEAD").unwrap()])
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

fn assert_identity_failure_before_external_io(ctx: testutil::TestContext, diagnostic: &str) {
    ctx.hook_cmd("pre-push").assert().failure().stderr(predicate::str::contains(diagnostic));

    assert!(ctx.github().requests().is_empty());
    assert!(ctx.recorded_pushes().is_empty());
}

fn linked_managed_stack(ctx: &testutil::TestContext, branch: &str, id: &str) -> std::path::PathBuf {
    let linked = ctx.dir.path().join(branch);
    ctx.git_cmd()
        .args(["worktree", "add", "-b", branch])
        .arg(&linked)
        .arg("main")
        .assert()
        .success();
    for (suffix, value) in [
        ("gherritManaged", testutil::MANAGED_PRIVATE),
        ("pushRemote", "."),
        ("remote", "."),
        ("merge", &format!("refs/heads/{branch}")),
    ] {
        ctx.git_cmd()
            .current_dir(&linked)
            .arg("config")
            .arg(format!("branch.{branch}.{suffix}"))
            .arg(value)
            .assert()
            .success();
    }
    ctx.git_cmd()
        .current_dir(&linked)
        .args([
            "commit",
            "--allow-empty",
            "--no-verify",
            "-m",
            &format!("Linked work\n\ngherrit-pr-id: {id}"),
        ])
        .assert()
        .success();
    linked
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
fn test_non_utf8_stack_body_fails_before_external_io() {
    let ctx = stack_with_raw_commit_message_bytes(
        b"Work\n\nBody contains \xff.\n\ngherrit-pr-id: Gone\n",
    );

    assert_identity_failure_before_external_io(ctx, "non-UTF-8 message body");
}

#[test]
fn test_continued_stack_id_fails_before_external_io() {
    for continuation in [" continuation", " \t"] {
        let ctx =
            stack_with_raw_commit_message(&format!("Work\n\ngherrit-pr-id: Gone\n{continuation}"));

        assert_identity_failure_before_external_io(ctx, "invalid gherrit-pr-id trailer");
    }
}

#[test]
fn test_empty_and_valid_stack_ids_are_multiple() {
    let ctx = stack_with_raw_commit_message("Work\n\ngherrit-pr-id: \ngherrit-pr-id: Gvalid");

    assert_identity_failure_before_external_io(ctx, "multiple gherrit-pr-id trailers");
}

#[test]
fn test_noncanonical_stack_id_separators_fail_before_external_io() {
    for trailer in ["gherrit-pr-id:Gone", "gherrit-pr-id=Gone", "gherrit-pr-id:\tGone"] {
        let ctx = stack_with_raw_commit_message(&format!("Work\n\n{trailer}"));

        assert_identity_failure_before_external_io(ctx, "invalid gherrit-pr-id trailer syntax");
    }
}

#[test]
fn test_malformed_stack_id_beside_an_exact_id_is_multiple() {
    let ctx =
        stack_with_raw_commit_message("Work\n\ngherrit-pr-id: Gvalid\ngherrit-pr-id:Gmalformed");

    assert_identity_failure_before_external_io(ctx, "multiple gherrit-pr-id trailers");
}

#[test]
fn test_overlong_stack_id_fails_before_external_io() {
    let id = "G".repeat(129);
    let ctx = stack_with_raw_commit_message(&format!("Work\n\ngherrit-pr-id: {id}"));

    assert_identity_failure_before_external_io(ctx, "longer than the 128-byte limit");
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

    assert!(ctx.github().requests().is_empty());
    assert!(ctx.recorded_pushes().is_empty());
}

#[test]
fn test_stack_id_cannot_be_a_ref_path_ancestor_of_a_nested_default_branch() {
    let ctx = testutil::test_context!()
        .with_remote()
        .with_initial_commit()
        .with_mock_github()
        .with_git_interceptor()
        .build();
    let default_branch = "release/main";
    let default_ref = format!("refs/heads/{default_branch}");
    let default_tip = ctx.remote_ref_oid("refs/heads/main").unwrap();
    ctx.run_git(&["branch", "--move", "main", default_branch]);
    ctx.remote_git_cmd().args(["update-ref", &default_ref, "refs/heads/main"]).assert().success();
    ctx.remote_git_cmd().args(["symbolic-ref", "HEAD", &default_ref]).assert().success();
    ctx.remote_git_cmd().args(["update-ref", "-d", "refs/heads/main"]).assert().success();
    ctx.checkout_managed_private("nested-default-collision");
    ctx.commit_with_explicit_gherrit_id("Do not occupy a default-branch ancestor", "release");

    ctx.hook_cmd("pre-push")
        .assert()
        .failure()
        .stderr(predicate::str::contains(
            "managed branch 'refs/heads/release' conflicts with repository default branch 'refs/heads/release/main'",
        ));

    assert!(ctx.github().requests().is_empty());
    assert!(ctx.recorded_pushes().is_empty());
    assert_eq!(ctx.remote_ref_oid(&default_ref).as_deref(), Some(default_tip.as_str()));
    assert_eq!(ctx.remote_ref_oid("refs/heads/release"), None);
}

#[test]
fn test_autosquash_guidance_uses_the_validated_local_ref_with_split_destinations() {
    let ctx = testutil::test_context!()
        .with_remote()
        .with_initial_commit()
        .with_mock_github()
        .with_git_interceptor()
        .build();
    let push_destination = configured_remote_url(&ctx, "origin");
    let fetch_repository = ctx.dir.path().join("fetch-only.git");
    ctx.init_bare_repo(&fetch_repository);
    let fetch_destination =
        fetch_repository.to_string_lossy().replace(std::path::MAIN_SEPARATOR, "/");
    ctx.set_config("remote.origin.url", Some(&fetch_destination));
    ctx.set_config("remote.origin.pushurl", Some(&push_destination));
    ctx.run_git(&["update-ref", "refs/remotes/origin/main", "refs/heads/main"]);
    ctx.checkout_managed_private("split-destination-autosquash");
    ctx.commit_with_explicit_gherrit_id("fixup! pending work", "Gpending");

    ctx.hook_cmd("pre-push")
        .assert()
        .failure()
        .stderr(predicate::str::contains("git rebase -i --autosquash refs/heads/main"))
        .stderr(predicate::str::contains("origin/main").not())
        .stderr(predicate::str::contains("refs/remotes/origin/main").not());

    assert!(ctx.github().requests().is_empty());
    assert!(ctx.recorded_pushes().is_empty());
}

#[test]
fn test_replacement_cannot_hide_a_stack_id_duplicated_through_a_merge() {
    let ctx = testutil::test_context!()
        .with_remote()
        .with_initial_commit()
        .with_mock_github()
        .with_git_interceptor()
        .build();
    ctx.checkout_managed_private("duplicate-merged-id");
    ctx.commit_with_explicit_gherrit_id("Stack change", "Gduplicate");
    ctx.run_git(&["checkout", "-b", "side", "main"]);
    ctx.commit_with_explicit_gherrit_id("Side change", "Gduplicate");
    let side = ctx.head_oid();
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
    ctx.git_cmd().arg("replace").arg(side).arg(replacement.trim()).assert().success();
    ctx.git_cmd()
        .args(["log", "-1", "--format=%B", "side"])
        .assert()
        .success()
        .stdout(predicate::str::contains("Replacement without a GHerrit ID"))
        .stdout(predicate::str::contains("gherrit-pr-id").not());
    ctx.git_cmd()
        .args(["--no-replace-objects", "log", "-1", "--format=%B", "side"])
        .assert()
        .success()
        .stdout(predicate::str::contains("gherrit-pr-id: Gduplicate"));
    ctx.run_git(&["checkout", "duplicate-merged-id"]);
    ctx.run_git(&["merge", "--no-ff", "side", "-m", "Merge side\n\ngherrit-pr-id: Gmerge"]);

    ctx.hook_cmd("pre-push").assert().failure().stderr(predicate::str::contains(
        "HEAD ancestry contains multiple commits with gherrit-pr-id 'Gduplicate'",
    ));

    assert!(ctx.github().requests().is_empty());
    assert!(ctx.recorded_pushes().is_empty());
}

#[test]
fn test_stack_id_duplicated_in_default_history_fails_before_github_or_writes() {
    let ctx = testutil::test_context!()
        .with_remote()
        .with_initial_commit()
        .with_mock_github()
        .with_git_interceptor()
        .build();
    ctx.commit_with_explicit_gherrit_id("Default-branch history", "Gduplicate");
    ctx.run_git(&["push", "--quiet", "--no-verify", "origin", "refs/heads/main:refs/heads/main"]);
    let fixture_pushes = ctx.recorded_pushes();
    ctx.checkout_managed_private("duplicate-default-id");
    ctx.commit_with_explicit_gherrit_id("Stack change", "Gduplicate");

    ctx.hook_cmd("pre-push").assert().failure().stderr(predicate::str::contains(
        "HEAD ancestry contains multiple commits with gherrit-pr-id 'Gduplicate'",
    ));

    assert!(ctx.github().requests().is_empty());
    assert_eq!(ctx.recorded_pushes(), fixture_pushes);
}

#[test]
fn test_case_variant_id_in_default_history_is_still_a_duplicate() {
    let ctx = testutil::test_context!()
        .with_remote()
        .with_initial_commit()
        .with_mock_github()
        .with_git_interceptor()
        .build();
    ctx.commit("Default-branch history\n\nGherrit-Pr-Id: Gduplicate");
    ctx.run_git(&["push", "--quiet", "--no-verify", "origin", "refs/heads/main:refs/heads/main"]);
    let fixture_pushes = ctx.recorded_pushes();
    ctx.checkout_managed_private("case-variant-duplicate-default-id");
    ctx.commit_with_explicit_gherrit_id("Stack change", "Gduplicate");

    ctx.hook_cmd("pre-push").assert().failure().stderr(predicate::str::contains(
        "HEAD ancestry contains multiple commits with gherrit-pr-id 'Gduplicate'",
    ));

    assert!(ctx.github().requests().is_empty());
    assert_eq!(ctx.recorded_pushes(), fixture_pushes);
}

#[test]
fn test_default_branch_must_be_on_the_first_parent_stack_path() {
    let ctx = testutil::test_context!()
        .with_remote()
        .with_initial_commit()
        .with_mock_github()
        .with_git_interceptor()
        .build();
    ctx.checkout_managed_private("first-parent");
    ctx.commit_with_gherrit_id("Stack change");

    ctx.run_git(&["checkout", "main"]);
    ctx.commit("Advance the default branch");
    ctx.run_git(&["push", "--quiet", "--no-verify", "origin", "refs/heads/main:refs/heads/main"]);
    let fixture_pushes = ctx.recorded_pushes();
    ctx.run_git(&["checkout", "first-parent"]);
    ctx.run_git(&["merge", "--no-ff", "main", "-m", "Merge the default branch"]);

    ctx.hook_cmd("pre-push")
        .assert()
        .failure()
        .stderr(predicate::str::contains("does not descend from 'main' on its first-parent path"));

    assert!(ctx.github().requests().is_empty());
    assert_eq!(ctx.recorded_pushes(), fixture_pushes);
}

#[test]
fn test_nonempty_common_grafts_file_in_linked_worktree_blocks_publication() {
    let ctx = testutil::test_context!().with_remote().with_initial_commit().build();
    let linked = linked_managed_stack(&ctx, "linked-feature", "Glinked");
    let linked_head = ctx
        .git_cmd()
        .current_dir(&linked)
        .args(["rev-parse", "HEAD"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();

    let grafts = ctx.repo_path.join(".git/info/grafts");
    fs::write(grafts, linked_head).unwrap();

    ctx.gherrit_cmd().current_dir(&linked).args(["hook", "pre-push"]).assert().failure().stderr(
        predicate::str::contains(
            "info/grafts file is nonempty because grafts rewrite commit ancestry",
        ),
    );
    assert!(ctx.remote_ref_oid("refs/heads/Glinked").is_none());
}

#[test]
fn test_common_shallow_file_is_checked_despite_gix_config_redirection() {
    let ctx = testutil::test_context!().with_remote().with_initial_commit().build();
    let linked = linked_managed_stack(&ctx, "shallow-feature", "Gshallow");
    ctx.git_cmd()
        .current_dir(&linked)
        .args(["config", "gitoxide.core.shallowFile", "redirected-shallow"])
        .assert()
        .success();
    let linked_head = ctx
        .git_cmd()
        .current_dir(&linked)
        .args(["rev-parse", "HEAD"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();

    fs::write(ctx.repo_path.join(".git/shallow"), linked_head).unwrap();
    ctx.git_cmd()
        .current_dir(&linked)
        .args(["rev-parse", "--is-shallow-repository"])
        .assert()
        .success()
        .stdout("true\n");

    ctx.gherrit_cmd().current_dir(&linked).args(["hook", "pre-push"]).assert().failure().stderr(
        predicate::str::contains(
            "common Git directory's shallow file is nonempty because shallow history omits",
        ),
    );
    assert!(ctx.remote_ref_oid("refs/heads/Gshallow").is_none());
}

#[test]
fn test_effective_shallow_file_from_gix_config_blocks_publication() {
    let ctx = testutil::test_context!().with_remote().with_initial_commit().build();
    ctx.checkout_managed_private("configured-shallow");
    let id = ctx.commit_with_gherrit_id("Configured shallow work");
    ctx.run_git(&["config", "gitoxide.core.shallowFile", "alternate-shallow"]);
    fs::write(ctx.repo_path.join(".git/alternate-shallow"), format!("{}\n", ctx.head_oid()))
        .unwrap();

    ctx.hook_cmd("pre-push").assert().failure().stderr(predicate::str::contains(
        "effective shallow file is nonempty because shallow history omits",
    ));
    assert!(ctx.remote_ref_oid(&format!("refs/heads/{id}")).is_none());
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
    let requests_before = ctx.github().requests().len();
    ctx.inject_failure(testutil::FailureKind::UpdatePr);

    ctx.gherrit_cmd()
        .args(["hook", "pre-push"])
        .assert()
        .failure()
        .stderr(predicates::str::contains("acknowledgement is indeterminate"));
    ctx.assert_failure_consumed();
    let requests = ctx.github().requests();
    assert_eq!(
        &requests[requests_before..],
        [vec![open_head_query(&ctx)], vec![testutil::GraphQlOperation::UpdatePr],],
        "an indeterminate update response must stop without replay or continuation"
    );
    let prs = ctx.github().pull_requests();
    assert_eq!(prs.len(), 1);
    assert_eq!(prs[0].title, "Initial Work");

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
    assert!(ctx.github().requests().is_empty());
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
        .stderr(predicate::str::contains("fatal GraphQL errors"));
    ctx.assert_failure_consumed();
    assert_eq!(ctx.github().requests(), vec![vec![open_head_query(&ctx)]]);
    assert!(ctx.github().pull_requests().is_empty());
    assert!(ctx.recorded_pushes().is_empty());
}

#[test]
fn test_malformed_observed_pull_request_identity_fails_before_git_publication() {
    let ctx = unpublished_managed_commit("feature-malformed-pr-identity");
    let head = ctx.gherrit_id("HEAD").unwrap();
    ctx.github().seed_pull_request_with_invalid_number(0, "Observed title", "", head, "main");
    let refs_before = ctx.remote_refs("refs");

    ctx.hook_cmd("pre-push")
        .assert()
        .failure()
        .stderr(predicate::str::contains("invalid pull request number 0"));

    assert_eq!(ctx.remote_refs("refs"), refs_before);
    assert!(ctx.recorded_pushes().is_empty());
    assert_eq!(ctx.github().requests(), vec![vec![open_head_query(&ctx)]]);
}

#[test]
fn test_pre_push_pr_list_retries_a_transient_http_failure() {
    let ctx = unpublished_managed_commit("feature-pr-list-transient");
    ctx.inject_failure(testutil::FailureKind::QueryHttp(
        testutil::RetryableHttpStatus::ServiceUnavailable,
    ));

    ctx.gherrit_cmd().args(["hook", "pre-push"]).assert().success();

    ctx.assert_failure_consumed();
    assert_eq!(
        ctx.github().requests(),
        [
            vec![open_head_query(&ctx)],
            vec![open_head_query(&ctx)],
            vec![terminal_head_query(&ctx)],
            vec![testutil::GraphQlOperation::CreatePr],
            vec![testutil::GraphQlOperation::UpdatePr],
        ]
    );
    assert_eq!(ctx.github().pull_requests().len(), 1);
}

#[test]
fn test_pre_push_pr_list_stops_after_three_transient_http_retries() {
    let ctx = unpublished_managed_commit("feature-pr-list-retries-exhausted");
    (0..=3).for_each(|_| {
        ctx.inject_failure(testutil::FailureKind::QueryHttp(
            testutil::RetryableHttpStatus::TooManyRequests,
        ));
    });

    ctx.gherrit_cmd()
        .args(["hook", "pre-push"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("Injected QueryHttp(TooManyRequests) failure"));

    ctx.assert_failure_consumed();
    assert_eq!(ctx.github().requests(), vec![vec![open_head_query(&ctx)]; 4]);
    assert!(ctx.github().pull_requests().is_empty());
    assert!(ctx.recorded_pushes().is_empty());
}

#[test]
fn test_pre_push_pr_list_retries_a_response_transport_failure() {
    let ctx = unpublished_managed_commit("feature-pr-list-transport");
    ctx.inject_failure(testutil::FailureKind::QueryTransport);

    ctx.gherrit_cmd().args(["hook", "pre-push"]).assert().success();

    ctx.assert_failure_consumed();
    assert_eq!(
        ctx.github().requests(),
        [
            vec![open_head_query(&ctx)],
            vec![open_head_query(&ctx)],
            vec![terminal_head_query(&ctx)],
            vec![testutil::GraphQlOperation::CreatePr],
            vec![testutil::GraphQlOperation::UpdatePr],
        ]
    );
}

#[test]
fn test_pre_push_pr_list_stops_after_three_response_transport_retries() {
    let ctx = unpublished_managed_commit("feature-pr-list-transport-retries-exhausted");
    (0..=3).for_each(|_| ctx.inject_failure(testutil::FailureKind::QueryTransport));

    ctx.gherrit_cmd().args(["hook", "pre-push"]).assert().failure();

    ctx.assert_failure_consumed();
    assert_eq!(ctx.github().requests(), vec![vec![open_head_query(&ctx)]; 4]);
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
        .stderr(predicate::str::contains("acknowledgement is indeterminate"));
    ctx.assert_failure_consumed();
    assert_eq!(
        ctx.github().requests(),
        [
            vec![open_head_query(&ctx)],
            vec![terminal_head_query(&ctx)],
            vec![testutil::GraphQlOperation::CreatePr],
        ],
        "an indeterminate create response must stop without replay or continuation"
    );
    assert!(ctx.github().pull_requests().is_empty());
    assert_eq!(ctx.recorded_pushes().iter().filter(|push| push.succeeded()).count(), 1);
}

#[test]
fn test_pre_push_pr_create_service_unavailable_is_not_replayed() {
    let ctx = testutil::test_context!()
        .with_remote()
        .with_initial_commit()
        .with_mock_github()
        .with_git_interceptor()
        .build();
    ctx.checkout_managed_private("feature-pr-create-service-unavailable");
    ctx.commit_with_gherrit_id("Work");

    ctx.inject_failure(testutil::FailureKind::CreatePrHttp(
        testutil::RetryableHttpStatus::ServiceUnavailable,
    ));

    ctx.gherrit_cmd()
        .args(["hook", "pre-push"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("indeterminate"));
    ctx.assert_failure_consumed();
    assert_eq!(
        ctx.github().requests(),
        [
            vec![open_head_query(&ctx)],
            vec![terminal_head_query(&ctx)],
            vec![testutil::GraphQlOperation::CreatePr],
        ],
        "a retryable HTTP response must not replay a mutation request"
    );
    assert!(ctx.github().pull_requests().is_empty());
    assert_eq!(ctx.recorded_pushes().iter().filter(|push| push.succeeded()).count(), 1);
}

#[test]
fn test_pre_push_pr_create_redirect_is_not_followed() {
    for redirect in [testutil::RedirectStatus::Temporary, testutil::RedirectStatus::Permanent] {
        let ctx = testutil::test_context!()
            .with_remote()
            .with_initial_commit()
            .with_mock_github()
            .with_git_interceptor()
            .build();
        ctx.checkout_managed_private("feature-pr-create-redirect");
        ctx.commit_with_gherrit_id("Work");
        ctx.inject_failure(testutil::FailureKind::CreatePrRedirect(redirect));

        ctx.gherrit_cmd()
            .args(["hook", "pre-push"])
            .assert()
            .failure()
            .stderr(predicate::str::contains("indeterminate"));

        ctx.assert_failure_consumed();
        assert_eq!(
            ctx.github()
                .requests()
                .iter()
                .filter(|request| request.contains(&testutil::GraphQlOperation::CreatePr))
                .count(),
            1,
            "the original mutation endpoint must receive exactly one request for {redirect:?}"
        );
        assert_eq!(
            ctx.github().redirect_trap_requests(),
            0,
            "the client must not follow {redirect:?} mutation redirects"
        );
        assert!(ctx.github().pull_requests().is_empty());
    }
}

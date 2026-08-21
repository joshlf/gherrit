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

fn local_ref_oid(ctx: &testutil::TestContext, repository: &Path, ref_name: &str) -> Option<String> {
    let output = ctx
        .git_cmd()
        .current_dir(repository)
        .args(["rev-parse", "--verify", "--quiet", ref_name])
        .output()
        .expect("failed to inspect local ref");
    match output.status.code() {
        Some(0) => Some(String::from_utf8(output.stdout).unwrap().trim().to_owned()),
        Some(1) => None,
        code => panic!("git rev-parse failed with exit code {code:?}"),
    }
}

fn configured_origin(ctx: &testutil::TestContext) -> String {
    let output = ctx
        .git_cmd()
        .args(["remote", "get-url", "origin"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    String::from_utf8(output).unwrap().trim().to_owned()
}

#[test]
fn test_full_stack_lifecycle_mocked() {
    let ctx = testutil::test_context!()
        .with_remote()
        .with_initial_commit()
        .with_mock_github()
        .with_git_interceptor()
        .build();

    // Setup: Create 'main' and a feature branch
    ctx.checkout_managed_private("feature-stack");

    ctx.commit_with_gherrit_id("Commit A");
    let commit_a_id = ctx.gherrit_id("HEAD").unwrap();
    let commit_a_oid = ctx.head_oid();

    ctx.commit_with_gherrit_id("Commit B");
    let commit_b_id = ctx.gherrit_id("HEAD").unwrap();
    let commit_b_oid = ctx.head_oid();

    // Trigger Pre-Push Hook (Simulate 'git push'). We call the hook directly
    // because simulating a real 'git push' that calls the hook recursively is
    // complex in a test env.
    testutil::assert_success_snapshot!(
        ctx,
        ctx.gherrit_cmd().args(["hook", "pre-push"]),
        "full_stack_lifecycle_push"
    );

    // Verify Side Effects (Mock Only)
    testutil::assert_pr_snapshot!(ctx, "full_stack_lifecycle_state");

    assert!(
        ctx.recorded_pushes().iter().any(|push| push.succeeded()),
        "Expected a successful push"
    );
    assert_eq!(
        ctx.remote_ref_oid(&format!("refs/heads/{commit_a_id}")).as_deref(),
        Some(commit_a_oid.as_str())
    );
    assert_eq!(
        ctx.remote_ref_oid(&format!("refs/heads/{commit_b_id}")).as_deref(),
        Some(commit_b_oid.as_str())
    );

    let heads = ctx.recorded_git_invocations(testutil::GitOperation::LsRemoteHeads);
    let versions = ctx.recorded_git_invocations(testutil::GitOperation::LsRemoteActiveVersions);
    assert_eq!(heads.len(), 1, "one publication attempt has one global head observation");
    assert_eq!(versions.len(), 1, "ordinary stacks use one batched active-history observation");
    assert!(
        ctx.recorded_git_invocations(testutil::GitOperation::LsRemoteOther).is_empty(),
        "publication must not issue any other remote-ref query"
    );
    assert_eq!(
        heads[0],
        [
            "git",
            "--no-replace-objects",
            "--config-env=remote.gherrit-publication.url=GHERRIT_PRIVATE_PUSH_DESTINATION",
            "--config-env=remote.gherrit-publication.pushurl=GHERRIT_PRIVATE_PUSH_DESTINATION",
            "-c",
            "http.followRedirects=false",
            "ls-remote",
            "--quiet",
            "--symref",
            "--",
            "gherrit-publication",
            "HEAD",
            "refs/heads/*",
            "refs/tags/gherrit",
        ]
        .map(ToOwned::to_owned)
    );
    let expected_versions = [
        "git",
        "--no-replace-objects",
        "--config-env=remote.gherrit-publication.url=GHERRIT_PRIVATE_PUSH_DESTINATION",
        "--config-env=remote.gherrit-publication.pushurl=GHERRIT_PRIVATE_PUSH_DESTINATION",
        "-c",
        "http.followRedirects=false",
        "ls-remote",
        "--quiet",
        "--",
        "gherrit-publication",
    ]
    .map(ToOwned::to_owned)
    .into_iter()
    .chain([
        format!("refs/tags/gherrit/{commit_a_id}"),
        format!("refs/tags/gherrit/{commit_a_id}/*"),
        format!("refs/tags/gherrit/{commit_b_id}"),
        format!("refs/tags/gherrit/{commit_b_id}/*"),
    ])
    .collect::<Vec<_>>();
    assert_eq!(versions[0], expected_versions);
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
        [(stack_id.as_str(), "main"), ("Gmerge", stack_id.as_str())]
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
        "Document an example\n\ngherrit-pr-id: Gexample\n\nExplanation.\n\ngherrit-pr-id: Greal",
    ]);

    ctx.hook_cmd("pre-push").assert().success();

    assert!(ctx.remote_ref_oid("refs/heads/Gexample").is_none());
    assert_eq!(ctx.remote_ref_oid("refs/heads/Greal").as_deref(), Some(ctx.head_oid().as_str()));
    let pull_requests = ctx.github().pull_requests();
    assert_eq!(pull_requests.len(), 1);
    let body = pull_requests[0].body.as_deref().expect("created PR body");
    assert!(body.contains("gherrit-pr-id: Gexample"));
    assert!(!body.contains("\ngherrit-pr-id: Greal\n"));
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
    assert_eq!(pull_requests[0].title.as_deref(), Some("Literal commit"));
    assert_eq!(
        &ctx.recorded_pushes()[0].arguments()[..7],
        [
            "git",
            "--no-replace-objects",
            "--config-env=remote.gherrit-publication.url=GHERRIT_PRIVATE_PUSH_DESTINATION",
            "--config-env=remote.gherrit-publication.pushurl=GHERRIT_PRIVATE_PUSH_DESTINATION",
            "-c",
            "http.followRedirects=false",
            "push",
        ]
    );
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
        assert_eq!(pull_requests[0].title.as_deref(), Some("Locally available work"));
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
fn test_version_increment() {
    let ctx = testutil::test_context!()
        .with_remote()
        .with_initial_commit()
        .with_mock_github()
        .with_git_interceptor()
        .build();

    // Create feature branch
    ctx.checkout_managed_private("feat-versioning");
    ctx.commit_with_gherrit_id("Feature Commit");
    let gherrit_id = ctx.gherrit_id("HEAD").unwrap();
    let v1_oid = ctx.head_oid();
    let managed_ref = format!("refs/heads/{gherrit_id}");
    let v1_ref = format!("refs/tags/gherrit/{gherrit_id}/v1");
    let v2_ref = format!("refs/tags/gherrit/{gherrit_id}/v2");

    // Push 1 (v1)
    testutil::assert_success_snapshot!(ctx, ctx.hook_cmd("pre-push"), "version_increment_v1");

    assert_eq!(ctx.remote_ref_oid(&managed_ref).as_deref(), Some(v1_oid.as_str()));
    assert_eq!(ctx.remote_ref_oid(&v1_ref).as_deref(), Some(v1_oid.as_str()));
    assert_eq!(local_ref_oid(&ctx, &ctx.repo_path, &v1_ref), None);

    let bogus_local_ref = format!("refs/tags/gherrit/{gherrit_id}/v999");
    ctx.git_cmd().args(["update-ref", &bogus_local_ref, &v1_oid]).assert().success();

    // Amend commit (modifies SHA, keeps Change-ID)
    ctx.amend();
    let v2_oid = ctx.head_oid();
    assert_ne!(v2_oid, v1_oid);

    // Push 2 (v2)
    testutil::assert_success_snapshot!(ctx, ctx.hook_cmd("pre-push"), "version_increment_v2");

    assert_eq!(ctx.remote_ref_oid(&managed_ref).as_deref(), Some(v2_oid.as_str()));
    assert_eq!(ctx.remote_ref_oid(&v1_ref).as_deref(), Some(v1_oid.as_str()));
    assert_eq!(ctx.remote_ref_oid(&v2_ref).as_deref(), Some(v2_oid.as_str()));
    assert_eq!(ctx.remote_ref_oid(&bogus_local_ref), None);
    assert_eq!(local_ref_oid(&ctx, &ctx.repo_path, &v2_ref), None);
    assert_eq!(
        local_ref_oid(&ctx, &ctx.repo_path, &bogus_local_ref).as_deref(),
        Some(v1_oid.as_str())
    );

    let pushes = ctx.recorded_pushes();
    assert_eq!(pushes.len(), 2, "Expected one push per published version");
    assert!(
        pushes[1].arguments().iter().all(|argument| !argument.contains(&v1_ref)),
        "The second push must not attempt to republish the immutable v1 tag: {:?}",
        pushes[1].arguments()
    );
}

#[derive(Clone, Copy)]
enum ConcurrentLeaseChange {
    ManagedHead,
    NextVersionTag,
}

fn assert_real_git_rejects_changed_tuple_lease(change: ConcurrentLeaseChange) {
    let ctx = testutil::test_context!()
        .with_remote()
        .with_initial_commit()
        .with_mock_github()
        .with_git_interceptor()
        .build();
    ctx.checkout_managed_private("lease-race");
    let id = ctx.commit_with_gherrit_id("Publish v1");
    let v1_oid = ctx.head_oid();
    let managed_ref = format!("refs/heads/{id}");
    let v1_ref = format!("refs/tags/gherrit/{id}/v1");
    let v2_ref = format!("refs/tags/gherrit/{id}/v2");

    ctx.hook_cmd("pre-push").assert().success();
    let request_count = ctx.github().requests().len();
    let main_oid = ctx.remote_ref_oid("refs/heads/main").expect("remote main");
    ctx.amend();

    let raced_ref = match change {
        ConcurrentLeaseChange::ManagedHead => &managed_ref,
        ConcurrentLeaseChange::NextVersionTag => &v2_ref,
    };
    ctx.update_remote_ref_before_next_passthrough_push(raced_ref, "refs/heads/main");

    ctx.hook_cmd("pre-push")
        .assert()
        .failure()
        .stderr(predicates::str::contains("`git push` failed"));

    let pushes = ctx.recorded_pushes();
    assert_eq!(pushes.len(), 2);
    assert!(pushes[0].succeeded());
    assert!(!pushes[1].succeeded());
    let arguments = pushes[1].arguments();
    assert!(arguments.iter().any(|argument| argument == "--atomic"));
    assert!(
        arguments
            .iter()
            .any(|argument| { argument == &format!("--force-with-lease={managed_ref}:{v1_oid}") })
    );
    assert!(arguments.iter().any(|argument| argument == &format!("--force-with-lease={v2_ref}:")));

    assert_eq!(ctx.remote_ref_oid(&v1_ref).as_deref(), Some(v1_oid.as_str()));
    match change {
        ConcurrentLeaseChange::ManagedHead => {
            assert_eq!(ctx.remote_ref_oid(&managed_ref).as_deref(), Some(main_oid.as_str()));
            assert_eq!(ctx.remote_ref_oid(&v2_ref), None);
        }
        ConcurrentLeaseChange::NextVersionTag => {
            assert_eq!(ctx.remote_ref_oid(&managed_ref).as_deref(), Some(v1_oid.as_str()));
            assert_eq!(ctx.remote_ref_oid(&v2_ref).as_deref(), Some(main_oid.as_str()));
        }
    }
    assert_eq!(
        &ctx.github().requests()[request_count..],
        &[vec![testutil::GraphQlOperation::Query]],
        "a rejected Git tuple must not be followed by a GitHub mutation"
    );
}

#[test]
fn real_branch_lease_rejection_preserves_the_atomic_tuple() {
    assert_real_git_rejects_changed_tuple_lease(ConcurrentLeaseChange::ManagedHead);
}

#[test]
fn real_tag_creation_lease_rejection_preserves_the_atomic_tuple() {
    assert_real_git_rejects_changed_tuple_lease(ConcurrentLeaseChange::NextVersionTag);
}

#[test]
fn unchanged_retry_reconciles_prs_without_publishing_another_version() {
    let ctx = testutil::test_context!()
        .with_remote()
        .with_initial_commit()
        .with_mock_github()
        .with_git_interceptor()
        .build();
    ctx.checkout_managed_private("unchanged-retry");
    let id = ctx.commit_with_gherrit_id("Publish once");

    ctx.hook_cmd("pre-push").assert().success();
    let request_count = ctx.github().requests().len();
    assert_eq!(ctx.recorded_pushes().len(), 1);

    ctx.hook_cmd("pre-push").assert().success();

    assert_eq!(ctx.recorded_pushes().len(), 1, "an unchanged head must not create v2");
    assert_eq!(ctx.remote_ref_oid(&format!("refs/tags/gherrit/{id}/v2")), None);
    assert_eq!(
        &ctx.github().requests()[request_count..],
        &[vec![testutil::GraphQlOperation::Query]],
        "the retry must still observe and reconcile its pull request"
    );
}

#[test]
fn mixed_changed_and_unchanged_stack_publishes_only_the_changed_change() {
    let ctx = testutil::test_context!()
        .with_remote()
        .with_initial_commit()
        .with_mock_github()
        .with_git_interceptor()
        .build();
    ctx.checkout_managed_private("mixed-publication");
    let unchanged = ctx.commit_with_gherrit_id("Unchanged root");
    let changed = ctx.commit_with_gherrit_id("Changed tip");

    ctx.hook_cmd("pre-push").assert().success();
    ctx.amend();
    ctx.hook_cmd("pre-push").assert().success();

    let pushes = ctx.recorded_pushes();
    assert_eq!(pushes.len(), 2);
    let second = pushes[1].arguments();
    assert!(second.iter().any(|argument| argument.contains(&format!("/{changed}/v2"))));
    assert!(second.iter().all(|argument| !argument.contains(&format!("/{unchanged}/v2"))));
    assert_eq!(ctx.remote_ref_oid(&format!("refs/tags/gherrit/{unchanged}/v2")), None);
    assert!(ctx.remote_ref_oid(&format!("refs/tags/gherrit/{changed}/v2")).is_some());

    let prs = ctx.github().pull_requests();
    let unchanged_body = prs
        .iter()
        .find(|pr| pr.head == unchanged)
        .and_then(|pr| pr.body.as_deref())
        .expect("unchanged pull request body");
    let changed_body = prs
        .iter()
        .find(|pr| pr.head == changed)
        .and_then(|pr| pr.body.as_deref())
        .expect("changed pull request body");
    assert!(!unchanged_body.contains("Latest Update:"));
    assert!(changed_body.contains("Latest Update:** v2"));
}

#[test]
fn complete_stack_planning_precedes_every_ref_write() {
    let ctx = testutil::test_context!()
        .with_remote()
        .with_initial_commit()
        .with_mock_github()
        .with_git_interceptor()
        .build();
    ctx.checkout_managed_private("plan-before-write");
    ctx.commit_with_gherrit_id("Valid first change");
    let invalid = ctx.commit_with_gherrit_id("Invalid later change");
    let invalid_head = format!("refs/heads/{invalid}");
    let invalid_v2 = format!("refs/tags/gherrit/{invalid}/v2");
    ctx.remote_git_cmd().args(["update-ref", &invalid_head, "refs/heads/main"]).assert().success();
    ctx.remote_git_cmd().args(["update-ref", &invalid_v2, "refs/heads/main"]).assert().success();
    let refs_before = ctx.remote_refs("refs");

    ctx.hook_cmd("pre-push")
        .assert()
        .failure()
        .stderr(predicates::str::contains("noncontiguous version tags"));

    assert_eq!(ctx.remote_refs("refs"), refs_before);
    assert!(ctx.recorded_pushes().is_empty());
    assert!(ctx.github().requests().is_empty());
}

#[test]
fn inactive_version_history_is_not_requested_or_validated() {
    let ctx = testutil::test_context!()
        .with_remote()
        .with_initial_commit()
        .with_mock_github()
        .with_git_interceptor()
        .build();
    let inactive = "Ginactive";
    let malformed_inactive = format!("refs/tags/gherrit/{inactive}/v01");
    ctx.remote_git_cmd()
        .args(["update-ref", &malformed_inactive, "refs/heads/main"])
        .assert()
        .success();
    ctx.checkout_managed_private("active-history-only");
    let active = ctx.commit_with_gherrit_id("Active change");

    ctx.hook_cmd("pre-push").assert().success();

    let queries = ctx.recorded_git_invocations(testutil::GitOperation::LsRemoteActiveVersions);
    assert_eq!(queries.len(), 1);
    assert!(queries[0].iter().all(|argument| !argument.contains(inactive)));
    assert!(queries[0].iter().any(|argument| argument == &format!("refs/tags/gherrit/{active}")));
    assert_eq!(
        ctx.remote_ref_oid(&malformed_inactive).as_deref(),
        ctx.remote_ref_oid("refs/heads/main").as_deref()
    );
    assert!(ctx.remote_ref_oid(&format!("refs/tags/gherrit/{active}/v1")).is_some());
}

#[test]
fn fresh_clone_without_local_tags_continues_remote_version_history() {
    let ctx = testutil::test_context!()
        .with_remote()
        .with_initial_commit()
        .with_mock_github()
        .with_git_interceptor()
        .build();
    ctx.checkout_managed_private("original-publication");
    let id = ctx.commit_with_gherrit_id("Clone-independent versioning");
    ctx.hook_cmd("pre-push").assert().success();

    let origin = configured_origin(&ctx);
    let fresh = ctx.dir.path().join("fresh-clone");
    ctx.git_cmd()
        .current_dir(ctx.dir.path())
        .args(["clone", "--no-tags"])
        .arg(&origin)
        .arg(&fresh)
        .assert()
        .success();
    let run_fresh = |arguments: &[&str]| {
        ctx.git_cmd().current_dir(&fresh).args(arguments).assert().success();
    };
    run_fresh(&["config", "user.email", "test@example.com"]);
    run_fresh(&["config", "user.name", "Test User"]);
    run_fresh(&["checkout", "-b", "fresh-publication", &format!("origin/{id}")]);
    for (suffix, value) in [
        ("gherritManaged", testutil::MANAGED_PRIVATE),
        ("pushRemote", "."),
        ("remote", "."),
        ("merge", "refs/heads/fresh-publication"),
    ] {
        run_fresh(&["config", &format!("branch.fresh-publication.{suffix}"), value]);
    }
    assert_eq!(local_ref_oid(&ctx, &fresh, &format!("refs/tags/gherrit/{id}/v1")), None);
    run_fresh(&["commit", "--amend", "--allow-empty", "--no-verify", "--no-edit"]);
    let fresh_head = local_ref_oid(&ctx, &fresh, "HEAD").unwrap();

    ctx.gherrit_cmd_at(&fresh).args(["hook", "pre-push"]).assert().success();

    assert_eq!(
        ctx.remote_ref_oid(&format!("refs/tags/gherrit/{id}/v2")).as_deref(),
        Some(fresh_head.as_str())
    );
    assert_eq!(local_ref_oid(&ctx, &fresh, &format!("refs/tags/gherrit/{id}/v2")), None);
}

#[test]
fn test_remote_version_history_is_authoritative() {
    let ctx = testutil::test_context!()
        .with_remote()
        .with_initial_commit()
        .with_mock_github()
        .with_git_interceptor()
        .build();

    // Initial setup
    ctx.checkout_managed_private("feature-conflict");
    ctx.commit_with_gherrit_id("Commit V1");

    // Push V1
    testutil::assert_success_snapshot!(ctx, ctx.hook_cmd("pre-push"), "optimistic_locking_v1");

    let gherrit_id = ctx.gherrit_id("HEAD").unwrap();
    let managed_ref = format!("refs/heads/{gherrit_id}");
    let pushed_oid = ctx.remote_ref_oid(&managed_ref).expect("Managed ref was not pushed");

    // Add another immutable remote history record without creating any local
    // tag. The next attempt must continue after the remote record rather than
    // trying to reuse its version number.
    let tag_name = format!("gherrit/{}/v2", gherrit_id);

    // Create tag pointing to the branch we just pushed
    ctx.remote_git_cmd()
        .args(["tag", &tag_name, &format!("refs/heads/{}", gherrit_id)])
        .assert()
        .success();

    // Create local commit for V2 (modify to ensure new hash).
    // Note: We change the message to guarantee a different SHA even if running
    // quickly. We MUST preserve the Change-ID to simulate an update to the SAME
    // stack.
    let new_msg = format!("Commit V1 (Amended)\n\ngherrit-pr-id: {}", gherrit_id);
    ctx.amend_with_message(&new_msg);

    ctx.hook_cmd("pre-push").assert().success();

    let pushes = ctx.recorded_pushes();
    assert_eq!(pushes.len(), 2, "Expected one push for each local commit object");
    assert!(pushes.iter().all(testutil::PushRecord::succeeded));
    assert_eq!(
        ctx.remote_ref_oid(&managed_ref).as_deref(),
        Some(ctx.head_oid().as_str()),
        "the managed head must advance"
    );
    assert_eq!(
        ctx.remote_ref_oid(&format!("refs/tags/gherrit/{gherrit_id}/v2")).as_deref(),
        Some(pushed_oid.as_str())
    );
    assert_eq!(
        ctx.remote_ref_oid(&format!("refs/tags/gherrit/{gherrit_id}/v3")).as_deref(),
        Some(ctx.head_oid().as_str())
    );
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
        1,
        "GraphQL backoff must not alter the independent Git publication batch"
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

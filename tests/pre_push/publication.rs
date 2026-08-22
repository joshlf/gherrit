use std::{collections::BTreeSet, fs, path::Path};

const ACTIVE_VERSION_QUERY_BUDGET_BYTES: usize = 16 * 1024;
const MANY_ACTIVE_ID_COUNT: usize = 60;
const MANY_ACTIVE_ID_LEN: usize = 120;

fn commit_many_active_changes(ctx: &testutil::TestContext) -> Vec<String> {
    let ids = (0..MANY_ACTIVE_ID_COUNT)
        .map(|index| {
            let prefix = format!("G{index:03}");
            format!("{prefix}{}", "a".repeat(MANY_ACTIVE_ID_LEN - prefix.len()))
        })
        .collect::<Vec<_>>();

    let pattern_bytes = ids
        .iter()
        .flat_map(|id| {
            let root = format!("refs/tags/gherrit/{id}");
            [root.len() + 1, root.len() + 3]
        })
        .sum::<usize>();
    assert!(pattern_bytes > ACTIVE_VERSION_QUERY_BUDGET_BYTES);

    ids.iter().enumerate().for_each(|(index, id)| {
        ctx.commit_with_explicit_gherrit_id(&format!("Change {index}"), id);
    });
    ids
}

fn active_version_patterns(ids: &[String]) -> Vec<String> {
    ids.iter()
        .flat_map(|id| {
            let root = format!("refs/tags/gherrit/{id}");
            [root.clone(), format!("{root}/*")]
        })
        .collect()
}

fn observed_active_version_patterns(queries: &[Vec<String>]) -> Vec<String> {
    queries
        .iter()
        .flatten()
        .filter(|argument| argument.starts_with("refs/tags/gherrit/"))
        .cloned()
        .collect()
}

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

fn local_ref_oid(ctx: &testutil::TestContext, ref_name: &str) -> Option<String> {
    let output = ctx
        .git_cmd()
        .args(["rev-parse", "--verify", "--quiet", ref_name])
        .output()
        .expect("failed to inspect local ref");
    match output.status.code() {
        Some(0) => Some(String::from_utf8(output.stdout).unwrap().trim().to_owned()),
        Some(1) => None,
        code => panic!("git rev-parse failed with exit code {code:?}"),
    }
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
    assert_eq!(heads.len(), 1, "one attempt has one global head observation");
    assert_eq!(versions.len(), 1, "an ordinary stack has one exact history observation");
    assert!(ctx.recorded_git_invocations(testutil::GitOperation::LsRemoteOther).is_empty());
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
    assert!(
        versions[0].iter().any(|argument| argument == &format!("refs/tags/gherrit/{commit_a_id}"))
    );
    assert!(
        versions[0]
            .iter()
            .any(|argument| argument == &format!("refs/tags/gherrit/{commit_b_id}/*"))
    );
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
    for pull_request in pull_requests {
        assert_eq!(
            ctx.remote_ref_oid(&format!("refs/heads/{}", pull_request.head)).as_deref(),
            Some(pull_request.head_oid.as_str())
        );
        assert_eq!(
            ctx.remote_ref_oid(&format!("refs/heads/{}", pull_request.base)).as_deref(),
            Some(pull_request.base_oid.as_str())
        );
    }
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
        &ctx.recorded_pushes()[0].arguments()[..9],
        [
            "git",
            "--no-replace-objects",
            "--config-env=remote.gherrit-publication.url=GHERRIT_PRIVATE_PUSH_DESTINATION",
            "--config-env=remote.gherrit-publication.pushurl=GHERRIT_PRIVATE_PUSH_DESTINATION",
            "-c",
            "http.followRedirects=false",
            "-c",
            "push.pushOption=",
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
    let bogus_local_ref = format!("refs/tags/gherrit/{gherrit_id}/v999");

    // Push 1 (v1)
    testutil::assert_success_snapshot!(ctx, ctx.hook_cmd("pre-push"), "version_increment_v1");

    assert_eq!(ctx.remote_ref_oid(&managed_ref).as_deref(), Some(v1_oid.as_str()));
    assert_eq!(ctx.remote_ref_oid(&v1_ref).as_deref(), Some(v1_oid.as_str()));
    assert_eq!(local_ref_oid(&ctx, &v1_ref), None, "remote version state is not persisted locally");

    // Local tags are neither authority nor a cache. A stale high tag must not
    // affect the version selected from the push destination.
    ctx.git_cmd().args(["update-ref", &bogus_local_ref, "HEAD"]).assert().success();

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
    assert_eq!(local_ref_oid(&ctx, &v1_ref), None);
    assert_eq!(local_ref_oid(&ctx, &v2_ref), None);
    assert_eq!(local_ref_oid(&ctx, &bogus_local_ref).as_deref(), Some(v1_oid.as_str()));

    let pushes = ctx.recorded_pushes();
    assert_eq!(pushes.len(), 2, "Expected one push per published version");
    assert!(
        pushes[1].arguments().iter().all(|argument| !argument.contains(&v1_ref)),
        "The second push must not attempt to republish the immutable v1 tag: {:?}",
        pushes[1].arguments()
    );

    // Retrying an already-published stack still reconciles GitHub but does
    // not synthesize a new immutable version.
    ctx.hook_cmd("pre-push").assert().success();
    assert_eq!(ctx.recorded_pushes().len(), 2);
    assert_eq!(ctx.remote_ref_oid(&v2_ref).as_deref(), Some(v2_oid.as_str()));
}

#[test]
fn fresh_clone_without_tags_continues_remote_history() {
    let ctx = testutil::test_context!()
        .with_remote()
        .with_initial_commit()
        .with_mock_github()
        .with_git_interceptor()
        .build();
    ctx.checkout_managed_private("original-clone");
    ctx.commit_with_explicit_gherrit_id("Version one", "Gfresh");
    ctx.hook_cmd("pre-push").assert().success();
    let v1_oid = ctx.head_oid();

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
    let fresh = ctx.dir.path().join("fresh-clone");
    ctx.git_cmd_at(ctx.dir.path())
        .args(["clone", "--no-tags", origin.trim()])
        .arg(&fresh)
        .assert()
        .success();
    ctx.git_cmd_at(&fresh).args(["checkout", "-b", "fresh-feature"]).assert().success();
    for (suffix, value) in [
        ("gherritManaged", testutil::MANAGED_PRIVATE),
        ("pushRemote", "."),
        ("remote", "."),
        ("merge", "refs/heads/fresh-feature"),
    ] {
        ctx.git_cmd_at(&fresh)
            .args(["config", &format!("branch.fresh-feature.{suffix}"), value])
            .assert()
            .success();
    }
    ctx.git_cmd_at(&fresh)
        .args([
            "commit",
            "--allow-empty",
            "--no-verify",
            "-m",
            "Version two\n\ngherrit-pr-id: Gfresh",
        ])
        .assert()
        .success();
    assert_eq!(
        ctx.git_cmd_at(&fresh)
            .args(["show-ref", "--verify", "--quiet", "refs/tags/gherrit/Gfresh/v1"])
            .assert()
            .get_output()
            .status
            .code(),
        Some(1)
    );
    let v2_oid = String::from_utf8(
        ctx.git_cmd_at(&fresh)
            .args(["rev-parse", "HEAD"])
            .assert()
            .success()
            .get_output()
            .stdout
            .clone(),
    )
    .unwrap();
    let v2_oid = v2_oid.trim();

    ctx.gherrit_cmd_at(&fresh).args(["hook", "pre-push"]).assert().success();

    assert_ne!(v2_oid, v1_oid);
    assert_eq!(ctx.remote_ref_oid("refs/heads/Gfresh").as_deref(), Some(v2_oid));
    assert_eq!(ctx.remote_ref_oid("refs/tags/gherrit/Gfresh/v1").as_deref(), Some(v1_oid.as_str()));
    assert_eq!(ctx.remote_ref_oid("refs/tags/gherrit/Gfresh/v2").as_deref(), Some(v2_oid));
}

#[test]
fn remote_history_selects_the_next_version() {
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

    // Create v2 on the remote without creating a corresponding local tag. The
    // next publication must be v3. In a bare repository, refs can be created
    // directly.
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

    // The remote history, not missing local tags, selects v3.
    ctx.hook_cmd("pre-push").assert().success();

    let pushes = ctx.recorded_pushes();
    assert_eq!(pushes.len(), 2);
    assert!(pushes.iter().all(testutil::PushRecord::succeeded));
    assert_eq!(ctx.remote_ref_oid(&managed_ref).as_deref(), Some(ctx.head_oid().as_str()));
    assert_eq!(
        ctx.remote_ref_oid(&format!("refs/tags/gherrit/{gherrit_id}/v3")).as_deref(),
        Some(ctx.head_oid().as_str())
    );
    assert_ne!(ctx.head_oid(), pushed_oid);
}

#[test]
fn concurrent_head_change_fails_the_atomic_branch_and_tag_leases() {
    let ctx = testutil::test_context!()
        .with_remote()
        .with_initial_commit()
        .with_mock_github()
        .with_git_interceptor()
        .build();
    ctx.checkout_managed_private("feature-lease-race");
    ctx.commit_with_gherrit_id("Commit V1");
    ctx.hook_cmd("pre-push").assert().success();

    let gherrit_id = ctx.gherrit_id("HEAD").unwrap();
    let managed_ref = format!("refs/heads/{gherrit_id}");
    let v1_ref = format!("refs/tags/gherrit/{gherrit_id}/v1");
    let v2_ref = format!("refs/tags/gherrit/{gherrit_id}/v2");
    let v1_oid = ctx.head_oid();
    ctx.amend();
    ctx.update_remote_ref_before_push(&managed_ref, "refs/heads/main");
    let requests_before = ctx.github().requests().len();

    ctx.hook_cmd("pre-push")
        .assert()
        .failure()
        .stderr(predicates::str::contains("Could not acknowledge `git push`"));

    let pushes = ctx.recorded_pushes();
    assert_eq!(pushes.len(), 2);
    assert!(pushes[0].succeeded());
    assert!(!pushes[1].succeeded());
    assert!(pushes[1].arguments().iter().any(|argument| argument == "--atomic"));
    assert!(pushes[1].arguments().contains(&format!("--force-with-lease={managed_ref}:{v1_oid}")));
    assert!(pushes[1].arguments().contains(&format!("--force-with-lease={v2_ref}:")));
    assert_ne!(ctx.remote_ref_oid(&managed_ref).as_deref(), Some(ctx.head_oid().as_str()));
    assert_eq!(ctx.remote_ref_oid(&v1_ref).as_deref(), Some(v1_oid.as_str()));
    assert_eq!(ctx.remote_ref_oid(&v2_ref), None);
    let requests = ctx.github().requests();
    let retry_requests = &requests[requests_before..];
    assert!(!retry_requests.is_empty());
    assert!(
        retry_requests
            .iter()
            .flatten()
            .all(|operation| *operation == testutil::GraphQlOperation::Query)
    );
}

#[test]
fn concurrent_tag_creation_fails_the_atomic_branch_and_tag_leases() {
    let ctx = testutil::test_context!()
        .with_remote()
        .with_initial_commit()
        .with_mock_github()
        .with_git_interceptor()
        .build();
    ctx.checkout_managed_private("feature-tag-lease-race");
    ctx.commit_with_gherrit_id("Commit V1");
    ctx.hook_cmd("pre-push").assert().success();

    let gherrit_id = ctx.gherrit_id("HEAD").unwrap();
    let managed_ref = format!("refs/heads/{gherrit_id}");
    let v1_ref = format!("refs/tags/gherrit/{gherrit_id}/v1");
    let v2_ref = format!("refs/tags/gherrit/{gherrit_id}/v2");
    let v1_oid = ctx.head_oid();
    let concurrent_oid = ctx.remote_ref_oid("refs/heads/main").unwrap();
    ctx.amend();
    ctx.update_remote_ref_before_push(&v2_ref, "refs/heads/main");
    let requests_before = ctx.github().requests().len();

    ctx.hook_cmd("pre-push")
        .assert()
        .failure()
        .stderr(predicates::str::contains("Could not acknowledge `git push`"));

    let pushes = ctx.recorded_pushes();
    assert_eq!(pushes.len(), 2);
    assert!(pushes[0].succeeded());
    assert!(!pushes[1].succeeded());
    assert!(pushes[1].arguments().iter().any(|argument| argument == "--atomic"));
    assert!(pushes[1].arguments().contains(&format!("--force-with-lease={managed_ref}:{v1_oid}")));
    assert!(pushes[1].arguments().contains(&format!("--force-with-lease={v2_ref}:")));
    assert_eq!(ctx.remote_ref_oid(&managed_ref).as_deref(), Some(v1_oid.as_str()));
    assert_eq!(ctx.remote_ref_oid(&v1_ref).as_deref(), Some(v1_oid.as_str()));
    assert_eq!(ctx.remote_ref_oid(&v2_ref).as_deref(), Some(concurrent_oid.as_str()));
    let requests = ctx.github().requests();
    let retry_requests = &requests[requests_before..];
    assert!(!retry_requests.is_empty());
    assert!(
        retry_requests
            .iter()
            .flatten()
            .all(|operation| *operation == testutil::GraphQlOperation::Query)
    );
}

fn assert_lost_push_receipt_stops_before_github_mutation(replacement: &'static str) {
    let ctx = testutil::test_context!()
        .with_remote()
        .with_initial_commit()
        .with_mock_github()
        .with_git_interceptor()
        .build();
    ctx.checkout_managed_private("lost-push-receipt");
    let id = ctx.commit_with_gherrit_id("Publish despite a lost receipt");
    let head = ctx.head_oid();
    ctx.replace_push_stdout_after_passthrough(replacement);

    ctx.hook_cmd("pre-push")
        .assert()
        .failure()
        .stderr(predicates::str::contains("Could not acknowledge `git push`"));

    ctx.assert_failure_consumed();
    assert_eq!(ctx.remote_ref_oid(&format!("refs/heads/{id}")).as_deref(), Some(head.as_str()));
    assert_eq!(
        ctx.remote_ref_oid(&format!("refs/tags/gherrit/{id}/v1")).as_deref(),
        Some(head.as_str())
    );
    assert!(ctx.github().pull_requests().is_empty());
    assert!(
        ctx.github()
            .requests()
            .iter()
            .flatten()
            .all(|operation| { *operation == testutil::GraphQlOperation::Query })
    );

    ctx.hook_cmd("pre-push").assert().success();
    assert_eq!(ctx.recorded_pushes().len(), 1, "retry must observe instead of replaying");
    assert_eq!(ctx.github().pull_requests().len(), 1);
}

#[test]
fn a_successful_push_with_a_dropped_receipt_is_indeterminate() {
    assert_lost_push_receipt_stops_before_github_mutation("");
}

#[test]
fn a_successful_push_with_a_malformed_receipt_is_indeterminate() {
    assert_lost_push_receipt_stops_before_github_mutation("To \nDone\n");
}

#[test]
fn a_complete_crlf_receipt_acknowledges_a_real_push() {
    let ctx = testutil::test_context!()
        .with_remote()
        .with_initial_commit()
        .with_mock_github()
        .with_git_interceptor()
        .build();
    ctx.checkout_managed_private("crlf-push-receipt");
    ctx.run_git(&[
        "commit",
        "--allow-empty",
        "--no-verify",
        "-m",
        "Accept CRLF receipts\n\ngherrit-pr-id: Gcrlf",
    ]);
    let head = ctx.head_oid();
    ctx.convert_push_stdout_to_crlf_after_passthrough();

    ctx.hook_cmd("pre-push").assert().success();
    ctx.assert_failure_consumed();
    assert_eq!(ctx.remote_ref_oid("refs/heads/Gcrlf").as_deref(), Some(head.as_str()));
    assert_eq!(ctx.remote_ref_oid("refs/tags/gherrit/Gcrlf/v1").as_deref(), Some(head.as_str()));
    assert_eq!(ctx.github().pull_requests().len(), 1);
}

#[test]
fn command_scoped_empty_push_option_sends_no_inherited_push_options() {
    let ctx = testutil::test_context!()
        .with_remote()
        .with_initial_commit()
        .with_mock_github()
        .with_git_interceptor()
        .build();
    ctx.remote_git_cmd()
        .args(["config", "receive.advertisePushOptions", "true"])
        .assert()
        .success();
    let hook = ctx.remote_path().join("hooks/pre-receive");
    fs::write(
        &hook,
        "#!/bin/sh\nset -eu\nprintf '%s\\n' \"${GIT_PUSH_OPTION_COUNT-unset}\" >push-option-count\n",
    )
    .unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        let mut permissions = fs::metadata(&hook).unwrap().permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&hook, permissions).unwrap();
    }
    ctx.set_config("push.pushOption", Some("untrusted"));
    ctx.checkout_managed_private("clear-push-option");
    ctx.commit_with_gherrit_id("Do not inherit a push option");
    ctx.hook_cmd("pre-push").assert().success();
    assert_eq!(fs::read_to_string(ctx.remote_path().join("push-option-count")).unwrap(), "0\n");
}

#[test]
fn publication_between_head_and_history_observations_is_rejected_before_writes() {
    let ctx = testutil::test_context!()
        .with_remote()
        .with_initial_commit()
        .with_mock_github()
        .with_git_interceptor()
        .build();
    ctx.checkout_managed_private("observation-race");
    ctx.commit_with_gherrit_id("Commit V1");
    ctx.hook_cmd("pre-push").assert().success();

    let gherrit_id = ctx.gherrit_id("HEAD").unwrap();
    let managed_ref = format!("refs/heads/{gherrit_id}");
    let v1_ref = format!("refs/tags/gherrit/{gherrit_id}/v1");
    let v2_ref = format!("refs/tags/gherrit/{gherrit_id}/v2");
    let v1_oid = ctx.head_oid();
    let concurrent_oid = ctx.remote_ref_oid("refs/heads/main").unwrap();
    ctx.amend();

    // The two reads are intentionally not described as one snapshot. Model a
    // concurrent publisher committing a coherent head/tag tuple after the
    // global head query but before the exact version query. Coupling the two
    // results must reject the torn observation before either system is
    // mutated by this attempt.
    ctx.update_remote_refs_before_active_version_observation([
        (managed_ref.as_str(), concurrent_oid.as_str()),
        (v2_ref.as_str(), concurrent_oid.as_str()),
    ]);
    let github_requests_before = ctx.github().requests().len();
    let pushes_before = ctx.recorded_pushes().len();

    ctx.hook_cmd("pre-push")
        .assert()
        .failure()
        .stderr(predicates::str::contains("head does not match its latest version tag"));

    assert_eq!(ctx.github().requests().len(), github_requests_before);
    assert_eq!(ctx.recorded_pushes().len(), pushes_before);
    assert_eq!(ctx.remote_ref_oid(&managed_ref).as_deref(), Some(concurrent_oid.as_str()));
    assert_eq!(ctx.remote_ref_oid(&v1_ref).as_deref(), Some(v1_oid.as_str()));
    assert_eq!(ctx.remote_ref_oid(&v2_ref).as_deref(), Some(concurrent_oid.as_str()));
    assert_ne!(ctx.head_oid(), concurrent_oid);
    assert_eq!(ctx.recorded_git_invocations(testutil::GitOperation::LsRemoteHeads).len(), 2);
    assert_eq!(
        ctx.recorded_git_invocations(testutil::GitOperation::LsRemoteActiveVersions).len(),
        2
    );
    assert!(ctx.recorded_git_invocations(testutil::GitOperation::LsRemoteOther).is_empty());
}

#[test]
fn malformed_inactive_version_history_is_not_observed() {
    let ctx = testutil::test_context!()
        .with_remote()
        .with_initial_commit()
        .with_mock_github()
        .with_git_interceptor()
        .build();
    let malformed_inactive = "refs/tags/gherrit/Ginactive/v01";
    ctx.remote_git_cmd()
        .args(["update-ref", malformed_inactive, "refs/heads/main"])
        .assert()
        .success();
    ctx.checkout_managed_private("active-history-only");
    ctx.commit_with_explicit_gherrit_id("Active change", "Gactive");

    ctx.hook_cmd("pre-push").assert().success();

    let queries = ctx.recorded_git_invocations(testutil::GitOperation::LsRemoteActiveVersions);
    assert_eq!(queries.len(), 1);
    assert!(queries[0].iter().all(|argument| !argument.contains("Ginactive")));
    assert!(queries[0].iter().any(|argument| argument == "refs/tags/gherrit/Gactive"));
    assert_eq!(
        ctx.remote_ref_oid(malformed_inactive).as_deref(),
        ctx.remote_ref_oid("refs/heads/main").as_deref()
    );
    assert!(ctx.remote_ref_oid("refs/tags/gherrit/Gactive/v1").is_some());
}

#[test]
fn active_version_observation_batches_cover_every_local_id() {
    let ctx = testutil::test_context!()
        .with_remote()
        .with_initial_commit()
        .with_mock_github()
        .with_git_interceptor()
        .build();
    ctx.checkout_managed_private("batched-active-history");
    let ids = commit_many_active_changes(&ctx);

    ctx.hook_cmd("pre-push").assert().success();

    let queries = ctx.recorded_git_invocations(testutil::GitOperation::LsRemoteActiveVersions);
    assert!(queries.len() > 1, "fixture must require more than one active-history query");
    assert_eq!(observed_active_version_patterns(&queries), active_version_patterns(&ids));
    assert!(queries.iter().all(|query| {
        query
            .iter()
            .filter(|argument| argument.starts_with("refs/tags/gherrit/"))
            .map(|argument| argument.len() + 1)
            .sum::<usize>()
            <= ACTIVE_VERSION_QUERY_BUDGET_BYTES
    }));
    assert_eq!(ctx.recorded_git_invocations(testutil::GitOperation::LsRemoteHeads).len(), 1);
    assert!(ctx.recorded_git_invocations(testutil::GitOperation::LsRemoteOther).is_empty());
    assert!(!ctx.recorded_pushes().is_empty());
    assert!(ctx.recorded_pushes().iter().all(testutil::PushRecord::succeeded));

    let expected_refs = ids
        .iter()
        .flat_map(|id| [format!("refs/heads/{id}"), format!("refs/tags/gherrit/{id}/v1")])
        .collect::<BTreeSet<_>>();
    let actual_refs = ctx
        .remote_refs("refs")
        .into_iter()
        .filter(|ref_name| ref_name != "refs/heads/main")
        .collect::<BTreeSet<_>>();
    assert_eq!(actual_refs, expected_refs);
    assert_eq!(
        ctx.github()
            .pull_requests()
            .into_iter()
            .map(|pull_request| pull_request.head)
            .collect::<BTreeSet<_>>(),
        ids.into_iter().collect()
    );
}

#[test]
fn later_active_version_observation_failure_blocks_every_write() {
    let ctx = testutil::test_context!()
        .with_remote()
        .with_initial_commit()
        .with_mock_github()
        .with_git_interceptor()
        .build();
    ctx.checkout_managed_private("failed-later-active-history");
    let ids = commit_many_active_changes(&ctx);
    let refs_before = ctx.remote_refs("refs");
    ctx.expect_git_output(testutil::GitOperation::LsRemoteActiveVersions, "");
    ctx.expect_git_failure(testutil::GitOperation::LsRemoteActiveVersions);

    ctx.hook_cmd("pre-push").assert().failure().stderr(predicates::str::contains(
        "`git ls-remote` failed while observing active version history",
    ));

    ctx.assert_failure_consumed();
    let queries = ctx.recorded_git_invocations(testutil::GitOperation::LsRemoteActiveVersions);
    assert_eq!(queries.len(), 2, "the first batch succeeded before the second failed");
    assert_eq!(observed_active_version_patterns(&queries), active_version_patterns(&ids));
    assert_eq!(ctx.recorded_git_invocations(testutil::GitOperation::LsRemoteHeads).len(), 1);
    assert!(ctx.recorded_git_invocations(testutil::GitOperation::LsRemoteOther).is_empty());
    assert!(ctx.github().requests().is_empty());
    assert!(ctx.recorded_pushes().is_empty());
    assert_eq!(ctx.remote_refs("refs"), refs_before);
}

#[test]
fn global_head_arguments_do_not_grow_with_the_local_stack() {
    let observe = |count: usize| {
        let ctx = testutil::test_context!()
            .with_remote()
            .with_initial_commit()
            .with_mock_github()
            .with_git_interceptor()
            .build();
        ctx.checkout_managed_private(&format!("constant-head-query-{count}"));
        for index in 0..count {
            ctx.commit_with_explicit_gherrit_id(&format!("Change {index}"), &format!("G{index}"));
        }
        ctx.hook_cmd("pre-push").assert().success();
        let queries = ctx.recorded_git_invocations(testutil::GitOperation::LsRemoteHeads);
        assert_eq!(queries.len(), 1);
        queries.into_iter().next().unwrap()
    };

    assert_eq!(observe(1), observe(40));
}

#[test]
fn empty_local_stack_only_observes_global_heads() {
    let ctx = testutil::test_context!()
        .with_remote()
        .with_initial_commit()
        .with_git_interceptor()
        .build();
    ctx.checkout_managed_private("empty-stack");

    ctx.hook_cmd("pre-push").assert().success();

    assert_eq!(ctx.recorded_git_invocations(testutil::GitOperation::LsRemoteHeads).len(), 1);
    assert!(
        ctx.recorded_git_invocations(testutil::GitOperation::LsRemoteActiveVersions).is_empty()
    );
    assert!(ctx.recorded_pushes().is_empty());
}

#[test]
fn global_nonlocal_heads_do_not_expand_the_active_stack() {
    let ctx = testutil::test_context!()
        .with_remote()
        .with_initial_commit()
        .with_mock_github()
        .with_git_interceptor()
        .build();
    ctx.remote_git_cmd()
        .args(["update-ref", "refs/heads/Ginactive", "refs/heads/main"])
        .assert()
        .success();
    ctx.checkout_managed_private("ignore-nonlocal-head");
    ctx.commit_with_explicit_gherrit_id("Active", "Gactive");

    ctx.hook_cmd("pre-push").assert().success();

    assert!(ctx.remote_ref_oid("refs/tags/gherrit/Gactive/v1").is_some());
    assert!(ctx.remote_ref_oid("refs/tags/gherrit/Ginactive/v1").is_none());
    assert_eq!(ctx.github().pull_requests().len(), 1);
    assert_eq!(ctx.github().pull_requests()[0].head, "Gactive");
}

#[test]
fn oversized_late_id_fails_after_global_heads_but_before_active_history() {
    let ctx = testutil::test_context!()
        .with_remote()
        .with_initial_commit()
        .with_mock_github()
        .with_git_interceptor()
        .build();
    ctx.checkout_managed_private("oversized-active-history");
    ctx.commit_with_explicit_gherrit_id("Small", "Gsmall");
    let oversized = format!("G{}", "a".repeat(9_000));
    ctx.commit_with_explicit_gherrit_id("Oversized", &oversized);

    ctx.hook_cmd("pre-push")
        .assert()
        .failure()
        .stderr(predicates::str::contains("too long for a remote observation query"));

    assert_eq!(ctx.recorded_git_invocations(testutil::GitOperation::LsRemoteHeads).len(), 1);
    assert!(
        ctx.recorded_git_invocations(testutil::GitOperation::LsRemoteActiveVersions).is_empty()
    );
    assert!(ctx.recorded_pushes().is_empty());
    assert!(ctx.github().requests().is_empty());
}

#[test]
fn invalid_later_history_blocks_every_earlier_publication() {
    let ctx = testutil::test_context!()
        .with_remote()
        .with_initial_commit()
        .with_mock_github()
        .with_git_interceptor()
        .build();
    ctx.checkout_managed_private("whole-stack-preflight");
    ctx.commit_with_explicit_gherrit_id("Valid first change", "Gvalid");
    ctx.commit_with_explicit_gherrit_id("Invalid later change", "Gbad");
    ctx.remote_git_cmd()
        .args(["update-ref", "refs/heads/Gbad", "refs/heads/main"])
        .assert()
        .success();
    ctx.remote_git_cmd()
        .args(["update-ref", "refs/tags/gherrit/Gbad/v2", "refs/heads/main"])
        .assert()
        .success();
    let refs_before = ctx.remote_refs("refs");

    ctx.hook_cmd("pre-push")
        .assert()
        .failure()
        .stderr(predicates::str::contains("noncontiguous version tags"));

    assert_eq!(ctx.remote_refs("refs"), refs_before);
    assert!(ctx.remote_ref_oid("refs/heads/Gvalid").is_none());
    assert!(ctx.recorded_pushes().is_empty());
    assert!(ctx.github().requests().is_empty());
}

#[test]
fn observed_owned_base_rejects_mixed_publication_representations() {
    let ctx = testutil::test_context!()
        .with_remote()
        .with_initial_commit()
        .with_mock_github()
        .with_git_interceptor()
        .build();
    ctx.checkout_managed_private("owned-base");
    ctx.commit_with_explicit_gherrit_id("Owned base", "Gowned");
    ctx.remote_git_cmd()
        .args(["update-ref", "refs/heads/gherrit-bases/Gowned", "refs/heads/main"])
        .assert()
        .success();
    let refs_before = ctx.remote_refs("refs");

    ctx.hook_cmd("pre-push")
        .assert()
        .failure()
        .stderr(predicates::str::contains("mixed representations"));

    assert_eq!(ctx.remote_refs("refs"), refs_before);
    assert!(ctx.recorded_pushes().is_empty());
    assert!(ctx.github().requests().is_empty());
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

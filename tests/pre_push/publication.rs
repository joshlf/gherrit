use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    path::Path,
    sync::mpsc,
    thread,
    time::Duration,
};

const LOCAL_REF_QUERY_BUDGET_BYTES: usize = 16 * 1024;
const MANY_LOCAL_ID_COUNT: usize = 60;
const MANY_LOCAL_ID_LEN: usize = 120;

fn commit_many_local_changes(ctx: &testutil::TestContext) -> Vec<String> {
    let ids = (0..MANY_LOCAL_ID_COUNT)
        .map(|index| {
            let prefix = format!("G{index:03}");
            format!("{prefix}{}", "a".repeat(MANY_LOCAL_ID_LEN - prefix.len()))
        })
        .collect::<Vec<_>>();

    let pattern_bytes = local_observation_patterns(&ids)
        .into_iter()
        .map(|pattern| pattern.len() + 1)
        .sum::<usize>();
    assert!(pattern_bytes > LOCAL_REF_QUERY_BUDGET_BYTES);

    ids.iter().enumerate().for_each(|(index, id)| {
        ctx.commit_with_explicit_gherrit_id(&format!("Change {index}"), id);
    });
    ids
}

fn local_observation_patterns(ids: &[String]) -> Vec<String> {
    ids.iter()
        .flat_map(|id| {
            let root = format!("refs/tags/gherrit/{id}");
            [
                format!("refs/heads/{id}"),
                format!("refs/heads/gherrit-bases/{id}"),
                root.clone(),
                format!("{root}/*"),
            ]
        })
        .collect()
}

fn observed_local_patterns(queries: &[Vec<String>]) -> Vec<String> {
    queries
        .iter()
        .flatten()
        .filter(|argument| {
            (argument.starts_with("refs/heads/") && argument.as_str() != "refs/heads/main")
                || argument.starts_with("refs/tags/gherrit/")
        })
        .cloned()
        .collect()
}

fn push_destinations(push: &testutil::PushRecord) -> BTreeSet<String> {
    push.arguments()
        .iter()
        .filter_map(|argument| {
            if let Some(lease) = argument.strip_prefix("--force-with-lease=") {
                return lease.split_once(':').map(|(destination, _)| destination);
            }
            argument.split_once(':').map(|(_, destination)| destination)
        })
        .filter(|destination| destination.starts_with("refs/"))
        .map(str::to_owned)
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
    let default_oid = ctx.remote_ref_oid("refs/heads/main").unwrap();

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
    ctx.assert_owned_base_tuple(&testutil::OwnedBaseTuple {
        id: commit_a_id.clone(),
        version: 1,
        head_oid: commit_a_oid.clone(),
        base_oid: default_oid,
        marker_oid: Some(commit_a_oid.clone()),
    });
    ctx.assert_owned_base_tuple(&testutil::OwnedBaseTuple {
        id: commit_b_id.clone(),
        version: 1,
        head_oid: commit_b_oid.clone(),
        base_oid: commit_a_oid,
        marker_oid: Some(commit_b_oid),
    });

    let default_queries = ctx.recorded_git_invocations(testutil::GitOperation::LsRemoteDefault);
    let local_queries = ctx.recorded_git_invocations(testutil::GitOperation::LsRemoteLocal);
    assert_eq!(default_queries.len(), 1, "one attempt has one default-branch observation");
    assert_eq!(local_queries.len(), 1, "an ordinary stack has one exact local observation");
    assert!(ctx.recorded_git_invocations(testutil::GitOperation::LsRemoteOther).is_empty());
    assert_eq!(
        default_queries[0],
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
        ]
        .map(ToOwned::to_owned)
    );
    assert!(
        local_queries[0]
            .iter()
            .any(|argument| argument == &format!("refs/tags/gherrit/{commit_a_id}"))
    );
    assert!(
        local_queries[0]
            .iter()
            .any(|argument| argument == &format!("refs/tags/gherrit/{commit_b_id}/*"))
    );
}

#[test]
fn mixed_established_and_new_stack_publishes_only_the_new_tuple() {
    let ctx = testutil::test_context!()
        .with_remote()
        .with_initial_commit()
        .with_mock_github()
        .with_git_interceptor()
        .build();
    ctx.checkout_managed_private("mixed-established-new");
    let default_oid = ctx.remote_ref_oid("refs/heads/main").unwrap();

    let established_id = ctx.commit_with_gherrit_id("Established root");
    let established_oid = ctx.head_oid();
    ctx.hook_cmd("pre-push").assert().success();
    let pushes_before = ctx.recorded_pushes().len();
    let events_before = ctx.external_events().len();
    assert_eq!(pushes_before, 2, "first publication has tuple and marker barriers");

    let new_id = ctx.commit_with_gherrit_id("New child");
    let new_oid = ctx.head_oid();
    ctx.hook_cmd("pre-push").assert().success();

    ctx.assert_owned_base_tuple(&testutil::OwnedBaseTuple {
        id: established_id.clone(),
        version: 1,
        head_oid: established_oid.clone(),
        base_oid: default_oid.clone(),
        marker_oid: Some(established_oid.clone()),
    });
    ctx.assert_owned_base_tuple(&testutil::OwnedBaseTuple {
        id: new_id.clone(),
        version: 1,
        head_oid: new_oid.clone(),
        base_oid: established_oid.clone(),
        marker_oid: Some(new_oid.clone()),
    });

    let pushes = ctx.recorded_pushes();
    let second_attempt_pushes = &pushes[pushes_before..];
    assert_eq!(second_attempt_pushes.len(), 2, "new child adds one tuple and one marker batch");
    assert!(second_attempt_pushes.iter().all(testutil::PushRecord::succeeded));
    for destination in [
        format!("refs/heads/{established_id}"),
        format!("refs/heads/gherrit-bases/{established_id}"),
        format!("refs/tags/gherrit/{established_id}/v1"),
        format!("refs/tags/gherrit/{established_id}/pr"),
    ] {
        assert!(
            second_attempt_pushes
                .iter()
                .all(|push| !push_destinations(push).contains(&destination)),
            "the established head, owned base, version, and marker destinations must all be absent; found {destination}"
        );
    }
    let pull_requests = ctx.github().pull_requests();
    let new_base = format!("gherrit-bases/{new_id}");
    assert_eq!(pull_requests.len(), 2);
    assert_eq!(
        pull_requests
            .iter()
            .map(|pull_request| (
                pull_request.head.as_str(),
                pull_request.base.as_str(),
                pull_request.base_oid.as_str(),
            ))
            .collect::<Vec<_>>(),
        [
            (established_id.as_str(), "main", default_oid.as_str()),
            (new_id.as_str(), new_base.as_str(), established_oid.as_str()),
        ]
    );
    assert!(pull_requests.iter().all(|pull_request| {
        pull_request.body.as_deref().is_some_and(|body| body.contains("#1") && body.contains("#2"))
    }));

    let events = ctx.external_events();
    let second_attempt_events = &events[events_before..];
    let writes = second_attempt_events
        .iter()
        .filter(|event| {
            matches!(
                event,
                testutil::ExternalEvent::GitPush(_)
                    | testutil::ExternalEvent::GraphQl(testutil::GraphQlExchange::Mutation { .. })
            )
        })
        .collect::<Vec<_>>();
    let [
        testutil::ExternalEvent::GitPush(tuple_push),
        testutil::ExternalEvent::GraphQl(testutil::GraphQlExchange::Mutation {
            operations: creates,
        }),
        testutil::ExternalEvent::GitPush(marker_push),
        testutil::ExternalEvent::GraphQl(testutil::GraphQlExchange::Mutation {
            operations: updates,
        }),
    ] = writes.as_slice()
    else {
        panic!("write events did not follow tuple -> create -> marker -> update: {writes:#?}");
    };

    assert_eq!(
        push_destinations(tuple_push),
        BTreeSet::from([
            format!("refs/heads/{new_id}"),
            format!("refs/heads/gherrit-bases/{new_id}"),
            format!("refs/tags/gherrit/{new_id}/v1"),
        ])
    );
    assert_eq!(
        tuple_push
            .arguments()
            .iter()
            .filter(|argument| argument.starts_with("--force-with-lease="))
            .cloned()
            .collect::<Vec<_>>(),
        [
            format!("--force-with-lease=refs/heads/{new_id}:"),
            format!("--force-with-lease=refs/heads/gherrit-bases/{new_id}:"),
            format!("--force-with-lease=refs/tags/gherrit/{new_id}/v1:"),
        ]
    );
    assert_eq!(
        tuple_push
            .arguments()
            .iter()
            .filter(|argument| {
                argument
                    .split_once(':')
                    .is_some_and(|(_, destination)| destination.starts_with("refs/"))
            })
            .cloned()
            .collect::<Vec<_>>(),
        [
            format!("{new_oid}:refs/heads/{new_id}"),
            format!("{established_oid}:refs/heads/gherrit-bases/{new_id}"),
            format!("{new_oid}:refs/tags/gherrit/{new_id}/v1"),
        ]
    );

    assert_eq!(
        push_destinations(marker_push),
        BTreeSet::from([format!("refs/tags/gherrit/{new_id}/pr")])
    );
    assert_eq!(
        marker_push
            .arguments()
            .iter()
            .filter(|argument| argument.starts_with("--force-with-lease="))
            .cloned()
            .collect::<Vec<_>>(),
        [format!("--force-with-lease=refs/tags/gherrit/{new_id}/pr:")]
    );
    assert_eq!(
        marker_push
            .arguments()
            .iter()
            .filter(|argument| {
                argument
                    .split_once(':')
                    .is_some_and(|(_, destination)| destination.starts_with("refs/"))
            })
            .cloned()
            .collect::<Vec<_>>(),
        [format!("{new_oid}:refs/tags/gherrit/{new_id}/pr")]
    );
    assert_eq!(creates.len(), 1);
    let create = &creates[0];
    assert_eq!(create.operation, testutil::GraphQlOperation::CreatePr);
    assert_eq!(create.alias.as_deref(), Some("op0"));
    assert_eq!(
        create.input.keys().map(String::as_str).collect::<Vec<_>>(),
        [
            "baseRefName",
            "body",
            "clientMutationId",
            "headRefName",
            "headRepositoryId",
            "repositoryId",
            "title",
        ]
    );
    assert_eq!(create.input.get("repositoryId").map(String::as_str), Some("REPO_NODE_ID"));
    assert_eq!(create.input.get("headRepositoryId"), create.input.get("repositoryId"));
    assert_eq!(create.input.get("headRefName").map(String::as_str), Some(new_id.as_str()));
    assert_eq!(create.input.get("baseRefName").map(String::as_str), Some(new_base.as_str()));
    assert_eq!(create.input.get("title").map(String::as_str), Some("New child"));
    let create_mutation_id = format!("gherrit:create:{new_id}");
    assert_eq!(
        create.input.get("clientMutationId").map(String::as_str),
        Some(create_mutation_id.as_str())
    );
    let provisional_body = create.input.get("body").expect("create input contains a body");
    assert!(!provisional_body.contains("#1") && !provisional_body.contains("#2"));
    assert!(provisional_body.contains(&format!("refs/heads/{new_id}")));
    assert!(!provisional_body.contains("<!-- gherrit-meta:"));
    assert_eq!(
        create.selected_fields,
        [
            "clientMutationId",
            "pullRequest.baseRefName",
            "pullRequest.baseRefOid",
            "pullRequest.baseRepository.id",
            "pullRequest.headRefName",
            "pullRequest.headRefOid",
            "pullRequest.headRepository.id",
            "pullRequest.id",
            "pullRequest.number",
            "pullRequest.state",
        ]
    );
    for argument in [
        format!("--force-with-lease=refs/tags/gherrit/{new_id}/pr:"),
        format!("{new_oid}:refs/tags/gherrit/{new_id}/pr"),
    ] {
        assert!(marker_push.arguments().contains(&argument), "marker push omitted {argument}");
    }
    assert_eq!(updates.len(), 2);
    for (index, (update, pull_request)) in updates.iter().zip(&pull_requests).enumerate() {
        assert_eq!(update.operation, testutil::GraphQlOperation::UpdatePr);
        let alias = format!("op{index}");
        assert_eq!(update.alias.as_deref(), Some(alias.as_str()));
        let update_mutation_id = format!("gherrit:update:{}", pull_request.node_id);
        assert_eq!(
            update.input,
            BTreeMap::from([
                ("body".to_owned(), pull_request.body.clone().unwrap()),
                ("clientMutationId".to_owned(), update_mutation_id),
                ("pullRequestId".to_owned(), pull_request.node_id.clone()),
            ])
        );
        assert_eq!(
            update.selected_fields,
            ["clientMutationId", "pullRequest.id", "pullRequest.number"]
        );
    }

    testutil::assert_pr_snapshot!(ctx, "mixed_established_and_new_stack_state");
    let trace = format!(
        "ESTABLISHED ID (MUST BE ABSENT FROM PUSHES): {established_id}\n\
         NEW ID: {new_id}\n\n\
         SECOND-ATTEMPT EXTERNAL EVENTS:\n{second_attempt_events:#?}",
    );
    insta::assert_snapshot!("mixed_established_and_new_stack_trace", ctx.sanitize(&trace));
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
        [(stack_id.as_str(), "main"), ("Gmerge", "gherrit-bases/Gmerge")]
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
    assert_eq!(pushes.len(), 3, "the first publication also establishes its PR marker");
    assert!(
        pushes[2].arguments().iter().all(|argument| !argument.contains(&v1_ref)),
        "The v2 tuple must not attempt to republish the immutable v1 tag: {:?}",
        pushes[2].arguments()
    );

    // Retrying an already-published stack still reconciles GitHub but does
    // not synthesize a new immutable version.
    ctx.hook_cmd("pre-push").assert().success();
    assert_eq!(ctx.recorded_pushes().len(), 3);
    assert_eq!(ctx.remote_ref_oid(&v2_ref).as_deref(), Some(v2_oid.as_str()));
}

#[test]
fn adjacent_duplicate_versions_are_preserved_but_not_generated() {
    let ctx = testutil::test_context!()
        .with_remote()
        .with_initial_commit()
        .with_mock_github()
        .with_git_interceptor()
        .build();

    ctx.checkout_managed_private("feature-duplicate-version-history");
    let id = ctx.commit_with_gherrit_id("Version A");
    ctx.hook_cmd("pre-push").assert().success();

    let a = ctx.head_oid();
    let managed_ref = format!("refs/heads/{id}");
    let base_ref = format!("refs/heads/gherrit-bases/{id}");
    let marker_ref = format!("refs/tags/gherrit/{id}/pr");
    let v1_ref = format!("refs/tags/gherrit/{id}/v1");
    let v2_ref = format!("refs/tags/gherrit/{id}/v2");
    let v3_ref = format!("refs/tags/gherrit/{id}/v3");
    let v4_ref = format!("refs/tags/gherrit/{id}/v4");
    let base = ctx.remote_ref_oid(&base_ref).expect("published owned base");
    let pushes_after_v1 = ctx.recorded_pushes().len();
    let requests_after_v1 = ctx.github().requests().len();

    // Immutable history may contain evidence that this publisher would not
    // generate. Inject an adjacent v2 tag at exactly the v1 revision while
    // leaving the coherent head/base/marker state intact.
    ctx.remote_git_cmd().args(["update-ref", &v2_ref, &a]).assert().success();
    assert_eq!(ctx.remote_ref_oid(&managed_ref).as_deref(), Some(a.as_str()));
    assert_eq!(ctx.remote_ref_oid(&v1_ref).as_deref(), Some(a.as_str()));
    assert_eq!(ctx.remote_ref_oid(&v2_ref).as_deref(), Some(a.as_str()));
    assert_eq!(ctx.remote_ref_oid(&marker_ref).as_deref(), Some(a.as_str()));

    // Exact local work already equals the last observed version. Replaying
    // publication must accept both tag positions without creating v3 or
    // performing any Git write.
    ctx.hook_cmd("pre-push").assert().success();
    assert_eq!(ctx.recorded_pushes().len(), pushes_after_v1);
    assert_eq!(
        &ctx.github().requests()[requests_after_v1..],
        &[vec![testutil::GraphQlOperation::Query], vec![testutil::GraphQlOperation::UpdatePr],],
        "the newly observed immutable tag only changes rendered PR history"
    );
    assert_eq!(ctx.remote_ref_oid(&v3_ref), None);

    ctx.amend_with_message("Version B");
    let b = ctx.head_oid();
    assert_ne!(a, b);

    // A genuinely new literal revision extends the observed history from v2
    // to v3; it must not collapse or overwrite either duplicate predecessor.
    ctx.hook_cmd("pre-push").assert().success();
    assert_eq!(ctx.recorded_pushes().len(), pushes_after_v1 + 1);
    ctx.assert_owned_base_tuple(&testutil::OwnedBaseTuple {
        id: id.clone(),
        version: 3,
        head_oid: b.clone(),
        base_oid: base,
        marker_oid: Some(a.clone()),
    });
    assert_eq!(ctx.remote_ref_oid(&v1_ref).as_deref(), Some(a.as_str()));
    assert_eq!(ctx.remote_ref_oid(&v2_ref).as_deref(), Some(a.as_str()));
    assert_eq!(ctx.remote_ref_oid(&v3_ref).as_deref(), Some(b.as_str()));
    assert_eq!(ctx.remote_ref_oid(&v4_ref), None);

    // The resulting v3 state is itself retry-safe and cannot synthesize v4.
    let requests_after_v3 = ctx.github().requests().len();
    ctx.hook_cmd("pre-push").assert().success();
    assert_eq!(ctx.recorded_pushes().len(), pushes_after_v1 + 1);
    assert_eq!(
        &ctx.github().requests()[requests_after_v3..],
        &[vec![testutil::GraphQlOperation::Query]],
        "an unchanged retry only observes GitHub"
    );
    assert_eq!(ctx.remote_ref_oid(&v4_ref), None);
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

    // Publish a distinct complete v2 tuple without creating a local tag, then
    // advance local work once more. Remote history must select v3.
    ctx.amend_with_message(&format!("Remote V2\n\ngherrit-pr-id: {gherrit_id}"));
    let remote_v2 = ctx.head_oid();
    let literal_base = ctx.remote_ref_oid("refs/heads/main").unwrap();
    ctx.seed_owned_base_tuple(&testutil::OwnedBaseTuple {
        id: gherrit_id.clone(),
        version: 2,
        head_oid: remote_v2,
        base_oid: literal_base,
        marker_oid: Some(pushed_oid.clone()),
    });
    ctx.amend_with_message(&format!("Local V3\n\ngherrit-pr-id: {gherrit_id}"));

    // The remote history, not missing local tags, selects v3.
    ctx.hook_cmd("pre-push").assert().success();

    let pushes = ctx.recorded_pushes();
    assert_eq!(pushes.len(), 3);
    assert!(pushes.iter().all(testutil::PushRecord::succeeded));
    assert_eq!(ctx.remote_ref_oid(&managed_ref).as_deref(), Some(ctx.head_oid().as_str()));
    assert_eq!(
        ctx.remote_ref_oid(&format!("refs/tags/gherrit/{gherrit_id}/v3")).as_deref(),
        Some(ctx.head_oid().as_str())
    );
    assert_ne!(ctx.head_oid(), pushed_oid);
}

#[test]
fn competing_complete_tuple_before_push_causes_exact_lease_loss_before_github_mutation() {
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
    let default_oid = ctx.remote_ref_oid("refs/heads/main").unwrap();
    let default_tree = String::from_utf8(
        ctx.remote_git_cmd()
            .args(["rev-parse", "refs/heads/main^{tree}"])
            .assert()
            .success()
            .get_output()
            .stdout
            .clone(),
    )
    .unwrap();
    let concurrent_oid = String::from_utf8(
        ctx.remote_git_cmd()
            .arg("commit-tree")
            .arg(default_tree.trim())
            .args(["-p", "refs/heads/main", "-m"])
            .arg(format!("Concurrent V2\n\ngherrit-pr-id: {gherrit_id}"))
            .assert()
            .success()
            .get_output()
            .stdout
            .clone(),
    )
    .unwrap();
    let concurrent_oid = concurrent_oid.trim().to_owned();
    ctx.amend();
    let local_oid = ctx.head_oid();

    // A competing publisher advances the same change through a valid complete
    // tuple after our observation but immediately before our push. The
    // interceptor applies both updates in one remote transaction, so this is a
    // real competing publication rather than an intermediate malformed state.
    ctx.update_remote_refs_before_push([
        (managed_ref.as_str(), concurrent_oid.as_str()),
        (v2_ref.as_str(), concurrent_oid.as_str()),
    ]);
    let requests_before = ctx.github().requests().len();

    ctx.hook_cmd("pre-push")
        .assert()
        .failure()
        .stderr(predicates::str::contains("Could not acknowledge `git push`"));

    let pushes = ctx.recorded_pushes();
    assert_eq!(pushes.len(), 3);
    assert!(pushes[0].succeeded());
    assert!(pushes[1].succeeded());
    assert!(!pushes[2].succeeded());
    assert!(pushes[2].arguments().iter().any(|argument| argument == "--atomic"));
    assert!(pushes[2].arguments().contains(&format!("--force-with-lease={managed_ref}:{v1_oid}")));
    assert!(pushes[2].arguments().contains(&format!("--force-with-lease={v2_ref}:")));
    assert_eq!(ctx.remote_ref_oid(&managed_ref).as_deref(), Some(concurrent_oid.as_str()));
    assert_eq!(ctx.remote_ref_oid(&v1_ref).as_deref(), Some(v1_oid.as_str()));
    assert_eq!(ctx.remote_ref_oid(&v2_ref).as_deref(), Some(concurrent_oid.as_str()));
    assert_ne!(local_oid, concurrent_oid);
    ctx.assert_owned_base_tuple(&testutil::OwnedBaseTuple {
        id: gherrit_id,
        version: 2,
        head_oid: concurrent_oid,
        base_oid: default_oid,
        marker_oid: Some(v1_oid),
    });
    let requests = ctx.github().requests();
    let retry_requests = &requests[requests_before..];
    assert_eq!(
        retry_requests,
        &[vec![testutil::GraphQlOperation::Query]],
        "lease loss must stop before every post-observation GitHub mutation"
    );
}

#[test]
fn competing_exact_desired_tuple_satisfies_stale_git_leases() {
    let ctx = testutil::test_context!()
        .with_remote()
        .with_initial_commit()
        .with_mock_github()
        .with_git_interceptor()
        .build();
    ctx.checkout_managed_private("feature-already-desired-race");
    ctx.commit_with_gherrit_id("Commit V1");
    ctx.hook_cmd("pre-push").assert().success();

    let gherrit_id = ctx.gherrit_id("HEAD").unwrap();
    let managed_ref = format!("refs/heads/{gherrit_id}");
    let base_ref = format!("refs/heads/gherrit-bases/{gherrit_id}");
    let v1_ref = format!("refs/tags/gherrit/{gherrit_id}/v1");
    let v2_ref = format!("refs/tags/gherrit/{gherrit_id}/v2");
    let v1_oid = ctx.head_oid();
    let default_oid = ctx.remote_ref_oid("refs/heads/main").unwrap();
    let original_pr = ctx.github().pull_requests().pop().unwrap();

    ctx.amend_with_message("Commit V2");
    let v2_oid = ctx.head_oid();
    assert_ne!(v2_oid, v1_oid);

    // Make the local object available to the bare repository without exposing
    // the desired v2 refs before GHerrit's exact observation.
    ctx.remote_git_cmd()
        .args(["fetch", "--quiet", "--no-tags"])
        .arg(&ctx.repo_path)
        .arg("HEAD")
        .assert()
        .success();

    // Another publisher installs exactly the head and immutable tag GHerrit
    // intends to push. Git treats both refspecs as already up to date, so the
    // stale head lease and absence lease do not reject this atomic push.
    ctx.update_remote_refs_before_push([
        (managed_ref.as_str(), v2_oid.as_str()),
        (v2_ref.as_str(), v2_oid.as_str()),
    ]);
    let pushes_before = ctx.recorded_pushes().len();
    let requests_before = ctx.github().requests().len();

    ctx.hook_cmd("pre-push").assert().success();

    let retry_pushes = &ctx.recorded_pushes()[pushes_before..];
    assert_eq!(retry_pushes.len(), 1);
    assert!(retry_pushes[0].succeeded());
    assert!(retry_pushes[0].arguments().iter().any(|argument| argument == "--atomic"));
    assert!(
        retry_pushes[0].arguments().contains(&format!("--force-with-lease={managed_ref}:{v1_oid}"))
    );
    assert!(
        retry_pushes[0]
            .arguments()
            .contains(&format!("--force-with-lease={base_ref}:{default_oid}"))
    );
    assert!(retry_pushes[0].arguments().contains(&format!("--force-with-lease={v2_ref}:")));
    assert_eq!(
        push_destinations(&retry_pushes[0]),
        BTreeSet::from([managed_ref.clone(), base_ref.clone(), v2_ref.clone()])
    );

    assert_eq!(ctx.remote_ref_oid(&managed_ref).as_deref(), Some(v2_oid.as_str()));
    assert_eq!(ctx.remote_ref_oid(&base_ref).as_deref(), Some(default_oid.as_str()));
    assert_eq!(ctx.remote_ref_oid(&v1_ref).as_deref(), Some(v1_oid.as_str()));
    assert_eq!(ctx.remote_ref_oid(&v2_ref).as_deref(), Some(v2_oid.as_str()));
    ctx.assert_owned_base_tuple(&testutil::OwnedBaseTuple {
        id: gherrit_id.clone(),
        version: 2,
        head_oid: v2_oid.clone(),
        base_oid: default_oid.clone(),
        marker_oid: Some(v1_oid),
    });

    let pull_requests = ctx.github().pull_requests();
    assert_eq!(pull_requests.len(), 1, "the race must not create a second pull request");
    assert_eq!(pull_requests[0].number, original_pr.number);
    assert_eq!(pull_requests[0].node_id, original_pr.node_id);
    assert_eq!(pull_requests[0].title.as_deref(), Some("Commit V2"));
    assert_eq!(pull_requests[0].head, gherrit_id);
    assert_eq!(pull_requests[0].head_oid, v2_oid);
    assert_eq!(pull_requests[0].base, "main");
    assert_eq!(pull_requests[0].base_oid, default_oid);
    let retry_requests = &ctx.github().requests()[requests_before..];
    assert!(
        retry_requests
            .iter()
            .flatten()
            .all(|operation| *operation != testutil::GraphQlOperation::CreatePr)
    );
    assert!(
        retry_requests
            .iter()
            .flatten()
            .any(|operation| *operation == testutil::GraphQlOperation::UpdatePr)
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
    let default_tree = String::from_utf8(
        ctx.remote_git_cmd()
            .args(["rev-parse", "refs/heads/main^{tree}"])
            .assert()
            .success()
            .get_output()
            .stdout
            .clone(),
    )
    .unwrap();
    let concurrent_oid = String::from_utf8(
        ctx.remote_git_cmd()
            .arg("commit-tree")
            .arg(default_tree.trim())
            .args(["-p", "refs/heads/main", "-m", "Concurrent complete revision"])
            .assert()
            .success()
            .get_output()
            .stdout
            .clone(),
    )
    .unwrap();
    let concurrent_oid = concurrent_oid.trim().to_owned();
    ctx.amend();
    ctx.update_remote_ref_before_push(&v2_ref, &concurrent_oid);
    let requests_before = ctx.github().requests().len();

    ctx.hook_cmd("pre-push")
        .assert()
        .failure()
        .stderr(predicates::str::contains("Could not acknowledge `git push`"));

    let pushes = ctx.recorded_pushes();
    assert_eq!(pushes.len(), 3);
    assert!(pushes[0].succeeded());
    assert!(pushes[1].succeeded());
    assert!(!pushes[2].succeeded());
    assert!(pushes[2].arguments().iter().any(|argument| argument == "--atomic"));
    assert!(pushes[2].arguments().contains(&format!("--force-with-lease={managed_ref}:{v1_oid}")));
    assert!(pushes[2].arguments().contains(&format!("--force-with-lease={v2_ref}:")));
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
        ctx.remote_ref_oid(&format!("refs/heads/gherrit-bases/{id}")).as_deref(),
        ctx.remote_ref_oid("refs/heads/main").as_deref()
    );
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
    assert_eq!(
        ctx.recorded_pushes().len(),
        2,
        "retry observes the tuple and publishes only the later marker barrier"
    );
    assert_eq!(ctx.github().pull_requests().len(), 1);
    assert_eq!(
        ctx.remote_ref_oid(&format!("refs/tags/gherrit/{id}/pr")).as_deref(),
        Some(head.as_str())
    );
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
fn marker_publication_failure_is_safe_with_or_without_the_remote_effect() {
    for marker_reached_remote in [false, true] {
        let ctx = testutil::test_context!()
            .with_remote()
            .with_initial_commit()
            .with_mock_github()
            .with_git_interceptor()
            .build();
        ctx.checkout_managed_private(if marker_reached_remote {
            "marker-lost-ack"
        } else {
            "marker-no-effect"
        });
        let id = ctx.commit_with_gherrit_id("Establish through a marker barrier");
        let head = ctx.head_oid();
        let default = ctx.remote_ref_oid("refs/heads/main").unwrap();
        ctx.seed_owned_base_tuple(&testutil::OwnedBaseTuple {
            id: id.clone(),
            version: 1,
            head_oid: head.clone(),
            base_oid: default,
            marker_oid: None,
        });

        if marker_reached_remote {
            ctx.replace_push_stdout_after_passthrough("");
        } else {
            ctx.expect_git_failure(testutil::GitOperation::Push);
        }
        ctx.hook_cmd("pre-push").assert().failure();
        ctx.assert_failure_consumed();

        let marker_ref = format!("refs/tags/gherrit/{id}/pr");
        assert_eq!(ctx.remote_ref_oid(&marker_ref).is_some(), marker_reached_remote);
        let provisional = ctx.github().pull_requests();
        assert_eq!(provisional.len(), 1);
        assert_eq!(provisional[0].base, format!("gherrit-bases/{id}"));

        if marker_reached_remote {
            let pushes_before_hidden_local_pr = ctx.recorded_pushes();
            let creates_before_hidden_local_pr = ctx
                .github()
                .requests()
                .iter()
                .flatten()
                .filter(|operation| **operation == testutil::GraphQlOperation::CreatePr)
                .count();
            ctx.github().suppress_pull_request_from_next_local_observation(provisional[0].number);
            ctx.hook_cmd("pre-push").assert().failure().stderr(predicates::str::contains(
                "pull-request marker but no same-repository pull request",
            ));
            assert_eq!(ctx.recorded_pushes(), pushes_before_hidden_local_pr);
            assert_eq!(ctx.github().pull_requests(), provisional);
            assert_eq!(ctx.remote_ref_oid(&marker_ref).as_deref(), Some(head.as_str()));
            assert_eq!(
                ctx.github()
                    .requests()
                    .iter()
                    .flatten()
                    .filter(|operation| **operation == testutil::GraphQlOperation::CreatePr)
                    .count(),
                creates_before_hidden_local_pr,
                "a durable marker must suppress create when exact local observation omits the pull request"
            );
        }

        ctx.hook_cmd("pre-push").assert().success();
        assert_eq!(ctx.github().pull_requests().len(), 1, "retry must not duplicate the PR");
        assert_eq!(ctx.github().pull_requests()[0].base, "main");
        assert_eq!(ctx.remote_ref_oid(&marker_ref).as_deref(), Some(head.as_str()));
        assert_eq!(
            ctx.github()
                .requests()
                .iter()
                .flatten()
                .filter(|operation| **operation == testutil::GraphQlOperation::CreatePr)
                .count(),
            1,
            "the stable owned-base creation key is sent at most once after a complete receipt"
        );
    }
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
fn complete_tuple_published_before_local_ref_observation_is_extended() {
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
    let v3_ref = format!("refs/tags/gherrit/{gherrit_id}/v3");
    let v1_oid = ctx.head_oid();
    let default_oid = ctx.remote_ref_oid("refs/heads/main").unwrap();
    let default_tree = String::from_utf8(
        ctx.remote_git_cmd()
            .args(["rev-parse", "refs/heads/main^{tree}"])
            .assert()
            .success()
            .get_output()
            .stdout
            .clone(),
    )
    .unwrap();
    let concurrent_oid = String::from_utf8(
        ctx.remote_git_cmd()
            .arg("commit-tree")
            .arg(default_tree.trim())
            .args(["-p", "refs/heads/main", "-m"])
            .arg(format!("Concurrent complete revision\n\ngherrit-pr-id: {gherrit_id}"))
            .assert()
            .success()
            .get_output()
            .stdout
            .clone(),
    )
    .unwrap();
    let concurrent_oid = concurrent_oid.trim().to_owned();
    ctx.amend();
    let local_oid = ctx.head_oid();

    // The initial default read and first complete local ref observation are not
    // one snapshot. Model another publisher committing a coherent
    // head/base/version tuple between them. One per-ID query must observe that
    // complete tuple and the still-agreeing default boundary, allowing this
    // attempt to extend it without joining per-ID evidence from different
    // advertisements.
    ctx.update_remote_refs_before_local_remote_observation([
        (managed_ref.as_str(), concurrent_oid.as_str()),
        (v2_ref.as_str(), concurrent_oid.as_str()),
    ]);
    let github_requests_before = ctx.github().requests().len();
    let pushes_before = ctx.recorded_pushes().len();

    ctx.hook_cmd("pre-push").assert().success();

    let github_requests = ctx.github().requests();
    assert_eq!(
        github_requests.get(github_requests_before),
        Some(&vec![testutil::GraphQlOperation::Query])
    );
    assert!(
        github_requests[github_requests_before..]
            .iter()
            .flatten()
            .all(|operation| *operation != testutil::GraphQlOperation::CreatePr)
    );
    assert_eq!(ctx.recorded_pushes().len(), pushes_before + 1);
    assert_eq!(ctx.remote_ref_oid(&managed_ref).as_deref(), Some(local_oid.as_str()));
    assert_eq!(ctx.remote_ref_oid(&v1_ref).as_deref(), Some(v1_oid.as_str()));
    assert_eq!(ctx.remote_ref_oid(&v2_ref).as_deref(), Some(concurrent_oid.as_str()));
    assert_eq!(ctx.remote_ref_oid(&v3_ref).as_deref(), Some(local_oid.as_str()));
    ctx.assert_owned_base_tuple(&testutil::OwnedBaseTuple {
        id: gherrit_id,
        version: 3,
        head_oid: local_oid,
        base_oid: default_oid,
        marker_oid: Some(v1_oid),
    });
    assert_eq!(ctx.recorded_git_invocations(testutil::GitOperation::LsRemoteDefault).len(), 2);
    assert_eq!(ctx.recorded_git_invocations(testutil::GitOperation::LsRemoteLocal).len(), 2);
    assert!(ctx.recorded_git_invocations(testutil::GitOperation::LsRemoteOther).is_empty());
}

#[test]
fn local_ref_observation_rejects_a_changed_default_boundary_before_writes() {
    let ctx = testutil::test_context!()
        .with_remote()
        .with_initial_commit()
        .with_mock_github()
        .with_git_interceptor()
        .build();
    ctx.checkout_managed_private("moving-default");
    ctx.commit_with_explicit_gherrit_id("Local work", "Glocal");

    let default_tree = String::from_utf8(
        ctx.remote_git_cmd()
            .args(["rev-parse", "refs/heads/main^{tree}"])
            .assert()
            .success()
            .get_output()
            .stdout
            .clone(),
    )
    .unwrap();
    let concurrent_default = String::from_utf8(
        ctx.remote_git_cmd()
            .arg("commit-tree")
            .arg(default_tree.trim())
            .args(["-p", "refs/heads/main", "-m", "Move the default branch"])
            .assert()
            .success()
            .get_output()
            .stdout
            .clone(),
    )
    .unwrap();
    let concurrent_default = concurrent_default.trim().to_owned();
    ctx.update_remote_refs_before_local_remote_observation([(
        "refs/heads/main",
        concurrent_default.as_str(),
    )]);

    ctx.hook_cmd("pre-push")
        .assert()
        .failure()
        .stderr(predicates::str::contains("default branch moved after symbolic HEAD observation"));

    assert_eq!(ctx.remote_ref_oid("refs/heads/main").as_deref(), Some(concurrent_default.as_str()));
    assert!(ctx.remote_ref_oid("refs/heads/Glocal").is_none());
    assert!(ctx.recorded_pushes().is_empty());
    assert_eq!(ctx.github().requests(), [vec![testutil::GraphQlOperation::Query]]);
    assert_eq!(ctx.recorded_git_invocations(testutil::GitOperation::LsRemoteDefault).len(), 1);
    assert_eq!(ctx.recorded_git_invocations(testutil::GitOperation::LsRemoteLocal).len(), 1);
    assert!(ctx.recorded_git_invocations(testutil::GitOperation::LsRemoteOther).is_empty());
}

#[test]
fn malformed_unrelated_version_history_is_not_observed() {
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
    ctx.checkout_managed_private("local-state-only");
    ctx.commit_with_explicit_gherrit_id("Active change", "Gactive");

    ctx.hook_cmd("pre-push").assert().success();

    let queries = ctx.recorded_git_invocations(testutil::GitOperation::LsRemoteLocal);
    assert_eq!(queries.len(), 1);
    assert!(queries[0].iter().all(|argument| !argument.contains("Ginactive")));
    for expected in local_observation_patterns(&["Gactive".to_owned()]) {
        assert!(queries[0].contains(&expected), "missing exact local pattern {expected}");
    }
    assert_eq!(
        ctx.remote_ref_oid(malformed_inactive).as_deref(),
        ctx.remote_ref_oid("refs/heads/main").as_deref()
    );
    assert!(ctx.remote_ref_oid("refs/tags/gherrit/Gactive/v1").is_some());
}

#[test]
fn exact_local_ref_observation_batches_cover_every_local_id() {
    let ctx = testutil::test_context!()
        .with_remote()
        .with_initial_commit()
        .with_mock_github()
        .with_git_interceptor()
        .build();
    ctx.checkout_managed_private("batched-local-state");
    let ids = commit_many_local_changes(&ctx);

    ctx.hook_cmd("pre-push").assert().success();

    let queries = ctx.recorded_git_invocations(testutil::GitOperation::LsRemoteLocal);
    assert!(queries.len() > 1, "fixture must require more than one exact local query");
    assert_eq!(observed_local_patterns(&queries), local_observation_patterns(&ids));
    assert!(queries.iter().all(|query| {
        query
            .iter()
            .filter(|argument| argument.starts_with("refs/"))
            .map(|argument| argument.len() + 1)
            .sum::<usize>()
            <= LOCAL_REF_QUERY_BUDGET_BYTES
    }));
    assert_eq!(ctx.recorded_git_invocations(testutil::GitOperation::LsRemoteDefault).len(), 1);
    assert!(ctx.recorded_git_invocations(testutil::GitOperation::LsRemoteOther).is_empty());
    assert!(!ctx.recorded_pushes().is_empty());
    assert!(ctx.recorded_pushes().iter().all(testutil::PushRecord::succeeded));

    let expected_refs = ids
        .iter()
        .flat_map(|id| {
            [
                format!("refs/heads/{id}"),
                format!("refs/heads/gherrit-bases/{id}"),
                format!("refs/tags/gherrit/{id}/pr"),
                format!("refs/tags/gherrit/{id}/v1"),
            ]
        })
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
fn later_exact_local_ref_observation_failure_blocks_every_write() {
    let ctx = testutil::test_context!()
        .with_remote()
        .with_initial_commit()
        .with_mock_github()
        .with_git_interceptor()
        .build();
    ctx.checkout_managed_private("failed-later-local-state");
    let ids = commit_many_local_changes(&ctx);
    let refs_before = ctx.remote_refs("refs");
    let first_local_response =
        format!("{}\trefs/heads/main\n", ctx.remote_ref_oid("refs/heads/main").unwrap());
    ctx.expect_git_output(testutil::GitOperation::LsRemoteLocal, first_local_response);
    ctx.expect_git_failure(testutil::GitOperation::LsRemoteLocal);

    ctx.hook_cmd("pre-push").assert().failure().stderr(predicates::str::contains(
        "`git ls-remote` failed while observing exact local refs",
    ));

    ctx.assert_failure_consumed();
    let queries = ctx.recorded_git_invocations(testutil::GitOperation::LsRemoteLocal);
    assert_eq!(queries.len(), 2, "the first batch succeeded before the second failed");
    let observed = observed_local_patterns(&queries);
    let expected = local_observation_patterns(&ids);
    assert!(
        observed.len() < expected.len(),
        "the failure must prevent at least one later planned batch"
    );
    assert_eq!(observed, expected[..observed.len()]);
    assert_eq!(ctx.recorded_git_invocations(testutil::GitOperation::LsRemoteDefault).len(), 1);
    assert!(ctx.recorded_git_invocations(testutil::GitOperation::LsRemoteOther).is_empty());
    assert_eq!(
        ctx.github().requests(),
        [vec![testutil::GraphQlOperation::Query; MANY_LOCAL_ID_COUNT]]
    );
    assert!(ctx.recorded_pushes().is_empty());
    assert_eq!(ctx.remote_refs("refs"), refs_before);
}

#[test]
fn remote_default_query_does_not_grow_with_the_local_stack() {
    let observe = |count: usize| {
        let ctx = testutil::test_context!()
            .with_remote()
            .with_initial_commit()
            .with_mock_github()
            .with_git_interceptor()
            .build();
        ctx.checkout_managed_private(&format!("constant-default-query-{count}"));
        for index in 0..count {
            ctx.commit_with_explicit_gherrit_id(&format!("Change {index}"), &format!("G{index}"));
        }
        ctx.hook_cmd("pre-push").assert().success();
        let queries = ctx.recorded_git_invocations(testutil::GitOperation::LsRemoteDefault);
        assert_eq!(queries.len(), 1);
        queries.into_iter().next().unwrap()
    };

    assert_eq!(observe(1), observe(40));
}

#[test]
fn checked_management_intent_controls_public_branch_links_despite_push_remote_drift() {
    for (branch, state, drifted_push_remote, expected_link) in [
        ("private-intent", testutil::MANAGED_PRIVATE, "origin", None),
        (
            "public-intent",
            testutil::MANAGED_PUBLIC,
            ".",
            Some("This PR is on branch [public\\-intent](../tree/public-intent)."),
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
        assert_eq!(pull_requests.len(), 1);
        let body = pull_requests[0].body.as_deref().expect("published PR has a body");
        let branch_links = body
            .lines()
            .filter(|line| line.starts_with("This PR is on branch ["))
            .collect::<Vec<_>>();
        assert_eq!(branch_links, expected_link.into_iter().collect::<Vec<_>>(), "state={state}");
    }
}

#[test]
fn unrelated_git_and_github_state_is_not_observed_or_mutated() {
    let ctx = testutil::test_context!()
        .with_remote()
        .with_initial_commit()
        .with_mock_github()
        .with_git_interceptor()
        .build();
    let default = ctx.remote_ref_oid("refs/heads/main").unwrap();
    let unrelated_tuple = testutil::OwnedBaseTuple {
        id: "Ginactive".to_owned(),
        version: 1,
        head_oid: default.clone(),
        base_oid: default.clone(),
        marker_oid: Some(default.clone()),
    };
    ctx.seed_owned_base_tuple(&unrelated_tuple);
    ctx.github().seed_pull_request(testutil::PullRequestSeed::root(
        17,
        "Unrelated work",
        "<!-- gherrit-meta: unrelated ordinary text -->",
        "Ginactive",
        &default,
        "main",
        &default,
    ));
    let unrelated_pr = ctx.github().pull_requests();
    ctx.checkout_managed_private("ignore-unrelated-state");
    ctx.commit_with_explicit_gherrit_id("Active", "Gactive");

    ctx.hook_cmd("pre-push").assert().success();

    assert!(ctx.remote_ref_oid("refs/tags/gherrit/Gactive/v1").is_some());
    ctx.assert_owned_base_tuple(&unrelated_tuple);
    let pull_requests = ctx.github().pull_requests();
    assert_eq!(pull_requests.len(), 2);
    let unrelated_after = pull_requests
        .iter()
        .filter(|pull_request| pull_request.head == "Ginactive")
        .collect::<Vec<_>>();
    assert_eq!(unrelated_after, unrelated_pr.iter().collect::<Vec<_>>());
    let local_queries = ctx.recorded_git_invocations(testutil::GitOperation::LsRemoteLocal);
    assert_eq!(local_queries.len(), 1);
    assert!(local_queries[0].iter().all(|argument| !argument.contains("Ginactive")));
    let github_queries = ctx
        .external_events()
        .into_iter()
        .filter_map(|event| match event {
            testutil::ExternalEvent::GraphQl(testutil::GraphQlExchange::Repository {
                connections,
                ..
            }) => Some(connections),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(github_queries.len(), 1);
    assert_eq!(
        github_queries[0].iter().map(|connection| connection.head.as_deref()).collect::<Vec<_>>(),
        [Some("Gactive")]
    );
    assert_eq!(github_queries[0][0].states, ["CLOSED", "MERGED", "OPEN"]);
}

#[test]
fn oversized_late_id_fails_after_default_but_before_exact_local_observation() {
    let ctx = testutil::test_context!()
        .with_remote()
        .with_initial_commit()
        .with_mock_github()
        .with_git_interceptor()
        .build();
    ctx.checkout_managed_private("oversized-local-observation");
    ctx.commit_with_explicit_gherrit_id("Small", "Gsmall");
    let oversized = format!("G{}", "a".repeat(9_000));
    ctx.commit_with_explicit_gherrit_id("Oversized", &oversized);

    ctx.hook_cmd("pre-push")
        .assert()
        .failure()
        .stderr(predicates::str::contains("too long for a remote observation query"));

    assert_eq!(ctx.recorded_git_invocations(testutil::GitOperation::LsRemoteDefault).len(), 1);
    assert!(ctx.recorded_git_invocations(testutil::GitOperation::LsRemoteLocal).is_empty());
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
        .args(["update-ref", "refs/heads/gherrit-bases/Gbad", "refs/heads/main"])
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
    assert_eq!(ctx.github().requests(), [vec![testutil::GraphQlOperation::Query; 2]]);
}

#[test]
fn head_and_tag_without_an_owned_base_fail_closed() {
    let ctx = testutil::test_context!()
        .with_remote()
        .with_initial_commit()
        .with_mock_github()
        .with_git_interceptor()
        .build();
    ctx.checkout_managed_private("owned-base");
    ctx.commit_with_explicit_gherrit_id("Owned base", "Gowned");
    ctx.remote_git_cmd().args(["update-ref", "refs/heads/Gowned", "HEAD"]).assert().success();
    ctx.remote_git_cmd()
        .args(["update-ref", "refs/tags/gherrit/Gowned/v1", "HEAD"])
        .assert()
        .success();
    let refs_before = ctx.remote_refs("refs");

    ctx.hook_cmd("pre-push")
        .assert()
        .failure()
        .stderr(predicates::str::contains("does not have a complete owned base"));

    assert_eq!(ctx.remote_refs("refs"), refs_before);
    assert!(ctx.recorded_pushes().is_empty());
    assert_eq!(ctx.github().requests(), [vec![testutil::GraphQlOperation::Query]]);
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
        "GraphQL backoff must not split either the tuple or marker Git batch"
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
fn exact_local_git_and_github_observations_start_concurrently_after_default() {
    let ctx = testutil::test_context!()
        .with_remote()
        .with_initial_commit()
        .with_mock_github()
        .with_git_interceptor()
        .build();
    ctx.checkout_managed_private("concurrent-initial-observation");
    ctx.commit_with_gherrit_id("Observe both systems concurrently");

    let local_git = ctx.gate_next_local_remote_response();
    let local_github = ctx.github().gate_next_local_observation_response();
    let (completed, receive) = mpsc::channel();
    thread::scope(|scope| {
        let mut command = ctx.hook_cmd("pre-push");
        scope.spawn(move || completed.send(command.output()).unwrap());

        local_git.wait_started();
        local_github.wait_started();
        assert_eq!(
            ctx.recorded_git_invocations(testutil::GitOperation::LsRemoteDefault).len(),
            1,
            "the default branch must be observed before either local observation starts"
        );
        local_git.release();
        local_github.release();

        let output = receive
            .recv_timeout(Duration::from_secs(10))
            .expect("held publication did not finish")
            .expect("failed to run held publication");
        assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    });

    assert_eq!(ctx.recorded_git_invocations(testutil::GitOperation::LsRemoteDefault).len(), 1);
    assert_eq!(ctx.recorded_git_invocations(testutil::GitOperation::LsRemoteLocal).len(), 1);
    assert_eq!(ctx.github().requests().first(), Some(&vec![testutil::GraphQlOperation::Query]));
}

#[test]
fn writes_wait_for_complete_exact_local_pagination() {
    let ctx = testutil::test_context!()
        .with_remote()
        .with_initial_commit()
        .with_mock_github()
        .with_git_interceptor()
        .build();
    ctx.checkout_managed_private("complete-local-pagination");
    ctx.commit_with_explicit_gherrit_id("Current work", "Glocal");
    let head = ctx.head_oid();
    let default = ctx.remote_ref_oid("refs/heads/main").unwrap();
    ctx.seed_owned_base_tuple(&testutil::OwnedBaseTuple {
        id: "Glocal".to_owned(),
        version: 1,
        head_oid: head.clone(),
        base_oid: default.clone(),
        marker_oid: Some(head.clone()),
    });
    for (number, title, state) in [
        (7, "Current work", testutil::PullRequestState::Open),
        (8, "Closed history", testutil::PullRequestState::Closed),
        (9, "Merged history", testutil::PullRequestState::Merged),
    ] {
        ctx.github().seed_pull_request(testutil::PullRequestSeed::root(
            number, title, "", "Glocal", &head, "main", &default,
        ));
        ctx.github().set_pull_request_state(number, state);
    }
    let pull_requests_before = ctx.github().pull_requests();
    ctx.limit_graphql_connection_page_size(1);

    let pagination = ctx.github().gate_next_local_pagination_response();
    let (completed, receive) = mpsc::channel();
    thread::scope(|scope| {
        let mut command = ctx.hook_cmd("pre-push");
        scope.spawn(move || completed.send(command.output()).unwrap());

        pagination.wait_started();
        assert!(ctx.recorded_pushes().is_empty());
        assert_eq!(ctx.github().pull_requests(), pull_requests_before);
        assert!(
            ctx.github()
                .requests()
                .iter()
                .flatten()
                .all(|operation| *operation == testutil::GraphQlOperation::Query)
        );
        pagination.release();

        let output = receive
            .recv_timeout(Duration::from_secs(10))
            .expect("publication did not finish after exact-local pagination resumed")
            .expect("failed to run paginated publication");
        assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    });

    assert!(ctx.recorded_pushes().is_empty());
    assert_eq!(ctx.github().pull_requests().len(), 3);
    assert!(
        ctx.github()
            .requests()
            .iter()
            .flatten()
            .any(|operation| { *operation == testutil::GraphQlOperation::UpdatePr })
    );
    assert!(
        !ctx.github()
            .requests()
            .iter()
            .flatten()
            .any(|operation| { *operation == testutil::GraphQlOperation::CreatePr })
    );
}

#[test]
fn empty_stack_stops_after_remote_default_observation() {
    let ctx = testutil::test_context!()
        .with_remote()
        .with_initial_commit()
        .with_mock_github()
        .with_git_interceptor()
        .build();
    ctx.checkout_managed_private("empty-held-local-observation");

    ctx.hook_cmd("pre-push").assert().success();

    assert_eq!(ctx.recorded_git_invocations(testutil::GitOperation::LsRemoteDefault).len(), 1);
    assert!(ctx.recorded_git_invocations(testutil::GitOperation::LsRemoteLocal).is_empty());
    assert!(ctx.github().requests().is_empty());
    assert!(ctx.github().pull_requests().is_empty());
    assert!(ctx.recorded_pushes().is_empty());
}

#[derive(Clone, Copy, Debug)]
enum SuppressedLocalObservationRetry {
    Http,
    ResponseTransport,
    ConnectionPageBackoff,
}

#[test]
fn suppressed_local_observation_survives_every_first_page_retry_and_then_converges() {
    for retry in [
        SuppressedLocalObservationRetry::Http,
        SuppressedLocalObservationRetry::ResponseTransport,
        SuppressedLocalObservationRetry::ConnectionPageBackoff,
    ] {
        let ctx = testutil::test_context!()
            .with_remote()
            .with_initial_commit()
            .with_mock_github()
            .with_git_interceptor()
            .build();
        ctx.checkout_managed_private(&format!("suppressed-local-retry-{retry:?}"));
        let id = ctx.commit_with_gherrit_id("Recover a provisional pull request");
        let head = ctx.head_oid();
        let default = ctx.remote_ref_oid("refs/heads/main").unwrap();
        ctx.seed_owned_base_tuple(&testutil::OwnedBaseTuple {
            id: id.clone(),
            version: 1,
            head_oid: head.clone(),
            base_oid: default.clone(),
            marker_oid: None,
        });
        ctx.github().seed_pull_request(testutil::PullRequestSeed::owned_base(
            7,
            "Recover a provisional pull request",
            "unnumbered provisional body",
            &id,
            &head,
            &default,
        ));
        let provisional = ctx.github().pull_requests();
        ctx.github().suppress_pull_request_from_next_local_observation(7);
        match retry {
            SuppressedLocalObservationRetry::Http => ctx.inject_failure(
                testutil::FailureKind::QueryHttp(testutil::RetryableHttpStatus::ServiceUnavailable),
            ),
            SuppressedLocalObservationRetry::ResponseTransport => {
                ctx.inject_failure(testutil::FailureKind::QueryTransport)
            }
            SuppressedLocalObservationRetry::ConnectionPageBackoff => {
                ctx.limit_graphql_connection_page_size(1)
            }
        }

        ctx.hook_cmd("pre-push")
            .assert()
            .failure()
            .stderr(predicates::str::contains("already exists"));
        ctx.assert_failure_consumed();
        assert_eq!(ctx.github().pull_requests(), provisional, "retry={retry:?}");
        assert!(ctx.remote_ref_oid(&format!("refs/tags/gherrit/{id}/pr")).is_none());
        assert!(
            ctx.recorded_git_invocations(testutil::GitOperation::Push).is_empty(),
            "no marker push may be attempted after duplicate create rejection; retry={retry:?}"
        );

        let failed_events = ctx.external_events();
        let first_local_requests = failed_events
            .iter()
            .filter_map(|event| match event {
                testutil::ExternalEvent::GraphQl(testutil::GraphQlExchange::Repository {
                    connections,
                    ..
                }) if matches!(
                    connections.as_slice(),
                    [connection]
                        if connection.head.as_deref() == Some(id.as_str())
                            && connection.after.is_none()
                            && connection.states == ["CLOSED", "MERGED", "OPEN"]
                ) =>
                {
                    Some(&connections[0])
                }
                _ => None,
            })
            .collect::<Vec<_>>();
        assert!(first_local_requests.len() >= 2, "retry={retry:?}");
        assert!(first_local_requests.iter().all(|request| request.after.is_none()));
        let expected_page_sizes: &[usize] = match retry {
            SuppressedLocalObservationRetry::Http
            | SuppressedLocalObservationRetry::ResponseTransport => &[100, 100],
            SuppressedLocalObservationRetry::ConnectionPageBackoff => &[100, 50, 25, 12, 6, 3, 1],
        };
        assert_eq!(
            first_local_requests.iter().map(|request| request.first).collect::<Vec<_>>(),
            expected_page_sizes,
            "retry={retry:?}"
        );
        let create_operations = failed_events
            .iter()
            .filter_map(|event| match event {
                testutil::ExternalEvent::GraphQl(testutil::GraphQlExchange::Mutation {
                    operations,
                }) => Some(operations),
                _ => None,
            })
            .flatten()
            .filter(|operation| operation.operation == testutil::GraphQlOperation::CreatePr)
            .collect::<Vec<_>>();
        let [create] = create_operations.as_slice() else {
            panic!("expected exactly one stable-key create attempt; retry={retry:?}");
        };
        assert_eq!(create.input.get("repositoryId").map(String::as_str), Some("REPO_NODE_ID"));
        assert_eq!(create.input.get("headRefName"), Some(&id));
        assert_eq!(create.input.get("baseRefName"), Some(&format!("gherrit-bases/{id}")));
        assert_eq!(create.input.get("clientMutationId"), Some(&format!("gherrit:create:{id}")));
        assert!(!failed_events.iter().any(|event| matches!(
            event,
            testutil::ExternalEvent::GraphQl(testutil::GraphQlExchange::Mutation {
                operations,
            }) if operations
                .iter()
                .any(|operation| operation.operation == testutil::GraphQlOperation::UpdatePr)
        )));

        let events_before_retry = failed_events.len();
        ctx.hook_cmd("pre-push").assert().success();
        assert_eq!(ctx.github().pull_requests().len(), 1, "retry={retry:?}");
        assert_eq!(ctx.github().pull_requests()[0].base, "main", "retry={retry:?}");
        assert_ne!(ctx.github().pull_requests()[0].body, provisional[0].body, "retry={retry:?}");
        assert!(
            ctx.github().pull_requests()[0].body.as_deref().is_some_and(|body| body.contains("#7")),
            "retry={retry:?}"
        );
        assert_eq!(
            ctx.remote_ref_oid(&format!("refs/tags/gherrit/{id}/pr")).as_deref(),
            Some(head.as_str()),
            "retry={retry:?}"
        );
        let retry_events = ctx.external_events();
        assert!(retry_events[events_before_retry..].iter().any(|event| matches!(
            event,
            testutil::ExternalEvent::GitPush(push)
                if push.arguments().iter().any(|argument| {
                    argument.ends_with(&format!(":refs/tags/gherrit/{id}/pr"))
                })
        )));
        assert!(retry_events[events_before_retry..].iter().any(|event| matches!(
            event,
            testutil::ExternalEvent::GraphQl(testutil::GraphQlExchange::Mutation {
                operations,
            }) if operations
                .iter()
                .any(|operation| operation.operation == testutil::GraphQlOperation::UpdatePr)
        )));
    }
}

#[test]
fn lost_create_acknowledgement_recovers_without_a_duplicate() {
    let ctx = testutil::test_context!()
        .with_remote()
        .with_initial_commit()
        .with_mock_github()
        .with_git_interceptor()
        .build();
    ctx.checkout_managed_private("lost-create-ack");
    let id = ctx.commit_with_gherrit_id("Recover one provisional pull request");
    let head = ctx.head_oid();
    let default = ctx.remote_ref_oid("refs/heads/main").unwrap();
    ctx.seed_owned_base_tuple(&testutil::OwnedBaseTuple {
        id: id.clone(),
        version: 1,
        head_oid: head.clone(),
        base_oid: default.clone(),
        marker_oid: None,
    });
    ctx.inject_failure(testutil::FailureKind::ApplyMutationIdsThenDisconnect(
        vec![format!("gherrit:create:{id}")].into_boxed_slice(),
    ));

    ctx.hook_cmd("pre-push").assert().failure().stderr(predicates::str::contains("indeterminate"));
    ctx.assert_failure_consumed();
    let provisional = ctx.github().pull_requests();
    assert_eq!(provisional.len(), 1);
    assert_eq!(provisional[0].base, format!("gherrit-bases/{id}"));
    assert!(ctx.remote_ref_oid(&format!("refs/tags/gherrit/{id}/pr")).is_none());

    // Change this same proposal from root to nonroot before retrying. Its
    // desired final base changes, but its permanent create key remains the
    // same head/owned-base pair.
    ctx.run_git(&["checkout", "-b", "inserted-root", "main"]);
    let inserted_id = ctx.commit_with_gherrit_id("Inserted root");
    let inserted_head = ctx.head_oid();
    ctx.run_git(&["checkout", "lost-create-ack"]);
    ctx.run_git(&["rebase", "--keep-empty", "--onto", &inserted_head, "main"]);
    let rebased_head = ctx.head_oid();
    assert_ne!(rebased_head, head);
    ctx.seed_owned_base_tuple(&testutil::OwnedBaseTuple {
        id: inserted_id.clone(),
        version: 1,
        head_oid: inserted_head.clone(),
        base_oid: default.clone(),
        marker_oid: Some(inserted_head.clone()),
    });
    ctx.github().seed_pull_request(testutil::PullRequestSeed::root(
        7,
        "Inserted root",
        "",
        &inserted_id,
        &inserted_head,
        "main",
        &default,
    ));

    ctx.github().suppress_pull_request_from_next_local_observation(provisional[0].number);
    ctx.hook_cmd("pre-push").assert().failure().stderr(predicates::str::contains("already exists"));
    assert_eq!(
        ctx.github().pull_requests().iter().filter(|pull_request| pull_request.head == id).count(),
        1,
        "the stable key forbids a duplicate after root status changes"
    );
    assert!(ctx.remote_ref_oid(&format!("refs/tags/gherrit/{id}/pr")).is_none());

    ctx.hook_cmd("pre-push").assert().success();
    let pull_requests = ctx.github().pull_requests();
    assert_eq!(pull_requests.len(), 2);
    let recovered = pull_requests
        .iter()
        .find(|pull_request| pull_request.head == id)
        .expect("recovered provisional pull request");
    assert_eq!(recovered.base, format!("gherrit-bases/{id}"));
    assert_eq!(recovered.head_oid, rebased_head);
    assert_eq!(recovered.base_oid, inserted_head);
    let inserted = pull_requests
        .iter()
        .find(|pull_request| pull_request.head == inserted_id)
        .expect("inserted root pull request");
    assert_eq!(inserted.base, "main");
    ctx.assert_owned_base_tuple(&testutil::OwnedBaseTuple {
        id: id.clone(),
        version: 2,
        head_oid: rebased_head.clone(),
        base_oid: inserted_head.clone(),
        marker_oid: Some(rebased_head.clone()),
    });
    assert_eq!(
        ctx.remote_ref_oid(&format!("refs/tags/gherrit/{id}/v1")).as_deref(),
        Some(head.as_str()),
        "the first immutable version remains the original root proposal"
    );
}

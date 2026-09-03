use std::fs;

use predicates::prelude::*;

fn install_composite_pre_push(ctx: &testutil::TestContext, body: &str) {
    let hook = ctx.repo_path.join(".git/hooks/pre-push");
    fs::write(
        hook,
        format!(
            "#!/bin/sh\n\
             set -eu\n\
             printf 'enter:%s\\n' \"$1\" >> \"$GHERRIT_HOOK_LOG\"\n\
             gherrit hook pre-push \"$@\"\n\
             {body}\n"
        ),
    )
    .unwrap();
}

fn checkout_from_main(ctx: &testutil::TestContext, branch: &str) {
    ctx.run_git(&["checkout", "main"]);
    ctx.checkout_new(branch);
}

fn remote_marker_number(ctx: &testutil::TestContext, id: &str, expected_head: &str) -> usize {
    let marker = format!("refs/tags/gherrit/{id}/pr");
    assert_eq!(ctx.remote_ref_oid(&format!("{marker}^{{}}")).as_deref(), Some(expected_head));
    let tag = ctx
        .remote_git_cmd()
        .args(["cat-file", "tag", &marker])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    String::from_utf8(tag)
        .unwrap()
        .lines()
        .last()
        .and_then(|line| line.strip_prefix("gherrit-canonical-pr-v1 "))
        .unwrap()
        .parse()
        .unwrap()
}

fn internal_publication_pushes(ctx: &testutil::TestContext) -> usize {
    ctx.recorded_pushes()
        .iter()
        .filter(|push| {
            push.arguments().iter().any(|argument| argument.starts_with("gherrit-publication"))
        })
        .count()
}

fn assert_remote_tuple(ctx: &testutil::TestContext, id: &str, head: &str, base: &str) {
    assert_eq!(ctx.remote_ref_oid(&format!("refs/heads/{id}")).as_deref(), Some(head));
    assert_eq!(
        ctx.remote_ref_oid(&format!("refs/heads/gherrit-bases/{id}")).as_deref(),
        Some(base)
    );
    assert_eq!(ctx.remote_ref_oid(&format!("refs/tags/gherrit/{id}/v1")).as_deref(), Some(head));
}

#[test]
fn installed_hook_publishes_private_public_empty_and_dry_run_intent() {
    let ctx = testutil::test_context!()
        .with_remote()
        .with_installed_hooks()
        .with_initial_commit()
        .with_mock_github()
        .build();

    ctx.checkout_new("feature-boundary");
    ctx.assert_config("branch.feature-boundary.gherritManaged", Some(testutil::MANAGED_PRIVATE));
    ctx.commit("Feature through installed hook");

    let id = ctx.gherrit_id("HEAD").unwrap();
    let oid = ctx.head_oid();
    let message =
        ctx.git_cmd().args(["show", "-s", "--format=%B", "HEAD"]).output().unwrap().stdout;
    let message = String::from_utf8(message).unwrap();
    assert_eq!(message.lines().filter(|line| line.starts_with("gherrit-pr-id: ")).count(), 1);

    ctx.git_cmd().arg("push").assert().success();

    assert_eq!(ctx.remote_ref_oid(&format!("refs/heads/{id}")).as_deref(), Some(oid.as_str()));
    assert_eq!(
        ctx.remote_ref_oid(&format!("refs/heads/gherrit-bases/{id}")),
        ctx.remote_ref_oid("refs/heads/main")
    );
    assert_eq!(
        ctx.remote_ref_oid(&format!("refs/tags/gherrit/{id}/v1")).as_deref(),
        Some(oid.as_str())
    );
    assert!(ctx.remote_ref_oid(&format!("refs/tags/gherrit/{id}/pr")).is_some());
    assert_eq!(ctx.remote_ref_oid("refs/heads/feature-boundary"), None);
    let prs = ctx.github().pull_requests();
    assert_eq!(prs.len(), 1);
    assert_eq!(prs[0].head, id);
    testutil::assert_pr_snapshot!(ctx, "installed_pre_push_pr_state");

    checkout_from_main(&ctx, "public-boundary");
    ctx.gherrit_cmd().args(["manage", "--public"]).assert().success();
    ctx.assert_config("branch.public-boundary.pushRemote", Some("."));
    ctx.commit("Public feature through installed hook");
    let id = ctx.gherrit_id("HEAD").unwrap();
    let head = ctx.head_oid();

    ctx.git_cmd().arg("push").assert().success();

    assert_eq!(ctx.remote_ref_oid("refs/heads/public-boundary").as_deref(), Some(head.as_str()));
    let public = ctx
        .github()
        .pull_requests()
        .into_iter()
        .find(|pull_request| pull_request.head == id)
        .unwrap();
    assert!(public.body.contains("[public\\-boundary](/owner/repo/tree/public-boundary)"));

    checkout_from_main(&ctx, "empty-public");
    ctx.gherrit_cmd().args(["manage", "--public"]).assert().success();
    let head = ctx.head_oid();
    let refs_before = ctx.remote_refs("refs");

    ctx.git_cmd().arg("push").assert().success();

    assert_eq!(ctx.remote_ref_oid("refs/heads/empty-public").as_deref(), Some(head.as_str()));
    let mut expected_refs = refs_before;
    expected_refs.push("refs/heads/empty-public".to_owned());
    expected_refs.sort();
    assert_eq!(ctx.remote_refs("refs"), expected_refs);
    assert_eq!(ctx.github().pull_requests().len(), 2);

    checkout_from_main(&ctx, "dry-run-boundary");
    ctx.commit("Publish despite the enclosing dry run");
    let id = ctx.gherrit_id("HEAD").unwrap();
    let head = ctx.head_oid();

    ctx.git_cmd().args(["push", "--dry-run"]).assert().success();

    assert_eq!(ctx.remote_ref_oid(&format!("refs/heads/{id}")).as_deref(), Some(head.as_str()));
    assert_eq!(ctx.remote_ref_oid("refs/heads/dry-run-boundary"), None);
    assert_eq!(ctx.github().pull_requests().len(), 3);
}

#[test]
fn installed_hook_preserves_git_protocol_arguments_and_input() {
    let ctx = testutil::test_context!()
        .with_remote()
        .with_installed_hooks()
        .with_initial_commit()
        .with_mock_github()
        .build();
    ctx.checkout_new("hook-input-boundary");
    ctx.commit("Exercise the installed hook boundary");
    let id = ctx.gherrit_id("HEAD").unwrap();
    let head = ctx.head_oid();

    // The remote main branch is already current, so this invocation has no
    // ref-update records. The hook must still forward and reject its argv.
    ctx.git_cmd()
        .args(["push", "origin", "main"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("must use the local no-op destination"));

    ctx.git_cmd()
        .args(["push", "origin", "hook-input-boundary"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("must use the local no-op destination"));
    assert_eq!(ctx.remote_ref_oid("refs/heads/hook-input-boundary"), None);

    ctx.git_cmd()
        .args(["push", ".", "HEAD:refs/heads/alternate"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("cannot include an enclosing Git ref update"));
    ctx.git_cmd()
        .args(["rev-parse", "--verify", "--quiet", "refs/heads/alternate"])
        .assert()
        .code(1);

    assert!(ctx.remote_ref_oid(&format!("refs/heads/{id}")).is_none());
    assert!(ctx.github().requests().is_empty());

    ctx.gherrit_cmd().arg("unmanage").assert().success();
    #[cfg(unix)]
    {
        use std::{ffi::OsString, os::unix::ffi::OsStringExt as _};

        let location = OsString::from_vec(b"remote-\xff.git".to_vec());
        ctx.installed_hook_cmd("pre-push").arg("raw").arg(location).assert().success();
    }

    let log = ctx.dir.path().join("pre-push.log");
    let input = ctx.dir.path().join("pre-push.input");
    install_composite_pre_push(&ctx, "tee \"$GHERRIT_HOOK_INPUT\" >/dev/null");
    ctx.git_cmd()
        .env("GHERRIT_HOOK_LOG", &log)
        .env("GHERRIT_HOOK_INPUT", &input)
        .args(["push", "origin", "hook-input-boundary"])
        .assert()
        .success();

    assert_eq!(
        ctx.remote_ref_oid("refs/heads/hook-input-boundary").as_deref(),
        Some(head.as_str())
    );
    assert_eq!(fs::read_to_string(log).unwrap(), "enter:origin\n");
    assert_eq!(
        fs::read_to_string(input).unwrap(),
        format!(
            "refs/heads/hook-input-boundary {head} refs/heads/hook-input-boundary {}\n",
            "0".repeat(head.len())
        )
    );
}

#[test]
fn composite_hook_observes_complete_internal_pushes_and_unterminated_output() {
    let ctx = testutil::test_context!()
        .with_remote()
        .with_installed_hooks()
        .with_initial_commit()
        .with_mock_github()
        .build();
    let log = ctx.dir.path().join("pre-push.log");
    install_composite_pre_push(
        &ctx,
        "printf 'policy accepted for %s' \"$1\"\n\
         while IFS= read -r line; do\n\
         printf 'record:%s:%s\\n' \"$1\" \"$line\" >> \"$GHERRIT_HOOK_LOG\"\n\
         done\n\
         printf 'policy:%s\\n' \"$1\" >> \"$GHERRIT_HOOK_LOG\"",
    );

    ctx.checkout_new("composite-boundary");
    ctx.commit("Feature through a chatty composite hook");
    let id = ctx.gherrit_id("HEAD").unwrap();
    let head = ctx.head_oid();
    let base = ctx.remote_ref_oid("refs/heads/main").unwrap();
    let null = "0".repeat(head.len());

    ctx.git_cmd().env("GHERRIT_HOOK_LOG", &log).arg("push").assert().success();

    assert!(ctx.remote_ref_oid(&format!("refs/heads/{id}")).is_some());
    let marker = ctx.remote_ref_oid(&format!("refs/tags/gherrit/{id}/pr")).unwrap();
    assert_eq!(ctx.github().pull_requests().len(), 1);
    assert_eq!(
        fs::read_to_string(log).unwrap(),
        format!(
            "enter:.\n\
             enter:gherrit-publication\n\
             record:gherrit-publication:{head} {head} refs/heads/{id} {null}\n\
             record:gherrit-publication:{base} {base} refs/heads/gherrit-bases/{id} {null}\n\
             record:gherrit-publication:{head} {head} refs/tags/gherrit/{id}/v1 {null}\n\
             policy:gherrit-publication\n\
             enter:gherrit-publication\n\
             record:gherrit-publication:{marker} {marker} refs/tags/gherrit/{id}/pr {null}\n\
             policy:gherrit-publication\n\
             policy:.\n"
        )
    );
}

#[test]
fn inherited_internal_marker_does_not_suppress_a_linked_worktree() {
    let ctx = testutil::test_context!()
        .with_remote()
        .with_installed_hooks()
        .with_initial_commit()
        .with_mock_github()
        .build();
    let hooks = ctx.repo_path.join(".git/hooks");
    ctx.git_cmd().args(["config", "extensions.worktreeConfig", "true"]).assert().success();
    ctx.git_cmd().arg("config").arg("core.hooksPath").arg(&hooks).assert().success();

    let linked = ctx.dir.path().join("linked-recursion-boundary");
    ctx.git_cmd()
        .args(["worktree", "add", "-b", "linked-recursion-boundary"])
        .arg(&linked)
        .arg("main")
        .assert()
        .success();
    for (suffix, value) in [
        ("gherritManaged", testutil::MANAGED_PRIVATE),
        ("pushRemote", "."),
        ("remote", "."),
        ("merge", "refs/heads/linked-recursion-boundary"),
    ] {
        ctx.git_cmd()
            .current_dir(&linked)
            .args(["config", &format!("branch.linked-recursion-boundary.{suffix}"), value])
            .assert()
            .success();
    }
    ctx.git_cmd()
        .current_dir(&linked)
        .args(["commit", "--allow-empty", "-m", "Nested linked publication"])
        .assert()
        .success();
    let destination =
        ctx.git_cmd().args(["remote", "get-url", "--push", "origin"]).output().unwrap().stdout;
    let destination = String::from_utf8(destination).unwrap();
    ctx.git_cmd()
        .current_dir(&linked)
        .args(["config", "--worktree", "remote.gherrit-publication.url", destination.trim()])
        .assert()
        .success();
    ctx.git_cmd().args(["remote", "get-url", "gherrit-publication"]).assert().failure();

    let log = ctx.dir.path().join("nested-pre-push.log");
    let nested_stderr = ctx.dir.path().join("nested-push.stderr");
    // Git's documented foreign-repository idiom clears the outer Git
    // process's repository-local variables while retaining GHerrit's marker.
    install_composite_pre_push(
        &ctx,
        "if [ \"$1\" = gherrit-publication ] && [ \"${GHERRIT_NESTED_PUSH_ACTIVE-}\" != 1 ]; then\n\
           (\n\
             unset $(git rev-parse --local-env-vars)\n\
             if GHERRIT_NESTED_PUSH_ACTIVE=1 git -C \"$GHERRIT_NESTED_REPO\" push gherrit-publication HEAD:refs/heads/nested-enclosing >/dev/null 2>\"$GHERRIT_NESTED_STDERR\"; then\n\
               printf 'linked hook was incorrectly suppressed\\n' >&2\n\
               exit 74\n\
             fi\n\
           )\n\
           printf 'linked-hook-active\\n' >> \"$GHERRIT_HOOK_LOG\"\n\
         fi",
    );
    ctx.checkout_new("outer-recursion-boundary");
    ctx.commit("Outer publication starts nested push");
    let outer_id = ctx.gherrit_id("HEAD").unwrap();
    let outer_head = ctx.head_oid();

    ctx.git_cmd()
        .env("GHERRIT_HOOK_LOG", &log)
        .env("GHERRIT_NESTED_REPO", &linked)
        .env("GHERRIT_NESTED_STDERR", &nested_stderr)
        .arg("push")
        .assert()
        .success();

    assert_eq!(
        ctx.remote_ref_oid(&format!("refs/heads/{outer_id}")).as_deref(),
        Some(outer_head.as_str())
    );
    assert_eq!(ctx.remote_ref_oid("refs/heads/nested-enclosing"), None);
    let pull_request_heads = ctx
        .github()
        .pull_requests()
        .into_iter()
        .map(|pull_request| pull_request.head)
        .collect::<Vec<_>>();
    assert_eq!(pull_request_heads, [outer_id]);
    let log = fs::read_to_string(log).unwrap();
    assert!(log.lines().any(|line| line == "linked-hook-active"));
    let nested_stderr = fs::read_to_string(nested_stderr).unwrap();
    assert!(
        nested_stderr.contains("must use the local no-op destination"),
        "unexpected nested-push failure:\n{nested_stderr}"
    );
    assert!(
        log.lines().filter(|line| *line == "enter:gherrit-publication").count() >= 2,
        "the outer internal push and linked enclosing push reuse one remote name"
    );
}

#[test]
fn installed_hooks_converge_two_publishers_after_one_marker_lease_wins() {
    const ID: &str = "Goverlappublishers";
    const FIRST_BRANCH: &str = "overlap-first";
    const SECOND_BRANCH: &str = "overlap-second";

    let ctx = testutil::test_context!()
        .with_remote()
        .with_installed_hooks()
        .with_initial_commit()
        .with_mock_github()
        .with_publication_overlap()
        .build();
    ctx.checkout_managed_private(FIRST_BRANCH);
    ctx.commit_with_explicit_gherrit_id("Publish one exact change concurrently", ID);
    let head = ctx.head_oid();
    let base = ctx.remote_ref_oid("refs/heads/main").unwrap();

    // Seed the complete immutable tuple without hook verification. The
    // overlap then races only create identity and the marker absence lease.
    ctx.git_cmd()
        .args([
            "push",
            "--quiet",
            "--no-verify",
            "--atomic",
            "origin",
            &format!("HEAD:refs/heads/{ID}"),
            &format!("HEAD^:refs/heads/gherrit-bases/{ID}"),
            &format!("HEAD:refs/tags/gherrit/{ID}/v1"),
        ])
        .assert()
        .success();
    assert_remote_tuple(&ctx, ID, &head, &base);
    assert!(ctx.remote_ref_oid(&format!("refs/tags/gherrit/{ID}/pr")).is_none());
    assert!(ctx.github().pull_requests().is_empty());
    assert!(ctx.remote_ref_oid(&format!("refs/heads/{FIRST_BRANCH}")).is_none());
    assert!(ctx.remote_ref_oid(&format!("refs/heads/{SECOND_BRANCH}")).is_none());

    let hooks = ctx.repo_path.join(".git/hooks");
    ctx.git_cmd().arg("config").arg("core.hooksPath").arg(&hooks).assert().success();
    let linked = ctx.dir.path().join("overlap-linked");
    ctx.git_cmd()
        .args(["worktree", "add", "-b", SECOND_BRANCH])
        .arg(&linked)
        .arg("HEAD")
        .assert()
        .success();
    for (suffix, value) in [
        ("gherritManaged", testutil::MANAGED_PRIVATE),
        ("pushRemote", "."),
        ("remote", "."),
        ("merge", "refs/heads/overlap-second"),
    ] {
        ctx.set_config(&format!("branch.{SECOND_BRANCH}.{suffix}"), Some(value));
    }

    let mut first = ctx.git_cmd();
    first.current_dir(&ctx.repo_path).arg("push");
    let mut second = ctx.git_cmd();
    second.current_dir(&linked).arg("push");
    let (first, second, provisional) = std::thread::scope(|scope| {
        // This guard is inside the scope so unwind cancellation releases and
        // fails server handlers before Rust joins blocked publisher threads.
        let cancellation = ctx.publication_overlap().cancellation_guard();
        let first = scope.spawn(move || first.output().unwrap());
        let second = scope.spawn(move || second.output().unwrap());

        ctx.publication_overlap().wait_for_create_arrivals();
        assert_eq!(
            ctx.github().requests(),
            [vec![testutil::GraphQlOperation::Query], vec![testutil::GraphQlOperation::Query],]
        );
        assert!(ctx.github().pull_requests().is_empty());
        assert!(ctx.remote_ref_oid(&format!("refs/tags/gherrit/{ID}/pr")).is_none());
        assert_remote_tuple(&ctx, ID, &head, &base);
        assert_eq!(
            ctx.recorded_git_operations()
                .iter()
                .filter(|operation| **operation == testutil::GitOperation::LsRemote)
                .count(),
            4,
            "both publishers completed initial and exact remote observation"
        );

        ctx.publication_overlap().release_create_applications();
        ctx.publication_overlap().wait_for_create_applications();
        let created = ctx.github().pull_requests();
        assert_eq!(created.len(), 2);
        assert_ne!(created[0].number, created[1].number);
        assert_ne!(created[0].node_id, created[1].node_id);
        assert!(created.iter().all(|pull_request| {
            pull_request.state == testutil::PullRequestState::Open
                && pull_request.is_draft
                && pull_request.head == ID
                && pull_request.title == "Publish one exact change concurrently"
                && !pull_request.body.is_empty()
                && pull_request.base == format!("gherrit-bases/{ID}")
        }));
        assert_eq!(created[0].body, created[1].body);
        assert_eq!(
            ctx.github().requests(),
            [
                vec![testutil::GraphQlOperation::Query],
                vec![testutil::GraphQlOperation::Query],
                vec![testutil::GraphQlOperation::CreatePr],
                vec![testutil::GraphQlOperation::CreatePr],
            ]
        );
        assert!(ctx.remote_ref_oid(&format!("refs/tags/gherrit/{ID}/pr")).is_none());
        assert_remote_tuple(&ctx, ID, &head, &base);

        ctx.publication_overlap().release_create_responses();
        ctx.publication_overlap().wait_for_marker_arrivals();
        assert!(ctx.remote_ref_oid(&format!("refs/tags/gherrit/{ID}/pr")).is_none());
        assert_remote_tuple(&ctx, ID, &head, &base);
        assert_eq!(
            ctx.github().requests(),
            [
                vec![testutil::GraphQlOperation::Query],
                vec![testutil::GraphQlOperation::Query],
                vec![testutil::GraphQlOperation::CreatePr],
                vec![testutil::GraphQlOperation::CreatePr],
            ]
        );
        ctx.publication_overlap().release_marker_pushes();

        let outputs = (first.join().unwrap(), second.join().unwrap());
        cancellation.disarm();
        (outputs.0, outputs.1, created)
    });

    assert_ne!(first.status.success(), second.status.success());
    let marker_ref = format!("refs/tags/gherrit/{ID}/pr");
    let internal_pushes = ctx
        .recorded_pushes()
        .into_iter()
        .filter(|push| {
            push.arguments().iter().any(|argument| argument.starts_with("gherrit-publication"))
        })
        .collect::<Vec<_>>();
    assert_eq!(
        internal_pushes.len(),
        2,
        "the converged tuple needs only two marker attempts; all pushes: {:?}",
        (
            ctx.recorded_pushes(),
            String::from_utf8_lossy(&first.stderr),
            String::from_utf8_lossy(&second.stderr),
            ctx.remote_ref_oid(&marker_ref)
        )
    );
    assert_eq!(internal_pushes.iter().filter(|push| push.succeeded()).count(), 1);
    let marker_sources = internal_pushes
        .iter()
        .map(|push| {
            assert!(
                push.arguments().contains(&format!("--force-with-lease={marker_ref}:")),
                "every internal push must use the marker absence lease"
            );
            let refspec = push
                .arguments()
                .iter()
                .find(|argument| argument.ends_with(&format!(":{marker_ref}")))
                .expect("every internal push contains only a marker creation");
            assert_eq!(
                push.arguments()
                    .iter()
                    .filter(|argument| argument.ends_with(&format!(":{marker_ref}")))
                    .count(),
                1,
                "each internal push contains one marker refspec"
            );
            assert!(
                push.arguments().iter().all(
                    |argument| !argument.contains(":refs/heads/") && !argument.ends_with("/v1")
                ),
                "an initial tuple push must not occur"
            );
            refspec.split_once(':').unwrap().0.to_owned()
        })
        .collect::<Vec<_>>();
    assert_ne!(marker_sources[0], marker_sources[1]);

    let selected = remote_marker_number(&ctx, ID, &head);
    let overlapped = ctx.github().pull_requests();
    let canonical = overlapped.iter().find(|pull_request| pull_request.number == selected).unwrap();
    let duplicate = overlapped.iter().find(|pull_request| pull_request.number != selected).unwrap();
    assert_eq!(canonical.state, testutil::PullRequestState::Open);
    assert!(canonical.is_draft);
    assert_eq!(canonical.base, "main");
    assert_eq!(duplicate.state, testutil::PullRequestState::Open);
    assert!(duplicate.is_draft);
    assert_eq!(duplicate.base, format!("gherrit-bases/{ID}"));
    assert_eq!(
        &ctx.github().requests()[4..],
        &[vec![testutil::GraphQlOperation::UpdatePr]],
        "only the marker winner may project; neither attempt closes a duplicate"
    );
    assert_remote_tuple(&ctx, ID, &head, &base);
    assert!(ctx.remote_ref_oid(&format!("refs/heads/{FIRST_BRANCH}")).is_none());
    assert!(ctx.remote_ref_oid(&format!("refs/heads/{SECOND_BRANCH}")).is_none());

    let provisional_winner =
        provisional.iter().find(|pull_request| pull_request.number == selected).unwrap();
    let provisional_loser =
        provisional.iter().find(|pull_request| pull_request.number != selected).unwrap();
    assert_eq!(duplicate, provisional_loser, "the marker loser remains byte-for-byte provisional");
    assert_eq!(canonical.number, provisional_winner.number);
    assert_eq!(canonical.node_id, provisional_winner.node_id);
    assert_eq!(canonical.state, provisional_winner.state);
    assert_eq!(canonical.is_draft, provisional_winner.is_draft);
    assert_eq!(canonical.title, provisional_winner.title);
    assert_eq!(canonical.head, provisional_winner.head);
    assert_ne!(canonical.body, provisional_winner.body);
    assert_ne!(canonical.base, provisional_winner.base);

    let pushes_after_overlap = internal_publication_pushes(&ctx);
    let requests_before_repair = ctx.github().requests().len();
    ctx.git_cmd().arg("push").assert().success();
    let repaired = ctx.github().pull_requests();
    let mut expected_repaired = overlapped.clone();
    expected_repaired
        .iter_mut()
        .find(|pull_request| pull_request.number != selected)
        .unwrap()
        .state = testutil::PullRequestState::Closed;
    assert_eq!(repaired, expected_repaired);
    assert_eq!(
        repaired.iter().find(|pull_request| pull_request.number == selected).unwrap().state,
        testutil::PullRequestState::Open
    );
    assert_eq!(
        repaired.iter().find(|pull_request| pull_request.number != selected).unwrap().state,
        testutil::PullRequestState::Closed
    );
    assert_eq!(internal_publication_pushes(&ctx), pushes_after_overlap);
    assert_eq!(
        &ctx.github().requests()[requests_before_repair..],
        &[
            vec![testutil::GraphQlOperation::Query],
            vec![testutil::GraphQlOperation::Query],
            vec![testutil::GraphQlOperation::ClosePr],
        ]
    );

    let requests_before_quiescence = ctx.github().requests().len();
    ctx.git_cmd().arg("push").assert().success();
    assert_eq!(ctx.github().pull_requests(), repaired);
    assert_eq!(internal_publication_pushes(&ctx), pushes_after_overlap);
    assert_eq!(
        &ctx.github().requests()[requests_before_quiescence..],
        &[vec![testutil::GraphQlOperation::Query]]
    );
    assert!(ctx.remote_ref_oid(&format!("refs/heads/{FIRST_BRANCH}")).is_none());
    assert!(ctx.remote_ref_oid(&format!("refs/heads/{SECOND_BRANCH}")).is_none());
}

#[test]
fn independent_pre_push_check_can_reject_an_internal_publication() {
    let ctx = testutil::test_context!()
        .with_remote()
        .with_installed_hooks()
        .with_initial_commit()
        .with_mock_github()
        .build();
    let log = ctx.dir.path().join("pre-push.log");
    install_composite_pre_push(
        &ctx,
        "case \"$1\" in\n\
         gherrit-publication*)\n\
           printf 'independent policy denied this publication\\n' >&2\n\
           exit 73\n\
           ;;\n\
         esac",
    );

    ctx.checkout_new("rejected-composite-boundary");
    ctx.commit("Feature rejected by independent hook");
    let id = ctx.gherrit_id("HEAD").unwrap();

    let private_destination = ctx
        .git_cmd()
        .args(["remote", "get-url", "--push", "origin"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let private_destination = String::from_utf8(private_destination).unwrap();
    let stderr = ctx
        .git_cmd()
        .env("GHERRIT_HOOK_LOG", &log)
        .arg("push")
        .assert()
        .failure()
        .get_output()
        .stderr
        .clone();
    let stderr = String::from_utf8_lossy(&stderr);
    assert!(stderr.contains("independent policy denied this publication"), "{stderr}");
    assert!(!stderr.contains(private_destination.trim()), "{stderr}");

    assert_eq!(fs::read_to_string(&log).unwrap(), "enter:.\nenter:gherrit-publication\n");
    assert_eq!(ctx.remote_ref_oid(&format!("refs/heads/{id}")), None);
    assert_eq!(ctx.remote_ref_oid("refs/heads/rejected-composite-boundary"), None);
    assert!(ctx.github().pull_requests().is_empty());

    install_composite_pre_push(&ctx, "printf 'policy:%s\\n' \"$1\" >> \"$GHERRIT_HOOK_LOG\"");
    ctx.git_cmd().env("GHERRIT_HOOK_LOG", &log).arg("push").assert().success();
    assert!(ctx.remote_ref_oid(&format!("refs/tags/gherrit/{id}/pr")).is_some());
    assert!(ctx.remote_ref_oid("refs/heads/rejected-composite-boundary").is_none());
    assert_eq!(ctx.github().pull_requests().len(), 1);
}

#[test]
fn later_composite_check_can_reject_the_outer_push_after_publication() {
    let ctx = testutil::test_context!()
        .with_remote()
        .with_installed_hooks()
        .with_initial_commit()
        .with_mock_github()
        .build();
    let log = ctx.dir.path().join("pre-push.log");
    install_composite_pre_push(
        &ctx,
        "if [ \"$1\" = . ]; then\n\
         exit 73\n\
         fi",
    );
    ctx.checkout_new("outer-policy-boundary");
    ctx.commit("Publish before a later outer policy rejects");
    let id = ctx.gherrit_id("HEAD").unwrap();

    ctx.git_cmd().env("GHERRIT_HOOK_LOG", &log).arg("push").assert().failure().code(1);

    assert!(ctx.remote_ref_oid(&format!("refs/heads/{id}")).is_some());
    assert!(ctx.remote_ref_oid(&format!("refs/heads/gherrit-bases/{id}")).is_some());
    assert!(ctx.remote_ref_oid(&format!("refs/tags/gherrit/{id}/v1")).is_some());
    assert!(ctx.remote_ref_oid(&format!("refs/tags/gherrit/{id}/pr")).is_some());
    assert_eq!(ctx.remote_ref_oid("refs/heads/outer-policy-boundary"), None);
    assert_eq!(ctx.github().pull_requests().len(), 1);

    let refs = ctx.remote_refs("refs");
    install_composite_pre_push(&ctx, ":");
    ctx.git_cmd().env("GHERRIT_HOOK_LOG", &log).arg("push").assert().success();
    assert_eq!(ctx.remote_refs("refs"), refs);
    assert_eq!(ctx.github().pull_requests().len(), 1);
}

#[test]
fn second_internal_hook_rejection_leaves_a_safe_prefix_for_retry() {
    let ctx = testutil::test_context!()
        .with_remote()
        .with_installed_hooks()
        .with_initial_commit()
        .with_mock_github()
        .build();
    let log = ctx.dir.path().join("pre-push.log");
    install_composite_pre_push(
        &ctx,
        "if [ \"$1\" = gherrit-publication ]; then\n\
         count=$(grep -c '^enter:gherrit-publication$' \"$GHERRIT_HOOK_LOG\")\n\
         [ \"$count\" -ne 2 ] || exit 73\n\
         fi",
    );
    ctx.checkout_new("second-rejected-boundary");
    ctx.commit("Reject only the marker barrier once");
    let id = ctx.gherrit_id("HEAD").unwrap();

    ctx.git_cmd().env("GHERRIT_HOOK_LOG", &log).arg("push").assert().failure();

    assert!(ctx.remote_ref_oid(&format!("refs/heads/{id}")).is_some());
    assert!(ctx.remote_ref_oid(&format!("refs/heads/gherrit-bases/{id}")).is_some());
    assert!(ctx.remote_ref_oid(&format!("refs/tags/gherrit/{id}/v1")).is_some());
    assert!(ctx.remote_ref_oid(&format!("refs/tags/gherrit/{id}/pr")).is_none());
    assert_eq!(ctx.remote_ref_oid("refs/heads/second-rejected-boundary"), None);
    let pull_requests = ctx.github().pull_requests();
    assert_eq!(pull_requests.len(), 1);
    assert_eq!(pull_requests[0].base, format!("gherrit-bases/{id}"));

    ctx.git_cmd().env("GHERRIT_HOOK_LOG", &log).arg("push").assert().success();

    assert!(ctx.remote_ref_oid(&format!("refs/tags/gherrit/{id}/pr")).is_some());
    assert!(ctx.remote_ref_oid("refs/heads/second-rejected-boundary").is_none());
    assert_eq!(ctx.github().pull_requests()[0].base, "main");
}

#[test]
fn installed_hook_enforces_local_history_preconditions() {
    let ctx = testutil::test_context!()
        .with_remote()
        .with_installed_hooks()
        .with_initial_commit()
        .build();

    ctx.checkout_new("autosquash-boundary");
    ctx.commit("Work in progress");
    let autosquash_id = ctx.gherrit_id("HEAD").unwrap();
    ctx.commit("fixup! Work in progress");
    let refs_before = ctx.remote_refs("refs");

    ctx.git_cmd()
        .arg("push")
        .assert()
        .failure()
        .stderr(predicate::str::contains("Stack contains pending fixup/squash/amend commits"));

    assert_eq!(ctx.remote_refs("refs"), refs_before);
    assert_eq!(ctx.remote_ref_oid("refs/heads/autosquash-boundary"), None);
    assert_eq!(ctx.remote_ref_oid(&format!("refs/heads/{autosquash_id}")), None);

    ctx.run_git(&["checkout", "main"]);
    ctx.gherrit_cmd().args(["manage", "--private"]).assert().success();
    fs::write(ctx.repo_path.join("custom-grafts"), format!("{}\n", ctx.head_oid())).unwrap();
    ctx.git_cmd().env("GIT_GRAFT_FILE", "custom-grafts").arg("push").assert().success();
    assert_eq!(ctx.remote_refs("refs"), refs_before);

    checkout_from_main(&ctx, "custom-graft-boundary");
    ctx.commit("Feature with custom graft environment");
    let graft_id = ctx.gherrit_id("HEAD").unwrap();
    fs::write(ctx.repo_path.join("custom-grafts"), format!("{}\n", ctx.head_oid())).unwrap();
    ctx.git_cmd().env("GIT_GRAFT_FILE", "custom-grafts").arg("push").assert().failure().stderr(
        predicate::str::contains(
            "file named by GIT_GRAFT_FILE is nonempty because the enclosing Git push retains",
        ),
    );
    assert_eq!(ctx.remote_refs("refs"), refs_before);
    assert_eq!(ctx.remote_ref_oid(&format!("refs/heads/{graft_id}")), None);

    checkout_from_main(&ctx, "custom-shallow-boundary");
    ctx.commit("Feature with custom shallow environment");
    let shallow_id = ctx.gherrit_id("HEAD").unwrap();
    fs::write(ctx.repo_path.join("custom-shallow"), format!("{}\n", ctx.head_oid())).unwrap();
    ctx.git_cmd().env("GIT_SHALLOW_FILE", "custom-shallow").arg("push").assert().failure().stderr(
        predicate::str::contains(
            "file named by GIT_SHALLOW_FILE is nonempty because the enclosing Git push retains",
        ),
    );
    assert_eq!(ctx.remote_refs("refs"), refs_before);
    assert_eq!(ctx.remote_ref_oid(&format!("refs/heads/{shallow_id}")), None);
}

#[test]
fn installed_hook_rejects_public_names_that_overlap_owned_refs() {
    let ctx = testutil::test_context!()
        .with_remote()
        .with_installed_hooks()
        .with_initial_commit()
        .with_mock_github()
        .build();
    ctx.checkout_managed_public("Gfirst");
    ctx.commit_with_explicit_gherrit_id("First change", "Gfirst");
    ctx.commit_with_explicit_gherrit_id("Second change", "Gsecond");

    ctx.git_cmd()
        .arg("push")
        .assert()
        .failure()
        .stderr(predicate::str::contains("cannot collide with a change-owned head"));

    assert!(ctx.remote_ref_oid("refs/heads/Gfirst").is_none());
    assert!(ctx.remote_ref_oid("refs/heads/Gsecond").is_none());
    assert!(ctx.remote_refs("refs/tags/gherrit").is_empty());
    assert!(ctx.github().pull_requests().is_empty());
    assert!(ctx.github().requests().is_empty());

    ctx.run_git(&["checkout", "main"]);
    ctx.checkout_managed_public("gherrit-bases/Gthird");
    ctx.commit_with_explicit_gherrit_id("Third change", "Gthird");

    ctx.git_cmd()
        .arg("push")
        .assert()
        .failure()
        .stderr(predicate::str::contains("reserved 'gherrit-bases' namespace"));

    assert!(ctx.remote_ref_oid("refs/heads/Gthird").is_none());
    assert!(ctx.remote_ref_oid("refs/heads/gherrit-bases/Gthird").is_none());
    assert!(ctx.remote_refs("refs/tags/gherrit").is_empty());
    assert!(ctx.github().pull_requests().is_empty());
    assert!(ctx.github().requests().is_empty());
}

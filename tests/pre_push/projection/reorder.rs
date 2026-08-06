use std::collections::HashMap;

use testutil::mock_server::{BaseUpdate, MockPrArgs, PrEntry};

fn context() -> testutil::TestContext {
    testutil::test_context!()
        .with_remote()
        .with_installed_hooks()
        .with_initial_commit()
        .with_mock_github()
        .with_git_interceptor()
        .build()
}

fn create_stack(ctx: &testutil::TestContext, titles: &[&str]) -> Vec<String> {
    ctx.checkout_new("feature-reorder");
    let mut ids = Vec::new();
    for title in titles {
        ctx.commit(title);
        ids.push(ctx.gherrit_id("HEAD").unwrap());
    }
    ctx.hook_cmd("pre-push").assert().success();
    ctx.mutate_mock_state(|state| state.base_updates.clear());
    ids
}

fn rewrite_stack(ctx: &testutil::TestContext, commits: &[(&str, &str)]) {
    ctx.run_git(&["reset", "--hard", "main"]);
    for (title, id) in commits {
        let message = format!("{title}\n\ngherrit-pr-id: {id}");
        ctx.commit(&message);
    }
}

fn pr_by_head<'a>(prs: &'a [PrEntry], head: &str) -> &'a PrEntry {
    prs.iter().find(|pr| pr.head.ref_field == head).unwrap()
}

fn updates_by_head(state: &testutil::mock_server::MockState) -> HashMap<String, Vec<BaseUpdate>> {
    state
        .prs
        .iter()
        .map(|pr| {
            let updates =
                state.base_updates.iter().filter(|update| update.pr_id == pr.id).cloned().collect();
            (pr.head.ref_field.clone(), updates)
        })
        .collect()
}

fn assert_all_open(state: &testutil::mock_server::MockState) {
    for pr in &state.prs {
        assert_eq!(pr.state, "OPEN", "PR #{} was unexpectedly merged", pr.number);
    }
}

#[test]
fn adjacent_swap_stages_the_new_root_before_publishing() {
    let ctx = context();
    let ids = create_stack(&ctx, &["A", "B"]);
    let (a, b) = (&ids[0], &ids[1]);

    rewrite_stack(&ctx, &[("B reordered", b), ("A reordered", a)]);
    ctx.hook_cmd("pre-push").assert().success();

    ctx.inspect_mock_state(|state| {
        assert_all_open(state);
        assert_eq!(pr_by_head(&state.prs, b).base.ref_field, "main");
        assert_eq!(pr_by_head(&state.prs, a).base.ref_field, *b);

        let updates = updates_by_head(state);
        assert_eq!(
            updates[b],
            [BaseUpdate {
                pr_id: pr_by_head(&state.prs, b).id,
                old_base: a.clone(),
                new_base: "main".to_string()
            }]
        );
        assert_eq!(
            updates[a],
            [BaseUpdate {
                pr_id: pr_by_head(&state.prs, a).id,
                old_base: "main".to_string(),
                new_base: b.clone()
            }]
        );
    });
}

#[test]
fn current_safe_base_is_preferred_over_an_unsafe_desired_base() {
    let ctx = context();
    let ids = create_stack(&ctx, &["A", "B", "C"]);
    let (a, b, c) = (&ids[0], &ids[1], &ids[2]);

    rewrite_stack(&ctx, &[("A reordered", a), ("C reordered", c), ("B reordered", b)]);
    ctx.hook_cmd("pre-push").assert().success();

    ctx.inspect_mock_state(|state| {
        assert_all_open(state);
        let updates = updates_by_head(state);
        assert_eq!(
            updates[b],
            [BaseUpdate {
                pr_id: pr_by_head(&state.prs, b).id,
                old_base: a.clone(),
                new_base: c.clone()
            }],
            "B should remain on its safe current base A until publication completes"
        );
        assert_eq!(
            updates[c],
            [BaseUpdate {
                pr_id: pr_by_head(&state.prs, c).id,
                old_base: b.clone(),
                new_base: a.clone()
            }],
            "C should move directly to its safe final base A"
        );
    });
}

#[test]
fn nearest_safe_common_ancestor_avoids_the_default_branch() {
    let ctx = context();
    let ids = create_stack(&ctx, &["A", "B", "C", "D"]);
    let (a, b, c, d) = (&ids[0], &ids[1], &ids[2], &ids[3]);

    rewrite_stack(
        &ctx,
        &[("A reordered", a), ("D reordered", d), ("C reordered", c), ("B reordered", b)],
    );
    ctx.hook_cmd("pre-push").assert().success();

    ctx.inspect_mock_state(|state| {
        assert_all_open(state);
        let updates = updates_by_head(state);
        assert_eq!(
            updates[c],
            [
                BaseUpdate {
                    pr_id: pr_by_head(&state.prs, c).id,
                    old_base: b.clone(),
                    new_base: a.clone(),
                },
                BaseUpdate {
                    pr_id: pr_by_head(&state.prs, c).id,
                    old_base: a.clone(),
                    new_base: d.clone(),
                },
            ],
            "C should park on the nearest common ancestor A, not main"
        );
    });
}

#[test]
fn mock_github_marks_an_unprotected_swap_merged() {
    let ctx = context();
    let ids = create_stack(&ctx, &["A", "B"]);
    let (a, b) = (&ids[0], &ids[1]);

    rewrite_stack(&ctx, &[("B reordered", b), ("A reordered", a)]);
    let b_oid =
        ctx.git_cmd().args(["rev-parse", "HEAD~1"]).assert().success().get_output().stdout.clone();
    let b_oid = String::from_utf8(b_oid).unwrap().trim().to_string();
    let a_oid = ctx.head_oid();
    ctx.git_cmd()
        .arg("push")
        .args(["--quiet", "--no-verify", "--atomic", "--force", "origin"])
        .arg(format!("{b_oid}:refs/heads/{b}"))
        .arg(format!("{a_oid}:refs/heads/{a}"))
        .assert()
        .success();

    ctx.inspect_mock_state(|state| {
        assert_eq!(pr_by_head(&state.prs, b).state, "MERGED");
        assert_eq!(pr_by_head(&state.prs, a).state, "OPEN");
    });
}

#[test]
fn failed_publication_leaves_prs_on_safe_staging_bases() {
    let ctx = context();
    let ids = create_stack(&ctx, &["A", "B"]);
    let (a, b) = (&ids[0], &ids[1]);

    rewrite_stack(&ctx, &[("B reordered", b), ("A reordered", a)]);
    ctx.hook_cmd("pre-push").env("MOCK_BIN_FAIL_CMD", "git:push").assert().failure();

    ctx.inspect_mock_state(|state| {
        assert_all_open(state);
        assert_eq!(pr_by_head(&state.prs, b).base.ref_field, "main");
        assert_eq!(pr_by_head(&state.prs, a).base.ref_field, "main");
        assert_eq!(state.pushes.last().unwrap().exit_code, 1);
    });
}

#[test]
fn unrelated_base_consumer_blocks_the_ref_rewrite() {
    let ctx = context();
    let ids = create_stack(&ctx, &["A", "B"]);
    let (a, b) = (&ids[0], &ids[1]);

    ctx.run_git(&["checkout", "-b", "external", "main"]);
    ctx.commit("External change");
    ctx.git_cmd()
        .args([
            "push",
            "--quiet",
            "--no-verify",
            "origin",
            "refs/heads/external:refs/heads/external",
        ])
        .assert()
        .success();
    ctx.run_git(&["checkout", "feature-reorder"]);

    ctx.mutate_mock_state(|state| {
        state.add_pr(PrEntry::mock(MockPrArgs {
            id: 100,
            title: "External PR".to_string(),
            body: String::new(),
            head: "external".to_string(),
            base: a.clone(),
            repo_owner: testutil::DEFAULT_OWNER,
            repo_name: testutil::DEFAULT_REPO,
        }));
    });

    rewrite_stack(&ctx, &[("B reordered", b), ("A reordered", a)]);
    let remote_refs = ctx.remote_refs("refs");
    ctx.hook_cmd("pre-push")
        .assert()
        .failure()
        .stderr(predicates::str::contains("unrelated open PRs target them: PR #100"));

    assert_eq!(ctx.remote_refs("refs"), remote_refs);
    ctx.inspect_mock_state(|state| {
        assert_all_open(state);
        assert!(state.base_updates.is_empty());
    });
}

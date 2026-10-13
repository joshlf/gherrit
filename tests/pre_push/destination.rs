#[cfg(unix)]
use std::os::unix::fs::{PermissionsExt as _, symlink};
use std::{fs, path::Path};

use predicates::prelude::*;

fn configured_url(ctx: &testutil::TestContext, arguments: &[&str]) -> String {
    let output = ctx.git_cmd().args(arguments).assert().success().get_output().stdout.clone();
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

fn configure_file(ctx: &testutil::TestContext, path: &Path, key: &str, value: &str) {
    fs::write(path, "").unwrap();
    ctx.git_cmd()
        .args(["config", "--file"])
        .arg(path)
        .args(["--add", key, value])
        .assert()
        .success();
}

fn include_when_remote_url_matches(ctx: &testutil::TestContext, destination: &str, path: &Path) {
    // `hasconfig:remote.*.url` treats its suffix as a Git wildmatch pattern.
    // Escape pattern syntax so this fixture names the literal URL. This is
    // load-bearing on Windows, where a native path's backslashes would
    // otherwise escape the following characters instead of matching them.
    let destination = destination.chars().fold(String::new(), |mut pattern, character| {
        if matches!(character, '\\' | '*' | '?' | '[') {
            pattern.push('\\');
        }
        pattern.push(character);
        pattern
    });
    let key = format!("includeIf.hasconfig:remote.*.url:{destination}.path");
    ctx.git_cmd().args(["config", "--add", &key]).arg(path).assert().success();
}

fn configure_unmatched_fetch_destination(ctx: &testutil::TestContext, name: &str) {
    let path = ctx.dir.path().join(name);
    ctx.init_bare_repo(&path);
    let destination = path.to_string_lossy().replace(std::path::MAIN_SEPARATOR, "/");
    ctx.set_config("remote.origin.url", Some(&destination));
}

fn recorded_remote(push: &testutil::PushRecord) -> &str {
    let arguments = push.arguments();
    let separator = arguments.iter().position(|argument| argument == "--").expect("push has --");
    arguments.get(separator + 1).expect("push has a destination after --")
}

#[test]
fn push_destination_controls_observation_publication_and_github_identity() {
    let ctx = testutil::test_context!()
        .repository("push-owner", "push-repo")
        .with_remote()
        .with_initial_commit()
        .with_mock_github()
        .with_git_interceptor()
        .build();
    let push_destination = configured_url(&ctx, &["remote", "get-url", "origin"]);

    let fetch_parent = ctx.dir.path().join("fetch-owner");
    fs::create_dir(&fetch_parent).unwrap();
    let fetch_repository = fetch_parent.join("fetch-repo.git");
    ctx.init_bare_repo(&fetch_repository);
    let fetch_destination =
        fetch_repository.to_string_lossy().replace(std::path::MAIN_SEPARATOR, "/");
    ctx.set_config("remote.origin.url", Some(&fetch_destination));
    ctx.set_config("remote.origin.pushurl", Some(&push_destination));

    // A user remote with GHerrit's preferred internal name must not be
    // overwritten or consulted. The adapter chooses a proved-absent name.
    ctx.set_config("remote.gherrit-publication.url", Some(&fetch_destination));

    ctx.checkout_managed_private("split-remote");
    let id = ctx.commit_with_gherrit_id("Publish through the push destination");
    let managed_ref = format!("refs/heads/{id}");

    ctx.hook_cmd("pre-push").assert().success();

    assert_eq!(ctx.remote_ref_oid(&managed_ref).as_deref(), Some(ctx.head_oid().as_str()));
    assert_eq!(bare_ref_oid(&ctx, &fetch_repository, &managed_ref), None);
    assert_eq!(ctx.github().repository(), ("push-owner".to_owned(), "push-repo".to_owned()));
    let pull_requests = ctx.github().pull_requests();
    assert_eq!(pull_requests.len(), 1);
    let body = pull_requests[0].body.as_str();
    let canonical_fetch_url = "https://github.com/push-owner/push-repo.git";
    assert_eq!(body.matches(canonical_fetch_url).count(), 4);
    assert!(!body.contains("git fetch origin"));
    assert!(!body.contains("git pull origin"));
    assert!(!body.contains(&fetch_destination));
    assert!(!body.contains(&push_destination));

    let pushes = ctx.recorded_pushes();
    assert_eq!(pushes.len(), 2);
    assert!(pushes.iter().all(|push| recorded_remote(push) == "gherrit-publication-1"));
    for push in &pushes {
        for literal in [&push_destination, &fetch_destination] {
            assert!(push.arguments().iter().all(|argument| !argument.contains(literal)));
            assert!(!format!("{push:?}").contains(literal));
        }
    }
}

#[cfg(unix)]
#[test]
fn symlinked_local_destination_binds_git_and_github_to_the_canonical_target() {
    let ctx = testutil::test_context!()
        .repository("actual-owner", "actual-repository")
        .with_remote()
        .with_initial_commit()
        .with_mock_github()
        .with_git_interceptor()
        .build();
    let actual = configured_url(&ctx, &["remote", "get-url", "origin"]);
    let claimed = ctx.dir.path().join("attacker/claimed-owner/claimed-repository.git");
    fs::create_dir_all(claimed.parent().unwrap()).unwrap();
    symlink(&actual, &claimed).unwrap();
    ctx.set_config("remote.origin.pushurl", Some(claimed.to_str().unwrap()));

    ctx.checkout_managed_private("symlinked-destination");
    let id = ctx.commit_with_gherrit_id("Bind a symlinked destination");

    ctx.hook_cmd("pre-push").assert().success();

    assert_eq!(
        ctx.remote_ref_oid(&format!("refs/heads/{id}")).as_deref(),
        Some(ctx.head_oid().as_str())
    );
    assert_eq!(
        ctx.github().repository(),
        ("actual-owner".to_owned(), "actual-repository".to_owned())
    );
    assert_eq!(ctx.github().pull_requests().len(), 1);
}

#[test]
fn multiple_push_destinations_are_rejected_before_external_writes() {
    let ctx = testutil::test_context!()
        .with_remote()
        .with_initial_commit()
        .with_mock_github()
        .with_git_interceptor()
        .build();
    let first = ctx.dir.path().join("secret-first/owner/repo.git");
    let second = ctx.dir.path().join("secret-second/owner/repo.git");
    for destination in [&first, &second] {
        fs::create_dir_all(destination.parent().unwrap()).unwrap();
        ctx.init_bare_repo(destination);
        ctx.git_cmd()
            .args(["config", "--add", "remote.origin.pushurl"])
            .arg(destination)
            .assert()
            .success();
    }
    ctx.checkout_managed_private("multiple-push-destinations");
    ctx.commit_with_gherrit_id("Reject an ambiguous destination");

    let output = ctx.hook_cmd("pre-push").output().unwrap();

    assert!(!output.status.success());
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(stderr.contains("has 2 push destinations; exactly one is required"));
    assert!(!stderr.contains("secret-first"));
    assert!(!stderr.contains("secret-second"));
    assert!(ctx.github().requests().is_empty());
    assert!(ctx.recorded_pushes().is_empty());
}

#[test]
fn unsupported_configured_remote_transport_is_rejected_before_external_io() {
    for key in ["uploadpack", "receivepack", "vcs", "serverOption"] {
        let ctx = testutil::test_context!()
            .with_remote()
            .with_initial_commit()
            .with_mock_github()
            .with_git_interceptor()
            .build();
        let private_value = format!("private-{key}-value");
        ctx.set_config(&format!("remote.origin.{key}"), Some(&private_value));
        ctx.checkout_managed_private(&format!("unsupported-{key}"));
        ctx.commit_with_gherrit_id("Reject an unsupported remote transport setting");

        let output = ctx.hook_cmd("pre-push").output().unwrap();

        assert!(!output.status.success());
        let stderr = String::from_utf8(output.stderr).unwrap();
        assert!(stderr.contains("configures unsupported transport settings"));
        assert!(stderr.contains(key));
        assert!(!stderr.contains(&private_value));
        assert!(ctx.github().requests().is_empty());
        assert!(ctx.recorded_pushes().is_empty());
    }
}

#[cfg(unix)]
#[test]
fn failed_push_does_not_disclose_the_destination_or_child_diagnostics() {
    let ctx = testutil::test_context!()
        .repository("private-owner", "private-repository")
        .with_remote()
        .with_initial_commit()
        .with_mock_github()
        .with_git_interceptor()
        .build();
    let hook = ctx.dir.path().join("private-owner/private-repository.git/hooks/pre-receive");
    fs::write(&hook, "#!/bin/sh\nprintf 'private child diagnostic\\n' >&2\nexit 1\n").unwrap();
    fs::set_permissions(&hook, fs::Permissions::from_mode(0o755)).unwrap();
    ctx.checkout_managed_private("private-failure");
    ctx.commit_with_gherrit_id("Keep destination diagnostics private");

    let output = ctx.hook_cmd("pre-push").output().unwrap();

    assert!(!output.status.success());
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(
        stderr.contains("Git publication was rejected without changing the requested refs"),
        "stderr={stderr:?}"
    );
    for private in ["private-owner", "private-repository", "private child diagnostic"] {
        assert!(!stderr.contains(private), "stderr disclosed {private:?}: {stderr}");
    }
    assert!(ctx.github().pull_requests().is_empty());
}

#[test]
fn file_conditioned_configuration_blocks_the_internal_remote_name() {
    let ctx = testutil::test_context!()
        .with_remote()
        .with_initial_commit()
        .with_mock_github()
        .with_git_interceptor()
        .build();
    let destination = configured_url(&ctx, &["remote", "get-url", "origin"]);

    let include = ctx.dir.path().join("conditional-remote.config");
    configure_file(
        &ctx,
        &include,
        "remote.gherrit-publication.receivepack",
        "unplanned-receive-pack",
    );
    include_when_remote_url_matches(&ctx, &destination, &include);

    ctx.checkout_managed_private("conditional-private-remote");
    let id = ctx.commit_with_gherrit_id("Avoid an active conditioned remote name");

    ctx.hook_cmd("pre-push").assert().success();

    assert_eq!(
        ctx.remote_ref_oid(&format!("refs/heads/{id}")).as_deref(),
        Some(ctx.head_oid().as_str())
    );
    let pushes = ctx.recorded_pushes();
    assert_eq!(pushes.len(), 2);
    assert!(pushes.iter().all(|push| recorded_remote(push) == "gherrit-publication-1"));
    assert!(pushes.iter().all(|push| {
        push.arguments().iter().all(|argument| argument != "unplanned-receive-pack")
    }));
}

#[test]
fn private_remote_selection_accounts_for_command_scoped_conditionals() {
    let ctx = testutil::test_context!()
        .with_remote()
        .with_initial_commit()
        .with_mock_github()
        .with_git_interceptor()
        .build();
    let destination = configured_url(&ctx, &["remote", "get-url", "origin"]);
    let private_destination =
        dunce::canonicalize(&destination).unwrap().into_os_string().into_string().unwrap();
    configure_unmatched_fetch_destination(&ctx, "conditional-fetch.git");
    ctx.set_config("remote.origin.pushurl", Some(&destination));

    let include = ctx.dir.path().join("conditional-remote.config");
    configure_file(
        &ctx,
        &include,
        "remote.gherrit-publication.receivepack",
        "unplanned-receive-pack",
    );
    ctx.git_cmd()
        .args(["config", "--file"])
        .arg(&include)
        .args([
            "--add",
            "remote.gherrit-publication-1.receivepack",
            "second-unplanned-receive-pack",
        ])
        .assert()
        .success();
    include_when_remote_url_matches(&ctx, &private_destination, &include);

    ctx.checkout_managed_private("command-scoped-conditional-remote");
    let id = ctx.commit_with_gherrit_id("Avoid names activated by command-scoped conditionals");

    ctx.hook_cmd("pre-push").assert().success();

    assert_eq!(
        ctx.remote_ref_oid(&format!("refs/heads/{id}")).as_deref(),
        Some(ctx.head_oid().as_str())
    );
    let pushes = ctx.recorded_pushes();
    assert_eq!(pushes.len(), 2);
    assert!(pushes.iter().all(|push| recorded_remote(push) == "gherrit-publication-2"));
    assert!(pushes.iter().all(|push| {
        push.arguments().iter().all(|argument| {
            argument != "unplanned-receive-pack" && argument != "second-unplanned-receive-pack"
        })
    }));
}

#[test]
fn destination_conditioned_configured_remote_proxy_is_preserved_privately() {
    let ctx = testutil::test_context!()
        .with_remote()
        .with_initial_commit()
        .with_mock_github()
        .with_git_interceptor()
        .build();
    let destination = configured_url(&ctx, &["remote", "get-url", "origin"]);

    let include = ctx.dir.path().join("conditional-proxy.config");
    configure_file(&ctx, &include, "remote.origin.proxy", "opaque-proxy-secret");
    ctx.git_cmd()
        .args(["config", "--file"])
        .arg(&include)
        .args(["--add", "remote.origin.proxyAuthMethod", "opaque-auth-secret"])
        .assert()
        .success();
    include_when_remote_url_matches(&ctx, &destination, &include);

    ctx.checkout_managed_private("conditional-proxy");
    let id = ctx.commit_with_gherrit_id("Preserve an active configured remote proxy");

    ctx.hook_cmd("pre-push").assert().success();

    assert_eq!(
        ctx.remote_ref_oid(&format!("refs/heads/{id}")).as_deref(),
        Some(ctx.head_oid().as_str())
    );
    let pushes = ctx.recorded_pushes();
    assert_eq!(pushes.len(), 2, "initial publication requires tuple and marker pushes");
    pushes.iter().for_each(|push| {
        let arguments = push.arguments();
        assert!(arguments.iter().any(|argument| {
            argument
                == "--config-env=remote.gherrit-publication.proxy=GHERRIT_PRIVATE_REMOTE_PROXY"
        }));
        assert!(arguments.iter().any(|argument| {
            argument
                == "--config-env=remote.gherrit-publication.proxyAuthMethod=GHERRIT_PRIVATE_REMOTE_PROXY_AUTH_METHOD"
        }));
        for private in ["opaque-proxy-secret", "opaque-auth-secret"] {
            assert!(arguments.iter().all(|argument| !argument.contains(private)));
            assert!(!format!("{push:?}").contains(private));
        }
    });
}

#[test]
fn destination_conditioned_configured_remote_proxy_remains_inactive() {
    let ctx = testutil::test_context!()
        .with_remote()
        .with_initial_commit()
        .with_mock_github()
        .with_git_interceptor()
        .build();
    let destination = configured_url(&ctx, &["remote", "get-url", "origin"]);
    configure_unmatched_fetch_destination(&ctx, "proxy-conditional-fetch.git");
    ctx.set_config("remote.origin.pushurl", Some(&destination));

    let include = ctx.dir.path().join("conditional-proxy.config");
    configure_file(&ctx, &include, "remote.origin.proxy", "opaque-proxy-secret");
    ctx.git_cmd()
        .args(["config", "--file"])
        .arg(&include)
        .args(["--add", "remote.origin.proxyAuthMethod", "opaque-auth-secret"])
        .assert()
        .success();
    include_when_remote_url_matches(&ctx, &destination, &include);

    ctx.checkout_managed_private("inactive-conditional-proxy");
    let id = ctx.commit_with_gherrit_id("Ignore an inactive configured remote proxy");

    ctx.hook_cmd("pre-push").assert().success();

    assert_eq!(
        ctx.remote_ref_oid(&format!("refs/heads/{id}")).as_deref(),
        Some(ctx.head_oid().as_str())
    );
    let pushes = ctx.recorded_pushes();
    assert_eq!(pushes.len(), 2, "initial publication requires tuple and marker pushes");
    pushes.iter().for_each(|push| {
        let arguments = push.arguments();
        assert!(arguments.iter().all(|argument| !argument.contains(".proxy=")));
        assert!(arguments.iter().all(|argument| !argument.contains(".proxyAuthMethod=")));
        for private in ["opaque-proxy-secret", "opaque-auth-secret"] {
            assert!(arguments.iter().all(|argument| !argument.contains(private)));
            assert!(!format!("{push:?}").contains(private));
        }
    });
}

#[test]
fn inherited_command_scope_participates_in_conditioned_configuration() {
    let ctx = testutil::test_context!()
        .with_remote()
        .with_initial_commit()
        .with_mock_github()
        .with_git_interceptor()
        .build();
    let destination = configured_url(&ctx, &["remote", "get-url", "origin"]);
    configure_unmatched_fetch_destination(&ctx, "command-scope-fetch.git");
    ctx.set_config("remote.origin.pushurl", Some(&destination));

    let include = ctx.dir.path().join("command-scope-conditional.config");
    configure_file(
        &ctx,
        &include,
        "remote.gherrit-publication-1.receivepack",
        "conditioned-receive-pack",
    );
    include_when_remote_url_matches(&ctx, &destination, &include);

    ctx.checkout_managed_private("inherited-command-scope");
    let id = ctx.commit_with_gherrit_id("Respect inherited command-scope configuration");

    let mut command = ctx.hook_cmd("pre-push");
    command
        .env("GIT_CONFIG_COUNT", "4")
        .env("GIT_CONFIG_KEY_0", "remote.command-scope.url")
        .env("GIT_CONFIG_VALUE_0", &destination)
        .env("GIT_CONFIG_KEY_1", "remote.gherrit-publication.receivepack")
        .env("GIT_CONFIG_VALUE_1", "command-scope-receive-pack")
        .env("GIT_CONFIG_KEY_2", "remote.origin.proxy")
        .env("GIT_CONFIG_VALUE_2", "inherited-proxy-secret")
        .env("GIT_CONFIG_KEY_3", "remote.origin.proxyAuthMethod")
        .env("GIT_CONFIG_VALUE_3", "inherited-auth-secret")
        .assert()
        .success();

    assert_eq!(
        ctx.remote_ref_oid(&format!("refs/heads/{id}")).as_deref(),
        Some(ctx.head_oid().as_str())
    );
    let pushes = ctx.recorded_pushes();
    assert_eq!(pushes.len(), 2, "initial publication requires tuple and marker pushes");
    pushes.iter().for_each(|push| {
        let arguments = push.arguments();
        // The inherited URL activates the conditional second collision; both
        // names therefore participate in absent-name selection. The effective
        // configured-remote proxy values are copied to the selected name.
        assert_eq!(recorded_remote(push), "gherrit-publication-2");
        assert!(arguments.iter().any(|argument| {
            argument
                == "--config-env=remote.gherrit-publication-2.proxy=GHERRIT_PRIVATE_REMOTE_PROXY"
        }));
        assert!(arguments.iter().any(|argument| {
            argument
                == "--config-env=remote.gherrit-publication-2.proxyAuthMethod=GHERRIT_PRIVATE_REMOTE_PROXY_AUTH_METHOD"
        }));
        for private in [
            "conditioned-receive-pack",
            "command-scope-receive-pack",
            "inherited-proxy-secret",
            "inherited-auth-secret",
        ] {
            assert!(arguments.iter().all(|argument| !argument.contains(private)));
            assert!(!format!("{push:?}").contains(private));
        }
    });
}

#[test]
fn one_url_rewrite_is_bound_but_a_second_rewrite_is_rejected() {
    let stable = testutil::test_context!()
        .with_remote()
        .with_initial_commit()
        .with_mock_github()
        .with_git_interceptor()
        .build();
    let destination = configured_url(&stable, &["remote", "get-url", "origin"]);
    let replacement = destination
        .strip_suffix("owner/repo.git")
        .expect("the fixture remote ends in its repository identity");
    stable.set_config("remote.origin.url", Some("publish:owner/repo.git"));
    stable.set_config("remote.origin.pushurl", None);
    stable.set_config(&format!("url.{replacement}.insteadOf"), Some("publish:"));
    stable.checkout_managed_private("one-rewrite");
    let id = stable.commit_with_gherrit_id("Bind one configured URL rewrite");

    stable.hook_cmd("pre-push").assert().success();
    assert_eq!(
        stable.remote_ref_oid(&format!("refs/heads/{id}")).as_deref(),
        Some(stable.head_oid().as_str())
    );

    let chained = testutil::test_context!()
        .with_remote()
        .with_initial_commit()
        .with_mock_github()
        .with_git_interceptor()
        .build();
    chained.set_config("remote.origin.url", Some("rewrite-a:owner/repo.git"));
    chained.set_config("remote.origin.pushurl", None);
    chained.set_config("url.rewrite-b:.insteadOf", Some("rewrite-a:"));
    chained.set_config("url.rewrite-c:.insteadOf", Some("rewrite-b:"));
    chained.checkout_managed_private("chained-rewrite");
    chained.commit_with_gherrit_id("Reject another rewrite after resolution");

    chained
        .hook_cmd("pre-push")
        .assert()
        .failure()
        .stderr(predicate::str::contains(
            "Git URL rewrite configuration changes the resolved push destination",
        ))
        .stderr(predicate::str::contains("rewrite-a:").not())
        .stderr(predicate::str::contains("rewrite-b:").not())
        .stderr(predicate::str::contains("rewrite-c:").not());
    assert!(chained.github().requests().is_empty());
    assert!(chained.recorded_pushes().is_empty());
}

#[test]
fn repeated_configured_remote_values_fail_before_external_io() {
    for second in ["origin", "publish"] {
        let ctx = testutil::test_context!()
            .with_remote()
            .with_initial_commit()
            .with_mock_github()
            .with_git_interceptor()
            .build();
        ctx.git_cmd().args(["config", "--add", "gherrit.remote", "origin"]).assert().success();
        ctx.git_cmd().args(["config", "--add", "gherrit.remote", second]).assert().success();
        ctx.checkout_managed_private("repeated-remote");
        ctx.commit_with_gherrit_id("Reject repeated publication remotes");

        ctx.hook_cmd("pre-push")
            .assert()
            .failure()
            .stderr(predicate::str::contains("GHerrit remote is configured more than once"));

        assert!(ctx.github().requests().is_empty());
        assert!(ctx.recorded_pushes().is_empty());
    }
}

#[test]
fn option_like_configured_remote_name_is_data() {
    let ctx = testutil::test_context!()
        .with_remote()
        .with_initial_commit()
        .with_mock_github()
        .with_git_interceptor()
        .build();
    let destination = configured_url(&ctx, &["remote", "get-url", "origin"]);
    ctx.set_config("remote.-publish.url", Some(&destination));
    ctx.set_config("gherrit.remote", Some("-publish"));
    ctx.checkout_managed_private("option-like-remote");
    let id = ctx.commit_with_gherrit_id("Treat the configured name as data");

    ctx.hook_cmd("pre-push").assert().success();

    assert_eq!(
        ctx.remote_ref_oid(&format!("refs/heads/{id}")).as_deref(),
        Some(ctx.head_oid().as_str())
    );
    assert_eq!(ctx.github().pull_requests().len(), 1);
}

#[test]
fn inherited_push_configuration_cannot_add_refs_or_server_options() {
    let ctx = testutil::test_context!()
        .with_remote()
        .with_initial_commit()
        .with_mock_github()
        .with_git_interceptor()
        .build();
    ctx.checkout_managed_private("configured-push-inputs");
    let id = ctx.commit_with_gherrit_id("Publish only the planned refs");
    ctx.run_git(&["tag", "--annotate", "unplanned", "--message", "Unplanned tag"]);
    ctx.set_config("push.followTags", Some("true"));
    ctx.set_config("push.recurseSubmodules", Some("only"));
    ctx.set_config("push.pushOption", Some("unplanned-server-option"));

    ctx.hook_cmd("pre-push").assert().success();

    assert_eq!(
        ctx.remote_ref_oid(&format!("refs/heads/{id}")).as_deref(),
        Some(ctx.head_oid().as_str())
    );
    assert_eq!(ctx.remote_ref_oid("refs/tags/unplanned"), None);

    let pushes = ctx.recorded_pushes();
    assert_eq!(pushes.len(), 2);
    for push in pushes {
        let arguments = push.arguments();
        assert!(arguments.windows(2).any(|pair| pair == ["-c", "push.followTags=false"]));
        assert!(arguments.windows(2).any(|pair| pair == ["-c", "push.recurseSubmodules=no"]));
        assert!(arguments.windows(2).any(|pair| pair == ["-c", "push.pushOption="]));
        assert!(arguments.iter().all(|argument| argument != "unplanned-server-option"));
    }
}

#[test]
fn remote_and_github_default_branches_must_agree() {
    for mismatch in ["name", "tip"] {
        let ctx = testutil::test_context!()
            .with_remote()
            .with_initial_commit()
            .with_mock_github()
            .with_git_interceptor()
            .build();
        let remote_tip = ctx.remote_ref_oid("refs/heads/main").unwrap();
        ctx.checkout_managed_private(&format!("mismatched-default-{mismatch}"));
        ctx.commit_with_gherrit_id("Do not choose between repository views");
        match mismatch {
            "name" => ctx.github().set_default_branch("master", &remote_tip),
            "tip" => ctx.github().set_default_branch("main", &ctx.head_oid()),
            _ => unreachable!(),
        }

        ctx.hook_cmd("pre-push")
            .assert()
            .failure()
            .stderr(predicate::str::contains("Git and GitHub disagree"));

        assert!(ctx.github().pull_requests().is_empty());
        assert!(ctx.recorded_pushes().is_empty());
    }
}

#[test]
fn local_default_branch_must_match_the_push_repository() {
    let ctx = testutil::test_context!()
        .with_remote()
        .with_initial_commit()
        .with_mock_github()
        .with_git_interceptor()
        .build();
    ctx.checkout_managed_private("divergent-local-default");
    ctx.commit_with_gherrit_id("Reject a divergent local foundation");
    ctx.run_git(&["branch", "--force", "main", "HEAD"]);

    ctx.hook_cmd("pre-push").assert().failure().stderr(predicate::str::contains(
        "Local default branch 'main' does not match the push repository",
    ));

    assert!(ctx.github().requests().is_empty());
    assert!(ctx.recorded_pushes().is_empty());
}

#[test]
fn a_change_id_cannot_name_the_exact_default_branch() {
    let ctx = testutil::test_context!()
        .with_remote()
        .with_initial_commit()
        .with_mock_github()
        .with_git_interceptor()
        .build();
    let default_tip = ctx.remote_ref_oid("refs/heads/main").unwrap();
    ctx.checkout_managed_private("default-name-collision");
    ctx.commit_with_explicit_gherrit_id("Do not overwrite the default branch", "main");

    ctx.hook_cmd("pre-push").assert().failure().stderr(predicate::str::contains(
        "managed branch 'refs/heads/main' conflicts with repository default branch 'refs/heads/main'",
    ));

    assert!(ctx.github().requests().is_empty());
    assert!(ctx.recorded_pushes().is_empty());
    assert_eq!(ctx.remote_ref_oid("refs/heads/main").as_deref(), Some(default_tip.as_str()));
}

#[test]
fn repository_with_a_nonstandard_default_branch_publishes_against_it() {
    let ctx = testutil::test_context!()
        .with_remote()
        .with_initial_commit()
        .with_mock_github()
        .with_git_interceptor()
        .build();
    ctx.run_git(&["branch", "--move", "main", "master"]);
    ctx.remote_git_cmd()
        .args(["update-ref", "refs/heads/master", "refs/heads/main"])
        .assert()
        .success();
    ctx.remote_git_cmd().args(["symbolic-ref", "HEAD", "refs/heads/master"]).assert().success();
    ctx.remote_git_cmd().args(["update-ref", "-d", "refs/heads/main"]).assert().success();
    ctx.checkout_managed_private("master-root");
    ctx.commit_with_gherrit_id("Publish from the observed default branch");

    ctx.hook_cmd("pre-push").assert().success();

    let pull_requests = ctx.github().pull_requests();
    assert_eq!(pull_requests.len(), 1);
    assert_eq!(pull_requests[0].base, "master");
}

#[test]
fn repository_default_cannot_occupy_the_owned_base_namespace() {
    let ctx = testutil::test_context!()
        .with_remote()
        .with_initial_commit()
        .with_mock_github()
        .with_git_interceptor()
        .build();
    let default_branch = "gherrit-bases/primary";
    ctx.run_git(&["branch", "--move", "main", default_branch]);
    ctx.remote_git_cmd()
        .args(["update-ref", &format!("refs/heads/{default_branch}"), "refs/heads/main"])
        .assert()
        .success();
    ctx.remote_git_cmd()
        .args(["symbolic-ref", "HEAD", &format!("refs/heads/{default_branch}")])
        .assert()
        .success();
    ctx.remote_git_cmd().args(["update-ref", "-d", "refs/heads/main"]).assert().success();
    ctx.checkout_managed_private("reserved-default-root");
    ctx.commit_with_gherrit_id("Reject an ambiguous default namespace");

    ctx.hook_cmd("pre-push").assert().failure().stderr(predicate::str::contains(
        "repository default branch is in GHerrit's reserved 'gherrit-bases' namespace",
    ));

    assert!(ctx.github().requests().is_empty());
    assert!(ctx.recorded_pushes().is_empty());
}

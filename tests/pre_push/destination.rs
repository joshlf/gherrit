#[cfg(unix)]
use std::io::Write as _;
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

fn assert_private_remote(push: &testutil::PushRecord, literal: &str) {
    assert!(recorded_remote(push).starts_with("gherrit-publication"));
    assert!(push.arguments().iter().all(|argument| !argument.contains(literal)));
    assert!(!format!("{push:?}").contains(literal));
}

fn assert_private_remote_arguments(arguments: &[String], literals: &[&str]) {
    let separator = arguments.iter().position(|argument| argument == "--").expect("command has --");
    assert!(arguments[separator + 1].starts_with("gherrit-publication"));
    assert!(
        literals.iter().all(|literal| arguments.iter().all(|argument| !argument.contains(literal)))
    );
}

#[cfg(unix)]
#[test]
fn backslash_in_a_unix_local_path_is_not_a_separator() {
    let ctx = testutil::test_context!()
        .with_remote()
        .with_initial_commit()
        .with_mock_github()
        .with_git_interceptor()
        .build();
    let parent = ctx.dir.path().join("misleading-owner");
    fs::create_dir(&parent).unwrap();
    let destination = parent.join(r"different-owner\repo.git");
    ctx.init_bare_repo(&destination);
    ctx.set_config("remote.origin.pushurl", destination.to_str());
    ctx.checkout_managed_private("backslash-destination");
    ctx.commit_with_gherrit_id("Keep Unix filename bytes literal");

    ctx.hook_cmd("pre-push")
        .assert()
        .failure()
        .stderr(predicate::str::contains("does not identify a supported GitHub repository"));

    assert!(ctx.recorded_pushes().is_empty());
    assert!(ctx.github().requests().is_empty());
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
    ctx.set_config("remote.gherrit-publication.url", Some(&fetch_destination));

    ctx.checkout_managed_private("split-remote");
    let id = ctx.commit_with_gherrit_id("Publish through the push destination");
    let managed_ref = format!("refs/heads/{id}");
    let v1_ref = format!("refs/tags/gherrit/{id}/v1");
    let v2_ref = format!("refs/tags/gherrit/{id}/v2");
    let local_high_ref = format!("refs/tags/gherrit/{id}/v999");
    let fetch_high_ref = format!("refs/tags/gherrit/{id}/v77");
    let push_v1_oid = ctx.remote_ref_oid("refs/heads/main").unwrap();
    ctx.remote_git_cmd().args(["update-ref", &managed_ref, "refs/heads/main"]).assert().success();
    ctx.remote_git_cmd().args(["update-ref", &v1_ref, "refs/heads/main"]).assert().success();

    // Fetch-side and local version state deliberately disagree with the push
    // destination. Neither may participate in authority.
    let fetch_git_dir = format!("--git-dir={}", fetch_repository.display());
    ctx.git_cmd()
        .args([
            &fetch_git_dir,
            "fetch",
            "--no-tags",
            &push_destination,
            "refs/heads/main:refs/heads/main",
        ])
        .assert()
        .success();
    for ref_name in [&managed_ref, &fetch_high_ref] {
        ctx.git_cmd()
            .args([&fetch_git_dir, "update-ref", ref_name, "refs/heads/main"])
            .assert()
            .success();
    }
    ctx.git_cmd().args(["update-ref", &local_high_ref, "HEAD"]).assert().success();

    ctx.hook_cmd("pre-push").assert().success();

    let pushes = ctx.recorded_pushes();
    assert_eq!(pushes.len(), 1);
    assert_private_remote(&pushes[0], &push_destination);
    assert_eq!(recorded_remote(&pushes[0]), "gherrit-publication-1");
    assert_eq!(ctx.remote_ref_oid(&managed_ref).as_deref(), Some(ctx.head_oid().as_str()));
    assert_eq!(ctx.remote_ref_oid(&v1_ref).as_deref(), Some(push_v1_oid.as_str()));
    assert_eq!(ctx.remote_ref_oid(&v2_ref).as_deref(), Some(ctx.head_oid().as_str()));
    assert_eq!(ctx.remote_ref_oid(&local_high_ref), None);
    assert_eq!(
        bare_ref_oid(&ctx, &fetch_repository, &managed_ref).as_deref(),
        Some(push_v1_oid.as_str())
    );
    assert_eq!(bare_ref_oid(&ctx, &fetch_repository, &v2_ref), None);
    assert_eq!(ctx.github().repository(), ("push-owner".to_string(), "push-repo".to_string()));
    assert_eq!(ctx.github().pull_requests().len(), 1);

    let head_queries =
        ctx.recorded_git_invocations(testutil::GitOperation::LsRemoteManagedBranches);
    let version_queries =
        ctx.recorded_git_invocations(testutil::GitOperation::LsRemoteActiveVersions);
    assert_eq!(head_queries.len(), 1);
    assert_eq!(version_queries.len(), 1);
    for arguments in head_queries.iter().chain(&version_queries) {
        assert_private_remote_arguments(arguments, &[&push_destination, &fetch_destination]);
    }
    assert!(head_queries[0].contains(&managed_ref));
    assert!(head_queries[0].contains(&format!("refs/heads/gherrit-bases/{id}")));
    assert!(version_queries[0].contains(&format!("refs/tags/gherrit/{id}")));
    assert!(version_queries[0].contains(&format!("refs/tags/gherrit/{id}/*")));
}

#[test]
fn empty_push_destination_starts_at_v1_despite_fetch_and_local_tags() {
    let ctx = testutil::test_context!()
        .repository("push-owner", "push-repo")
        .with_remote()
        .with_initial_commit()
        .with_mock_github()
        .with_git_interceptor()
        .build();
    let push_destination = configured_url(&ctx, &["remote", "get-url", "origin"]);

    let fetch_parent = ctx.dir.path().join("old-fetch-owner");
    fs::create_dir(&fetch_parent).unwrap();
    let fetch_repository = fetch_parent.join("old-fetch-repo.git");
    ctx.init_bare_repo(&fetch_repository);
    let fetch_destination =
        fetch_repository.to_string_lossy().replace(std::path::MAIN_SEPARATOR, "/");
    ctx.set_config("remote.origin.url", Some(&fetch_destination));
    ctx.set_config("remote.origin.pushurl", Some(&push_destination));

    ctx.checkout_managed_private("split-empty-push");
    let id = ctx.commit_with_gherrit_id("Start at the push destination");
    let managed_ref = format!("refs/heads/{id}");
    let v1_ref = format!("refs/tags/gherrit/{id}/v1");
    let old_ref = format!("refs/tags/gherrit/{id}/v42");

    let fetch_git_dir = format!("--git-dir={}", fetch_repository.display());
    ctx.git_cmd()
        .args([
            &fetch_git_dir,
            "fetch",
            "--no-tags",
            &push_destination,
            "refs/heads/main:refs/heads/main",
        ])
        .assert()
        .success();
    for ref_name in [&managed_ref, &old_ref] {
        ctx.git_cmd()
            .args([&fetch_git_dir, "update-ref", ref_name, "refs/heads/main"])
            .assert()
            .success();
    }
    ctx.git_cmd().args(["update-ref", &old_ref, "HEAD"]).assert().success();

    ctx.hook_cmd("pre-push").assert().success();

    assert_eq!(ctx.remote_ref_oid(&managed_ref).as_deref(), Some(ctx.head_oid().as_str()));
    assert_eq!(ctx.remote_ref_oid(&v1_ref).as_deref(), Some(ctx.head_oid().as_str()));
    assert_eq!(ctx.remote_ref_oid(&old_ref), None);
    assert!(bare_ref_oid(&ctx, &fetch_repository, &old_ref).is_some());
}

#[test]
fn baseline_keys_and_duplicate_destinations_force_a_fresh_internal_name() {
    let ctx = testutil::test_context!()
        .with_remote()
        .with_initial_commit()
        .with_mock_github()
        .with_git_interceptor()
        .build();
    let destination = configured_url(&ctx, &["remote", "get-url", "origin"]);
    let trap = ctx.dir.path().join("baseline-collision.git");
    ctx.init_bare_repo(&trap);
    let trap = trap.to_string_lossy().replace(std::path::MAIN_SEPARATOR, "/");
    for key in [
        "ReMoTe.GhErRiT-PuBlIcAtIoN.url",
        "remote.gherrit-publication.url",
        "remote.gherrit-publication.pushurl",
        "remote.gherrit-publication.pushURL",
    ] {
        ctx.git_cmd().args(["config", "--add", key, &trap]).assert().success();
    }
    ctx.checkout_managed_private("duplicate-baseline-destinations");
    let id = ctx.commit_with_gherrit_id("Choose an unconfigured publication remote");

    ctx.hook_cmd("pre-push").assert().success();

    assert_eq!(
        ctx.remote_ref_oid(&format!("refs/heads/{id}")).as_deref(),
        Some(ctx.head_oid().as_str())
    );
    let pushes = ctx.recorded_pushes();
    assert_eq!(pushes.len(), 1);
    assert_private_remote(&pushes[0], &destination);
    assert_eq!(recorded_remote(&pushes[0]), "gherrit-publication-1");
}

#[test]
fn similarly_prefixed_configuration_does_not_block_the_first_internal_name() {
    let ctx = testutil::test_context!()
        .with_remote()
        .with_initial_commit()
        .with_mock_github()
        .with_git_interceptor()
        .build();
    let destination = configured_url(&ctx, &["remote", "get-url", "origin"]);
    ctx.set_config("remote.gherrit-publication-extra.receivepack", Some("unrelated-receive-pack"));
    ctx.checkout_managed_private("similarly-prefixed-remote");
    ctx.commit_with_gherrit_id("Ignore a different remote name");

    ctx.hook_cmd("pre-push").assert().success();

    let pushes = ctx.recorded_pushes();
    assert_eq!(pushes.len(), 1);
    assert_private_remote(&pushes[0], &destination);
    assert_eq!(recorded_remote(&pushes[0]), "gherrit-publication");
}

#[test]
fn fetch_url_is_the_single_push_destination_when_pushurl_is_absent() {
    let ctx = testutil::test_context!()
        .with_remote()
        .with_initial_commit()
        .with_mock_github()
        .with_git_interceptor()
        .build();
    let fetch_destination = configured_url(&ctx, &["remote", "get-url", "origin"]);
    assert_eq!(
        ctx.git_cmd()
            .args(["config", "--get-all", "remote.origin.pushurl"])
            .output()
            .unwrap()
            .status
            .code(),
        Some(1)
    );
    ctx.checkout_managed_private("fetch-fallback");
    ctx.commit_with_gherrit_id("Use the ordinary remote URL");

    ctx.hook_cmd("pre-push").assert().success();

    let pushes = ctx.recorded_pushes();
    assert_eq!(pushes.len(), 1);
    assert_private_remote(&pushes[0], &fetch_destination);
}

#[test]
fn git_observation_does_not_follow_an_http_redirect() {
    let ctx = testutil::test_context!()
        .with_remote()
        .with_initial_commit()
        .with_mock_github()
        .with_git_interceptor()
        .build();
    let destination = format!("{}/owner/repo.git", ctx.mock_server_url());
    ctx.set_config("remote.origin.pushurl", Some(&destination));
    ctx.checkout_managed_private("redirecting-destination");
    ctx.commit_with_gherrit_id("Do not follow a repository redirect");

    ctx.hook_cmd("pre-push")
        .assert()
        .failure()
        .stderr(predicate::str::contains(
            "`git ls-remote --symref` failed for GHerrit remote 'origin'",
        ))
        .stderr(predicate::str::contains(&destination).not());

    assert_eq!(ctx.github().git_redirect_source_requests(), 1);
    assert_eq!(ctx.github().git_redirect_trap_requests(), 0);
    assert!(ctx.github().requests().is_empty());
    assert!(ctx.recorded_pushes().is_empty());
}

#[test]
fn url_scoped_http_redirect_configuration_is_rejected_without_disclosure() {
    let ctx = testutil::test_context!()
        .with_remote()
        .with_initial_commit()
        .with_mock_github()
        .with_git_interceptor()
        .build();
    let destination = format!("{}/owner/repo.git", ctx.mock_server_url());
    let scoped_key = format!("HtTp.{destination}.FoLlOwReDiReCtS");
    ctx.set_config("remote.origin.pushurl", Some(&destination));
    ctx.set_config(&scoped_key, Some("true"));

    // This checks the reason for the production guard: Git's URL matcher lets
    // the scoped local value outrank a global command-line value.
    ctx.git_cmd()
        .args([
            "-c",
            "http.followRedirects=false",
            "config",
            "--get-urlmatch",
            "http.followRedirects",
            &destination,
        ])
        .assert()
        .success()
        .stdout("true\n");

    ctx.checkout_managed_private("hostile-redirect-configuration");
    ctx.commit_with_gherrit_id("Reject a scoped redirect override");

    ctx.hook_cmd("pre-push")
        .assert()
        .failure()
        .stderr(predicate::str::contains(
            "Git HTTP redirect configuration does not disable redirects",
        ))
        .stderr(predicate::str::contains(&destination).not())
        .stderr(predicate::str::contains(&scoped_key).not());

    assert_eq!(ctx.github().git_redirect_source_requests(), 0);
    assert_eq!(ctx.github().git_redirect_trap_requests(), 0);
    assert!(ctx.github().requests().is_empty());
    assert!(ctx.recorded_pushes().is_empty());
}

#[test]
fn unrelated_url_scoped_redirect_configuration_is_accepted() {
    let ctx = testutil::test_context!()
        .with_remote()
        .with_initial_commit()
        .with_mock_github()
        .with_git_interceptor()
        .build();
    let destination = format!("{}/owner/repo.git", ctx.mock_server_url());
    ctx.set_config("remote.origin.pushurl", Some(&destination));
    ctx.set_config("http.https://unrelated.invalid/.followRedirects", Some("true"));
    ctx.checkout_managed_private("unrelated-redirect-configuration");
    ctx.commit_with_gherrit_id("Ignore an unrelated redirect override");

    ctx.hook_cmd("pre-push").assert().failure().stderr(predicate::str::contains(
        "`git ls-remote --symref` failed for GHerrit remote 'origin'",
    ));

    assert_eq!(ctx.github().git_redirect_source_requests(), 1);
    assert_eq!(ctx.github().git_redirect_trap_requests(), 0);
    assert!(ctx.github().requests().is_empty());
    assert!(ctx.recorded_pushes().is_empty());
}

#[test]
fn every_false_redirect_spelling_is_accepted() {
    for value in ["false", "no", "off", "0"] {
        let ctx = testutil::test_context!()
            .with_remote()
            .with_initial_commit()
            .with_mock_github()
            .with_git_interceptor()
            .build();
        let destination = format!("{}/owner/repo.git", ctx.mock_server_url());
        let scoped_key = format!("http.{destination}.followRedirects");
        ctx.set_config("remote.origin.pushurl", Some(&destination));
        ctx.set_config(&scoped_key, Some(value));
        ctx.checkout_managed_private(&format!("false-redirect-{value}"));
        ctx.commit_with_gherrit_id("Accept a disabled redirect policy");

        ctx.hook_cmd("pre-push").assert().failure().stderr(predicate::str::contains(
            "`git ls-remote --symref` failed for GHerrit remote 'origin'",
        ));

        assert_eq!(ctx.github().git_redirect_source_requests(), 1, "value: {value}");
        assert_eq!(ctx.github().git_redirect_trap_requests(), 0, "value: {value}");
    }
}

#[test]
fn non_boolean_redirect_policies_fail_closed() {
    for value in ["initial", "not-a-boolean"] {
        let ctx = testutil::test_context!()
            .with_remote()
            .with_initial_commit()
            .with_mock_github()
            .with_git_interceptor()
            .build();
        let destination = format!("{}/owner/repo.git", ctx.mock_server_url());
        let scoped_key = format!("http.{destination}.followRedirects");
        ctx.set_config("remote.origin.pushurl", Some(&destination));
        ctx.set_config(&scoped_key, Some(value));
        ctx.checkout_managed_private(&format!("invalid-redirect-{value}"));
        ctx.commit_with_gherrit_id("Reject an ambiguous redirect policy");

        ctx.hook_cmd("pre-push")
            .assert()
            .failure()
            .stderr(predicate::str::contains(
                "Git HTTP redirect configuration does not disable redirects",
            ))
            .stderr(predicate::str::contains(&destination).not());

        assert_eq!(ctx.github().git_redirect_source_requests(), 0, "value: {value}");
        assert!(ctx.recorded_pushes().is_empty());
    }
}

#[test]
fn redirect_policy_lookup_failure_stops_before_external_io() {
    let ctx = testutil::test_context!()
        .with_remote()
        .with_initial_commit()
        .with_mock_github()
        .with_git_interceptor()
        .build();
    let destination = format!("{}/owner/repo.git", ctx.mock_server_url());
    ctx.set_config("remote.origin.pushurl", Some(&destination));
    ctx.checkout_managed_private("redirect-policy-failure");
    ctx.commit_with_gherrit_id("Fail closed when redirect policy is unreadable");
    ctx.expect_git_failure(testutil::GitOperation::HttpRedirectPolicy);

    ctx.hook_cmd("pre-push")
        .assert()
        .failure()
        .stderr(predicate::str::contains(
            "Git HTTP redirect configuration does not disable redirects",
        ))
        .stderr(predicate::str::contains(&destination).not());

    ctx.assert_failure_consumed();
    assert_eq!(ctx.github().git_redirect_source_requests(), 0);
    assert!(ctx.github().requests().is_empty());
    assert!(ctx.recorded_pushes().is_empty());
}

#[test]
fn uri_user_information_is_rejected_before_external_io() {
    for destination in [
        "https://token-secret@github.com/owner/repo.git",
        "ssh://git@github.com/owner/repo.git",
        "file://user:password-secret@localhost/tmp/owner/repo.git",
    ] {
        let ctx = testutil::test_context!()
            .with_remote()
            .with_initial_commit()
            .with_mock_github()
            .with_git_interceptor()
            .build();
        ctx.set_config("remote.origin.pushurl", Some(destination));
        ctx.checkout_managed_private("unsupported-uri-user-information");
        ctx.commit_with_gherrit_id("Require credentials outside the destination");

        ctx.hook_cmd("pre-push")
            .assert()
            .failure()
            .stderr(predicate::str::contains("contains URI user information"))
            .stderr(predicate::str::contains(
                "use a Git credential helper or an SCP-style SSH destination",
            ))
            .stderr(predicate::str::contains(destination).not())
            .stderr(predicate::str::contains("token-secret").not())
            .stderr(predicate::str::contains("password-secret").not());

        assert_eq!(ctx.github().git_redirect_source_requests(), 0);
        assert!(ctx.github().requests().is_empty());
        assert!(ctx.recorded_pushes().is_empty());
    }
}

#[test]
fn a_conditional_include_cannot_hide_a_redirect_override() {
    let ctx = testutil::test_context!()
        .with_remote()
        .with_initial_commit()
        .with_mock_github()
        .with_git_interceptor()
        .build();
    let destination = format!("{}/owner/repo.git", ctx.mock_server_url());
    ctx.set_config("remote.origin.pushurl", Some(&destination));
    configure_unmatched_fetch_destination(&ctx, "unmatched-fetch.git");

    let include = ctx.dir.path().join("redirect-override.config");
    let scoped_key = format!("http.{destination}.followRedirects");
    configure_file(&ctx, &include, &scoped_key, "true");
    include_when_remote_url_matches(&ctx, &destination, &include);

    // The include is inactive while the configured remote is inspected and
    // becomes active only when the private remote receives this destination.
    ctx.git_cmd()
        .args([
            "-c",
            &format!("remote.gherrit-publication.url={destination}"),
            "config",
            "--get-urlmatch",
            "http.followRedirects",
            &destination,
        ])
        .assert()
        .success()
        .stdout("true\n");

    ctx.checkout_managed_private("conditional-redirect-override");
    ctx.commit_with_gherrit_id("Inspect the network command's exact configuration");

    ctx.hook_cmd("pre-push")
        .assert()
        .failure()
        .stderr(predicate::str::contains(
            "Git HTTP redirect configuration does not disable redirects",
        ))
        .stderr(predicate::str::contains(&destination).not());

    assert_eq!(ctx.github().git_redirect_source_requests(), 0);
    assert_eq!(ctx.github().git_redirect_trap_requests(), 0);
    assert!(ctx.recorded_pushes().is_empty());
}

#[test]
fn inherited_git_transport_diagnostics_are_removed_from_children() {
    let ctx = testutil::test_context!()
        .with_remote()
        .with_initial_commit()
        .with_mock_github()
        .with_git_interceptor()
        .build();
    let destination = configured_url(&ctx, &["remote", "get-url", "origin"]);
    ctx.checkout_managed_private("private-transport-diagnostics");
    ctx.commit_with_gherrit_id("Remove inherited Git transport diagnostics");
    let trace = ctx.dir.path().join("git-trace.log");
    let trace_curl = ctx.dir.path().join("git-trace-curl.log");
    let trace2 = ctx.dir.path().join("git-trace2.json");

    ctx.hook_cmd("pre-push")
        .env("GIT_TRACE", &trace)
        .env("GIT_TRACE_CURL", &trace_curl)
        .env("GIT_TRACE2_EVENT", &trace2)
        .env("GIT_CURL_VERBOSE", "1")
        .assert()
        .success();

    let retained = [trace, trace_curl, trace2]
        .into_iter()
        .filter_map(|path| fs::read(path).ok())
        .flatten()
        .collect::<Vec<_>>();
    let retained = String::from_utf8_lossy(&retained);
    assert!(!retained.contains(&destination));
    assert!(!retained.contains("gherrit-publication"));
    assert!(!retained.contains("ls-remote"));
    assert!(!retained.contains(" push "));
}

#[test]
fn one_instead_of_rewrite_resolves_to_a_stable_destination() {
    let ctx = testutil::test_context!()
        .with_remote()
        .with_initial_commit()
        .with_mock_github()
        .with_git_interceptor()
        .build();
    let destination = configured_url(&ctx, &["remote", "get-url", "origin"]);
    let replacement = destination
        .strip_suffix("owner/repo.git")
        .expect("the fixture remote ends in its repository identity");
    ctx.set_config("remote.origin.url", Some("publish:owner/repo.git"));
    ctx.set_config("remote.origin.pushurl", None);
    ctx.set_config(&format!("url.{replacement}.insteadOf"), Some("publish:"));
    ctx.checkout_managed_private("one-instead-of");
    ctx.commit_with_gherrit_id("Publish through one fetch rewrite");

    ctx.hook_cmd("pre-push").assert().success();

    let pushes = ctx.recorded_pushes();
    assert_eq!(pushes.len(), 1);
    assert_private_remote(&pushes[0], &destination);
}

#[test]
fn one_push_instead_of_rewrite_resolves_to_a_stable_destination() {
    let ctx = testutil::test_context!()
        .with_remote()
        .with_initial_commit()
        .with_mock_github()
        .with_git_interceptor()
        .build();
    let destination = configured_url(&ctx, &["remote", "get-url", "origin"]);
    let replacement = destination
        .strip_suffix("owner/repo.git")
        .expect("the fixture remote ends in its repository identity");
    ctx.set_config("remote.origin.url", Some("publish:owner/repo.git"));
    ctx.set_config("remote.origin.pushurl", None);
    ctx.set_config(&format!("url.{replacement}.pushInsteadOf"), Some("publish:"));
    ctx.checkout_managed_private("one-push-instead-of");
    ctx.commit_with_gherrit_id("Publish through one push rewrite");

    ctx.hook_cmd("pre-push").assert().success();

    let pushes = ctx.recorded_pushes();
    assert_eq!(pushes.len(), 1);
    assert_private_remote(&pushes[0], &destination);
}

#[test]
fn a_destination_which_is_also_a_remote_name_uses_the_literal_repository() {
    let ctx = testutil::test_context!()
        .with_remote()
        .with_initial_commit()
        .with_mock_github()
        .with_git_interceptor()
        .build();
    let source = configured_url(&ctx, &["remote", "get-url", "origin"]);

    let destination_parent = ctx.repo_path.join("owner");
    fs::create_dir(&destination_parent).unwrap();
    let destination = destination_parent.join("repo");
    ctx.git_cmd().args(["clone", "--bare", &source]).arg(&destination).assert().success();

    let colliding_remote = ctx.dir.path().join("colliding-remote.git");
    ctx.git_cmd().args(["clone", "--bare", &source]).arg(&colliding_remote).assert().success();
    let colliding_url = colliding_remote.to_string_lossy().replace(std::path::MAIN_SEPARATOR, "/");

    // Git accepts a manually configured remote name containing a slash. Using
    // `owner/repo` directly as a repository argument would select this remote
    // and ignore the relative repository path with the same spelling.
    ctx.set_config("remote.origin.pushurl", Some("owner/repo"));
    ctx.set_config("remote.owner/repo.url", Some(&colliding_url));
    ctx.checkout_managed_private("remote-name-collision");
    let id = ctx.commit_with_gherrit_id("Do not follow a colliding remote");
    let managed_ref = format!("refs/heads/{id}");

    // The mock GitHub server validates PR heads against its original fixture
    // repository, not this alternate destination. Reaching PR creation and
    // failing there proves the Git push itself selected the literal path.
    ctx.hook_cmd("pre-push")
        .assert()
        .failure()
        .stderr(predicate::str::contains("Head branch"))
        .stderr(predicate::str::contains("does not exist"));

    assert_eq!(
        bare_ref_oid(&ctx, &destination, &managed_ref).as_deref(),
        Some(ctx.head_oid().as_str())
    );
    assert_eq!(bare_ref_oid(&ctx, &colliding_remote, &managed_ref), None);
    let pushes = ctx.recorded_pushes();
    assert_eq!(pushes.len(), 1);
    assert_private_remote(&pushes[0], "owner/repo");
}

#[test]
fn missing_push_destination_is_rejected_before_external_writes() {
    let ctx = testutil::test_context!()
        .with_remote()
        .with_initial_commit()
        .with_mock_github()
        .with_git_interceptor()
        .build();
    ctx.run_git(&["remote", "remove", "origin"]);
    ctx.checkout_managed_private("missing-push-destination");
    ctx.commit_with_gherrit_id("Cannot publish");

    ctx.hook_cmd("pre-push").assert().failure().stderr(predicate::str::contains(
        "GHerrit remote 'origin' has no resolvable push destination",
    ));

    assert!(ctx.github().requests().is_empty());
    assert!(ctx.recorded_pushes().is_empty());
}

#[test]
fn multiple_push_destinations_are_rejected_without_disclosure_or_writes() {
    let ctx = testutil::test_context!()
        .with_remote()
        .with_initial_commit()
        .with_mock_github()
        .with_git_interceptor()
        .build();
    let first = "https://user:first-secret@example.invalid/owner/one.git";
    let second = "https://user:second-secret@example.invalid/owner/two.git";
    ctx.run_git(&["config", "--add", "remote.origin.pushurl", first]);
    ctx.run_git(&["config", "--add", "remote.origin.pushurl", second]);
    ctx.checkout_managed_private("multiple-push-destinations");
    ctx.commit_with_gherrit_id("Cannot publish atomically");

    ctx.hook_cmd("pre-push")
        .assert()
        .failure()
        .stderr(predicate::str::contains(
            "GHerrit remote 'origin' has 2 push destinations; exactly one is required",
        ))
        .stderr(predicate::str::contains("first-secret").not())
        .stderr(predicate::str::contains("second-secret").not());

    assert!(ctx.github().requests().is_empty());
    assert!(ctx.recorded_pushes().is_empty());
}

#[test]
fn a_second_instead_of_rewrite_is_rejected_before_external_io() {
    let ctx = testutil::test_context!()
        .with_remote()
        .with_initial_commit()
        .with_mock_github()
        .with_git_interceptor()
        .build();
    ctx.set_config("remote.origin.url", Some("rewrite-a:owner/repo.git"));
    ctx.set_config("remote.origin.pushurl", None);
    ctx.set_config("url.rewrite-b:.insteadOf", Some("rewrite-a:"));
    ctx.set_config("url.rewrite-c:.insteadOf", Some("rewrite-b:"));
    ctx.checkout_managed_private("chained-instead-of");
    ctx.commit_with_gherrit_id("Reject a second fetch rewrite");

    ctx.hook_cmd("pre-push")
        .assert()
        .failure()
        .stderr(predicate::str::contains(
            "Git URL rewrite configuration changes the resolved push destination",
        ))
        .stderr(predicate::str::contains("rewrite-a:").not())
        .stderr(predicate::str::contains("rewrite-b:").not())
        .stderr(predicate::str::contains("rewrite-c:").not());

    assert!(ctx.github().requests().is_empty());
    assert!(ctx.recorded_pushes().is_empty());
}

#[test]
fn push_instead_of_does_not_rewrite_an_explicit_internal_push_url() {
    let ctx = testutil::test_context!()
        .with_remote()
        .with_initial_commit()
        .with_mock_github()
        .with_git_interceptor()
        .build();
    let destination = configured_url(&ctx, &["remote", "get-url", "origin"]);
    let trap_parent = ctx.dir.path().join("rewrite-trap");
    fs::create_dir(&trap_parent).unwrap();
    let trap = trap_parent.join("repo.git");
    ctx.init_bare_repo(&trap);
    let trap_url = trap.to_string_lossy().replace(std::path::MAIN_SEPARATOR, "/");

    // Git does not apply pushInsteadOf when a remote has an explicit pushurl.
    // The private internal remote defines both url and pushurl, so this rule is
    // harmless even though it matches the resolved destination exactly.
    ctx.set_config("remote.origin.pushurl", Some(&destination));
    ctx.set_config(&format!("url.{trap_url}.pushInsteadOf"), Some(&destination));
    ctx.checkout_managed_private("explicit-pushurl");
    let id = ctx.commit_with_gherrit_id("Keep the explicit push destination");
    let managed_ref = format!("refs/heads/{id}");

    ctx.hook_cmd("pre-push").assert().success();

    assert_eq!(ctx.remote_ref_oid(&managed_ref).as_deref(), Some(ctx.head_oid().as_str()));
    assert_eq!(bare_ref_oid(&ctx, &trap, &managed_ref), None);
    let pushes = ctx.recorded_pushes();
    assert_eq!(pushes.len(), 1);
    assert_private_remote(&pushes[0], &destination);
}

#[test]
fn probe_activated_push_destination_configuration_forces_a_fresh_final_name() {
    let ctx = testutil::test_context!()
        .with_remote()
        .with_initial_commit()
        .with_mock_github()
        .with_git_interceptor()
        .build();
    let destination = configured_url(&ctx, &["remote", "get-url", "origin"]);
    configure_unmatched_fetch_destination(&ctx, "conditional-fetch.git");
    ctx.set_config("remote.origin.pushurl", Some(&destination));

    let trap = ctx.dir.path().join("conditional-push-trap.git");
    ctx.init_bare_repo(&trap);
    let trap_url = trap.to_string_lossy().replace(std::path::MAIN_SEPARATOR, "/");
    let include = ctx.dir.path().join("conditional-push.config");
    configure_file(&ctx, &include, "remote.gherrit-publication.pushurl", &trap_url);
    include_when_remote_url_matches(&ctx, &destination, &include);

    ctx.checkout_managed_private("conditional-push-destination");
    let id = ctx.commit_with_gherrit_id("Avoid an included push destination");
    let managed_ref = format!("refs/heads/{id}");

    ctx.hook_cmd("pre-push").assert().success();

    assert_eq!(ctx.remote_ref_oid(&managed_ref).as_deref(), Some(ctx.head_oid().as_str()));
    assert_eq!(bare_ref_oid(&ctx, &trap, &managed_ref), None);
    let pushes = ctx.recorded_pushes();
    assert_eq!(pushes.len(), 1);
    assert_private_remote(&pushes[0], &destination);
    assert_eq!(recorded_remote(&pushes[0]), "gherrit-publication-1");
}

#[test]
fn probe_activated_remote_configuration_forces_a_fresh_final_name() {
    let ctx = testutil::test_context!()
        .with_remote()
        .with_initial_commit()
        .with_mock_github()
        .with_git_interceptor()
        .build();
    let destination = configured_url(&ctx, &["remote", "get-url", "origin"]);
    configure_unmatched_fetch_destination(&ctx, "private-remote-fetch.git");
    ctx.set_config("remote.origin.pushurl", Some(&destination));

    let include = ctx.dir.path().join("private-remote.config");
    configure_file(
        &ctx,
        &include,
        "remote.gherrit-publication.receivepack",
        "malicious-receive-pack",
    );
    include_when_remote_url_matches(&ctx, &destination, &include);
    ctx.checkout_managed_private("conditional-private-remote");
    let id = ctx.commit_with_gherrit_id("Avoid hidden private remote behavior");
    let managed_ref = format!("refs/heads/{id}");

    ctx.hook_cmd("pre-push").assert().success();

    assert_eq!(ctx.remote_ref_oid(&managed_ref).as_deref(), Some(ctx.head_oid().as_str()));
    let pushes = ctx.recorded_pushes();
    assert_eq!(pushes.len(), 1);
    assert_private_remote(&pushes[0], &destination);
    assert_eq!(recorded_remote(&pushes[0]), "gherrit-publication-1");
}

#[test]
fn a_failed_local_destination_does_not_disclose_any_path_spelling() {
    let ctx = testutil::test_context!()
        .with_remote()
        .with_initial_commit()
        .with_mock_github()
        .with_git_interceptor()
        .build();
    ctx.set_config(
        "remote.origin.pushurl",
        Some("./raw-secret/../normalized-secret/owner/repo.git"),
    );
    ctx.checkout_managed_private("undisclosed-local-destination");
    ctx.commit_with_gherrit_id("Keep local destination paths private");

    ctx.hook_cmd("pre-push")
        .assert()
        .failure()
        .stderr(predicate::str::contains(
            "`git ls-remote --symref` failed for GHerrit remote 'origin'",
        ))
        .stderr(predicate::str::contains("raw-secret").not())
        .stderr(predicate::str::contains("normalized-secret").not())
        .stderr(predicate::str::contains("repo.git").not());

    assert!(ctx.github().requests().is_empty());
    assert!(ctx.recorded_pushes().is_empty());
}

#[test]
fn an_option_like_remote_name_is_resolved_as_data() {
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
    ctx.commit_with_gherrit_id("Treat the remote name as data");

    ctx.hook_cmd("pre-push").assert().success();

    let pushes = ctx.recorded_pushes();
    assert_eq!(pushes.len(), 1);
    assert_private_remote(&pushes[0], &destination);
}

#[test]
fn a_remote_name_containing_an_equals_sign_is_resolved_as_data() {
    let ctx = testutil::test_context!()
        .with_remote()
        .with_initial_commit()
        .with_mock_github()
        .with_git_interceptor()
        .build();
    let destination = configured_url(&ctx, &["remote", "get-url", "origin"]);
    ctx.set_config("remote.publish=primary.url", Some(&destination));
    ctx.set_config("gherrit.remote", Some("publish=primary"));
    ctx.checkout_managed_private("equals-remote");
    ctx.commit_with_gherrit_id("Treat every validated remote name as data");

    ctx.hook_cmd("pre-push").assert().success();

    let pushes = ctx.recorded_pushes();
    assert_eq!(pushes.len(), 1);
    assert_private_remote(&pushes[0], &destination);
}

#[test]
fn a_control_character_in_the_remote_name_fails_before_external_io() {
    let ctx = testutil::test_context!()
        .with_remote()
        .with_initial_commit()
        .with_mock_github()
        .with_git_interceptor()
        .build();
    ctx.set_config("gherrit.remote", Some("publish\nelsewhere"));
    ctx.checkout_managed_private("control-remote");
    ctx.commit_with_gherrit_id("Reject an ambiguous remote name");

    ctx.hook_cmd("pre-push")
        .assert()
        .failure()
        .stderr(predicate::str::contains("configured GHerrit remote contains a control character"));

    assert!(ctx.github().requests().is_empty());
    assert!(ctx.recorded_pushes().is_empty());
}

#[test]
fn repeated_configured_remote_values_fail_before_external_io() {
    for second_value in ["origin", "publish"] {
        let ctx = testutil::test_context!()
            .with_remote()
            .with_initial_commit()
            .with_mock_github()
            .with_git_interceptor()
            .build();
        let network_destination = format!("{}/owner/repo.git", ctx.mock_server_url());
        ctx.set_config("remote.origin.pushurl", Some(&network_destination));
        ctx.git_cmd().args(["config", "--add", "gherrit.remote", "origin"]).assert().success();
        ctx.git_cmd().args(["config", "--add", "gherrit.remote", second_value]).assert().success();
        ctx.checkout_managed_private("repeated-remote");
        ctx.commit_with_gherrit_id("Reject repeated publication remotes");

        ctx.hook_cmd("pre-push")
            .assert()
            .failure()
            .stderr(predicate::str::contains("GHerrit remote is configured more than once"));

        assert_eq!(ctx.github().git_redirect_source_requests(), 0);
        assert!(ctx.github().requests().is_empty());
        assert!(ctx.recorded_pushes().is_empty());
    }
}

#[cfg(unix)]
#[test]
fn a_non_utf8_remote_name_fails_closed_before_external_io() {
    let ctx = testutil::test_context!()
        .with_remote()
        .with_initial_commit()
        .with_mock_github()
        .with_git_interceptor()
        .build();
    fs::OpenOptions::new()
        .append(true)
        .open(ctx.repo_path.join(".git/config"))
        .unwrap()
        .write_all(b"\n[gherrit]\n\tremote = \xff\n")
        .unwrap();
    ctx.checkout_managed_private("non-utf8-remote");
    ctx.commit_with_gherrit_id("Reject an unreadable remote name");

    ctx.hook_cmd("pre-push")
        .assert()
        .failure()
        .stderr(predicate::str::contains("configured GHerrit remote is not valid UTF-8"));

    assert!(ctx.github().requests().is_empty());
    assert!(ctx.recorded_pushes().is_empty());
}

#[test]
fn the_remote_and_github_default_branch_must_have_the_same_name() {
    let ctx = testutil::test_context!()
        .with_remote()
        .with_initial_commit()
        .with_mock_github()
        .with_git_interceptor()
        .build();
    let main_tip = ctx.head_oid();
    ctx.github().set_default_branch("master", &main_tip);
    ctx.checkout_managed_private("mismatched-default-name");
    ctx.commit_with_gherrit_id("Do not guess which default branch wins");

    ctx.hook_cmd("pre-push").assert().failure().stderr(predicate::str::contains(
        "Git and GitHub disagree about the repository default branch name",
    ));

    assert!(ctx.github().pull_requests().is_empty());
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
        "gherrit-pr-id 'main', which conflicts with the repository default branch",
    ));

    assert!(ctx.github().requests().is_empty());
    assert!(ctx.recorded_pushes().is_empty());
    assert_eq!(ctx.remote_ref_oid("refs/heads/main").as_deref(), Some(default_tip.as_str()));
}

#[test]
fn a_head_change_id_remains_publishable_after_tail_matching_refs_exist() {
    let ctx = testutil::test_context!()
        .with_remote()
        .with_initial_commit()
        .with_mock_github()
        .with_git_interceptor()
        .build();
    ctx.checkout_managed_private("head-change-id");
    ctx.commit_with_explicit_gherrit_id("Publish a HEAD change ID", "HEAD");
    let v1_oid = ctx.head_oid();

    ctx.hook_cmd("pre-push").assert().success();
    assert_eq!(ctx.remote_ref_oid("refs/heads/HEAD").as_deref(), Some(v1_oid.as_str()));
    assert_eq!(ctx.remote_ref_oid("refs/tags/gherrit/HEAD/v1").as_deref(), Some(v1_oid.as_str()));

    ctx.remote_git_cmd()
        .args([
            "tag",
            "--annotate",
            "temporary-tail-matching-tag",
            "refs/heads/main",
            "--message",
            "Unrelated tail-matching tag",
        ])
        .assert()
        .success();
    ctx.remote_git_cmd()
        .args(["update-ref", "refs/tags/HEAD", "refs/tags/temporary-tail-matching-tag"])
        .assert()
        .success();
    ctx.remote_git_cmd()
        .args(["update-ref", "-d", "refs/tags/temporary-tail-matching-tag"])
        .assert()
        .success();
    ctx.amend();
    let v2_oid = ctx.head_oid();

    // `git ls-remote <repository> HEAD` uses tail matching and now reports the
    // pseudo-ref, managed branch, and unrelated tag. Only the pseudo-ref is
    // default-branch evidence; the other valid records must remain harmless.
    ctx.hook_cmd("pre-push").assert().success();

    assert_eq!(ctx.remote_ref_oid("refs/heads/HEAD").as_deref(), Some(v2_oid.as_str()));
    assert_eq!(ctx.remote_ref_oid("refs/tags/gherrit/HEAD/v1").as_deref(), Some(v1_oid.as_str()));
    assert_eq!(ctx.remote_ref_oid("refs/tags/gherrit/HEAD/v2").as_deref(), Some(v2_oid.as_str()));
    assert!(ctx.remote_ref_oid("refs/tags/HEAD").is_some());
    assert_eq!(ctx.recorded_pushes().len(), 2);
}

#[test]
fn the_remote_and_github_default_branch_must_have_the_same_tip() {
    let ctx = testutil::test_context!()
        .with_remote()
        .with_initial_commit()
        .with_mock_github()
        .with_git_interceptor()
        .build();
    ctx.checkout_managed_private("mismatched-default-tip");
    ctx.commit_with_gherrit_id("Do not reconcile divergent repository views");
    ctx.github().set_default_branch("main", &ctx.head_oid());

    ctx.hook_cmd("pre-push").assert().failure().stderr(predicate::str::contains(
        "Git and GitHub disagree about the tip of default branch 'main'",
    ));

    assert!(ctx.github().pull_requests().is_empty());
    assert!(ctx.recorded_pushes().is_empty());
}

#[test]
fn the_local_default_branch_must_match_the_push_repository() {
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

    assert!(ctx.github().pull_requests().is_empty());
    assert!(ctx.recorded_pushes().is_empty());
}

#[test]
fn a_repository_whose_default_branch_is_master_publishes_against_master() {
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
    ctx.commit_with_gherrit_id("Publish from a master-based repository");

    ctx.hook_cmd("pre-push").assert().success();

    let prs = ctx.github().pull_requests();
    assert_eq!(prs.len(), 1);
    assert_eq!(prs[0].base, "master");
}

#[test]
fn pre_push_works_from_a_linked_worktree() {
    let ctx = testutil::test_context!()
        .with_remote()
        .with_initial_commit()
        .with_mock_github()
        .with_git_interceptor()
        .build();
    let linked = ctx.dir.path().join("linked");
    ctx.git_cmd()
        .args(["worktree", "add", "-b", "linked-stack"])
        .arg(&linked)
        .arg("main")
        .assert()
        .success();
    ctx.configure_managed_private("linked-stack");
    ctx.git_cmd_at(&linked)
        .args([
            "commit",
            "--allow-empty",
            "--no-verify",
            "-m",
            "Publish from a linked worktree\n\ngherrit-pr-id: Glinked",
        ])
        .assert()
        .success();

    ctx.gherrit_cmd_at(&linked).args(["hook", "pre-push"]).assert().success();

    assert_eq!(ctx.github().pull_requests().len(), 1);
    assert_eq!(ctx.github().pull_requests()[0].head, "Glinked");
}

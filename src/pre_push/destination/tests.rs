use std::{
    ffi::OsStr,
    fs,
    io::Write as _,
    process, thread,
    time::{Duration, Instant},
};

use super::*;

const OBSERVATION_FIXTURE_MODE: &str = "GHERRIT_INITIAL_OBSERVATION_FIXTURE_MODE";
const OBSERVATION_FIXTURE_TEST: &str =
    "pre_push::destination::tests::initial_observation_process_fixture";
const TRANSPORT_FIXTURE_MODE: &str = "GHERRIT_REMOTE_TRANSPORT_FIXTURE_MODE";
const TRANSPORT_FIXTURE_TEST: &str =
    "pre_push::destination::tests::remote_transport_settings_process_fixture";

fn remote() -> util::RemoteName {
    util::RemoteName::from_config(b"origin").unwrap()
}

fn resolved(output: &[u8]) -> ResolvedDestination {
    resolved_result(output).unwrap()
}

fn resolved_result(output: &[u8]) -> Result<ResolvedDestination> {
    ResolvedDestination::from_git_output(remote(), output, &env::current_dir().unwrap())
}

fn destination() -> PushDestination {
    PushDestination {
        resolved: resolved(b"https://github.com/owner/repo.git\n"),
        internal_remote: INTERNAL_REMOTE_STEM.to_owned(),
        transport: RemoteTransportSettings::default(),
    }
}

fn git_dir_identity() -> util::GitDirIdentity {
    util::Repo::open(".").unwrap().git_dir_identity().clone()
}

fn arguments(command: &Command) -> Vec<&OsStr> {
    command.get_args().collect()
}

fn observation_fixture(mode: &str) -> Command {
    let mut command = Command::new(env::current_exe().unwrap());
    command.env_clear();
    #[cfg(windows)]
    if let Some(system_root) = env::var_os("SystemRoot") {
        command.env("SystemRoot", &system_root).env("WINDIR", system_root);
    }
    command
        .args(["--exact", OBSERVATION_FIXTURE_TEST, "--nocapture"])
        .env(OBSERVATION_FIXTURE_MODE, mode);
    command
}

fn transport_fixture(context: &testutil::TestContext, mode: &str) -> process::Output {
    let global = context.dir.path().join("empty-global.config");
    fs::write(&global, "").unwrap();
    Command::new(env::current_exe().unwrap())
        .args(["--exact", TRANSPORT_FIXTURE_TEST, "--nocapture"])
        .current_dir(&context.repo_path)
        .env(TRANSPORT_FIXTURE_MODE, mode)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", global)
        .env_remove(GIT_CONFIG_PARAMETERS_ENV)
        .env_remove("GIT_CONFIG_COUNT")
        .output()
        .unwrap()
}

#[test]
fn initial_observation_process_fixture() {
    let Ok(mode) = env::var(OBSERVATION_FIXTURE_MODE) else {
        return;
    };
    match mode.as_str() {
        "hang" => thread::sleep(Duration::from_secs(30)),
        "overflow" => {
            std::io::stdout().write_all(&[b'x'; 256]).unwrap();
            std::io::stdout().flush().unwrap();
            thread::sleep(Duration::from_secs(30));
        }
        mode => panic!("unknown initial-observation fixture mode {mode}"),
    }
    process::exit(0);
}

#[test]
fn remote_transport_settings_process_fixture() {
    let Ok(mode) = env::var(TRANSPORT_FIXTURE_MODE) else {
        return;
    };
    let destination = resolved(b"https://github.com/owner/repo.git\n");
    let settings = destination.inspect_remote_transport_settings().unwrap_or_else(|error| {
        if mode == "control" {
            let rendered = error.to_string();
            assert!(rendered.contains("transport setting containing control characters"));
            assert!(!rendered.contains("opaque-control-secret"));
            process::exit(0);
        }
        panic!("transport inspection failed: {error:?}");
    });
    match mode.as_str() {
        "last-and-empty" => {
            assert!(settings.proxy.as_deref() == Some(""), "proxy selection mismatch");
            assert!(
                settings.proxy_auth_method.as_deref() == Some("digest"),
                "proxy authentication selection mismatch"
            );
        }
        "auth-only" => {
            assert!(settings.proxy.is_none(), "an absent proxy became present");
            assert!(
                settings.proxy_auth_method.as_deref() == Some("digest"),
                "proxy authentication selection mismatch"
            );
        }
        "control" => panic!("a control-bearing transport setting was accepted"),
        mode => panic!("unknown transport fixture mode {mode}"),
    }
}

#[test]
fn effective_remote_transport_settings_follow_git_value_semantics() {
    let context =
        testutil::TestContextBuilder::new("unused").with_remote().with_initial_commit().build();
    for (key, value) in [
        ("remote.origin.proxy", "first-proxy"),
        ("remote.origin.proxy", ""),
        ("remote.origin.proxyAuthMethod", "basic"),
        ("remote.origin.proxyAuthMethod", "digest"),
    ] {
        context.git_cmd().args(["config", "--add", key, value]).assert().success();
    }

    let output = transport_fixture(&context, "last-and-empty");
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));

    context.git_cmd().args(["config", "--unset-all", "remote.origin.proxy"]).assert().success();
    let output = transport_fixture(&context, "auth-only");
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));

    context
        .git_cmd()
        .args(["config", "--replace-all", "remote.origin.proxy", "opaque-control-secret\u{85}"])
        .assert()
        .success();
    let output = transport_fixture(&context, "control");
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
}

#[tokio::test(flavor = "current_thread")]
async fn initial_observation_hang_is_bounded_off_the_async_runtime() {
    let started = Instant::now();
    let observation = observe_default_branch_command(
        observation_fixture("hang"),
        "origin",
        Duration::from_millis(50),
        1024,
    );
    let peer = async {
        tokio::time::sleep(Duration::from_millis(10)).await;
        started.elapsed()
    };
    let (error, peer_elapsed) = tokio::join!(observation, peer);
    let error = error.unwrap_err();

    assert!(format!("{error:?}").contains("remote Git command timed out"));
    assert!(peer_elapsed < Duration::from_secs(1));
    assert!(started.elapsed() < Duration::from_secs(2));
}

#[tokio::test(flavor = "current_thread")]
async fn initial_observation_excess_output_enters_bounded_cleanup() {
    let started = Instant::now();
    let error = observe_default_branch_command(
        observation_fixture("overflow"),
        "origin",
        Duration::from_secs(2),
        32,
    )
    .await
    .unwrap_err();

    assert!(format!("{error:?}").contains("stdout exceeded the 32-byte limit"));
    assert!(!format!("{error:?}").contains(&"x".repeat(32)));
    assert!(started.elapsed() < Duration::from_secs(1));
}

#[test]
fn recognizes_every_private_transport_diagnostic_family() {
    for name in [
        b"GIT_TRACE".as_slice(),
        b"git_trace_curl",
        b"GiT_TrAcE2_Event",
        b"GIT_TRACE_FUTURE_FAMILY",
        b"GIT_CURL_VERBOSE",
        b"git_curl_verbose",
    ] {
        assert!(is_git_transport_diagnostic(name), "name: {name:?}");
    }
    for name in [b"GHERRIT_TRACE".as_slice(), b"GIT_CURL_VERBOSE_EXTRA", b"GIT_FLUSH", b"TRACE"] {
        assert!(!is_git_transport_diagnostic(name), "name: {name:?}");
    }
}

#[test]
fn transport_diagnostics_remove_future_families_and_disable_trace2_config_targets() {
    let mut command = Command::new("git");
    clear_git_transport_diagnostics_from(
        &mut command,
        [
            "GIT_TRACE_FUTURE_FAMILY",
            "GIT_CURL_VERBOSE",
            "GIT_TRACE2",
            "GIT_TRACE2_PERF",
            "GIT_TRACE2_EVENT",
        ]
        .map(std::ffi::OsString::from),
    );

    for variable in ["GIT_TRACE_FUTURE_FAMILY", "GIT_CURL_VERBOSE"] {
        assert_eq!(
            command
                .get_envs()
                .find(|(name, _)| *name == OsStr::new(variable))
                .map(|(_, value)| value),
            Some(None)
        );
    }
    for variable in ["GIT_TRACE2", "GIT_TRACE2_PERF", "GIT_TRACE2_EVENT"] {
        assert_eq!(
            command
                .get_envs()
                .find(|(name, _)| *name == OsStr::new(variable))
                .and_then(|(_, value)| value),
            Some(OsStr::new("0"))
        );
    }
}

#[test]
fn configured_trace2_targets_do_not_observe_a_destination_command() {
    let context =
        testutil::TestContextBuilder::new("unused").with_remote().with_initial_commit().build();
    let literal = String::from_utf8(
        context
            .git_cmd()
            .args(["remote", "get-url", "origin"])
            .assert()
            .success()
            .get_output()
            .stdout
            .clone(),
    )
    .unwrap();
    let literal = literal.trim();
    let destination = PushDestination {
        resolved: resolved(format!("{literal}\n").as_bytes()),
        internal_remote: INTERNAL_REMOTE_STEM.to_owned(),
        transport: RemoteTransportSettings::default(),
    };

    let system = context.dir.path().join("trace2-system.config");
    let global = context.dir.path().join("trace2-global.config");
    let traces = [
        context.dir.path().join("trace2-event.json"),
        context.dir.path().join("trace2-normal.log"),
        context.dir.path().join("trace2-perf.log"),
    ];
    fs::write(&system, "").unwrap();
    context
        .git_cmd()
        .args(["config", "--file"])
        .arg(&system)
        .args(["--add", "trace2.eventTarget"])
        .arg(&traces[0])
        .assert()
        .success();
    fs::write(&global, "").unwrap();
    for (key, trace) in [("trace2.normalTarget", &traces[1]), ("trace2.perfTarget", &traces[2])] {
        context
            .git_cmd()
            .args(["config", "--file"])
            .arg(&global)
            .args(["--add", key])
            .arg(trace)
            .assert()
            .success();
    }

    let mut probe = context.git_cmd();
    probe
        .env("GIT_CONFIG_NOSYSTEM", "0")
        .env("GIT_CONFIG_SYSTEM", &system)
        .env("GIT_CONFIG_GLOBAL", &global)
        .args(["rev-parse", "--git-dir"])
        .assert()
        .success();
    traces.iter().for_each(|trace| fs::remove_file(trace).unwrap());

    let mut command = destination.ls_remote(["--symref".to_owned()], ["HEAD".to_owned()]);
    command
        .current_dir(&context.repo_path)
        .env("GIT_CONFIG_NOSYSTEM", "0")
        .env("GIT_CONFIG_SYSTEM", &system)
        .env("GIT_CONFIG_GLOBAL", &global);
    let output = command.output().unwrap();

    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    assert!(traces.iter().all(|trace| !trace.exists()));
}

#[test]
fn every_destination_bearing_git_command_disables_redirects() {
    let destination = destination();
    let literal = "https://github.com/owner/repo.git";
    let redirect_parameter = format!(
        "{}={}",
        quote_git_config_parameter(&format!("http.{literal}.followRedirects")),
        quote_git_config_parameter("false")
    );
    let ls_remote = destination.ls_remote(["--symref".to_string()], ["HEAD".to_string()]);
    assert_eq!(
        arguments(&ls_remote),
        [
            "--no-replace-objects",
            "--config-env=remote.gherrit-publication.url=GHERRIT_PRIVATE_PUSH_DESTINATION",
            "--config-env=remote.gherrit-publication.pushurl=GHERRIT_PRIVATE_PUSH_DESTINATION",
            "-c",
            "http.followRedirects=false",
            "ls-remote",
            "--symref",
            "--",
            "gherrit-publication",
            "HEAD",
        ]
        .map(OsStr::new)
    );

    let git_dir = git_dir_identity();
    let push =
        destination.push(&git_dir, ["--atomic".to_string()], ["HEAD:refs/heads/Gone".to_string()]);
    assert_eq!(
        arguments(&push),
        [
            "--no-replace-objects",
            "--config-env=remote.gherrit-publication.url=GHERRIT_PRIVATE_PUSH_DESTINATION",
            "--config-env=remote.gherrit-publication.pushurl=GHERRIT_PRIVATE_PUSH_DESTINATION",
            "-c",
            "http.followRedirects=false",
            "-c",
            "push.followTags=false",
            "-c",
            "push.recurseSubmodules=no",
            "-c",
            "push.pushOption=",
            "push",
            "--atomic",
            "--",
            "gherrit-publication",
            "HEAD:refs/heads/Gone",
        ]
        .map(OsStr::new)
    );
    assert!(arguments(&push).iter().all(|argument| {
        let argument = argument.to_string_lossy();
        !argument.ends_with(".url=") && !argument.ends_with(".pushurl=")
    }));
    assert!(arguments(&push).iter().all(|argument| *argument != OsStr::new(literal)));
    assert_eq!(
        push.get_envs()
            .find(|(name, _)| *name == OsStr::new(DESTINATION_ENV))
            .and_then(|(_, value)| value),
        Some(OsStr::new(literal))
    );
    let parameters = push
        .get_envs()
        .find(|(name, _)| *name == OsStr::new(GIT_CONFIG_PARAMETERS_ENV))
        .and_then(|(_, value)| value)
        .unwrap()
        .to_str()
        .unwrap();
    assert!(parameters.ends_with(&redirect_parameter));
    assert_eq!(
        push.get_envs()
            .find(|(name, _)| *name == OsStr::new(INTERNAL_PRE_PUSH_REMOTE_ENV))
            .and_then(|(_, value)| value),
        Some(OsStr::new("gherrit-publication"))
    );
    assert_eq!(
        push.get_envs()
            .find(|(name, _)| *name == OsStr::new(INTERNAL_PRE_PUSH_GIT_DIR_ENV))
            .and_then(|(_, value)| value),
        Some(git_dir.as_os_str())
    );
}

#[test]
fn remote_transport_settings_are_private_command_scoped_state() {
    let mut destination = destination();
    destination.transport = RemoteTransportSettings {
        proxy: Some("opaque-proxy-secret".to_owned()),
        proxy_auth_method: Some("opaque-auth-secret".to_owned()),
    };

    for command in [
        destination.ls_remote(std::iter::empty(), std::iter::empty()),
        destination.push(&git_dir_identity(), std::iter::empty(), std::iter::empty()),
    ] {
        let arguments = arguments(&command);
        assert!(arguments.contains(&OsStr::new(
            "--config-env=remote.gherrit-publication.proxy=GHERRIT_PRIVATE_REMOTE_PROXY"
        )));
        assert!(arguments.contains(&OsStr::new(
            "--config-env=remote.gherrit-publication.proxyAuthMethod=GHERRIT_PRIVATE_REMOTE_PROXY_AUTH_METHOD"
        )));
        for private in ["opaque-proxy-secret", "opaque-auth-secret"] {
            assert!(arguments.iter().all(|argument| *argument != OsStr::new(private)));
        }
        for (name, value) in
            [(PROXY_ENV, "opaque-proxy-secret"), (PROXY_AUTH_METHOD_ENV, "opaque-auth-secret")]
        {
            assert_eq!(
                command
                    .get_envs()
                    .find(|(actual, _)| *actual == OsStr::new(name))
                    .and_then(|(_, value)| value),
                Some(OsStr::new(value))
            );
        }
    }

    destination.transport =
        RemoteTransportSettings { proxy: Some(String::new()), proxy_auth_method: None };
    let command = destination.ls_remote(std::iter::empty(), std::iter::empty());
    assert_eq!(
        command
            .get_envs()
            .find(|(name, _)| *name == OsStr::new(PROXY_ENV))
            .and_then(|(_, value)| value),
        Some(OsStr::new(""))
    );
    assert_eq!(
        command
            .get_envs()
            .find(|(name, _)| *name == OsStr::new(PROXY_AUTH_METHOD_ENV))
            .map(|(_, value)| value),
        Some(None)
    );
}

#[test]
fn exact_redirect_parameter_preserves_command_scope_and_wins() {
    let literal = "https://redirect.invalid/private/repo.git";
    let key = format!("http.{literal}.followRedirects");
    let inherited = format!(
        "{}={} {}={}",
        quote_git_config_parameter("credential.helper"),
        quote_git_config_parameter("sentinel-helper"),
        quote_git_config_parameter(&key),
        quote_git_config_parameter("true")
    );
    let parameters =
        command_scope_http_redirect_parameters(literal, Some(OsStr::new(&inherited))).unwrap();

    assert_eq!(quote_git_config_parameter("a'b"), "'a'\\''b'",);
    assert!(parameters.to_str().unwrap().starts_with(&inherited));

    let command = |arguments: &[&str]| {
        let mut command = Command::new("git");
        command
            .args(arguments)
            .env(GIT_CONFIG_PARAMETERS_ENV, &parameters)
            // The numbered environment source is read before
            // `GIT_CONFIG_PARAMETERS`. Its hostile exact value must not
            // supersede the appended value.
            .env("GIT_CONFIG_COUNT", "1")
            .env("GIT_CONFIG_KEY_0", &key)
            .env("GIT_CONFIG_VALUE_0", "true");
        command.output().unwrap()
    };

    let redirect =
        command(&["config", "--bool", "--get-urlmatch", "http.followRedirects", literal]);
    assert!(redirect.status.success());
    assert_eq!(redirect.stdout, b"false\n");

    let helper = command(&["config", "--get", "credential.helper"]);
    assert!(helper.status.success());
    assert_eq!(helper.stdout, b"sentinel-helper\n");
}

#[cfg(unix)]
#[test]
fn exact_redirect_parameter_preserves_non_utf8_command_scope_bytes() {
    use std::os::unix::ffi::{OsStrExt as _, OsStringExt as _};

    let inherited = OsString::from_vec(b"'credential.helper'='helper-\xff'".to_vec());
    let parameters = command_scope_http_redirect_parameters(
        "https://github.com/owner/repo.git",
        Some(&inherited),
    )
    .unwrap();
    let bytes = parameters.as_bytes();

    assert_eq!(&bytes[..inherited.len()], inherited.as_bytes());
    assert_eq!(bytes[inherited.len()], b' ');
    assert!(bytes.ends_with(b"='false'"));
}

#[test]
fn remote_selection_uses_complete_case_insensitive_configuration_keys() {
    let configuration = [
        b"remote.gherrit-publication.url".to_vec(),
        b"ReMoTe.GhErRiT-PuBlIcAtIoN-1.ReCeIvEpAcK".to_vec(),
        b"remote.gherrit-publication-extra.url".to_vec(),
        b"remote.gherrit-publication-10.pushurl".to_vec(),
    ];

    assert_eq!(select_absent_remote(INTERNAL_REMOTE_STEM, &configuration), "gherrit-publication-2");
}

#[test]
fn configured_remote_transport_policy_is_explicit_and_case_correct() {
    let allowed = [
        b"remote.origin.proxy".to_vec(),
        b"REMOTE.origin.PROXYAUTHMETHOD".to_vec(),
        b"remote.origin.fetch".to_vec(),
        b"remote.origin.push".to_vec(),
        b"remote.origin.unknownFutureKey".to_vec(),
        b"remote.Origin.receivepack".to_vec(),
        b"remote.origin-extra.vcs".to_vec(),
    ];
    assert!(reject_unsupported_remote_transport_configuration("origin", &allowed).is_ok());

    for (key, expected) in [
        ("remote.origin.uploadpack", "uploadpack"),
        ("REMOTE.origin.RECEIVEPACK", "receivepack"),
        ("remote.origin.VcS", "vcs"),
        ("remote.origin.serveroption", "serverOption"),
    ] {
        let error =
            reject_unsupported_remote_transport_configuration("origin", &[key.as_bytes().to_vec()])
                .unwrap_err()
                .to_string();
        assert!(error.contains(expected), "error: {error}");
        assert!(error.contains("GHerrit remote 'origin'"));
    }

    let all = [
        b"remote.origin.serverOption".to_vec(),
        b"remote.origin.receivepack".to_vec(),
        b"remote.origin.serveroption".to_vec(),
    ];
    let error =
        reject_unsupported_remote_transport_configuration("origin", &all).unwrap_err().to_string();
    assert_eq!(
        error,
        "GHerrit remote 'origin' configures unsupported transport settings: receivepack, serverOption"
    );
}

#[test]
fn config_record_framing_distinguishes_no_values_from_empty_values() {
    assert_eq!(nul_terminated_records(b""), Some(Vec::new()));
    assert_eq!(nul_terminated_records(b"value\0"), Some(vec![b"value".to_vec()]));
    assert_eq!(nul_terminated_records(b"\0"), Some(vec![Vec::new()]));
    assert_eq!(
        nul_terminated_records(b"one\0\0three\0"),
        Some(vec![b"one".to_vec(), Vec::new(), b"three".to_vec()])
    );
    assert_eq!(nul_terminated_records(b"unterminated"), None);
}

#[test]
fn final_remote_validation_accepts_only_the_planned_transport() {
    let remote = INTERNAL_REMOTE_STEM;
    let destination = b"private/path/owner/repo.git";
    let base_names = vec![
        b"remote.gherrit-publication.url".to_vec(),
        b"remote.gherrit-publication.pushurl".to_vec(),
        b"remote.unrelated.receivepack".to_vec(),
    ];
    let valid_urls = vec![destination.to_vec()];
    let validate = |names: &[Vec<u8>],
                    urls: &[Vec<u8>],
                    pushurls: &[Vec<u8>],
                    proxies: &[Vec<u8>],
                    auth_methods: &[Vec<u8>],
                    transport: &RemoteTransportSettings| {
        validate_internal_remote_configuration(
            remote,
            names,
            ObservedRemoteValues { urls, pushurls, proxies, proxy_auth_methods: auth_methods },
            destination,
            transport,
        )
    };

    assert!(
        validate(
            &base_names,
            &valid_urls,
            &valid_urls,
            &[],
            &[],
            &RemoteTransportSettings::default(),
        )
        .is_ok()
    );

    for transport in [
        RemoteTransportSettings { proxy: Some(String::new()), proxy_auth_method: None },
        RemoteTransportSettings { proxy: None, proxy_auth_method: Some("basic".to_owned()) },
        RemoteTransportSettings {
            proxy: Some("proxy-value".to_owned()),
            proxy_auth_method: Some("digest".to_owned()),
        },
    ] {
        let mut names = base_names.clone();
        let proxies =
            transport.proxy.as_ref().map_or_else(Vec::new, |value| vec![value.as_bytes().to_vec()]);
        let auth_methods = transport
            .proxy_auth_method
            .as_ref()
            .map_or_else(Vec::new, |value| vec![value.as_bytes().to_vec()]);
        if transport.proxy.is_some() {
            names.push(b"remote.gherrit-publication.proxy".to_vec());
        }
        if transport.proxy_auth_method.is_some() {
            names.push(b"REMOTE.GHERRIT-PUBLICATION.PROXYAUTHMETHOD".to_vec());
        }
        assert!(
            validate(&names, &valid_urls, &valid_urls, &proxies, &auth_methods, &transport,)
                .is_ok(),
            "transport was rejected"
        );
    }

    for (names, urls, pushurls, proxies, auth_methods, transport) in [
        (
            vec![
                b"remote.gherrit-publication.url".to_vec(),
                b"remote.gherrit-publication.url".to_vec(),
                b"remote.gherrit-publication.pushurl".to_vec(),
            ],
            vec![destination.to_vec(), destination.to_vec()],
            valid_urls.clone(),
            Vec::new(),
            Vec::new(),
            RemoteTransportSettings::default(),
        ),
        (
            vec![
                b"remote.gherrit-publication.url".to_vec(),
                b"remote.gherrit-publication.pushurl".to_vec(),
                b"remote.gherrit-publication.pushURL".to_vec(),
            ],
            valid_urls.clone(),
            vec![destination.to_vec(), destination.to_vec()],
            Vec::new(),
            Vec::new(),
            RemoteTransportSettings::default(),
        ),
        (
            base_names.clone(),
            vec![b"different".to_vec()],
            valid_urls.clone(),
            Vec::new(),
            Vec::new(),
            RemoteTransportSettings::default(),
        ),
        (
            base_names.clone(),
            valid_urls.clone(),
            vec![b"different".to_vec()],
            Vec::new(),
            Vec::new(),
            RemoteTransportSettings::default(),
        ),
        (
            vec![
                b"remote.gherrit-publication.url".to_vec(),
                b"remote.gherrit-publication.pushurl".to_vec(),
                b"remote.gherrit-publication.receivepack".to_vec(),
            ],
            valid_urls.clone(),
            valid_urls.clone(),
            Vec::new(),
            Vec::new(),
            RemoteTransportSettings::default(),
        ),
        (
            {
                let mut names = base_names.clone();
                names.push(b"remote.gherrit-publication.proxy".to_vec());
                names
            },
            valid_urls.clone(),
            valid_urls.clone(),
            vec![b"unplanned".to_vec()],
            Vec::new(),
            RemoteTransportSettings::default(),
        ),
        (
            base_names.clone(),
            valid_urls.clone(),
            valid_urls.clone(),
            Vec::new(),
            Vec::new(),
            RemoteTransportSettings { proxy: Some("missing".to_owned()), proxy_auth_method: None },
        ),
        (
            vec![b"remote.gherrit-publication.url".to_vec()],
            valid_urls.clone(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            RemoteTransportSettings::default(),
        ),
        (
            vec![b"remote.gherrit-publication.pushurl".to_vec()],
            Vec::new(),
            valid_urls.clone(),
            Vec::new(),
            Vec::new(),
            RemoteTransportSettings::default(),
        ),
    ] {
        assert!(
            validate(&names, &urls, &pushurls, &proxies, &auth_methods, &transport,).is_err(),
            "names: {names:?}, urls: {urls:?}, pushurls: {pushurls:?}, proxies: {proxies:?}, auth methods: {auth_methods:?}"
        );
    }
}

#[test]
fn derives_identity_from_supported_push_destinations() {
    for (destination, owner, repository) in [
        ("https://github.com/owner/repo.git", "owner", "repo"),
        ("https://github.com/owner/repo", "owner", "repo"),
        ("git@github.com:owner/repo.git", "owner", "repo"),
        ("git@github.com:owner/repo", "owner", "repo"),
        ("[::1]:owner/repo.git", "owner", "repo"),
        ("user@[2001:db8::1]:owner/repo", "owner", "repo"),
        ("alias:owner/repo.git", "owner", "repo"),
        ("alias:owner/repo", "owner", "repo"),
        ("http://localhost:3000/owner/repo.git", "owner", "repo"),
        ("http://my-gh.com/owner/repo", "owner", "repo"),
        ("https://github.com/user-name/repo", "user-name", "repo"),
        ("https://github.com/user_name/repo", "user_name", "repo"),
        ("https://github.com/user.name/repo.name.git", "user.name", "repo.name"),
    ] {
        let destination = resolved(format!("{destination}\n").as_bytes());

        assert_eq!(destination.coordinates.owner, owner);
        assert_eq!(destination.coordinates.repository, repository);
    }
}

#[test]
fn production_github_compatibility_retains_the_resolved_authority() {
    for destination in [
        "https://github.com/owner/repo.git",
        "https://GITHUB.COM/owner/repo.git",
        "ssh://github.com/owner/repo.git",
        "git@github.com:owner/repo.git",
        "github.com:owner/repo.git",
    ] {
        assert!(targets_production_github(&parse_destination(destination).unwrap()));
    }
    for destination in [
        "http://github.com/owner/repo.git",
        "git://github.com/owner/repo.git",
        "https://gitlab.example/owner/repo.git",
        "https://github.com.evil.example/owner/repo.git",
        "https://github.com:443/owner/repo.git",
        "evil://github.com/owner/repo.git",
        "HTTPS://github.com/owner/repo.git",
    ] {
        assert!(!targets_production_github(&parse_destination(destination).unwrap()));
    }
}

#[test]
fn canonical_local_destination_binds_git_literal_and_repository_identity() {
    let directory = tempfile::tempdir().unwrap();
    let target = directory.path().join("victim/actual-owner/actual-repository.git");
    fs::create_dir_all(&target).unwrap();
    let spelled =
        directory.path().join("victim/actual-owner/../actual-owner/actual-repository.git");
    let destination = ResolvedDestination::from_git_output(
        remote(),
        format!("{}\n", spelled.display()).as_bytes(),
        directory.path(),
    )
    .unwrap();

    assert_eq!(Path::new(&destination.literal), dunce::canonicalize(&target).unwrap());
    assert_eq!(destination.coordinates.owner, "actual-owner");
    assert_eq!(destination.coordinates.repository, "actual-repository");
}

#[cfg(unix)]
#[test]
fn canonical_local_destination_follows_one_symlink_before_deriving_identity() {
    use std::os::unix::fs::symlink;

    let directory = tempfile::tempdir().unwrap();
    let target = directory.path().join("victim/actual-owner/actual-repository.git");
    let link = directory.path().join("attacker/claimed-owner/claimed-repository.git");
    fs::create_dir_all(&target).unwrap();
    fs::create_dir_all(link.parent().unwrap()).unwrap();
    symlink(&target, &link).unwrap();
    let destination = ResolvedDestination::from_git_output(
        remote(),
        b"attacker/claimed-owner/claimed-repository.git\n",
        directory.path(),
    )
    .unwrap();

    assert_eq!(Path::new(&destination.literal), dunce::canonicalize(&target).unwrap());
    assert_eq!(destination.coordinates.owner, "actual-owner");
    assert_eq!(destination.coordinates.repository, "actual-repository");
}

#[test]
fn rejects_file_urls_with_actionable_redacted_advice() {
    let secret = "file:///private/secret-owner/secret-repository.git";
    let error =
        resolved_result(format!("{secret}\n").as_bytes()).err().expect("file URLs are unsupported");

    assert!(error.to_string().contains("uses an unsupported file URL"));
    assert!(error.to_string().contains("native filesystem path"));
    assert!(!error.to_string().contains(secret));
    assert!(!error.to_string().contains("secret-owner"));
}

#[cfg(windows)]
#[test]
fn windows_accepts_native_paths_and_crlf_output() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("owner/repo.git");
    fs::create_dir_all(&path).unwrap();
    let destination = resolved(format!("{}\r\n", path.display()).as_bytes());
    assert_eq!(destination.coordinates.owner, "owner");
    assert_eq!(destination.coordinates.repository, "repo");

    let destination = resolved(b"https://github.com/owner/repo.git\r\n");
    assert_eq!(destination.coordinates.owner, "owner");
    assert_eq!(destination.coordinates.repository, "repo");

    let oid = "1111111111111111111111111111111111111111";
    assert_eq!(
        parse_default_branch(format!("ref: refs/heads/main\tHEAD\r\n{oid}\tHEAD\r\n").as_bytes())
            .unwrap(),
        DefaultBranch::new("main".to_string(), ObjectId::from_hex(oid.as_bytes()).unwrap())
            .unwrap()
    );
}

#[cfg(not(windows))]
#[test]
fn unix_preserves_terminal_cr_and_treats_backslash_as_data() {
    let destination = b"https://github.com/owner/repo.git\r\n";
    let error = resolved_result(destination)
        .err()
        .expect("a Unix destination's terminal CR must not be discarded");
    assert!(!error.to_string().contains("https://"));

    assert!(matches!(
        parse_destination(r"C:\tmp\owner\repo.git"),
        Some(ParsedDestination::Scp { .. })
    ));
    assert!(matches!(
        parse_destination(r"/tmp/attacker/owner\repo.git"),
        Some(ParsedDestination::Local { .. })
    ));
}

#[test]
fn rejects_ambiguous_or_malformed_destinations_without_disclosure() {
    for destination in [
        "",
        "credential-without-a-repository",
        "1https://github.com/owner/repo",
        "https://git hub/owner/repo",
        "https://github.com/owner/repo?token=secret",
        "https://github.com/owner/repo#secret",
        "https://github.com/owner/repo/extra",
        r"https://github.com/owner\attacker/repo",
        "git@github.com:owner/repo/extra",
        r"git@github.com:owner\repo",
        "[::1:owner/repo",
        "[::1]owner/repo",
        "user@[::1]]:owner/repo",
        "::1:owner/repo",
        r"file:///tmp/attacker/owner\repo",
        "https://github.com/owner/repo\r",
        "https://github.com/owner/repo\npoison",
        "https://github.com/owner/repo\0suffix",
    ] {
        let error = resolved_result(destination.as_bytes())
            .err()
            .expect("malformed destination must be rejected");
        if !destination.is_empty() {
            assert!(!error.to_string().contains(destination), "destination: {destination:?}");
        }
    }
    assert!(resolved_result(b"\xff\n").is_err());
}

#[test]
fn rejects_uri_user_information_without_disclosure() {
    for destination in [
        "https://token-secret@github.com/owner/repo.git",
        "ssh://git@github.com/owner/repo.git",
        "file://user:password-secret@localhost/tmp/owner/repo.git",
    ] {
        let error = resolved_result(format!("{destination}\n").as_bytes())
            .err()
            .expect("URI user information must be rejected");
        assert!(error.to_string().contains("use a Git credential helper or an SCP-style SSH"));
        assert!(!error.to_string().contains(destination));
        assert!(!error.to_string().contains("secret"));
    }

    assert!(resolved_result(b"git@github.com:owner/repo.git\n").is_ok());
}

#[test]
fn rejects_zero_or_multiple_destinations_without_disclosing_them() {
    let error = resolved_result(b"").err().expect("an empty destination must be rejected");
    assert_eq!(error.to_string(), "GHerrit remote 'origin' has no push destination");

    let secret = "https://user:secret@example.com/owner/repo.git";
    let output = format!("{secret}\nowner/other.git\n");
    let error =
        resolved_result(output.as_bytes()).err().expect("multiple destinations must be rejected");
    assert_eq!(
        error.to_string(),
        "GHerrit remote 'origin' has 2 push destinations; exactly one is required for atomic publication"
    );
    assert!(!error.to_string().contains(secret));
    assert!(!error.to_string().contains("secret"));
}

#[test]
fn child_diagnostic_is_local_bounded_redacted_and_terminal_safe() {
    let directory = tempfile::tempdir().unwrap();
    let canonical_path = directory.path().join("private-owner/private-repository.git");
    fs::create_dir_all(&canonical_path).unwrap();
    let private = directory.path().join("private-owner/../private-owner/private-repository.git");
    let literal = private.to_string_lossy().into_owned();
    let canonical = fs::canonicalize(&canonical_path).unwrap().to_string_lossy().into_owned();
    assert_ne!(literal, canonical);
    let destination = PushDestination {
        resolved: resolved(format!("{literal}\n").as_bytes()),
        internal_remote: INTERNAL_REMOTE_STEM.to_owned(),
        transport: RemoteTransportSettings {
            proxy: Some("opaque-proxy-secret".to_owned()),
            proxy_auth_method: Some("opaque-auth-secret".to_owned()),
        },
    };
    let diagnostic = format!(
        "policy denied publication for {canonical}\x1b[31m\r\n\
         normalized local destination {canonical}\n\
         proxy opaque-proxy-secret uses opaque-auth-secret\n\
         alternate transport ssh://private-host/private-owner/private-repository.git\n\
         remote: private server text for {canonical}\n\
         error: failed to push some refs to '{canonical}'\n\
         error: atomic push failed for ref refs/heads/Gone\n\
         ! [rejected] Gone -> Gone (atomic push failed)\n\
         fatal: the remote end hung up unexpectedly\n"
    );

    let rendered = destination
        .render_child_diagnostic(diagnostic.as_bytes(), diagnostic.len() as u64)
        .unwrap();

    assert!(rendered.contains("policy denied publication for <private destination>"));
    assert!(rendered.contains("normalized local destination <private destination>"));
    assert!(
        rendered.contains("proxy <private transport setting> uses <private transport setting>")
    );
    assert!(rendered.contains("alternate transport <path or URL redacted>"));
    assert!(rendered.contains("\\u{1b}[31m\\r"));
    for private in [
        &literal,
        &canonical,
        "private-host",
        "private-owner",
        "private-repository",
        "opaque-proxy-secret",
        "opaque-auth-secret",
    ] {
        assert!(!rendered.contains(private), "rendered diagnostic disclosed {private:?}");
    }
    assert!(!rendered.contains("private server text"));
    assert!(!rendered.contains("atomic push failed"));
    assert!(!rendered.contains("[rejected]"));
    assert!(!rendered.contains("remote end hung up"));
    assert!(!rendered.contains('\x1b'));
    assert!(!rendered.contains('\r'));
}

#[test]
fn truncated_child_diagnostics_discard_the_unclassifiable_first_line() {
    let destination = destination();
    let local = "local policy denied https://github.com/owner/repo.git";

    for (first_line, private_fragment) in [
        ("server-private text whose remote prefix was omitted", "server-private"),
        ("ner/repo.git whose destination prefix was omitted", "ner/repo.git"),
    ] {
        let retained = format!("{first_line}\n{local}\n");
        let rendered = destination
            .render_child_diagnostic(
                retained.as_bytes(),
                u64::try_from(retained.len()).unwrap() + 17,
            )
            .unwrap();

        assert_eq!(
            rendered,
            format!(
                "local policy denied <private destination>\n[{} earlier diagnostic bytes omitted]",
                17 + first_line.len() + 1
            )
        );
        assert!(!rendered.contains(private_fragment));
    }

    let retained = b"ivate/repository.git";
    let total = u64::try_from(retained.len()).unwrap() + 29;
    assert_eq!(
        destination.render_child_diagnostic(retained, total),
        Some(format!("[{total} earlier diagnostic bytes omitted]"))
    );
}

#[test]
fn empty_or_remote_only_child_diagnostics_are_suppressed() {
    let destination = destination();
    let remote_only = b"remote: server-private text\n";

    assert_eq!(destination.render_child_diagnostic(b"", 0), None);
    assert_eq!(
        destination.render_child_diagnostic(remote_only, u64::try_from(remote_only.len()).unwrap()),
        None
    );
}

#[test]
fn parses_exact_symbolic_default_branch_observation() {
    let oid = "1111111111111111111111111111111111111111";
    assert_eq!(
        parse_default_branch(format!("ref: refs/heads/master\tHEAD\n{oid}\tHEAD\n").as_bytes())
            .unwrap(),
        DefaultBranch::new("master".to_string(), ObjectId::from_hex(oid.as_bytes()).unwrap())
            .unwrap()
    );
}

#[test]
fn default_branch_rejects_the_owned_base_namespace() {
    let oid = ObjectId::from_hex(b"1111111111111111111111111111111111111111").unwrap();
    for name in ["gherrit-bases", "gherrit-bases/main"] {
        assert_eq!(
            DefaultBranch::new(name.to_owned(), oid).unwrap_err().to_string(),
            "The repository default branch is in GHerrit's reserved 'gherrit-bases' namespace"
        );
    }
    assert!(DefaultBranch::new("gherrit-bases-other".to_owned(), oid).is_ok());
}

#[test]
fn default_branch_record_order_is_irrelevant() {
    let oid = "1111111111111111111111111111111111111111";
    let expected =
        DefaultBranch::new("main".to_string(), ObjectId::from_hex(oid.as_bytes()).unwrap())
            .unwrap();

    for output in [
        format!("ref: refs/heads/main\tHEAD\n{oid}\tHEAD\n"),
        format!("{oid}\tHEAD\nref: refs/heads/main\tHEAD\n"),
    ] {
        assert_eq!(parse_default_branch(output.as_bytes()).unwrap(), expected);
    }
}

#[test]
fn ignores_valid_unrelated_remote_records_including_non_utf8_names() {
    let oid = "1111111111111111111111111111111111111111";
    let other = "2222222222222222222222222222222222222222";
    let mut output = format!(
        "{other}\trefs/heads/HEAD\n\
         {other}\trefs/tags/HEAD\n\
         {other}\trefs/tags/HEAD^{{}}\n\
         ref: refs/heads/elsewhere\trefs/remotes/origin/HEAD\n\
         {oid}\tHEAD\n\
         ref: refs/heads/main\tHEAD\n"
    )
    .into_bytes();
    output.extend_from_slice(format!("{other}\trefs/heads/").as_bytes());
    output.extend_from_slice(b"\xff-HEAD\n");

    assert_eq!(
        parse_default_branch(&output).unwrap(),
        DefaultBranch::new("main".to_string(), ObjectId::from_hex(oid.as_bytes()).unwrap(),)
            .unwrap()
    );
}

#[test]
fn rejects_every_malformed_default_branch_observation() {
    let oid = "1111111111111111111111111111111111111111";
    let null_oid = "0000000000000000000000000000000000000000";
    for output in [
        Vec::new(),
        format!("{oid}\tHEAD\n").into_bytes(),
        format!("ref: refs/tags/main\tHEAD\n{oid}\tHEAD\n").into_bytes(),
        format!("ref: refs/heads/main\r\tHEAD\n{oid}\tHEAD\n").into_bytes(),
        "ref: refs/heads/main\tHEAD\nxyz\tHEAD\n".to_string().into_bytes(),
        format!("ref: refs/heads/main\tHEAD\n{null_oid}\tHEAD\n").into_bytes(),
        format!("ref: refs/heads/main\tHEAD\n{oid}\tOTHER\n").into_bytes(),
        format!("ref: refs/heads/main\tHEAD\n{oid}\tHEAD\nextra\n").into_bytes(),
        format!(
            "ref: refs/heads/main\tHEAD\n\
             ref: refs/heads/main\tHEAD\n{oid}\tHEAD\n"
        )
        .into_bytes(),
        format!("ref: refs/heads/main\tHEAD\n{oid}\tHEAD\n{oid}\tHEAD\n").into_bytes(),
        format!("ref: refs/heads/main\tHEAD\n{oid}\tHEAD\nxyz\trefs/heads/other\n").into_bytes(),
        format!("ref: refs/heads/main\tHEAD\n{oid}\tHEAD\n{null_oid}\trefs/heads/other\n")
            .into_bytes(),
        format!("ref: invalid\trefs/heads/other\n{oid}\tHEAD\n").into_bytes(),
        format!("ref: refs/heads/main\tHEAD\n{oid}\tHEAD\n{oid}\tinvalid\n").into_bytes(),
        format!(
            "ref: refs/heads/main\tHEAD\n{oid}\tHEAD\n\
             {oid}\trefs/heads/other^{{}}\n"
        )
        .into_bytes(),
        format!(
            "ref: refs/heads/main\tHEAD\n{oid}\tHEAD\n\
             ref: refs/heads/other\trefs/tags/HEAD^{{}}\n"
        )
        .into_bytes(),
        {
            let mut output = b"ref: refs/heads/".to_vec();
            output.extend_from_slice(b"\xff\tHEAD\n");
            output.extend_from_slice(format!("{oid}\tHEAD\n").as_bytes());
            output
        },
    ] {
        assert!(parse_default_branch(&output).is_err(), "output: {output:?}");
    }
}

use std::{env, ffi::OsStr, fs, path::Path, process::Command};

use super::{
    command::{clear_git_repository_overrides, command_invocation},
    repository::*,
};
use crate::cmd;

const PROMISOR_CONFIGURATION_FIXTURE_MODE: &str = "GHERRIT_PROMISOR_CONFIGURATION_FIXTURE_MODE";
const PROMISOR_CONFIGURATION_FIXTURE_TEST: &str =
    "util::tests::promisor_configuration_process_fixture";

fn promisor_configuration_fixture(
    current_dir: &Path,
    global: &Path,
    overridden_config: &Path,
    expected: bool,
) {
    let output = Command::new(env::current_exe().unwrap())
        .args(["--exact", PROMISOR_CONFIGURATION_FIXTURE_TEST, "--nocapture"])
        .current_dir(current_dir)
        .env(PROMISOR_CONFIGURATION_FIXTURE_MODE, expected.to_string())
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", global)
        .env("GIT_CONFIG", overridden_config)
        .env("GIT_CONFIG_PARAMETERS", "'extensions.partialClone=parameters'")
        .env("GIT_CONFIG_COUNT", "1")
        .env("GIT_CONFIG_KEY_0", "extensions.partialClone")
        .env("GIT_CONFIG_VALUE_0", "command")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "promisor fixture failed:\nstdout={}\nstderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn promisor_configuration_process_fixture() {
    let Ok(expected) = env::var(PROMISOR_CONFIGURATION_FIXTURE_MODE) else {
        return;
    };
    let expected: bool = expected.parse().unwrap();
    let repository = Repo::open(".").unwrap();
    assert_eq!(repository.has_promisor_remote().unwrap(), expected);
}

#[test]
fn remote_names_are_decoded_and_validated_without_fallback() {
    for value in [b"origin".as_slice(), b"-publish", "rémote".as_bytes()] {
        assert_eq!(RemoteName::from_config(value).unwrap().as_str().as_bytes(), value);
    }

    for value in [b"".as_slice(), b"bad\nname", b"bad\rname", b"bad\0name", b"\xff"] {
        assert!(RemoteName::from_config(value).is_err(), "value: {value:?}");
    }
}

#[test]
fn the_default_remote_requires_at_most_one_configured_value() {
    assert_eq!(default_remote_name_from_values(std::iter::empty()).unwrap().as_str(), "origin");
    assert_eq!(
        default_remote_name_from_values([b"publish".as_slice()]).unwrap().as_str(),
        "publish"
    );

    for values in [
        [b"origin".as_slice(), b"origin".as_slice()],
        [b"origin".as_slice(), b"publish".as_slice()],
    ] {
        let error = default_remote_name_from_values(values)
            .err()
            .expect("repeated values must be rejected");
        assert_eq!(error.to_string(), "The GHerrit remote is configured more than once");
    }
}

#[test]
fn git_commands_use_the_literal_local_graph_without_lazy_fetches() {
    let command = cmd("git", ["status"]);
    let arguments = command
        .get_args()
        .map(|argument| argument.to_string_lossy().into_owned())
        .collect::<Vec<_>>();
    assert_eq!(arguments, ["--no-replace-objects", "status"]);
    let environment = command.get_envs().collect::<std::collections::HashMap<_, _>>();
    for variable in ["GIT_NO_LAZY_FETCH", "GIT_NO_REPLACE_OBJECTS"] {
        assert_eq!(environment[OsStr::new(variable)], Some(OsStr::new("1")));
    }
    for variable in ["GIT_GRAFT_FILE", "GIT_REPLACE_REF_BASE", "GIT_SHALLOW_FILE"] {
        assert_eq!(environment[OsStr::new(variable)], None);
    }

    for variable in ["GIT_OBJECT_DIRECTORY", "GIT_ALTERNATE_OBJECT_DIRECTORIES"] {
        assert!(!environment.contains_key(OsStr::new(variable)));
    }

    let command = cmd("gh", ["auth", "token"]);
    assert_eq!(command.get_args().collect::<Vec<_>>(), [OsStr::new("auth"), OsStr::new("token")]);
    assert!(command.get_envs().next().is_none());
}

#[test]
fn clearing_repository_overrides_preserves_command_scope_configuration() {
    let mut command = Command::new("git");
    command
        .env("GIT_CONFIG", "redirected.config")
        .env("GIT_CONFIG_PARAMETERS", "'remote.origin.proxy=preserved'")
        .env("GIT_CONFIG_COUNT", "1")
        .env("GIT_CONFIG_KEY_0", "remote.origin.proxyAuthMethod")
        .env("GIT_CONFIG_VALUE_0", "digest");

    clear_git_repository_overrides(&mut command);

    let environment = command.get_envs().collect::<std::collections::HashMap<_, _>>();
    assert_eq!(environment[OsStr::new("GIT_CONFIG")], None);
    for variable in
        ["GIT_CONFIG_PARAMETERS", "GIT_CONFIG_COUNT", "GIT_CONFIG_KEY_0", "GIT_CONFIG_VALUE_0"]
    {
        assert!(environment[OsStr::new(variable)].is_some(), "variable={variable}");
    }
}

#[test]
fn command_diagnostics_render_only_program_and_arguments() {
    let mut command = Command::new("git");
    command.args(["status", "path with spaces"]);
    command.env("VISIBLE_TO_CHILD_ONLY", "secret");
    command.env_remove("REMOVED_FROM_CHILD");

    assert_eq!(command_invocation(&command), r#""git" "status" "path with spaces""#);
}

#[test]
fn literal_graph_open_options_deny_object_environment_at_every_trust_level() {
    let options = literal_graph_open_options();
    for trust in [gix::sec::Trust::Full, gix::sec::Trust::Reduced] {
        assert_eq!(options.by_level(trust).permissions.env.objects, gix::sec::Permission::Deny);
    }
}

#[test]
fn git_directory_identity_is_stable_and_worktree_specific() {
    let context = testutil::TestContextBuilder::new("unused").with_initial_commit().build();
    let linked = context.dir.path().join("linked-identity");
    context
        .git_cmd()
        .args(["worktree", "add", "--detach"])
        .arg(&linked)
        .arg("HEAD")
        .assert()
        .success();

    let main = Repo::open(context.repo_path.to_str().unwrap()).unwrap();
    let reopened = Repo::open(context.repo_path.to_str().unwrap()).unwrap();
    let linked = Repo::open(linked.to_str().unwrap()).unwrap();

    assert!(main.git_dir_identity() == reopened.git_dir_identity());
    assert!(main.git_dir_identity() != linked.git_dir_identity());
}

#[test]
fn git_versions_are_parsed_for_no_lazy_fetch_support() {
    for (output, expected) in [
        ("git version 2.44.0\n", (2, 44)),
        ("git version 2.45.0\n", (2, 45)),
        ("git version 2.48.1 (Apple Git-154)\n", (2, 48)),
        ("git version 3.0.0.windows.1\n", (3, 0)),
    ] {
        assert_eq!(parse_git_version(output.as_bytes()).unwrap(), expected);
    }

    for output in [b"2.45.0\n".as_slice(), b"git version invalid\n", b"git version 2\n"] {
        assert!(parse_git_version(output).is_err());
    }

    let error = require_git_no_lazy_fetch(b"git version 2.44.9\n").unwrap_err();
    assert!(error.to_string().contains("requires Git 2.45 or newer"));
    require_git_no_lazy_fetch(b"git version 2.45.0\n").unwrap();
    require_git_no_lazy_fetch(b"git version 3.0.0\n").unwrap();
}

#[test]
fn promisor_configuration_is_additive_across_every_occurrence() {
    for output in [
        b"remote.origin.promisor\ntrue\0remote.origin.promisor\nfalse\0".as_slice(),
        b"remote.origin.promisor\0remote.origin.promisor\nfalse\0",
        b"remote.origin.partialclonefilter\nblob:none\0",
        b"remote.origin.partialclonefilter\n\0",
        b"remote.origin.promisor\ntrue\0remote./cache.promisor\ntrue\0",
        b"remote.origin.promisor\ntrue\0remote./cache.promisor\nfalse\0",
        b"remote.origin.promisor\ntrue\0remote./cache.promisor\0",
        b"remote.origin.promisor\ntrue\0remote./cache.partialclonefilter\0",
    ] {
        assert!(parse_remote_promisor_config(output).unwrap(), "output: {output:?}");
    }

    for output in [
        b"remote.origin.promisor\nfalse\0".as_slice(),
        b"remote.origin.promisor\n\0remote.origin.promisor\nfalse\0",
        b"remote./cache.promisor\ntrue\0",
        b"remote./cache.partialclonefilter\nblob:none\0",
        b"remote./cache.partialclonefilter\0",
    ] {
        assert!(!parse_remote_promisor_config(output).unwrap(), "output: {output:?}");
    }

    assert!(
        parse_remote_promisor_config(
            b"remote./cache.promisor\ntrue\0remote.origin.promisor\ntrue\0"
        )
        .unwrap()
    );

    for output in [
        b"remote.origin.promisor\ninvalid\0".as_slice(),
        b"remote.origin.promisor\ntrue\0remote.origin.promisor\ninvalid\0",
        b"remote./cache.promisor\ninvalid\0",
        b"remote.origin.partialclonefilter\0",
        b"remote.origin.promisor\ntrue",
        b"remote.origin.unexpected\ntrue\0",
    ] {
        assert!(parse_remote_promisor_config(output).is_err(), "output: {output:?}");
    }
}

#[test]
fn git_config_preserves_each_promisor_occurrence_for_decoding() {
    let parse = |contents: &str| {
        let config = tempfile::NamedTempFile::new().unwrap();
        fs::write(config.path(), contents).unwrap();
        let output = cmd("git", ["config", "--file"])
            .arg(config.path())
            .args(["--null", "--get-regexp", REMOTE_PROMISOR_CONFIG_PATTERN])
            .output()
            .unwrap();
        assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
        parse_remote_promisor_config(&output.stdout)
    };

    for contents in [
        "[remote \"origin\"]\n\tpromisor = true\n\tpromisor = false\n",
        "[remote \"origin\"]\n\tpromisor\n\tpromisor = false\n",
        "[remote \"origin\"]\n\tpartialCloneFilter = blob:none\n",
        "[remote \"origin\"]\n\tpartialCloneFilter =\n",
    ] {
        assert!(parse(contents).unwrap(), "contents:\n{contents}");
    }
    assert!(!parse("[remote \"origin\"]\n\tpromisor =\n\tpromisor = false\n").unwrap());
    for contents in [
        "[remote \"origin\"]\n\tpromisor = true\n\tpromisor = invalid\n",
        "[remote \"origin\"]\n\tpartialCloneFilter\n",
    ] {
        assert!(parse(contents).is_err(), "contents:\n{contents}");
    }
}

#[test]
fn slash_leading_promisor_remote_names_are_ignored_like_git() {
    let context = testutil::TestContextBuilder::new("unused").with_initial_commit().build();
    context.run_git(&["config", "core.repositoryFormatVersion", "1"]);
    context.run_git(&["config", "extensions.partialClone", "/cache"]);
    context.run_git(&["config", "remote./cache.promisor", "true"]);
    context.run_git(&["config", "remote./cache.partialCloneFilter", "blob:none"]);

    let repository = Repo::open(context.repo_path.to_str().unwrap()).unwrap();
    assert!(!repository.has_promisor_remote().unwrap());

    context.run_git(&["config", "remote.origin.promisor", "true"]);
    let repository = Repo::open(context.repo_path.to_str().unwrap()).unwrap();
    assert!(repository.has_promisor_remote().unwrap());
}

#[test]
fn promisor_configuration_uses_direct_common_config_and_bound_repository() {
    let context = testutil::TestContextBuilder::new("unused").with_initial_commit().build();
    let global = context.dir.path().join("global.config");
    fs::write(&global, "[extensions]\n\tpartialClone = global\n").unwrap();
    let redirected = context.dir.path().join("redirected.config");
    fs::write(
        &redirected,
        "[extensions]\n\tpartialClone = redirected\n[remote \"redirected\"]\n\tpromisor = true\n",
    )
    .unwrap();
    let empty = context.dir.path().join("empty.config");
    fs::write(&empty, "").unwrap();

    // Global, command, and GIT_CONFIG scopes may describe a different
    // repository. None may create a partial-clone extension here.
    promisor_configuration_fixture(&context.repo_path, &global, &redirected, false);

    context.run_git(&["config", "core.repositoryFormatVersion", "1"]);
    context.run_git(&["config", "extensions.partialClone", "origin"]);
    // A linked worktree reads the common configuration, not its own
    // per-worktree Git directory.
    let linked = context.dir.path().join("linked");
    context
        .git_cmd()
        .args(["worktree", "add", "--detach"])
        .arg(&linked)
        .arg("HEAD")
        .assert()
        .success();
    promisor_configuration_fixture(&linked, &global, &empty, true);

    context.run_git(&["config", "--unset-all", "extensions.partialClone"]);
    context.run_git(&["config", "remote.origin.promisor", "true"]);
    // `GIT_CONFIG` must neither supply a false promisor remote nor hide
    // the selected repository's local promisor remote.
    promisor_configuration_fixture(&context.repo_path, &global, &empty, true);
}

#[test]
fn config_env_support_requires_git_2_31() {
    let error = require_git_config_env_output(b"git version 2.30.9\n").unwrap_err();
    assert_eq!(
        error.to_string(),
        "GHerrit requires Git 2.31 or newer to bind the private publication destination without placing it in the top-level Git process arguments; found Git 2.30"
    );
    require_git_config_env_output(b"git version 2.31.0\n").unwrap();
    require_git_config_env_output(b"git version 3.0.0\n").unwrap();
}

#[test]
#[should_panic(expected = "Command cannot be empty")]
fn test_cmd_macro_empty_panic() {
    cmd!("");
}

#[test]
#[should_panic(expected = "Command cannot be empty")]
fn test_cmd_macro_whitespace_panic() {
    cmd!("   ");
}

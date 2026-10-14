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
fn git_versions_are_parsed_for_no_lazy_fetch_support() {
    for (output, expected) in [
        ("git version 2.44.0\n", (2, 44, 0)),
        ("git version 2.45.0\n", (2, 45, 0)),
        ("git version 2.48.1 (Apple Git-154)\n", (2, 48, 1)),
        ("git version 3.0.0.windows.1\n", (3, 0, 0)),
    ] {
        assert_eq!(parse_git_version(output.as_bytes()).unwrap(), expected);
    }

    for output in [b"2.45.0\n".as_slice(), b"git version invalid\n", b"git version 2\n"] {
        assert!(parse_git_version(output).is_err());
    }

    let error = require_git_no_lazy_fetch(b"git version 2.44.9\n").unwrap_err();
    assert!(error.to_string().contains("requires Git 2.45.1 or newer"));
    assert!(require_git_no_lazy_fetch(b"git version 2.45.0\n").is_err());
    require_git_no_lazy_fetch(b"git version 2.45.1\n").unwrap();
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
#[should_panic(expected = "Command cannot be empty")]
fn test_cmd_macro_empty_panic() {
    cmd!("");
}

#[test]
#[should_panic(expected = "Command cannot be empty")]
fn test_cmd_macro_whitespace_panic() {
    cmd!("   ");
}

#[test]
fn test_get_repo_owner_name() {
    for (url, (owner, repo)) in [
        ("https://github.com/owner/repo.git", ("owner", "repo")),
        ("https://github.com/owner/repo", ("owner", "repo")),
        ("git@github.com:owner/repo.git", ("owner", "repo")),
        ("git@github.com:owner/repo", ("owner", "repo")),
        ("alias:owner/repo.git", ("owner", "repo")),
        ("alias:owner/repo", ("owner", "repo")),
        ("http://localhost:3000/owner/repo.git", ("owner", "repo")),
        ("http://my-gh.com/owner/repo", ("owner", "repo")),
        ("/tmp/test/owner/repo.git", ("owner", "repo")),
        ("/tmp/owner/repo", ("owner", "repo")),
        ("owner/repo", ("owner", "repo")),
        ("https://github.com/user-name/repo", ("user-name", "repo")),
        ("https://github.com/user_name/repo", ("user_name", "repo")),
        ("https://github.com/user.name/repo", ("user.name", "repo")),
        ("https://github.com/user/repo-name", ("user", "repo-name")),
        ("https://github.com/user/repo_name", ("user", "repo_name")),
        ("https://github.com/user/repo.name", ("user", "repo.name")),
        ("https://github.com/user/repo.name.git", ("user", "repo.name")),
    ] {
        let expect = (owner.to_string(), repo.to_string());
        assert_eq!(get_repo_owner_name(url).unwrap(), expect);
    }
}

#[test]
fn promisor_numeric_booleans_use_native_git_rules_for_every_occurrence() {
    let context = testutil::TestContextBuilder::new("unused").with_initial_commit().build();
    let empty = context.dir.path().join("empty.config");
    fs::write(&empty, "").unwrap();
    for (value, expected) in [
        ("1k", true),
        ("0x1", true),
        ("-0x10", true),
        ("010", true),
        ("+17", true),
        ("0k", false),
        ("0x0", false),
        ("+0", false),
    ] {
        context.run_git(&["config", "--replace-all", "remote.origin.promisor", value]);
        if expected {
            context.run_git(&["config", "--add", "remote.origin.promisor", "false"]);
        }
        promisor_configuration_fixture(&context.repo_path, &empty, &empty, expected);
    }
}

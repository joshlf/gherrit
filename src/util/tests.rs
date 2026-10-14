use std::{ffi::OsStr,process::Command};

use super::{command::command_invocation, repository::*};
use crate::cmd;

#[test]
fn git_commands_use_the_literal_local_graph() {
    let command = cmd("git", ["status"]);
    let arguments = command
        .get_args()
        .map(|argument| argument.to_string_lossy().into_owned())
        .collect::<Vec<_>>();
    assert_eq!(arguments, ["--no-replace-objects", "status"]);
    let environment = command.get_envs().collect::<std::collections::HashMap<_, _>>();
    assert_eq!(environment[OsStr::new("GIT_NO_REPLACE_OBJECTS")], Some(OsStr::new("1")));
    for variable in [
        "GIT_ALTERNATE_OBJECT_DIRECTORIES",
        "GIT_GRAFT_FILE",
        "GIT_OBJECT_DIRECTORY",
        "GIT_REPLACE_REF_BASE",
        "GIT_SHALLOW_FILE",
    ] {
        assert_eq!(environment[OsStr::new(variable)], None);
    }

    let command = cmd("gh", ["auth", "token"]);
    assert_eq!(command.get_args().collect::<Vec<_>>(), [OsStr::new("auth"), OsStr::new("token")]);
    assert!(command.get_envs().next().is_none());
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

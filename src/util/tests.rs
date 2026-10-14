use super::repository::get_repo_owner_name;
use crate::cmd;

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

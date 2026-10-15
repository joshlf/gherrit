#!/usr/bin/env python3
"""Read local execution facts without fetching or changing repository state."""

import argparse
import json
import os
import re
from pathlib import Path
import shutil
import subprocess
from urllib.parse import urlsplit, urlunsplit


def command(args, cwd):
    env = dict(os.environ, GIT_OPTIONAL_LOCKS="0", GIT_NO_LAZY_FETCH="1")
    try:
        result = subprocess.run(args, cwd=cwd, env=env, capture_output=True, timeout=10)
    except (OSError, subprocess.TimeoutExpired) as error:
        return {"status": "unavailable", "reason": type(error).__name__}
    if result.returncode:
        # Do not expose arbitrary stderr or configuration that could contain credentials.
        return {"status": "unavailable", "exit_code": result.returncode}
    try:
        value = result.stdout.decode("utf-8").rstrip("\n")
    except UnicodeError:
        return {"status": "unavailable", "reason": "non-UTF-8 output"}
    return {"status": "available", "value": value}


def redact_url(value):
    if "://" in value:
        try:
            parts = urlsplit(value)
            host = parts.hostname
            if not host:
                return "<unrecognized remote URL>"
            host = f"[{host}]" if ":" in host else host
            if parts.port:
                host += f":{parts.port}"
            return urlunsplit((parts.scheme, host, parts.path, "", ""))
        except ValueError:
            return "<unrecognized remote URL>"
    # Only SCP-shaped remotes have user information before an at-sign.
    # Slashes before the at-sign identify ordinary local filesystem paths.
    if re.fullmatch(r"[^/@:]+@(?:\[[^\]]+\]|[^/:]+):.+", value):
        return value.split("@", 1)[1]
    return value


def inspect(cwd, binary=None):
    def git(*args):
        return command(["git", *args], cwd)

    def config(key, *, trim=False):
        result = git("config", "--show-origin", "-z", "--get", key)
        if result.get("exit_code") == 1:
            return {"status": "absent"}
        if result["status"] == "available":
            fields = result.pop("value").split("\0")
            if len(fields) != 3 or fields[-1]:
                return {"status": "unavailable", "reason": "unexpected config response"}
            result.update(source=fields[0], value=fields[1].strip() if trim else fields[1])
        return result

    directory = Path(cwd).resolve()
    search_path = os.pathsep.join(str((directory / entry).resolve())
        for entry in os.environ.get("PATH", os.defpath).split(os.pathsep))
    executable = str((directory / binary).resolve()) if binary else shutil.which("gherrit", path=search_path)
    executable_info = {"path": executable, "source": "argument" if binary else "PATH"}
    executable_info["version"] = command([executable, "--version"], cwd) if executable else {"status": "absent"}
    paths = {name: git("rev-parse", "--path-format=absolute", option) for name, option in [
        ("worktree", "--show-toplevel"), ("git_directory", "--git-dir"), ("common_directory", "--git-common-dir")
    ]}
    hooks = {"configuration": config("core.hooksPath"), "effective_path": git("rev-parse", "--path-format=absolute", "--git-path", "hooks")}
    remote = config("gherrit.remote", trim=True)
    if remote["status"] == "absent":
        remote = {"status": "available", "value": "origin", "source": "GHerrit fallback"}
    result = {"schema": 1, "executable": executable_info, "paths": paths, "hooks": hooks,
              "remote": remote, "freshness": "Local observations only; remote freshness is unknown.", "default_candidates": []}
    if remote["status"] != "available":
        return result
    name = remote["value"]
    url = config(f"remote.{name}.url", trim=True)
    if url["status"] == "available":
        url["value"] = redact_url(url["value"])
    remote["url"] = url
    remote_head = git("symbolic-ref", "--quiet", f"refs/remotes/{name}/HEAD")
    result["remote_head"] = remote_head
    candidates = []
    prefix = f"refs/remotes/{name}/"
    if remote_head["status"] == "available" and remote_head["value"].startswith(prefix):
        candidates.append((remote_head["value"][len(prefix):], "remote-tracking HEAD"))
    configured = config("init.defaultBranch", trim=True)
    result["configured_default"] = configured
    if configured["status"] == "available":
        candidates.append((configured["value"], configured["source"]))
    for branch in ["main", "master", "trunk"]:
        observation = git("show-ref", "--verify", "--hash", f"refs/heads/{branch}")
        if observation["status"] == "available":
            candidates.append((branch, "common local branch name"))
    candidates.append(("main", "GHerrit fallback"))
    for branch, source in candidates:
        result["default_candidates"].append({"name": branch, "source": source,
            "local_oid": git("rev-parse", "--verify", f"refs/heads/{branch}^{{commit}}"),
            "tracking_oid": git("rev-parse", "--verify", f"refs/remotes/{name}/{branch}^{{commit}}")})
    result["selection_note"] = "Candidate order mirrors successful discovery; unavailable facts do not establish which fallback GHerrit will accept."
    return result


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--directory", default=".")
    parser.add_argument("--binary", help="Explicit executable to query with --version")
    args = parser.parse_args()
    print(json.dumps(inspect(args.directory, args.binary), indent=2))


if __name__ == "__main__":
    main()

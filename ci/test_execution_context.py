#!/usr/bin/env python3
import contextlib
import copy
import io
import json
import os
from pathlib import Path
import subprocess
import tempfile
import unittest
from unittest.mock import patch

import execution_context as context


class ExecutionContextTests(unittest.TestCase):
    def setUp(self):
        home = tempfile.TemporaryDirectory()
        self.addCleanup(home.cleanup)
        environment = patch.dict(os.environ, {"PATH": os.defpath, "HOME": home.name,
            "GIT_CONFIG_NOSYSTEM": "1", "GIT_CONFIG_GLOBAL": os.devnull}, clear=True)
        environment.start()
        self.addCleanup(environment.stop)

    def test_redaction(self):
        self.assertEqual(context.redact_url("https://user:secret@github.com/owner/repo?token=hidden#fragment"), "https://github.com/owner/repo")
        self.assertEqual(context.redact_url("git@github.com:owner/repo.git"), "github.com:owner/repo.git")
        self.assertEqual(context.redact_url("git@github.com:owner/repo@rev.git"), "github.com:owner/repo@rev.git")
        for path in ["../mirrors/team@release", "/mirrors/team@release", "./user@host:path"]:
            self.assertEqual(context.redact_url(path), path)
        self.assertEqual(context.redact_url("https://[::1]:123/owner/repo"), "https://[::1]:123/owner/repo")

    def test_real_linked_worktree_is_read_only(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            repo, linked = root / "repo", root / "linked"
            def git(*args):
                return subprocess.check_output(["git", "-c", "core.hooksPath=/dev/null", "-C", str(repo), *args], stderr=subprocess.DEVNULL, timeout=10).decode().strip()
            subprocess.run(["git", "init", "-b", "main", str(repo)], check=True, capture_output=True, timeout=10)
            git("-c", "user.name=Fixture", "-c", "user.email=fixture@example.com", "commit", "--allow-empty", "-m", "Initial")
            git("worktree", "add", "-b", "feature", str(linked))
            git("config", "core.hooksPath", " relative-hooks ")
            git("config", "gherrit.remote", " origin ")
            git("config", "init.defaultBranch", " main ")
            git("config", "remote.origin.url", " https://user:secret@github.com/owner/repo?token=hidden ")
            before = git("show-ref"), (repo / ".git/config").read_bytes()
            report = context.inspect(linked, "/missing/binary")
            self.assertEqual(report["paths"]["worktree"]["value"], str(linked.resolve()))
            self.assertEqual(report["paths"]["common_directory"]["value"], str((repo / ".git").resolve()))
            self.assertEqual(report["hooks"]["configuration"]["value"], " relative-hooks ")
            self.assertEqual(report["hooks"]["effective_path"]["value"], str(linked.resolve() / " relative-hooks "))
            self.assertEqual(report["remote"]["value"], "origin")
            self.assertEqual(report["remote"]["url"]["value"], "https://github.com/owner/repo")
            self.assertEqual(report["configured_default"]["value"], "main")
            self.assertEqual(report["default_candidates"][0]["local_oid"]["value"], git("rev-parse", "HEAD"))
            self.assertEqual(report["executable"]["version"]["status"], "unavailable")
            self.assertNotIn("secret", json.dumps(report))
            self.assertNotIn("hidden", json.dumps(report))
            self.assertEqual(before, (git("show-ref"), (repo / ".git/config").read_bytes()))

    def test_relative_path_lookup_and_execution_share_inspected_directory(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            launch, inspected = root / "launch", root / "inspected"
            launch.mkdir()
            inspected.mkdir()
            for path, version in [(launch, "launch-version"), (inspected, "inspected-version")]:
                binary = path / "gherrit"
                binary.write_text(f"#!/bin/sh\nprintf '{version}\\n'\n")
                binary.chmod(0o755)
            original = Path.cwd()
            try:
                os.chdir(launch)
                with patch.dict(os.environ, {"PATH": "."}):
                    report = context.inspect(inspected)
                    self.assertEqual(report["executable"]["path"], str(inspected.resolve() / "gherrit"))
                    self.assertEqual(report["executable"]["version"]["value"], "inspected-version")
                    explicit = context.inspect(inspected, "gherrit")
                    self.assertEqual(explicit["executable"]["version"]["value"], "inspected-version")
            finally:
                os.chdir(original)

    def test_command_outcomes_and_read_only_process_contract(self):
        for outcome, expected in [
            (subprocess.CompletedProcess([], 0, b" value \n\n", b""), {"status": "available", "value": " value "}),
            (subprocess.CompletedProcess([], 7, b"secret stdout", b"secret stderr"), {"status": "unavailable", "exit_code": 7}),
            (subprocess.CompletedProcess([], 0, b"\xff", b""), {"status": "unavailable", "reason": "non-UTF-8 output"}),
            (OSError("secret path"), {"status": "unavailable", "reason": "OSError"}),
            (subprocess.TimeoutExpired("secret command", 10), {"status": "unavailable", "reason": "TimeoutExpired"}),
        ]:
            with self.subTest(outcome=type(outcome).__name__):
                kwargs = {"side_effect": outcome} if isinstance(outcome, Exception) else {"return_value": outcome}
                with patch.object(context.subprocess, "run", **kwargs) as call:
                    self.assertEqual(context.command(["git", "status"], "/inspection"), expected)
                self.assertEqual(call.call_args.args, (["git", "status"],))
                self.assertEqual(call.call_args.kwargs["cwd"], "/inspection")
                self.assertEqual(call.call_args.kwargs["timeout"], 10)
                self.assertTrue(call.call_args.kwargs["capture_output"])
                for key in ["GIT_OPTIONAL_LOCKS", "GIT_NO_LAZY_FETCH"]:
                    self.assertEqual(call.call_args.kwargs["env"][key], "0" if key.endswith("LOCKS") else "1")

    def scripted_inspect(self, tail):
        available = lambda value: {"status": "available", "value": value}
        rows = [
            (["git", "rev-parse", "--path-format=absolute", "--show-toplevel"], available("/worktree")),
            (["git", "rev-parse", "--path-format=absolute", "--git-dir"], available("/gitdir")),
            (["git", "rev-parse", "--path-format=absolute", "--git-common-dir"], available("/common")),
            (["git", "config", "--show-origin", "-z", "--get", "core.hooksPath"], available("file:config\0 literal hooks \0")),
            (["git", "rev-parse", "--path-format=absolute", "--git-path", "hooks"], available("/effective-hooks")),
            *tail,
        ]
        def command(args, cwd):
            self.assertEqual(cwd, "/inspected")
            self.assertTrue(rows, f"unexpected command {args}")
            expected, result = rows.pop(0)
            self.assertEqual(args, expected)
            return copy.deepcopy(result)
        with patch.object(context, "command", command), patch.object(context.shutil, "which", return_value=None):
            report = context.inspect("/inspected")
        self.assertEqual(rows, [], "unconsumed local observations")
        self.assertEqual(report["schema"], 1)
        self.assertEqual(report["executable"], {"path": None, "source": "PATH", "version": {"status": "absent"}})
        self.assertEqual(report["paths"], {"worktree": available("/worktree"), "git_directory": available("/gitdir"), "common_directory": available("/common")})
        self.assertEqual(report["hooks"], {"configuration": {"status": "available", "source": "file:config", "value": " literal hooks "}, "effective_path": available("/effective-hooks")})
        self.assertEqual(report["freshness"], "Local observations only; remote freshness is unknown.")
        return report

    def test_failed_or_malformed_remote_observation_stops_discovery(self):
        for result, expected in [
            ({"status": "unavailable", "exit_code": 2}, {"status": "unavailable", "exit_code": 2}),
            ({"status": "available", "value": "missing terminator"}, {"status": "unavailable", "reason": "unexpected config response"}),
            ({"status": "available", "value": "file:config\0origin\0extra"}, {"status": "unavailable", "reason": "unexpected config response"}),
        ]:
            report = self.scripted_inspect([(["git", "config", "--show-origin", "-z", "--get", "gherrit.remote"], result)])
            self.assertEqual(report["remote"], expected)
            self.assertEqual(report["default_candidates"], [])
            self.assertNotIn("remote_head", report)
            self.assertNotIn("configured_default", report)
            self.assertNotIn("selection_note", report)

    def test_candidate_order_and_failed_oids_are_preserved(self):
        available = lambda value: {"status": "available", "value": value}
        absent = {"status": "unavailable", "exit_code": 1}
        for configured_remote in [False, True]:
            name = "upstream" if configured_remote else "origin"
            for head in [f"refs/remotes/{name}/release", "refs/remotes/other/release", None]:
                for configured_result, configured_expected in [
                    (available("file:init\0 configured \0"), {"status": "available", "value": "configured", "source": "file:init"}),
                    (absent, {"status": "absent"}),
                    ({"status": "unavailable", "exit_code": 2}, {"status": "unavailable", "exit_code": 2}),
                ]:
                    configured = configured_expected["status"] == "available"
                    tail = [
                        (["git", "config", "--show-origin", "-z", "--get", "gherrit.remote"], available("file:config\0 upstream \0") if configured_remote else absent),
                        (["git", "config", "--show-origin", "-z", "--get", f"remote.{name}.url"], absent),
                        (["git", "symbolic-ref", "--quiet", f"refs/remotes/{name}/HEAD"], available(head) if head else absent),
                        (["git", "config", "--show-origin", "-z", "--get", "init.defaultBranch"], configured_result),
                        (["git", "show-ref", "--verify", "--hash", "refs/heads/main"], available("local-main")),
                        (["git", "show-ref", "--verify", "--hash", "refs/heads/master"], absent),
                        (["git", "show-ref", "--verify", "--hash", "refs/heads/trunk"], available("local-trunk")),
                    ]
                    expected = []
                    if head == f"refs/remotes/{name}/release": expected.append(("release", "remote-tracking HEAD"))
                    if configured: expected.append(("configured", "file:init"))
                    expected += [("main", "common local branch name"), ("trunk", "common local branch name"), ("main", "GHerrit fallback")]
                    candidates = []
                    for branch, source in expected:
                        tail += [(["git", "rev-parse", "--verify", f"refs/heads/{branch}^{{commit}}"], available("local-" + branch)),
                                 (["git", "rev-parse", "--verify", f"refs/remotes/{name}/{branch}^{{commit}}"], absent)]
                        candidates.append({"name": branch, "source": source, "local_oid": available("local-" + branch), "tracking_oid": absent})
                    report = self.scripted_inspect(tail)
                    self.assertEqual(report["default_candidates"], candidates)
                    self.assertEqual(report["remote"], {"status": "available", "source": "file:config" if configured_remote else "GHerrit fallback", "value": name, "url": {"status": "absent"}})
                    self.assertEqual(report["remote_head"], available(head) if head else absent)
                    self.assertEqual(report["configured_default"], configured_expected)
                    self.assertIn("unavailable facts do not establish", report["selection_note"])

    def test_redaction_handles_malformed_urls_and_ipv6_scp(self):
        for raw in ["https:///no-host", "https://host:bad/path", "https://[broken/path"]:
            self.assertEqual(context.redact_url(raw), "<unrecognized remote URL>")
        self.assertEqual(context.redact_url("user@[::1]:owner/repo"), "[::1]:owner/repo")
        self.assertEqual(context.redact_url("ssh://user:secret@host:42/owner/repo?q=secret#secret"), "ssh://host:42/owner/repo")
        self.assertEqual(context.redact_url("relative/repo"), "relative/repo")

    def test_cli_prints_the_report_and_forwards_options(self):
        for argv, expected in [([], (".", None)), (["--directory", "elsewhere", "--binary", "tool"], ("elsewhere", "tool"))]:
            output = io.StringIO()
            with patch("sys.argv", ["execution_context.py", *argv]), patch.object(context, "inspect", return_value={"schema": 1}) as call, contextlib.redirect_stdout(output):
                context.main()
            self.assertEqual(call.call_args.args, expected)
            self.assertEqual(json.loads(output.getvalue()), {"schema": 1})

    def test_failed_command_is_not_absence(self):
        result = context.command(["/missing/executable"], ".")
        self.assertEqual(result["status"], "unavailable")
        self.assertNotIn("value", result)


if __name__ == "__main__":
    unittest.main()

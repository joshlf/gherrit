#!/usr/bin/env python3
import json
from pathlib import Path
import subprocess
import tempfile
import unittest

import execution_context as context


class ExecutionContextTests(unittest.TestCase):
    def test_redaction(self):
        self.assertEqual(context.redact_url("https://user:secret@github.com/owner/repo?token=hidden#fragment"), "https://github.com/owner/repo")
        self.assertEqual(context.redact_url("git@github.com:owner/repo.git"), "github.com:owner/repo.git")
        self.assertEqual(context.redact_url("https://[::1]:123/owner/repo"), "https://[::1]:123/owner/repo")

    def test_real_linked_worktree_is_read_only(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            repo, linked = root / "repo", root / "linked"
            def git(*args):
                return subprocess.check_output(["git", "-c", "core.hooksPath=/dev/null", "-C", str(repo), *args], stderr=subprocess.DEVNULL).decode().strip()
            subprocess.run(["git", "init", "-b", "main", str(repo)], check=True, capture_output=True)
            git("-c", "user.name=Fixture", "-c", "user.email=fixture@example.com", "commit", "--allow-empty", "-m", "Initial")
            git("worktree", "add", "-b", "feature", str(linked))
            git("config", "core.hooksPath", "relative-hooks")
            git("config", "remote.origin.url", "https://user:secret@github.com/owner/repo?token=hidden")
            before = git("show-ref"), (repo / ".git/config").read_bytes()
            report = context.inspect(linked, "/missing/binary")
            self.assertEqual(report["paths"]["worktree"]["value"], str(linked.resolve()))
            self.assertEqual(report["paths"]["common_directory"]["value"], str((repo / ".git").resolve()))
            self.assertEqual(report["hooks"]["effective_path"]["value"], str(linked.resolve() / "relative-hooks"))
            self.assertEqual(report["default_candidates"][0]["local_oid"]["value"], git("rev-parse", "HEAD"))
            self.assertEqual(report["executable"]["version"]["status"], "unavailable")
            self.assertNotIn("secret", json.dumps(report))
            self.assertNotIn("hidden", json.dumps(report))
            self.assertEqual(before, (git("show-ref"), (repo / ".git/config").read_bytes()))

    def test_failed_command_is_not_absence(self):
        result = context.command(["/missing/executable"], ".")
        self.assertEqual(result["status"], "unavailable")
        self.assertNotIn("value", result)


if __name__ == "__main__":
    unittest.main()

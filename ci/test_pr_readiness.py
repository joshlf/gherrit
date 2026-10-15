#!/usr/bin/env python3
import copy
import contextlib
import io
import subprocess
import unittest
from unittest.mock import patch

import pr_readiness as readiness

HEAD, BASE = "a" * 40, "b" * 40
PREFIX = "repos/owner/repo"
PR_PATH = f"{PREFIX}/pulls/7"
PR = {"number": 7, "head": {"sha": HEAD}, "base": {"sha": BASE}}
CHECK_PATH = f"{PREFIX}/commits/{HEAD}/check-runs?filter=all&per_page=100&page=1"
RUN_PATH = f"{PREFIX}/actions/runs?head_sha={HEAD}&per_page=100&page=1"
REVIEW_PATH = f"{PR_PATH}/reviews?per_page=100&page=1"


def run(conclusion="success", status="completed"):
    return {"id": 8, "workflow_id": 12, "event": "pull_request", "head_sha": HEAD,
            "status": status, "conclusion": conclusion, "pull_requests": [PR]}


class Script:
    def __init__(self, rows):
        self.rows = rows
        self.calls = []

    def __call__(self, path, method="GET"):
        self.calls.append((path, method))
        if not self.rows:
            raise AssertionError(f"Unexpected {method} {path}")
        expected_path, expected_method, result = self.rows.pop(0)
        if (path, method) != (expected_path, expected_method):
            raise AssertionError((path, method, expected_path, expected_method))
        if isinstance(result, Exception):
            raise result
        return copy.deepcopy(result)


def observations(runs=(), checks=(), reviews=(), after=PR):
    return [(PR_PATH, "GET", PR),
            (CHECK_PATH, "GET", {"total_count": len(checks), "check_runs": list(checks)}),
            (RUN_PATH, "GET", {"total_count": len(runs), "workflow_runs": list(runs)}),
            (REVIEW_PATH, "GET", list(reviews)), (PR_PATH, "GET", after)]


class ReadinessTests(unittest.TestCase):
    def observe(self, rows, **kwargs):
        script = Script(rows)
        with patch.object(readiness, "api", script):
            report = readiness.observe("owner/repo", 7, **kwargs)
        self.assertEqual(script.rows, [])
        return report

    def test_empty_and_states_are_distinct(self):
        self.assertEqual(self.observe(observations())["workflows_state"], "no_run")
        for row, expected in [(run(), "successful"), (run("action_required"), "approval_required"),
                              (run(None, "in_progress"), "running"), (run("failure"), "failed"),
                              (run("skipped"), "unknown")]:
            self.assertEqual(self.observe(observations(runs=[row]))["workflows_state"], expected)

    def test_latest_attempt_and_head_specific_formal_reviews(self):
        failed, passed = run("failure"), run()
        passed["id"] = 9
        reviews = [{"id": 1, "user": {"login": "old"}, "state": "APPROVED", "commit_id": "c" * 40},
                   {"id": 2, "user": {"login": "current"}, "state": "APPROVED", "commit_id": HEAD},
                   {"id": 3, "user": {"login": "commenter"}, "state": "COMMENTED", "commit_id": HEAD}]
        report = self.observe(observations(runs=[failed, passed], reviews=reviews))
        self.assertEqual(report["workflows_state"], "successful")
        self.assertEqual([(r["login"], r["current_head"]) for r in report["reviews"]], [("old", False), ("current", True)])
        self.assertEqual(len(report["workflow_runs"]), 2)

    def test_changed_base_invalidates_summary(self):
        changed = copy.deepcopy(PR)
        changed["base"]["sha"] = "c" * 40
        report = self.observe(observations(runs=[run()], after=changed))
        self.assertFalse(report["observation_consistent"])
        self.assertEqual(report["workflows_state"], "unknown")

    def test_paginated_reviews_and_explicit_cutoff(self):
        rows = [{"id": n} for n in range(100)]
        script = Script([("reviews?per_page=100&page=1", "GET", rows), ("reviews?per_page=100&page=2", "GET", [{"id": 100}])])
        with patch.object(readiness, "api", script):
            items, complete = readiness.pages("reviews")
        self.assertEqual(len(items), 101)
        self.assertTrue(complete)
        with patch.object(readiness, "api", return_value={"workflow_runs": rows, "total_count": 1001}):
            items, complete = readiness.pages("runs", "workflow_runs", max_pages=10)
        self.assertEqual(len(items), 1000)
        self.assertFalse(complete)
        self.assertEqual(readiness.aggregate(items, complete), "unknown")

    def test_approval_requires_expected_commits(self):
        script = Script([(PR_PATH, "GET", PR)])
        with patch.object(readiness, "api", script), self.assertRaises(readiness.ObservationError):
            readiness.observe("owner/repo", 7, approve=True, expected=("c" * 40, BASE))
        self.assertFalse(any(method == "POST" for _, method in script.calls))

    def test_approval_rechecks_pr_and_run(self):
        waiting = run("action_required")
        rows = observations(runs=[waiting]) + [(PR_PATH, "GET", PR),
            (f"{PREFIX}/actions/runs/8", "GET", waiting),
            (f"{PREFIX}/actions/runs/8/approve", "POST", None), (PR_PATH, "GET", PR)]
        report = self.observe(rows, approve=True, expected=(HEAD, BASE))
        self.assertEqual(report["approved_workflow_runs"], [8])
        self.assertEqual(report["workflows_state"], "approval_required")  # observation predates approval

    def test_no_approval_when_run_base_changed(self):
        waiting = run("action_required")
        changed = copy.deepcopy(waiting)
        changed["pull_requests"][0]["base"]["sha"] = "c" * 40
        script = Script(observations(runs=[waiting]) + [(PR_PATH, "GET", PR), (f"{PREFIX}/actions/runs/8", "GET", changed)])
        with patch.object(readiness, "api", script), self.assertRaises(readiness.ObservationError):
            readiness.observe("owner/repo", 7, approve=True, expected=(HEAD, BASE))
        self.assertFalse(any(method == "POST" for _, method in script.calls))

    def test_changed_pr_before_approval_sends_no_post(self):
        changed = copy.deepcopy(PR)
        changed["head"]["sha"] = "c" * 40
        script = Script(observations(runs=[run("action_required")]) + [(PR_PATH, "GET", changed)])
        with patch.object(readiness, "api", script), self.assertRaises(readiness.ObservationError):
            readiness.observe("owner/repo", 7, approve=True, expected=(HEAD, BASE))
        self.assertFalse(any(method == "POST" for _, method in script.calls))

    def test_failed_approval_reports_uncertainty(self):
        waiting = run("action_required")
        script = Script(observations(runs=[waiting]) + [(PR_PATH, "GET", PR),
            (f"{PREFIX}/actions/runs/8", "GET", waiting),
            (f"{PREFIX}/actions/runs/8/approve", "POST", readiness.ObservationError("request failed"))])
        with patch.object(readiness, "api", script), self.assertRaisesRegex(readiness.ObservationError, "uncertain outcome"):
            readiness.observe("owner/repo", 7, approve=True, expected=(HEAD, BASE))

    def test_later_read_failure_reports_completed_approvals(self):
        first = run("action_required")
        second = copy.deepcopy(first)
        second.update(id=9, workflow_id=13)
        rows = observations(runs=[first, second]) + [(PR_PATH, "GET", PR),
            (f"{PREFIX}/actions/runs/8", "GET", first),
            (f"{PREFIX}/actions/runs/8/approve", "POST", None), (PR_PATH, "GET", PR),
            (f"{PREFIX}/actions/runs/9", "GET", readiness.ObservationError("read unavailable"))]
        script = Script(rows)
        with patch.object(readiness, "api", script), self.assertRaisesRegex(readiness.ObservationError, r"completed approvals \[8\]"):
            readiness.observe("owner/repo", 7, approve=True, expected=(HEAD, BASE))
        self.assertEqual(script.rows, [])

    def test_cli_api_failure_does_not_expose_stderr(self):
        result = subprocess.CompletedProcess([], 1, b"", b"secret credential")
        with patch.object(readiness.subprocess, "run", return_value=result) as call:
            with self.assertRaisesRegex(readiness.ObservationError, "exit 1") as raised:
                readiness.api("repos/owner/repo/pulls/7")
        self.assertNotIn("secret", str(raised.exception))
        self.assertEqual(call.call_args.kwargs["timeout"], 30)

    def test_other_pr_on_same_head_does_not_hide_target_approval(self):
        waiting = run("action_required")
        other = copy.deepcopy(run())
        other["id"] = 9
        other["pull_requests"][0]["number"] = 8
        rows = observations(runs=[waiting, other]) + [(PR_PATH, "GET", PR),
            (f"{PREFIX}/actions/runs/8", "GET", waiting),
            (f"{PREFIX}/actions/runs/8/approve", "POST", None), (PR_PATH, "GET", PR)]
        report = self.observe(rows, approve=True, expected=(HEAD, BASE))
        self.assertEqual(report["approved_workflow_runs"], [8])
        self.assertEqual(report["workflows_state"], "approval_required")

    def test_old_base_and_unbound_workflows_are_not_current_success(self):
        old_base = copy.deepcopy(run())
        old_base["pull_requests"][0]["base"]["sha"] = "c" * 40
        self.assertEqual(self.observe(observations(runs=[old_base]))["workflows_state"], "no_run")
        unbound = copy.deepcopy(run())
        unbound["pull_requests"] = []
        report = self.observe(observations(runs=[unbound]))
        self.assertEqual(report["workflows_state"], "unknown")
        self.assertFalse(report["complete"]["workflows"])

    def test_nullable_check_app_and_check_requested_action(self):
        check = {"id": 20, "head_sha": HEAD, "name": "External app", "app": None,
                 "status": "completed", "conclusion": "success"}
        report = self.observe(observations(checks=[check]))
        self.assertEqual(report["checks_state"], "successful")
        check["conclusion"] = "action_required"
        report = self.observe(observations(checks=[check]))
        self.assertEqual(report["checks_state"], "action_required")
        self.assertEqual(report["approved_workflow_runs"], [])

    def test_cli_normalizes_accepted_uppercase_commits(self):
        with patch("sys.argv", ["pr_readiness.py", "--repo", "owner/repo", "7",
                "--approve-workflows", "--expect-head", HEAD.upper(), "--expect-base", BASE.upper()]), \
                patch.object(readiness, "observe", return_value={}) as call, contextlib.redirect_stdout(io.StringIO()):
            readiness.main()
        self.assertEqual(call.call_args.args, ("owner/repo", 7, True, (HEAD, BASE)))

    def test_api_failures_are_unavailable_not_success(self):
        script = Script([(PR_PATH, "GET", readiness.ObservationError("unavailable"))])
        with patch.object(readiness, "api", script), self.assertRaises(readiness.ObservationError):
            readiness.observe("owner/repo", 7)


if __name__ == "__main__":
    unittest.main()

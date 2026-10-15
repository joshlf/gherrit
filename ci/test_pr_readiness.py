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
            "status": status, "conclusion": conclusion, "pull_requests": [copy.deepcopy(PR)]}


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
    def script(self, rows):
        script = Script(rows)
        self.addCleanup(self.assertEqual, script.rows, [], "unconsumed API expectations")
        return script

    def observe(self, rows, **kwargs):
        script = self.script(rows)
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
        script = self.script([("reviews?per_page=100&page=1", "GET", rows), ("reviews?per_page=100&page=2", "GET", [{"id": 100}])])
        with patch.object(readiness, "api", script):
            items, complete = readiness.pages("reviews")
        self.assertEqual(len(items), 101)
        self.assertTrue(complete)
        script = self.script([(f"runs?per_page=100&page={page}", "GET", {"workflow_runs": rows, "total_count": 1001}) for page in range(1, 11)])
        with patch.object(readiness, "api", script):
            items, complete = readiness.pages("runs", "workflow_runs", max_pages=10)
        self.assertEqual(len(items), 1000)
        self.assertFalse(complete)
        self.assertEqual(readiness.aggregate(items, complete), "unknown")

    def test_approval_requires_expected_commits(self):
        script = self.script([(PR_PATH, "GET", PR)])
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
        script = self.script(observations(runs=[waiting]) + [(PR_PATH, "GET", PR), (f"{PREFIX}/actions/runs/8", "GET", changed)])
        with patch.object(readiness, "api", script), self.assertRaises(readiness.ObservationError):
            readiness.observe("owner/repo", 7, approve=True, expected=(HEAD, BASE))
        self.assertFalse(any(method == "POST" for _, method in script.calls))

    def test_changed_pr_before_approval_sends_no_post(self):
        changed = copy.deepcopy(PR)
        changed["head"]["sha"] = "c" * 40
        script = self.script(observations(runs=[run("action_required")]) + [(PR_PATH, "GET", changed)])
        with patch.object(readiness, "api", script), self.assertRaises(readiness.ObservationError):
            readiness.observe("owner/repo", 7, approve=True, expected=(HEAD, BASE))
        self.assertFalse(any(method == "POST" for _, method in script.calls))

    def test_failed_approval_reports_uncertainty(self):
        waiting = run("action_required")
        script = self.script(observations(runs=[waiting]) + [(PR_PATH, "GET", PR),
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
        script = self.script(rows)
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
        for length in [40, 64]:
            head, base = "a" * length, "b" * length
            with patch("sys.argv", ["pr_readiness.py", "--repo", "owner/repo", "7",
                    "--approve-workflows", "--expect-head", head.upper(), "--expect-base", base.upper()]), \
                    patch.object(readiness, "observe", return_value={}) as call, contextlib.redirect_stdout(io.StringIO()):
                readiness.main()
            self.assertEqual(call.call_args.args, ("owner/repo", 7, True, (head, base)))

    def test_reported_total_requires_a_nonnegative_integer(self):
        for total in [False, True, -1, "1", None, 2]:
            script = self.script([("runs?per_page=100&page=1", "GET", {"workflow_runs": [{"id": 1}], "total_count": total})])
            with patch.object(readiness, "api", script):
                self.assertEqual(readiness.pages("runs", "workflow_runs"), ([{"id": 1}], False))

    def test_identity_rejects_missing_or_unusable_object_ids(self):
        for bad in [None, {}, {"head": None, "base": {"sha": BASE}}]:
            with self.subTest(bad=bad), self.assertRaises(readiness.ObservationError):
                readiness.identity(bad)
        for value in [None, True, 1, {}, [], "", "a" * 39, "a" * 41, "a" * 63, "a" * 65, "g" * 40]:
            for field in ["head", "base"]:
                bad = copy.deepcopy(PR)
                bad[field]["sha"] = value
                with self.subTest(field=field, value=value), self.assertRaises(readiness.ObservationError):
                    readiness.identity(bad)
        for length in [40, 64]:
            pair = "a" * length, "b" * length
            self.assertEqual(readiness.identity({"head": {"sha": pair[0]}, "base": {"sha": pair[1]}}), pair)

    def test_api_success_and_safe_failures(self):
        for output, expected in [(b'{"id": 1}', {"id": 1}), (b" \n", None)]:
            with patch.object(readiness.subprocess, "run", return_value=subprocess.CompletedProcess([], 0, output, b"secret")) as call:
                self.assertEqual(readiness.api("path", "POST"), expected)
                self.assertEqual(call.call_args.args, (["gh", "api", "--method", "POST", "path"],))
                self.assertEqual(call.call_args.kwargs["timeout"], 30)
        for output in [b"not JSON secret", b"\xff"]:
            with patch.object(readiness.subprocess, "run", return_value=subprocess.CompletedProcess([], 0, output, b"secret")), self.assertRaisesRegex(readiness.ObservationError, "unusable JSON") as raised:
                readiness.api("path")
            self.assertNotIn("secret", str(raised.exception))
        for error in [OSError("secret"), subprocess.TimeoutExpired("secret", 30)]:
            with patch.object(readiness.subprocess, "run", side_effect=error), self.assertRaisesRegex(readiness.ObservationError, "unavailable") as raised:
                readiness.api("path")
            self.assertNotIn("secret", str(raised.exception))

    def test_every_status_and_aggregate_priority(self):
        cases = [(status, None, "running") for status in ["queued", "requested", "waiting", "pending", "in_progress"]]
        cases += [("completed", conclusion, "failed") for conclusion in ["failure", "cancelled", "timed_out", "startup_failure", "stale"]]
        cases += [("completed", "success", "successful"), ("completed", "neutral", "unknown"), (None, None, "unknown"), ("future", "success", "unknown")]
        for status, conclusion, expected in cases:
            self.assertEqual(readiness.state({"status": status, "conclusion": conclusion}), expected)
        for status in [None, "queued", "completed", "future"]:
            self.assertEqual(readiness.state({"status": status, "conclusion": "action_required"}), "action_required")
        priority = ["unknown", "approval_required", "action_required", "failed", "running", "successful"]
        for a, left in enumerate(priority):
            for b, right in enumerate(priority):
                rows = [left, right]
                self.assertEqual(readiness.aggregate(rows, True, lambda value: value), priority[min(a, b)])
                self.assertEqual(readiness.aggregate(rows, False, lambda value: value), "unknown")
        self.assertEqual(readiness.aggregate([], True), "no_run")
        self.assertEqual(readiness.aggregate([], False), "unknown")
        for event, expected in [("pull_request", "approval_required"), ("push", "action_required"), (None, "action_required")]:
            self.assertEqual(readiness.workflow_state({"event": event, "conclusion": "action_required"}), expected)

    def test_pagination_rejects_bad_shapes_and_preserves_every_row(self):
        for payload in [None, [], {}, {"check_runs": None}, {"check_runs": [1]}]:
            script = self.script([("checks?filter=all&per_page=100&page=1", "GET", payload)])
            with patch.object(readiness, "api", script), self.assertRaises(readiness.ObservationError):
                readiness.pages("checks?filter=all", "check_runs")
        for payload in [None, {}, [1]]:
            script = self.script([("reviews?per_page=100&page=1", "GET", payload)])
            with patch.object(readiness, "api", script), self.assertRaises(readiness.ObservationError):
                readiness.pages("reviews")
        script = self.script([("checks?per_page=100&page=1", "GET", {"check_runs": []})])
        with patch.object(readiness, "api", script): self.assertEqual(readiness.pages("checks", "check_runs"), ([], True))

    def test_grouping_keeps_apps_names_events_and_reviewers_distinct(self):
        checks = [
            {"id": 9, "head_sha": HEAD, "name": "build", "app": {"id": 1}, "status": "completed", "conclusion": "success"},
            {"id": 8, "head_sha": HEAD, "name": "build", "app": {"id": 1}, "status": "completed", "conclusion": "failure"},
            {"id": 7, "head_sha": HEAD, "name": "build", "app": {"id": 2}, "status": "completed", "conclusion": "failure"},
        ]
        reviews = [{"id": n, "user": {"login": user}, "state": state, "commit_id": commit} for n, user, state, commit in [
            (3, "alice", "DISMISSED", HEAD), (1, "alice", "APPROVED", HEAD), (4, "alice", "COMMENTED", HEAD),
            (2, "bob", "CHANGES_REQUESTED", "c" * 40)]]
        report = self.observe(observations(checks=checks, reviews=reviews))
        self.assertEqual(report["checks_state"], "failed")
        self.assertEqual(report["checks"], checks)
        self.assertEqual(report["reviews"], [{"login": "alice", "state": "DISMISSED", "commit": HEAD, "current_head": True},
            {"login": "bob", "state": "CHANGES_REQUESTED", "commit": "c" * 40, "current_head": False}])
        with self.assertRaises(readiness.ObservationError): readiness.check_key({"app": []})
        first, tied = {"id": 1, "group": 0, "value": "first"}, {"id": 1, "group": 0, "value": "tied"}
        self.assertEqual(readiness.latest([first, tied], lambda row: row["group"]), [first])

    def test_wrong_head_or_changed_identity_cannot_report_success(self):
        wrong = run()
        wrong["head_sha"] = "c" * 40
        report = self.observe(observations(runs=[wrong], checks=[wrong]))
        self.assertEqual(report["checks"], [])
        self.assertEqual(report["workflow_runs"], [])
        self.assertEqual(report["complete"], {"checks": False, "workflows": False, "reviews": True})
        self.assertEqual(report["checks_state"], "unknown")
        self.assertEqual(report["workflows_state"], "unknown")
        changed = copy.deepcopy(PR)
        changed["head"]["sha"] = "c" * 40
        report = self.observe(observations(runs=[run()], after=changed))
        self.assertFalse(report["observation_consistent"])
        self.assertEqual(report["checks_state"], "unknown")
        script = self.script(observations(runs=[run("action_required")], after=changed))
        with patch.object(readiness, "api", script), self.assertRaisesRegex(readiness.ObservationError, "Changed or incomplete"):
            readiness.observe("owner/repo", 7, approve=True, expected=(HEAD, BASE))

    def test_missing_expected_or_incomplete_workflows_cannot_be_approved(self):
        for expected, waiting, message in [(None, run("action_required"), "requires expected"), ((HEAD, BASE), dict(run("action_required"), pull_requests=[]), "Changed or incomplete")]:
            script = self.script(observations(runs=[waiting]))
            with patch.object(readiness, "api", script), self.assertRaisesRegex(readiness.ObservationError, message):
                readiness.observe("owner/repo", 7, approve=True, expected=expected)
        report = self.observe(observations(runs=[run()]) + [(PR_PATH, "GET", PR)], approve=True, expected=(HEAD, BASE))
        self.assertEqual(report["approved_workflow_runs"], [])

    def test_each_fresh_run_guard_blocks_approval(self):
        mutations = [{"head_sha": "c" * 40}, {"event": "push"}, {"conclusion": "success"},
                     {"pull_requests": []}, {"pull_requests": None}, {"pull_requests": [None]}]
        for mutation in mutations:
            script = self.script(observations(runs=[run("action_required")]) + [(PR_PATH, "GET", PR),
                (f"{PREFIX}/actions/runs/8", "GET", dict(run("action_required"), **mutation))])
            with self.subTest(mutation=mutation), patch.object(readiness, "api", script), self.assertRaises(readiness.ObservationError):
                readiness.observe("owner/repo", 7, approve=True, expected=(HEAD, BASE))
        for bad_id in [None, True, 0, -1, "8"]:
            waiting = dict(run("action_required"), id=bad_id)
            script = self.script(observations(runs=[waiting]) + [(PR_PATH, "GET", PR)])
            with patch.object(readiness, "api", script), self.assertRaisesRegex(readiness.ObservationError, "Unusable workflow run ID"):
                readiness.observe("owner/repo", 7, approve=True, expected=(HEAD, BASE))

    def test_push_event_needs_no_pr_binding_and_is_never_approved(self):
        row = dict(run("action_required"), event="push", pull_requests=[])
        report = self.observe(observations(runs=[row]) + [(PR_PATH, "GET", PR)], approve=True, expected=(HEAD, BASE))
        self.assertEqual(report["workflows_state"], "action_required")
        self.assertEqual(report["workflow_runs"], [row])
        self.assertEqual(report["approved_workflow_runs"], [])

    def test_successful_approval_prefix_is_preserved_after_final_pr_change(self):
        waiting = run("action_required")
        changed = copy.deepcopy(PR)
        changed["base"]["sha"] = "c" * 40
        report = self.observe(observations(runs=[waiting]) + [(PR_PATH, "GET", PR),
            (f"{PREFIX}/actions/runs/8", "GET", waiting), (f"{PREFIX}/actions/runs/8/approve", "POST", None),
            (PR_PATH, "GET", changed)], approve=True, expected=(HEAD, BASE))
        self.assertEqual(report["approved_workflow_runs"], [8])
        self.assertFalse(report["observation_consistent"])
        self.assertEqual(report["checks_state"], "unknown")
        self.assertEqual(report["workflows_state"], "unknown")

    def test_cli_rejects_bad_inputs_before_observation(self):
        for args in [["--repo", "bad", "7"], ["--repo", "owner/repo", "0"],
                     ["--repo", "owner/repo", "7", "--expect-head", HEAD],
                     ["--repo", "owner/repo", "7", "--expect-base", BASE],
                     ["--repo", "owner/repo", "7", "--expect-head", "bad", "--expect-base", BASE],
                     ["--repo", "owner/repo", "7", "--approve-workflows"]]:
            with patch("sys.argv", ["readiness.py", *args]), patch.object(readiness, "observe") as call, contextlib.redirect_stderr(io.StringIO()), self.assertRaises(SystemExit) as exit:
                readiness.main()
            self.assertEqual(exit.exception.code, 2)
            call.assert_not_called()
        output = io.StringIO()
        with patch("sys.argv", ["readiness.py", "--repo", "owner/repo", "7"]), patch.object(readiness, "observe", return_value={"ok": True}) as call, contextlib.redirect_stdout(output):
            readiness.main()
        self.assertEqual(call.call_args.args, ("owner/repo", 7, False, None))
        self.assertEqual(output.getvalue(), '{\n  "ok": true\n}\n')
        for error in [readiness.ObservationError("unavailable"), KeyError("field"), TypeError("shape"), ValueError("value")]:
            message = io.StringIO()
            with patch("sys.argv", ["readiness.py", "--repo", "owner/repo", "7"]), patch.object(readiness, "observe", side_effect=error), contextlib.redirect_stderr(message), self.assertRaises(SystemExit) as exit:
                readiness.main()
            self.assertEqual(exit.exception.code, 1)
            self.assertIn("Observation/approval incomplete", message.getvalue())

    def test_every_observation_failure_stops_without_a_post(self):
        all_rows = observations(runs=[run("action_required")])
        for index in range(len(all_rows)):
            rows = copy.deepcopy(all_rows[:index + 1])
            path, method, _ = rows[-1]
            rows[-1] = (path, method, readiness.ObservationError("unavailable"))
            script = self.script(rows)
            with patch.object(readiness, "api", script), self.assertRaises(readiness.ObservationError):
                readiness.observe("owner/repo", 7, approve=True, expected=(HEAD, BASE))
            self.assertTrue(all(method == "GET" for _, method in script.calls))

    def test_malformed_fresh_run_or_binding_is_an_observation_error(self):
        for reply in [None, [], 1]:
            script = self.script(observations(runs=[run("action_required")]) + [(PR_PATH, "GET", PR),
                (f"{PREFIX}/actions/runs/8", "GET", reply)])
            with patch.object(readiness, "api", script), self.assertRaisesRegex(readiness.ObservationError, "workflow run shape"):
                readiness.observe("owner/repo", 7, approve=True, expected=(HEAD, BASE))
        script = self.script(observations(runs=[dict(run(), pull_requests=[None])])[:4])
        with patch.object(readiness, "api", script), self.assertRaisesRegex(readiness.ObservationError, "bindings"):
            readiness.observe("owner/repo", 7)

    def test_later_uncertain_post_keeps_the_completed_prefix_without_replay(self):
        first, second = run("action_required"), dict(run("action_required"), id=9, workflow_id=13)
        script = self.script(observations(runs=[first, second]) + [(PR_PATH, "GET", PR),
            (f"{PREFIX}/actions/runs/8", "GET", first), (f"{PREFIX}/actions/runs/8/approve", "POST", None),
            (PR_PATH, "GET", PR), (f"{PREFIX}/actions/runs/9", "GET", second),
            (f"{PREFIX}/actions/runs/9/approve", "POST", readiness.ObservationError("lost reply"))])
        with patch.object(readiness, "api", script), self.assertRaisesRegex(readiness.ObservationError, r"uncertain outcome; prior completed approvals \[8\]"):
            readiness.observe("owner/repo", 7, approve=True, expected=(HEAD, BASE))
        self.assertEqual([path for path, method in script.calls if method == "POST"], [f"{PREFIX}/actions/runs/8/approve", f"{PREFIX}/actions/runs/9/approve"])


if __name__ == "__main__":
    unittest.main()

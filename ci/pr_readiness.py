#!/usr/bin/env python3
"""Observe current-head checks and reviews; optionally approve guarded workflow runs."""

import argparse
import json
import re
import subprocess
from urllib.parse import urlencode


class ObservationError(RuntimeError):
    pass


def api(path, method="GET"):
    try:
        result = subprocess.run(["gh", "api", "--method", method, path], capture_output=True, timeout=30)
    except (OSError, subprocess.TimeoutExpired) as error:
        raise ObservationError(f"GitHub request unavailable: {type(error).__name__}") from error
    if result.returncode:
        raise ObservationError(f"GitHub request failed (exit {result.returncode})")
    try:
        return json.loads(result.stdout) if result.stdout.strip() else None
    except (ValueError, UnicodeError) as error:
        raise ObservationError("GitHub returned unusable JSON") from error


def pages(path, key=None, max_pages=100):
    """Bound pagination and expose incomplete observations, including Actions' 1,000 cap."""
    items = []
    for page in range(1, max_pages + 1):
        payload = api(path + ("&" if "?" in path else "?") + urlencode({"per_page": 100, "page": page}))
        rows = payload.get(key) if key and isinstance(payload, dict) else payload if not key else None
        if not isinstance(rows, list) or any(not isinstance(row, dict) for row in rows):
            raise ObservationError("Unexpected paginated response shape")
        items.extend(rows)
        if len(rows) < 100:
            # A provider total larger than the rows contradicts completeness.
            total = payload.get("total_count", len(items)) if key else len(items)
            return items, isinstance(total, int) and total <= len(items)
    return items, False


def identity(pr):
    try:
        return pr["head"]["sha"], pr["base"]["sha"]
    except (KeyError, TypeError) as error:
        raise ObservationError("Pull request lacks head/base identity") from error


def state(row):
    status, conclusion = row.get("status"), row.get("conclusion")
    if conclusion == "action_required":
        return "action_required"
    if status in {"queued", "requested", "waiting", "pending", "in_progress"}:
        return "running"
    if status == "completed":
        if conclusion == "success":
            return "successful"
        if conclusion in {"failure", "cancelled", "timed_out", "startup_failure", "stale"}:
            return "failed"
    return "unknown"


def workflow_state(row):
    value = state(row)
    return "approval_required" if value == "action_required" and row.get("event") == "pull_request" else value


def check_key(row):
    app = row.get("app")
    if app is not None and not isinstance(app, dict):
        raise ObservationError("Unexpected check-run app shape")
    return (app.get("id") if app else None, row.get("name"))


def aggregate(rows, complete, classify=state):
    if not complete:
        return "unknown"
    if not rows:
        return "no_run"
    states = {classify(row) for row in rows}
    for value in ["unknown", "approval_required", "action_required", "failed", "running"]:
        if value in states:
            return value
    return "successful"


def latest(rows, group):
    result = {}
    for row in rows:
        key = group(row)
        if key not in result or row["id"] > result[key]["id"]:
            result[key] = row
    return list(result.values())


def observe(repo, number, approve=False, expected=None):
    prefix = f"repos/{repo}"
    pr_path = f"{prefix}/pulls/{number}"
    before = api(pr_path)
    head, base = identity(before)
    if expected is not None and (head, base) != expected:
        raise ObservationError("Pull request differs from the expected head/base; no approval sent")
    checks, checks_complete = pages(f"{prefix}/commits/{head}/check-runs?filter=all", "check_runs")
    runs, runs_complete = pages(f"{prefix}/actions/runs?head_sha={head}", "workflow_runs", max_pages=10)
    reviews, reviews_complete = pages(f"{pr_path}/reviews")
    checks_complete = checks_complete and all(row.get("head_sha") == head for row in checks)
    runs_complete = runs_complete and all(row.get("head_sha") == head for row in runs)
    checks = [row for row in checks if row.get("head_sha") == head]
    runs = [row for row in runs if row.get("head_sha") == head]
    # Head SHA is shared by PRs with different bases. Retain every run in the
    # evidence, but select this PR/base before choosing its latest attempt.
    target_runs = []
    for row in runs:
        if row.get("event") == "pull_request":
            bindings = row.get("pull_requests")
            if not isinstance(bindings, list) or not bindings:
                runs_complete = False
                continue
            if not any(item.get("number") == number and identity(item) == (head, base) for item in bindings):
                continue
        target_runs.append(row)
    current_checks = latest(checks, check_key)
    current_runs = latest(target_runs, lambda row: (row.get("workflow_id"), row.get("event"),
        (number, head, base) if row.get("event") == "pull_request" else None))
    effective_reviews = latest([row for row in reviews if row.get("state") in {"APPROVED", "CHANGES_REQUESTED", "DISMISSED"}], lambda row: row["user"]["login"])
    after = api(pr_path)
    consistent = identity(after) == (head, base)
    approved = []
    if approve:
        if expected is None:
            raise ObservationError("Approval requires expected head and base commits")
        if not consistent or not runs_complete:
            raise ObservationError("Changed or incomplete workflow observation; no approval sent")
        try:
            for row in current_runs:
                if workflow_state(row) != "approval_required":
                    continue
                if identity(api(pr_path)) != expected:
                    raise ObservationError(f"PR changed; stopped after approvals {approved}")
                run_id = row.get("id")
                if type(run_id) is not int or run_id <= 0:
                    raise ObservationError("Unusable workflow run ID")
                run = api(f"{prefix}/actions/runs/{run_id}")
                linked = any(item.get("number") == number and identity(item) == expected for item in run.get("pull_requests", []))
                if run.get("head_sha") != head or run.get("event") != "pull_request" or workflow_state(run) != "approval_required" or not linked:
                    raise ObservationError(f"Workflow identity/state changed or is not bound to this PR/base; stopped after approvals {approved}")
                # No claim of a compare-and-swap fence: the PR can still change
                # between the final observation and this provider operation.
                try:
                    api(f"{prefix}/actions/runs/{run_id}/approve", "POST")
                except ObservationError as error:
                    raise ObservationError(f"Approval of run {run_id} has uncertain outcome; prior completed approvals {approved}: {error}") from error
                approved.append(run_id)
            consistent = identity(api(pr_path)) == (head, base)
        except (ObservationError, KeyError, TypeError, ValueError) as error:
            raise ObservationError(f"Approval incomplete; completed approvals {approved}: {error}") from error

    return {
        "repository": repo, "pull_request": number, "head": head, "base": base,
        "observation_consistent": consistent,
        "checks_state": aggregate(current_checks, checks_complete) if consistent else "unknown",
        "workflows_state": aggregate(current_runs, runs_complete, workflow_state) if consistent else "unknown",
        "complete": {"checks": checks_complete, "workflows": runs_complete, "reviews": reviews_complete},
        "checks": checks, "workflow_runs": runs,
        "reviews": [{"login": row["user"]["login"], "state": row["state"], "commit": row.get("commit_id"), "current_head": row.get("commit_id") == head} for row in effective_reviews],
        "approved_workflow_runs": approved,
        "limitations": "Head/base observations are not a concurrency fence or merge permission. Required rules, merge-commit checks, status contexts, review comments/reactions and author approval are not evaluated. Approval results precede fresh CI completion; rerun this helper.",
    }


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--repo", required=True)
    parser.add_argument("number", type=int)
    parser.add_argument("--approve-workflows", action="store_true")
    parser.add_argument("--expect-head")
    parser.add_argument("--expect-base")
    args = parser.parse_args()
    if not re.fullmatch(r"[A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+", args.repo) or args.number <= 0:
        parser.error("Use an explicit owner/repository and positive PR number")
    expected = (args.expect_head, args.expect_base) if args.expect_head and args.expect_base else None
    if (args.expect_head or args.expect_base) and expected is None:
        parser.error("Provide both expected commits")
    if expected and any(not re.fullmatch(r"[0-9a-fA-F]{40}|[0-9a-fA-F]{64}", oid) for oid in expected):
        parser.error("Expected commits must be full hexadecimal object IDs")
    if expected:
        expected = tuple(oid.lower() for oid in expected)
    if args.approve_workflows and expected is None:
        parser.error("--approve-workflows requires --expect-head and --expect-base")
    try:
        print(json.dumps(observe(args.repo, args.number, args.approve_workflows, expected), indent=2))
    except (ObservationError, KeyError, TypeError, ValueError) as error:
        # No raw provider payload or token-bearing CLI stderr.
        parser.exit(1, f"Observation/approval incomplete: {error}\n")


if __name__ == "__main__":
    main()

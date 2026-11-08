#!/usr/bin/env python3
"""Report checks and reviews for the current PR head; optionally approve
workflows.
"""

import argparse
import json
import re
import subprocess
from urllib.parse import urlencode


OBJECT_ID_PATTERN = r"[0-9a-fA-F]{40}|[0-9a-fA-F]{64}"


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
    """Limit pagination and report incomplete results, including Actions'
    1,000-run cap.
    """
    items = []
    for page in range(1, max_pages + 1):
        payload = api(path + ("&" if "?" in path else "?") + urlencode({"per_page": 100, "page": page}))
        rows = payload.get(key) if key and isinstance(payload, dict) else payload if not key else None
        if not isinstance(rows, list) or any(not isinstance(row, dict) for row in rows):
            raise ObservationError("Unexpected paginated response shape")
        items.extend(rows)
        if len(rows) < 100:
            # If the reported total exceeds the rows returned, the results are
            # incomplete.
            total = payload.get("total_count", len(items)) if key else len(items)
            return items, type(total) is int and 0 <= total <= len(items)
    return items, False


def identity(pr):
    try:
        pair = pr["head"]["sha"], pr["base"]["sha"]
    except (KeyError, TypeError) as error:
        raise ObservationError("Pull request lacks head/base identity") from error
    if any(not isinstance(oid, str) or not re.fullmatch(OBJECT_ID_PATTERN, oid) for oid in pair):
        raise ObservationError("Pull request has unusable head/base identity")
    return pair


def linked_to_pr(bindings, number, expected):
    if not isinstance(bindings, list) or any(not isinstance(item, dict) for item in bindings):
        raise ObservationError("Unexpected workflow pull-request bindings")
    return any(item.get("number") == number and identity(item) == expected for item in bindings)


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
    app_id = app.get("id") if app else None
    # An unknown provider does not establish a shared check identity.
    return ("check", row["id"]) if app_id is None else ("app", app_id, row.get("name"))


def workflow_key(row, number, expected):
    if row.get("event") == "pull_request":
        scope = ("pr", number, *expected)
    elif row.get("head_branch") is not None:
        scope = ("branch", row["head_branch"])
    else:
        # Without a branch identity, keep each workflow run independently.
        scope = ("run", row["id"])
    return row.get("workflow_id"), row.get("event"), scope


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


def review_key(row):
    # A deleted account has no user identity. Keep each such review separately.
    user = row["user"]
    return ("review", row["id"]) if user is None else ("user", user["login"])


def latest(rows, group):
    result = {}
    for row in rows:
        key = group(row)
        if key not in result or row["id"] > result[key]["id"]:
            result[key] = row
    return list(result.values())


def select_workflow_runs(runs, number, expected, runs_complete):
    """Choose runs for this PR/base; mark runs without a PR link incomplete."""
    target_runs = []
    for row in runs:
        if row.get("event") == "pull_request":
            bindings = row.get("pull_requests")
            if not isinstance(bindings, list) or not bindings:
                runs_complete = False
                continue
            if not linked_to_pr(bindings, number, expected):
                continue
        target_runs.append(row)
    return target_runs, runs_complete


def approve_observed_workflows(prefix, number, expected, current_runs):
    """Send workflow approvals and report earlier approvals if a later step
    fails.

    The caller checks whether the run information is complete. Read the PR
    and each run again just before approval. These reads do not prevent
    concurrent changes.
    """
    pr_path = f"{prefix}/pulls/{number}"
    head, _ = expected
    approved = []
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
            if not isinstance(run, dict):
                raise ObservationError("Unexpected workflow run shape")
            linked = linked_to_pr(run.get("pull_requests", []), number, expected)
            if run.get("head_sha") != head or run.get("event") != "pull_request" or workflow_state(run) != "approval_required" or not linked:
                raise ObservationError(f"Workflow identity/state changed or is not bound to this PR/base; stopped after approvals {approved}")
            # The PR may still change between this final read and GitHub
            # applying the approval request. These reads do not prevent
            # concurrent changes.
            try:
                api(f"{prefix}/actions/runs/{run_id}/approve", "POST")
            except ObservationError as error:
                raise ObservationError(f"Approval of run {run_id} has uncertain outcome; prior completed approvals {approved}: {error}") from error
            approved.append(run_id)
        consistent = identity(api(pr_path)) == expected
    except (ObservationError, KeyError, TypeError, ValueError) as error:
        raise ObservationError(f"Approval incomplete; completed approvals {approved}: {error}") from error
    return approved, consistent


def observe(repo, number, approve=False, expected=None):
    prefix = f"repos/{repo}"
    pr_path = f"{prefix}/pulls/{number}"
    before = api(pr_path)
    head, base = identity(before)
    if expected is not None and (head, base) != expected:
        raise ObservationError("Pull request differs from the expected head/base; no approval sent")
    checks, checks_complete = pages(f"{prefix}/commits/{head}/check-runs?filter=all", "check_runs")
    # The check-run endpoint omits suites beyond its most recent 1,000 even
    # after every returned page. Read the suite count after those pages so an
    # endpoint cap cannot silently turn partial coverage into success.
    suites = api(f"{prefix}/commits/{head}/check-suites?per_page=1")
    suite_count = suites.get("total_count") if isinstance(suites, dict) else None
    if type(suite_count) is not int or suite_count < 0:
        raise ObservationError("Unexpected check-suite count")
    checks_complete = checks_complete and suite_count <= 1000
    runs, runs_complete = pages(f"{prefix}/actions/runs?head_sha={head}", "workflow_runs", max_pages=10)
    reviews, reviews_complete = pages(f"{pr_path}/reviews")
    checks_complete = checks_complete and all(row.get("head_sha") == head for row in checks)
    runs_complete = runs_complete and all(row.get("head_sha") == head for row in runs)
    checks = [row for row in checks if row.get("head_sha") == head]
    runs = [row for row in runs if row.get("head_sha") == head]
    # Different PRs may share a head SHA while using different bases. Keep every
    # run in the report, but choose this PR/base before selecting its newest run.
    target_runs, runs_complete = select_workflow_runs(runs, number, (head, base), runs_complete)
    current_checks = latest(checks, check_key)
    current_runs = latest(target_runs, lambda row: workflow_key(row, number, (head, base)))
    effective_reviews = latest([row for row in reviews if row.get("state") in {"APPROVED", "CHANGES_REQUESTED", "DISMISSED"}], review_key)
    after = api(pr_path)
    consistent = identity(after) == (head, base)
    approved = []
    if approve:
        if expected is None:
            raise ObservationError("Approval requires expected head and base commits")
        if not consistent or not runs_complete:
            raise ObservationError("Changed or incomplete workflow observation; no approval sent")
        approved, consistent = approve_observed_workflows(prefix, number, expected, current_runs)

    return {
        "repository": repo, "pull_request": number, "head": head, "base": base,
        "observation_consistent": consistent,
        "checks_state": aggregate(current_checks, checks_complete) if consistent else "unknown",
        "workflows_state": aggregate(current_runs, runs_complete, workflow_state) if consistent else "unknown",
        "complete": {"checks": checks_complete, "workflows": runs_complete, "reviews": reviews_complete},
        "checks": checks, "workflow_runs": runs,
        "reviews": [{"login": None if row["user"] is None else row["user"]["login"], "state": row["state"], "commit": row.get("commit_id"), "current_head": consistent and row.get("commit_id") == head} for row in effective_reviews],
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
    if expected and any(not re.fullmatch(OBJECT_ID_PATTERN, oid) for oid in expected):
        parser.error("Expected commits must be full hexadecimal object IDs")
    if expected:
        expected = tuple(oid.lower() for oid in expected)
    if args.approve_workflows and expected is None:
        parser.error("--approve-workflows requires --expect-head and --expect-base")
    try:
        print(json.dumps(observe(args.repo, args.number, args.approve_workflows, expected), indent=2))
    except (ObservationError, KeyError, TypeError, ValueError) as error:
        # Leave out raw GitHub replies and CLI stderr, which may contain tokens.
        parser.exit(1, f"Observation/approval incomplete: {error}\n")


if __name__ == "__main__":
    main()

#!/usr/bin/env bash

set -euo pipefail

python3 -c '
import json
import re
import sys

PREFIX = "<!-- gherrit-meta: "
SUFFIX = " -->"
ID = re.compile(r"^G[a-z2-7]{32}$")


def fail(message: str) -> None:
    print(message, file=sys.stderr)
    raise SystemExit(1)


body = sys.stdin.read().rstrip()
start = body.rfind(PREFIX)
if start < 0:
    fail("Could not find terminal GHerrit metadata in the PR body.")

candidate = body[start:]
if not candidate.endswith(SUFFIX) or start + len(candidate) != len(body):
    fail("GHerrit metadata comment is not terminal.")

payload = candidate[len(PREFIX):-len(SUFFIX)]
try:
    metadata = json.loads(payload)
except json.JSONDecodeError as primary:
    if not payload.endswith("\""):
        fail(f"GHerrit metadata is not valid JSON: {primary}")
    try:
        metadata = json.loads(payload[:-1])
    except json.JSONDecodeError:
        fail(f"GHerrit metadata is not valid JSON: {primary}")

if not isinstance(metadata, dict) or set(metadata) != {"id", "parent", "child"}:
    fail("GHerrit metadata must contain exactly id, parent, and child.")

for field in ("id", "parent", "child"):
    value = metadata[field]
    if field == "id":
        valid_type = isinstance(value, str)
    else:
        valid_type = value is None or isinstance(value, str)
    if not valid_type:
        fail(f"GHerrit metadata field {field} has the wrong type.")
    if isinstance(value, str) and not ID.fullmatch(value):
        fail(f"GHerrit metadata field {field} is not a canonical GHerrit ID.")

if metadata["parent"] == metadata["id"]:
    fail("GHerrit metadata names its own ID as its parent.")
if metadata["child"] == metadata["id"]:
    fail("GHerrit metadata names its own ID as its child.")

print(json.dumps(metadata, sort_keys=True, separators=(",", ":")))
'

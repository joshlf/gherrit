#!/usr/bin/env bash

set -euo pipefail

cd "$(dirname "$0")/.."

parser=ci/extract_stack_child.sh

assert_child() {
  local expected=$1
  local body=$2
  local actual
  actual=$(printf '%s\n' "$body" | bash "$parser")
  if [[ $actual != "$expected" ]]; then
    echo "expected child '$expected', got '$actual'" >&2
    exit 1
  fi
}

assert_rejected() {
  local body=$1
  if printf '%s\n' "$body" | bash "$parser" >/dev/null 2>&1; then
    echo "expected malformed metadata to be rejected" >&2
    exit 1
  fi
}

assert_child Gchild '<!-- gherrit-meta: {"id":"Gid","parent":null,"child":"Gchild"} -->'
assert_child Gchild '<!-- gherrit-meta: {"id": "Gid", "parent": null, "child": "Gchild"}" -->'
assert_child '' '<!-- gherrit-meta: {"id":"Gid","parent":null,"child":null} -->'

assert_child Gactual 'A commit-body example:
<!-- gherrit-meta: {"id":"Gfake","parent":null,"child":"Gwrong"} -->

<!-- gherrit-meta: {"id":"Gid","parent":null,"child":"Gactual"} -->'

assert_rejected 'No metadata here.'
assert_rejected '<!-- gherrit-meta: {"id":"Gid","parent":null} -->'
assert_rejected '<!-- gherrit-meta: not-json -->'
assert_rejected '<!-- gherrit-meta: {"child":"Gone"} {"child":"Gtwo"} -->'

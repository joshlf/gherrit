#!/usr/bin/env bash

set -euo pipefail

cd "$(dirname "$0")/.."
selector=ci/select_cascade_child.sh

assert_selected() {
  local expected=$1
  local child=$2
  local parent=$3
  local json=$4
  local actual
  actual=$(printf '%s\n' "$json" | bash "$selector" "$child" "$parent")
  if [[ $actual != "$expected" ]]; then
    echo "expected PR '$expected', got '$actual'" >&2
    exit 1
  fi
}

assert_rejected() {
  local child=$1
  local parent=$2
  local json=$3
  if printf '%s\n' "$json" | bash "$selector" "$child" "$parent" >/dev/null 2>&1; then
    echo "expected child selection to fail" >&2
    exit 1
  fi
}

assert_selected 42 Gchild Gparent \
  '[{"number":42,"headRefName":"Gchild","baseRefName":"Gparent"}]'
assert_rejected Gchild Gparent '[]'
assert_rejected Gchild Gparent \
  '[{"number":42,"headRefName":"Gchild","baseRefName":"Gparent"},{"number":43,"headRefName":"Gchild","baseRefName":"Gparent"}]'
assert_rejected Gchild Gparent \
  '[{"number":42,"headRefName":"Gother","baseRefName":"Gparent"}]'
assert_rejected Gchild Gparent \
  '[{"number":42,"headRefName":"Gchild","baseRefName":"Gold-parent"}]'
assert_rejected Gchild Gparent '{}'

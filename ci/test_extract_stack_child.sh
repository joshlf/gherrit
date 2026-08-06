#!/usr/bin/env bash

set -euo pipefail

cd "$(dirname "$0")/.."

child_parser=ci/extract_stack_child.sh
metadata_parser=ci/extract_stack_metadata.sh
id=Gaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa
child=Gbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb

assert_child() {
  local expected=$1
  local body=$2
  local actual
  actual=$(printf '%s' "$body" | bash "$child_parser")
  if [[ $actual != "$expected" ]]; then
    echo "expected child '$expected', got '$actual'" >&2
    exit 1
  fi
}

assert_metadata() {
  local expected=$1
  local body=$2
  local actual
  actual=$(printf '%s' "$body" | bash "$metadata_parser")
  if [[ $actual != "$expected" ]]; then
    echo "expected metadata '$expected', got '$actual'" >&2
    exit 1
  fi
}

assert_rejected() {
  local body=$1
  if printf '%s' "$body" | bash "$metadata_parser" >/dev/null 2>&1; then
    echo "expected malformed metadata to be rejected" >&2
    exit 1
  fi
}

canonical="<!-- gherrit-meta: {\"id\":\"$id\",\"parent\":null,\"child\":\"$child\"} -->"
assert_child "$child" "$canonical"
assert_metadata "{\"child\":\"$child\",\"id\":\"$id\",\"parent\":null}" "$canonical"
assert_child "$child" "<!-- gherrit-meta: {\"id\": \"$id\", \"parent\": null, \"child\": \"$child\"}\" -->"
assert_child '' "<!-- gherrit-meta: {\"id\":\"$id\",\"parent\":null,\"child\":null} -->"

assert_child "$child" "A commit-body example:
<!-- gherrit-meta: {\"id\":\"$child\",\"parent\":null,\"child\":null} -->

$canonical"

assert_rejected 'No metadata here.'
assert_rejected "<!-- gherrit-meta: {\"id\":\"$id\",\"parent\":null,\"child\":null} -->
text"
assert_rejected "<!-- gherrit-meta: {\"id\":\"$id\",\"parent\":null} -->"
assert_rejected '<!-- gherrit-meta: not-json -->'
assert_rejected "<!-- gherrit-meta: {\"id\":\"$id\",\"parent\":null,\"child\":null,\"extra\":true} -->"
assert_rejected '<!-- gherrit-meta: {"id":"main","parent":null,"child":null} -->'
assert_rejected "<!-- gherrit-meta: {\"id\":\"$id\",\"parent\":\"$id\",\"child\":null} -->"
assert_rejected "<!-- gherrit-meta: {\"id\":\"$id\",\"parent\":null,\"child\":\"$id\"} -->"

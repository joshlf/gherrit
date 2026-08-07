#!/usr/bin/env bash

set -euo pipefail

cd "$(dirname "$0")/.."
selector=ci/select_cascade_child.sh
child=Gaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa
parent=Gbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb
other=Gcccccccccccccccccccccccccccccccc
default=main
repository=R_target_repository

metadata() {
  local id=$1
  local parent_id=$2
  if [[ -z $parent_id ]]; then
    printf '<!-- gherrit-meta: {"id":"%s","parent":null,"child":null} -->' "$id"
  else
    printf '<!-- gherrit-meta: {"id":"%s","parent":"%s","child":null} -->' "$id" "$parent_id"
  fi
}

candidate() {
  local number=$1
  local head=$2
  local base=$3
  local body=$4
  local head_repository=$5
  local base_repository=$6
  local cross=$7
  local oid=${8:-0123456789012345678901234567890123456789}
  local blocker=${9:-none}
  local base_oid=${10:-1111111111111111111111111111111111111111}
  jq -cn \
    --argjson number "$number" \
    --arg nodeId "PR_$number" \
    --arg head "$head" \
    --arg base "$base" \
    --arg body "$body" \
    --arg headRepository "$head_repository" \
    --arg baseRepository "$base_repository" \
    --argjson cross "$cross" \
    --arg oid "$oid" \
    --arg base_oid "$base_oid" \
    --arg blocker "$blocker" '
      {
        id: $nodeId,
        number: $number,
        headRefName: $head,
        headRefOid: $oid,
        baseRefName: $base,
        baseRefOid: $base_oid,
        body: $body,
        isCrossRepository: $cross,
        isInMergeQueue: ($blocker == "queue"),
        autoMergeRequest: (if $blocker == "auto" then {enabledAt: "now"} else null end),
        stackEntry: (if $blocker == "stack" then {id: "stack"} else null end),
        headRepository: (if $headRepository == "null" then null else {id: $headRepository} end),
        baseRepository: (if $baseRepository == "null" then null else {id: $baseRepository} end)
      }
    '
}

array() { jq -cs '.'; }

assert_selected() {
  local expected_number=$1 expected_mode=$2 parent_exists=$3 json=$4
  local selected
  selected=$(printf '%s\n' "$json" | bash "$selector" \
    "$child" "$parent" "$default" "$repository" "$parent_exists")
  if [[ $(jq -er '.number' <<<"$selected") != "$expected_number" ||
        $(jq -er '.mode' <<<"$selected") != "$expected_mode" ||
        $(jq -er '.baseRefOid' <<<"$selected") != 1111111111111111111111111111111111111111 ]]; then
    echo "unexpected child selection: $selected" >&2
    exit 1
  fi
}

assert_rejected() {
  local parent_exists=$1 json=$2
  if printf '%s\n' "$json" | bash "$selector" \
    "$child" "$parent" "$default" "$repository" "$parent_exists" \
    >/dev/null 2>&1; then
    echo "expected child selection to fail" >&2
    exit 1
  fi
}

canonical_body=$(metadata "$child" "$parent")
promoted_body=$(metadata "$child" '')
canonical_parent=$(candidate 42 "$child" "$parent" "$canonical_body" "$repository" "$repository" false)
canonical_retargeted=$(candidate 42 "$child" "$default" "$canonical_body" "$repository" "$repository" false)
canonical_promoted=$(candidate 42 "$child" "$default" "$promoted_body" "$repository" "$repository" false)
fork=$(candidate 43 "$child" "$parent" "$canonical_body" R_fork "$repository" true)

assert_selected 42 parent true "$(printf '%s\n' "$canonical_parent" | array)"
assert_selected 42 automatically-retargeted false "$(printf '%s\n' "$canonical_retargeted" | array)"
assert_selected 42 default-based-recovery true "$(printf '%s\n' "$canonical_retargeted" | array)"
assert_selected 42 default-based-recovery true "$(printf '%s\n' "$canonical_promoted" | array)"
assert_rejected true '[]'
assert_rejected true "$(printf '%s\n' "$fork" | array)"
assert_selected 42 parent true "$(printf '%s\n%s\n' "$fork" "$canonical_parent" | array)"
assert_rejected true "$(printf '%s\n%s\n' "$canonical_parent" "$(candidate 44 "$child" "$parent" "$canonical_body" "$repository" "$repository" false)" | array)"
assert_rejected true "$(printf '%s\n' "$(candidate 42 "$other" "$parent" "$canonical_body" "$repository" "$repository" false)" | array)"
assert_rejected true "$(printf '%s\n' "$(candidate 42 "$child" "$other" "$canonical_body" "$repository" "$repository" false)" | array)"
assert_rejected true "$(printf '%s\n' "$(candidate 42 "$child" "$parent" 'not metadata' "$repository" "$repository" false)" | array)"
assert_rejected true "$(printf '%s\n' "$(candidate 42 "$child" "$parent" "$(metadata "$other" "$parent")" "$repository" "$repository" false)" | array)"
assert_rejected true "$(printf '%s\n' "$(candidate 42 "$child" "$parent" "$(metadata "$child" "$other")" "$repository" "$repository" false)" | array)"
assert_rejected true "$(printf '%s\n' "$(candidate 42 "$child" "$parent" "$canonical_body" null "$repository" false)" | array)"
for blocker in queue auto stack; do
  assert_rejected true "$(printf '%s\n' "$(candidate 42 "$child" "$parent" "$canonical_body" "$repository" "$repository" false '' "$blocker")" | array)"
done

missing_base_oid=$(jq 'del(.baseRefOid)' <<<"$canonical_parent")
assert_rejected true "$(printf '%s\n' "$missing_base_oid" | array)"
assert_rejected true '{}'

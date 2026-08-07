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


legacy_g=G0123456789abcdef0123456789abcdef01234567
legacy_i=I0123456789abcdef0123456789abcdef01234567
assert_metadata "{\"child\":null,\"id\":\"$legacy_g\",\"parent\":null}" \
  "<!-- gherrit-meta: {\"id\":\"$legacy_g\",\"parent\":null,\"child\":null} -->"
assert_metadata "{\"child\":null,\"id\":\"$legacy_i\",\"parent\":null}" \
  "<!-- gherrit-meta: {\"id\":\"$legacy_i\",\"parent\":null,\"child\":null} -->"

assert_trailer() {
  local expected=$1 message=$2 actual
  actual=$(printf '%s' "$message" | bash ci/extract_gherrit_id.sh)
  if [[ $actual != "$expected" ]]; then
    echo "expected trailer ID '$expected', got '$actual'" >&2
    exit 1
  fi
}
assert_trailer "$id" "Subject"$'\n\n'"GhErRiT-Pr-Id: $id"
assert_trailer "$legacy_g" "Subject"$'\n\n'"GHERRIT-PR-ID: $legacy_g"
assert_trailer "$legacy_i" "Subject"$'\n\n'"gherrit-pr-id: $legacy_i"

oid1=1111111111111111111111111111111111111111
oid2=2222222222222222222222222222222222222222
version_lines=$(printf '%s\trefs/tags/gherrit/%s/v1\n%s\trefs/tags/gherrit/%s/v2\n' \
  "$oid1" "$id" "$oid2" "$id")
version_state=$(printf '%s' "$version_lines" | python3 ci/gherrit_protocol.py \
  version-state --id "$id" --expected-head "$oid2")
[[ $(jq -er '.latest' <<<"$version_state") == 2 ]]
[[ $(printf '%s' "$version_lines" | python3 ci/gherrit_protocol.py \
  authenticate-version --id "$id" --expected-target "$oid1") == 1 ]]
if printf '%s\trefs/tags/gherrit/%s/v01\n' "$oid1" "$id" |
  python3 ci/gherrit_protocol.py version-state --id "$id" --expected-head "$oid1" \
    >/dev/null 2>&1; then
  echo "expected noncanonical cascade version tag to be rejected" >&2
  exit 1
fi

repository=R_repo
consumer_body="<!-- gherrit-meta: {\"id\":\"$child\",\"parent\":\"$id\",\"child\":null} -->"
consumer_json=$(
  jq -cn \
    --arg repository "$repository" \
    --arg parent "$id" \
    --arg child "$child" \
    --arg body "$consumer_body" '
      {data:{repository:{id:$repository,pullRequests:{totalCount:1,nodes:[{
        id:"PR_child",number:2,body:$body,
        headRefName:$child,headRefOid:"0123456789abcdef",
        baseRefName:$parent,baseRefOid:"fedcba9876543210",
        isCrossRepository:false,isInMergeQueue:false,
        autoMergeRequest:null,stackEntry:null,
        headRepository:{id:$repository},baseRepository:{id:$repository}
      }]}}}}
    '
)
selected=$(
  printf '%s' "$consumer_json" |
    python3 ci/gherrit_protocol.py base-consumer \
      --child "$id" --grandchild "$child" --repository "$repository"
)
[[ $(jq -er '.headRefName' <<<"$selected") == "$child" ]]
[[ $(jq -er '.baseRefOid' <<<"$selected") == fedcba9876543210 ]]

assert_consumer_rejected() {
  local payload=$1
  shift
  if printf '%s' "$payload" |
    python3 ci/gherrit_protocol.py base-consumer \
      --child "$id" "$@" --repository "$repository" >/dev/null 2>&1; then
    echo "expected unsafe base consumer to be rejected" >&2
    exit 1
  fi
}
assert_consumer_rejected \
  "$(jq '.data.repository.pullRequests.nodes[0].isCrossRepository = true' <<<"$consumer_json")" \
  --grandchild "$child"
assert_consumer_rejected \
  "$(jq '.data.repository.pullRequests.nodes[0].headRepository = null' <<<"$consumer_json")" \
  --grandchild "$child"
assert_consumer_rejected \
  "$(jq 'del(.data.repository.pullRequests.nodes[0].baseRefOid)' <<<"$consumer_json")" \
  --grandchild "$child"
assert_consumer_rejected \
  "$(jq '.data.repository.pullRequests.totalCount = 2 | .data.repository.pullRequests.nodes += [.data.repository.pullRequests.nodes[0]]' <<<"$consumer_json")" \
  --grandchild "$child"
assert_consumer_rejected "$consumer_json"

# Promotion replaces the complete authenticated generated tail, including stale
# navigation, and keeps high-version history bounded rather than quadratic.
parent=Gcccccccccccccccccccccccccccccccc
full_body=$(cat <<EOF
<!-- WARNING: This PR description is automatically generated by GHerrit. Any manual edits will be overwritten on the next push. -->

User-authored body.

---

- 👉 stale navigation

<details>
<summary><strong>⬇️ Download this PR</strong></summary>

######

</details>

*Stacked PRs enabled by [GHerrit](https://github.com/joshlf/gherrit).*

<!-- WARNING: GHerrit relies on the following metadata to work properly. DO NOT EDIT OR REMOVE. --><!-- gherrit-meta: {"id":"$id","parent":"$parent","child":"$child"} -->
EOF
)
promoted=$(printf '%s' "$full_body" | python3 ci/gherrit_protocol.py promote-body \
  --id "$id" --parent "$parent" --latest 500 \
  --repo-url https://github.com/owner/repository --base main)
grep -Fq "[Next](https://github.com/owner/repository/compare/$id..$child)" <<<"$promoted"
grep -Fq "[Current](https://github.com/owner/repository/compare/main..$id)" <<<"$promoted"
if grep -Fq 'stale navigation' <<<"$promoted"; then
  echo "promotion retained stale navigation" >&2
  exit 1
fi
grep -Fq '**Latest Update:** v500' <<<"$promoted"
grep -Fq 'Showing the latest 32 of 500 patch versions' <<<"$promoted"
(( $(printf '%s' "$promoted" | wc -c) < 60000 ))

# An unrepresentable provisional body is rejected before the caller can publish
# a branch/version transaction.
oversized=$(python3 - <<'PYBODY'
print('x' * 60001, end='')
PYBODY
)
oversized_body=$(cat <<EOF
<!-- WARNING: This PR description is automatically generated by GHerrit. Any manual edits will be overwritten on the next push. -->

$oversized

---

*GHerrit is completing the initial projection for this PR.*

<!-- WARNING: GHerrit relies on the following metadata to work properly. DO NOT EDIT OR REMOVE. --><!-- gherrit-meta: {"id":"$id","parent":"$parent","child":null} -->
EOF
)
if printf '%s' "$oversized_body" | python3 ci/gherrit_protocol.py promote-body \
  --id "$id" --parent "$parent" --latest 2 \
  --repo-url https://github.com/owner/repository --base main \
  >/dev/null 2>&1; then
  echo "oversized promoted body unexpectedly succeeded" >&2
  exit 1
fi

echo "stack metadata and cascade protocol parser tests passed"

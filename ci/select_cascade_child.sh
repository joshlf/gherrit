#!/usr/bin/env bash

set -euo pipefail

child_id=${1:?usage: select_cascade_child.sh CHILD_ID MERGED_HEAD DEFAULT_BRANCH REPOSITORY_ID PARENT_REF_EXISTS}
merged_head=${2:?usage: select_cascade_child.sh CHILD_ID MERGED_HEAD DEFAULT_BRANCH REPOSITORY_ID PARENT_REF_EXISTS}
default_branch=${3:?usage: select_cascade_child.sh CHILD_ID MERGED_HEAD DEFAULT_BRANCH REPOSITORY_ID PARENT_REF_EXISTS}
repository_id=${4:?usage: select_cascade_child.sh CHILD_ID MERGED_HEAD DEFAULT_BRANCH REPOSITORY_ID PARENT_REF_EXISTS}
parent_ref_exists=${5:?usage: select_cascade_child.sh CHILD_ID MERGED_HEAD DEFAULT_BRANCH REPOSITORY_ID PARENT_REF_EXISTS}
script_dir=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)

if [[ $parent_ref_exists != true && $parent_ref_exists != false ]]; then
  echo "PARENT_REF_EXISTS must be true or false." >&2
  exit 1
fi

payload=$(cat)
if ! jq -e 'type == "array"' >/dev/null <<<"$payload"; then
  echo "Child PR lookup must return an array." >&2
  exit 1
fi

mapfile -t candidates < <(
  jq -c \
    --arg child "$child_id" \
    --arg repository "$repository_id" '
      .[] |
      select(
        .headRefName == $child and
        .isCrossRepository == false and
        .headRepository.id == $repository and
        .baseRepository.id == $repository
      )
    ' <<<"$payload"
)

if [[ ${#candidates[@]} -eq 0 ]]; then
  echo "No same-repository open child PR exists for '$child_id'." >&2
  exit 1
elif [[ ${#candidates[@]} -gt 1 ]]; then
  echo "Multiple same-repository open child PRs exist for '$child_id'." >&2
  exit 1
fi

candidate=${candidates[0]}
number=$(jq -er '.number | numbers' <<<"$candidate")
base=$(jq -er '.baseRefName | strings' <<<"$candidate")
head_oid=$(jq -er '.headRefOid | strings | select(length > 0)' <<<"$candidate")
body=$(jq -er '.body | strings' <<<"$candidate")

if ! metadata=$(printf '%s' "$body" | bash "$script_dir/extract_stack_metadata.sh"); then
  echo "Child PR #$number has invalid terminal GHerrit metadata." >&2
  exit 1
fi
if [[ $(jq -er '.id' <<<"$metadata") != "$child_id" ]]; then
  echo "Child PR #$number metadata ID does not match '$child_id'." >&2
  exit 1
fi
if [[ $(jq -er '.parent // ""' <<<"$metadata") != "$merged_head" ]]; then
  echo "Child PR #$number metadata does not point back to '$merged_head'." >&2
  exit 1
fi

if [[ $base == "$merged_head" ]]; then
  mode=parent
elif [[ $base == "$default_branch" && $parent_ref_exists == false ]]; then
  mode=automatically-retargeted
else
  echo "Child PR #$number targets '$base', which is not a legitimate pre-cascade base." >&2
  exit 1
fi

jq -cn \
  --argjson number "$number" \
  --arg baseRefName "$base" \
  --arg headRefOid "$head_oid" \
  --arg mode "$mode" \
  '{number: $number, baseRefName: $baseRefName, headRefOid: $headRefOid, mode: $mode}'

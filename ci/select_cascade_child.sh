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
node_id=$(jq -er '.id | strings | select(length > 0)' <<<"$candidate")
base=$(jq -er '.baseRefName | strings' <<<"$candidate")
head_oid=$(jq -er '.headRefOid | strings | select(length > 0)' <<<"$candidate")
body=$(jq -er '.body | strings' <<<"$candidate")

if jq -e '.isInMergeQueue == true' >/dev/null <<<"$candidate"; then
  echo "Child PR #$number is in the merge queue; refusing to mutate it." >&2
  exit 1
fi
if jq -e '.autoMergeRequest != null' >/dev/null <<<"$candidate"; then
  echo "Child PR #$number has auto-merge enabled; refusing to mutate it." >&2
  exit 1
fi
if jq -e '.stackEntry != null' >/dev/null <<<"$candidate"; then
  echo "Child PR #$number belongs to a native GitHub stack; refusing to mutate it." >&2
  exit 1
fi

if ! metadata=$(printf '%s' "$body" | bash "$script_dir/extract_stack_metadata.sh"); then
  echo "Child PR #$number has invalid terminal GHerrit metadata." >&2
  exit 1
fi
if [[ $(jq -er '.id' <<<"$metadata") != "$child_id" ]]; then
  echo "Child PR #$number metadata ID does not match '$child_id'." >&2
  exit 1
fi
metadata_parent=$(jq -r '.parent // ""' <<<"$metadata")
if [[ $metadata_parent != "$merged_head" && -n $metadata_parent ]]; then
  echo "Child PR #$number metadata does not point back to '$merged_head' or describe an already-promoted child." >&2
  exit 1
fi

if [[ $base == "$merged_head" ]]; then
  mode=parent
elif [[ $base == "$default_branch" ]]; then
  if [[ $parent_ref_exists == false ]]; then
    mode=automatically-retargeted
  else
    mode=default-based-recovery
  fi
else
  echo "Child PR #$number targets '$base', which is not a legitimate cascade or recovery base." >&2
  exit 1
fi

jq -cn \
  --argjson number "$number" \
  --arg nodeId "$node_id" \
  --arg baseRefName "$base" \
  --arg headRefOid "$head_oid" \
  --arg mode "$mode" \
  --arg body "$body" \
  --arg metadataParent "$metadata_parent" \
  '{number: $number, nodeId: $nodeId, baseRefName: $baseRefName, headRefOid: $headRefOid, mode: $mode, body: $body, metadataParent: $metadataParent}'

#!/usr/bin/env bash

set -euo pipefail

child_id=${1:?usage: select_cascade_child.sh CHILD_ID MERGED_HEAD}
merged_head=${2:?usage: select_cascade_child.sh CHILD_ID MERGED_HEAD}

if ! selection=$(
  jq -cer --arg child "$child_id" --arg parent "$merged_head" '
    if type != "array" then
      error("child PR lookup must return an array")
    elif length == 0 then
      error("no open child PR exists")
    elif length > 1 then
      error("multiple open child PRs exist")
    elif .[0].headRefName != $child then
      error("child PR head does not match GHerrit metadata")
    elif .[0].baseRefName != $parent then
      error("child PR no longer targets the merged parent")
    else
      .[0].number
    end
  '
); then
  echo "Could not select a unique current child PR for '$child_id' beneath '$merged_head'." >&2
  exit 1
fi

printf '%s\n' "$selection"

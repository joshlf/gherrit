#!/usr/bin/env bash

set -euo pipefail

action_path=${ACTION_PATH:-$(cd "$(dirname "$0")/.." && pwd)}
merged_pr_body=${MERGED_PR_BODY-}

child_id=$(
  printf '%s\n' "$merged_pr_body" |
    bash "$action_path/ci/extract_stack_child.sh"
)

if [[ -z $child_id ]]; then
  echo "Merged PR has no child defined in metadata. Reached top of stack."
  exit 0
fi

echo "Merged PR indicates next child is ID: $child_id"

# GHerrit branches use the stable ID as their exact name.
child_pr=$(gh pr list --head "$child_id" --json number --jq '.[0].number')
if [[ -z $child_pr ]]; then
  echo "Error: Metadata says child is $child_id, but no open PR exists for branch '$child_id'."
  echo "The chain might be broken or the child was deleted."
  exit 1
fi

echo "Identified Child PR: #$child_pr"

git config user.name "github-actions[bot]"
git config user.email "41898282+github-actions[bot]@users.noreply.github.com"

gh pr checkout "$child_pr"
gh pr edit "$child_pr" --base main

if ! git rebase origin/main; then
  echo "::error::Rebase conflict for PR #$child_pr. Manual intervention required."
  exit 1
fi

git push --force-with-lease

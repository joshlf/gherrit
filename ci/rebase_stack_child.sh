#!/usr/bin/env bash

set -euo pipefail

# This action is a separate writer from the pre-push publisher. A merged PR's
# saved metadata names the next logical change; it is not a fresh observation
# of that child's current Git history or PR state.
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

# Bind lookup and subsequent PR operations to the workflow repository.
repository=${GITHUB_REPOSITORY:?GITHUB_REPOSITORY is required for stack advancement}
child_prs=$(gh pr list --repo "$repository" --head "$child_id" --limit 100 \
  --json number,headRefName,isCrossRepository,headRepository)
child_pr=$(jq -er --arg repository "$repository" --arg child "$child_id" '
  if type != "array" or length != 1 then
    error("Expected exactly one open child PR")
  elif (.[0] | .isCrossRepository == false
      and .headRefName == $child
      and ((.headRepository.nameWithOwner | ascii_downcase) == ($repository | ascii_downcase))
      and (.number | type == "number") and .number > 0
      and .number == (.number | floor)) then
    .[0].number
  else error("Child PR does not match the repository and managed head")
  end
' <<<"$child_prs")

echo "Identified Child PR: #$child_pr"

git config user.name "github-actions[bot]"
git config user.email "41898282+github-actions[bot]@users.noreply.github.com"

# These are separate operations, not a transaction. In particular, the base edit
# remains applied if the subsequent local rebase fails. Only the immediate child
# is advanced here; this does not rebase the remainder of the stack in one run.
gh pr checkout "$child_pr" --repo "$repository"
gh pr edit "$child_pr" --repo "$repository" --base main

if ! git rebase origin/main; then
  echo "::error::Rebase conflict for PR #$child_pr. Manual intervention required."
  exit 1
fi

# The tracking ref supplies this push's default lease. It does not fence the PR
# base edit above or prevent a competing writer from changing other stack refs.
git push --force-with-lease

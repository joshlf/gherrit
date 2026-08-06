#!/usr/bin/env bash

set -euo pipefail

action_path=${ACTION_PATH:-$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)}

required=(
  MERGED_PR_NUMBER MERGED_PR_HEAD MERGED_PR_BASE
  MERGED_HEAD_REPOSITORY_ID MERGED_BASE_REPOSITORY_ID
  DEFAULT_BRANCH TARGET_REPOSITORY TARGET_REPOSITORY_ID
)
for name in "${required[@]}"; do
  if [[ -z ${!name:-} ]]; then
    echo "::error::Missing required pull_request event field '$name'."
    exit 1
  fi
done

if [[ "$MERGED_PR_BASE" != "$DEFAULT_BRANCH" ]]; then
  echo "PR #$MERGED_PR_NUMBER targeted '$MERGED_PR_BASE', not the default branch '$DEFAULT_BRANCH'; refusing to cascade a non-root merge."
  exit 0
fi
if [[ "$MERGED_HEAD_REPOSITORY_ID" != "$TARGET_REPOSITORY_ID" ||
      "$MERGED_BASE_REPOSITORY_ID" != "$TARGET_REPOSITORY_ID" ]]; then
  echo "::error::Merged PR #$MERGED_PR_NUMBER is not wholly owned by repository '$TARGET_REPOSITORY'."
  exit 1
fi

EVENT_METADATA=$(
  printf '%s' "$MERGED_PR_BODY" |
    bash "$action_path/ci/extract_stack_metadata.sh"
)
if [[ $(jq -er '.id' <<<"$EVENT_METADATA") != "$MERGED_PR_HEAD" ]]; then
  echo "::error::Merged PR metadata ID does not match head branch '$MERGED_PR_HEAD'."
  exit 1
fi
if [[ $(jq -er '.parent // ""' <<<"$EVENT_METADATA") != "" ]]; then
  echo "::error::Merged root PR metadata unexpectedly names a parent."
  exit 1
fi
CHILD_ID=$(jq -er '.child // ""' <<<"$EVENT_METADATA")

OWNER=${TARGET_REPOSITORY%%/*}
REPOSITORY_NAME=${TARGET_REPOSITORY#*/}
if [[ -z $OWNER || -z $REPOSITORY_NAME || $OWNER == "$TARGET_REPOSITORY" ]]; then
  echo "::error::Invalid target repository '$TARGET_REPOSITORY'."
  exit 1
fi

MERGED_LOOKUP=$(
  gh api graphql \
    -f owner="$OWNER" \
    -f name="$REPOSITORY_NAME" \
    -F number="$MERGED_PR_NUMBER" \
    -f query='query($owner: String!, $name: String!, $number: Int!) {
      repository(owner: $owner, name: $name) {
        id
        defaultBranchRef { name }
        mergedPr: pullRequest(number: $number) {
          number
          body
          merged
          headRefName
          baseRefName
          isCrossRepository
          headRepository { id }
          baseRepository { id }
        }
      }
    }'
)

if [[ $(jq -er '.data.repository.id' <<<"$MERGED_LOOKUP") != "$TARGET_REPOSITORY_ID" ]]; then
  echo "::error::GraphQL repository identity does not match the workflow event."
  exit 1
fi
if [[ $(jq -er '.data.repository.defaultBranchRef.name' <<<"$MERGED_LOOKUP") != "$DEFAULT_BRANCH" ]]; then
  echo "::error::GraphQL default branch does not match the workflow event."
  exit 1
fi
if ! jq -e \
  --argjson number "$MERGED_PR_NUMBER" \
  --arg head "$MERGED_PR_HEAD" \
  --arg base "$DEFAULT_BRANCH" \
  --arg repository "$TARGET_REPOSITORY_ID" '
    .data.repository.mergedPr |
    .number == $number and
    .merged == true and
    .headRefName == $head and
    .baseRefName == $base and
    .isCrossRepository == false and
    .headRepository.id == $repository and
    .baseRepository.id == $repository
  ' >/dev/null <<<"$MERGED_LOOKUP"; then
  echo "::error::Merged PR #$MERGED_PR_NUMBER failed repository or topology identity checks."
  exit 1
fi

CURRENT_METADATA=$(
  jq -er '.data.repository.mergedPr.body' <<<"$MERGED_LOOKUP" |
    bash "$action_path/ci/extract_stack_metadata.sh"
)
if [[ "$CURRENT_METADATA" != "$EVENT_METADATA" ]]; then
  echo "::error::Merged PR metadata changed between the merge event and cascade lookup."
  exit 1
fi

if [[ -z "$CHILD_ID" ]]; then
  echo "Merged PR has no child defined in metadata. Reached top of stack."
  exit 0
fi
echo "Merged PR indicates next child is ID: $CHILD_ID"

CHILD_LOOKUP=$(
  gh api graphql \
    -f owner="$OWNER" \
    -f name="$REPOSITORY_NAME" \
    -f head="$CHILD_ID" \
    -f parentRef="refs/heads/$MERGED_PR_HEAD" \
    -f query='query($owner: String!, $name: String!, $head: String!, $parentRef: String!) {
      repository(owner: $owner, name: $name) {
        id
        parentRef: ref(qualifiedName: $parentRef) { id }
        pullRequests(headRefName: $head, first: 100, states: [OPEN]) {
          totalCount
          nodes {
            number
            body
            headRefName
            headRefOid
            baseRefName
            isCrossRepository
            headRepository { id }
            baseRepository { id }
          }
        }
      }
    }'
)

if [[ $(jq -er '.data.repository.id' <<<"$CHILD_LOOKUP") != "$TARGET_REPOSITORY_ID" ]]; then
  echo "::error::Child lookup repository identity changed."
  exit 1
fi
CHILD_TOTAL=$(jq -er '.data.repository.pullRequests.totalCount' <<<"$CHILD_LOOKUP")
CHILD_RETURNED=$(jq -er '.data.repository.pullRequests.nodes | length' <<<"$CHILD_LOOKUP")
if (( CHILD_TOTAL != CHILD_RETURNED )); then
  echo "::error::Child lookup returned only $CHILD_RETURNED of $CHILD_TOTAL candidates; refusing an unpaginated selection."
  exit 1
fi
PARENT_REF_EXISTS=$(jq -r '.data.repository.parentRef != null' <<<"$CHILD_LOOKUP")
CHILD_SELECTION=$(
  jq -c '.data.repository.pullRequests.nodes' <<<"$CHILD_LOOKUP" |
    bash "$action_path/ci/select_cascade_child.sh" \
      "$CHILD_ID" "$MERGED_PR_HEAD" "$DEFAULT_BRANCH" \
      "$TARGET_REPOSITORY_ID" "$PARENT_REF_EXISTS"
)

CHILD_PR=$(jq -er '.number' <<<"$CHILD_SELECTION")
CHILD_BASE=$(jq -er '.baseRefName' <<<"$CHILD_SELECTION")
CHILD_HEAD_OID=$(jq -er '.headRefOid' <<<"$CHILD_SELECTION")
CHILD_MODE=$(jq -er '.mode' <<<"$CHILD_SELECTION")
echo "Identified current child PR: #$CHILD_PR ($CHILD_MODE)"

git config user.name "github-actions[bot]"
git config user.email "41898282+github-actions[bot]@users.noreply.github.com"

gh pr checkout "$CHILD_PR" --repo "$TARGET_REPOSITORY"
if [[ $(git rev-parse HEAD) != "$CHILD_HEAD_OID" ]]; then
  echo "::error::Checked-out child head does not match the GraphQL-observed OID."
  exit 1
fi

mapfile -t ACTUAL_IDS < <(
  git log -1 --format=%B |
    git interpret-trailers --parse |
    sed -n 's/^gherrit-pr-id: //p'
)
if [[ ${#ACTUAL_IDS[@]} -ne 1 || ${ACTUAL_IDS[0]} != "$CHILD_ID" ]]; then
  echo "::error::Child PR #$CHILD_PR head does not carry exactly the expected GHerrit ID '$CHILD_ID'."
  exit 1
fi

if [[ "$CHILD_BASE" != "$DEFAULT_BRANCH" ]]; then
  gh pr edit "$CHILD_PR" --repo "$TARGET_REPOSITORY" --base "$DEFAULT_BRANCH"
fi

git fetch origin "$DEFAULT_BRANCH"
if ! git rebase "origin/$DEFAULT_BRANCH"; then
  echo "::error::Rebase conflict for PR #$CHILD_PR. Manual intervention required."
  exit 1
fi

git push --force-with-lease origin "HEAD:refs/heads/$CHILD_ID"

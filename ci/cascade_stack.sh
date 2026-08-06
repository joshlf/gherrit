#!/usr/bin/env bash

set -euo pipefail

action_path=${ACTION_PATH:-$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)}

required=(
  MERGED_PR_NUMBER MERGED_PR_HEAD MERGED_PR_BASE MERGED_PR_BODY
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
          headRefOid
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
    (.headRefOid | type == "string" and length > 0) and
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

MERGED_HEAD_OID=$(jq -er '.data.repository.mergedPr.headRefOid' <<<"$MERGED_LOOKUP")
MERGED_VERSION_LINES=$(git ls-remote --tags origin "refs/tags/gherrit/$MERGED_PR_HEAD/v*")
printf '%s\n' "$MERGED_VERSION_LINES" |
  python3 "$action_path/ci/gherrit_protocol.py" version-state \
    --id "$MERGED_PR_HEAD" --expected-head "$MERGED_HEAD_OID" >/dev/null

if [[ -z "$CHILD_ID" ]]; then
  echo "Merged PR has no child defined in metadata. Reached top of stack."
  exit 0
fi
echo "Merged PR indicates next child is ID: $CHILD_ID"

query_child() {
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
            id
            number
            body
            headRefName
            headRefOid
            baseRefName
            isCrossRepository
            isInMergeQueue
            autoMergeRequest { enabledAt }
            stackEntry { id }
            headRepository { id }
            baseRepository { id }
          }
        }
      }
    }'
}

select_child() {
  local lookup=$1
  if [[ $(jq -er '.data.repository.id' <<<"$lookup") != "$TARGET_REPOSITORY_ID" ]]; then
    echo "::error::Child lookup repository identity changed." >&2
    return 1
  fi
  local total returned parent_exists
  total=$(jq -er '.data.repository.pullRequests.totalCount' <<<"$lookup")
  returned=$(jq -er '.data.repository.pullRequests.nodes | length' <<<"$lookup")
  if (( total != returned )); then
    echo "::error::Child lookup returned only $returned of $total candidates; refusing an unpaginated selection." >&2
    return 1
  fi
  parent_exists=$(jq -r '.data.repository.parentRef != null' <<<"$lookup")
  jq -c '.data.repository.pullRequests.nodes' <<<"$lookup" |
    bash "$action_path/ci/select_cascade_child.sh" \
      "$CHILD_ID" "$MERGED_PR_HEAD" "$DEFAULT_BRANCH" \
      "$TARGET_REPOSITORY_ID" "$parent_exists"
}

CHILD_LOOKUP=$(query_child)
CHILD_SELECTION=$(select_child "$CHILD_LOOKUP")
CHILD_PR=$(jq -er '.number' <<<"$CHILD_SELECTION")
CHILD_NODE_ID=$(jq -er '.nodeId' <<<"$CHILD_SELECTION")
CHILD_BASE=$(jq -er '.baseRefName' <<<"$CHILD_SELECTION")
CHILD_HEAD_OID=$(jq -er '.headRefOid' <<<"$CHILD_SELECTION")
CHILD_MODE=$(jq -er '.mode' <<<"$CHILD_SELECTION")
CHILD_BODY=$(jq -er '.body' <<<"$CHILD_SELECTION")
echo "Identified current child PR: #$CHILD_PR ($CHILD_MODE)"

git config user.name "github-actions[bot]"
git config user.email "41898282+github-actions[bot]@users.noreply.github.com"

gh pr checkout "$CHILD_PR" --repo "$TARGET_REPOSITORY"
if [[ $(git rev-parse HEAD) != "$CHILD_HEAD_OID" ]]; then
  echo "::error::Checked-out child head does not match the GraphQL-observed OID."
  exit 1
fi
ACTUAL_ID=$(git log -1 --format=%B | bash "$action_path/ci/extract_gherrit_id.sh")
if [[ "$ACTUAL_ID" != "$CHILD_ID" ]]; then
  echo "::error::Child PR #$CHILD_PR head does not carry exactly the expected GHerrit ID '$CHILD_ID'."
  exit 1
fi

VERSION_LINES=$(git ls-remote --tags origin "refs/tags/gherrit/$CHILD_ID/v*")
VERSION_STATE=$(
  printf '%s\n' "$VERSION_LINES" |
    python3 "$action_path/ci/gherrit_protocol.py" version-state \
      --id "$CHILD_ID" --expected-head "$CHILD_HEAD_OID"
)
LATEST_VERSION=$(jq -er '.latest' <<<"$VERSION_STATE")
NEXT_VERSION=$(jq -er '.next' <<<"$VERSION_STATE")

git fetch --no-tags origin "$DEFAULT_BRANCH"
DEFAULT_OID=$(git rev-parse "origin/$DEFAULT_BRANCH")

head_is_canonical_child() {
  [[ $(git rev-parse HEAD) != "$DEFAULT_OID" ]] || return 1
  [[ $(git rev-list --parents -n 1 HEAD | wc -w) -eq 2 ]] || return 1
  [[ $(git rev-parse HEAD^) == "$DEFAULT_OID" ]] || return 1
  [[ $(git rev-list --count "origin/$DEFAULT_BRANCH..HEAD") -eq 1 ]] || return 1
  [[ $(git log -1 --format=%B | bash "$action_path/ci/extract_gherrit_id.sh") == "$CHILD_ID" ]]
}

if ! head_is_canonical_child; then
  if [[ $(git rev-list --parents -n 1 HEAD | wc -w) -ne 2 ]]; then
    echo "::error::Child PR head is not a linear single-parent commit."
    exit 1
  fi
  OLD_PARENT=$(git rev-parse HEAD^)
  OLD_PARENT_ID=$(git log -1 --format=%B "$OLD_PARENT" | bash "$action_path/ci/extract_gherrit_id.sh")
  if [[ "$OLD_PARENT_ID" != "$MERGED_PR_HEAD" ]]; then
    echo "::error::Child PR head is not directly based on an authenticated commit for merged parent '$MERGED_PR_HEAD'."
    exit 1
  fi
  printf '%s\n' "$MERGED_VERSION_LINES" |
    python3 "$action_path/ci/gherrit_protocol.py" authenticate-version \
      --id "$MERGED_PR_HEAD" --expected-target "$OLD_PARENT" >/dev/null
  if ! git rebase --reapply-cherry-picks --keep-empty --empty=keep \
    --onto "origin/$DEFAULT_BRANCH" "$OLD_PARENT"; then
    echo "::error::Rebase conflict for PR #$CHILD_PR. The PR base was not changed; manual intervention is required."
    exit 1
  fi
fi

if ! head_is_canonical_child; then
  echo "::error::Rebased child is not exactly one authenticated GHerrit commit above '$DEFAULT_BRANCH'."
  exit 1
fi
NEW_HEAD_OID=$(git rev-parse HEAD)
if [[ "$NEW_HEAD_OID" == "$DEFAULT_OID" ]]; then
  echo "::error::Rebase would make the child head equal the default branch; refusing an indirectly merged PR state."
  exit 1
fi
POST_REBASE_ID=$(git log -1 --format=%B | bash "$action_path/ci/extract_gherrit_id.sh")
if [[ "$POST_REBASE_ID" != "$CHILD_ID" ]]; then
  echo "::error::Rebased child does not carry exactly the expected GHerrit ID '$CHILD_ID'."
  exit 1
fi

if [[ "$NEW_HEAD_OID" != "$CHILD_HEAD_OID" ]]; then
  VERSION_REF="refs/tags/gherrit/$CHILD_ID/v$NEXT_VERSION"
  if ! git push --atomic --no-verify \
    --force-with-lease="refs/heads/$CHILD_ID:$CHILD_HEAD_OID" \
    --force-with-lease="$VERSION_REF:" \
    origin \
    "$NEW_HEAD_OID:refs/heads/$CHILD_ID" \
    "$NEW_HEAD_OID:$VERSION_REF"; then
    OBSERVED_REFS=$(git ls-remote origin "refs/heads/$CHILD_ID" "$VERSION_REF")
    OBSERVED_HEAD=$(awk -v ref="refs/heads/$CHILD_ID" '$2 == ref { print $1 }' <<<"$OBSERVED_REFS")
    OBSERVED_VERSION=$(awk -v ref="$VERSION_REF" '$2 == ref { print $1 }' <<<"$OBSERVED_REFS")
    if [[ "$OBSERVED_HEAD" == "$NEW_HEAD_OID" && "$OBSERVED_VERSION" == "$NEW_HEAD_OID" ]]; then
      echo "Git push reported failure, but the exact atomic branch-and-version result is present; continuing."
    elif [[ "$OBSERVED_HEAD" == "$CHILD_HEAD_OID" && -z "$OBSERVED_VERSION" ]]; then
      echo "::error::Cascade Git publication was not applied. The PR base remains unchanged and the operation can be retried."
      exit 1
    else
      echo "::error::Cascade Git publication has an inconsistent partial or externally modified result."
      exit 1
    fi
  fi
  LATEST_VERSION=$NEXT_VERSION
else
  echo "Child head is already the canonical commit above the updated default branch; no new patch version is required."
fi

DESIRED_BODY=$(
  printf '%s' "$CHILD_BODY" |
    python3 "$action_path/ci/gherrit_protocol.py" promote-body \
      --id "$CHILD_ID" \
      --parent "$MERGED_PR_HEAD" \
      --latest "$LATEST_VERSION" \
      --repo-url "https://github.com/$TARGET_REPOSITORY" \
      --base "$DEFAULT_BRANCH"
)

needs_projection_update=true
if [[ "$CHILD_BASE" == "$DEFAULT_BRANCH" && "$CHILD_BODY" == "$DESIRED_BODY" ]]; then
  needs_projection_update=false
fi

update_failed=false
if [[ $needs_projection_update == true ]]; then
  if ! gh api graphql \
    -f prId="$CHILD_NODE_ID" \
    -f base="$DEFAULT_BRANCH" \
    -f body="$DESIRED_BODY" \
    -f query='mutation($prId: ID!, $base: String!, $body: String!) {
      updatePullRequest(input: {pullRequestId: $prId, baseRefName: $base, body: $body}) {
        pullRequest { id number }
      }
    }' >/dev/null; then
    update_failed=true
  fi
fi

FINAL_LOOKUP=$(query_child)
FINAL_SELECTION=$(select_child "$FINAL_LOOKUP")
FINAL_NODE_ID=$(jq -er '.nodeId' <<<"$FINAL_SELECTION")
FINAL_BASE=$(jq -er '.baseRefName' <<<"$FINAL_SELECTION")
FINAL_HEAD=$(jq -er '.headRefOid' <<<"$FINAL_SELECTION")
FINAL_BODY=$(jq -er '.body' <<<"$FINAL_SELECTION")
if [[ "$FINAL_NODE_ID" == "$CHILD_NODE_ID" &&
      "$FINAL_BASE" == "$DEFAULT_BRANCH" &&
      "$FINAL_HEAD" == "$NEW_HEAD_OID" &&
      "$FINAL_BODY" == "$DESIRED_BODY" ]]; then
  if [[ $update_failed == true ]]; then
    echo "GitHub reported a PR projection failure, but the exact promoted state is present; continuing."
  fi
  echo "Cascaded PR #$CHILD_PR onto '$DEFAULT_BRANCH' at version v$LATEST_VERSION."
  exit 0
fi

if [[ $update_failed == true ]]; then
  echo "::error::PR projection update failed and re-observation did not find the exact promoted state. Git publication is durable; rerun the cascade to finish projection."
else
  echo "::error::PR projection update returned success, but re-observation found a different state."
fi
exit 1

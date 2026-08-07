#!/usr/bin/env bash

set -euo pipefail

action_path=${ACTION_PATH:-$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)}

required=(
  MERGED_PR_NUMBER MERGED_PR_HEAD MERGED_PR_BASE MERGED_PR_BODY
  MERGED_HEAD_REPOSITORY_ID MERGED_BASE_REPOSITORY_ID
  DEFAULT_BRANCH GITHUB_SERVER_URL TARGET_REPOSITORY TARGET_REPOSITORY_ID
)
for name in "${required[@]}"; do
  if [[ -z ${!name:-} ]]; then
    echo "::error::Missing required pull_request event field '$name'."
    exit 1
  fi
done

validate_repository_dag_authority() {
  local shallow common_dir grafts

  shallow=$(git rev-parse --is-shallow-repository 2>/dev/null) || {
    echo "::error::Could not determine whether the cascade checkout has complete history."
    exit 1
  }
  if [[ "$shallow" != false ]]; then
    echo "::error::GHerrit cannot prove cascade reachability from a shallow repository. Check out complete history before running the cascade action."
    exit 1
  fi

  if [[ ${GIT_REPLACE_REF_BASE+x} == x ]]; then
    echo "::error::GHerrit cannot prove cascade reachability while GIT_REPLACE_REF_BASE is set."
    exit 1
  fi
  if [[ -n $(git for-each-ref --format='%(refname)' refs/replace/) ]]; then
    echo "::error::GHerrit cannot prove cascade reachability while local replace refs exist."
    exit 1
  fi

  common_dir=$(git rev-parse --path-format=absolute --git-common-dir 2>/dev/null) || {
    echo "::error::Could not resolve the checkout's common Git directory."
    exit 1
  }
  grafts=$common_dir/info/grafts
  if [[ -s "$grafts" ]] && grep -q '[^[:space:]]' "$grafts"; then
    echo "::error::GHerrit cannot prove cascade reachability while the common .git/info/grafts file is nonempty."
    exit 1
  fi
}


resolve_publication_remote() {
  local remote_name=origin remote_target kind lookup
  local fetch_owner fetch_repository push_owner push_repository
  local -a fetch_urls push_urls

  mapfile -t fetch_urls < <(git remote get-url --all "$remote_name")
  mapfile -t push_urls < <(git remote get-url --push --all "$remote_name")
  if [[ ${#fetch_urls[@]} -ne 1 ]]; then
    echo "::error::Remote '$remote_name' has ${#fetch_urls[@]} effective fetch URLs; GHerrit requires exactly one authenticated repository."
    exit 1
  fi
  if [[ ${#push_urls[@]} -ne 1 ]]; then
    echo "::error::Remote '$remote_name' has ${#push_urls[@]} effective push URLs; GHerrit refuses multi-destination publication."
    exit 1
  fi

  remote_target=$(
    python3 "$action_path/ci/gherrit_protocol.py" remote-target \
      --fetch "${fetch_urls[0]}" \
      --push "${push_urls[0]}" \
      --server-url "$GITHUB_SERVER_URL" \
      --workdir "$PWD"
  )
  PUBLISH_URL=$(jq -er '.gitUrl' <<<"$remote_target")
  kind=$(jq -er '.kind' <<<"$remote_target")
  if [[ $kind == local ]]; then
    return 0
  fi
  if [[ $kind != network ]]; then
    echo "::error::Unsupported authenticated Git remote kind '$kind'."
    exit 1
  fi

  fetch_owner=$(jq -er '.fetchOwner' <<<"$remote_target")
  fetch_repository=$(jq -er '.fetchRepository' <<<"$remote_target")
  push_owner=$(jq -er '.pushOwner' <<<"$remote_target")
  push_repository=$(jq -er '.pushRepository' <<<"$remote_target")
  lookup=$(
    gh api graphql \
      -f fetchOwner="$fetch_owner" \
      -f fetchName="$fetch_repository" \
      -f pushOwner="$push_owner" \
      -f pushName="$push_repository" \
      -f query='query($fetchOwner: String!, $fetchName: String!, $pushOwner: String!, $pushName: String!) {
        fetchRepository: repository(owner: $fetchOwner, name: $fetchName) { id nameWithOwner }
        pushRepository: repository(owner: $pushOwner, name: $pushName) { id nameWithOwner }
      }'
  )
  if [[ $(jq -er '.data.fetchRepository.id' <<<"$lookup") != "$TARGET_REPOSITORY_ID" ||
        $(jq -er '.data.pushRepository.id' <<<"$lookup") != "$TARGET_REPOSITORY_ID" ]]; then
    echo "::error::The effective fetch and push URLs do not both identify repository '$TARGET_REPOSITORY' by immutable GitHub node ID."
    exit 1
  fi
}

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

PUBLISH_URL=
resolve_publication_remote

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
MERGED_VERSION_LINES=$(git ls-remote --tags "$PUBLISH_URL" "refs/tags/gherrit/$MERGED_PR_HEAD/v*")
printf '%s\n' "$MERGED_VERSION_LINES" |
  python3 "$action_path/ci/gherrit_protocol.py" version-state \
    --id "$MERGED_PR_HEAD" --expected-head "$MERGED_HEAD_OID" >/dev/null

if [[ -z "$CHILD_ID" ]]; then
  echo "Merged PR has no child defined in metadata. Reached top of stack."
  exit 0
fi

validate_repository_dag_authority
export GIT_NO_REPLACE_OBJECTS=1

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
            baseRefOid
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
CHILD_BASE_OID=$(jq -er '.baseRefOid' <<<"$CHILD_SELECTION")
CHILD_HEAD_OID=$(jq -er '.headRefOid' <<<"$CHILD_SELECTION")
CHILD_MODE=$(jq -er '.mode' <<<"$CHILD_SELECTION")
CHILD_BODY=$(jq -er '.body' <<<"$CHILD_SELECTION")
CHILD_METADATA=$(
  printf '%s' "$CHILD_BODY" |
    bash "$action_path/ci/extract_stack_metadata.sh"
)
GRANDCHILD_ID=$(jq -er '.child // ""' <<<"$CHILD_METADATA")
echo "Identified current child PR: #$CHILD_PR ($CHILD_MODE)"

git config user.name "github-actions[bot]"
git config user.email "41898282+github-actions[bot]@users.noreply.github.com"

git fetch --no-tags "$PUBLISH_URL" "refs/heads/$CHILD_ID" >/dev/null
git checkout -B "$CHILD_ID" FETCH_HEAD >/dev/null
if [[ $(git rev-parse HEAD) != "$CHILD_HEAD_OID" ]]; then
  echo "::error::Checked-out child head does not match the GraphQL-observed OID."
  exit 1
fi
ACTUAL_ID=$(git log -1 --format=%B | bash "$action_path/ci/extract_gherrit_id.sh")
if [[ "$ACTUAL_ID" != "$CHILD_ID" ]]; then
  echo "::error::Child PR #$CHILD_PR head does not carry exactly the expected GHerrit ID '$CHILD_ID'."
  exit 1
fi

VERSION_LINES=$(git ls-remote --tags "$PUBLISH_URL" "refs/tags/gherrit/$CHILD_ID/v*")
VERSION_STATE=$(
  printf '%s\n' "$VERSION_LINES" |
    python3 "$action_path/ci/gherrit_protocol.py" version-state \
      --id "$CHILD_ID" --expected-head "$CHILD_HEAD_OID"
)
LATEST_VERSION=$(jq -er '.latest' <<<"$VERSION_STATE")
NEXT_VERSION=$(jq -er '.next' <<<"$VERSION_STATE")

DEFAULT_TRACKING_REF="refs/remotes/gherrit-publish/$DEFAULT_BRANCH"
git fetch --no-tags "$PUBLISH_URL" "refs/heads/$DEFAULT_BRANCH:$DEFAULT_TRACKING_REF" >/dev/null
DEFAULT_OID=$(git rev-parse "$DEFAULT_TRACKING_REF")

head_is_reachable_from_base() {
  local head=$1 base=$2 context=$3 status
  if git --no-replace-objects merge-base --is-ancestor "$head" "$base"; then
    echo "::error::$context: PR head $head is reachable from associated base $base."
    exit 1
  else
    status=$?
    if (( status != 1 )); then
      echo "::error::$context: could not determine reachability between PR head $head and associated base $base."
      exit 1
    fi
  fi
}

authenticate_child_base() {
  local base_name=$1 base_oid=$2 context=$3 status
  if [[ $base_name == "$MERGED_PR_HEAD" ]]; then
    printf '%s\n' "$MERGED_VERSION_LINES" |
      python3 "$action_path/ci/gherrit_protocol.py" authenticate-version \
        --id "$MERGED_PR_HEAD" --expected-target "$base_oid" >/dev/null
  elif [[ $base_name == "$DEFAULT_BRANCH" ]]; then
    if git --no-replace-objects merge-base --is-ancestor "$base_oid" "$DEFAULT_OID"; then
      :
    else
      status=$?
      if (( status == 1 )); then
        echo "::error::$context: child PR associated base $base_oid is not in the authenticated '$DEFAULT_BRANCH' history."
      else
        echo "::error::$context: could not authenticate child PR associated default-base OID $base_oid."
      fi
      exit 1
    fi
  else
    echo "::error::$context: child PR targets unexpected base '$base_name'."
    exit 1
  fi
}

authenticate_child_base "$CHILD_BASE" "$CHILD_BASE_OID" "Before cascade publication"
head_is_reachable_from_base "$CHILD_HEAD_OID" "$CHILD_BASE_OID" "Before cascade publication"

head_is_canonical_child() {
  [[ $(git rev-parse HEAD) != "$DEFAULT_OID" ]] || return 1
  [[ $(git rev-list --parents -n 1 HEAD | wc -w) -eq 2 ]] || return 1
  [[ $(git rev-parse HEAD^) == "$DEFAULT_OID" ]] || return 1
  [[ $(git rev-list --count "$DEFAULT_TRACKING_REF..HEAD") -eq 1 ]] || return 1
  [[ $(git log -1 --format=%B | bash "$action_path/ci/extract_gherrit_id.sh") == "$CHILD_ID" ]]
}

if [[ $(git rev-list --parents -n 1 HEAD | wc -w) -ne 2 ]]; then
  echo "::error::Child PR head is not a linear single-parent commit."
  exit 1
fi
OLD_PARENT=$(git rev-parse HEAD^)

if git --no-replace-objects merge-base --is-ancestor HEAD "$DEFAULT_OID"; then
  echo "::error::Authenticated child head is already reachable from '$DEFAULT_BRANCH'; refusing to cascade an already-landed PR."
  exit 1
else
  status=$?
  if (( status != 1 )); then
    echo "::error::Could not determine whether the child head is already reachable from '$DEFAULT_BRANCH'."
    exit 1
  fi
fi

if [[ "$OLD_PARENT" == "$DEFAULT_OID" ]]; then
  : # The child is already canonical on the current default tip.
elif git --no-replace-objects merge-base --is-ancestor "$OLD_PARENT" "$DEFAULT_OID"; then
  echo "Authenticated child is based on an older default-branch commit; rebasing it onto the current '$DEFAULT_BRANCH'."
  if ! git --no-replace-objects rebase --reapply-cherry-picks --keep-empty --empty=keep \
    --onto "$DEFAULT_TRACKING_REF" "$OLD_PARENT"; then
    echo "::error::Rebase conflict for PR #$CHILD_PR. The PR base was not changed; manual intervention is required."
    exit 1
  fi
else
  status=$?
  if (( status != 1 )); then
    echo "::error::Could not determine whether the child parent belongs to '$DEFAULT_BRANCH'."
    exit 1
  fi
  OLD_PARENT_ID=$(git log -1 --format=%B "$OLD_PARENT" | bash "$action_path/ci/extract_gherrit_id.sh")
  if [[ "$OLD_PARENT_ID" != "$MERGED_PR_HEAD" ]]; then
    echo "::error::Child PR head is not directly based on an authenticated commit for merged parent '$MERGED_PR_HEAD'."
    exit 1
  fi
  printf '%s\n' "$MERGED_VERSION_LINES" |
    python3 "$action_path/ci/gherrit_protocol.py" authenticate-version \
      --id "$MERGED_PR_HEAD" --expected-target "$OLD_PARENT" >/dev/null
  if ! git --no-replace-objects rebase --reapply-cherry-picks --keep-empty --empty=keep \
    --onto "$DEFAULT_TRACKING_REF" "$OLD_PARENT"; then
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
head_is_reachable_from_base "$NEW_HEAD_OID" "$CHILD_BASE_OID" "Planned cascade publication"

PROSPECTIVE_VERSION=$LATEST_VERSION
if [[ "$NEW_HEAD_OID" != "$CHILD_HEAD_OID" ]]; then
  PROSPECTIVE_VERSION=$NEXT_VERSION
fi

# Body promotion is fallible, so compute and validate it before publishing the
# durable branch-and-version transaction. The exact result is reused for the
# later projection mutation.
DESIRED_BODY=$(
  printf '%s' "$CHILD_BODY" |
    python3 "$action_path/ci/gherrit_protocol.py" promote-body \
      --id "$CHILD_ID" \
      --parent "$MERGED_PR_HEAD" \
      --latest "$PROSPECTIVE_VERSION" \
      --repo-url "https://github.com/$TARGET_REPOSITORY" \
      --base "$DEFAULT_BRANCH"
)

verify_child_association() {
  local phase=$1 expected_head=$2 expected_base=$3 expected_base_oid=$4
  local lookup selection node head base base_oid body
  lookup=$(query_child)
  selection=$(select_child "$lookup")
  node=$(jq -er '.nodeId' <<<"$selection")
  head=$(jq -er '.headRefOid' <<<"$selection")
  base=$(jq -er '.baseRefName' <<<"$selection")
  base_oid=$(jq -er '.baseRefOid' <<<"$selection")
  body=$(jq -er '.body' <<<"$selection")
  if [[ "$node" != "$CHILD_NODE_ID" || "$head" != "$expected_head" ||
        "$base" != "$expected_base" || "$base_oid" != "$expected_base_oid" ||
        "$body" != "$CHILD_BODY" ]]; then
    echo "::error::$phase: child PR identity, head, base association, or body changed unexpectedly."
    exit 1
  fi
  authenticate_child_base "$base" "$base_oid" "$phase"
  head_is_reachable_from_base "$head" "$base_oid" "$phase"
}

query_base_consumers() {
  gh api graphql \
    -f owner="$OWNER" \
    -f name="$REPOSITORY_NAME" \
    -f base="$CHILD_ID" \
    -f query='query($owner: String!, $name: String!, $base: String!) {
      repository(owner: $owner, name: $name) {
        id
        pullRequests(baseRefName: $base, first: 100, states: [OPEN]) {
          totalCount
          nodes {
            id
            number
            body
            headRefName
            headRefOid
            baseRefName
            baseRefOid
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

BASE_CONSUMER_NODE_ID=
BASE_CONSUMER_HEAD_OID=
BASE_CONSUMER_BASE_OID=

validate_base_consumers() {
  local phase=$1
  local lookup selection grandchild_node grandchild_head associated_base_oid
  local remote_head child_version_lines expected_child_head version_lines
  local grandchild_parent base_oid status
  lookup=$(query_base_consumers)
  selection=$(
    printf '%s' "$lookup" |
      python3 "$action_path/ci/gherrit_protocol.py" base-consumer \
        --child "$CHILD_ID" \
        --grandchild "$GRANDCHILD_ID" \
        --repository "$TARGET_REPOSITORY_ID"
  )
  if [[ "$selection" == null ]]; then
    if [[ "$phase" == after && -n "$BASE_CONSUMER_NODE_ID" ]]; then
      echo "::error::Authenticated grandchild PR disappeared after child-branch publication."
      exit 1
    fi
    return 0
  fi

  grandchild_node=$(jq -er '.nodeId' <<<"$selection")
  grandchild_head=$(jq -er '.headRefOid' <<<"$selection")
  associated_base_oid=$(jq -er '.baseRefOid' <<<"$selection")
  expected_child_head=$CHILD_HEAD_OID
  [[ "$phase" == after ]] && expected_child_head=$NEW_HEAD_OID
  child_version_lines=$(git ls-remote --tags "$PUBLISH_URL" "refs/tags/gherrit/$CHILD_ID/v*")
  printf '%s\n' "$child_version_lines" |
    python3 "$action_path/ci/gherrit_protocol.py" version-state \
      --id "$CHILD_ID" --expected-head "$expected_child_head" >/dev/null
  printf '%s\n' "$child_version_lines" |
    python3 "$action_path/ci/gherrit_protocol.py" authenticate-version \
      --id "$CHILD_ID" --expected-target "$associated_base_oid" >/dev/null
  if [[ "$phase" == after ]]; then
    if [[ -z "$BASE_CONSUMER_NODE_ID" ||
          "$grandchild_node" != "$BASE_CONSUMER_NODE_ID" ||
          "$grandchild_head" != "$BASE_CONSUMER_HEAD_OID" ]]; then
      echo "::error::Authenticated grandchild PR identity or head changed during child-branch publication."
      exit 1
    fi
    if [[ "$associated_base_oid" != "$BASE_CONSUMER_BASE_OID" &&
          "$associated_base_oid" != "$CHILD_HEAD_OID" &&
          "$associated_base_oid" != "$NEW_HEAD_OID" ]]; then
      echo "::error::Authenticated grandchild PR changed to an unexpected associated base OID during child-branch publication."
      exit 1
    fi
  else
    BASE_CONSUMER_NODE_ID=$grandchild_node
    BASE_CONSUMER_HEAD_OID=$grandchild_head
    BASE_CONSUMER_BASE_OID=$associated_base_oid
  fi

  remote_head=$(git ls-remote "$PUBLISH_URL" "refs/heads/$GRANDCHILD_ID" |
    awk -v ref="refs/heads/$GRANDCHILD_ID" '$2 == ref { print $1 }')
  if [[ -z "$remote_head" || "$remote_head" != "$grandchild_head" ]]; then
    echo "::error::Authenticated grandchild PR head does not match remote branch '$GRANDCHILD_ID'."
    exit 1
  fi
  git fetch --no-tags "$PUBLISH_URL" "refs/heads/$GRANDCHILD_ID" >/dev/null
  if [[ $(git rev-parse FETCH_HEAD) != "$grandchild_head" ]]; then
    echo "::error::Fetched grandchild branch does not match its GraphQL-observed head."
    exit 1
  fi
  if [[ $(git log -1 --format=%B FETCH_HEAD | bash "$action_path/ci/extract_gherrit_id.sh") != "$GRANDCHILD_ID" ]]; then
    echo "::error::Authenticated grandchild branch does not carry its expected GHerrit ID."
    exit 1
  fi
  if [[ $(git rev-list --parents -n 1 FETCH_HEAD | wc -w) -ne 2 ]]; then
    echo "::error::Authenticated grandchild head is not exactly one linear commit."
    exit 1
  fi
  grandchild_parent=$(git rev-parse FETCH_HEAD^)
  printf '%s\n' "$child_version_lines" |
    python3 "$action_path/ci/gherrit_protocol.py" authenticate-version \
      --id "$CHILD_ID" --expected-target "$grandchild_parent" >/dev/null

  version_lines=$(git ls-remote --tags "$PUBLISH_URL" "refs/tags/gherrit/$GRANDCHILD_ID/v*")
  printf '%s\n' "$version_lines" |
    python3 "$action_path/ci/gherrit_protocol.py" version-state \
      --id "$GRANDCHILD_ID" --expected-head "$grandchild_head" >/dev/null

  for base_oid in "$associated_base_oid" "$CHILD_HEAD_OID" "$NEW_HEAD_OID"; do
    if git --no-replace-objects merge-base --is-ancestor "$grandchild_head" "$base_oid"; then
      echo "::error::Grandchild PR head would be reachable from rewritten base branch '$CHILD_ID'; refusing an indirect merge."
      exit 1
    else
      status=$?
      if (( status != 1 )); then
        echo "::error::Could not prove grandchild reachability safety for rewritten branch '$CHILD_ID'."
        exit 1
      fi
    fi
  done
}

validate_base_consumers before

if [[ "$NEW_HEAD_OID" != "$CHILD_HEAD_OID" ]]; then
  VERSION_REF="refs/tags/gherrit/$CHILD_ID/v$NEXT_VERSION"
  if ! git push --atomic --no-verify \
    --force-with-lease="refs/heads/$CHILD_ID:$CHILD_HEAD_OID" \
    --force-with-lease="$VERSION_REF:" \
    "$PUBLISH_URL" \
    "$NEW_HEAD_OID:refs/heads/$CHILD_ID" \
    "$NEW_HEAD_OID:$VERSION_REF"; then
    OBSERVED_REFS=$(git ls-remote "$PUBLISH_URL" "refs/heads/$CHILD_ID" "$VERSION_REF")
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
  LATEST_VERSION=$PROSPECTIVE_VERSION
else
  echo "Child head is already the canonical commit above the updated default branch; no new patch version is required."
fi

verify_child_association "After cascade publication" "$NEW_HEAD_OID" "$CHILD_BASE" "$CHILD_BASE_OID"
validate_base_consumers after

needs_projection_update=true
if [[ "$CHILD_BASE" == "$DEFAULT_BRANCH" && "$CHILD_BODY" == "$DESIRED_BODY" ]]; then
  needs_projection_update=false
fi

update_failed=false
if [[ $needs_projection_update == true ]]; then
  if ! printf '%s' "$DESIRED_BODY" |
    jq -Rs \
      --arg prId "$CHILD_NODE_ID" \
      --arg base "$DEFAULT_BRANCH" \
      --arg query 'mutation($prId: ID!, $base: String!, $body: String!) {
        updatePullRequest(input: {pullRequestId: $prId, baseRefName: $base, body: $body}) {
          pullRequest { id number }
        }
      }' \
      '{query: $query, variables: {prId: $prId, base: $base, body: .}}' |
    gh api graphql --input - >/dev/null; then
    update_failed=true
  fi
fi

FINAL_LOOKUP=$(query_child)
FINAL_SELECTION=$(select_child "$FINAL_LOOKUP")
FINAL_NODE_ID=$(jq -er '.nodeId' <<<"$FINAL_SELECTION")
FINAL_BASE=$(jq -er '.baseRefName' <<<"$FINAL_SELECTION")
FINAL_BASE_OID=$(jq -er '.baseRefOid' <<<"$FINAL_SELECTION")
FINAL_HEAD=$(jq -er '.headRefOid' <<<"$FINAL_SELECTION")
FINAL_BODY=$(jq -er '.body' <<<"$FINAL_SELECTION")
if [[ "$FINAL_NODE_ID" == "$CHILD_NODE_ID" &&
      "$FINAL_BASE" == "$DEFAULT_BRANCH" &&
      "$FINAL_BASE_OID" == "$DEFAULT_OID" &&
      "$FINAL_HEAD" == "$NEW_HEAD_OID" &&
      "$FINAL_BODY" == "$DESIRED_BODY" ]]; then
  head_is_reachable_from_base "$FINAL_HEAD" "$FINAL_BASE_OID" "After PR projection"
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

#!/usr/bin/env bash

set -euo pipefail

repo_root=$(cd "$(dirname "$0")/.." && pwd)
root=$(mktemp -d)
trap 'rm -rf "$root"' EXIT

parent_id=Gbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb
child_id=Gaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa
repository_id=R_target_repository
repository_name=owner/repository

run_automatic_retarget_case() {
  local shape=$1
  local case_dir="$root/$shape"
  local remote="$case_dir/remote.git"
  local work="$case_dir/work"
  local bin="$case_dir/bin"
  local edit_marker="$case_dir/pr-edit-called"

  mkdir -p "$case_dir" "$bin"
  git init --bare --initial-branch=main "$remote" >/dev/null
  git clone "$remote" "$work" >/dev/null 2>&1
  (
    cd "$work"
    git config user.name test
    git config user.email test@example.com

    echo base > base.txt
    git add base.txt
    git commit -m base >/dev/null
    git push -u origin main >/dev/null

    git checkout -b "$parent_id" >/dev/null
    echo parent > parent.txt
    git add parent.txt
    git commit -m parent -m "gherrit-pr-id: $parent_id" >/dev/null
    git push -u origin "$parent_id" >/dev/null

    git checkout -b "$child_id" >/dev/null
    echo child > child.txt
    git add child.txt
    git commit -m child -m "gherrit-pr-id: $child_id" >/dev/null
    git push -u origin "$child_id" >/dev/null
    child_head_oid=$(git rev-parse HEAD)

    git checkout main >/dev/null
    case "$shape" in
      squash)
        git merge --squash "$parent_id" >/dev/null
        git commit -m 'squash parent' >/dev/null
        ;;
      rebase)
        git cherry-pick --no-commit "$parent_id" >/dev/null
        git commit -m 'rebase-merged parent' >/dev/null
        ;;
      *)
        echo "unknown merge shape '$shape'" >&2
        exit 1
        ;;
    esac
    git push origin main >/dev/null
    git push origin --delete "$parent_id" >/dev/null

    merged_body="<!-- gherrit-meta: {\"id\":\"$parent_id\",\"parent\":null,\"child\":\"$child_id\"} -->"
    child_body="<!-- gherrit-meta: {\"id\":\"$child_id\",\"parent\":\"$parent_id\",\"child\":null} -->"

    cat > "$bin/gh" <<'GH'
#!/usr/bin/env bash
set -euo pipefail

if [[ ${1:-} == api && ${2:-} == graphql ]]; then
  joined=$*
  if [[ $joined == *'mergedPr: pullRequest'* ]]; then
    jq -cn \
      --arg repository "$TARGET_REPOSITORY_ID" \
      --arg default "$DEFAULT_BRANCH" \
      --argjson number "$MERGED_PR_NUMBER" \
      --arg body "$MERGED_PR_BODY" \
      --arg head "$MERGED_PR_HEAD" '
        {data: {repository: {
          id: $repository,
          defaultBranchRef: {name: $default},
          mergedPr: {
            number: $number,
            body: $body,
            merged: true,
            headRefName: $head,
            baseRefName: $default,
            isCrossRepository: false,
            headRepository: {id: $repository},
            baseRepository: {id: $repository}
          }
        }}}
      '
  elif [[ $joined == *'parentRef: ref'* ]]; then
    jq -cn \
      --arg repository "$TARGET_REPOSITORY_ID" \
      --argjson number 42 \
      --arg body "$TEST_CHILD_BODY" \
      --arg head "$TEST_CHILD_ID" \
      --arg oid "$TEST_CHILD_HEAD_OID" \
      --arg default "$DEFAULT_BRANCH" '
        {data: {repository: {
          id: $repository,
          parentRef: null,
          pullRequests: {
            totalCount: 1,
            nodes: [{
              number: $number,
              body: $body,
              headRefName: $head,
              headRefOid: $oid,
              baseRefName: $default,
              isCrossRepository: false,
              headRepository: {id: $repository},
              baseRepository: {id: $repository}
            }]
          }
        }}}
      '
  else
    echo "unexpected GraphQL query" >&2
    exit 1
  fi
elif [[ ${1:-} == pr && ${2:-} == checkout ]]; then
  git checkout "$TEST_CHILD_ID" >/dev/null
elif [[ ${1:-} == pr && ${2:-} == edit ]]; then
  touch "$TEST_EDIT_MARKER"
else
  echo "unexpected gh invocation: $*" >&2
  exit 1
fi
GH
    chmod +x "$bin/gh"

    export PATH="$bin:$PATH"
    export ACTION_PATH="$repo_root"
    export GH_TOKEN=test-token
    export MERGED_PR_BODY="$merged_body"
    export MERGED_PR_NUMBER=7
    export MERGED_PR_HEAD="$parent_id"
    export MERGED_PR_BASE=main
    export MERGED_HEAD_REPOSITORY_ID="$repository_id"
    export MERGED_BASE_REPOSITORY_ID="$repository_id"
    export DEFAULT_BRANCH=main
    export TARGET_REPOSITORY="$repository_name"
    export TARGET_REPOSITORY_ID="$repository_id"
    export TEST_CHILD_BODY="$child_body"
    export TEST_CHILD_ID="$child_id"
    export TEST_CHILD_HEAD_OID="$child_head_oid"
    export TEST_EDIT_MARKER="$edit_marker"

    bash "$repo_root/ci/cascade_stack.sh"

    if [[ -e $edit_marker ]]; then
      echo "$shape automatically-retargeted cascade redundantly edited the PR base" >&2
      exit 1
    fi
    git fetch origin "$child_id" >/dev/null
    if [[ $(git rev-list --count origin/main..FETCH_HEAD) != 1 ]]; then
      echo "$shape cascade retained the already-landed parent patch" >&2
      exit 1
    fi
    if [[ $(git diff --name-only origin/main..FETCH_HEAD) != child.txt ]]; then
      echo "$shape cascade produced an unexpected child diff" >&2
      git diff --name-status origin/main..FETCH_HEAD >&2
      exit 1
    fi
  )
}

run_automatic_retarget_case squash
run_automatic_retarget_case rebase

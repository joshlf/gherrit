#!/usr/bin/env bash

set -euo pipefail

expected_base=42e1ff1e5a99f8d7a4a966ca60b9245cbc1609a6
expected_tree=4e163518479a1b4ea06088279517ab1a7fdbb1f9
feature_ref=refs/heads/agent/safe-pr-reorder

if [[ ${GITHUB_HEAD_REF:-} != agent/bootstrap-gherrit-build ||
      ${GITHUB_BASE_REF:-} != agent/safe-pr-reorder ]]; then
  echo "Refusing to publish outside the dedicated transport PR." >&2
  exit 1
fi

observed=$(git ls-remote origin "$feature_ref" | awk -v ref="$feature_ref" '$2 == ref { print $1 }')
if [[ -z $observed ]]; then
  echo "Feature branch does not exist." >&2
  exit 1
fi

git fetch --force origin "$feature_ref:refs/remotes/origin/agent/safe-pr-reorder"
observed_tree=$(git rev-parse "refs/remotes/origin/agent/safe-pr-reorder^{tree}")
if [[ $observed_tree == "$expected_tree" ]]; then
  echo "The exact reviewed final tree is already published."
  exit 0
fi
if [[ $observed != "$expected_base" ]]; then
  echo "Feature branch moved unexpectedly: expected $expected_base, observed $observed." >&2
  exit 1
fi

cat agent_payloads/review3/safe-pr-reorder-review3.patch.gz.b64.* \
  > /tmp/review3.patch.gz.b64
test "$(sha256sum /tmp/review3.patch.gz.b64 | awk '{print $1}')" = \
  50d14daf2ca0328733e32d4c3a9d924466db5cb95c8996c6e066f657a3e05388
base64 --decode /tmp/review3.patch.gz.b64 \
  | gzip --decompress \
  > /tmp/review3.patch
test "$(sha256sum /tmp/review3.patch | awk '{print $1}')" = \
  79f2684f87c75bd4ac0e58d9fa961767eb7563eebf3639945ce409cfd8e4da7f

worktree=$(mktemp -d)
trap 'git worktree remove --force "$worktree" >/dev/null 2>&1 || true; rm -rf "$worktree"' EXIT
git worktree add --detach "$worktree" "$expected_base"
(
  cd "$worktree"
  git config user.name "github-actions[bot]"
  git config user.email "41898282+github-actions[bot]@users.noreply.github.com"
  git am /tmp/review3.patch
  test "$(git rev-list --count "$expected_base"..HEAD)" = 2
  test "$(git rev-parse 'HEAD^{tree}')" = "$expected_tree"
  test "$(git show --no-patch --format=%s HEAD~1)" = \
    "Protect and recover cascade transitions"
  test "$(git show --no-patch --format=%s HEAD)" = \
    "Check grafts in the common Git directory"
  git diff --check "$expected_base"...HEAD
  git push --no-verify \
    --force-with-lease="$feature_ref:$expected_base" \
    origin "HEAD:$feature_ref"
)

#!/usr/bin/env bash

set -euo pipefail

root=$(cd "$(dirname "$0")/.." && pwd)
runner="$root/ci/rebase_stack_child.sh"
test_dir=$(mktemp -d)
trap 'rm -rf "$test_dir"' EXIT

fake_bin="$test_dir/bin"
trace="$test_dir/trace"
mkdir -p "$fake_bin"

cat >"$fake_bin/gh" <<'EOF'
#!/usr/bin/env bash
set -euo pipefail
printf 'gh' >>"$TRACE"
printf '\t%s' "$@" >>"$TRACE"
printf '\n' >>"$TRACE"
if [[ $1 == pr && $2 == list ]]; then
  printf '%s\n' "${FAKE_PR_NUMBER-17}"
fi
EOF

cat >"$fake_bin/git" <<'EOF'
#!/usr/bin/env bash
set -euo pipefail
printf 'git' >>"$TRACE"
printf '\t%s' "$@" >>"$TRACE"
printf '\n' >>"$TRACE"
if [[ $1 == rebase && ${FAIL_REBASE-0} == 1 ]]; then
  exit 1
fi
EOF

chmod +x "$fake_bin/gh" "$fake_bin/git"

metadata() {
  local child=$1
  printf '<!-- gherrit-meta: {"id":"Gparent","parent":null,"child":%s} -->' "$child"
}

run_action() {
  local body=$1
  local pr_number=${2-17}
  local fail_rebase=${3-0}
  env -i \
    "PATH=$fake_bin:$PATH" \
    "ACTION_PATH=$root" \
    "MERGED_PR_BODY=$body" \
    "TRACE=$trace" \
    "FAKE_PR_NUMBER=$pr_number" \
    "FAIL_REBASE=$fail_rebase" \
    GH_TOKEN=test \
    bash "$runner"
}

assert_trace() {
  local expected=$1
  local actual
  actual=$(cat "$trace" 2>/dev/null || true)
  if [[ $actual != "$expected" ]]; then
    printf 'expected trace:\n%s\nactual trace:\n%s\n' "$expected" "$actual" >&2
    exit 1
  fi
}

: >"$trace"
run_action "$(metadata null)"
assert_trace ''

: >"$trace"
run_action "$(metadata '"Gchild"')"
expected=$'gh\tpr\tlist\t--head\tGchild\t--json\tnumber\t--jq\t.[0].number\n'
expected+=$'git\tconfig\tuser.name\tgithub-actions[bot]\n'
expected+=$'git\tconfig\tuser.email\t41898282+github-actions[bot]@users.noreply.github.com\n'
expected+=$'gh\tpr\tcheckout\t17\n'
expected+=$'gh\tpr\tedit\t17\t--base\tmain\n'
expected+=$'git\trebase\torigin/main\n'
expected+=$'git\tpush\t--force-with-lease'
assert_trace "$expected"

: >"$trace"
if run_action "$(metadata '"Gmissing"')" ''; then
  echo "expected a missing child PR to fail" >&2
  exit 1
fi
assert_trace $'gh\tpr\tlist\t--head\tGmissing\t--json\tnumber\t--jq\t.[0].number'

: >"$trace"
if run_action "$(metadata '"Gconflict"')" 42 1; then
  echo "expected a rebase conflict to fail" >&2
  exit 1
fi
expected=$'gh\tpr\tlist\t--head\tGconflict\t--json\tnumber\t--jq\t.[0].number\n'
expected+=$'git\tconfig\tuser.name\tgithub-actions[bot]\n'
expected+=$'git\tconfig\tuser.email\t41898282+github-actions[bot]@users.noreply.github.com\n'
expected+=$'gh\tpr\tcheckout\t42\n'
expected+=$'gh\tpr\tedit\t42\t--base\tmain\n'
expected+=$'git\trebase\torigin/main'
assert_trace "$expected"

# Development Guidelines

This document covers guidelines for developing code changes.

## Build and Test

Use the commands in [validation.md](./validation.md) before submitting a
change. They match the checks run in CI, including the required test-build
configuration.

Use [testing.md](./testing.md) when choosing a test layer or extending the test
infrastructure. It defines the product-risk model and the target architecture
for the ongoing migration away from broad system fixtures.

- `cargo build`: Builds the project.

### Test Dependencies

The integration tests use local Git repositories and a local GitHub API stub.
They do not require GitHub credentials or an authenticated GitHub CLI.

The [resource-policy inventory](resource-policies.md) records existing limits,
their units and their owning operations. It does not change their values.

## Rust Version

This project uses the stable Rust toolchain and the 2024 edition. Ensure your
code compiles and passes tests on the stable channel.

## Management refusal

Explicit `manage` and `unmanage` requests return failure when configuration drift
blocks the requested change. Automatic post-checkout management remains
advisory: a known drift refusal warns and preserves configuration without making
Git report a failed checkout. Failed observations and writes still return errors.
The installed-hook regression covers tracking the default branch and contrasts
its successful checkout with a failing explicit management request.

## Hook installation

The installer inspects all destination bytes and constructs the complete plan
before creating directories or replacing hooks. Binary hooks follow the same
sentinel ownership and `--force` rules as text hooks. Each replacement is
written and made executable in a temporary sibling, then renamed over its
destination. A failed preparation leaves that hook untouched. Replacement
replaces a symlink entry rather than modifying its target. Existing file
ownership, ACLs, and other metadata are not copied to the replacement.

The three hooks are replaced sequentially, not transactionally. Earlier
complete replacements can remain after a later failure; rerunning installation
repairs that prefix. This does not guarantee durability after power loss, lock
out another installer, or establish exclusive ownership of an external path.
Git uses common-directory default hooks in linked worktrees; explicit relative
hook paths are resolved from each worktree or the bare Git directory.

## Stack action

`ci/rebase_stack_child.sh` uses the workflow's `GITHUB_REPOSITORY` for lookup,
checkout and PR editing. A nonempty child ID must identify exactly one open
same-repository PR with that exact head name. Missing, multiple, fork or
mismatched observations stop before checkout or mutation. The shell contracts
in `ci/test_rebase_stack_child.sh` cover those refusal paths.

## Local execution context

Run `python3 ci/execution_context.py --directory /path/to/worktree` to inspect
which binary is on PATH, its version, worktree and common Git directories,
effective hooks, selected remote, and default-branch candidates with local and
tracking OIDs. Use `--binary /path/to/gherrit` to query an explicit executable.
Relative PATH entries and explicit binary paths resolve from the inspected
directory, so lookup and invocation identify the same executable. Local remote
paths retain literal at-signs; only recognized URL/SCP user information is
removed. The helper makes local observations only. It does not fetch or write config,
and it labels remote freshness unknown. Missing configuration and failed
commands are distinct; an unavailable fact does not authorize a fallback.
Each subprocess has a ten-second timeout. Remote URL user information, queries
and fragments are removed, and failed commands do not print arbitrary stderr.
This is a developer diagnostic, not a stable product status or admission API.

`ci/validate.sh devtools` exercises redaction and a native linked-worktree
contract, including relative hooks and unchanged configuration and refs.

## Current-head PR observations

Run `python3 ci/pr_readiness.py --repo owner/repository 123` for a read-only
report of head and base commits, head-commit checks, workflow runs, and formal
review decisions. It distinguishes no run, approval required, running, failed,
successful and unknown. It summarizes the latest check/app and workflow/event
for the observed PR and base, excluding other PRs or old bases that share its
head. Unbound PR runs leave the workflow observation incomplete. Nullable check
apps remain valid, and a check's requested action is distinct from workflow
approval. It retains attempts as evidence and marks whether each formal decision
names the current head. Ordinary review comments, bot messages and emoji
reactions do not become formal approvals. It reads the PR again after
observation; a changed head or base invalidates the summaries.

The report is not permission to merge. It omits rulesets, required-check
selection, merge-commit checks, commit status contexts and comment/reaction
approvals. Head checks alone do not prove validation against the recorded base.
Missing API access fails rather than indicating green CI. Each request has a
30-second timeout. Reviews/checks are bounded at 100 pages of 100 records;
filtered workflow searches are bounded at ten pages because GitHub caps them
at 1,000 results. A full bound is explicitly incomplete and summarizes unknown.
See GitHub's [workflow run API](https://docs.github.com/en/rest/actions/workflow-runs),
[check API](https://docs.github.com/en/rest/checks/runs), and
[review API](https://docs.github.com/en/rest/pulls/reviews).

To approve existing workflow runs deliberately, add `--approve-workflows
--expect-head FULL_OID --expect-base FULL_OID`. The helper rechecks the PR and
each run before the POST, requiring an approval-required pull-request run
whose PR record names both expected commits. An absent binding refuses the
operation. A later failure may leave earlier approvals applied; errors report
that prefix. A failed POST has an uncertain outcome. These observations cannot
fence a concurrent edit. Approval starts execution; run the read-only helper
again for fresh results. The helper never dispatches a new workflow or changes
credentials. Its scripted API contracts run with `ci/validate.sh devtools`.

*Updated by an AI agent acting on joshlf's behalf.*

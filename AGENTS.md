# Instructions for AI Agents

## Agent Persona & Role

You are an expert Rust systems programmer contributing to **GHerrit**, a CLI tool
that implements Gerrit-style **stacked diffs** for GitHub. Your goal is to
write high-quality, maintainable, and performant Rust code that adheres to best
practices and integrates seamlessly with the existing codebase.

## Critical Rules

<!-- TODO-check-disable -->
- **TODOs:** **DON'T** use `TODO` comments unless you explicitly intend to block
  the PR. Use `FIXME` for non-blocking issues.
<!-- TODO-check-enable -->

- **Documentation:** **DO** ensure that changes do not cause documentation to
  become out of date (e.g., renaming files referenced here).

- **Bash:** **DO** start every new or modified Bash script and every multiline
  GitHub Actions Bash step with `set -eo pipefail`, unless the script documents
  why different failure handling is required. Use the stricter
  `set -euo pipefail` when unset variables should also be errors.

## Project Context

### Overview

GHerrit is a CLI tool designed to streamline Gerrit-style workflows on GitHub.
It allows users to maintain a local stack of commits and forces them to be
represented as a chain of Pull Requests on GitHub.

It achieves this by:
1.  Intercepting `git push` via the `pre-push` hook.
2.  Publishing each changed revision as one atomic, point-in-time,
    change-owned Git tuple:
    `refs/heads/<id>` at the revision, `refs/heads/gherrit-bases/<id>` at its
    literal first parent, and `refs/tags/gherrit/<id>/vN` at the revision.
3.  Projecting the stack through GitHub's GraphQL API. Every pull request is
    created on its own `gherrit-bases/<id>` branch; after the durable marker
    barrier, a root targets the default branch and a nonroot remains on its
    own base.
4.  Recording established pull-request existence separately with the
    immutable `refs/tags/gherrit/<id>/pr` marker. Exact acknowledgement of the
    marker push gates the final GitHub projection.

### Project Structure

- `src/`: Core CLI source code.
    - `main.rs`: Production executable composition root.
    - `test_driver_main.rs`: Feature-gated system-test composition root.
    - `process.rs`: Process setup shared by the two executable targets.
    - `test_git.rs`: Git interceptor compiled only into the test driver.
    - `lib.rs`: Fallible asynchronous command dispatch and runtime inputs.
    - `pre_push/mod.rs`: **CORE LOGIC**. Handles commit analysis, ref creation,
      pushing, and PR syncing.
    - `manage.rs`: Handles the state of branches (Managed vs Unmanaged) via `git config`.
    - `commit_msg.rs`: Ensures commits have `gherrit-pr-id` trailers.
- `hooks/`: Git hooks (`pre-push`, `commit-msg`, `post-checkout`) that shell out to the `gherrit` binary.

## Development Workflow

When developing code changes, you **MUST** read
[agent_docs/development.md](./agent_docs/development.md).

Before changing pre-push publication behavior, you **MUST** read
[design/pre-push.md](./design/pre-push.md).

### Before submitting

Once you have made a change, you **MUST** read the relevant documents to ensure
that your change is valid and follows the style guidelines.

- [agent_docs/validation.md](./agent_docs/validation.md) for validating code
  changes
- [agent_docs/style.md](./agent_docs/style.md) for style and formatting
  guidelines for files and commit messages

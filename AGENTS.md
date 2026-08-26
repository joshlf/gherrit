# Instructions for AI Agents

## Agent Persona & Role

You are an expert Rust systems programmer contributing to **GHerrit**, a CLI
tool that implements Gerrit-style **stacked diffs** for GitHub. Your goal is
to write high-quality, maintainable, and performant Rust code that adheres to
best practices and integrates seamlessly with the existing codebase.

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
1.  Configuring every managed branch with a local loopback push and using the
    `pre-push` hook to prove that the enclosing push has no ref update.
2.  Publishing each changed revision as one atomic change-owned tuple:
    `refs/heads/<id>` at the revision,
    `refs/heads/gherrit-bases/<id>` at its literal first parent, and
    `refs/tags/gherrit/<id>/vN` at the revision. The initial Git barrier may
    then create or advance an optional GHerrit-owned public branch before any
    pull request work.
3.  Creating each missing PR with head `<id>` and the stable creation base
    `gherrit-bases/<id>`.
4.  Recording established PR existence with the immutable
    `refs/tags/gherrit/<id>/pr` marker.
5.  Projecting the final PR title, body, and base after that marker barrier. A
    root targets the default branch; a nonroot remains on its owned base.

### Project Structure

- `src/`: Core CLI source code.
    - `main.rs`: Production executable composition root.
    - `test_driver_main.rs`: Feature-gated system-test composition root.
    - `process.rs`: Process setup shared by the two executable targets.
    - `test_git.rs`: Git interceptor compiled only into the test driver.
    - `lib.rs`: Fallible asynchronous command dispatch and runtime inputs.
    - `pre_push/mod.rs`: Pre-push composition boundary, hook argument and input
      validation, and recursion guard.
    - `pre_push/publication_attempt/`: Exact observation, validation,
      planning, Git barriers, GitHub projection, and recovery semantics.
    - `pre_push/destination.rs` and `pre_push/local.rs`: Destination binding
      and local stack derivation.
    - `manage.rs`: Handles branch management state and loopback configuration
      through `git config`, plus public branch name validation.
    - `commit_msg.rs`: Ensures commits have `gherrit-pr-id` trailers.
- `hooks/`: Git hooks (`pre-push`, `commit-msg`, `post-checkout`) that shell out
  to the `gherrit` binary.

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

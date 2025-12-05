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

## Project Context

### Overview

GHerrit is a CLI tool designed to streamline Gerrit-style workflows on GitHub.
It allows users to maintain a local stack of commits and forces them to be
represented as a chain of Pull Requests on GitHub.

It achieves this by:
1.  Intercepting `git push` via the `pre-push` hook.
2.  Creating unique `refs/gherrit/<id>` refs for every commit.
3.  Pushing these refs to the remote.
4.  Using the `gh` CLI tool to create/update PRs and chain them together (setting the base of one PR to the head of the previous one).

### Project Structure

- `src/`: Core CLI source code.
    - `main.rs`: Entry point and CLI definition.
    - `pre_push.rs`: **CORE LOGIC**. Handles the commit analysis, ref creation, pushing, and PR syncing.
    - `manage.rs`: Handles the state of branches (Managed vs Unmanaged) via `git config`.
    - `commit_msg.rs`: Ensures commits have `gherrit-pr-id` trailers.
- `hooks/`: Git hooks (`pre-push`, `commit-msg`, `post-checkout`) that shell out to the `gherrit` binary.

## Development Workflow

When developing code changes, you **MUST** read
[agent_docs/development.md](./agent_docs/development.md).

### Before submitting

Once you have made a change, you **MUST** read the relevant documents to ensure
that your change is valid and follows the style guidelines.

- [agent_docs/validation.md](./agent_docs/validation.md) for validating code
  changes
- [agent_docs/style.md](./agent_docs/style.md) for style and formatting
  guidelines for files and commit messages

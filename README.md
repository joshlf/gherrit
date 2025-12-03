# GHerrit

**GHerrit** is a tool that brings a **Gerrit-style "Stacked Diffs" workflow** to GitHub.

It allows you to maintain a single local branch containing a stack of commits (e.g., `feature-A` -\> `feature-B` -\> `feature-C`) and automatically synchronizes them to GitHub as a chain of dependent Pull Requests.

## Installation

### Prerequisites

  * **Rust**: You must have a working Rust toolchain (`cargo`).
  * **GitHub CLI (`gh`)**: GHerrit uses the `gh` tool to create and manage PRs. Ensure you are authenticated (`gh auth login`).

### Setup

1.  **Install the Binary:**

    ```bash
    cargo install --path .
    ```

2.  **Configure Hooks:**
    GHerrit relies on Git hooks to intercept commits and pushes. In the repository you wish to manage:

    ```bash
    # Copy hooks to your .git directory
    cp hooks/commit-msg .git/hooks/commit-msg
    cp hooks/pre-push .git/hooks/pre-push

    ```bash
    # Ensure they are executable
    chmod +x .git/hooks/commit-msg
    chmod +x .git/hooks/pre-push
    ```

3.  **Setup GitHub Action (Optional but Recommended):**
    To enable automatic cascading merges (where merging a parent PR automatically rebases its child), add the following workflow to your repository at `.github/workflows/gherrit-rebase-stack.yml`:

    ```yaml
    name: Rebase Stack
    on:
      pull_request:
        types: [closed]

    permissions:
      contents: write
      pull-requests: write

    jobs:
      rebase-stack:
        if: github.event.pull_request.merged == true
        runs-on: ubuntu-latest
        steps:
          - uses: actions/checkout@v4
            with:
              fetch-depth: 0
              token: ${{ secrets.GITHUB_TOKEN }}

          - name: Run Gherrit Cascade
            uses: joshlf/gherrit@main
            with:
              token: ${{ secrets.GITHUB_TOKEN }}
              pr_body: ${{ github.event.pull_request.body }}
    ```

## Usage

Once installed, simply work as if you were using Gerrit.

### 1\. Creating a Stack

Create a branch to track your work, and create multiple commits.

```bash
git checkout -b api-endpoints

# Hack on feature A
git commit -m "optimize database query construction"

# Hack on feature B (which depends on A)
git commit -m "add api endpoints"
```

*Note: The `commit-msg` hook automatically appends a unique `gherrit-pr-id` to every commit message.*

### 2\. Pushing

When you are ready to upload your changes, simply push:

```bash
git push
```

**GHerrit intercepts this push.** Instead of pushing your local branch directly, it:

1.  Analyzes your stack of commits.
2.  Pushes each commit to a dedicated "phantom branch" on GitHub.
3.  Creates or Updates a Pull Request for each commit.
4.  Updates the PR bodies to include navigation links:

<img width="915" height="317" alt="Screenshot 2025-12-02 at 6 46 15 PM" src="https://github.com/user-attachments/assets/6ee80641-af67-4b37-9f57-797207637bbe" />

### 3\. Updating the Stack

To modify a commit in the middle of the stack, use interactive rebase:

```bash
git rebase -i main
# (Edit, squash, or reword commits)
```

Then push again:

```bash
git push
```

GHerrit will detect the changes based on the persistent `gherrit-pr-id` in the commit trailers and update the corresponding PRs in place.

-----

## Design & Architecture

### Core Architecture

#### `gherrit-pr-id` Trailer and Phantom Branches

Inspired by Gerrit, each commit managed by GHerrit includes a trailer line in its commit message, e.g., `gherrit-pr-id: G847...`.

GitHub identifies PRs by *branch name* (specifically, a PR is a request to merge the contents of one *branch* into another). A branch can contain multiple commits, leading to a one-to-many  relationship between PRs and commits. In the Gerrit style, we want a one-to-one relationship between PRs and commits. However, Git commits do not have stable identifiers – commit hashes change on rebase, on `git commit --amend`, etc. The `gerrit-pr-id` trailer acts as a stable key for the commit that survives rebases and other commit changes.

Since the user will have a single branch locally containing multiple commits, a normal `git push` would simply result in a single PR for the whole branch. Instead, GHerrit pushes changes by synthesizing "phantom" branches: Each commit is pushed to a branch whose name matches that commit's `gherrit-pr-id` trailer. GHerrit then uses the `gh` tool to create or update one PR for each commit, setting the base and source branches to the appropriate phantom branches.

#### `pre-push` Hook

GHerrit synchronizes changes with GitHub in a `pre-push` hook. This allows users to use their normal `git push` flow instead of using a bespoke command like (hypothetically) `gherrit sync`.

#### PR Rewriting

Since Gerrit supports stacked commits, the Gerrit UI for a particular commit lists the other commits in that commit's stack:

<img width="1440" height="374" alt="image" src="https://github.com/user-attachments/assets/4a393bca-e839-4d1f-9092-fc8d69e2edd6" />

&nbsp;

GHerrit emulates this by rewriting each PR's message with links to other PRs in the same stack:

<img width="915" height="317" alt="Screenshot 2025-12-02 at 6 46 15 PM" src="https://github.com/user-attachments/assets/6ee80641-af67-4b37-9f57-797207637bbe" />

#### Cascading Merge Automation

When managing a stack of PRs on GitHub, merging a parent PR (e.g., `feature-A`) into `main` causes a problem for its child PR (`feature-B`). Since `feature-B` was based on the *branch* `feature-A`, and `feature-A` has now been squashed and merged into `main`, GitHub sees the commits in `feature-B` as "new" relative to `main`, even if they are identical to the ones just merged. This often results in "phantom diffs" or merge conflicts.

To solve this, GHerrit implements a **Cascading Merge** system:

1.  **Metadata Injection**: When pushing, GHerrit injects hidden metadata into the PR description (inside an HTML comment) containing the IDs of the parent and child PRs.
2.  **Automated Rebase**: A GitHub Action (`gherrit-rebase-stack.yml`) triggers whenever a PR is merged. It:
    *   Reads the metadata to find the *child* PR's ID.
    *   Finds the child PR by its synthesized branch name (e.g., `G...`)
    *   Retargets the child PR to base off `main`.
    *   Rebases the child PR onto the new `main`.
    *   Force-pushes the updated child PR.

This ensures that as soon as you merge the bottom of the stack, the next PR automatically updates and becomes ready for review/merge, keeping the entire chain healthy without manual intervention.

### Hybrid Workflow Support

GHerrit is designed to work seamlessly with developers using other, non-GHerrit workflows. In order to accomplish this, GHerrit tracks whether each local branch is "managed" or "unmanaged". By default, branches created locally are managed, while branches created remotely (and checked out locally) are "unmanaged". A branch's management state can be changed with `gherrit manage` or `gherrit unmanage`.

The `commit-msg` and `pre-push` hooks respect the management state – when operating on an unmanaged branch, both are no-ops, allowing `git commit` and `git push` to behave as though GHerrit didn't exist.

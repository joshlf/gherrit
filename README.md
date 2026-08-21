# GHerrit

> **Note:** GHerrit is currently in alpha. You're welcome to use it, but please
> be aware that we may make breaking changes.

**GHerrit** is a tool that brings a **Gerrit-style "Stacked Diffs" workflow**
to GitHub.

It allows you to maintain a single local branch containing a stack of commits
(e.g., `feature-A` -> `feature-B` -> `feature-C`) and automatically
synchronizes them to GitHub as a chain of dependent Pull Requests.

## Installation

### Prerequisites

  * **Rust**: You must have a working Rust toolchain (`cargo`).
  * **GitHub CLI (`gh`)**: GHerrit uses the `gh` tool to authenticate to GitHub
    so it can create and manage PRs. Ensure you are authenticated (`gh auth
    login`).

### Setup

1.  **Install the Binary:**

    ```bash
    cargo install --git https://github.com/joshlf/gherrit gherrit
    ```

2.  **Install Hooks:**
    GHerrit relies on Git hooks to intercept branch creation, commits, and
    pushes. In the repository you wish to manage:

    ```bash
    gherrit install
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

*Note: GHerrit's `commit-msg` hook automatically appends a unique
`gherrit-pr-id` to every commit message.*

### 2\. Pushing

When you are ready to upload your changes, simply push:

```bash
git push
```

**GHerrit intercepts this push.** Instead of pushing your local branch directly, it:

1.  Analyzes your stack of commits.
2.  Pushes each commit to a dedicated "phantom branch" on GitHub.
3.  Creates or Updates a Pull Request for each commit.
4.  Updates the PR bodies to include navigation links.
5.  Injects a "Patch History" table into the PR description. Because GHerrit
    tracks every version of your commit, this table provides direct links to
    view the **diff between versions** (e.g., "Compare v3 vs v2"). This allows
    reviewers to immediately see what changed since their last review.

<img width="918" height="575" alt="Screenshot 2025-12-05 at 1 13 16 PM" src="https://github.com/user-attachments/assets/97d59a3d-0697-4c74-a833-9cc6da2089ee" />

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

GHerrit will detect the changes based on the persistent `gherrit-pr-id` in the
commit trailers and update the corresponding PRs in place.

## Configuration

### Public vs. Private Stacks

By default, GHerrit configures managed branches as **Private Stacks**. On `git
push`, GHerrit will synchronize your stack to GitHub without actually pushing
your local branch tip to the remote server. This avoids cluttering the remote
repository with branches and avoids leaking the names of your local branches to
remote users.

If you wish to maintain a **Public Stack** (where your local branch is *also*
pushed to `origin` for backup or collaboration), you can override this:
```bash
gherrit manage --public
```

## Design & Architecture

*If you only intend to **use** GHerrit, and don't care about its internals,
then you can stop reading now.*

### Core Architecture

#### `gherrit-pr-id` Trailer and Phantom Branches

Inspired by Gerrit, each commit managed by GHerrit includes a trailer line in
its commit message, e.g., `gherrit-pr-id: G847...`.

GitHub identifies PRs by *branch name* (specifically, a PR is a request to
merge the contents of one *branch* into another). A branch can contain multiple
commits, leading to a one-to-many relationship between PRs and commits. In the
Gerrit style, we want a one-to-one relationship between PRs and commits.
However, Git commits do not have stable identifiers – commit hashes change on
rebase, on `git commit --amend`, etc. The `gerrit-pr-id` trailer acts as a
stable key for the commit that survives rebases and other commit changes.

Since the user will have a single branch locally containing multiple commits, a
normal `git push` would simply result in a single PR for the whole branch.
Instead, GHerrit pushes changes by synthesizing "phantom" branches: Each commit
is pushed to a branch whose name matches that commit's `gherrit-pr-id` trailer.
GHerrit then uses the GitHub API to create or update one PR for each commit,
setting the base and source branches to the appropriate phantom branches.

#### Version Tags

In addition to pushing branches, GHerrit pushes a lightweight tag for every
version of every commit in the stack, formatted as
`refs/tags/gherrit/<id>/v<version>`. Normally, force-push workflows destroy the
history of previous iterations. By tagging every version, GHerrit persists the
entire evolution of a PR. These version tags can be used to diff any two
versions of a PR – this is how GHerrit generates the **Patch History Table** in
the PR description.

#### Git Ref Drift Detection

GHerrit's publication protocol assumes one publisher at a time. It does not
provide serializable multi-publisher publication, and GitHub mutations cannot
be conditioned on an expected Git ref state.

Within that boundary, GHerrit detects Git ref drift between observation and
its own Git writes. One remote observation establishes the default branch and
all managed heads. After deriving the local stack, a second batched observation
reads immutable version history only for its active changes. Work therefore
scales with repository heads plus active histories, not every version ever
published in the repository.

Before changing a managed head, GHerrit derives the next version from remote
history, leases the exact observed head, and requires the new version tag to be
absent. If either ref changes after observation, Git rejects the
complete atomic push. A change already at its desired head is a true no-op and
sends no lease. These checks protect refs which GHerrit actually updates; they
do not fence the later pull request metadata projection. Local version tags do
not participate in these decisions.

Before observing GitHub or writing refs, GHerrit renders every variable push
argument and partitions the complete branch-and-tag pair for each change into
conservatively byte-budgeted batches. A pair is never split between pushes. An
individually oversized change anywhere in the stack rejects the whole plan;
an unchanged stack produces no push.

#### `pre-push` Hook

GHerrit synchronizes changes with GitHub in a `pre-push` hook. This allows
users to use their normal `git push` flow instead of using a bespoke command
like (hypothetically) `gherrit sync`.

##### "Loopback" Interception Strategy

By default, GHerrit configures managed branches to treat the local repository as
its own upstream. It sets:

*   `branch.<name>.pushRemote = .`
*   `branch.<name>.remote = .`
*   `branch.<name>.merge = refs/heads/<name>`

This configuration has two benefits:

1.  **Interception:** On `git push`, once GHerrit's `pre-push` hook returns
    (after synchronizing the stack to GitHub), Git will always complete the
    push. Other than causing `git push` to fail with a user-visible error,
    there is no way to for the `pre-push` hook to prevent the push from
    completing. Setting `pushRemote = .` ensures that, when the push is
    performed, it targets the local repository, which is a no-op.
2.  **UX:** This configuration satisfies Git's upstream requirements, allowing
    users to run `git push` immediately after branch creation without seeing
    "fatal: The current branch has no upstream branch" errors.

#### PR Rewriting

Since Gerrit supports stacked commits, the Gerrit UI for a particular commit
lists the other commits in that commit's stack:

<img width="1440" height="374" alt="image" src="https://github.com/user-attachments/assets/4a393bca-e839-4d1f-9092-fc8d69e2edd6" />

&nbsp;

GHerrit emulates this by rewriting each PR's message with links to other PRs in
the same stack:

<img width="915" height="317" alt="Screenshot 2025-12-02 at 6 46 15 PM" src="https://github.com/user-attachments/assets/6ee80641-af67-4b37-9f57-797207637bbe" />

#### Landing a Stack

Only merge the root PR whose base is the repository's default branch. GHerrit
does not automatically rebase the remaining stack after the root lands.

Run the following commands with the managed stack branch checked out.
`<target-fetch-remote>` must fetch from the same repository as GHerrit's
configured push destination. Do not assume that the configured GHerrit remote
is suitable: a Git remote can fetch from one repository and push to another.
Add or use a separate fetch remote when necessary.

Replace `<default-branch>` with the repository's default branch and
`<merged-root-commit>` with the local commit for the PR which just landed:

```bash
git fetch <target-fetch-remote> \
    refs/heads/<default-branch>:refs/heads/<default-branch>
git rebase --onto refs/heads/<default-branch> <merged-root-commit>
git push
```

The explicit fetch destination advances the local default branch. The refspec
has no leading `+`, so Git accepts only a fast-forward and refuses to overwrite
a divergent local branch. If that command fails, reconcile the local default
branch deliberately rather than forcing the fetch. GHerrit uses the local
default branch, not a remote-tracking branch, as the start of the stack.

For example, if `upstream` fetches from the repository GHerrit pushes to and
that repository's default branch is `main`:

```bash
git fetch upstream refs/heads/main:refs/heads/main
git rebase --onto refs/heads/main <merged-root-commit>
git push
```

GHerrit updates the remaining PRs from the rebased commits.

### Hybrid Workflow Support

GHerrit is designed to work seamlessly with developers using other, non-GHerrit
workflows. In order to accomplish this, GHerrit tracks whether each local
branch is "managed" or "unmanaged". By default, branches created locally are
managed, while branches created remotely (and checked out locally) are
"unmanaged". A branch's management state can be changed with `gherrit manage`
or `gherrit unmanage`.

The `commit-msg` and `pre-push` hooks respect the management state – when
operating on an unmanaged branch, both are no-ops, allowing `git commit` and
`git push` to behave as though GHerrit didn't exist.

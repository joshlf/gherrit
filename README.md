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
  * **GitHub authentication**: Either set `GITHUB_TOKEN` to a token which can
    read and write pull requests in the destination repository, or authenticate
    the GitHub CLI with `gh auth login`.

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
2.  Publishes each changed commit's head, literal first-parent base, and new
    immutable version tag as one atomic tuple.
3.  Creates missing Pull Requests on their permanent owned bases.
4.  Records established Pull Requests with a separate immutable Git marker,
    then updates their final bases and numbered navigation through GitHub's
    GraphQL API.
5.  Injects a "Patch History" table into the PR description. Because GHerrit
    tracks every version of your commit, this table provides direct links to
    view the **diff between versions** (e.g., "Compare v3 vs v2"). This allows
    reviewers to immediately see what changed since their last review.

<img width="918" height="575" alt="Screenshot 2025-12-05 at 1 13 16 PM" src="https://github.com/user-attachments/assets/97d59a3d-0697-4c74-a833-9cc6da2089ee" />

### 3\. Updating the Stack

To modify a commit in the middle of the stack, use interactive rebase:

```bash
git rebase --rebase-merges -i main
# (Edit, squash, or reword commits)
```

Then push again:

```bash
git push
```

GHerrit will detect the changes based on the persistent `gherrit-pr-id` in the
commit trailers and update the corresponding PRs in place. The
`--rebase-merges` option preserves merge topology in stacks which contain
merge commits.

### 4\. Landing a Stack

Merge only the root PR, whose base is the repository's default branch. After it
lands, update your local copy of the repository's default branch, rebase the
remaining stack onto it, and run `git push` again. Repeat from the new root
until the stack is landed.

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

#### `gherrit-pr-id` Trailer and Owned Publication Refs

Inspired by Gerrit, each commit managed by GHerrit includes a trailer line in
its commit message, e.g., `gherrit-pr-id: G847...`.

GitHub identifies PRs by *branch name* (specifically, a PR is a request to
merge the contents of one *branch* into another). A branch can contain multiple
commits, leading to a one-to-many relationship between PRs and commits. In the
Gerrit style, we want a one-to-one relationship between PRs and commits.
However, Git commits do not have stable identifiers – commit hashes change on
rebase, on `git commit --amend`, etc. The `gherrit-pr-id` trailer acts as a
stable key for the commit that survives rebases and other commit changes.

Since the user has a single local branch containing multiple commits, a normal
`git push` would result in one PR for the whole branch. GHerrit instead gives
each change two owned branches. For change `G`, `refs/heads/G` points to the
change's current revision and `refs/heads/gherrit-bases/G` points to that
revision's literal first parent. The latter never names another change's
mutable head branch.

Every pull request is created with head `G` and base `gherrit-bases/G`. This
stable creation key remains the same across amendments, rebases, reorders, and
root-status changes. After Git records that the pull request exists, the final
GraphQL projection places a root pull request on the repository's default
branch; every nonroot pull request remains on its own `gherrit-bases/G` branch.

#### Version Tags

Alongside both owned branches, GHerrit publishes a lightweight tag for every
version of every commit in the stack, formatted as
`refs/tags/gherrit/<id>/v<version>`. The three refs form one point-in-time
publication tuple:

```text
refs/heads/G                 -> current revision
refs/heads/gherrit-bases/G   -> current revision's literal first parent
refs/tags/gherrit/G/vN       -> current revision
```

When any member must change, GHerrit publishes the whole tuple atomically; it
never exposes a head-only or head-and-tag update. Normally, force-push
workflows destroy the history of previous iterations. By tagging every
version, GHerrit persists the entire evolution of a PR. These version tags can
be used to diff any two versions of a PR – this is how GHerrit generates the
**Patch History Table** in the PR description.

The tags advertised by the configured push destination are the authoritative
version history. GHerrit neither reads nor creates local version tags, so a
fresh clone and an older working copy select the same next version. Before any
write, one global observation establishes the default branch and every remote
head. After deriving the local stack, one or more byte-bounded requests observe
exact version-tag namespaces only for active changes. Attempts therefore use
one global Git read plus one or more active-history reads; ordinary stacks need
two Git reads total. Response and backend work scale with all heads plus active
histories rather than every historical version tag. GHerrit then
requires every published history to be contiguous from `v1` and its latest tag
to agree with the managed head. It validates and plans the complete local stack
before publishing any prefix of it.

#### Leased Git Updates

GHerrit's publication protocol assumes one publisher at a time. For every
change it does update, GHerrit nevertheless rejects drift between observation
and the Git write: both mutable branches are leased against the exact objects
observed at the push destination, and a new version tag (e.g., `v2`) is leased
against absence:
`--force-with-lease=refs/tags/gherrit/<id>/v<ver>:`. All three tuple members are
sent in the same atomic push and are never split between bounded batches.
Up-to-date tuples are not included in a push, and GitHub mutations are not
serialized with Git writes, so these leases are not a general multi-publisher
lock.

The trailing colon (`:`) tells Git to ensure the ref does **not** already exist
on the remote. If another user has already pushed `v2` in the interim, the
assertion fails and the complete atomic batch is rejected. The publisher must
observe the destination again before retrying.

Pull-request existence uses a separate immutable marker,
`refs/tags/gherrit/<id>/pr`, rather than a fourth member of the publication
tuple. GHerrit prepares this create-only marker only after it has acknowledged
or observed the corresponding pull request. Exact acknowledgement of the
marker batch is a second Git barrier: final GraphQL body and base updates are
not available before it. A lost marker acknowledgement therefore leaves the
pull request safely on its owned base for a later attempt.

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

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
  * **Git**: Version 2.31 or newer. Promisor and partial-clone repositories
    require Git 2.45 or newer.
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

**GHerrit intercepts this push.** Instead of pushing your local branch
directly, it:

1.  Analyzes your stack of commits.
2.  Atomically publishes each changed commit's head, literal first-parent
    base, and next immutable version tag.
3.  For a public stack, reconciles its GHerrit-owned branch projection after
    those change tuples and before any Pull Request write.
4.  Creates missing Pull Requests on permanent, change-owned base branches.
5.  Records each established Pull Request with an immutable Git marker before
    applying its final base, title, body, and navigation links.
6.  Injects a "Patch History" table into the PR description. Because GHerrit
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
push`, GHerrit will publish your stack to GitHub without actually pushing
your local branch tip to the remote server. This avoids cluttering the remote
repository with branches and avoids leaking the names of your local branches to
remote users.

If you wish to maintain a **Public Stack**, GHerrit can also project the tip of
your local branch to the configured push destination:

```bash
gherrit manage --public
```

A public branch is a GHerrit-owned, force-updated projection. If it must
change, GHerrit leases it against the exact remote value observed at the start
of the attempt. A competing value then rejects the containing atomic push. If
the branch was already at the desired tip, GHerrit performs no redundant write
or lease. An independently written value which GHerrit has already observed is
intentionally replaced. Public mode is therefore suitable for backup and
read-only sharing, not as a bidirectional collaboration branch. Do not update
the remote branch independently or use it as a stable base for independent
work or Pull Requests. Changing a branch to private mode or renaming it does
not delete an earlier public ref.

The first path component of a public branch name must contain at least one
character other than an ASCII letter or digit, and the name cannot be
`gherrit-bases` or below that namespace. The public branch also cannot equal or
be a ref-path ancestor or descendant of the repository default branch. These
rules keep it disjoint from every branch owned by a GHerrit change and from the
default branch. Names such as `feature-/work` and `release-candidate` work;
names such as `feature`, `feature/work`, and `Gchange/backup` do not.
An unrelated ordinary remote branch which is a ref-path ancestor or descendant
can still make the public push fail until that ref is removed, the branch is
renamed, or private mode is used.

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

Since the user has one local branch containing multiple commits, a normal
`git push` would result in one PR for the whole branch. GHerrit instead gives
each change `G` two owned branches. `refs/heads/G` points to the current
revision, while `refs/heads/gherrit-bases/G` points to that revision's literal
first parent. The owned base never points to another change's mutable head
branch.

Every PR is created with head `G` and base `gherrit-bases/G`. This stable
creation key does not change when a stack is amended, rebased, reordered, or
moved between root and nonroot positions. Once the PR's existence is durably
recorded, the final projection places a root PR on the repository's default
branch; a nonroot PR remains on its own `gherrit-bases/G` branch.

#### Version Tags

Alongside both owned branches, GHerrit publishes a lightweight tag for every
version of every change. The three refs form one point-in-time tuple:

```text
refs/heads/G                 -> current revision
refs/heads/gherrit-bases/G   -> current revision's literal first parent
refs/tags/gherrit/G/vN       -> current revision
```

When any member must change, GHerrit publishes the complete tuple atomically.
It never exposes a head-only or head-and-tag update. Immutable version tags
preserve earlier iterations for the **Patch History Table** in each PR body.
The configured push destination, rather than local tags, is the authoritative
version history, so a fresh clone and an older working copy choose the same
next version.

#### Publication Barriers and Concurrency

Before writing, GHerrit observes the exact default branch, the optional public
branch, and only the owned Git namespaces and all-state GitHub connections for
IDs in the local stack. This keeps network and backend work proportional to the
local stack and its history rather than to unrelated repository state. It
validates and plans the complete stack before publishing any prefix.

Every planned mutable-ref update is leased against its exact observed object,
every new tag is leased against absence, and all members of a tuple share one
atomic push. If another publisher wins a conflicting lease, the entire batch
is rejected. If it publishes the same desired tuple first, Git's exact
acknowledgement can accept the tuple as already complete and the current
attempt continues. A rejected or indeterminate push ends the attempt; a fresh
invocation then reobserves durable state.

For a public stack, an out-of-date public branch is created or advanced with an
exact lease in the initial Git stage. A branch already at the desired tip needs
only its exact initial observation. Any public operation follows every tuple
operation, and all initial Git batches must be acknowledged before GHerrit
creates or updates a pull request. An empty public stack still projects its
branch tip but does not authenticate to GitHub.

Pull-request existence uses a separate immutable marker,
`refs/tags/gherrit/G/pr`. GHerrit can create this marker only after observing
the PR or receiving its exact create acknowledgement. The marker is a second
Git barrier: final GitHub projection is unavailable until the marker push is
acknowledged. A crash or lost acknowledgement can therefore leave only safe
states—a complete old or new tuple, a public branch at its prior or desired
tip, a PR on its permanent owned base, or a completed marker. GHerrit does not
roll effects back or retry ambiguous writes within the same attempt; the next
invocation reconstructs authority from Git and exact local PR observations.

#### `pre-push` Hook

GHerrit publishes changes to GitHub in a `pre-push` hook. This allows
users to use their normal `git push` flow instead of using a bespoke command
like (hypothetically) `gherrit sync`.

##### "Loopback" Interception Strategy

GHerrit configures every managed branch, private or public, to treat the local
repository as its own upstream. It sets:

*   `branch.<name>.pushRemote = .`
*   `branch.<name>.remote = .`
*   `branch.<name>.merge = refs/heads/<name>`

This configuration has two benefits:

1.  **Interception:** On plain `git push`, GHerrit requires Git's hook
    arguments to identify the `.` destination and requires standard input to
    contain no ref update. Once GHerrit's acknowledged publication finishes,
    the enclosing Git process therefore has no remaining remote effect. An
    external destination or any refspec which would produce an update is
    rejected. Git does not expose whether the user explicitly supplied `.` or
    an already-up-to-date refspec, because those have the same no-effect hook
    shape.
2.  **UX:** This configuration satisfies Git's upstream requirements, allowing
    users to run `git push` immediately after branch creation without seeing
    "fatal: The current branch has no upstream branch" errors.

Git does not tell a pre-push hook whether the user passed `--dry-run`.
Consequently, `git push --dry-run` still runs GHerrit's real publication
protocol even though the enclosing loopback push is a dry run.

An installed or composite hook wrapper must forward both hook arguments
unchanged and leave standard input connected to GHerrit. Hook success proves
only that the enclosing Git process has no ref update; another hook or a later
Git failure can still make the enclosing `git push` fail after GHerrit has
durably published. Running `git push` again safely reobserves that state.

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

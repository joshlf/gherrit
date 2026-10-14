# GHerrit

> **Note:** GHerrit is currently in alpha. You're welcome to use it, but please
> be aware that we may make breaking changes.

**GHerrit** is a tool that brings a **Gerrit-style "Stacked Diffs" workflow**
to GitHub.

It allows you to maintain a single local branch containing a stack of commits
(e.g., `feature-A` -> `feature-B` -> `feature-C`) and automatically
publishes them to GitHub as a chain of dependent Pull Requests.

## Installation

### Prerequisites

  * **Rust**: You must have a working Rust toolchain (`cargo`).
  * **Git**: Version 2.31 or newer. Promisor and partial-clone repositories
    require Git 2.45.1 or newer.
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

> **Important upgrade note:** This release cannot adopt pull requests or
> stacks published by GHerrit versions from before owned base branches and
> pull-request markers. Finish an existing stack with the GHerrit version that
> created it, or assign new `gherrit-pr-id` values to its remaining commits and
> start new pull requests before using this release.
>
> Existing public managed branches also need one configuration migration. On
> each such branch, run `gherrit manage` once to replace the old direct push
> destination with the local loopback used by this publication model. Public
> branch names must now be disjoint from GHerrit's owned ref namespaces. If an
> existing name is rejected, rename the branch before keeping it public or run
> `gherrit manage --private` to stop projecting a same-named remote branch.

When you are ready to upload your changes, simply push:

```bash
git push
```

**GHerrit intercepts this push.** It publishes each changed commit as a
revision of the PR identified by that commit's `gherrit-pr-id`, records
immutable patch history, and updates titles, descriptions, and navigation.
New PRs start as drafts. The Patch History table links directly to submitted
revisions and comparisons between versions.

The [architecture map](design/architecture.md) explains identities and code
responsibilities. The [publication contract](design/pre-push.md) specifies
owned bases, canonical PR markers, ordered effects, and recovery.

<img width="918" height="575" alt="Screenshot 2025-12-05 at 1 13 16 PM" src="https://github.com/user-attachments/assets/97d59a3d-0697-4c74-a833-9cc6da2089ee" />

### 3\. Updating the Stack

Fetch the default tip from the same push destination GHerrit uses before
rebasing. These examples use a destination whose default branch is `main`:

Define this Bash helper once in the terminal where you work on the stack. It
fetches the push destination and carries the selected remote's proxy settings
into that URL's native Git context. It leaves persistent configuration alone.

```bash
gherrit_fetch_default() (
    set -eo pipefail
    unset GIT_CONFIG
    gherrit_remote="$(git config --default origin --get gherrit.remote)"
    gherrit_destination="$(git remote get-url --push -- "$gherrit_remote")"
    transport_args=()
    for setting in proxy proxyAuthMethod; do
        if value="$(git config --get "remote.$gherrit_remote.$setting")"; then
            config_env="GHERRIT_FETCH_$setting"
            export "$config_env=$value"
            transport_args+=("--config-env=remote.$gherrit_destination.$setting=$config_env")
        elif [ "$?" -ne 1 ]; then
            return 1
        fi
    done
    git "${transport_args[@]}" fetch --no-tags --refmap= -- \
        "$gherrit_destination" "$1"
    git rev-parse --verify FETCH_HEAD
)
```

```bash
set -eo pipefail
default_tip="$(gherrit_fetch_default refs/heads/main)"
git rebase -i "$default_tip"
# (Edit, squash, or reword commits)
```

Then push again:

```bash
git push
```

GHerrit will detect the changes based on the persistent `gherrit-pr-id` in the
commit trailers and update the corresponding PRs in place. Keep the stack
linear when rebasing.

### Inspecting a stack

```bash
gherrit status --base "$default_tip"
gherrit status --base "$default_tip" --remote --json
```

Local status reports management drift, repository capabilities, hook paths,
and commit problems without authentication or networking. `--remote` adds
current refs, all-lifecycle PR facts, and native membership. Unavailable data
remains unavailable; reports do not repair or publish. See the
[inspection contract](design/inspection.md) for byte representations and limits.

### 4\. Landing a Stack

If you installed the former automatic stack-rebase workflow, remove
`.github/workflows/gherrit-rebase-stack.yml` (or the equivalent workflow under
another name) before landing with this version. GHerrit no longer provides the
`joshlf/gherrit@main` Action, so leaving that workflow enabled will fail after a
PR merge. The manual steps below replace its automatic cascade.

Merge only the root PR, whose base is the repository's default branch. After it
lands, fetch its destination default tip again. Remove the merged local root
from the remaining stack, and publish again. For a repository whose default
branch is `main`, using the fetch helper defined above:

```bash
set -eo pipefail
default_tip="$(gherrit_fetch_default refs/heads/main)"
git rebase --onto "$default_tip" <merged-root-commit>
git push
```

Replace `<merged-root-commit>` with the commit ID of the local root which just
landed. The helper selects the same push destination that GHerrit uses:
`gherrit.remote` when configured and `origin` otherwise. Naming both that
destination and `main` prevents the local branch's upstream or the remote's
fetch URL from selecting a different repository. Rebase against the captured
OID; a local `main` alias may be absent or divergent. Refresh this tip before
each rebase. Repeat from the new root until the stack is landed.

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
of the attempt. A competing different value then rejects the containing atomic
push; a competitor which has already established the exact desired tip can be
acknowledged as current. If the branch was already at the desired tip when
observed, GHerrit performs no redundant write or lease. An independently
written value which GHerrit has already observed is intentionally replaced.
Public mode is therefore suitable for backup and
read-only sharing, not as a bidirectional collaboration branch. Do not update
the remote branch independently or use it as a stable base for independent
work or Pull Requests. Changing a branch to private mode or renaming it does
not delete an earlier public ref.

Public names cannot themselves be valid GHerrit change IDs (1–128 ASCII letters
or digits), or use `gherrit-bases` or a name below it. Exact names such as
`feature`, `stack`, and `Gchange` are reserved because a public update could
otherwise overwrite an unrelated change's head.

Hierarchical names such as `feature/work` and `Gchange/backup` are supported
when the actual stack has no conflicting change ID. The public name also
cannot equal or be a ref-path ancestor or descendant of the repository default
branch. For example, `Gchange/backup` cannot publish a stack containing change
`Gchange`. Names such as `release-candidate` also work.

An unrelated or concurrently created remote branch which is a ref-path
ancestor or descendant can still make the atomic public push fail. Rename the
public branch or use private mode; removal of an independently owned remote
ref requires its owner's decision.

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

Alongside both owned branches, GHerrit publishes a unique annotated tag for every
version of every change. The three refs form one point-in-time tuple:

```text
refs/heads/G                 -> current revision
refs/heads/gherrit-bases/G   -> current revision's literal first parent
refs/tags/gherrit/G/vN       -> publication annotation -> current revision
```

When any member must change, GHerrit publishes the complete tuple atomically.
It never exposes a head-only or head-and-tag update. Immutable version tags
preserve earlier iterations for the **Patch History Table** in each PR body.
The configured push destination, rather than local tags, is the authoritative
version history, so a fresh clone and an older working copy choose the same
next version.

New version annotations carry fresh publication tokens, so repeated revisions
cannot defeat the version-create lease through Git's advertised no-op handling.
Before enabling this writer on existing IDs, drain all old/in-flight publishers
and inspect legacy history for coherence. A lightweight prefix also requires a
validated canonical marker binding its original PR, including a closed or
merged review. Unmarked legacy histories cannot publish; this release does not
seed markers or migrate ownership. Read-only inspection remains available.
These requirements do not make old writers safe or repair prior corruption.
See the
[publication contract](design/pre-push.md#writer-cutover).

#### Publication Barriers and Concurrency

Before writing, GHerrit observes the exact default branch, the optional public
branch, and only the owned Git namespaces and GitHub connections for IDs in
the local stack. It exhausts one OPEN-only GitHub connection per ID. Terminal
rows are not downloaded: an immutable Git marker identifies the canonical
pull request number, and an absent marker-bound OPEN row makes the attempt fail
closed. This keeps network and backend work proportional to the local stack
and currently relevant rows rather than to unrelated repository state. It
validates and plans the complete stack before publishing any prefix.

Before the first network wait, GHerrit captures the logical local branch, its
checked management intent, and its exact `HEAD` object as one attempt input.
Stack collection continues from that retained object. A checkout or branch-ref
move during remote observation therefore cannot relabel another branch's
commits or change which local tip the attempt publishes.

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

Canonical pull request identity uses a separate immutable annotated marker,
`refs/tags/gherrit/G/pr`, which records one pull request number and targets the
first published revision. Without a marker, the lowest visible OPEN number is
only a deterministic contender. GHerrit can construct that contender's marker
after observing it or after receiving an exact create acknowledgement. The
fixed marker ref's absence lease admits only one different tag object. The
acknowledged marker—not number ordering—permanently selects the canonical
pull request.

Every create is a draft on its permanent owned base. If a marker-bound ready
root will become a nonroot, GHerrit first converts it to draft and requires an
exact acknowledgement before any Git write. The marker is a later Git barrier:
final GitHub projection is unavailable until its push is acknowledged. After
that barrier, GHerrit closes every other visible OPEN pull request regardless
of number and applies the desired base, title, body, and navigation only to the
marker-bound canonical row. GHerrit never marks a pull request ready
automatically.

A crash or lost acknowledgement can therefore leave only safe states—a draft
conversion, a complete old or new tuple, a public branch at its prior or
desired tip, a draft pull request on its permanent owned base, or a completed
marker. GHerrit does not roll effects back or retry ambiguous writes within
the same attempt; the next invocation reconstructs authority from Git and
exact local OPEN observations.

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

*Updated by an AI agent acting on joshlf's behalf.*

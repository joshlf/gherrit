# GHerrit

GHerrit is a Git plugin that allows GitHub to support the stacked PRs feature supported natively by [Gerrit](https://www.gerritcodereview.com/).

*GHerrit is extremely alpha. You're welcome to use it, but it has many sharp edges and a lot of missing functionality!*

## Installation

Use `cargo install` to build and install the `gherrit` tool. Install the Git hooks in the `hooks` directory.

## Workflow

GHerrit's intended workflow is the same as Gerrit's. In particular, each Git commit will correspond to a single GitHub PR.
PRs are updated not by *adding* commits, but instead by *modifying* existing commits (using `git commit --amend`, `git rebase`, or some similar workflow).
When commits are pushed, GHerrit's `pre-push` hook automatically generates a PR for each commit. It uses the PR's description to
display useful information about the stack of PRs:

![Screenshot 2025-04-17 at 10 21 01 AM](https://github.com/user-attachments/assets/d6a9c953-593f-4226-88fa-9f640b442bef)

Locally, this stack of PRs is a stack of commits:

```
$ git log
commit 68989f9215b4f2a005cef45f60de8214bc06123d (HEAD -> test, origin/test, origin/Ia1d35fae853121c027b3b21fcade62f874af6313)
Author: Joshua Liebow-Feeser <hello@joshlf.com>
Date:   Thu Sep 14 00:11:18 2023 -0700

    commit 3
    
    commit 3 body
    
    gherrit-pr-id: Ia1d35fae853121c027b3b21fcade62f874af6313

commit 08b688f8c4d9fd91719a912adf42fd6de380c6c0 (origin/I401a8a1d2f8a1cd68cd3525d1f8ed76d1b4eab78)
Author: Joshua Liebow-Feeser <hello@joshlf.com>
Date:   Thu Sep 14 00:11:14 2023 -0700

    commit 2
    
    commit 2 body
    
    gherrit-pr-id: I401a8a1d2f8a1cd68cd3525d1f8ed76d1b4eab78

commit 08d418ead812febb3761a3592cb9b664519d960c (origin/I3612fc7166fba8361ff3ba1b33f800a5d899a560)
Author: Joshua Liebow-Feeser <hello@joshlf.com>
Date:   Thu Sep 14 00:10:54 2023 -0700

    commit 1
    
    commit 1 body
    
    gherrit-pr-id: I3612fc7166fba8361ff3ba1b33f800a5d899a560
```

This workflow allows you to work on multiple dependent features simultaneously. The work product
is the *sequence* of commits. This work product is edited over time until some prefix of the sequence
of commits is ready to be merged. For example, imagine the following commits (listed most recent to least recent,
in the style of `git log`):

- C - `debug: Optimize live debugging of TCP sockets`
- B - `debug: Support live debugging of TCP sockets`
- A - `transport layer: Support listing open TCP sockets`

In this example, commit A adds an API to the transport layer subsystem. This API is then consumed by commit B.
Finally, commit C optimizes the machinery introduced in commit B.

Now imagine that we realize that the API added in commit A doesn't quite fit our needs, and we need to change
it in some way. We use `git rebase -i` to edit commit A directly to make the needed changes, and to update
commits B and C to make use of these changes. We only merge the PR for commit A once we are confident - based
on our experience with commits B and C - that it is in its final, polished state.

## Implementation

The `commit-msg` hook will automatically generate a line like the following at the end of every commit message:

```text
gherrit-pr-id: Ia1d35fae853121c027b3b21fcade62f874af6313
```

This is used to uniquely and stably identify the commit over time, even if the commit is edited (which changes its Git commit ID).

On `git push`, the `pre-push` hook walks the commits which have been added in the current branch relative
to the `main` branch (eventually, we'll make this configurable). It pushes each commit to a GitHub branch with the
same name as its GHerrit PR ID (`Ia1d35fae853121c027b3b21fcade62f874af6313` in the example above). Finally,
it uses the `gh` CLI tool (eventually, we'll switch to using GitHub's API directly) to create a PR which
requests to merge this branch into the corresponding branch of the parent commit.

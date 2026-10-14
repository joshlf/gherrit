# Pre-push publication

GHerrit projects one local first-parent commit stack into remote Git refs and
GitHub pull requests. Git stores submitted revisions and canonical PR identity;
GitHub stores their review projection. This is the owned-base protocol selected
by this implementation. It does not establish GitHub native-stack landing or
interruptible adoption safety.

## Operating envelope

Every writer of managed heads, bases, archives, canonical markers and PR
projections must follow this protocol. The independent cascade Action is
removed. Assume a fixed repository default branch name and tip during an
attempt, direct GitHub head refs, immutable repository-unique PR numbers, and
trusted destination-bound Git transport. Local ancestry must be complete after
explicit bounded acquisition. Relevant GitHub observations must be complete.

The PR lifecycle, head repository/name, readiness, auto-merge and queue state
must not independently change between observation and mutation. In particular, a
returned draft/base state must describe the service's current state. An extra
read cannot fence an actor which bypasses this envelope. Top-level GraphQL
aliases may apply in any subset, but each alias is one indivisible PR operation.
Durable effects must eventually become stably visible for convergence.

These are assumptions, not guarantees supplied by this client. No global stack
manifest or active-change universe is maintained. Unrelated IDs are not admitted
into the plan. Concurrent default movement, landing, manual namespace edits,
ownership migration and garbage collection require separate designs.

## Local intent and destination

Capture the attached branch, its exact tip and management intent before remote
work. Managed pushes use a loopback destination and must contain no enclosing
Git ref update. The hook validates argv and stdin; private internal pushes carry
a recursion capability bound to this worktree's Git directory. An enclosing
`--dry-run` does not make the hook's own publication dry-run.

Resolve exactly one push URL after Git configuration rewriting, bind its GitHub
coordinates, and retain that destination for reads, acquisition and writes.
Unsupported or ambiguous routing fails before credential lookup. Adding the
private URL must preserve the checked transport context. Known settings for
unused remotes can make names unavailable; newly active global or unrecognized
settings stop resolution before transport. Preserve native paths and intentional
command-scope Git configuration; prevent inherited repository selectors from
redirecting bound children. Credentials and sensitive URLs must not appear in
diagnostics. Child processes share the bounded supervisor in `src/subprocess/`;
its platform limitations are in [the subprocess
contract](../src/subprocess/mod.rs).

Derive changes from the captured tip to the observed remote default OID along
literal first parents. A stale or absent redundant local default alias is not a
reason to reject a valid stack. Reject missing/ambiguous raw IDs, repeated IDs,
autosquash work, invalid titles, unsupported graph state and incomplete
ancestry. IDs are bounded ASCII alphanumeric ref components. The commit hook and
collector share raw syntax; the writer verifies newly inserted identity bytes.

## Durable representation

For change G, the current tuple is:

```text
refs/heads/G                 -> submitted commit H
refs/heads/gherrit-bases/G   -> H's literal first parent P
refs/tags/gherrit/G/vN       -> unique annotated publication object -> H
refs/tags/gherrit/G/pr       -> immutable canonical PR annotation
```

A tuple publishes head, owned base and next version together in one atomic Git
push. Version numbers are canonical contiguous positive integers allocated from
the destination, with checked overflow. Local tags never allocate versions. The
same H after an intervening revision occupies a new version position. An exact
already-observed tuple needs no new version.

Each prepared tuple creates one canonical annotated tag with a fresh random
256-bit nonce. Its exact object OID is retained through materialization, refspec
construction and receipt validation. Its bounded payload binds ID, version and
submitted commit (`version_object.rs`). Direct and peeled OIDs are separate
evidence; exact canonical bytes must match the peeled revision.

This distinction is necessary: Git can omit an advertised equal-OID update
before checking its old-value/absence lease. With lightweight versions, a
delayed A→B/v2 attempt can resume after another publisher performs A→B/v2→A/v3
and incorrectly restore head B while latest v3 remains A. A unique annotation
makes the delayed attempt's fixed v2 create conflict instead of disappearing as
an equal-OID no-op. This depends on nonce uniqueness and **one transmission of
each prepared action**. Never resend that token-bearing action through a fresh
advertisement. A fresh invocation reobserves and prepares a new attempt.

History admission checks all relevant refs, immutable contiguous archives,
canonical annotation bytes, current head/latest revision agreement, literal
first parents and required ancestry across historical revisions. Legacy
lightweight prefixes remain readable. Once annotations appear, a lightweight
suffix is rejected. This compatibility reader does not make old writers safe.

### Writer cutover

Drain every old/in-flight publisher before enabling annotated-version writers.
Old writers can still publish unsafe lightweight transactions or reject the new
representation. Do not roll back a writer into this namespace. Inspect and
repair any already inconsistent legacy history before cutover; the new reader
rejects incoherent state and does not silently reconstruct or rewrite archives.
Existing version refs and canonical markers never move or get deleted
automatically.

## Git and GitHub observation

Observe exact destination refs and fully paginated OPEN PR connections for each
local ID. Tail matches, peeled rows, cross-repository rows, missing fields and
truncation are transport facts, not authority. Keep raw decoding separate from
history and lifecycle admission. Read-only inspection reports facts without
acquiring publication authority.

JSON must contain unique decoded object keys and complete input. Correlate query
aliases, pagination cursors, requested IDs and mutation receipts exactly. HTTP
body, request-size, operation-count, deadline and subprocess bounds are enforced
by their owning adapters. Retry read-only transient failures within their
budget; never replay a transmitted mutation. A missing, malformed, terminal or
lost receipt is indeterminate: preceding effects may have applied.

## Canonical identity and lifecycle

Every missing review is created as a draft on `gherrit-bases/G`. Without a
marker, the lowest visible same-repository OPEN number is a contender, not
authority. Contenders race immutable canonical annotations at the one fixed
`G/pr` ref under an absence lease. The annotation binds the canonical number;
only that number may receive the final projection after the marker barrier.

A marker-bound review missing from the complete OPEN observation stops the
attempt. GHerrit does not replace/reopen a closed canonical review. After marker
publication, close every other visible OPEN review for this ID, regardless of
number. An unusable duplicate or canonical receipt stops subsequent effects.
Stale mutations may change fields on a terminal row before returning CLOSED or
MERGED; stopping afterward does not prove that the write was prevented.

A root targets the default branch; a nonroot targets its owned base. A ready
root which will become a nonroot must be converted to draft and acknowledged
before Git writes. Every observed canonical nonroot on an owned base must
already be draft; a ready owned-base review stops the attempt, even if it is not
moving. Unmarked contenders and duplicates must also be draft. Queue/auto-merge
state which violates owned-base admission blocks the attempt. GHerrit never
automatically marks a PR ready. Navigation is a projection of the local stack
and can be temporarily stale under competing publishers.

## Planning, effects and recovery

The planner consumes validated local/Git/GitHub evidence for one destination.
Execution checks that its supplied repository matches that binding. Preflight
all batches and renderable fields before exposing effects. A plan cannot gain
permission by combining observations from unrelated repositories.

Effects cross acknowledgement barriers in order: draft departures; initial
atomic Git tuples and optional public projection; draft creates; canonical
markers; duplicate closes and canonical projections. The public branch is one
complete final Git unit, may share the final tuple batch, and never becomes an
inter-PR comparison base. It is updated under its exact observed lease when
needed.

Bounded batching preserves indivisible units, not whole-stack transactions. A
rejected or indeterminate effect ends the attempt. Earlier batches may remain
committed. A later invocation starts from new observations and derives remaining
work; it does not resume cached write authority. Under stable intent, visibility
and the operating envelope, fresh attempts converge. This is not serializability
against arbitrary external writers.

## Evidence

[Testing guidance](../agent_docs/testing.md) maps pure contracts, real-Git
witnesses, HTTP arrivals, semantic schedules and installed-hook scenarios. The
external ref model stores current raw refs independently of history, models
advertisement no-ops separately from atomic receive, and asserts coherence after
effects. Bounded exploration is evidence about those schedules, not a proof of
all service behavior or native-stack adoption.

*Updated by an AI agent acting on joshlf's behalf.*

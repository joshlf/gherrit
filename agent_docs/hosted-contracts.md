# Explicit GitHub provider probes

Normal Cargo tests are hermetic. The separate feature-gated executable runs
only when an operator supplies all four required arguments:

```bash
cargo run --features hosted-contracts --bin gherrit-hosted-contracts -- \
  --repository OWNER/DISPOSABLE-REPO --allow-mutations --scenario all \
  --output /absolute/path/new-receipts.jsonl
```

Select `draft-lifecycle`, `native-restack`, `workflow-delivery`, or `all`.
Every scenario creates two run-owned branches/PRs, two commits/trees, and one
native stack. Native restack/all marks the bottom PR ready and requests its
merge into the disposable repository's default branch without bypassing rules.
Branch protection, queue policy, merge-method availability, token permissions,
and installed workflows belong to that target. Choose an authorized disposable
target; the tool never infers one from the current checkout.

The unique `gherrit-contracts/<timestamp-random>/` namespace and newly created
PR/stack acknowledgements are recorded before subsequent mutations. JSONL
intent is flushed and synced before every request; responses, failure outcome,
workflow observations, original heads, and created resource identities follow.
No token or child stderr is written. A lost/malformed mutation receipt stops
the run without automatic replay. A new invocation is a new run, not recovery
of an uncertain prior effect. Inspect its journal and service state first.

Only the journal file is flushed and synced; its parent directory is not
synced. Power loss can therefore lose the fresh journal pathname. A missing
journal never proves that no remote effect occurred: inspect the disposable
repository's service state before another run, and do not infer cleanup
ownership without validated resource receipts.

The run has a 40-request/180-second budget, each child a 30-second/4-MiB cap,
finite prepared stdin, and the production process-group/job cleanup boundary.
Six bounded merge polls distinguish pending/queued from terminal observations.
No visible workflow runs in the bounded read is not proof of missed delivery
or its cause. Preserve event/run evidence alongside workflow configuration
and permission context before interpreting an absence.

Resources remain in the disposable target for inspection. There is no pattern
cleanup, deletion, adoption, or mutation of preexisting PRs. Do not run this
tool in an ordinary contribution repository. No live mutation probe was run
as part of this cleanup's validation; CLI authorization/budget contracts and
compilation are hermetic, and the existing supervisor owns lifecycle tests.

*Updated by an AI agent acting on joshlf's behalf.*

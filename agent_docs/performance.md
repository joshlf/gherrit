# Deterministic performance contracts

Run the representative workloads independently of wall-clock benchmarks:

```bash
cargo test --lib --all-features --locked performance_contract -- --nocapture
```

Each workload prints a `GHERRIT_PERFORMANCE` JSON record for reproducible
counts and bytes. No timing threshold changes behavior or makes CI dependent
on machine load.

The fresh-stack workloads cover 1, 8, 32, and 64 changes. They require exactly
three tuple refs, one create alias, one marker, and one final projection alias
per change. They record the production driver's Git batches, mutation request
boundaries, total projected body bytes, and maximum body bytes. Every body
remains within the 128-KiB projection limit. An immediately fresh converged
retry performs zero effects. These are execution counts in the semantic model,
not physical HTTP or OS process counts.

The history workloads cover 1, 8, 64, and 256 distinct sibling revisions,
sixteen repeated root positions, one common ancestor, and two immutable version
positions per revision. Retained commit records equal distinct revisions plus
the common ancestor; retained parent edges equal distinct revisions. History
positions remain distinct even when OIDs repeat. These evidence counts are not
allocator peak-heap measurements.

Provider and transport counts have separate owners: GitHub observation tests
bound raw rows by `N + 99`, pages by `2N + 99`, and exact per-head cursors;
request/argv tests own byte and alias budgets. The complete-process unrelated
PR witness adds 512 unrequested heads without changing requests, operations,
or final state. The semantic model deliberately makes no network-count claim.

Before adding a cache or parallel effect, retain these contracts and measure
the affected physical adapter separately. Keep point-in-time timing data
separate from deterministic counts and from publication's safety assumptions.

*Updated by an AI agent acting on joshlf's behalf.*

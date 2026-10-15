# Resource policy owners

These are separate policies, even where their current values happen to match.
Keep streaming admission and aggregate accounting with the owning operation.
A formatter or a subprocess cap cannot replace a caller's aggregate budget.

| Owner | Policy and unit |
| --- | --- |
| `src/hosted.rs` | Factual reads: 4 MiB per response, 16 MiB and 10,000 rows per observation; 32 pages and 1,000 rows per head connection |
| `src/hosted_contracts.rs` | Explicit probe: 40 requests, 180 seconds per run; each child gets at most 30 seconds and 4 MiB stdout; polling fits the remaining run deadline |
| `src/pre_push/inspection.rs` | Read-only refs: 64 MiB aggregate output, 100,000 selected refs; 16 KiB argv batches |
| `src/inspect.rs` | At most 1,000 displayed commits; ancestry identity scan separately stops after 10,000 commits |
| `src/pre_push/local.rs` | PR title: nonempty, at most 256 Unicode scalars; this is not a UTF-8 byte limit |
| `src/pre_push/publication_attempt/body.rs` | Generated body: at most 131,072 UTF-8 bytes, including rendered/escaped history |
| `src/pre_push/publication_attempt/remote.rs` | Ref query: 16 KiB variable argv; an ID's patterns remain indivisible |
| `src/pre_push/publication_attempt/refs.rs` | Atomic push: 16 KiB variable argv; tuple/public units remain indivisible |
| `src/pre_push/publication_attempt/github/mod.rs` | Mutation request: 64 aliases and 1 MiB serialized bytes |
| `src/pre_push/publication_attempt/github/transport.rs` | Query: 256 KiB request, 64 MiB response; mutation: 4 MiB response; error envelope: 64 KiB |
| `src/pre_push/publication_attempt/github/transport.rs` | Connect 10 seconds; reads/writes 30 seconds; mutation attempt 60 seconds; complete observation 10 minutes; read-only retry delays 100/200/400 ms |
| `src/subprocess/mod.rs` | Remote Git: 120 seconds and 64 MiB stdout; regular-file stdin: 64 MiB; retained stderr: 16 KiB; cleanup: 5 seconds with a 1-second supervisor grace |
| `src/diagnostic.rs` | Human detail: at most 160 escaped bytes before an optional ellipsis; the ellipsis is outside the cap |
| `src/pre_push/publication_attempt/github/observation.rs` | Observation-specific human detail: 80 escaped bytes, separately bounded identity enumeration |

Limits are tested at their owning boundaries. For example, body tests count
UTF-8 bytes, title tests count scalars, transport tests exercise body admission,
and subprocess tests exercise streaming overflow and cleanup. These values do
not establish a whole-stack transaction or a provider-wide service limit.

*Authored by an AI agent acting on joshlf's behalf.*

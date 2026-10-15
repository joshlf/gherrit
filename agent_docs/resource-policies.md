# Resource policies in the current implementation

Limits belong to their operations. Equal values in different rows do not make
those rows one policy. This inventory describes the code on `main`; it does not
add the observation, process-supervision or hosted-probe limits proposed in
outstanding work.

| Owner | Unit and current value | Meaning |
| --- | --- | --- |
| `src/pre_push/mod.rs::read_commit_trailers` | 120 commit arguments per Git invocation | Bounds each trailer-query argument list; output is collected by the command adapter |
| `src/pre_push/publication.rs` | 80 publication targets per push | Leaves command-line headroom for branch and tag refspecs and leases |
| `src/pre_push/publication.rs` | 250 branch arguments per remote query | Separately bounds each `ls-remote` invocation |
| `src/pre_push/batching.rs` | 64 initial GraphQL operations per batch | Rejected batches halve down to a single operation |
| `src/pre_push/batching.rs` | 262,144 bytes of query text | Local heuristic; equality is accepted and larger queries back off |
| `src/pre_push/github.rs` | 100 PR candidates per ID and lifecycle query | A remaining page rejects the observation instead of silently choosing from a prefix |
| `src/pre_push/body.rs` | 131,072 rendered UTF-8 bytes | Switches full history to sparse history above this boundary; it is not a final body truncation limit |
| `src/test_git.rs` | 10 seconds per socket read/write operation | Test-driver Git-interceptor transport only |
| `testutil/src/command.rs` | 60 seconds by default, override per command | Test command execution deadline |
| `testutil/src/command.rs` | 5 seconds for cleanup waits | Descendant termination and output collection have their existing separate waits |
| `testutil/src/lib.rs` | 10 seconds each for server startup/shutdown | Fixture readiness and completion are separate observations |
| `.github/workflows/ci.yml` | 15 minutes on Ubuntu; 25 minutes on Windows | Outer validation job deadlines |

The production command adapter in `src/util/command.rs` uses native process
completion without adding a GHerrit deadline or output-byte cap. This inventory
does not establish a total runtime, memory or request-count bound. Tightening
those contracts would change behavior and needs its own design and validation.

The exact-boundary query/body tests and batch-size tests live with the policies.
The native timeout and server-lifetime tests live with the fixture owners.
Preserve these distinctions when moving code; keep the limits close to the
operation that applies them.

*Authored by an AI agent acting on joshlf's behalf.*

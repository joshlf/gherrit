# Architecture and test ownership

The workflow is a local first-parent commit stack, with one durable identity
per logical change. Commit OIDs identify revisions, Unique annotated version tags preserve publication identity and submitted
revisions, and an annotated marker identifies the canonical PR.
Current hosted heads and native GitHub membership are separate observations.
The [publication contract](pre-push.md) owns effect order, authority,
assumptions, recovery, and concurrency arguments; this map locates code and
tests without restating that contract.

| Responsibility | Implementation | Owning evidence |
| --- | --- | --- |
| Composition and loopback admission | `lib.rs`, `pre_push/mod.rs` | CLI/process entry and exact hook-input policy tests |
| Repository layout, literal graph, local config, credentials | `util/{layout,repository,config,command,auth}.rs` | Real Git layout/config/promisor witnesses and utility unit contracts |
| Identity recognition and message editing | `identity.rs`, `message.rs`, `commit_msg.rs` | Raw grammar/byte generators; installed hook and independent Git cleanup matrix |
| Complete installer inspection and per-file replacement | `install.rs` | Literal shim bytes/modes, binary conflicts, interruption prefixes, rerun/symlink tests |
| Management ownership and complete edits | `manage.rs` | Pure literal plans and real-Git partial-prefix drift/force recovery |
| Exact push repository | `pre_push/destination.rs` | Real fetch/push URL, rewrite, environment, and destination-redaction fixtures |
| Local intent | `pre_push/local.rs`, `pre_push/autosquash.rs` | Captured HEAD, first-parent path, trailer/title/body and autosquash cases |
| Git facts and immutable history admission | `publication_attempt/{remote,history,marker,version}.rs` | Literal OIDs/ref sets, graph adversaries, canonical marker/version codec cases |
| Literal history graph | `src/pre_push/publication_attempt/history/graph.rs` | Mixed missing-root/ancestor provenance, all-parent reachability and promisor witnesses |
| Strict OPEN PR admission | `publication_attempt/github/observation.rs` | Complete exact-head cursors, identity registry, bounded row/page counterexamples |
| Bound production effect execution | `src/pre_push/publication_attempt/plan/execution.rs` | Existing consuming stage and failure-prefix tests |
| Pure decisions and consuming stages | `publication_attempt/plan/` | Independent typed effects and durable semantic-world transitions |
| Production ref spellings | `managed_refs.rs` | Literal wire assertions; independent native/model refs |
| Exact atomic Git acknowledgements | `publication_attempt/refs.rs` | Literal porcelain receipts, atomic rollback, exact leases and malformed/lost output |
| Single-send GitHub effects | `publication_attempt/github/{mutation,transport}.rs` | Literal provider receipts, null/missing/partial effects, no mutation replay |
| Failure classification and safe context | `publication_attempt/failure.rs` | Typed known/indeterminate outcomes, stage/target escaping and bounded display |
| Duplicate-key-safe JSON | `json.rs` | Generated duplicate-key and trailing-input cases |
| Shared bounded HTTP body and diagnostic primitives | `github_http.rs`, `diagnostic.rs` | Body-limit and safe-text transport contracts |
| Child ownership | `subprocess/{mod,unix,windows}.rs` | Platform process-group/job lifetime, terminal, cancellation, bounded cleanup fixtures |
| Factual lifecycle/native reads and inspection | `hosted.rs`, `inspect.rs`, `pre_push/inspection.rs` | Wrong-scope and incomplete-page cases, terminal facts, local read-only CLI/reftable witnesses |
| Factual decoding/transport boundaries | `src/hosted/graphql.rs`, `src/hosted/native.rs`, `src/hosted/read_api.rs` | Existing complete-resource, identity and budget cases |
| Probe journal and receipts | `src/hosted_contracts/journal.rs`, `src/hosted_contracts/receipts.rs` | Durable redaction and exact ownership receipt cases |
| Live provider behavior | `hosted_contracts.rs` (explicit feature/tool) | Disposable target receipts; never normal Cargo test side effects |

`publication_attempt/semantic_oracle/` is a private test tree: durable world,
query visibility, literal fixtures, reports, production consuming driver,
independent assertions, scenarios, and performance counts. Splitting it does
not turn a model result into physical provider evidence. Complete-process
fixtures separately exercise Git and bounded GraphQL transport. Retain these
distinct witnesses when changing publication or adding native landing.

Inspection never repairs config or produces an executable plan. Its terminal
PR facts do not weaken the publisher's OPEN-only checks. The raw identity
parser and legacy normalized-hook policy remain distinct. Config edits and
three hook replacements remain sequential operations; their interruption
contracts do not claim transactionality or cross-process locking.

See [inspection](inspection.md), [subprocess inventory](../agent_docs/subprocesses.md),
[testing](../agent_docs/testing.md), [performance counts](../agent_docs/performance.md),
[provider probes](../agent_docs/hosted-contracts.md), and
[historical issue/PR provenance](../agent_docs/issue-provenance.md).

*Updated by an AI agent acting on joshlf's behalf.*

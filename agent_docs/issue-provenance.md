# Historical issue and PR provenance

The following catalog was captured October 13, 2026 before this cleanup stack:
58 open issues and 57 open PRs (115 items), main at
`86179afb27feabd50ddd7d2dc2493892885db8ba` and the publication draft at
`6714aa3d29754c0968a41c61f86161cd7c548b55`. Thirty-nine PR heads were draft
ancestors; eighteen were alternatives. Bodies/scopes/ancestry and substantive
discussion informed the survey. Linked-only external conversations were not
verified requirements. This is provenance, not a live issue-status claim.

Relevance notes below describe that surveyed baseline. This cleanup has since
fixed default linked-worktree hooks (#176), binary hook conflict handling
(#184), and empty-message cancellation (#406); added status/identity diagnosis
(#92, #198), explicit reftable diagnostics (#354), typed failures/context
(#199, #385), and disposable provider tooling (#90, #189, #390). Other related
items supplied counterexamples for extraction, batching, recovery, or tests;
they are not automatically complete or authorized to close.

The stack preserves protocol decisions: no automatic orphan deletion, native
landing/adoption, restoration of earlier config, new ownership namespaces, or
legacy protocol migration. In particular, provider probe availability does
not resolve #390's missing-workflow cause. No existing issue or PR was closed
or retitled by this work. Refresh remote state before making those decisions.

## Issues

| Issue | Subject | Cleanup relevance |
| --- | --- | --- |
| [#18](https://github.com/joshlf/gherrit/issues/18) | Original notes | Historical product model. Keep logical-change identity separate from commit OIDs; the original v1 patch-set representation is not the current implementation. |
| [#37](https://github.com/joshlf/gherrit/issues/37) | Permit configuring a different target for a given branch | The hardcoded-main description is old; default-branch discovery exists. Arbitrary per-branch trunks remain a feature decision. |
| [#87](https://github.com/joshlf/gherrit/issues/87) | Detect and bail when PR bodies have been manually edited | Body ownership and preservation are behavior changes. Extract rendering and comparison first; do not add an unproved hash/footer protocol. |
| [#88](https://github.com/joshlf/gherrit/issues/88) | Garbage collect phantom branches | Useful for an inventory/dry-run tool. Name syntax and absence of an OPEN PR do not by themselves authorize deletion. |
| [#90](https://github.com/joshlf/gherrit/issues/90) | Add integration tests | Broad historical testing request. Hermetic process tests already exist; improve their assertions and coverage rather than recreate them. |
| [#92](https://github.com/joshlf/gherrit/issues/92) | Gracefully handle managing a previously un-managed branch | Read-only missing-ID preflight and actionable guidance are valuable. Automatically rewriting existing commits requires separate policy. |
| [#113](https://github.com/joshlf/gherrit/issues/113) | Prevent accidentally merging into an interior branch (rather than `main`) | Interior landing protection is behavioral. Draft nonroot draft-state and automation checks already address its owned-base contract. |
| [#122](https://github.com/joshlf/gherrit/issues/122) | Add `gherrit install` | Hook installation already exists. Remote configuration and an Action are additional setup policy; the draft removes its cascade Action. |
| [#127](https://github.com/joshlf/gherrit/issues/127) | Filter local gherrit tags | The pasted analysis is stale for main, which uses local tags in allocation. Do not apply tag-filtering advice without deciding authority and fetch behavior. |
| [#141](https://github.com/joshlf/gherrit/issues/141) | Only store most recent version tag locally | Local retention is a compatibility/authority decision. The draft uses remote history and does not create local version checkpoints. |
| [#161](https://github.com/joshlf/gherrit/issues/161) | Bug: PR automatically closed when it moves location in a stack | Important reorder counterexample, not a cleanup recipe. Proposed pre-targeting does not establish safety against delayed concurrent publishers. |
| [#163](https://github.com/joshlf/gherrit/issues/163) | Implement fine-grained locking using GitHub as the source of truth | Locking requires a new protocol and ownership/recovery model. It is not an automatic simplification of unconditional GitHub writes. |
| [#171](https://github.com/joshlf/gherrit/issues/171) | Support end-to-end testing against live GitHub repositories | Motivates opt-in provider contract tests. The old hardcoded-main and broad live-mode assumptions need updating; normal tests should remain hermetic. |
| [#172](https://github.com/joshlf/gherrit/issues/172) | Improve handling of stale and orphaned PRs | Inspection can show omitted or orphan candidates. Closing changes absent from one local stack requires an abandonment policy. |
| [#173](https://github.com/joshlf/gherrit/issues/173) | Auto-detect and persist base branch for stacked changes | Recursive base inheritance is new persistent behavior. Extract branch/default observations without adopting the heuristic. |
| [#174](https://github.com/joshlf/gherrit/issues/174) | `gh pr list` pagination limit causes sync failures on large stacks | The old gh CLI 30-row limit no longer describes main. Main rejects truncated 100-candidate GraphQL connections; draft pages exact OPEN connections. |
| [#175](https://github.com/joshlf/gherrit/issues/175) | Panic when `gh pr create` fails or returns unexpected output | The old gh-pr-create output parser has been replaced by GraphQL decoding. Preserve its lesson with strict receipts and diagnostic tests. |
| [#176](https://github.com/joshlf/gherrit/issues/176) | `gherrit install` installs hooks to wrong directory in Git worktrees | Still relevant: installer defaults to the per-worktree Git path plus hooks in both baselines. Extract layout and fix with real-Git worktree tests. |
| [#177](https://github.com/joshlf/gherrit/issues/177) | Strict ancestry check blocks pushes when upstream has diverged | Strict default ancestry is an intended constraint. Relaxing it changes supported histories; better diagnostics and tests can preserve behavior. |
| [#178](https://github.com/joshlf/gherrit/issues/178) | `gherrit-pr-id` allows overwriting arbitrary remote branches | Namespace/ref ownership needs exact validation. Draft adds default/public collision protections; renaming all managed heads is a migration decision. |
| [#179](https://github.com/joshlf/gherrit/issues/179) | `collect_commits` drops history when `main` is merged into the stack | The draft collects a first-parent stack from captured intent and checks broader ancestry separately. Preserve those distinct responsibilities. |
| [#181](https://github.com/joshlf/gherrit/issues/181) | `gherrit` breaks if Reflogs are disabled | Reflog dependence remains relevant to branch classification. Characterize disabled/missing reflogs and provide targeted diagnostics before changing heuristics. |
| [#182](https://github.com/joshlf/gherrit/issues/182) | `pre-push` hook hardcodes remote as "origin", ignoring actual push target | Main does not route publication from the enclosing remote arguments. Draft captures invocation and requires direct or exact empty loopback pushes. |
| [#183](https://github.com/joshlf/gherrit/issues/183) | `commit-msg` hook fails to add trailer if message body mentions "gherrit-pr-id" | The historical substring implementation has been replaced. Hook normalization still differs from the draft raw-footer parser; characterize and extract shared syntax. |
| [#184](https://github.com/joshlf/gherrit/issues/184) | `gherrit install` crashes on binary or non-UTF-8 hook files | Still relevant: binary hooks cause a UTF-8 read error before conflict/force handling. Byte inspection and a separate install plan are small hardening work. |
| [#189](https://github.com/joshlf/gherrit/issues/189) | In local testing, use a filesystem-local git "remote" so that we can test against real `git push`es instead of mocking `git` | Real filesystem-local Git remotes are already used. Retain those tests alongside semantic and intercepted protocol tests. |
| [#191](https://github.com/joshlf/gherrit/issues/191) | Weak test assertions | Partly addressed: generated-ID tests now validate base32 length/alphabet. Installer sentinel/existence checks are still weak. Suggested hex regex is obsolete. |
| [#196](https://github.com/joshlf/gherrit/issues/196) | Improve mental model and terminal UI/UX | Link-only UX brainstorming; inaccessible conversation contents were not assumed. Supports a clearer concept map and operation reporting. |
| [#198](https://github.com/joshlf/gherrit/issues/198) | Feature: `gherrit status` command to visualize branch state and stack depth | A read-only status command is an additive feature with strong reuse value. Extract observations first; local status should not imply remote knowledge. |
| [#199](https://github.com/joshlf/gherrit/issues/199) | UX: Abstract away "Phantom Branch" terminology in user-facing output and documentation | Simpler explanations and logical-change context are useful. Preserve exact refs for diagnostics rather than globally rewriting stderr. |
| [#202](https://github.com/joshlf/gherrit/issues/202) | UX: Proactively suggest "Stack Mode" when pushing multiple commits on an unmanaged branch | Auto-management and interactive prompting change push intent. An advisory report can be separated from that feature decision. |
| [#203](https://github.com/joshlf/gherrit/issues/203) | Feature: `gherrit next` and `gherrit prev` for local stack navigation | Navigation commands are features. Shared stack observations help, but checkout behavior and ambiguous child/stack selection need policy. |
| [#205](https://github.com/joshlf/gherrit/issues/205) | Feature: Visualize the PR stack structure in the terminal during `git push` | Structured ordered reporting is additive. The old parallel logging description is historical; report facts and outcomes from the current driver. |
| [#206](https://github.com/joshlf/gherrit/issues/206) | UX: Allow `gherrit` operations in Detached HEAD state when part of a known stack | Detached-head publication needs new intent/authority rules. Read-only inspection is separable; draft deliberately requires captured attached HEAD. |
| [#214](https://github.com/joshlf/gherrit/issues/214) | Bug: Pushing a merged commit creates a duplicate PR instead of failing | The old OPEN-only gh CLI lookup is not main, which checks historical states. Draft authenticates the canonical OPEN PR by immutable marker and fails when it is absent. |
| [#219](https://github.com/joshlf/gherrit/issues/219) | When going from unmanaged -> managed, store existing config values so they can be restored upon later managed -> unmanaged | Restoring prior config requires persistence and ownership rules. Pure transition plans and interruption tests are useful prerequisites. |
| [#227](https://github.com/joshlf/gherrit/issues/227) | Track GHerrit version in `.git/config` | Binary version and persistent format version differ. Add capability diagnostics first; introduce a format marker only for owned data requiring interpretation. |
| [#243](https://github.com/joshlf/gherrit/issues/243) | In PR description, include cloning instructions | Download instructions already exist. Maintain renderer/escaping tests rather than add a second command-generation path. |
| [#244](https://github.com/joshlf/gherrit/issues/244) | Compress patch comparison table over a certain size | Main has a sparse-table fallback; draft proves bounded recipes and renders through a bounded writer. Add metadata scaling workloads and preserve fallbacks. |
| [#262](https://github.com/joshlf/gherrit/issues/262) | `gherrit install` installs a pre-push hook that forwards arguments (`"$@"`) but `gherrit hook pre-push` doesn't expect arguments | Both baselines accept optional paired pre-push arguments now. Keep installed-shim argument-forwarding tests; the historical CLI mismatch is replaced. |
| [#263](https://github.com/joshlf/gherrit/issues/263) | PR UI should indicate which direction PRs stack (ie, that `main` is on the bottom) | Small presentation feature. Existing renderers/order tests can support it without changing topology or identity. |
| [#264](https://github.com/joshlf/gherrit/issues/264) | Overhaul concurrency design | Major concurrency design history and failure evidence. Draft handles a narrower explicit owned-base contract; native landing/adoption still need their own proof. |
| [#268](https://github.com/joshlf/gherrit/issues/268) | Refactor pre-push hook into a pipeline architecture | Pure stages and effect ordering are useful, but much stronger boundaries already exist in the draft. Avoid restarting a generic typestate rewrite. |
| [#272](https://github.com/joshlf/gherrit/issues/272) | Add timestamps to patch table | Timestamp display is a feature, not a refactor. Version records can expose immutable facts separately from rendering policy. |
| [#273](https://github.com/joshlf/gherrit/issues/273) | Support marking PRs as drafts, perhaps via commit message metadata | Commit metadata for draft intent is a feature. Draft-safe publication already has explicit state/receipt checks; do not conflate those contracts. |
| [#279](https://github.com/joshlf/gherrit/issues/279) | When comparing versions, support range diffs to avoid capturing rebasing? | Range-diff has new comparison semantics. Link-only analysis is not a verified specification; rendering extraction and literal fixtures are prerequisites. |
| [#280](https://github.com/joshlf/gherrit/issues/280) | Allow `git status` to detect when upstream is out of date | Changing loopback tracking to affect git status alters management behavior. GHerrit status can report publication state without that change. |
| [#281](https://github.com/joshlf/gherrit/issues/281) | Support parallel execution of merge queue for PRs in the same stack | Parallel merge-queue designs change landing and dependency semantics. Keep separate from cleanup and reassess against native stacks. |
| [#287](https://github.com/joshlf/gherrit/issues/287) | Shrink metadata size | ID encoding is already smaller base32; further encoding changes are compatibility work. Bodies/request scaling remain useful benchmark inputs. |
| [#352](https://github.com/joshlf/gherrit/issues/352) | Collect stacks relative to the remote default branch, not stale local main | Confirmed main stale-local-default boundary failure. Draft observes destination default name/OID and joins it with GitHub and local evidence. |
| [#354](https://github.com/joshlf/gherrit/issues/354) | Support or clearly reject repositories using the reftable backend | Unsupported reftable failures deserve targeted capability diagnostics. Reftable support itself requires dependency/backend work. |
| [#355](https://github.com/joshlf/gherrit/issues/355) | Stack reorder can partially apply and irreversibly mark PRs merged | Concrete irreversible partial reorder effects. Keep as a semantic/service counterexample, not an expectation of cross-service rollback. |
| [#385](https://github.com/joshlf/gherrit/issues/385) | Handle partial GraphQL resource-limit responses idempotently | Concrete partial and ambiguous GraphQL writes. Draft sends mutations once and recovers through fresh invocations; automatic same-attempt retry is a protocol change. |
| [#386](https://github.com/joshlf/gherrit/issues/386) | Avoid transient empty PRs while publishing reordered stacks | Transient empty/merged PR window motivates draft owned-base safety. Native chains need separate transition semantics and provider probes. |
| [#389](https://github.com/joshlf/gherrit/issues/389) | Fail closed when a stale remote HEAD makes a managed stack appear empty | Main trusts cached remote HEAD and can silently find an empty range. Draft observes the destination symbolic default and validates evidence. |
| [#390](https://github.com/joshlf/gherrit/issues/390) | Updating an interior PR can fail to trigger pull_request workflows | Observed absence of workflows; cause remains unknown. Add an opt-in service probe rather than assume the local mock models event delivery. |
| [#405](https://github.com/joshlf/gherrit/issues/405) | Publishing an amended child creates a redundant version for an unchanged parent | Unchanged parent creates a redundant version in main. Draft avoids new versions for unchanged literal revisions; retain this as a regression workload. |
| [#406](https://github.com/joshlf/gherrit/issues/406) | PR ID trailers shouldn't be added to empty commit messages | Still relevant: hook can add identity to an empty message and defeat abort. Fix against real Git cleanup/installed-hook behavior, not only string trimming. |

## Pull requests

“In draft” means the recorded head is an ancestor of draft commit
`6714aa3d29754c0968a41c61f86161cd7c548b55`. No recorded open PR head is
an ancestor of pinned main; equivalent code can still have landed through
another commit. The recorded ancestry classification below was checked from
exact Git objects; it is not a guess from PR titles.

| PR | Subject | In draft | Cleanup relevance |
| --- | --- | --- | --- |
| [#14](https://github.com/joshlf/gherrit/pull/14) | commit 1 | No | Historical fixture PR; current base/head trees have no changes. |
| [#15](https://github.com/joshlf/gherrit/pull/15) | commit 2 | No | Historical fixture PR; current base/head trees have no changes. |
| [#16](https://github.com/joshlf/gherrit/pull/16) | commit 3 | No | Historical fixture PR; current base/head trees have no changes. |
| [#40](https://github.com/joshlf/gherrit/pull/40) | Namespace synthesized GitHub branches | No | Ref namespace feature, requiring identity/compatibility migration. Reuse ownership motivation, not patch directly. |
| [#160](https://github.com/joshlf/gherrit/pull/160) | [pre-push] Delete old, unneeded version tags | No | Old local-tag deletion proposal; description itself questions effectiveness. Main allocation and draft authority differ. |
| [#185](https://github.com/joshlf/gherrit/pull/185) | Support base branches other than main | No | Old alternative-base implementation. Default-branch awareness is already present; arbitrary trunks remain separate. |
| [#188](https://github.com/joshlf/gherrit/pull/188) | [test] Add "live mode" end-to-end GitHub tests | No | Old broad live-mode test proposal. Useful provider-test motivation; do not silently make ordinary tests live. |
| [#190](https://github.com/joshlf/gherrit/pull/190) | [test] Be more precise in testing commit state | No | Independent Git commit/ref expectations are useful. Current tests already include real remotes and stronger oracles. |
| [#267](https://github.com/joshlf/gherrit/pull/267) | [WIP] Pre-push hook pipeline architecture | No | Old WIP typestate wrapper. Reuse separation motivation; draft has a more complete private consuming plan. |
| [#270](https://github.com/joshlf/gherrit/pull/270) | [pre-push] Factor out stack discovery | No | Small discovery extraction precedent. Preserve mechanical/semantic review separation. |
| [#304](https://github.com/joshlf/gherrit/pull/304) | [pre-push] Extract PR reconciliation model | No | Pure reconciliation precedent; explicitly superseded by 308 in its discussion. Related model exists on main despite no head ancestry. |
| [#346](https://github.com/joshlf/gherrit/pull/346) | [pre-push] Make PR projection a pure action plan | No | Earlier pure next-step planner. Draft replaces it with private consuming stages and exact evidence. |
| [#347](https://github.com/joshlf/gherrit/pull/347) | [pre-push] Reobserve ambiguous GraphQL writes | No | Earlier same-invocation reobservation/retry design. Incompatible with draft send-once recovery contract; do not port as cleanup. |
| [#348](https://github.com/joshlf/gherrit/pull/348) | [ci] Compile documentation examples | No | Earlier doctest/strict Bash variant; equivalent work is part of draft PR360. |
| [#351](https://github.com/joshlf/gherrit/pull/351) | Temporary transport for safe-pr-reorder review fixes | No | Temporary transport and publishing scaffolding; explicitly not intended for merge. Exclude from product cleanup proposals. |
| [#358](https://github.com/joshlf/gherrit/pull/358) | [pre-push] Make Git publication resumable | No | Earlier remote-history/checkpoint recovery alternative. Valuable counterexamples; do not restore reobservations/local checkpoint authority into the draft. |
| [#359](https://github.com/joshlf/gherrit/pull/359) | [docs] Specify pre-push publication | Yes | Publication contract and design documentation. Preserve assumptions and link obligations to test owners. |
| [#360](https://github.com/joshlf/gherrit/pull/360) | [ci] Compile documentation examples | Yes | Doctests and explicit Bash failure behavior. Already in draft; use executable validation source to reduce drift. |
| [#361](https://github.com/joshlf/gherrit/pull/361) | [action] Remove automatic stack rebasing | Yes | Removes cascade writer and body footer. Behavior-changing protocol slice, not an independent mechanical cleanup. |
| [#362](https://github.com/joshlf/gherrit/pull/362) | [pre-push] Seal literal local publication intent | Yes | Sealed literal intent and raw identity parsing. Shared identity-module extraction remains useful. |
| [#363](https://github.com/joshlf/gherrit/pull/363) | [pre-push] Bound GraphQL mutation transport | Yes | Bounded mutation transport and exact receipts. Retain read/write retry separation. |
| [#365](https://github.com/joshlf/gherrit/pull/365) | [pre-push] Bind publication to one bounded destination | Yes | Destination and bounded process binding. Reuse proven low-level boundaries without flattening their authority distinctions. |
| [#364](https://github.com/joshlf/gherrit/pull/364) | [pre-push] Disable implicit object acquisition | Yes | Disables implicit object acquisition. Keep capability/error checks close to literal repository operations. |
| [#366](https://github.com/joshlf/gherrit/pull/366) | [pre-push] Observe exact local Git refs | Yes | Exact-local Git-ref decoding and absence evidence. Useful codec/size-bound fuzz targets. |
| [#367](https://github.com/joshlf/gherrit/pull/367) | [pre-push] Decode exact local pull requests | Yes | Exact-local PR decoding. General factual readers must preserve pagination proof and publisher admission policy separately. |
| [#368](https://github.com/joshlf/gherrit/pull/368) | [pre-push] Extract local stack module | Yes | Move-only local module extraction. Good precedent for reviewable mechanical commits. |
| [#370](https://github.com/joshlf/gherrit/pull/370) | [pre-push] Bound remote Git command input | Yes | Prepared bounded subprocess input. Preserve platform cleanup and finite-input contract. |
| [#369](https://github.com/joshlf/gherrit/pull/369) | [pre-push] Require exact Git push receipts | Yes | Atomic units and exact Git receipts. Preserve exact tests and indivisible tuple semantics in refactors. |
| [#371](https://github.com/joshlf/gherrit/pull/371) | [pre-push] Correlate pull request identity evidence | Yes | Request-bound PR identity accumulation. Useful pagination/cursor/JSON fuzzing targets. |
| [#373](https://github.com/joshlf/gherrit/pull/373) | [pre-push] Validate complete change histories | Yes | Complete history graph validation and caching. Benchmark shared ancestry/repeated targets before changing traversal. |
| [#372](https://github.com/joshlf/gherrit/pull/372) | [pre-push] Normalize literal publication history | Yes | Literal publication-history normalization. Keep archived revision validity distinct from current native outcomes. |
| [#374](https://github.com/joshlf/gherrit/pull/374) | [pre-push] Bind remote history acquisition | Yes | Bound history acquisition to observed aliases and destination. Retain exact fetch side-effect tests. |
| [#376](https://github.com/joshlf/gherrit/pull/376) | [pre-push] Expose validated history evidence | Yes | Encapsulated validated history evidence. Preserve its narrow construction boundary. |
| [#375](https://github.com/joshlf/gherrit/pull/375) | [test] Exercise bounded remote Git commands | Yes | Real-Git acquisition/observation tests. Already covers valuable independent witnesses. |
| [#378](https://github.com/joshlf/gherrit/pull/378) | [pre-push] Freeze bounded pull-request body recipes | Yes | Bounded frozen body recipes. Already addresses much metadata scaling; preserve data escaping and immutable links. |
| [#377](https://github.com/joshlf/gherrit/pull/377) | [pre-push] Prepare strict GitHub publication transport | Yes | Strict GitHub observation and mutation transport. Extract diagnostics/outcome types without relaxing receipt requirements. |
| [#379](https://github.com/joshlf/gherrit/pull/379) | [pre-push] Plan owned-base publication with exact evidence | Yes | Owned-base pure planner and early preflight. Preserve consuming barriers; do not equate native chains with owned-base tuples. |
| [#381](https://github.com/joshlf/gherrit/pull/381) | [pre-push] Encapsulate one publication attempt | Yes | Private whole-attempt boundary. Avoid a new public API that lets callers recombine evidence and effects. |
| [#380](https://github.com/joshlf/gherrit/pull/380) | [pre-push] Bind Git publication to its destination | Yes | Repository/destination-bound effects. Explicit layout operations should reuse this path/identity distinction. |
| [#382](https://github.com/joshlf/gherrit/pull/382) | [test] Prove publication effect barriers | Yes | Exact effect-barrier tests. Retain complete payload traces when reorganizing fixtures. |
| [#383](https://github.com/joshlf/gherrit/pull/383) | [test] Model owned-base restart checkpoints | Yes | Literal durable-state restart model. Organize it into world, visibility, driver, reports, and scenarios. |
| [#384](https://github.com/joshlf/gherrit/pull/384) | [pre-push] Prepare exact-local publication | Yes | Captured HEAD/intent, public projection, coupled observation. Status needs observations without effect authority. |
| [#387](https://github.com/joshlf/gherrit/pull/387) | [test] Exercise concurrent publication schedules | Yes | Concurrent publisher schedule tests. Preserve actual request/batch boundaries and supported scope. |
| [#391](https://github.com/joshlf/gherrit/pull/391) | [test] Exercise terminal update receipts | Yes | Terminal update receipt rejection tests. Useful independent error/outcome classification cases. |
| [#392](https://github.com/joshlf/gherrit/pull/392) | [pre-push] Observe only open pull requests | Yes | OPEN-only publisher observation. Status/native readers need their own factual scope, not relaxed publisher checks. |
| [#393](https://github.com/joshlf/gherrit/pull/393) | [pre-push] Repair duplicate pull requests | Yes | Marker-authorized duplicate repair. Automatic orphan cleanup is a different policy. |
| [#394](https://github.com/joshlf/gherrit/pull/394) | [test] Model duplicate pull request recovery | Yes | Duplicate/restart semantic model. Preserve independent durable rows and marker authority. |
| [#395](https://github.com/joshlf/gherrit/pull/395) | [pre-push] Localize exact mutation limits | Yes | Localizes mutation limits. Already eliminates this legacy dependency; do not repeat it as a new task. |
| [#396](https://github.com/joshlf/gherrit/pull/396) | [pre-push] Drop partial diagnostic lines | Yes | Drops partial retained diagnostic lines. Preserve confidentiality and byte accounting in output refactors. |
| [#397](https://github.com/joshlf/gherrit/pull/397) | [pre-push] Activate exact-local publication | Yes | Activates draft and removes legacy runtime. Substantial behavior change; remaining legacy public-config repair is intentional compatibility. |
| [#398](https://github.com/joshlf/gherrit/pull/398) | [pre-push] Authenticate canonical pull requests | No | Earlier canonical-PR variant; discussion explicitly supersedes it with 400. Not an additional patch to land. |
| [#399](https://github.com/joshlf/gherrit/pull/399) | [test] Document the publication test architecture | Yes | Test architecture documentation. Already in draft; use as the ownership map for suite organization. |
| [#400](https://github.com/joshlf/gherrit/pull/400) | [pre-push] Enforce draft-safe publication | Yes | Draft-safety barrier. Preserve exact conversion/create receipts and planner admission rules. |
| [#401](https://github.com/joshlf/gherrit/pull/401) | [test] Exercise publication acknowledgement recovery | No | Earlier acknowledgement-recovery test variant, not a draft ancestor. Related coverage is already integrated at draft tip. |
| [#402](https://github.com/joshlf/gherrit/pull/402) | [pre-push] Encode canonical pull request markers | Yes | Canonical marker byte grammar. Suitable codec boundaries; marker representation is protocol, not disposable scaffolding. |
| [#403](https://github.com/joshlf/gherrit/pull/403) | [test] Bound low-level fixture commands | Yes | Bounds low-level fixture subprocesses. Production local/auth subprocess audit is still a distinct opportunity. |
| [#404](https://github.com/joshlf/gherrit/pull/404) | [test] Prove publication observation bounds | Yes | Proves concurrent observation and unrelated-PR independence. Extend measured workloads rather than remove these tests. |

## Coverage

The survey read all fetched item descriptions and changed-file scopes, checked
PR ancestry, indexed 64 discussion timelines, and examined selected substantive
discussions and code. It was not a line-by-line review of all 1,796 timeline
entries or every historical revision. The linked items preserve provenance;
their live state can differ from this dated catalog.

*Updated by an AI agent acting on joshlf's behalf.*

## Immutable survey revisions

The [surveyed commit inventory](surveyed-pr-heads.json) records each observed
PR head, base, merge base and ancestry classification against the named main
and draft tips. These are the revisions behind this dated survey; later
changes to the linked PR branches do not alter that evidence.

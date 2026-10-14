# Reviewing the restructured stack

Reviews 1–20 are shared foundations and active-workflow fixes. Review 21 is
dated historical provenance. The owned-base protocol decision begins at 22;
reviews 23–27 assemble its codecs, history, checked effects, consuming planner
and faithful recovery model. Review 28 activates the completed engine and
removes the legacy path. This index is the final documentation-only review.

The early management change preserves the old public push policy; activation
changes that policy together with its publisher. The CI foundation retains
Action tests until the explicit Action-removal review. Shared HTTP/process
primitives and identity syntax are reused by the selected engine.

Every replacement review has a fresh `gherrit-pr-id`. Old review branches and
archives remain historical evidence; they must not be republished in the new
order with their old identities. A nonce-bearing version archive is a separate
publication identity and peels to its submitted commit. See the
[protocol and writer-cutover contract](pre-push.md), [test evidence](../agent_docs/testing.md),
and [architecture map](architecture.md).

The map includes the 44 retained source reviews and their 20 previously
absorbed ancestors; a source can contribute to more than one replacement.

| Order | Change | Earlier reviews represented |
| --- | --- | --- |
| 1 | [test] Verify complete installed hook contracts | [407](https://github.com/joshlf/gherrit/pull/407) |
| 2 | [cleanup] Remove the unused Rayon startup pool | [408](https://github.com/joshlf/gherrit/pull/408) |
| 3 | [fix] Prepare complete installation in the common Git directory | [411](https://github.com/joshlf/gherrit/pull/411), [412](https://github.com/joshlf/gherrit/pull/412), [416](https://github.com/joshlf/gherrit/pull/416), [417](https://github.com/joshlf/gherrit/pull/417) |
| 4 | [ci] Share validation commands and compile documentation examples | [360](https://github.com/joshlf/gherrit/pull/360), [409](https://github.com/joshlf/gherrit/pull/409) |
| 5 | [fix] Preserve native message paths and existing identities | [413](https://github.com/joshlf/gherrit/pull/413), [414](https://github.com/joshlf/gherrit/pull/414), [415](https://github.com/joshlf/gherrit/pull/415), [418](https://github.com/joshlf/gherrit/pull/418) |
| 6 | [refactor] Separate repository, config, command and auth utilities | [419](https://github.com/joshlf/gherrit/pull/419) |
| 7 | [process] Bound child lifetimes and own their cleanup | [365](https://github.com/joshlf/gherrit/pull/365), [426](https://github.com/joshlf/gherrit/pull/426), [370](https://github.com/joshlf/gherrit/pull/370), [396](https://github.com/joshlf/gherrit/pull/396), [410](https://github.com/joshlf/gherrit/pull/410) |
| 8 | [test] Record raw arrivals and model actual Git ref commands | [403](https://github.com/joshlf/gherrit/pull/403), [383](https://github.com/joshlf/gherrit/pull/383), [387](https://github.com/joshlf/gherrit/pull/387) |
| 9 | [fix] Preserve Git cancellation and write canonical identities | [413](https://github.com/joshlf/gherrit/pull/413), [362](https://github.com/joshlf/gherrit/pull/362), [368](https://github.com/joshlf/gherrit/pull/368), [414](https://github.com/joshlf/gherrit/pull/414), [415](https://github.com/joshlf/gherrit/pull/415), [418](https://github.com/joshlf/gherrit/pull/418) |
| 10 | [cleanup] Remove the independent cascade Action writer | [361](https://github.com/joshlf/gherrit/pull/361) |
| 11 | [pre-push] Collect literal first-parent changes with checked identities | [362](https://github.com/joshlf/gherrit/pull/362), [368](https://github.com/joshlf/gherrit/pull/368) |
| 12 | [graphql] Decode unique JSON and bound mutation acknowledgements | [363](https://github.com/joshlf/gherrit/pull/363), [377](https://github.com/joshlf/gherrit/pull/377), [423](https://github.com/joshlf/gherrit/pull/423), [425](https://github.com/joshlf/gherrit/pull/425), [391](https://github.com/joshlf/gherrit/pull/391), [395](https://github.com/joshlf/gherrit/pull/395), [410](https://github.com/joshlf/gherrit/pull/410), [429](https://github.com/joshlf/gherrit/pull/429) |
| 13 | [repository] Make local graph facts literal and explicit | [364](https://github.com/joshlf/gherrit/pull/364) |
| 14 | [test] Compile explicit journaled probes for disposable GitHub targets | [430](https://github.com/joshlf/gherrit/pull/430) |
| 15 | [git] Bind publication reads and writes to one resolved push repository | [365](https://github.com/joshlf/gherrit/pull/365), [426](https://github.com/joshlf/gherrit/pull/426), [370](https://github.com/joshlf/gherrit/pull/370), [396](https://github.com/joshlf/gherrit/pull/396), [410](https://github.com/joshlf/gherrit/pull/410) |
| 16 | [git] Accept GitHub SSH URLs with the literal git user | [365](https://github.com/joshlf/gherrit/pull/365), [370](https://github.com/joshlf/gherrit/pull/370), [396](https://github.com/joshlf/gherrit/pull/396), [410](https://github.com/joshlf/gherrit/pull/410) |
| 17 | [pre-push] Anchor local collection at the observed remote default | [362](https://github.com/joshlf/gherrit/pull/362), [365](https://github.com/joshlf/gherrit/pull/365), [368](https://github.com/joshlf/gherrit/pull/368), [370](https://github.com/joshlf/gherrit/pull/370), [396](https://github.com/joshlf/gherrit/pull/396), [410](https://github.com/joshlf/gherrit/pull/410) |
| 18 | [manage] Observe complete local config and prepare every edit before writing | [420](https://github.com/joshlf/gherrit/pull/420), [362](https://github.com/joshlf/gherrit/pull/362), [368](https://github.com/joshlf/gherrit/pull/368), [421](https://github.com/joshlf/gherrit/pull/421) |
| 19 | [inspect] Report local repository facts without publication authority | [427](https://github.com/joshlf/gherrit/pull/427) |
| 20 | [inspect] Observe exact remote refs and factual hosted state without planning | [428](https://github.com/joshlf/gherrit/pull/428) |
| 21 | [docs] Preserve the dated issue and PR cleanup provenance | [431](https://github.com/joshlf/gherrit/pull/431) |
| 22 | [design] Specify nonce-fenced owned-base publication and its limits | [359](https://github.com/joshlf/gherrit/pull/359) |
| 23 | [publication] Distinguish unique publication objects from revisions | [366](https://github.com/joshlf/gherrit/pull/366), [402](https://github.com/joshlf/gherrit/pull/402), [423](https://github.com/joshlf/gherrit/pull/423) |
| 24 | [publication] Admit complete literal histories with unique version objects | [366](https://github.com/joshlf/gherrit/pull/366), [372](https://github.com/joshlf/gherrit/pull/372), [373](https://github.com/joshlf/gherrit/pull/373), [374](https://github.com/joshlf/gherrit/pull/374), [375](https://github.com/joshlf/gherrit/pull/375), [376](https://github.com/joshlf/gherrit/pull/376) |
| 25 | [publication] Preflight exact Git and GitHub actions and check their receipts | [369](https://github.com/joshlf/gherrit/pull/369), [371](https://github.com/joshlf/gherrit/pull/371), [377](https://github.com/joshlf/gherrit/pull/377), [378](https://github.com/joshlf/gherrit/pull/378), [393](https://github.com/joshlf/gherrit/pull/393), [400](https://github.com/joshlf/gherrit/pull/400), [423](https://github.com/joshlf/gherrit/pull/423), [425](https://github.com/joshlf/gherrit/pull/425), [367](https://github.com/joshlf/gherrit/pull/367), [391](https://github.com/joshlf/gherrit/pull/391), [392](https://github.com/joshlf/gherrit/pull/392), [395](https://github.com/joshlf/gherrit/pull/395), [410](https://github.com/joshlf/gherrit/pull/410), [429](https://github.com/joshlf/gherrit/pull/429) |
| 26 | [publication] Consume exact observations through ordered acknowledgement barriers | [379](https://github.com/joshlf/gherrit/pull/379), [381](https://github.com/joshlf/gherrit/pull/381), [382](https://github.com/joshlf/gherrit/pull/382), [384](https://github.com/joshlf/gherrit/pull/384), [393](https://github.com/joshlf/gherrit/pull/393), [400](https://github.com/joshlf/gherrit/pull/400), [380](https://github.com/joshlf/gherrit/pull/380) |
| 27 | [test] Explore retained publication attempts against literal external state | [383](https://github.com/joshlf/gherrit/pull/383), [387](https://github.com/joshlf/gherrit/pull/387), [394](https://github.com/joshlf/gherrit/pull/394), [422](https://github.com/joshlf/gherrit/pull/422), [424](https://github.com/joshlf/gherrit/pull/424) |
| 28 | [pre-push] Activate the checked publication engine and retire legacy writers | [397](https://github.com/joshlf/gherrit/pull/397), [403](https://github.com/joshlf/gherrit/pull/403), [404](https://github.com/joshlf/gherrit/pull/404), [399](https://github.com/joshlf/gherrit/pull/399), [413](https://github.com/joshlf/gherrit/pull/413), [419](https://github.com/joshlf/gherrit/pull/419), [420](https://github.com/joshlf/gherrit/pull/420), [427](https://github.com/joshlf/gherrit/pull/427), [428](https://github.com/joshlf/gherrit/pull/428), [431](https://github.com/joshlf/gherrit/pull/431), [414](https://github.com/joshlf/gherrit/pull/414), [415](https://github.com/joshlf/gherrit/pull/415), [418](https://github.com/joshlf/gherrit/pull/418), [421](https://github.com/joshlf/gherrit/pull/421) |

The upper layers introduce the corrected final representation directly. They
do not land a lightweight-only engine and later replace it, or reorganize the
old stronger oracle before replacing its storage assumptions. Existing/pending
and receipt-bound projection entries remain distinct because create receipts
establish authority which is absent before transmission.

The native Git differential case and production-driver ABA schedule establish
specific counterexamples and controls. They are not proofs of all GitHub
behavior, native landing, or migration/adoption safety. Terminal write receipts
may be unusable after fields changed; the model preserves that possibility.

*Authored by an AI agent acting on joshlf's behalf.*

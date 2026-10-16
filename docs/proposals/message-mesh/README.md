# Preserved mesh implementation proposal

Status: **SOURCE_ONLY / NOT_COMPILED / NOT_TESTED / NOT_DEPLOYED**.
This packet preserves authored work for resumption without placing an untested
mesh in the shipping crates. See [the handoff](../../agent-hub-resume.md).

`implementation.patch` contains the assembled source changes relative to the
owned local health-source working tree. `source-manifest.json` records exact
preimages, proposed files, contributor freezes and the private assembled source.
Verify every changed-file preimage before `git apply --check`; use an owned
isolated lane and preserve existing work. Do not apply blindly to current main.
The patch application check passed at checkpoint creation. No Rust compilation,
unit/backend execution or runtime deployment is implied by that check.

The private full assembly remains under
`.Codex/state/fleet-reconciliation/followthrough/message-mesh/assembled-candidate/source`.
Its complete file mapping includes historical helper versions; prepare a fresh
complete manifest after source integration and review, rather than claiming the
private snapshot matches a newer checkout automatically.

Blocking review finding: the old HTTP pub/sub bridge ignores the canonical
notification array and loses durable SSE resume IDs. Consume a present array,
including an empty array, and retain legacy fallback only when absent. Detect
both behaviors before qualifying this proposal.

`QA_PLAN.md`, `qa-contract.py`, `test_qa_contract.py` and `required-tests.json`
preserve the service-free evidence controller and contributor test inventory.
Ten Python controller regressions passed. The original inventory binds sixty-two
authored tests, including twenty-eight disposable backend tests; it must be
extended for the later HTTP runtime, credentials and Root integration tests.
These source counts are not executed test results. Derive exact selection and
pass counts from actual compiled libtest output.

Use the maintained disposable-services harness and admitted source-read-only
container route. Keep backends and artifacts owned, record exact identities and
cleanup, reuse the owned warm cache only after releasing its previous writer,
and preserve first failures. Never redirect these tests toward fleet stores.

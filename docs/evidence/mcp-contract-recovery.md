# MCP contract and recovery repair

Failure cases recorded before implementation (14 October 2026):
- A tool selected alone loses nested action/expectation fields through prose references.
- Missing variant fields (type without text, click without target, send without destination) pass schema validation.
- Fields belonging to another action variant or unknown nested fields pass schema validation.
- FilesInput loses common task/request fields or becomes impossible when strict variants are introduced.
- Valid browser actions, coordinate targets, wait alternatives or file operations regress.
- An unfinished or unknown operation advises a replacement ID instead of reconciliation.
- A same-session conflict masks an unfinished operation; begin conflicts omit the earlier task/operation.
- A request queued before its journal row exists supplies an unusable wait call.
- Finished argument conflicts need safe distinct-intention advice, while exact retries must still replay once.
- A misspelled tool gets no discovery path, or a guessed effectful tool is accidentally dispatched.
- Catalog size grows without visibility, supported protocol transport breaks, or help examples stop parsing.

Verification will retain the actual MCP catalog, error/replay receipts and output bytes. Daemon tests use stub desktop adapters and do not prove native input. A separate representative desktop workflow supplies that evidence.

## Implementation

Branch `mcp-contract-recovery`, based on published source `35b3c50` in `/home/halla/workspaces/ibara-mcp-contract-recovery`. All builds and evidence stay under `/home/halla/build/ibara-mcp-contract-recovery`.

- Each tool now carries its own complete nested shapes. Discriminated branches keep required/allowed fields; coordinate and wait targets have explicit schemas. Presentation metadata is compacted, validation constraints are retained. No additional tools or compatibility profiles were introduced.
- Unresolved journal, in-flight, automatic-begin and early-finish conflicts guide reconciliation using the original operation/task/computer. Exact retries retain their existing replay behavior. Finished conflicts explain separate intentions.
- Both direct-target and operator MCP entries return valid tool names and discovery calls on unknown names, before dispatch. Unknown tools remain protocol errors, as in the [negotiated MCP specification](https://modelcontextprotocol.io/specification/2025-06-18/server/tools#error-handling).

## Repeat the protocol/process check

From this worktree on Halla, using a build directory outside the checkout:

```sh
umask 077  # collector fixtures require private directories
export CARGO_TARGET_DIR=/home/halla/build/ibara-mcp-contract-recovery/target
export IBARA_E2E_ARTIFACTS=/home/halla/build/ibara-mcp-contract-recovery/evidence
/home/halla/.cargo/bin/cargo test --test server_entries mcp_contract_recovery_retains -- --nocapture
python3 scripts/check-mcp-contract.py "$IBARA_E2E_ARTIFACTS"
/home/halla/.cargo/bin/cargo test --test operator
/home/halla/.cargo/bin/cargo test --lib contract::
/home/halla/.cargo/bin/cargo test --lib store::tests::
/home/halla/.cargo/bin/cargo test --lib mcp::
/home/halla/.cargo/bin/cargo test --lib controller::tests::
```

The Python check uses Halla's installed `python3-jsonschema` to validate the actual catalog from the daemon E2E. It verifies each tool independently and exercises valid and invalid nested calls. Receipts preserve the actual operation references, conflict guidance, replay identity, single process-start line and exact output. The process E2E simulates a consumer losing the initial receipt; the existing operator E2E also exercises a real lost transport reply.

## Native regression and review

The installed release, without candidate installation, passed one representative task on automatically selected AK2 `cmp_aed35653d0933b48a0e7e19d`, task `task_d6ef4e2cc13f433bbe07953fa6ad73e3`. Mousepad saved `mcp-contract-note.txt` through native input. Replaying typing request `mcp-contract-type` returned original operation `op_29a816503b46401188d4d6187a7bfd7f`. The file read returned exactly 74 bytes with no duplicate text; finish automatically verified the exact content, reported complete and closed Mousepad. `evidence/native-regression.json` and `native-note.txt` retain the read-back bytes and finish receipt. Repeat with a fresh task/workspace and the same expected note, using the live element references.

One independent source review found an early-finish conflict path missing recovery details; it was repaired and covered by the daemon E2E. Targeted follow-up found no remaining material defects. The reviewer inspected tests but did not execute them.

Limits: native proof exercises installed-release input shapes, not candidate installation. No production package, service or fleet rollout changed. JSON Schema validity and protocol E2E do not establish that every supported model/harness preserves `oneOf` and `const`; no all-harness or smaller-model canary was run. Parser-level cross-field rules still supplement the input schemas. No measured agent-success score is claimed.

## Verification result — 14 October 2026

- Candidate daemon/process MCP E2E: 1 passed, including unresolved exec and finish conflicts, exact replay identity, one process execution, exact output, malformed-input field recovery and unknown-tool discovery.
- Actual catalog: 11 self-contained schemas, approximately 25 KiB; 16 independent valid/invalid schema cases passed. Both existing catalog-size guards now allow at most 64 KiB.
- Existing regression checks: 24 contract, 21 journal/store, 8 MCP/entry transport, 173 controller and 15 operator E2E tests passed. The operator suite includes a lost-reply transport test and now proves that unknown names open no SSH route.
- Native installed-release regression: exact 74-byte saved file, same operation on replay, automatic content check, complete finish and closed application window.
- `git diff --check` passed. Final successful logs use `verified-*.log`; the successful controller run is `verified-controller-private.log`. An earlier controller run with the shell's default umask failed the collector's private-parent fixture requirement; it passed under `umask 077`, with no weakening of production checks.

The baseline failure log demonstrates the old missing operation-reference/reconciliation data. The baseline catalog checker refused the cross-tool prose references. Retained evidence is in `/home/halla/build/ibara-mcp-contract-recovery/evidence`; `manifest.json` binds the final revision to hashes of the successful verification logs and artifacts.

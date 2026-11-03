# SC-FILTER.md — RTBC deployment admission gate

> **STATUS: CURRENT — reference documentation (implemented, active)**
> The RTBC gate is live in the node: policy, inspector, and wiring shipped2026-11-03;
> all gates green (clippy 0/0, workspace tests, CI-equivalent full suite).
> This is the design + implementation reference. The original todo-format plan this
> document replaces is retired.

## 1. What it is

A strict **pre-install admission gate for contract bytecode** — a bouncer at the door,
not a change to the law. If a deployment's initcode or returned runtime bytecode exceeds
the deliberately narrow policy, it is refused. No warnings, no review queue, no attempt
to prove malicious intent, no classification of contract purpose.

Binding principles:

- **Check-refuse, not EVM policy.** The EVM executes normally; our layer only checks and
  refuses. Zero modifications to revm or any EVM core; no custom opcode semantics; no
  fork patches.
- **Separate class only.** One policy struct + one scanner (`smartnet-evm::rtbc`), one
  inspector (`smartnet-evm::rtbc_inspector`).
- **Hooks only to check.** The inspector refuses frames; revm's normal failure handling
  does the rest (rollback, gas, receipt).
- **Let EVM do all.** Execution, metering, state, rollback stay 100% revm.

Honesty note: initcode size is checked truly pre-execution. Runtime bytecode cannot be
known until the constructor runs — the runtime check is *post-constructor, pre-install*.
A constructor can generate code dynamically, which is why the post-constructor gate is
mandatory. Nothing installs without passing it.

## 2. The policy — three hard checks, nothing else

1. **Initcode size** — reject creation input > **49,152 bytes** (EIP-3860's value).
   Size check only.
2. **Runtime size** — reject returned runtime > **24,576 bytes** (EIP-170's value).
   revm already enforces this under the MARS spec (belt-only here).
3. **Forbidden opcode** — reject if the *executable* instruction stream of the returned
   runtime contains **SELFDESTRUCT (0xff)**. PUSH-aware decode required.

Explicit non-goals (operator rulings, 2026-11-03):

- **No instruction-count limit** — brittle heuristic, not a cost guarantee.
- **No opcode scan of initcode** — its trailing regions (embedded runtime blob,
  constructor args) are data with a different decode alignment than the runtime;
  scanning them false-rejects real contracts (measured). Constructor-executed
  SELFDESTRUCT only destroys the contract being born in the same tx — little abuse
  value, not worth the false-positive risk.
- **No CALL / STATICCALL / DELEGATECALL / CREATE / CREATE2 restrictions.**
- **No SSTORE / JUMP / control-flow restrictions.**
- **No proxy restrictions** — Uniswap V2 factory/pair/router, ERC-20, proxies and
  CREATE2 factories keep deploying and working unchanged.
- **Not retroactive** — already-deployed code stays.

Policy in one line: reject oversized contracts and executable SELFDESTRUCT in runtime;
everything else follows normal EVM rules.

## 3. Where the checks run

One hook pair, all deployment routes:

- **`Inspector::create` (pre-execution)** — initcode size check. Rejecting returns
  `CreateInitCodeSizeLimit` and the frame never runs. Passing stashes an extra journal
  checkpoint (see §5).
- **`Inspector::create_end` (post-constructor, pre-install)** — runtime size + scan on
  the returned bytecode; accept → commit the stash, reject → revert the stash (full
  rollback) and set `CreateContractSizeLimit`.

Hook coverage is proven (registry source, revm-inspector19): top-level create txs,
internal `CREATE`, internal `CREATE2` and factory-created contracts all converge on
`inspect_frame_init → frame_start → inspector.create` — nested-in-constructor included.
Failed internal deployment follows normal EVM behavior (frame fails, caller continues);
failed top-level deployment produces a failed receipt.

Halt variants are revm's own closed enum, chosen for exact semantics:
`CreateInitCodeSizeLimit` (EIP-3860 semantic, pre-exec) and `CreateContractSizeLimit`
(EIP-170 / deposit-failure semantic, runtime).

## 4. Semantics (gas, receipt, rollback)

- **Rejection = code-deposit-failure style**: exceptional halt of the create frame,
  create-frame gas consumed — the same deterministic behavior revm uses for an oversized
  deposit. No new gas rules, no refunds, no special cases.
- **Atomicity**: a rejected deployment leaves no code and no partial account state —
  constructor storage and nested changes roll back.
- Gas accounting is deterministic; receipt status is the normal failed-deployment
  outcome. Top-level initcode >49,152 halts the tx with `CreateInitCodeSizeLimit`
  burning the full gas limit (consistent with the deposit-failure ruling).
- Rejection logs a clear reason: `tracing::warn!`, target `smartnet::rtbc`, carrying the
  `RtbcReject` variant + byte count. Never a panic.

## 5. The LIFO checkpoint design (why the inspector carries a stack)

Discovery during verification: `create_end` fires **after** revm's `return_create` has
already committed its checkpoint and installed the code — mutating the outcome alone
would leave nested creates' code in state while the creator sees failure.

Design (public journal API only, no revm changes):

1. `create` (after the initcode check passes): take an extra `journal.checkpoint()`,
   push it onto an inspector-owned LIFO stack — it sits *below* the frame's own
   checkpoint, so everything the frame does lands above it.
2. `create_end`: runtime accepted → commit the stash; rejected → revert the stash
   (removes the frame's effects *including* revm's already-committed install) and halt;
   constructor already failed normally → just close the stash.
3. The pre-hook's own reject also triggers `create_end` immediately (same frame) — a
   flag keeps it from popping an enclosing create's stash. Frames nest → LIFO.

Verified behavior (tests in `crates/evm/evm/src/rtbc_prototype.rs`, production inspector,
real revm execution): accept path gas-identical to NoOp; nested reject keeps parent
state and fully rolls back the child (no code, untouched account, logs dropped);
native-failure parity with NoOp (gas + state); **access-list/warming equivalence**
between native failure and gate rejection (the stop-rule probe); LIFO hygiene across
consecutive txs; deterministic reject gas.

## 6. Scanner requirements (load-bearing rules)

- Walk by opcode, **skip PUSH1–PUSH32 immediates** — never scan raw bytes for 0xff
  (real code contains long `PUSH20 ff…ff` address-mask runs).
- **Strip the trailing solc metadata suffix before scanning**: trailing 2-byte big-endian
  length `L`; the `L` bytes before it must start with a CBOR map header (0xa0..=0xbf),
  be ≤256 bytes, and contain a known key (`ipfs`/`bzzr0`/`bzzr1`/`solc`). Without this,
  the decode walks past solc's `fe` terminator into hash bytes and false-rejects
  production contracts (measured: `wemx` solc-7.6, `0xff` at pc=10826).
- **Truncated PUSH = end of decode, never an error** — EVM zero-pads PUSH data past code
  end (an earlier "malformed" rule would have rejected an 86-byte SafeMath library).
- Decoder is revm's own (`revm_bytecode::BytecodeIterator` via `Bytecode::new_legacy`) —
  no hand-rolled scanner.
- **Accepted false-negative class (documented, not fixed):** a determined author can hide
  executable `0xff` where the linear decode sees data (jump-into-push-data, fake metadata
  suffix). Sound for blocking basic abuse, not a proof of absence — matches the stated
  "very basic protection" scope.

### Empirical validation (real toolchain, 2026-11-03)

Contracts compiled with matching `solc` from `/home/usr3/solc` (0.6.12 / 0.7.0 / 0.7.6 /
0.8.26, optimizer on + off, plus an `--evm-version london` build) → **26 contract
variants** checked against both stages:

- Zero `SELFDESTRUCT`/`suicide` in any source; sizes far under both caps.
- Naive linear scan (no metadata strip): **12/24 false rejects** — all traced to
  decode-into-metadata or the truncated-push rule.
- With the §6 rules: **26/26 PASS.**
- Artifacts are checked in at `crates/evm/evm/tests/fixtures/` and asserted by both the
  scanner unit tests and the execution tests.

## 7. Compatibility test matrix — **all 13 items covered (2026-11-03, Wave 4b complete)**

Must fail (covered by tests): executable SELFDESTRUCT in runtime · runtime >24,576 ·
initcode >49,152 (top-level and nested-via-zero-memory) · constructor returning
oversized/forbidden runtime → full rollback · nested CREATE2 route rejection with caller
unaffected.

Must pass (covered):

-6. Truncated-PUSH/metadata decode edges (`rtbc::tests`).
- 7. **ERC-20 family, full**: `wemc` (0.8.26 london-evm), `wemx` brandnew/v2/v3 (0.6.12),
  `WEMTOKEN` (0.7.0/0.7.6), **`wemu` + `wwem` (0.5.16 — solc binary added2026-11-03,
  previously blocked)** — all scanned by `real_artifacts_pass_both_stages` (43 variants
  across 19 fixture files) and the token family deploys through the gate
  (`real_artifacts_deploy_through_gate`); wemx-7.6 remains the metadata-FP canary.
- 8. **Proxy + implementation via DELEGATECALL** — `proxy_and_impl_work_through_gate`:
  both contracts deploy through the gate; `proxy.value()` returns 42 through the
  delegatecall fallback.
- 9. **Uniswap V2 Factory + Pair + Router** — `uniswap_v2_trio_works_through_gate`:
  real Uniswap sources compiled and deployed through the gate; pair created by the
  factory (internal CREATE2); router wired (`factory()`/`WETH()` views verified);
  **real `addLiquidity`** (approve ×2 → transferFrom → `pair.mint` → non-zero reserves).
  The V2 library's hardcoded pair init-code hash was patched to our compiled pair's
  hash (local-compile requirement — the library computes pair addresses via the CREATE2
  formula, so the hash must match the bytecode actually deployed by the factory).
- 10. **CREATE2 factory** — `create2_factory_deploys_legit_child`: factory deploys a
  legit child; factory's returned address == alloy's CREATE2 formula == installed code
  (three-way assert).
- 11–13. `0xff` in PUSH data · `0xff` in initcode data (size-only) · exact boundaries
  (`rtbc::tests`).

Fixture logistics: compiled artifacts live in `crates/evm/evm/tests/fixtures/` (19 JSON
files,43 deployable variants). Sources are **not** kept in-repo: ERC-20/0.5.16 sources
as before, Uniswap trio fetched from upstream GitHub (`Uniswap/v2-core`,
`Uniswap/v2-periphery`, `Uniswap/solidity-lib` @ master) — re-fetchable; compiling the
pair again requires re-patching the library's init-code hash. Toolchain now includes
`/home/usr3/solc/solc-5.16` and `solc-6.6` alongside the existing binaries (Router02
pins `pragma =0.6.6`).

## 8. EIP verdicts (final)

| EIP | Verdict | Note |
|---|---|---|
| **3860** (initcode size) | **Adopted — size limit only** | MARS exec spec is `SpecId::LONDON`; revm's Shanghai-gated checks are dormant (top-level and internal CREATE/CREATE2 alike; no metering anywhere). RTBC carries the 49,152 cap; metering deliberately not adopted (block gas limit bounds cost). If the MARS spec ever bumps past Shanghai, revm's dormant checks wake up — revisit for dedup (same value ⇒ at worst a harmless double-check). |
| **6780** (SELFDESTRUCT same-tx-only) | **Not implemented** (operator-confirmed) | Rewrites EVM execution semantics — forbidden by this doc's own principles. The gate keeps SELFDESTRUCT out of all new code; only legacy code could be affected (genesis is audited clean). |
| **3541** (refuse 0xEF-prefixed code) | **Deferred** (operator) — no action | Already enforced by revm by default under London (`is_eip3541_disabled()` = false). The `optional-eip3541` feature only exposes *disabling* it. Leave revm config alone. |
| **3607** (reject txs from code-holding senders) | Out of scope here | tx-admission, not contract-admission — belongs to the mempool/validation discussion. |
| Gas-cost / EOF / 4844 / 1153 / 7702 / … | Out of scope | EVM-core decisions, not an admission filter. |

## 9. Implementation map

| Piece | Location |
|---|---|
| Policy + scanner (`RtbcPolicy`, `RtbcReject`, `check_initcode`, `check_runtime`, `strip_solc_metadata`) | `crates/evm/evm/src/rtbc.rs` |
| Inspector (`RtbcInspector`, `rtbc()` shorthand, LIFO stack, reject logging) | `crates/evm/evm/src/rtbc_inspector.rs` |
| Protocol constants (`MAX_INITCODE_SIZE = 49_152`, `MAX_RUNTIME_SIZE = 24_576`, authoritative) | `crates/chainspec/src/constants.rs` |
| Drift guard (`rtbc_policy_mirrors_chainspec_constants`) | `crates/evm/ethereum/src/lib.rs` (tests) |
| Acceptance/execution tests | `crates/evm/evm/src/rtbc_prototype.rs` (15 tests) |
| Scanner unit + real-artifact tests | `crates/evm/evm/src/rtbc.rs` (tests), fixtures in `crates/evm/evm/tests/fixtures/` |
| Wiring | every state-changing EVM construction site calls `evm_with_env_and_inspector(…, rtbc())` (13 sites): payload validation/execution + prewarm (`engine/tree`), reorg build (`node/builder`), `eth_estimateGas`, `eth_call` (plain + inspector), `debug_trace*`, `eth_callBundle`/`sim_bundle` (`rpc*`). Tuple form `(rtbc(), inspector)` where a site already passes its own inspector. |
| Intentionally un-gated | precompile-only `evm_for_block(EmptyDB, fake_header)` in `rpc-eth-api/helpers/config.rs` (no txs), `precompile_cache` raw `create_evm` (no txs), tests. |

Notes:
- `EvmFor<.., I = NoOpInspector>` pins `evm_with_env`'s return type — that is why wiring
  goes through the generic `evm_with_env_and_inspector` per site rather than one trait
  override.
- The policy default mirrors the chainspec constants (drift-guarded by test);
  chainspec remains the single source of truth.

## 10. Operational notes

- **Toolchain:** solc ≥0.8.20 defaults target Shanghai+ and emits **PUSH0**, which is
  `NotActivated` under MARS-London — compile with `--evm-version london` (evidence:
  `wemc` 0.8.26 default build fails to deploy; the london build deploys cleanly).
- **Rejections are visible**: `smartnet::rtbc` warn logs with variant + byte size.
- **Gates** (what this feature is held to): `make clippy` (budget 0),
  `cargo test --workspace --lib`, stable `cargo check --workspace` must be 0 warnings
  (nightly clippy does not fire `unused_crate_dependencies`; stable does), full CI
  suite `make cargo-test`.
- **Not retroactive:** live chain state may contain pre-filter contracts (chain at 47k+
  blocks); a state-level scan for executable 0xff is available on request. Genesis
  fixtures carry no contract code (audited).

## 11. Assessment (original review, for the record)

| Check | Assessment | Recommendation |
|---|---|---|
| Initcode size | Good | Keep |
| Runtime size | Good | Keep |
| PUSH-aware opcode scanning | Essential | Keep |
| Reject SELFDESTRUCT | Reasonable | Keep (semantics §4) |
| CALL restrictions | Dangerous for compatibility | Do not restrict |
| SSTORE restrictions | Dangerous for compatibility | Do not restrict |
| JUMP restrictions | Dangerous for compatibility | Do not restrict |
| Proxy restrictions | Unacceptable | Do not restrict |
| Instruction-count limit | Brittle, evadable | Removed (operator) |
| Initcode opcode scan | False-positive risk on data | Removed (operator) |

Final rule: RTBC validates deployment size and one forbidden opcode. It does not
classify contract purpose or restrict normal EVM functionality. Separate class,
hook-only, revm untouched.

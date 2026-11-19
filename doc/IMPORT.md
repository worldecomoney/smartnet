# RLP Import — Findings and Proposed Plan


## 1. Delivery path — direct to storage, in ranges (operator order, 2026-11-18)

**The import does not go through the engine.** No `newPayload` round-trip, no engine
queue, no cache-lock waits, no tree insert. Measured reason (2026-11-18 run, every block
in the file empty): `engine_ms = 13.1 → 15.6` per block with `queue_ms < 1` and
`persist_ms 0.0` — 14 ms of *machinery* around ~0 ms of work. reth's `import` shows the
same shape from the other side: chunked read → bulk write → execution range → one commit
per batch, never one round-trip per block.

Per range of **`IMPORT_RANGE = 1000`** blocks (was 100, operator 2026-11-19):

1. **Read** — `BufReader` with a **100 MB** read-ahead (`READ_CAPACITY`, was 1 MB), one
   RLP item at a time from the file in place (read-only, never copied).
2. **Decode** — `alloy_rlp::Decodable` → `smartnet_primitives::Block`; trailing bytes
   after the item are an error (a truncated chain never imports as complete).
3. **Chain position (check 1)** — `classify(number, parent, start, accepted)`: genesis and
   anything at/below the durable tip are skipped (resume); otherwise the number must be
   `prev + 1` **and** the parent hash must equal the previous block's hash. Anything else
   stops the import with the block number and file offset.
4. **Validator gate (check 2)** — `validate_static_signer(seal signer, enabled_set())`:
   recover the signer from the seal and require it in the RAM registry with
   `status: true`. `enabled_set()` is the same enabled set `get_val_chair` picks from,
   so there is exactly one true/false match in the codebase and a `status: false` record
   reaches neither the chair nor an accepted block. Nothing else gates acceptance (§6.7):
   no fee rule, no genesis `extraData` branch, no body-vs-header check.
5. **Execute the range** — the node's own EVM config (`EthEvmConfig` env/ctx + the rtbc
   create-admission inspector), state read from the stores: the range anchor on disk with
   in-range ancestors overlaid in memory. One EVM pass over the range, not one per block
   through the engine.
6. **Write + commit ONCE per range** — file-tier payload append → DB state/index → a
   **single commit**. The range's trie updates are computed once from the disk trie at
   the anchor (only the range's changed paths walk) and the root is checked against the
   range's **last header before the commit**, so the stores can never land with a wrong
   trie.
7. **Report** — only ever the **committed** tip. On every completed range print the DONE
   line with the block it reached (`done 1000 blocks block=7189`), and one
   `import progress block=… blocks_s=…` per `REPORT_INTERVAL` (10 s). The import runs on
   the blocking pool so `ctrl-c` (selected against the command future) is seen at once
   and stops the run at the next block boundary.

Where it runs: in the `smartnet import` arm of the CLI, **before the node boots** —
no engine, no mesh, no heartbeat exists yet, so the chain file is the only writer and
the sequence is exactly *import → mesh → mesh sync → RPC*. When the import returns, the
arm boots the node on top of what was imported and `main.rs` exits (the runner performs
the clean shutdown); a ctrl-C during the import stops at a range boundary and exits
without booting.

Rules that fall out of it:

- **Contract metadata rides bytecode.** Every contract with code that a block touches is
  probed on the post-block state (system calls, state discarded — diagnostic only), once
  per deployed address; the record is committed with the block and written as
  `{address}.meta` beside the runtime-bytecode file (operator: "same filename, just
  .meta"). Fields: the full contract `address`, `name`/`symbol`/`decimals`/`totalSupply`/`owner`
  (view calls), the contract `hash` (code hash), the `creator` (the address that ran the
  create frame, from the rtbc inspector's create log), and the `compiler` stamp (solc
  version from the bytecode's CBOR metadata). A holders stub `{address}.json` is written
  beside it — one owner entry, no amounts (`TokenHolder` = `r`/`a`/`b`/`p`); an external
  hourly cron script owns that file afterwards, so the node writes it only when absent. Contracts that do not answer `decimals()` get no sidecar.
  The bytecode tree is keyed by the deployed address —
  `bytecodes/{3hex}/{3hex}/{3hex}/{rest_of_address}` (operator: "it must be contract
  deploy address, not a hash"). The sync executor captures the same way.
- **Fail closed on the registry.** The import loads `validators.jsonl` into RAM itself
  (an import run never boots the validator) and refuses to start without a readable
  `status: true` validator — no registry, no import.
- **The commit is the durability point.** Nothing is reported before it is on disk in
  DB + FS; the reported `block` is `last_block_number()` (the `CanonicalChain` tip,
  written only by the save path).
- **Recovery anchors on `min(file tip, DB tip)`** (`StorageEngine::restore_unwind`):
  a range appends its lines first and commits once, so after a crash the file may lead
  the committed DB — the active files are trimmed back to the committed tip instead of
  being trusted (the old drop-2 anchor assumed one block per save). A seal only ever
  completes after its commit, so a compressed shard never holds an uncommitted block.
- **One writer.** Nothing else may touch the stores while an import runs: the engine's
  persistence service, the mesh and the validator are all started after it.
- **No engine in the loop, ever.** The engine's live path (queue, caches, tree, per-block
  state-root task) exists for blocks arriving from the network; a file is a sequential,
  already-closed stream.
- **No `persistence_threshold` override** for import any more — the import's own range is
  the batch. Network runs keep the engine default (persist every canonical block).
- **ctrl-c / SIGTERM** stops at a range boundary, persists the in-flight range (every
  executed block is already validated), then records `SHUTDOWN_CLEAN`.
- **Resume**: a re-run starts from the durable tip, so an interrupted run re-reads at most
  the last range.
- **No validator, no mesh, no RPC knobs** in the import feature surface — boot concerns
  of the standard node launch only.

## 2. Seal compatibility (verified, no bypass, no consensus change)

Verified against the full original chain (4,094,194 sealed blocks):

| Check | Result |
|---|---|
| `vanity[0]` | 216 on every block (≠ protocol version 1) → legacy fallthrough in `validate_smartnet_proposal` (`proposal_validation.rs:110-127`) |
| extraData | 97 B normal, 117 B at epoch checkpoints (≥ 97 min) |
| Signer | single genesis signer `0x24bf191a358c09e79b9ead2d8999d08590b5af31` = quorum |
| difficulty | 2 on block 1; no difficulty check in `validate_header_inner` |
| mixHash / nonce | zero as produced |
| uncles | none |
| base fee | pre-London `None` ↔ payload `0` round-trip exact (`rlp_decode.rs:10`, `base_fee_from_payload_v1`) |
| MINIMUM_GAS_LIMIT | 5000; genesis 4.7M passes |

SEAL rejection at any block → import stops, no retries. Must not occur in production for a normal export file.

## 3. Input file

- `~/Downloads/chaindata.rlp` — **READ ONLY**, 2,497,998,772 B. Never copy, never modify.
- Streamed **in place**, block by block (Go reader does full parse in seconds; same framing in Rust).
- Contents: 4,094,194 blocks, numbers 0..4,094,193, in order, ~610 B/blk; 2,954 txs in 1,974 blocks (first tx #17,214, first heavy #19,018, last 4,067,952); gasLimit 4.7M→30M; pre-London 15-field headers; block RLP = `[header, txs, ommers]`.

## 4. Schedule data — explicitly ignored

Genesis schedule entries carry no meaning for import; the client runs one baked-in rulebook
(`MARS.md`). The import layer does **zero** schedule logic:

- read RLP → decode → pass to engine → engine deals with it.

No detection, no per-entry branches, no EIP special-casing in import code. All blocks are treated identically.

## 6. Plan — SHIPPED (2026-10-25)

> **Superseded in part (2026-11-18):** §6.2–§6.4 describe the *engine-path* import
> (`new_payload` per block) that §1 replaced. The command surface (§6.1), the reader and
> `classify` tests (§6.4) and everything from §6.6 on still stand — only the delivery
> path changed: no engine round-trip, ranges of 100, one commit per range.

### 6.1 `crates/ethereum/cli/src/interface.rs`

- `ImportCommand { file: PathBuf }` (`#[derive(Debug, Parser)]`, positional path).
- `Commands::Import` variant (`#[command(name = "import")]`).
- Match arms: `chain_spec() → None`, `debug_namespace_enabled() → false`, `log_chain_directory_name` shares the None-arm DB chain-id read (`None | Some(Commands::Import(_))`).

### 6.2 `crates/ethereum/cli/src/app.rs` — arm in `run_commands_with` (dispatch at :731)

- New `Some(Commands::Import(cmd))` arm mirrors the existing None-arm boot: fail-fast `command.file.is_file()` → operator overlay → file tier → identity-from-DB → chain spec-from-DB → `operator_default_node_command`.
- Boot-critical keys only: `db.engine`, the **mesh** keys (`mesh_port`/`mesh_addr`/`mesh_peers_file`/`mesh_max_msg_mb`/`mesh_heartbeat_ms`), `cross_block_cache_size`, plus `node_cmd.validator_daemon = false` (below). No `heartbeat_ms` — the validator cadence does not exist for this launch.
  - **Correction to the original plan**: the mesh is *always on* — `mesh_gate` `std::process::exit(1)`s without `MESH_PORT`, and `load_mesh_identity` needs the operator config path. So `node_cmd.config` stays `Some(cfg_path)` exactly as the constructor sets it (the earlier idea of `config = None` would have killed the boot), and the mesh keys are stamped like a node run.
- Stays off: HTTP + every RPC quoting/gating/TTL knob (nothing submits transactions during import). *(The `RPC_SYNC`/`SYNC_PEERS`/`SYNC_BATCH`/`SYNC_POLL_INTERVAL` keys named here were removed entirely with RPC sync, `user-in.md` #5.)*
- **Validator — structurally disabled, not merely quiet.** New `validator_daemon: bool` on `NodeCommand`/`NodeConfig` (default `true`, so every other launch is byte-for-byte unchanged); import sets it `false`. The gate in `ethereum/node/src/node.rs` `launch_add_ons` becomes `ctx.config.validator_daemon && match config_path { … }` — import short-circuits *before* the config's `SEAL` key is read: no heartbeat task is ever spawned, no seal pass runs, no keystore is touched, zero `VALIDATOR_*` events. "No validator in import" is now impossible to violate by configuration; sync (the RPC sync service) never had validator involvement — both paths are pure engine feeds (`new_payload(_, true, true)`, `Valid` or stop), matching `EngineBlockSink`.
- Launch via `NodeCommand::execute(ctx, datadir_args, FnLauncher::new::<C,Ext>(launcher))`; the runner shuts the node down when the import loop returns.

### 6.3 `bin/smartnet/src/main.rs` (:24-31)

- Peek the parsed command before `cli.run(...)`:
  - `Some(file)` → `smartnet_node::import::run_import(&handle.node.add_ons_handle.beacon_engine_handle, &handle.node.provider, &file)` — on success the launcher future resolves and the runner gracefully shuts the node down and exits (`new_payload(_, true, true)` keeps `wait_for_persistence=true`, so state is durable before exit).
  - else → `handle.wait_for_node_exit()` as today.

### 6.4 `crates/ethereum/node/src/import.rs` (new) + `pub mod import;` in `lib.rs`

- `read_rlp_item` — port of `.ref/rlp-main.go` `readRLPItem` (streaming, never loads the whole file): prefix byte → short/long length → payload; clean EOF on a boundary = `Ok(None)`, mid-item EOF / >64 MiB length = `Err` (a truncated chain never imports as complete).
- `run_import(engine, provider, file)`:
  1. `start = (best_block_number, sealed_header(best).hash())` — read **once**; missing tip header = "run `smartnet init`" error.
  2. Per item: `alloy_rlp::Decodable` → `smartnet_primitives::Block` (trailing bytes rejected) → `classify(number, parent, start, accepted)`: genesis / `number <= start.0` ⇒ skip (resume); first accept must be `tip+1` with parent = tip hash; then strictly `prev+1` with parent = prev hash; anything else ⇒ stop with the block number + offset.
  3. `execution_data_from_block_rlp(block)` → `new_payload(_, true, true)` → engine contract identical to `EngineBlockSink`: `Valid` proceeds, anything else stops with `other:?` + `other.validation_error()` (no retries); record `(number, header.hash_slow())`.
  4. Progress `info!` per 10k scanned; final summary (scanned / imported / skipped / tip / `fmt_wall` elapsed — sync-style wall timing).
- Unit tests (10): reader boundaries (single byte, short string/list, 55↔56 form switch for strings *and* lists, clean-EOF vs truncation, 64 MiB cap, concatenated items), classify (genesis/resume skips, first-block parent/number, chaining, gap, wrong parent), and a block RLP round trip (encode `alloy_consensus::Block` → read → decode `smartnet_primitives::Block`, the exact wire pair mesh/sync use).

### 6.5 Build / verify — DONE

- `CARGO_NET_OFFLINE=true cargo check --workspace` clean; `make clippy` → 300 warnings ≤ budget 343 of the day (down 2: the import loop's `while let` / `is_multiple_of` fixes; `clippy.budget` has since ratcheted to 0); targeted tests green (10 import + 15 cli + 16 node + 4 it); `rustfmt +nightly` on the five touched files.
- `make cargo-test` workspace-wide: green except `-p smartnet-node-core --lib` and `-p smartnet-rpc-builder --lib` — **both reproduce on a clean tree (pre-existing baseline, unrelated)**.
- **Live run (2026-10-25, in progress)**: `smartnet init .ref/wemblockchain.json` OK (genesis `0x550d835e…ad00`); `smartnet import ~/Downloads/chaindata.rlp` runs detached (`setsid`, log `/tmp/opencode/import.log`) — resume proven across kills (tip 0 → 1549 → 2777 → streams on), mesh up, sync/HTTP off, **`grep -cE "VALIDATOR|heartbeat"` = 0** (was 60+ spam every 2 s before the structural flag). Rate ≈ 4 blocks/s, disk 26 G free.
- No Python pilot, no file copy. Success criterion (unchanged): full run reaches block 4,094,193 and a re-run skips everything (resume, `imported 0`) — that also resolves risk (b), the Mars-vs-Istanbul state-root divergence.

### 6.6 First tx block (17,214) — fee beneficiary — FIXED (2026-10-25)

IE.md:120's fee-path caution materialized exactly as predicted: the run streamed 14,436 empty blocks (identity transitions — genesis alloc handshake proven) and stopped at 17,214, the chain's first transaction block: `mismatched block state root: got 0x6aac…, expected 0xce45…`.

- **Cause**: geth credited the 21k-gas fee to `Engine().Author(header)` (`core/state_processor.go` passes `author = nil`, so clique's `ecrecover` of `extra_data` = the sealer), never to `header.coinbase`, which clique's `Prepare` zeroes (vote marker only). The envelope carries `fee_recipient = header.coinbase = 0x0`, so revm credited the fee to 0x0 and the state root diverged at every tx block.
- **Rejected fix**: rewriting `fee_recipient` to the signer in `execution_data_from_block_rlp` breaks the engine's payload→header gate (`keccak(rlp(reconstructed_header)) == payload.block_hash`, `alloy … PayloadError::BlockHash`) — hit on the first processed block (17,212) and reverted; the envelope stays byte-honest.
- **Shipped fix**: `execution_beneficiary(&ExecutionData)` (`crates/ethereum/engine-primitives/src/lib.rs`) — reconstructs the header exactly like the gate does (`calibri_lite_consensus::pre_consensus::into_block_raw`: difficulty 2, empty ommers, zero nonce, `txRoot` recomputed from payload txs), recovers the seal signer (`recover_calibri_lite_signer`), falls back to `fee_recipient` when no seal is recoverable. Wired only into `evm_env_for_execution_payload` (the sealed path — genesis carries validators); the non-sealed path and every live block are byte-for-byte unchanged (SmartNet sets coinbase = signer from the same keystore that signs the seal, so recovery resolves to the same address).
- **Tests** (3, real vector): full block-17214 RLP embedded from chaindata.rlp (item 17215) → envelope honest (`fee_recipient == 0x0`) + reconstruction hash round-trips + signer `0x24bf191a…` (genesis sealer) + fallback keeps `fee_recipient`.
- **Evidence**: resume at tip 17,209 → `Block added to canonical chain number=17214 txs=1 gas_used=21.00K` accepted, streaming continues, `grep -cE "VALIDATOR|heartbeat"` = 0.

### 6.7 Validation policy — exactly two checks (operator directive, 2026-11-17)

The fee fix carried the run through 17,214 / 17,259 / 17,939 / 18,424 / 19,018, then block
**19,545** (1 tx, 7,141-byte deployment) failed on `block gas used mismatch: got 1577054,
expected 1568954` — geth executed it under the chain's Istanbul-only schedule, this client
runs the collapsed ladder, so gas (and therefore receipts/state) cannot reproduce history.
That is what forced the policy down to two checks.

**Directive: import and sync check exactly two things, nothing more.**

1. **The signer is a REAL validator** — present in `validators.jsonl` with
   `status: true`. **Not** the SELECT chair (`chair_for(parent_hash, ts)`), **not** a
   quorum subset: any enabled validator's seal is accepted. Implemented as
   `validate_authored_header` → `validate_static_signer`, which recovers the signer
   from the seal and requires it in the registry.
   The registry is loaded **unconditionally** at bootstrap (`node.rs`), so this holds
   for every genesis shape — including a calibri\* genesis with no `extraData` signers,
   where the old `extraData` gate returned `Ok(())` and switched seal validation
   *off* entirely. **Fail closed:** a missing or empty registry authorizes nobody.
   *Genesis carries only the chain id for this purpose — validators live in
   `validators.jsonl`, nowhere else.*

2. **The previous block hash must match** — `validate_against_parent_hash_number`
   (live/mesh path) and import's `classify` (file import). Parent hash plus the number
   continuity that rides with it; nothing else about the pair.

**Nothing else gates acceptance.** Removed from the import/sync path:

- **All fee checks.** `baseFeePerGas` is never inspected — `SmartNetBaseFeeInvalid`,
  `validate_header_base_fee` and `validate_against_parent_eip1559_base_fee` are no
  longer called from this path. The fixed fee is a pricing rule for whoever *builds* a
  transaction (`doc/FEES-CURR.md`), not a block-acceptance gate.
- Ethereum-era header format assertions: wall-clock future-timestamp, `extraData` size,
  gas-field sanity, and London/Shanghai/Cancun/Prague field presence.
- Parent-relative rules other than the hash: timestamp monotonicity (tier-0), gas-limit
  ramp, EIP-1559 continuity, 4844 excess-blob.
- The genesis-derived profile/`extraData` branch that used to pick a second, stricter
  validation path — check 1 is now identical for every chain shape.

Still not checked here (unchanged): `validate_block_post_execution` — **gas used**,
receipt root, logs bloom, requests hash — and the state-root comparison. Execution still
runs (state advances from the client's own computation); headers keep their historical
field values, so hashes/linkage stay intact. The batch trie root is still verified
before commit — that is storage integrity, not a block-quality check.

Unit gates: consensus 18, engine-tree 76, workspace `cargo check --all-targets` clean.

#### Future enhancement — a one-function switch (operator note, 2026-11-17)

**Documented only — nothing is switched on today.** Import/sync security can be
tightened later by flipping a single function, with no other change:

| mode | what check 1 becomes |
|---|---|
| **current (default)** | the signer is a **legal active validator** — present in `validators.jsonl` with `status: true` |
| **stricter (optional)** | the **SELECT chair** for this block's `(parent_hash, timestamp)` **plus the full validator signature/proposal check** |

The switch lives in `validate_authored_header`
(`smartnet/crates/consensus/seal/src/beacon_pre_consensus.rs`): the current mode
calls `validate_static_signer(header, &enabled)`; the stricter mode re-introduces
`chair_for(enabled, parent_hash, timestamp)` to pick the expected signer and validates
against that. `SELECT` already falls back to the single validator when the registry
holds exactly one, so the stricter mode is safe in the common single-validator case —
a one-validator bootstrap behaves identically under both modes.

When enabling it, the extra signature/proposal path to restore is
`calibri_lite_consensus::validate_pre_consensus_header` (the profile-driven variant
that also checks the proposal metadata); `validate_static_signer` is the membership-only
path used today.

### 6.8 Shutdown ordering, import speed, `Syncing` — FIXED (2026-10-26)

Two operator reports after the 2-check build: (1) `recorded clean shutdown` landed while
a block was still executing (the engine then logged `Failed to send event: … Valid …`
for the abandoned payload); (2) import was ~10x slower than `geth import`.

**Root causes.**

- *Shutdown*: the runner's `run_until_ctrl_c` select dropped the command future the
  instant ctrl-c won — mid-`new_payload` — and the shutdown-marker task then wrote
  `SHUTDOWN_CLEAN` while the engine was still on block N.
- *Speed (and the 22,484 `Syncing` stop)*: `DEFAULT_PERSISTENCE_THRESHOLD = 0` with
  `memory_block_buffer_target = 0` — one Fjall commit (fsync) per canonical block on the
  hot path, and the parent's in-memory entry pruned the moment it lands. The
  fsync-dominated per-block cost is the speed gap vs geth (which batches commits); the
  prune/commit window is what `state_provider_builder` fell through at block 22,484
  (parent neither in memory nor yet visible in the DB → `Disconnected` → `Syncing`).

**Fixes.**

1. **Drain-on-exit** — new `smartnet_tasks::drain` (exit flag + drainable registration).
   The runner flips the flag on ctrl-c/SIGTERM and, while a drainable command is
   registered, waits up to 60s for the command future instead of dropping it. `run_import`
   registers itself and checks the flag **only at block boundaries** — an in-flight
   engine call always runs to completion and is recorded first; then it logs
   `import stopped by signal — the last block was fully processed` and returns. Verified
   sequence: `Received ctrl-c` → `exit signal: draining…` → `import stopped by signal …
   tip=23120` → engine's `Block added … 23120` → `recorded clean shutdown` — marker
   strictly last, zero errors after it. Without a registered drainable command the
   runner's behaviour is unchanged (future dropped immediately, as before).
2. **Batched persistence for import** — *(superseded 2026-11-18: the override is gone —
   see §1; the engine default is back and the import's own 100-block range is the batch)* —
   the import arm stamped
   `engine.persistence_threshold = 4096`, `persistence_backpressure_threshold = 5120`,
   `memory_block_buffer_target = 0` (batch = 4096, commit up to head when triggered).
   Import can batch safely: the chain file is the source of truth and a re-run resumes
   from the DB tip, so ≤ threshold accepted blocks may ride in memory. Network runs keep
   the zero threshold (immediate-persist policy unchanged). Prerequisite fix in
   `EngineArgs::tree_config()`: apply `with_persistence_backpressure_threshold` *before*
   `with_persistence_threshold` — both setters assert against the config's current
   values, so 4096 used to panic against the default 16.
3. **`Syncing` is not a verdict** — the Engine API defines it as "re-send later".
   Import re-sends the same payload up to 5× (300 ms backoff, logged) and still stops on
   every other status; the chain checks above still gate what may be submitted. The
   5 retries never fired in the verification run.

**Evidence** (post-fix run, resumed at 10,430): mean 26–29 ms/block, **~37 blocks/s**
(was ~5), progress line `blocks_s=37.4 engine_ms=23.3 queue_ms=2.5 persist_ms=0.0
syncing_retries=0`, one 1.29 s stall per 4096-block batch commit, **passed the old
22,484 failure with 0 retries, 0 errors**, ctrl-c drain verified, disk freed by the
recovery truncation (see below) went 32 → 40 G free.

Note: the earlier `Syncing` exit ran as an *error* (marker absent), so the next boot
performed crash recovery and truncated the DB back to the file-tier watermark (10,429);
the gap re-imported at full speed. Gates: workspace check clean, clippy 301 ≤ 343
(budget of the day; `clippy.budget` is now 0),
tests green (tasks 4 incl. drain, engine-primitives 3, evm-ethereum 9, engine-tree 83
serial, node-ethereum 16 + it 4 + 1, cli 15).

## 7. Untouched by design

Existing mesh sync, validator, peer files, keystores, consensus rules, storage layout,
export. Import adds exactly one layer: **stream → decode → check → execute range → one
commit** (§1). RPC sync was removed entirely (`user-in.md` #5).

Reference: `IE.md` for RLP specs.

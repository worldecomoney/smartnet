# RLP Import — Findings and Proposed Plan


## 1. Delivery path — same as RPC / mesh sync

The only sanctioned way blocks enter this client:

1. Decode one block item from the stream:
   `execution_data_from_block_rlp(&bytes, chain_spec)`  
   (`crates/ethereum/engine-primitives/src/lib.rs:82`)
2. Submit through the consensus engine:
   `engine.new_payload(payload, /*wait_for_persistence=*/true, /*wait_for_caches=*/true)`  
   (`crates/engine/primitives/src/message.rs:306`, returns `(PayloadStatus, NewPayloadTimings)`)
3. Require `PayloadStatusEnum::Valid`. Anything else → **stop immediately, no retries**.

Proven mirror patterns (do not modify them; copy the call shape only):

- `crates/engine/rpc-sync/src/lib.rs:76-128` — `EngineBlockSink::import_blocks` (own crate since 2026-10-26: the RPC catch-up service is isolated there, import never calls it)
- `crates/ethereum/node/src/mesh.rs:155-172`
- `crates/ethereum/validator/src/validator.rs:202-221`

Rules derived from this:

- **No direct DB / file-tier writes.** The engine + existing providers carry insertion; multiple non-standard storages are their concern.
- **No RLP-shape concern.** How the engine re-encodes or stores is irrelevant; import only reads and delivers.
- **No second import layer.** Existing sync paths stay untouched; import is one extra caller.
- **No validator involvement.** Import works with the validator offline. No validation config, peer files, keystores, or RPC knobs in the import feature surface — those are boot concerns of the standard node launch only.

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

### 6.1 `crates/ethereum/cli/src/interface.rs`

- `ImportCommand { file: PathBuf }` (`#[derive(Debug, Parser)]`, positional path).
- `Commands::Import` variant (`#[command(name = "import")]`).
- Match arms: `chain_spec() → None`, `debug_namespace_enabled() → false`, `log_chain_directory_name` shares the None-arm DB chain-id read (`None | Some(Commands::Import(_))`).

### 6.2 `crates/ethereum/cli/src/app.rs` — arm in `run_commands_with` (dispatch at :731)

- New `Some(Commands::Import(cmd))` arm mirrors the existing None-arm boot: fail-fast `command.file.is_file()` → operator overlay → file tier → identity-from-DB → chain spec-from-DB → `operator_default_node_command`.
- Boot-critical keys only: `db.engine`, the **mesh** keys (`mesh_port`/`mesh_addr`/`mesh_peers_file`/`mesh_max_msg_mb`/`mesh_heartbeat_ms`), `cross_block_cache_size`, plus `node_cmd.validator_daemon = false` (below). No `heartbeat_ms` — the validator cadence does not exist for this launch.
  - **Correction to the original plan**: the mesh is *always on* — `mesh_gate` `std::process::exit(1)`s without `MESH_PORT`, and `load_mesh_identity` needs the operator config path. So `node_cmd.config` stays `Some(cfg_path)` exactly as the constructor sets it (the earlier idea of `config = None` would have killed the boot), and the mesh keys are stamped like a node run.
- Stays off: `RPC_SYNC` (master switch, default false — the isolated catch-up crate never starts during import) plus `SYNC_PEERS`/`SYNC_BATCH`/`SYNC_POLL_INTERVAL` (empty peers ⇒ `sync_gate` logs "not started" — non-fatal, no peer traffic competes with the stream), HTTP + every RPC quoting/gating/TTL knob (nothing submits transactions during import).
- **Validator — structurally disabled, not merely quiet.** New `validator_daemon: bool` on `NodeCommand`/`NodeConfig` (default `true`, so every other launch is byte-for-byte unchanged); import sets it `false`. The gate in `ethereum/node/src/node.rs` `launch_add_ons` becomes `ctx.config.validator_daemon && match config_path { … }` — import short-circuits *before* the config's `SEAL` key is read: no heartbeat task is ever spawned, no seal pass runs, no keystore is touched, zero `VALIDATOR_*` events. "No validator in import" is now impossible to violate by configuration; sync (the RPC sync service) never had validator involvement — both paths are pure engine feeds (`new_payload(_, true, true)`, `Valid` or stop), matching `EngineBlockSink`.
- Launch via `NodeCommand::execute(ctx, datadir_args, FnLauncher::new::<C,Ext>(launcher))`; the runner shuts the node down when the import loop returns.

### 6.3 `bin/smartnet/src/main.rs` (:24-31)

- Peek the parsed command before `cli.run(...)`:
  - `Some(file)` → `smartnet_node_ethereum::import::run_import(&handle.node.add_ons_handle.beacon_engine_handle, &handle.node.provider, &file)` — on success the launcher future resolves and the runner gracefully shuts the node down and exits (`new_payload(_, true, true)` keeps `wait_for_persistence=true`, so state is durable before exit).
  - else → `handle.wait_for_node_exit()` as today.

### 6.4 `crates/ethereum/node/src/import.rs` (new) + `pub mod import;` in `lib.rs`

- `read_rlp_item` — port of `.ref/rlp-main.go` `readRLPItem` (streaming, never loads the whole file): prefix byte → short/long length → payload; clean EOF on a boundary = `Ok(None)`, mid-item EOF / >64 MiB length = `Err` (a truncated chain never imports as complete).
- `run_import(engine, provider, file)`:
  1. `start = (best_block_number, sealed_header(best).hash())` — read **once**; missing tip header = "run `smartnet init`" error.
  2. Per item: `alloy_rlp::Decodable` → `smartnet_ethereum_primitives::Block` (trailing bytes rejected) → `classify(number, parent, start, accepted)`: genesis / `number <= start.0` ⇒ skip (resume); first accept must be `tip+1` with parent = tip hash; then strictly `prev+1` with parent = prev hash; anything else ⇒ stop with the block number + offset.
  3. `execution_data_from_block_rlp(block)` → `new_payload(_, true, true)` → engine contract identical to `EngineBlockSink`: `Valid` proceeds, anything else stops with `other:?` + `other.validation_error()` (no retries); record `(number, header.hash_slow())`.
  4. Progress `info!` per 10k scanned; final summary (scanned / imported / skipped / tip / `fmt_wall` elapsed — sync-style wall timing).
- Unit tests (10): reader boundaries (single byte, short string/list, 55↔56 form switch for strings *and* lists, clean-EOF vs truncation, 64 MiB cap, concatenated items), classify (genesis/resume skips, first-block parent/number, chaining, gap, wrong parent), and a block RLP round trip (encode `alloy_consensus::Block` → read → decode `smartnet_ethereum_primitives::Block`, the exact wire pair mesh/sync use).

### 6.5 Build / verify — DONE

- `CARGO_NET_OFFLINE=true cargo check --workspace` clean; `make clippy` → 300 warnings ≤ budget 343 (down 2: the import loop's `while let` / `is_multiple_of` fixes); targeted tests green (10 import + 15 cli + 16 node + 4 it); `rustfmt +nightly` on the five touched files.
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

### 6.7 Validation policy — only two checks (operator directive, 2026-10-25)

The fee fix carried the run through 17,214 / 17,259 / 17,939 / 18,424 / 19,018, then block
**19,545** (1 tx, 7,141-byte deployment) failed on `block gas used mismatch: got 1577054,
expected 1568954` — geth executed it under the chain's Istanbul-only schedule, this client
runs the collapsed ladder, so gas (and therefore receipts/state) cannot reproduce history.

**Directive: the node checks exactly two things, nothing more:**

1. **Previous block hash + block continuity** — `validate_header_against_parent` (parent
   hash/number/timestamp, gas-limit ramp) + import's `classify` chaining.
2. **Valid signature of the official validator** — `validate_authored_header` inside
   `validate_header` (`SEAL_CHECK`, runs for every block; passed on all 4M history items).

Skipped at the engine admission point (`engine/tree/src/tree/payload_validator.rs`,
identical for import + RPC sync + mesh — one shared path):

- `validate_block_post_execution` — **gas used**, receipt root, logs bloom, requests hash.
- The **state-root comparison** (both the main check and the parallel state-root task's
  "double check"; the latter previously forced a serial recompute fallback on every
  diverged block — removed, warn spam gone, ~1.4 → ~3.8 blocks/s).
- Execution still runs (state advances from the client's own computation); headers keep
  their historical field values, so hashes/linkage stay intact.

Unit gates: engine-tree 83/83 (serial; parallel runs have pre-existing fixture races —
`HeaderNotFound(0)`, different test each time, all pass isolated), workspace check clean,
clippy 301 ≤ 343.

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
2. **Batched persistence for import** — the import arm stamps
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
the gap re-imported at full speed. Gates: workspace check clean, clippy 301 ≤ 343,
tests green (tasks 4 incl. drain, engine-primitives 3, evm-ethereum 9, engine-tree 83
serial, node-ethereum 16 + it 4 + 1, cli 15).

## 7. Untouched by design

Existing mesh sync, RPC sync, validator, peer files, keystores, consensus rules, storage layout, export. Import adds exactly one layer: stream → decode → engine.

Reference: `IE.md` for RLP specs.

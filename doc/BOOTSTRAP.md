# SmartNet Boot Sequence (as implemented)

> **STATUS: CURRENT — 2026-11-24**
>
> This page describes the shipped startup path, not an idealized design. It is source-backed; stale comments are called out where they disagree with code.
> See [`DOC-INDEX.md`](DOC-INDEX.md) for authority and [`ARCHITECTURE.md`](ARCHITECTURE.md) for the component view.

## Ordering note — read first

The requested phase labels are kept as sections below, but they are **not** all part of a normal `smartnet` startup:

- `init` and `import` are **separate CLI subcommands**, not normal boot phases.
- Config validation actually starts **before** the keystore.
- Mesh rumor relays are spawned **before** the boot sync wait.
- RPC modules are staged early, but the HTTP transport binds **after** the sync gate.
- The heartbeat task is spawned **last**, after RPC bind.

Actual normal-node order:

```text
CLI parse
→ operator config overlay validation
→ fixed gas price global
→ keystore guard
→ both bootstrap files + self endpoint check
→ txpool / RPC entry-gate policy globals
→ datadir + DB chain identity + genesis spec
→ CliRunner → NodeCommand::execute
→ engine crash recovery
→ shutdown final_protocol task registered
→ provider / genesis / components / pipeline / validator / consensus engine
→ staged RPC modules (not bound)
→ mesh bind / identity / preconnect
→ block relay + tx relay spawned
→ boot sync gate wait
→ RPC HTTP bind
→ heartbeat task
```

`smartnet init` runs before any normal boot and prepares the datadir. `smartnet import` imports a chain file, then boots a normal node on success.

---

## Keystore

**Position in normal boot:** after the operator config overlay and fixed-price stamping, before datadir/DB work.

1. `ensure_keystore_ready(&cfg_path)` at `smartnet/crates/node/cli/src/app.rs:1023`.
2. Function body at `app.rs:228-250`:
   - load the validator config section;
   - require `PASSWORD_FILE` (headless operation, blank password rejected);
   - use `COINBASE` as the keystore selector;
   - load `KeystoreMaterial` and return the key-derived address.
3. `KeystoreMaterial::load` at `smartnet/crates/node/validator/src/keystore_util.rs:136-141` reads the password, selects the encrypted JSON v3 file, reads encrypted bytes, and derives the address.
4. Enforcement at `keystore_util.rs:263-308`: version must be `3`, cipher `aes-128-ctr`, MAC checked, plaintext key files refused. No decrypted private key is retained; only encrypted bytes plus a zeroizing password remain (`keystore_util.rs:145-173`).

The same keystore is loaded again by mesh identity (`smartnet/crates/node/ethereum/src/mesh.rs:209-224`) and by the validator daemon when `SEAL = true` (`smartnet/crates/node/validator/src/lib.rs:207-227`).

`smartnet init` and `smartnet import` also call `ensure_keystore_ready` before their own work.

---

## Init

**Position:** not part of normal boot. `smartnet init` is a separate CLI arm (`app.rs:829-836`) that must have already run.

`InitCommand::execute` at `smartnet/crates/node/cli/src/commands/init_cmd.rs:88-174`:

1. Log start (`:93`).
2. Keystore first: `ensure_keystore_ready(global_config)` (`:95-97`).
3. Load config, `HOME_DIR_PATH`, `DATADIR_PATH`, `DB_ENGINE` (`:99-134`).
4. Require the datadir to be empty (`:136`).
5. Validate genesis seal config (`:138-139`).
6. Initialize DB/genesis via `EnvironmentArgs::init` (`:141-149`).
7. Write `SHUTDOWN_CLEAN` for the fresh datadir (`:158-166`).

Normal startup only verifies that init happened: `ensure_chain_datadir_initialized` at `smartnet/crates/node/cli/src/commands/node.rs:180-209` exits `1` when the datadir is missing or empty. Chain identity and genesis spec are read from the DB, not from config (`app.rs:660-701`, `app.rs:1105-1136`).

---

## Import

**Position:** separate CLI arm. It imports first, then boots a normal node on success.

Import arm order (`app.rs:842-1007`):

1. Resolve config path and check the chain file is a regular file (`:843-855`).
2. `load_operator_overlay` (`:856`) and fixed gas price global (`:859-861`).
3. `ensure_keystore_ready` (`:864`).
4. Resolve and load `validators.jsonl` + `mainnet.jsonl` (`:865-873`).
5. `ensure_mesh_self_consistent` (`:877-883`).
6. Datadir, file-tier base, DB chain identity (`:886-894`).
7. `bootstrap_network_settings`; missing genesis spec exits `6` (`:895-905`).
8. Chainspec from DB, default node command with validator disabled (`:907-913`).
9. Stamp mesh keys and persistence overrides (`:921-944`).
10. Enter runner and run `run_import` on the blocking pool (`:952`, `:991-996`).
11. If stopped by signal, return `Ok(())` without booting (`:998-1002`).
12. Otherwise call `node_cmd.execute` to boot a normal node (`:1003-1004`).

`run_import` at `smartnet/crates/node/ethereum/src/import.rs:360-642`:

1. Register drain and importing guard (`:382-384`).
2. Fail-closed validator registry load (`:386-407`).
3. Open chain file and anchor on the durable DB tip (`:409-426`).
4. Build `StorageEngine` (`:445-446`).
5. Read/decode/classify RLP items one at a time (`:462-523`).
6. Execute range, compute trie updates once, write via `engine.save_blocks`, join seals (`:530-565`).
7. Report committed blocks only, final joins, `shutdown_clean` (`:592-622`).

**Stale comment:** `smartnet/bin/smartnet/src/main.rs:24-28` says the launcher has nothing left to do after import. In reality `app.rs:1003-1004` boots a full node after a successful import.

---

## Config checks

**Position:** started first, before the keystore. Validation is split across several places.

### Operator overlay

`load_operator_overlay` at `app.rs:252-636`:

- Reads the config file and collects all problems rather than failing on the first (`:253-258`).
- Unknown keys are hard errors (`:370-378`); blank values are hard errors except config-owned data files and blank-tolerant keys (`:382-393`).
- RPC-disabled roles force `RPC_SERVER=false` (`:290-305`, `:488-490`).
- Resolves `mainnet.jsonl` (`:451-462`, `:492-501`).
- Required keys: `DATADIR_PATH`, `FIXED_GAS_PRICE_WEI`, `GPO_DEFAULT_SUGGESTED_FEE`, `CROSS_BLOCK_CACHE_SIZE`, `MESH_PORT`, `MESH_ADDR`, plus `HTTP_PORT`/`HTTP_ADDR` for plain node roles (`:511-533`).
- Sanity checks: `HTTP_PORT != 0` (`:538-543`), fixed price > 0 (`:544-546`), `TXPOOL_MARGIN` 0..=5 (`:547-553`), queued TTL 1..=600 (`:554-565`), `TXPOOL_VAL_MAX_TXS != 0` (`:566-572`), GPO fee > 0 (`:573-581`), `NODE_HEARTBEAT != 0` (`:587-593`), `MESH_MAX_MSG_MB != 0` (`:594-599`), `MESH_PORT >= 1024` and no HTTP collision (`:600-616`), `CROSS_BLOCK_CACHE_SIZE != 0` (`:617-622`).
- Aggregated bail at `:624-633`.

### Bootstrap files and self check

- `ensure_bootstrap_files_loaded` requires and loads both files; duplicate address/endpoint rejection in `smartnet/crates/node/ethereum/src/bootstrap_files.rs:71-100`.
- `ensure_mesh_self_consistent` (`app.rs:877-883`) checks the self-record against `MESH_ADDR`/`MESH_PORT`; see `bootstrap_files.rs:102-172`. No loopback/default fallback in the operator path.

### Policy globals and DB identity

- Fixed price global (`app.rs:1016-1018`), RPC entry gate globals (`:1047-1057`), batch policy (`:1060-1075`), txpool policy (`:1079-1095`).
- DB is the single source for chain ID, validators, and genesis spec (`app.rs:1105-1136`). Failures call `process::exit(err.exit_code())` (`:1117-1123`); missing genesis spec exits `6` (`:1129-1135`).
- `NodeCommand::execute` stamps the engine datadir, runs `engine.validate()`, checks init, builds `NodeConfig`, opens DB, and creates `NodeBuilder` (`smartnet/crates/node/cli/src/commands/node.rs:64-132`).

**Stale comments:** `app.rs:444-446`, `app.rs:1163`, and `smartnet/crates/node/core/src/args/rpc_server.rs:329-332` describe mesh as off when `MESH_PORT` is absent/0. Current code requires explicit `MESH_ADDR`/`MESH_PORT`, rejects ports below 1024, and mesh is always on (`app.rs:511-518`, `:582-616`; `mesh.rs:78-97`).

---

## Mesh sync

RPC modules are staged before mesh starts; the mesh service is then spawned, and the boot sync gate is waited on **after** the rumor relays are armed.

### Mesh service spawn

`spawn_mesh_service` at `smartnet/crates/node/ethereum/src/mesh.rs:251-394`:

1. `mesh_gate` requires `MESH_PORT` present and >= 1024 (`:86-97`, called `:268-271`).
2. Resolve peer file (`:273`).
3. Bind TCP listener (`:275-285`).
4. Load limits, then mesh identity from keystore (`:289-304`).
5. Resolve `validators.jsonl` path for the config-hash gate (`:306-318`).
6. Build `NodeInjector`, caches, spreader, server (`:320-351`).
7. Read peer targets, seed expected gate set, preconnect (`:353-360`).
8. Spawn inbound mesh WS server (`:362-377`).
9. Spawn block relay (`:379-386`) and tx relay (`:388-391`).

Misconfiguration and bind failures return `eyre` errors rather than calling `process::exit`, so the runner can fire graceful shutdown (`mesh.rs:265-285`).

### Outbound link session

`run_link_session` at `smartnet/crates/net/mesh/src/ws.rs:1508-1595`:

1. TCP/WebSocket connect (`:1523`).
2. Load allowlist and signed HELLO handshake (`:1524-1530`).
3. Config-hash gate (`:1531-1548`).
4. Lift pre-auth cap and mark connected (`:1549-1551`).
5. `bootstrap_sync` (`:1552-1562`).
6. Report boot result to the gate (`:1563-1575`).
7. Enter the session loop (`:1576-1588`).

Config hashes are `keccak256` over the raw bytes of both files, recomputed on every call and never cached (`smartnet/crates/net/mesh/src/config_hash.rs:24-38`). `bootstrap_sync` at `ws.rs:1381-1447` requests status, then fetches `local_tip + 1` one block at a time through the ordinary block inject path.

### Boot sync gate

- Module: `smartnet/crates/net/mesh/src/sync_gate.rs`; `BOOT_READY_TIMEOUT = 10s` (`:24-26`).
- Ready when expected peers are all reported, local tip >= max peer tip, and no pull is armed (`:131-149`).
- `wait_ready` returns `false` on timeout or channel drop (`:152-167`).
- Call site `smartnet/crates/node/ethereum/src/node.rs:422-436`. Timeout logs a warning and **proceeds** (`:431-436`).

---

## Mesh rumor

**Position:** relay tasks are spawned during mesh startup, **before** the boot sync wait above.

- Block relay: spawned at `mesh.rs:379-386`, function `spawn_block_relay` at `:397`. Canonical state stream → hash + full-block RLP → mark/store → spread.
- Tx relay: spawned at `mesh.rs:388-391`. Pool events → EIP-2718 bytes → mark/store → spread.
- `NodeInjector` at `mesh.rs:105-203`: transactions via `EthApiServer::send_raw_transaction` (`:135-151`), blocks via engine `new_payload` (`:154-171`), block RLP served via `eth_getBlockByNumber` (`:179-202`). No loopback socket.
- Block injection is serialized by a global one-permit gate (`smartnet/crates/net/mesh/src/ws.rs:100-107`, applied `:959-973`).

---

## RPC

**Position:** HTTP transport binds after the sync gate passes or times out. The boot
outcome is reported as `boot_sync=confirmed|unconfirmed` in the status heartbeat; a
timeout stays unconfirmed and is never reported as full synchronization.

1. Modules are staged without binding at `smartnet/crates/node/ethereum/src/node.rs:375-403`; staging helper `prepare_add_ons_with` at `smartnet/crates/node/builder/src/rpc.rs:1080-1092`.
2. `staged.eth_api()` is cloned for mesh and heartbeat use (`node.rs:405-420`).
3. Bind happens at `node.rs:438-439` via `StagedRpcAddOns::serve` (`rpc.rs:499-540`) and `launch_rpc_server_internal` (`:544-558`).
4. HTTP only: `smartnet/crates/rpc/rpc-builder/src/config.rs:105-116,129-151` never mounts WS RPC.
5. Production namespace allowlist is `eth`, `net`, `web3`, `rpc`, and `txpool` only when `RPC_MEMPOOL` is enabled (`smartnet/crates/rpc/rpc-server-types/src/module.rs:425-437`).

**Stale comments:** the RPC catch-up service is removed (`node.rs:337-340`), but comments at `node.rs:718-722` still describe block acquisition over `rpc-sync`. WS/HTTP auth comments at `smartnet/crates/node/builder/src/rpc.rs:1078-1079,1098-1099` do not match the HTTP-only binding.

---

## Heartbeat

**Position:** spawned last, after RPC bind (`smartnet/crates/node/ethereum/src/node.rs:438-448`).

1. Interval comes from `NODE_HEARTBEAT`, default `DEFAULT_HEARTBEAT_MS` (`node.rs:357-359`, `:480-481`).
2. The validator daemon is loaded inside the task only when `validator_enabled` (`node.rs:449-470`).
3. "Heartbeat started" log at `node.rs:472-478`.
4. Each tick runs the validator beat (`:490-492`) and a status line every
   `REPORT_INTERVAL` (`:493-508`) carrying `blockNr - X nodes connected - boot catch-up
   confirmed|unconfirmed`. The status line is suppressed during import (`:494`).

Mesh keepalive is **not** driven by this node heartbeat. It uses the protocol's own PING/PONG and bounded reconnect (`smartnet/crates/node/ethereum/src/mesh.rs:289-293`; [`MESH-PAYLOAD-PROTOCOL.md`](MESH-PAYLOAD-PROTOCOL.md) § "CURRENT — Layer 1 — Keepalive and generic acks"). `doc/HEARTBEAT.md` is stale where it describes mesh/bridge hooks on the one node heartbeat.

---

## Error paths and final shutdown protocol

- `CliRunner::run_command_until_exit` fires graceful shutdown on **both success and error** (`smartnet/crates/node/cli/src/runner.rs:60-98`, `:81-93`), with a 5s graceful timeout (`:193-215`), SIGINT/SIGTERM handling (`:248-318`), and a 60s drain timeout (`:320-349`).
- The `final_protocol` task is registered only **after** engine crash recovery (`smartnet/crates/node/builder/src/launch/engine.rs:44-60`, `:186-201`). The runner fires the graceful signal on success, command error and ctrl+c alike (`runner.rs:81-95`, `:131-139`): it waits for every other graceful task to finalize first, then flushes the file tier and writes `SHUTDOWN_CLEAN` with its timestamp LAST (`engine.rs:44-60`). Only then does the runner drop the runtime; a timeout leaves no marker and the next boot unwinds. Recovery is `StorageEngine::recover_on_boot` (`smartnet/crates/storage/provider/src/storage_engine.rs:475-489`): it skips unwind when `SHUTDOWN_CLEAN` exists, otherwise restores the unwind.
- Therefore: errors from `NodeCommand::execute` onward get runner graceful shutdown; earlier CLI validation errors (config overlay, keystore, bootstrap files, self-check, DB identity) are hard errors before the runner and cannot write the marker. `ensure_chain_datadir_initialized` exits directly inside `NodeCommand::execute` (`commands/node.rs:195`, `:207`), bypassing runner graceful shutdown.
- Final error surface: `smartnet/bin/smartnet/src/main.rs:43-47` prints the error and exits `1`.

### `final_protocol()` — descension sequence spec

One shutdown path for every started action. Source: `smartnet/crates/node/builder/src/launch/engine.rs:48-63`, `:183-201`; guard counter at `smartnet/crates/tasks/src/runtime.rs:600`; runner at `crates/node/cli/src/runner.rs:60-98`, `:103-139`, `:193-215`, `:352-371`; marker at `smartnet/crates/storage/provider/src/storage_engine.rs:475-507`.

**Triggers — same protocol, no special cases:** command success, command error from `NodeCommand::execute` onward, and SIGINT/SIGTERM. All three reach `CliRunner::graceful_shutdown_with_timeout` (`runner.rs:93`, `:137`) before `runtime_shutdown` (`runner.rs:358`). Errors before the runner and before registration (config overlay, keystore, bootstrap files, self-check, DB identity) are hard errors and deliberately write no marker.

**Registration:** immediately after `bootstrap_seq_20_recovery` (`engine.rs:183-201`). `recover_on_boot` has just cleared `SHUTDOWN_CLEAN`, so this run's exit decides the next boot's path.

**Participants:** graceful guard holders are the consensus engine task (`Terminate` → tree persistence, `engine.rs:417`), the local transaction backup task (`components/pool.rs:264`), and `final_protocol` itself. `TaskExecutor::graceful_tasks_remaining()` reports the live count (`runtime.rs:600`).

**Sequence:**

1. The task manager fires the graceful signal; `final_protocol` wakes holding its own guard.
2. It waits in 10 ms sleeps — no busy loop — until the guard count drops to `1`, i.e. every other started graceful action has finalized.
3. It constructs `StorageEngine` over the node's provider factory.
4. With the file-tier base set, `shutdown_clean(base)` flushes the file tier, then writes `SHUTDOWN_CLEAN` with its timestamp as the **last** durability event (`storage_engine.rs:493-507`).
5. The task returns and drops its guard (count `0`); only then does the runner drop the runtime on the `rt-shutdown` thread with a 5 s wait (`runner.rs:358-371`).
6. If the runner's 5 s graceful timeout or 5 s runtime-drop timeout elapses, no marker is guaranteed and the next boot takes the unwind path.

**Invariants:**

- `SHUTDOWN_CLEAN` is never written before engine persistence and backup work finish.
- A timeout or hard crash leaves no marker; `recover_on_boot` restores the unwind.
- Missing file-tier base or a `shutdown_clean` error is logged and leaves no marker.
- Import has its own equivalent ordering: `join_seal_tasks()` → `shutdown_clean()` (`smartnet/crates/node/ethereum/src/import.rs:618-622`).
- No EVM/executor, consensus, RPC-name, tx-type, or protocol behavior is changed by this sequence.

---

## Links

- [`ARCHITECTURE.md`](ARCHITECTURE.md) — component map and runtime flows.
- [`DESIGN-DECISIONS.md`](DESIGN-DECISIONS.md) — why boot, mesh, and RPC are split this way.
- [`MESH-PAYLOAD-PROTOCOL.md`](MESH-PAYLOAD-PROTOCOL.md) — normative wire spec for HELLO, config gate, sync, and keepalive.
- [`SECURITY_ARCHITECTURE.md`](SECURITY_ARCHITECTURE.md) — what the boot/identity checks do and do not protect.
- [`ORIGIN.md`](ORIGIN.md) — transaction origin labels for RPC and mesh injection.
- [`TXPOOL-CLENSING-ON-BLOCK-PROP-ACCEPT.md`](TXPOOL-CLENSING-ON-BLOCK-PROP-ACCEPT.md) — post-canonical pool lifecycle.
- [`FEES-CURR.md`](FEES-CURR.md) — fixed native fee policy stamped during config checks.
- [`TEST-RESULTS.md`](TEST-RESULTS.md) — dated test artifact and limits.
- [`COMPATIBILITY.md`](COMPATIBILITY.md) — evidence-backed status matrix.
- [`ROADMAP.md`](ROADMAP.md) / [`CHANGELOG.md`](CHANGELOG.md) — current status and dated history.
- [`DOC-INDEX.md`](DOC-INDEX.md) — authority and freshness map.
- [`README.md`](../README.md) — build and run entry point.

# SmartNet Design Decisions and Trade-offs

> **STATUS: CURRENT — 2026-11-24**
>
> This is a decision log, not a security proof. Each entry records what is shipped, why it was chosen, the consequence, the rejected option, and anything that must not be restored.
> Evidence tags: **[impl]** = current code/config; **[policy]** = operator doctrine recorded but no full ADR exists; **[future]** = design only.

## DD-01 — Separate private EVM network, not Ethereum legacy

- **Status:** Implemented **[impl]**.
- **Rationale:** SmartNet wants EVM/DeFi compatibility without Ethereum network/fork/governance compatibility. The node set, fees, and rules stay operator-controlled.
- **Consequence:** Familiar contracts and wallets work; Ethereum mainnet/testnet behavior, P2P discovery, and city forks do not.
- **Rejected option:** Track Ethereum hardforks and remain network-compatible.
- **Security implication:** The trust boundary is the operator config and closed peer list, not a public permissionless network.
- **Must not restore:** Ethereum fork/city-gate legacy.

## DD-02 — Mesh instead of P2P/KAD/NAT traversal

- **Status:** Implemented **[impl]**, rationale is operator doctrine **[policy]**.
- **Rationale:** The network is closed-list and deliberately small. A fixed peer set removes discovery, NAT traversal, and gossip-graph complexity. The mesh is fully operational and considered sensitive.
- **Consequence:** Operators must maintain `mainnet.jsonl`; a missing peer file disables outbound gossip but does not take down the inbound listener. A busy mesh port is a boot failure.
- **Rejected option:** Ethereum devp2p/KAD with dynamic discovery.
- **Security implication:** Membership is explicit, not discovered. There is no anonymous inbound peer set.
- **Must not restore:** P2P, KAD, NAT traversal.

## DD-03 — Signed HELLO identity replaces JWT/auth RPC

- **Status:** Implemented **[impl]**. JWT removal is operator doctrine **[policy]**; no separate detailed ADR is committed.
- **Rationale:** Peers authenticate each other with the node's keystore key before any protocol frame. The signed HELLO proves key control and rejects unlisted or replayed peers.
- **Consequence:** Identity is per-connection and local; roles/permissions are not exchanged on the wire. Consensus roles come from node config/DB.
- **Rejected option:** JWT bearer tokens and a separate auth/secure RPC layer.
- **Security implication:** Replay cache, allowlist, chain ID, and signature checks happen before Layer ≥ 3 traffic.
- **Must not restore:** JWT or auth/secure RPC.

## DD-04 — HTTP JSON-RPC only; WebSocket is binary Mesh

- **Status:** Implemented **[impl]**; operator ruling recorded 2026-11-19 **[policy]**.
- **Rationale:** Keep wallet-facing RPC boring and HTTP-only; keep the differentiating transport separate from RPC. Binary Mesh frames are RLP/protocol-specific and never carry JSON-RPC.
- **Consequence:** MetaMask uses HTTP. Mesh tools must speak the binary protocol in [`MESH-PAYLOAD-PROTOCOL.md`](MESH-PAYLOAD-PROTOCOL.md).
- **Rejected option:** Serving JSON-RPC over WebSocket or mixing RPC and mesh opcodes.
- **Security implication:** The two transports have separate parsers, limits, and failure modes.
- **Must not restore:** WebSocket RPC protocol.

## DD-05 — Single binary with in-process validator

- **Status:** Implemented **[impl]**; historical rationale exists in `1.md` but is not authority **[policy]**.
- **Rationale:** Avoid a separate validator process, separate RPC communication, and process coordination. The validator reads the local pool and submits to the local engine.
- **Consequence:** Simpler deployment and one config. `SEAL = true` enables the in-process seal loop; `NODE_HEARTBEAT` is the one cadence reused by services.
- **Rejected option:** External validator daemon talking to the node over RPC.
- **Security implication:** No validator RPC surface is exposed; signing stays inside the node process.
- **Must not restore:** Separate validator RPC coordination as a required architecture.

## DD-06 — File tier + Fjall instead of MDBX/RocksDB

- **Status:** Implemented **[impl]**; deeper rationale is doctrine **[policy]**.
- **Rationale:** Keep storage simple and separated from Ethereum database conventions. `StorageEngine` owns the ordered write protocol and the only unwind path.
- **Consequence:** Write order is fixed: FS append → DB index/state/trie → one fsync → trim. Clean shutdown writes a marker; crash recovery trims the file tail.
- **Rejected option:** MDBX, RocksDB, and multiple independent unwind implementations.
- **Security implication:** Fewer storage engines and one crash-recovery protocol.
- **Must not restore:** MDBX or RocksDB.

## DD-07 — Mars-only mixed rulebook

- **Status:** Implemented **[impl]**; doctrine in `AGENTS.md` and `TOD.md` **[policy]**.
- **Rationale:** Rules are selected for this private network, not inherited by fork date. Historical fee fields are not used to decide modern admission.
- **Consequence:** Import/sync can carry Istanbul/London-era fields for execution only; current wallet txs follow the announced fixed price. Older `doc/MARS.md` statements about `0x1` are stale and superseded by current code.
- **Rejected option:** City-gated fork schedule.
- **Security implication:** Operators control current policy instead of chain history deciding admissibility.
- **Must not restore:** City-fork gating as a network rule.

## DD-08 — Fixed native fee; ERC-20 economy untouched

- **Status:** Implemented **[impl]**; current rules in [`FEES-CURR.md`](FEES-CURR.md).
- **Rationale:** Native coin transfers have a predictable announced price. ERC-20/DeFi keep ordinary `gasUsed × gasPrice` behavior with no contract special-casing.
- **Consequence:** `FIXED_GAS_PRICE_WEI` drives native quotes/headers/wallet acceptance; `TXPOOL_MARGIN` is only an entry filter, never block assembly or acceptance.
- **Rejected option:** Auction-style native base fee and contract-dependent fee exceptions.
- **Security implication:** Fee policy is explicit and testable; historical imported headers are not trusted as current price policy.
- **Must not restore:** Ethereum legacy fee-market behavior for native transfers.

## DD-09 — `0x0`/`0x2` only; MetaMask behavior preserved

- **Status:** Implemented **[impl]**.
- **Rationale:** Support the two transaction envelopes used by the wallet surface; refuse `0x1` and `0x3+` at the shared RPC/mesh submit gate.
- **Consequence:** MetaMask keeps working with familiar `eth_*` names and tx types. Other envelope types are not a compatibility target.
- **Rejected option:** Accepting all EIP-2718 types or renaming RPC methods.
- **Security implication:** One narrow admission seam for wallet and mesh transactions.
- **Must not restore:** `eth_*` renames, tx types outside `0x0`/`0x2`, or MetaMask-breaking changes.

## DD-10 — Interim quorum `=1`; future 2/3 design kept separate

- **Status:** Current runtime implemented **[impl]**; 2/3/BT/minority-report material is future **[future]**.
- **Rationale:** Bootstrap must work with one validator. The current seal gate checks that one recovered signer equals the SELECT chair.
- **Consequence:** Multi-validator registration and chair selection exist, but there is no shipped multi-signature quorum certificate, no minority reports, and no tested 33%/67% behavior.
- **Rejected option:** Blocking bootstrap on a not-yet-implemented multi-sig quorum protocol.
- **Security implication:** Do not claim BFT guarantees. See [`quorum-doc.md`](quorum-doc.md) only as design material.
- **Must not restore:** Do not change `calibri-lite` consensus behavior without explicit permission.

## DD-11 — Config files as bootstrap truth; config hash gate on links

- **Status:** Implemented **[impl]**.
- **Rationale:** Both `validators.jsonl` and `mainnet.jsonl` must be present and loaded at boot. Outbound links compare raw whole-file hashes after HELLO.
- **Consequence:** Byte-level config equality is required between peers. Comments and whitespace count. A mismatch rejects the peer in memory for the process lifetime; no file is written or synced.
- **Rejected option:** Trusting peer-provided config or auto-fixing mismatched files.
- **Security implication:** Prevents a node from gossiping with a peer that has a different validator/peer set.
- **Must not restore:** File sync/push, cached hashes, or skipping the gate on heartbeat/spread.

## DD-12 — Endpoint self-check with no defaults

- **Status:** Implemented **[impl]**.
- **Rationale:** Node identity must match one explicit self-record; neither config nor file is authoritative alone.
- **Consequence:** `MESH_ADDR`/`MESH_PORT` and the self-record `ip`/`port_range` must agree. Missing/wrong values are hard errors; there is no `127.0.0.1`/`41711` fallback.
- **Rejected option:** Loopback defaults, port inference, or file-over-config precedence.
- **Security implication:** Prevents a node from accidentally advertising an endpoint it did not bind.
- **Must not restore:** Hardcoded self endpoints or silent fallback.

## DD-13 — Clean shutdown on bootstrap errors

- **Status:** Implemented **[impl]** (2026-11-24).
- **Rationale:** A failed boot is still a shutdown. If graceful tasks do not run, `SHUTDOWN_CLEAN` is missing and the next boot rewinds the chain.
- **Consequence:** Runner fires graceful shutdown on success and error; `final_protocol` is registered immediately after recovery and writes the marker only after every other started graceful task has finalized; mesh fatal paths return errors instead of `process::exit`.
- **Rejected option:** Hard-exit on mesh/config boot failure; skip marker until all components attach.
- **Security implication:** Avoids false crash recovery and unintended state regression.
- **Must not restore:** Error paths that bypass graceful shutdown after recovery.

## DD-14 — KISS and frozen core

- **Status:** Standing operator rule **[policy]**.
- **Rationale:** The project optimizes for a small efficient private-network client, not general Ethereum client completeness.
- **Consequence:** Do not over-engineer, do not fix what is not broken, and collect ideas in TODO before acting. EVM/executor, contract core, ERC-20 behavior/fees, DeFi V2, RPC names, tx types, and mesh basics are frozen.
- **Rejected option:** Broad rewrites or opportunistic background fixes.
- **Security implication:** Small surface area and reviewable changes.
- **Must not restore:** Any removed subsystem listed in [`ARCHITECTURE.md`](ARCHITECTURE.md) without explicit operator permission.

# SmartNet Security Architecture and Threat Model

> **STATUS: CURRENT — 2026-11-24**
>
> This is the security authority for mesh identity, handshake, authorization, and current limitations. It describes shipped behavior only; design-only quorum material is not presented as protection.
> Normative wire details are in [`MESH-PAYLOAD-PROTOCOL.md`](MESH-PAYLOAD-PROTOCOL.md) (CURRENT sections). Doc authority and freshness are in [`DOC-INDEX.md`](DOC-INDEX.md).

## 1. Scope and authority

- **`doc/SECURITY_ARCHITECTURE.md` (this page)** is authoritative for what SmartNet protects and does not protect today.
- **`doc/MESH-PAYLOAD-PROTOCOL.md`** is authoritative for the binary Mesh protocol. Its `OLD / PLANNED` section is explicitly historical, "not current behavior" and "not implemented or normative".
- **`doc/SECURITY.md` is historical/partial.** It is useful for old mempool/DeFi notes only. Its comparative claims ("100% DeFi compatible", "more secure than Ethereum", "Sybil resistant") are self-labelled aspirational and audit-pending, and its source paths (e.g. `crates/ethereum/validator/...`) are stale. Do not use it for mesh security or as a guarantee.
- **No independent audit exists.** Nothing in this document is an audit result. `Tested (unit)` references point to [`TEST-RESULTS.md`](TEST-RESULTS.md) (TR-2026-11-24-01); a named unit test confirms only the behavior it exercises.

## 2. Subject table

Status legend: **Implemented** = shipped mechanism; **Partial / local** = shipped but bounded or process-local; **Not implemented** = no current code path; **By design** = deliberate non-goal.

| Subject | Status | Current mechanism | Boundary / limitation | Evidence |
| --- | --- | --- | --- | --- |
| Node identity | Implemented | Mesh uses the node keystore; `Identity` derives an address from encrypted key material. HELLO signing drops the signing key immediately. | Signed HELLO proves **key control only**. It grants no roles and does not prove personhood, trustworthiness, or absence of compromise. Mesh authorization reads `mainnet.jsonl` `active && valid`; seal authorization reads `validators.jsonl` `status: true`. | `smartnet/crates/net/mesh/src/identity.rs:4-7,22-32,44-57,60-133`; `smartnet/crates/node/ethereum/src/mesh.rs:205-223`; `smartnet/crates/net/mesh/src/peers.rs:250-277`; `smartnet/crates/node/validator/src/run_once.rs:512-522` |
| Handshake authentication | Implemented | First frame must be a signed HELLO (178-byte payload). Digest binds domain, version, network id, both addresses, and both nonces. Verification checks version/network, allowlist, signature recovery, low-`s`, and `v ∈ {0,1}`. Client checks echoed nonce, initiator address, and non-zero responder nonce. | Failed handshakes close only that connection. No blacklist or reconnect ban. Handshake authenticity is not session encryption. | `smartnet/crates/net/mesh/src/protocol.rs:182-247`; `smartnet/crates/net/mesh/src/identity.rs:139-247`; `smartnet/crates/net/mesh/src/ws.rs:837-920,1252-1290` |
| Encryption status | **Not implemented** | Binary Mesh frames travel over WebSocket; text frames are rejected. | Current doc states there is **no TLS requirement** and transport security is optional/separate. Code dials plaintext `ws://`. Do **not** advertise "encrypted mesh". | `doc/MESH-PAYLOAD-PROTOCOL.md:408-413`; `smartnet/crates/net/mesh/src/peers.rs:65-68`; `smartnet/crates/net/mesh/src/ws.rs:1890-1948` |
| Replay protection | Partial / local | Random 32-byte nonces. Responder rejects a cached initiator nonce before signature recovery and inserts only after successful verification. Client rejects echo/initiator mismatch and zero responder nonce. Cache cap `4096` with bounded FIFO eviction. | Process-local and non-durable. Cache is keyed by initiator nonce alone; after restart or eviction old nonces are forgotten. Not a global anti-replay guarantee. | `smartnet/crates/net/mesh/src/identity.rs:286-330`; `smartnet/crates/net/mesh/src/ws.rs:97-98,867-916,1254-1271`; `doc/MESH-PAYLOAD-PROTOCOL.md:432-438` |
| Peer authorization | Implemented | `mainnet.jsonl` fields include address/key-id, IP, port, `active`, `valid`. Gossip targets and inbound allowlist require `active && valid` and not-self. Peer file is re-read at each handshake. Cheap allowlist check runs before signature recovery. | Allowlist membership is not reputation or scoring. A file address is a claim until backed by HELLO. The optional peer-file header is read but never checked. | `smartnet/crates/net/mesh/src/peers.rs:32-58,70-80,92-99,130-160,250-277`; `smartnet/crates/net/mesh/src/identity.rs:208-225`; `smartnet/crates/net/mesh/src/ws.rs:837-854,1012-1027` |
| Revocation | Partial / local | Setting `active: false` or `valid: false` removes a peer from the inbound allowlist on the next handshake/reload. Config-hash mismatch marks `ip:port` rejected in memory for the process and filters it from future spreads. | **File/config-driven and process-local.** No live session kick: handshake happens once, then the session loops. Config-hash rejections clear on restart. No persistent ban/blacklist for handshake or list failures. | `smartnet/crates/net/mesh/src/peers.rs:254-259`; `smartnet/crates/net/mesh/src/ws.rs:253-282,837-854`; `smartnet/crates/net/mesh/src/link.rs:41-58,246-250`; `doc/MESH-PAYLOAD-PROTOCOL.md:464-470` |
| Keystore | Implemented | Encrypted JSON v3 (AES-128-CTR, PBKDF2/scrypt, MAC check). Plaintext keystores are refused; the writer always encrypts. Password file is required headless and blank passwords fail. In-RAM encrypted bytes plus `Zeroizing` password; no retained signing key. Password writer uses mode `0o600`. | Security still depends on host file permissions and password-file protection. The keystore writer checks file existence but does not itself enforce a keystore file mode. | `smartnet/crates/node/validator/src/keystore_util.rs:65-90,109-119,162-173,176-192,229-249,263-332`; `smartnet/crates/node/validator/src/lib.rs:219-223` |
| Consensus / quorum assumptions | Implemented as interim single-signature gate | Interim threshold is `1`. Chair selection uses `status: true` validators (with a one-validator detour). `check_seal` recovers one signature and requires recovered signer == chair. Import and sync call the same gate. | **No shipped BFT, 2-of-3 quorum, minority reports, fishermen, slashing, or 33%/67% behavior.** Future `n/2+1` text is design only. Stale comments saying any authorized validator may seal do not match the runtime chair gate. | `calibri-lite/calibri-lite-consensus/src/consensus/calibri/mod.rs:136-147`; `calibri-lite/calibri-lite-consensus/src/select.rs:159-225`; `smartnet/crates/node/ethereum/src/import.rs:705-713`; `smartnet/crates/consensus/ethereum/src/beacon_pre_consensus.rs:33-50`; `doc/DESIGN-DECISIONS.md` (DD-10) |
| ChainID replay protection | Implemented as local admission/seal policy | HELLO digest includes network id and verification requires the expected network id. Txpool rejects a declared chain-id mismatch. Validator seal selection rejects missing or wrong chain id. | These are **local admission and seal policies**, not a global cross-chain replay proof. The txpool check applies only when `chain_id()` is `Some`; the validator seal filter is the stronger check for a missing chain id. | `smartnet/crates/net/mesh/src/identity.rs:69-76,139-161,192-197`; `smartnet/crates/txpool/src/validate/eth.rs:524-529`; `smartnet/crates/node/validator/src/mempool_policy.rs:588-607` |
| RPC access control | Namespace filtering only; no auth | HTTP-only config; WS JSON-RPC is never mounted. Production namespace allowlist is `eth,net,web3,rpc,txpool` (conditional `txpool`); other namespaces are hard-blocked and registry side paths are filtered. Launch uses `AuthServerHandle::noop()`; default middleware is no-op `Identity`. | Namespace filtering **is not authentication**. No JWT, authrpc, or separate auth server. `txpool_*` is an information-leak/DoS surface when enabled. Stale comments describe debug/auth endpoints that are not active. | `smartnet/crates/rpc/rpc-builder/src/config.rs:105-148`; `smartnet/crates/rpc/rpc-server-types/src/module.rs:33-45,423-437`; `smartnet/crates/rpc/rpc-builder/src/lib.rs:80-82,1000-1040`; `smartnet/crates/node/builder/src/rpc.rs:518-557`; `smartnet/crates/node/ethereum/src/node.rs:207-215` |
| Config-hash gate | Implemented, local | Raw-file keccak over both bootstrap files, recomputed on every call, no cache and no writes. Both `mainnet.jsonl` and `validators.jsonl` digests must match. | Byte-for-byte equality only, not config validity or semantic agreement. No config sync/push. Rejection is process-local and cleared on restart. | `smartnet/crates/net/mesh/src/config_hash.rs:4-8,24-38`; `smartnet/crates/net/mesh/src/ws.rs:923-956,1298-1346`; `smartnet/crates/net/mesh/src/link.rs:41-58` |
| Bootstrap file gates | Implemented, startup-local | Both files are required or boot fails. Duplicate address/endpoint rejection, COINBASE-vs-derived-address check, self-record lookup, and config↔record endpoint match. | Local startup checks only; no network-wide consistency proof. Self-check intentionally ignores `url`, `active`, `valid`, and `is_self`; `is_self` labels are inert bookkeeping. | `smartnet/crates/node/ethereum/src/bootstrap_files.rs:35-172`; `smartnet/crates/node/cli/src/app.rs:873-883` |
| Pre-auth / message limits | Partial DoS-surface control | 16 KiB pre-auth read budget with a hard read cap; WebSocket frame/message caps; binary-only frames; one-block inject gate; message and queue defaults. | Not full DoS protection. After authentication `ARMED = u64::MAX` (unlimited reads). No peer scoring or reputation. | `smartnet/crates/net/mesh/src/preauth.rs:1-44,69-99`; `smartnet/crates/net/mesh/src/ws.rs:100-107,262-267,959-970,1911-1948`; `smartnet/crates/net/mesh/src/protocol.rs:26-32,46-54,77-98` |
| Threat model | Documented here | See §5 (what we protect) and §6 (what we do not protect). | This is an engineering threat model, not an audit or a guarantee. | This document; [`TEST-RESULTS.md`](TEST-RESULTS.md); [`COMPATIBILITY.md`](COMPATIBILITY.md) |

## 3. Explicit statements

1. **No mandatory TLS or session encryption.** Mesh links use plaintext `ws://` in the current code and the protocol doc explicitly says transport security is optional/separate. Do not advertise "encrypted mesh" without code evidence.
2. **WebSocket carries binary Mesh frames, not JSON-RPC.** RPC is HTTP-only. `eth_subscribe`/`eth_unsubscribe` are registered API names but have no WS JSON-RPC transport.
3. **A compromised validator machine is not the same as an unauthorized network peer.** An unauthorized peer without a listed valid key is rejected by HELLO/allowlist. A compromised machine holding a valid listed key is indistinguishable to the mesh from the legitimate operator. See §7.
4. **What SmartNet does not protect against is listed explicitly in §6** and includes no BFT/quorum guarantee, no encryption, no durable replay defense, no compromise detection, and no independent audit.
5. **Revocation is file/config-driven and process-local** where that is the current behavior. There is no live kick and no persistent ban; changes take effect on the next handshake/reload or process restart.

## 4. Trust boundaries

| Boundary | Crosses how | Current control |
| --- | --- | --- |
| Operator → node | `config.toml`, keystore, two bootstrap files | Startup self-consistency checks; encrypted keystore; hard boot errors |
| Network → mesh session | First frame signed HELLO | Version/network/allowlist/signature checks; nonce replay probe; pre-auth byte budget |
| Mesh session → local state | Rumor → fetch → in-process inject | Binary frame validation, message caps, config-hash gate, chain-ID checks, seal/chair gate |
| Wallet → RPC | HTTP JSON-RPC | Namespace allowlist only; no auth; no WS transport |
| Validator key → seal | Local signing | Encrypted key at rest, no retained signing key, `status: true` chair gate |

## 5. What we protect

- **Authenticated mesh key control.** First frame must be signed HELLO; version, network, allowlist, and signature are checked before the session loop (`identity.rs:186-227`, `ws.rs:837-920`).
- **Cheap pre-signature rejection.** Unknown claimed addresses are rejected before signature recovery (`identity.rs:208-225`).
- **Bounded in-process HELLO replay defense.** Nonce cache cap and FIFO eviction (`ws.rs:97-98`, `identity.rs:298-330`).
- **File-provisioned membership.** `mainnet.jsonl` `active && valid` allowlist for gossip and inbound peers (`peers.rs:70-80,250-277`).
- **Local config-compatibility gate.** Both bootstrap files must hash equal at link bootstrap (`config_hash.rs:24-38`, `ws.rs:1298-1346`).
- **Startup self-consistency.** Identity, COINBASE selector, self record, config endpoint, and duplicate records (`bootstrap_files.rs:39-172`).
- **Encrypted signing material at rest.** Encrypted JSON v3, no plaintext acceptance, transient decryption only (`keystore_util.rs:109-119,162-173,263-332`).
- **Seal authority gate at import/sync.** Recovered seal signer must be the current `status: true` chair (`select.rs:203-225`, `import.rs:705-713`, `beacon_pre_consensus.rs:33-50`).
- **Local ChainID admission/seal checks.** `validate/eth.rs:524-529`, `mempool_policy.rs:588-607` (unit-tested in TR-2026-11-24-01).
- **Reduced RPC namespace exposure.** Production allowlist serves only `eth`, `net`, `web3`, `rpc` and gated `txpool`; `debug`, `trace` and other namespaces are blocked (`module.rs:33-45,423-437`). The former `admin` namespace was removed from the code.
- **Pre-auth and frame-size ceilings.** `preauth.rs:26-44`, `ws.rs:1911-1948`.

## 6. What we do NOT protect

- **No mandatory TLS or session encryption.** No confidentiality/integrity guarantee from transport; do not call the mesh encrypted.
- **No identity trust beyond key control.** HELLO does not prove personhood, trustworthiness, or that a key is uncompromised.
- **No reputation, peer scoring, Sybil proof, or uptime scoring.** Membership is a static allowlist; planned scoring in old docs is not implemented.
- **No live kick or persistent ban.** Failed handshakes close one connection; config-hash rejections are process-local and clear on restart.
- **No durable or global replay protection.** Bounded process-local nonce cache only.
- **No config validation or config push/sync.** Hash equality checks bytes, not meaning; no file is written or synced.
- **No BFT, 2-of-3 quorum, minority reports, fishermen, collateral burn, or slashing in runtime.** Current runtime is interim single-signature `quorum = 1`. This is per design, Calibri-lite protocol supports quorum=1 for PoA 
infrastructures to simplify rolling out. Calibri-lite also supports multiple sealers, full BFT and quorum as part of it's PoS+ consensus, scheduled for implementation in next releases.
- **No RPC authentication/JWT/authrpc.** Namespace filtering is not access control for callers.
- **No WS JSON-RPC.** WebSocket is binary Mesh only.
- **No mesh reorg/tip-regression detection** beyond current block-acceptance rules.
- **No compromise detection.** A valid listed key is accepted regardless of who controls it.
- **No independent audit and no 3+ validator network proof.** See [`TEST-RESULTS.md`](TEST-RESULTS.md) limitations and [`COMPATIBILITY.md`](COMPATIBILITY.md).

## 7. Compromised validator machine vs unauthorized network peer

Mesh and validator sealing share the same node keystore material (`mesh.rs:205-223`, `validator/src/lib.rs:224-229`), but the two authorization checks are different:

- **Mesh peer:** signed key control + `mainnet.jsonl` `active && valid`. It does **not** read validator `status` (`identity.rs:4-7`, `peers.rs:250-277`).
- **Seal authority:** `validators.jsonl` `status: true` plus chair selection (`run_once.rs:512-522`, `select.rs:159-225`).

Consequences:

- A peer without a listed valid key is rejected before the session loop.
- A compromised validator machine holding a valid listed key is **not distinguishable** from the legitimate operator by the mesh.
- With one validator, "listed key" and "chair" coincide; with more validators they are separate checks.
- Current controls are file revocation (`active`/`valid`, `status`) and process-local config-hash rejection. There is no compromise detection or live eviction.

## 8. Stale comments and contradictions to ignore

These are source comments or old docs that conflict with current behavior. Trust the CURRENT protocol doc and binding/validation code.

| Stale item | Current behavior |
| --- | --- |
| HELLO validation order comment (`identity.rs:332-334`) | Responder replay-probes before signature verification; normative order in `MESH-PAYLOAD-PROTOCOL.md:432-438`. |
| `PongDrop` "sender closes" comment (`protocol.rs:114-115,264-265`) | Receivers only close; no production sender exists (`MESH-PAYLOAD-PROTOCOL.md:482-486`). |
| Peer-list strict-equality comment (`protocol.rs:274-275`) | `fetch_peers` checks enclosed `list_hash` against received records, not equality with a local list (`ws.rs:1873-1882`). |
| RPC default namespace comment allowing `debug_*` (`config.rs:105-111`) | Production allowlist hard-blocks `debug` (`module.rs:33-45,423-437`). |
| Consensus comments saying any authorized validator may seal (`calibri/mod.rs:8-10,124-128`) | Runtime `check_seal` requires recovered signer == `get_val_chair` (`select.rs:203-225`). |
| Peer address called "verified" (`peers.rs:35-38`) | It is an unverified claim until HELLO (`peers.rs:92-99`, `identity.rs:4-7`). |
| RPC auth/unified Engine API comments (`builder/src/rpc.rs:579-580,1078-1079,1098-1099`) | Launch uses `AuthServerHandle::noop()` and only the regular RPC server (`rpc.rs:518-557`). |
| `MESH_ADDR` no-default rule vs listener binding `0.0.0.0` | Endpoint self-check still requires explicit config↔file agreement; do not infer peer identity from bind address. |
| 184-byte vs 178-byte HELLO | Consistent: 6-byte envelope + 178-byte HELLO payload (`protocol.rs:24-25,208-210`). |

## 9. Read next

- [`MESH-PAYLOAD-PROTOCOL.md`](MESH-PAYLOAD-PROTOCOL.md) — normative binary Mesh wire spec.
- [`ARCHITECTURE.md`](ARCHITECTURE.md) — component flow and removed-by-design list.
- [`DESIGN-DECISIONS.md`](DESIGN-DECISIONS.md) — trade-offs and must-not-restore markers.
- [`COMPATIBILITY.md`](COMPATIBILITY.md) — evidence-backed feature/status matrix.
- [`TEST-RESULTS.md`](TEST-RESULTS.md) — dated run artifact TR-2026-11-24-01 and its limits.
- [`DOC-INDEX.md`](DOC-INDEX.md) — authority and freshness map.

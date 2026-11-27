# Mesh Payload Protocol

!!! Mesh is fully operational since 22-NOV-2026. Avoid touching it unless very necessary, then do it with extra care. !!!

This page is split in two:

- **`CURRENT`** — what the shipped code does now. This is the normative part.
- **`OLD / PLANNED`** — historical records kept for design history. Those records are **not** current behavior and must not be implemented or treated as a spec unless explicitly requested by the operator.

The wire protocol is binary only. Layers 0–6 are separate: keepalive, identity, config gate, bootstrap, sync, and rumors never share opcodes or frames.

**Transport boundary (operator ruling, 2026-11-19): RPC = HTTP, nothing else.**
JSON-RPC is served over HTTP only. WebSocket carries **only** this binary mesh protocol — RLP frames, mesh-specific — and never RPC; there is no RPC via WebSocket on SmartNet.

Namespace (why it is laid out this way): opcodes are grouped by role first, so a new handshake or control message can never collide with an application message:

- `0x00–0x0F` — control: keepalive, generic acks, identity handshake, bootstrap, config-hash gate.
- `0x10–0x1F` — application request/`PING` messages.
- `0x20–0x2F` — application response/`PONG` messages.
- Within the application ranges: **block-related messages use low digits `0–7`, transaction-related messages use `8–F`**.

Every opcode is globally unique regardless of layer. Reserved slots stay reserved.

---

# CURRENT — implemented behavior

## CURRENT — Layer 0 — Envelope

Every `WebSocket` binary message is one frame:

```text
version: u8 (1)
opcode:  u8
length:  u32 big-endian (payload bytes)
payload: `length` bytes
```

- `version` = `1` (`protocol::VERSION`). Bump only on a breaking change; old peers are rejected with `UnsupportedVersion`.
- `length` **must equal the number of payload bytes that follow it exactly**. It is a deliberate protocol header, not WebSocket framing (WebSocket already gives message boundaries). A mismatch between `length` and the bytes actually present is a hard `LengthMismatch` decode error — the connection is closed, never tolerated.
- Frame size cap: `Limits::max_message_bytes` (default 32 MiB, `MESH_MAX_MSG_MB`). Operator
  values are clamped to `MAX_FRAME_BYTES` (32 MiB) so one frame always fits the send pool.
- Process-wide memory cap: 256 MiB (`MAX_MESH_BYTES`), split into three bounded pools:
  - 64 MiB object cache (`store::DEFAULT_MAX_BYTES`) — full rumor bytes kept for `RUMOR_GET`.
  - 160 MiB inbound (`MAX_RECV_BYTES`) — a received frame's real size is charged before
    the payload is retained and held through inject; idle readers hold no budget, and
    inject slots are taken before spawn so waiters cannot buffer.
  - 32 MiB outbound (`MAX_SEND_BYTES`) — the encoded frame's real size is charged before
    `send`, so a slow peer cannot accumulate full-size frame buffers.

  The three pools are independent and sum to exactly 256 MiB, so peer count and
  `MESH_MAX_MSG_MB` cannot push the mesh past the cap.
- No JSON anywhere on the wire. Fixed binary layouts. Text frames are rejected with `TextFrame`.

## CURRENT — Layer 1 — Keepalive and generic acks

Pure control. Payload is always empty — the decoder rejects a non-empty frame for these opcodes. Never carries sync, identity, config, or application data.

| Opcode | Message | Current meaning |
| ------ | ------- | --------------- |
| `0x01` | `PING` | Liveness probe. |
| `0x02` | *(reserved)* | Old `PONG`, superseded by `PONG_OK`. Stays reserved. |
| `0x03` | `PING_OK` | Receipt after a data-delivery `PONG`; closes that dialog. |
| `0x04` | `PONG_OK` | Generic success ack; also the keepalive answer. |
| `0x05` | *(reserved)* | Reserved. |
| `0x06` | `PONG_DROP` | Reject/terminate opcode. Receivers close on it. There is **no production sender** in the current code. |

- Keepalive dialog: `A → B: PING`, `B → A: PONG_OK`. That ends the exchange — no `PING_OK` on keepalive. The conversation resumes at the next heartbeat; the connection stays alive.
- `PONG_OK` never closes a connection. `PONG_DROP` closes it on the receiving side; the sender-side behavior described in older records is not implemented.
- Sent on `DEFAULT_KEEPALIVE_INTERVAL`. The client pings after half the interval passes with no outbound traffic. A peer is never declared dead for missing one heartbeat: the silence bound is one full keepalive period plus the margin.

### CURRENT — constants

| Constant | Current value | Role |
| -------- | ------------- | ---- |
| `DEFAULT_KEEPALIVE_INTERVAL` | 500 ms | Hardcoded mesh liveness cadence. Independent of `NODE_HEARTBEAT`; `MESH_HEARTBEAT` is retired and rejected as an unknown config key. |
| `SILENCE_MARGIN` | 300 ms | Margin on top of keepalive. |
| silence bound (derived) | 800 ms | No inbound frames for this long on either end → dead peer (server closes, client link exits and reconnects). |
| `DEFAULT_REQUEST_TIMEOUT` | 5000 ms | One dialog / one-shot helper / rumor exchange. |
| `DEFAULT_HANDSHAKE_TIMEOUT` | 5000 ms | WebSocket upgrade + first `HELLO_PING`; also bounds the config-hash reply on outbound links. |
| `CONNECT_GRACE` | 10 s | Grace for a link that has not completed its first connect. |
| `RECONNECT_MIN` / `RECONNECT_MAX` | 50 ms / 600 ms | Link reconnect backoff. |
| `LINK_WINDOW` | 64 | Announces/pipelines allowed in flight on one persistent link. This pipelines **rumor announces**, not block fetches. |
| `NONCE_CACHE_CAP` | 4096 | Bounded responder-side HELLO nonce replay cache. |
| `BOOT_READY_TIMEOUT` | 10 s | Boot waits this long for mesh sync readiness, then proceeds with a warning (fail-open). |

## CURRENT — Layer 2 — Identity handshake

One-time per connection, right after the WebSocket upgrade and before any other frame. Authenticates the peer; does not grant roles. Two distinct opcodes — both stages are checked, and they must not share an opcode:

| Opcode | Message | Direction |
| ------ | ------- | --------- |
| `0x07` | `HELLO_PING` | Initiator → responder: identity + fresh nonce + signature. |
| `0x08` | `HELLO_PONG` | Responder → initiator: identity + echoed nonce + own fresh nonce + signature. |

### CURRENT — HELLO payload (178 bytes fixed, identical for both opcodes)

The opcode lives in the envelope header; the payload carries no message-type byte.

| Field                | Size    | Notes |
| -------------------- | ------- | ----- |
| Protocol version     | 1 byte  | `1` |
| Network ID           | 8 bytes | BE u64 (chain id) |
| Initiator address    | 20 bytes| Always present; echoed by responder |
| Responder address    | 20 bytes| ZEROED in `HELLO_PING`; own address in `HELLO_PONG` |
| Initiator nonce      | 32 bytes| Fresh CSPRNG in `HELLO_PING`; echoed verbatim in `HELLO_PONG` |
| Responder nonce      | 32 bytes| Fresh CSPRNG in `HELLO_PONG`; ZEROED in `HELLO_PING` |
| Recoverable signature| 65 bytes| secp256k1, Ethereum-style (`r,s,v`) over the digest below |

The wire layout carries the addresses/nonces in the order given; the **digest** (what is actually signed) is a separate, fixed big-endian concatenation defined in the next section. "Sender address" in the validation step = the initiator address field on `HELLO_PING`, the responder address field on `HELLO_PONG`.

### CURRENT — HELLO flow

1. Initiator generates `i_nonce`, stores it as outstanding before sending, and sends `HELLO_PING`.
2. Responder requires the first frame to be `HELLO_PING`, probes its replay cache with `i_nonce`, verifies the HELLO, inserts the nonce, and replies with `HELLO_PONG`.
3. Initiator requires the first reply to be `HELLO_PONG`; it checks the echoed `i_nonce`, the echoed initiator address (must be its own), and a non-zero responder nonce; then it verifies the reply.
4. Both sides have now authenticated this peer locally. There is no third confirmation message. On an outbound persistent link, the **config-hash gate (Layer 3b)** runs next; on an inbound persistent link the session loop starts after HELLO.

### CURRENT — exact signed bytes

Digest = `keccak256` over this fixed-width, big-endian concatenation, in this order:

```text
domain     = "smartnet-mesh-hello-v1"   (ASCII, 22 bytes, no NUL)
version    = 1 byte   (protocol version)
network_id = 8 bytes  (BE u64, chain id)
initiator  = 20 bytes (initiator Ethereum address)
responder  = 20 bytes (responder Ethereum address; ZEROED in the initiator's own HELLO)
i_nonce    = 32 bytes (initiator nonce;   ZEROED in the initiator's own HELLO,
                       echoed verbatim in the responder's reply)
r_nonce    = 32 bytes (responder nonce;   ZEROED in the initiator's own HELLO,
                       fresh in the responder's reply)
```

- **`HELLO_PING` (initiator's own)**: `responder`, `r_nonce` are zeroed; `initiator`, `i_nonce` are its own address and fresh nonce.
- **`HELLO_PONG` (responder's reply)**: `initiator`, `i_nonce` = the initiator's echoed values; `responder`, `r_nonce` = its own address and fresh nonce.

Verification recovers the signer from the exact 32-byte digest and compares it to the sender address of that message: initiator on `HELLO_PING`, responder on `HELLO_PONG`. Mismatch = reject. The domain prefix separates this handshake from every other signed use of the key.

### CURRENT — Ethereum signature convention

- Construct the fixed-width preimage exactly as specified above.
- `digest = Keccak-256(preimage)`; sign these 32 bytes directly.
- No EIP-191 `personal_sign` prefix, no additional hash, no message transformation.
- Signature is exactly 65 bytes: `r` (32-byte BE), `s` (32-byte BE), `v` (1 byte, `0x00` or `0x01`). Wire order is `r || s || v`.
- Do not use transaction-signature values `27/28` or EIP-155 transaction `v` values on the wire.
- Require `1 <= r < n` and `1 <= s <= floor(n / 2)`. Signers produce low-`s`; verifiers reject high-`s` and never normalize it.
- If a signer normalizes `s` to `n - s`, it must flip the recovery parity bit: `v = v XOR 1`.

### CURRENT — nonce and validation order

- **Responder (actual call order)**: first-frame check → replay probe on `i_nonce` → `verify_hello` (version/network → allowlist → signature) → nonce insert → `HELLO_PONG`. A bad-signature frame cannot poison the cache; the post-verify insert still catches a parallel race on the same nonce.
- **Initiator (actual call order)**: first-reply check → echoed nonce/address/responder-nonce checks → `verify_hello` (version/network → allowlist → signature). On any mismatch or timeout the connection is closed.
- **Replay cache key**: the initiator nonce alone, not `(initiator, nonce)`. It is bounded FIFO (`NONCE_CACHE_CAP = 4096`), process-local, and connection-independent.
- **Allowlist**: a peer is admitted only if its identity field parses as a `0x` address and its record has `active && valid`. Legacy `key_id` is accepted as the field name. Non-address labels and paused/revoked entries are excluded. The peer file is re-read so membership changes take effect without restart.

### CURRENT — key loading (node bootstrap, fatal)

- Every node loads its identity key at bootstrap via **one shared keystore loader**.
- Keystore is an encrypted Ethereum JSON v3 file; plaintext key files are refused.
- Password comes **only** from the configured password file. No terminal prompt at bootstrap.
- The process keeps only encrypted keystore bytes and the zeroized password bytes. Each HELLO signature decrypts transiently and drops the key immediately.
- Missing, undecryptable, or plaintext key → **fatal**: the boot aborts.

## CURRENT — Node bootstrap preconditions (non-wire)

These are startup checks, not frames. A node will not boot unless they pass.

1. **Both files must exist and be loaded.** `validators.jsonl` is loaded into RAM for the block gate; `mainnet.jsonl` is actually read and parsed. Missing either is a hard boot error. Malformed peer lines are skipped with a warning; they are not a reason to disable the whole mesh.
2. **Whole-file `mainnet.jsonl` validation first.** Duplicate parseable addresses and duplicate `ip:port` endpoints are hard errors, reported with line numbers. No identity matching is attempted against a logically broken file.
3. **Identity derivation.** The keystore is decrypted and the address derived. Optional `COINBASE` is only a keystore-file selector; when present it must name the same derived address, or boot fails with `keys mismatch`.
4. **Self-record lookup.** The node finds its single `mainnet.jsonl` record by that derived address.
5. **Symmetric endpoint match.** Config `MESH_ADDR`/`MESH_PORT` and the self-record `ip`/`port_range` are checked against each other. Neither file is authoritative; the only question is whether they describe the same endpoint for this identity. Mismatch is a hard error showing both endpoints. Match logs INFO:

   ```text
   config.toml and mainnet.jsonl agree on this node's mesh endpoint
   ```

**No defaults and no hardcoded endpoint.** Both `MESH_ADDR` and `MESH_PORT` must be explicitly present; the port must be ≥ 1024 and must not collide with the HTTP listener. There is no loopback fallback. Production uses real routable IPs, which must be written explicitly in both files. `url`, `active`, `valid`, and the inert `is_self` label are ignored by this self-check. Nothing is written by the check.

The `is_self` labels set at load are inert bookkeeping only: no gate, seal, or gossip decision consults them.

## CURRENT — JSONL headers and record defaults (non-wire)

### `mainnet.jsonl`

Optional first-line header, detected only on the first non-blank/non-comment line that contains `mesh_protocol_ver`:

| Field | Type | Absent / wrong-typed default |
| ----- | ---- | ---------------------------- |
| `mesh_protocol_ver` | u64 | `0` |
| `signed` | string | `""` |
| `ts` | u64 | `0` |
| `hash` | string | `""` |
| `block` | u64 | `0` |

- Absent header → all defaults, no error. Wrong-typed fields default individually; the rest of the header is still read.
- The header line is skipped as a record. Blank lines and `#` comments are ignored; malformed record lines are warned and skipped.
- The header is kept in RAM and refreshed on every parse. It is bookkeeping only: no handshake, gossip, or config-gate decision consults it.
- Record defaults: `url = ""`, `active = true`, `valid = true`. `ip` and `port_range` are required.
- `active && valid` gates both inbound allowlist membership and outbound gossip targets, along with a parseable `0x` identity address.

### `validators.jsonl`

Optional first-line header, detected only on the first non-blank/non-comment line that contains `calibri_quorum_protocol_ver`:

| Field | Type | Absent / wrong-typed default |
| ----- | ---- | ---------------------------- |
| `calibri_quorum_protocol_ver` | u64 | `0` |
| `signed` | string | `""` |
| `ts` | u64 | `0` |
| `hash` | string | `""` |
| `block` | u64 | `0` |

- Same lenient behavior: absent → all defaults, wrong types default field-by-field, header line skipped as a record, bookkeeping only.
- The registry is loaded once into RAM at boot. The config gate described below still hashes the **raw file bytes on disk**, not the parsed RAM registry or header values.

## CURRENT — Link lifecycle order

**Outbound persistent link:**

```text
WebSocket connect/upgrade
  → HELLO_PING / HELLO_PONG        (Layer 2)
  → CONFIG_HASH_PING / CONFIG_HASH_PONG  (Layer 3b; bootstrap gate)
  → arm pre-auth budget
  → bootstrap_sync: status + one-block-at-a-time catch-up  (Layer 4)
  → session loop: rumor pipeline + keepalive               (Layers 1, 5, 6)
```

**Inbound persistent link:**

```text
WebSocket accept
  → HELLO_PING / HELLO_PONG        (Layer 2)
  → session loop                   (Layers 1, 3–6)
```

- The inbound side serves `CONFIG_HASH_PING` from its session loop but does not initiate the gate. It answers from its own current disk bytes; the received digests are not used for its own decision.
- One-shot helpers (`send_rumor_inner`, `ping_inner`, `fetch_peers_inner`) perform HELLO only; they do not run the config gate.
- Node startup preconnects all peer-file targets, seeds the boot sync gate, and runs `bootstrap_sync` on each outbound link. It does not use `fetch_peers` for node boot.

## CURRENT — Layer 3 — Bootstrap peer exchange

| Opcode | Message | Payload |
| ------ | ------- | ------- |
| `0x09` | `BOOTSTRAP_PEERS_PING` | empty |
| `0x0A` | `BOOTSTRAP_PEERS_PONG` | `list_hash` (32) + `count` (BE u16) + records |

- `list_hash = keccak256(encode_peer_records(peers))`.
- Canonical record encoding: `ip_len (u8) ‖ ip ‖ port (BE u64) ‖ flags (u8) ‖ key_len (u8) ‖ key`. `flags` bit 0 is `active`, bit 1 is `valid`. `url` is intentionally not carried.
- The server answers `BOOTSTRAP_PEERS_PING` directly; this dialog does not run the config-hash gate.
- `fetch_peers` (a helper/test path, not node boot) accepts a response only when `peer_list_hash(peers) == list_hash`. A mismatch is `TransportError::Protocol("bootstrap list hash mismatch")`. It does **not** compare against its own canonical list, does not set a mode, and does not log a warning-and-skip decision.
- No `PING_OK` follows the list: the `PONG` terminates the dialog.

Handshake failures, `PONG_DROP`, and list-hash failures remain per-connection decisions: no lasting blacklist from those. The **config-hash mismatch is the explicit exception** — see Layer 3b.

## CURRENT — Layer 3b — Bootstrap config-hash gate

| Opcode | Message | Payload |
| ------ | ------- | ------- |
| `0x0B` | `CONFIG_HASH_PING` | `mainnet_hash (32) ‖ validators_hash (32)` |
| `0x0C` | `CONFIG_HASH_PONG` | `mainnet_hash (32) ‖ validators_hash (32)` |

Fixed 64-byte payload for both opcodes. A non-64-byte payload is a protocol malformed error.

**Hash rule.** `get_real_file_hash(path)` is the only hash function for these two files. It reads the whole file and returns `keccak256(raw_bytes)`. It is recomputed on every call and never cached — no `OnceLock`, no sticky digest. `ConfigHashes::read` calls it once per file. A changed file is visible on the next link bootstrap. No file is ever written or synced.

**Dialog (outbound enforcement):**

```text
A: local = keccak256(raw mainnet.jsonl bytes) ‖ keccak256(raw validators.jsonl bytes)
A → B: CONFIG_HASH_PING { local.mainnet, local.validators }
B: recompute both digests from B's own disk bytes
B → A: CONFIG_HASH_PONG { B.mainnet, B.validators }
A: require B.mainnet == A.mainnet AND B.validators == A.validators
```

- Runs once per outbound link bootstrap, immediately after HELLO and before sync or gossip.
- Byte-for-byte equality is required. Comments, whitespace, blank lines, and headers all count because the raw file bytes are hashed.
- The reply is bounded by `DEFAULT_HANDSHAKE_TIMEOUT` (5 s).
- This gate is separate from Layer 3 `list_hash`. `list_hash` is `keccak256` of the canonical peer-record encoding; the gate never reads or compares `list_hash`.

**Mismatch semantics:**

- Either file differing yields `TransportError::ConfigMismatch("mainnet.jsonl")` or `("validators.jsonl")`.
- The link is closed; no failure frame is sent on the wire.
- The peer's `ip:port` is marked rejected in memory for the lifetime of the process. The link stops reconnecting and future gossip target selection filters the peer out. A restart clears the set and gives the peer one fresh bootstrap chance.
- Unreadable **local** files close the session but do **not** mark the peer rejected.
- No file is modified, no config is pushed, and no peer-list data is exchanged as part of this gate.

## CURRENT — Layer 4 — Sync (status + full-block fetch)

| Opcode | Message | Payload |
| ------ | ------- | ------- |
| `0x10` | `SYNC_BLOCK_PING` | `our_last_block_nr` (BE u64) — request for peer tip; carries sender's tip as hint |
| `0x20` | `SYNC_BLOCK_PONG` | `peer_last_block_nr` (BE u64) |
| `0x11` | `SYNC_BLOCK_RLP_PING` | `blockNr` (BE u64) — fetch one full block |
| `0x21` | `SYNC_BLOCK_RLP_PONG` | `blockNr` (BE u64) + full block RLP |

```text
Status:  A → B: SYNC_BLOCK_PING { our_nr }     B → A: SYNC_BLOCK_PONG { peer_nr }
Fetch:   A → B: SYNC_BLOCK_RLP_PING { nr }     B → A: SYNC_BLOCK_RLP_PONG { nr, RLP }
                                              A → B: PING_OK
```

**Outbound bootstrap path (`bootstrap_sync`):**

1. Send `SYNC_BLOCK_PING` with our tip; await `SYNC_BLOCK_PONG` with the peer's tip.
2. While `our_tip < peer_tip`, fetch `our_tip + 1` only.
3. Await the `SYNC_BLOCK_RLP_PONG` whose `blockNr` matches the outstanding request.
4. Inject through the ordinary block path, send `PING_OK`, and set `our_tip = max(head_read, injected_nr)`.
5. Repeat until `our_tip >= peer_tip`. Block fetch is **serialized**: one request in flight, one block per response. Responses may be matched by `blockNr`, but the code does not pipeline these fetches.

**Inbound session path:**

- A status report or forward announce arms an ordered pull range `[local_tip + 1 ..= target]`, one fetch in flight, advanced from the injected block number.
- `mode` flips to `Sync` when the peer reports/announces a height more than one block ahead; it returns to `Rumor` when caught up (`tip >= target`) and the peer is no longer ahead. Inbound sessions initialize to `Rumor`; outbound `bootstrap_sync` is a one-shot ordered pull and does not use `LinkMode`.
- The completed pull is reported to the boot sync gate. Boot readiness requires every target link to have made one exchange, local tip `>=` the highest reported peer tip, and no pull still armed; the gate times out after 10 s and returns `false` with a warning rather than blocking RPC forever. The heartbeat reports that outcome as `boot_sync=confirmed|unconfirmed`; a timeout is not full synchronization.
- There is **no reorg or tip-regression detection** in the current code. Only reported/announced heights are compared.

**Single-block rule.** The wire carries one block per `SYNC_BLOCK_RLP_PONG`, and a node-wide single-permit gate injects exactly one block at a time regardless of which path delivered it. No block batches; `LINK_WINDOW` pipelines announcements, not block fetches.

`peer_last_block_nr` is a synchronization hint only. Received blocks are validated and the canonical chain is decided by the node's real consensus/fork-choice rules, never trusted blindly from the height.

## CURRENT — Layer 5 — Block rumor

Announces carry the **block number, not the hash**.

| Opcode | Message | Payload |
| ------ | ------- | ------- |
| `0x12` | `RUMOR_BLOCKS_PING` | `blockNr` (BE u64) — announce |

```text
A → B: RUMOR_BLOCKS_PING { nr }
B → A: PONG_OK                              // B already has nr (height-based)
   or
B → A: SYNC_BLOCK_RLP_PING { nr }           // B needs the announced block
A → B: SYNC_BLOCK_RLP_PONG { nr, RLP }       // no mode change required for this fetch
B → A: PING_OK
```

- For a forward announce the current code sends the announced-block fetch **and** arms/extends the ordered range from `local_tip + 1`, re-sending its first fetch. For a one-block gap these two fetches can have the same `nr`.
- Mode flips to `Sync` only when `block_nr >= local_tip + 2`. A single-block gap or a backwards/up-to-date announce does not flip the mode.
- `PONG_OK` for a block announce is answered from height (`block_nr <= local_tip`), not from the seen/object caches.
- There is no dedicated `RUMOR_BLOCKS_PONG` (`0x22` reserved).
- On the outbound session loop, a peer's ordered fetch for a block we did not just announce is served from the chain when available.

## CURRENT — Layer 6 — Transaction rumor

Announces carry the transaction hash.

| Opcode | Message | Payload |
| ------ | ------- | ------- |
| `0x18` | `RUMOR_TX_PING` | `txHash` (32) — announce |
| `0x19` | `RUMOR_TX_REQUEST_PING` | `txHash` (32) — "send me that tx" |
| `0x29` | `RUMOR_TX_REQUEST_PONG` | `txHash` (32) + full tx bytes — "txHash sent" |

```text
A → B: RUMOR_TX_PING { hash }
B → A: PONG_OK                             // B already has it (SeenCache / ObjectCache)
   or
B → A: RUMOR_TX_REQUEST_PING { hash }      // B needs it
A → B: RUMOR_TX_REQUEST_PONG { hash, tx }  // self-describing answer
B → A: PING_OK                             // receipt after inject
```

There is no dedicated `RUMOR_TX_PONG` (`0x28` reserved).

## CURRENT — Dialog construction rule

- A dialog is a `PING` → `PONG` exchange, optionally closed by `PING_OK`. Build each payload so a dialog needs **at most two payload `PING`s and two payload `PONG`s** (`RUMOR_*_PING` + counter-request `*_PING` → data `PONG` → `PING_OK` is the deepest chain). This is a construction rule, not an enforced state machine; every message is self-describing.
- After a data-delivery `PONG` (`SYNC_BLOCK_RLP_PONG`, `RUMOR_TX_REQUEST_PONG`) the receiver replies `PING_OK` in the ordered pull/bootstrap paths — the receipt is sent only after the local inject completed.
- After `PONG_OK`, `SYNC_BLOCK_PONG`, or `BOOTSTRAP_PEERS_PONG` the `PONG` itself terminates the dialog — no `PING_OK` follows.
- The config gate is an additional `PING`→`PONG` control pair after HELLO. Success proceeds to sync/gossip; mismatch closes the link. There is no `PING_OK` for the gate.
- The keepalive dialog is `PING` → `PONG_OK` only.
- Announcements on a persistent outbound link are pipelined up to `LINK_WINDOW = 64`; block fetches are serialized.
- `PONG_DROP` is receiver-side terminate only in current code: receivers close, but no production path sends it.

## CURRENT — Duplicate detection

- Block `PONG_OK` is height-based (`block_nr <= local_tip`). Transaction `PONG_OK` uses `SeenCache` / `ObjectCache`.
- `PING_OK` in the ordered pull and bootstrap paths is sent only after the local inject completes. A failed inject there sends no `PING_OK`, clears the armed pull, and lets a later announce or reconnect restart the range.
- Stray/pushed-data handlers log a failed inject but still send `PING_OK`; they do not universally un-mark the object. Do not assume the stronger old guarantee.
- Blocks are recognized by `blockNr` for announces/requests and validated through the normal import path. Transactions are re-hashed on receive and compared to the announced `txHash`.

## CURRENT — Opcode registry (v1, complete)

| Opcode | Message | Layer | Payload |
| ------ | ------- | ----- | ------- |
| `0x01` | `PING` | 1 | empty |
| `0x02` | *(reserved — old `PONG`)* | — | — |
| `0x03` | `PING_OK` | 1 | empty |
| `0x04` | `PONG_OK` | 1 | empty |
| `0x05` | reserved | — | — |
| `0x06` | `PONG_DROP` | 1 | empty |
| `0x07` | `HELLO_PING` | 2 | identity + nonce + signature |
| `0x08` | `HELLO_PONG` | 2 | identity + echoed nonce + signature |
| `0x09` | `BOOTSTRAP_PEERS_PING` | 3 | empty |
| `0x0A` | `BOOTSTRAP_PEERS_PONG` | 3 | `list_hash` + records |
| `0x0B` | `CONFIG_HASH_PING` | 3b | `mainnet_hash (32)` + `validators_hash (32)` |
| `0x0C` | `CONFIG_HASH_PONG` | 3b | `mainnet_hash (32)` + `validators_hash (32)` |
| `0x0D–0x0F` | reserved (control expansion) | — | — |
| `0x10` | `SYNC_BLOCK_PING` | 4 | `our_last_block_nr` (BE u64) |
| `0x11` | `SYNC_BLOCK_RLP_PING` | 4 | `blockNr` (BE u64) |
| `0x12` | `RUMOR_BLOCKS_PING` | 5 | `blockNr` (BE u64) |
| `0x13–0x17` | reserved (block expansion) | — | — |
| `0x18` | `RUMOR_TX_PING` | 6 | `txHash` (32) |
| `0x19` | `RUMOR_TX_REQUEST_PING` | 6 | `txHash` (32) |
| `0x1A–0x1F` | reserved (tx expansion) | — | — |
| `0x20` | `SYNC_BLOCK_PONG` | 4 | `peer_last_block_nr` (BE u64) |
| `0x21` | `SYNC_BLOCK_RLP_PONG` | 4 | `blockNr` (BE u64) + full block RLP |
| `0x22` | reserved (`RUMOR_BLOCKS_PONG` slot — acks use `PONG_OK`) | — | — |
| `0x23–0x27` | reserved (block expansion) | — | — |
| `0x28` | reserved (`RUMOR_TX_PONG` slot — acks use `PONG_OK`) | — | — |
| `0x29` | `RUMOR_TX_REQUEST_PONG` | 6 | `txHash` (32) + tx bytes |
| `0x2A–0x2F` | reserved (tx expansion) | — | — |

## CURRENT — Out of scope

- No TLS requirement; transport security is optional and separate from identity.
- No validator roles/permissions exchanged on the wire; the handshake proves key control only. Consensus roles come from the node's own config/DB.
- No fishermen / collateral-burning / malicious-behavior classification yet; duplicate detection only reduces redundant work.
- No file synchronization or pushing of config from one peer to another. The config gate only compares hashes and closes on mismatch.

---

# OLD / PLANNED — historical records, not current behavior

Everything in this section was present on an earlier version of this page. It is preserved for design history only. **Do not treat it as implemented or normative.**

## OLD — Timings and config knobs

Superseded records:

- `DEFAULT_KEEPALIVE_INTERVAL` was documented as overridable by `MESH_HEARTBEAT`. Current code: `MESH_HEARTBEAT` is retired/unknown; mesh keepalive is hardcoded and independent of `NODE_HEARTBEAT`.
- `DEFAULT_REQUEST_TIMEOUT` was `1000 ms`. Current: `5000 ms`.
- `DEFAULT_HANDSHAKE_TIMEOUT` was `300 ms`, described as "WebSocket upgrade + first `HELLO_PING` only". Current: `5000 ms`, and it also bounds the config-hash reply.
- The HELLO domain was described as 21 bytes. Current: `b"smartnet-mesh-hello-v1"` is 22 bytes.
- Opcode `0x02` was marked "old `PONG` — keep it!!!". Current: it stays reserved and `PONG_OK` (`0x04`) is the live answer.
- Opcode range `0x0B–0x0F` was "reserved (control expansion)". Current: `0x0B`/`0x0C` are the config-hash gate; `0x0D–0x0F` remain reserved.

## OLD — HELLO validation records

Superseded records:

- The replay cache was keyed by `(initiator, nonce)`. Current: keyed by initiator nonce alone, bounded FIFO.
- Validation order was written as allowlist → replay → signature. Current responder call order is first-frame → replay probe → `verify_hello` (version/network → allowlist → signature) → nonce insert → reply.
- Allowlist was described as any `0x` address in the file. Current: the address must parse as `0x` **and** the record must have `active && valid`.

## OLD — `AUTH` / `MODE` flags

Superseded design record:

> Two connection-local flags, `AUTH` and `MODE`. Neither is ever sent as a payload. `AUTH` starts false and is set true only after this side verifies the peer's HELLO; while `AUTH = false`, all Layer ≥ 3 messages are rejected. `MODE` starts as `SYNC` on bootstrap success, flips to `RUMOR` when tips align, and flips back to `SYNC` on tip regression, reorg, or a gap of 2+ blocks without reconnecting. A node enters `RUMOR` only when its own tip equals the network tip. `SYNC`→`RUMOR` and `RUMOR`→`SYNC` need no reconnect.

Current code has no literal runtime `AUTH` flag: a failed handshake closes the connection before the session loop. Inbound sessions initialize `LinkMode::Rumor`; outbound `bootstrap_sync` is a one-shot ordered pull. There is no reorg/regression detection. Gap-based `Sync` flips are implemented for reported/announced heights, not for a general reorg signal.

## OLD — Peer-list strict-equality dialog

Superseded design record:

```text
A → B: BOOTSTRAP_PEERS_PING            (empty)
B → A: BOOTSTRAP_PEERS_PONG { list_hash, records }
A:  hash(records) == list_hash  AND  list_hash == hash(A's own canonical list)
      equal     → accept the list; A sets MODE = SYNC
                  (B sets MODE = SYNC when it serves the list)
      different → WARNING + skip this node: discard the response, do not adopt the
                  list, do not use this node as a bootstrap source.
```

Current `fetch_peers` checks only `peer_list_hash(peers) == list_hash`; it has no own-list equality check, no `MODE` write, and no warning-and-skip path. Node boot uses preconnect plus `bootstrap_sync`, not `fetch_peers`.

## OLD — "no blacklists anywhere" rule

Superseded record:

> No blacklists. Skip applies to this one response only — nothing is banned, remembered, or refused afterwards. The node may connect or retry at any time, and every attempt gets a fresh handshake and a fresh list comparison. A rejected handshake, a `PONG_DROP`, or a discarded list is a per-connection decision, not a lasting one.

Current code preserves this for handshake/list failures, but the config-hash gate is an explicit exception: a mismatch marks `ip:port` rejected in memory for the process lifetime, stops reconnect, and filters the peer out of gossip until restart.

## OLD — Sync and rumor flow records

Superseded records:

- Requester-side `SYNC_BLOCK_RLP` dialogs "may be pipelined; responses matched by `blockNr`". Current: block fetch is serialized, one in flight; only rumor announces are pipelined (`LINK_WINDOW = 64`).
- "Repeat until our tip equals the peer's tip." Current loops use `>=` (`our_tip < peer_tip`, `tip >= target`).
- "Bootstrap always starts in `SYNC`." Current: inbound sessions start `RUMOR`; outbound bootstrap is an ordered pull without `LinkMode`.
- "A later tip regression or reorg moves it back to `SYNC`." Current: no reorg/regression detection exists.
- A block announce was described as a single `SYNC_BLOCK_RLP_PING { nr }` reply with a one-block gap healing inside the dialog. Current: a forward announce also arms/extends the ordered range from `local_tip + 1`; for a one-block gap two fetch frames can be sent.

## OLD — `PONG_DROP` / receipt records

Superseded records:

- `PONG_DROP`: "the sender closes the connection right after". Current: only receivers close on it; no production sender exists.
- `PING_OK`: "A failed inject sends no `PING_OK` and un-marks the object so a later announce can retry". Current: this is true for ordered pull/bootstrap paths only. Stray/pushed-data handlers log a failed inject but still send `PING_OK` and do not un-mark.
- `PONG_OK` ("already have it") was described universally as "sent only after the object was actually processed locally". Current: block `PONG_OK` is height-based; tx `PONG_OK` uses the seen/object caches.

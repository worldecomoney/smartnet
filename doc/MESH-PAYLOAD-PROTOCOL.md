# Mesh Payload Protocol (UPDATED)

Single source of truth for the mesh wire protocol. Replaces the older payload/handshake
docs. Layers 0–6, one page. Keepalive, identity, flags, and application dialogs are
separate — they never share opcodes or frames.

**Transport boundary (operator ruling, 2026-11-19): RPC = HTTP, nothing else.**
JSON-RPC is served over HTTP only. WebSocket carries **only** this binary protocol —
RLP frames, mesh-specific — and never RPC; there is no RPC via WebSocket on SmartNet.

Namespace (why it is laid out this way): opcodes are grouped by role first, so a new
handshake or control message can never collide with an application message:

- `0x00–0x0F` — control: keepalive, generic acks, identity handshake, bootstrap.
- `0x10–0x1F` — application request/`PING` messages.
- `0x20–0x2F` — application response/`PONG` messages.
- Within the application ranges: **block-related messages use low digits `0–7`,
  transaction-related messages use `8–F`**.

Every opcode is globally unique regardless of layer. Reserved slots stay reserved.

## Layer 0 — Envelope (unchanged)

Every `WebSocket` binary message is one frame:

```text
version: u8 (1)
opcode:  u8
length:  u32 big-endian (payload bytes)
payload: `length` bytes
```

- `version` = `1` (`protocol::VERSION`). Bump only on a breaking change; old peers are
  rejected with `UnsupportedVersion`.
- `length` **must equal the number of payload bytes that follow it exactly**. It is a
  deliberate protocol header, not WebSocket framing (WebSocket already gives message
  boundaries). A mismatch between `length` and the bytes actually present is a hard
  `LengthMismatch` decode error — the connection is closed, never tolerated.
- Frame size cap: `Limits::max_message_bytes` (default 32 MiB, `MESH_MAX_MSG_MB`).
- No JSON anywhere on the wire. Fixed binary layouts.

## Layer 1 — Keepalive and generic acks (empty payloads)

Pure control. Payload is always empty — the decoder rejects a non-empty frame for these
opcodes. Never carries sync, identity, or application data.

| Opcode | Message    | Meaning |
| ------ | ---------- | ------- |
| `0x01` | `PING`     | Liveness probe. |
| `0x02` | `PONG`     | Liveness response. | keep it !!!
| `0x03` | `PING_OK`  | Receipt after a **data-delivery** `PONG` — used only where a terminator is needed to stop the conversation (closes that dialog). |
| `0x04` | `PONG_OK`  | Liveness answer; also the generic success ack everywhere below. |
| `0x06` | `PONG_DROP`| Reject/terminate: the sender closes the connection right after. |

- `0x02` (the old `PONG`) is **superseded by `PONG_OK`** and stays reserved. `0x05` is
  reserved.
- Keepalive dialog: `A → B: PING`, `B → A: PONG_OK`. That ends the exchange — no
  `PING_OK` on keepalive. The conversation simply resumes at the next heartbeat; the
  connection stays alive.
- `PONG_OK` never closes a connection; `PONG_DROP` always does.
- Sent at `KEEPALIVE_INTERVAL`, phase-locked (a ping goes out once half the interval
  passed with no outbound traffic). Silence past `DEAD_PEER_TIMEOUT` = dead peer; the
  link reconnects with bounded backoff. A peer is never declared dead for missing a
  single heartbeat — the dead check counts *total* silence across several intervals.

Default timings (shipped code values — `protocol.rs` / `ws.rs`):

| Constant | Default | Role |
| -------- | ------- | ---- |
| `DEFAULT_KEEPALIVE_INTERVAL` | 500 ms | Client ping cadence on a persistent link (`MESH_HEARTBEAT` overrides). |
| `SILENCE_MARGIN` | 300 ms | Margin on top of keepalive for the shared silence bound. |
| silence bound (derived) | 800 ms | No inbound frames for this long on either end → dead peer (server closes, client link exits and reconnects). |
| `DEFAULT_REQUEST_TIMEOUT` | 1000 ms | One dialog / one-shot helper. |
| `RECONNECT_MIN` / `RECONNECT_MAX` | 50 ms / 600 ms | Link reconnect backoff. |
| `DEFAULT_HANDSHAKE_TIMEOUT` | 300 ms | WebSocket upgrade + first `HELLO_PING` only. |

## Layer 2 — Identity handshake (signed HELLO, allowlist)

One-time per connection, right after the WebSocket upgrade and before any other frame.
Authenticates the peer; does not grant roles. Two distinct opcodes — both stages are
checked, and they must not share an opcode:

| Opcode | Message      | Direction |
| ------ | ------------ | --------- |
| `0x07` | `HELLO_PING` | Initiator → responder: identity + fresh nonce + signature. |
| `0x08` | `HELLO_PONG` | Responder → initiator: identity + echoed nonce + own fresh nonce + signature. |

### Payload (178 bytes fixed, identical for both opcodes)

The opcode lives in the envelope header; the payload carries no message-type byte.

| Field                | Size    | Notes                                    |
| -------------------- | ------- | ---------------------------------------- |
| Protocol version     | 1 byte  | `1`                                      |
| Network ID           | 8 bytes | BE u64 (chain id)                        |
| Initiator address    | 20 bytes| Always present; echoed by responder      |
| Responder address    | 20 bytes| ZEROED in `HELLO_PING`; own address in `HELLO_PONG` |
| Initiator nonce      | 32 bytes| Fresh CSPRNG in `HELLO_PING`; echoed verbatim in `HELLO_PONG` |
| Responder nonce      | 32 bytes| Fresh CSPRNG in `HELLO_PONG`; ZEROED in `HELLO_PING` |
| Recoverable signature| 65 bytes| secp256k1, Ethereum-style (`r,s,v`) over the digest below |

The wire layout carries the addresses/nonces in the order given; the **digest** (what is
actually signed) is a separate, fixed big-endian concatenation defined in the next
section. "Sender address" in the validation step = the initiator address field on
`HELLO_PING`, the responder address field on `HELLO_PONG`.

Two-message challenge/response, no extra round trip — **both nodes verify in this single
double ping&pong**:

1. Initiator sends `HELLO_PING` (its address, its fresh nonce, signed).
2. Responder verifies (allowlist + signature + echo-ability), replies `HELLO_PONG` (its
   address, the initiator's echoed nonce + its own fresh nonce, signed over both
   identities and both nonces).
3. Initiator verifies `HELLO_PONG` locally: recovered signer == claimed responder
   address, echoed `i_nonce` == its stored outstanding nonce, signature verifies.
4. Connection authenticated. Each side has now authenticated this peer **locally** —
   no third confirmation message exists or is needed. Application protocol (Layer 3+)
   begins.

### Signing rules — exact signed bytes

The digest is fully determined; there is no implementation latitude. Both sides sign and
verify the **same** digest construction, differing only in which fields are zeroed.

Digest = `keccak256` over this fixed-width, big-endian concatenation, in this order:

```text
domain     = "smartnet-mesh-hello-v1"   (ASCII, 21 bytes, no NUL)
version    = 1 byte   (protocol version)
network_id = 8 bytes  (BE u64, chain id)
initiator  = 20 bytes (initiator Ethereum address)
responder  = 20 bytes (responder Ethereum address; ZEROED in the initiator's own HELLO)
i_nonce    = 32 bytes (initiator nonce;   ZEROED in the initiator's own HELLO,
                       echoed verbatim in the responder's reply)
r_nonce    = 32 bytes (responder nonce;   ZEROED in the initiator's own HELLO,
                       fresh in the responder's reply)
```

- **`HELLO_PING` (initiator's own)**: `responder`, `r_nonce` are zeroed; `initiator`,
  `i_nonce` are its own address and fresh nonce. Signs the 65-byte recoverable signature
  over this digest.
- **`HELLO_PONG` (responder's reply)**: `initiator`, `i_nonce` = the initiator's
  (echoed, exact match required); `responder`, `r_nonce` = its own address and fresh
  nonce. Signs the digest binding both identities and both nonces.

Verification (Ethereum-style recoverable secp256k1) — the recovered signer is compared to
the **initiator** address on `HELLO_PING`, and to the **responder** address on
`HELLO_PONG` (each side recovers the signer and compares against *that message's own
sender address*, never the other one). Mismatch = reject. The domain prefix separates
this handshake from every other signed use of the key.

### Ethereum signature convention

The HELLO handshake uses recoverable ECDSA over secp256k1, following Ethereum address
derivation conventions.

**Digest calculation**
- Construct the fixed-width handshake preimage exactly as specified above.
- Calculate `digest = Keccak-256(preimage)`, producing exactly 32 bytes.
- Sign these 32 digest bytes directly.
- Do **not** apply the EIP-191 `personal_sign` prefix, any additional hash, or any other
  message transformation.

**Signature encoding**

The signature is exactly 65 bytes:
- `r`: 32-byte unsigned big-endian integer.
- `s`: 32-byte unsigned big-endian integer.
- `v`: 1-byte recovery identifier, either `0x00` or `0x01`.

Wire encoding: `r || s || v`.

The recovery identifier is the secp256k1 recovery ID/y-parity supported by
Ethereum-compatible recoverable-signature libraries. Do **not** use transaction-signature
values `27/28` or EIP-155 transaction `v` values on the wire.

**Canonical signature requirements**
- Require `1 <= r < n`, where `n` is the secp256k1 curve order.
- Require `1 <= s <= floor(n / 2)`.
- Signers MUST produce low-`s` signatures.
- Verifiers MUST reject high-`s` signatures; they MUST NOT silently normalize received
  signatures.
- If a signing implementation normalizes `s` to `n - s`, it MUST also flip the recovery
  parity bit: `v = v XOR 1`.

**Verification**
1. Validate the signature length, `r`, `s`, and `v`.
2. Recover the signer address from the exact 32-byte digest and signature, without applying
   another hash or prefix.
3. Compare the recovered 20-byte address against the expected HELLO sender address
   (**initiator** address on `HELLO_PING`; **responder** address on `HELLO_PONG`).
4. Reject the handshake if any check fails.

Implementations MUST use the same digest construction, signature byte order, recovery-ID
convention, and low-`s` rules.

### Nonce-state handling (must be exact)

- **Initiator**: generates `i_nonce` from a CSPRNG, stores it as *outstanding* before
  sending `HELLO_PING`. Marks the connection authenticated **only after** `HELLO_PONG`
  verifies: recovered address == claimed responder address, **and** the reply's echoed
  `i_nonce` == the stored outstanding nonce, **and** the signature verifies over the
  reconstructed digest. On any mismatch or timeout, the outstanding nonce is discarded
  and the connection is closed — never authenticated. The responder's signed echo of our
  nonce is what makes a captured `HELLO_PONG` useless on any other connection.
  (Enforced in `handshake_client`; regression-tested by replaying a captured
  `HELLO_PONG` on a second connection — it must be rejected.)
- **Responder**: on receiving `HELLO_PING`, verifies signature + allowlist *before*
  doing anything else. Only then generates a fresh `r_nonce` and sends `HELLO_PONG`. It
  echoes the initiator's nonce verbatim.
- **Responder replay cache**: a signed `HELLO_PING` captured off one connection would
  otherwise be replayable on a fresh connection. The responder keeps a **bounded replay
  cache of already-accepted initiator nonces** (connection-independent, oldest-first
  eviction at the cap): a `HELLO_PING` whose `(initiator, nonce)` pair is already in the
  cache is rejected before any reply is produced. No extra round trip is added — the
  cache is local state, not a message.
- Nonces are single-use; the digest binds both addresses and both nonces.

### Validation order (fast-reject first)

1. Parse fixed header: opcode (`0x07` only — a `HELLO_PONG` first is rejected), version,
   network id, sufficient payload length.
2. Extract the 20-byte initiator address (the claimed sender of this `HELLO_PING`).
3. **Allowlist check**: the claimed address must be in `mainnet.jsonl` (`key_id` = the
   `0x` address). Not in file → close immediately, no signature recovery. This is the
   cheap DoS gate.
4. **Replay check**: `(initiator, i_nonce)` must not be in the responder's bounded
   replay cache. Seen before → reject (no reply).
5. Verify signature, recover the signer address, compare to the **initiator** address
   (this message's own sender). Mismatch = reject.
6. Record the nonce in the replay cache and send `HELLO_PONG` — only after successful
   verification. On that reply the initiator will recover and compare against the
   **responder** address.

Unknown or mis-signed peer → drop the connection. This is the only gate that decides "is
this connection allowed"; payload validity stays at the application layer.

### Key loading (node bootstrap, fatal)

- Every node loads its identity key at bootstrap via **one shared keystore loader**
  (`load_secret_from_keystore`, exposed `pub` for all node/validator functionality; no
  separate test-only decrypt path).
- Keystore is an encrypted Ethereum JSON v3 file; plaintext key files are refused.
- Password comes **only** from the configured password file. No terminal prompt at
  bootstrap (headless-safe, no human in the loop on reboot).
- Decrypted key lives in memory only, decrypted once, never persisted or logged.
- Missing, undecryptable, or plaintext key → **fatal**: the boot aborts. A node with no
  verifiable identity does not join the mesh ("broken = no have").

The `key_id` field in `mainnet.jsonl` is the verified `0x` address (no longer a free-form
label).

## Connection flags (AUTH, MODE)

Two connection-local flags. Neither is ever sent as a payload — both sides track their
own copy, and both drop when the connection is lost.

| Flag | Values | Rules |
| ---- | ------ | ----- |
| `AUTH` | `true` / `false` | `false` on connect. Set `true` only after **this side** locally verified the peer's HELLO (responder: `HELLO_PING` verified; initiator: `HELLO_PONG` verified). **While `AUTH = false`, all Layer ≥ 3 messages are rejected** (the connection exists only to finish the handshake). Dropped on connection loss; re-established by a new handshake. |
| `MODE` | `SYNC` / `RUMOR` | Per-connection application state. Bootstrap success → `SYNC`. Tip alignment → `RUMOR`. Tip regression, reorg, or a **gap of 2+ blocks** → `SYNC` again, without reconnecting. |

`AUTH` means "I verified this peer's key on this connection" — it grants no roles or
validator privileges.

## Layer 3 — Bootstrap peer exchange

| Opcode | Message | Payload |
| ------ | ------- | ------- |
| `0x09` | `BOOTSTRAP_PEERS_PING` | empty |
| `0x0A` | `BOOTSTRAP_PEERS_PONG` | `list_hash` (32) + `count` (BE u16) + records |

Records keep the existing layout (`key_id`, `ip`, `port`, flags); `protocol.rs` is
normative for the record bytes. `list_hash` = `keccak256` of the canonical record
encoding.

Dialog — **strict equality, no merging**:

```
A → B: BOOTSTRAP_PEERS_PING            (empty)
B → A: BOOTSTRAP_PEERS_PONG { list_hash, records }
A:  hash(records) == list_hash  AND  list_hash == hash(A's own canonical list)
      equal     → accept the list; A sets MODE = SYNC
                  (B sets MODE = SYNC when it serves the list)
      different → WARNING + skip this node: discard the response, do not adopt the
                  list, do not use this node as a bootstrap source. Nothing else.
```

The list **must be identical or it is discarded** — there is no partial merging, no
fallback membership, no negotiation, and **no failure message on the wire** beyond the
warning + skip (nothing further is implemented in this version). `PING_OK` is not sent
here: the `PONG` already terminates the conversation and no further exchange follows.

**No blacklists.** "Skip" applies to this one response only — nothing is banned,
remembered, or refused afterwards. The node may connect or retry at **any time**, and
every attempt gets a fresh handshake and a fresh list comparison; tolerance to retries is
managed per connection, never by banning. The same rule applies everywhere in the
protocol: a rejected handshake, a `PONG_DROP`, or a discarded list is a
**per-connection** decision, not a lasting one.

## Layer 4 — Sync (status + full-block fetch)

Two separate pairs — a height exchange and a per-block RLP fetch. Not PING/PONG
overloads. Delivered blocks go through the ordinary inject/validate/store path.

| Opcode | Message | Payload |
| ------ | ------- | ------- |
| `0x10` | `SYNC_BLOCK_PING` | `our_last_block_nr` (BE u64) — request for the peer's last block nr; carries the sender's own tip as a hint |
| `0x20` | `SYNC_BLOCK_PONG` | `peer_last_block_nr` (BE u64) |
| `0x11` | `SYNC_BLOCK_RLP_PING` | `blockNr` (BE u64) — fetch one full block |
| `0x21` | `SYNC_BLOCK_RLP_PONG` | `blockNr` (BE u64) + full block RLP |

```
Status:  A → B: SYNC_BLOCK_PING { our_nr }     B → A: SYNC_BLOCK_PONG { peer_nr }     (done)
Fetch:   A → B: SYNC_BLOCK_RLP_PING { nr }     B → A: SYNC_BLOCK_RLP_PONG { nr, RLP } A → B: PING_OK
```

Sync flow (pull-based, one height per dialog):

1. `SYNC_BLOCK_PING` — "what is your last block?" (carries ours as a hint).
2. `SYNC_BLOCK_PONG` — "my last block is `BlockNrX`" (the peer's tip; a **hint**).
3. The requester computes the start as **`local_tip + 1`**, bounded above by the peer's
   available range — *not* `BlockNrX + 1`. If the peer is at 120 and we are at 100, we
   request from **101**. (Requesting from the peer's tip would skip everything we lack.)
4. It sends `SYNC_BLOCK_RLP_PING(n)` for one block; the peer answers
   `SYNC_BLOCK_RLP_PONG(n, full RLP)`; the requester validates/injects it through the
   normal block path, replies `PING_OK`, and advances to `n + 1`.
5. Repeat until our tip equals the peer's reported tip; then (and only then) this
   connection may transition to `RUMOR`. Requester-side dialogs **may be pipelined** —
   each `SYNC_BLOCK_RLP` dialog is independent and responses are matched by `blockNr`.

**Single-block rule (protocol invariant).** The wire already carries exactly one block
per `SYNC_BLOCK_RLP_PONG`. On top of that, a receiver injects blocks **strictly one at
a time, node-wide** — a global one-permit gate serializes every block inject regardless
of which connection (ordered pull, stray announce reply, bootstrap, link session)
delivered it. No block batches anywhere; only tx batches are supported. Request
pipelining (step 5) is fine — responses queue at the gate and inject serially.

`BlockNrX` is a **synchronization hint only**. A reported height does not prove the two
peers share the same chain history — if they disagree about fork choice, equal or greater
height says nothing about which chain is canonical. Received blocks are therefore validated
and the canonical chain determined by the node's real consensus/fork-choice rules, never
trusted blindly; consensus remains authoritative over the hint. Because a synced node
answers rumors truthfully and only the validator seals in order, the chain arrives in
order; a gap is simply `peer_tip > my_tip`, revealed directly by the status exchange.

**Bootstrap always starts in `SYNC`.** A node begins in `SYNC` and stays there until its
local chain is aligned with the network tip; only then does the connection stitch into
`RUMOR`. A later tip regression or reorg moves it back to `SYNC` without reconnecting.
Entering `RUMOR` means *initial synchronization is complete* — it does **not** mean the
node will never need an older block again (see Layer 5).

## Layer 5 — Block rumor

Announces carry the **block number, not the hash**.

| Opcode | Message | Payload |
| ------ | ------- | ------- |
| `0x12` | `RUMOR_BLOCKS_PING` | `blockNr` (BE u64) — announce |

```
A → B: RUMOR_BLOCKS_PING { nr }
B → A: PONG_OK                             // B already has nr — dialog ends
  or
B → A: SYNC_BLOCK_RLP_PING { nr }          // B needs nr — backwards fetch allowed,
A → B: SYNC_BLOCK_RLP_PONG { nr, RLP }     // NO mode switch required
B → A: PING_OK
```

- The response to a block announce is `PONG_OK` **or** `SYNC_BLOCK_RLP_PING` — the same
  fetch pair as Layer 4. There is no dedicated `RUMOR_BLOCKS_PONG` (`0x22` reserved).
- **No chat history**: every message is self-describing; nothing is remembered between
  dialogs, so each exchange can be constructed independently.
- **Backwards fetch is allowed** (`nr <= local_tip`, or any single missing block a peer
  asks for): answer the `SYNC_BLOCK_RLP_PING` directly — no need to switch to sync mode
  for it.
- **Gap policy (forward)**: if the announced/reported height is **2 or more** ahead of
  `local_tip`, `MODE` drops to `SYNC` and the node pulls `local_tip + 1 …` via
  `SYNC_BLOCK_RLP` until aligned, then returns to `RUMOR`. A gap of one block heals
  inside the rumor dialog itself (answer `SYNC_BLOCK_RLP_PING`, no mode change).

## Layer 6 — Transaction rumor

Announces carry the transaction hash.

| Opcode | Message | Payload |
| ------ | ------- | ------- |
| `0x18` | `RUMOR_TX_PING` | `txHash` (32) — announce |
| `0x19` | `RUMOR_TX_REQUEST_PING` | `txHash` (32) — "send me that tx" |
| `0x29` | `RUMOR_TX_REQUEST_PONG` | `txHash` (32) + full tx bytes — "txHash sent" |

```
A → B: RUMOR_TX_PING { hash }
B → A: PONG_OK                             // B already has it — dialog ends
  or
B → A: RUMOR_TX_REQUEST_PING { hash }      // B needs it
A → B: RUMOR_TX_REQUEST_PONG { hash, tx }  // self-describing answer
B → A: PING_OK                             // receipt after inject
```

There is no dedicated `RUMOR_TX_PONG` (`0x28` reserved) — the response set is `PONG_OK`
or `RUMOR_TX_REQUEST_PING`.

## Dialog construction rule

- A dialog is a `PING` → `PONG` exchange, optionally closed by `PING_OK`. Build each
  payload so a dialog needs **at most two payload `PING`s and two payload `PONG`s**
  (`RUMOR_*_PING` + counter-request `*_PING` → data `PONG` → `PING_OK` is the deepest
  chain). This is a construction rule, **not enforced on the wire** — no per-dialog state
  machine is required, because every message is self-describing (no chat history).
- After a **data-delivery** `PONG` (`SYNC_BLOCK_RLP_PONG`, `RUMOR_TX_REQUEST_PONG`)
  the receiver replies `PING_OK` — this is where a terminator is *needed*: it stops the
  conversation by protocol and resolves the data sender's pending dialog truthfully
  (receipt is sent only after the local inject completed). After `PONG_OK`,
  `SYNC_BLOCK_PONG`, or `BOOTSTRAP_PEERS_PONG` the `PONG` itself already terminates the
  conversation — no `PING_OK` follows. (`PING_OK` may optionally be used after any
  request/response to unify handling; that is allowed but not required.)
- The keepalive dialog is `PING` → `PONG_OK` only; it repeats at the next heartbeat.
- Connections stay alive across dialogs; a finished dialog closes nothing.
- `PONG_DROP` is outside dialogs: the sender closes the connection immediately after.

## Duplicate detection

- `PONG_OK` ("already have it") is truthful: sent only after the object was actually
  processed locally (its inject completed and it was marked in the `SeenCache` / it sits
  in the `ObjectCache`), never optimistically from an entry that may have expired.
- `PING_OK` (receipt) is sent only after the local inject of the received data completed.
  A failed inject sends no `PING_OK` and un-marks the object so a later announce can
  retry; the sender's dialog then times out and may retry.
- Blocks are recognized by `blockNr` for announces/requests and validated through the
  normal import path; transactions by `txHash` (re-hash on receive).

## Opcode registry (v1, complete)

| Opcode | Message | Layer | Payload |
| ------ | ------- | ----- | ------- |
| `0x01` | `PING` | 1 | empty |
| `0x02` | *(reserved — superseded by `PONG_OK`)* | — | — |
| `0x03` | `PING_OK` | 1 | empty |
| `0x04` | `PONG_OK` | 1 | empty |
| `0x05` | reserved | — | — |
| `0x06` | `PONG_DROP` | 1 | empty |
| `0x07` | `HELLO_PING` | 2 | identity + nonce + signature |
| `0x08` | `HELLO_PONG` | 2 | identity + echoed nonce + signature |
| `0x09` | `BOOTSTRAP_PEERS_PING` | 3 | empty |
| `0x0A` | `BOOTSTRAP_PEERS_PONG` | 3 | `list_hash` + records |
| `0x0B–0x0F` | reserved (control expansion) | — | — |
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

## Mode rules (operational)

- A node enters `RUMOR` **only when its own tip equals the network tip** (its tip is at
  or above the highest tip reported by a synced peer), not on a timer. Flipping to `RUMOR`
  early re-opens the gap hole.
- `SYNC`→`RUMOR` and `RUMOR`→`SYNC` need no reconnect; the mode is per-connection
  application state, layered on an already-authenticated link.
- Trigger back to `SYNC`: tip regression / reorg, or a gap of **2+** blocks against a
  reported/announced height. Single-block and backwards fetches never flip the mode.
- Layering order is fixed: **keepalive (Layer 1) → identity + flags (Layer 2) →
  application (Layers 3–6)**. While `AUTH = false`, Layers 3+ are rejected. No opcode is
  shared across layers.

## Out of scope (unchanged)

- No TLS requirement; transport security is optional and separate from identity.
- No validator roles/permissions exchanged on the wire; the handshake proves key control
  only. Consensus roles come from the node's own config/DB.
- No fishermen / collateral-burning / malicious-behavior classification yet; duplicate
  detection only reduces redundant work and retains attribution evidence for later.


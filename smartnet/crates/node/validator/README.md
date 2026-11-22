# smartnet-validator

Block producer for SmartNet / Calibri Lite, merged into the node as a library (no separate
binary). The node drives it in-process via `ValidatorIo`: it builds and seals a block, submits
**`smartnet_newPayload` (BlockRlp)** through the engine seam, and confirms canonical advance —
**no FCU** in the default run. The `keystore`, `seed-tx`, and `ping` subcommands are exposed by
the `smartnet` binary.

## Build

The validator is part of the node binary:

```bash
make build    # → bin/smartnet
```

## Operator config (required)

`make install` copies `smartnet/example.config.toml` → `~/.smartnet/config.toml` (one shared
config for node and validator; the `VALIDATOR` section lives there). That installed file is the
only config the validator reads (or pass `--config <path>`); the
repo example is a template, never opened at runtime. **Every key is required, and there are no
built-in defaults anywhere in the code**: a missing, empty or unparseable value aborts startup
with an error that lists every problem at once and names the key.

The template already ships sane basics for everything that is the same on every install —
so an unedited copy gets all the way up to the keys only you can answer:

| Left blank on purpose | Why |
|---|---|
| `KEY_PATH`, `PASSWORD_FILE` | where your keystore lives and the password file that unlocks it (both mandatory full paths) |

**Official params are NODE-owned — injected into the validator as hooks, never validator
config keys:**

- **Chain id** — genesis-determined, DB-stored (written at `smartnet init`), read by EVERY
  consumer through the single call fn `smartnet_file_tier::get_chain_id()` (set once at
  bootstrap, never changes; no config fallback, no default).
- **Node id** — this node's ETH `0x…` address, published once by the official keyset
  decrypt and read back through `smartnet_file_tier::get_node_id()` (single source of
  truth; never re-derived ad hoc).
- **Node RPC endpoint** (`seed-tx` / `ping`) — `HTTP_ADDR`/`HTTP_PORT` from config, the
  same values the node binds; the node CLI resolves them and hands them over.
- **Cadence** — `NODE_HEARTBEAT`, the node-owned heartbeat: the seal beat, the canonical
  probe timings and every other hook derive from it. The mesh keepalive is its own
  `MESH_HEARTBEAT`.
- **Validator registry** — `~/.smartnet/validators.jsonl` (not a config key): one
  `{"address":"0x…","status":true}` per line; `status: true` = may seal and is accepted as
  a validator (the SELECT chair gate). Read at bootstrap, held in RAM for the run; a sample
  ships at the repo root as `example.validators.jsonl`.

Three keys are genuinely optional and do have defaults: `VERB` (log verbosity, `1`) and the four
per-class block caps (`MAX_*_PER_BLOCK`, `64` / `32` / `1` / `2`).

| Key | Meaning |
|-----|---------|
| `KEY_PATH` | Directory for encrypted keystores — an absolute path under `~/.smartnet/`, e.g. `~/.smartnet/validator/secure` (`~` expands via `HOME`; relative paths are rejected). **The directory is the source of truth**: without `COINBASE` it must hold EXACTLY ONE file; with `COINBASE`, exactly one matching UTC-suffixed file is selected and other files are ignored |
| `KEY_FILE` | **Legacy, ignored** — boot never consults it; the keystore is the unique file in `KEY_PATH` (kept only so older configs parse) |
| `PASSWORD_FILE` | **Mandatory full path** to the password file, e.g. `~/.smartnet/validator/pw.txt` (relative paths are rejected). The password is ALWAYS a file: boot reads it to decrypt, `smartnet keystore` reads the SAME file to encrypt. Blank/absent = config error; there is no terminal prompt anywhere. Set it by hand — no command writes config |
| `ALLOW_EMPTY` | `true` / `false` — seal empty block when txpool empty (required; `false` = don't) |
| `VERB` | **Optional** log verbosity, `0`–`3`: `0` errors only, `1` info (default), `2` debug, `3` everything. Blank or absent keeps `1`. Written above `3` is a config error, not a silent clamp |
| `MAX_NATIVE_PER_BLOCK` | **Optional** cap on plain value transfers sealed per block. Default `64`. Blank or absent keeps it; an explicit value must be ≥ 1 |
| `MAX_ERC_20_PER_BLOCK` | **Optional** cap on ERC-20 txs per block. Default `32`. Same rules |
| `MAX_CONTRACTS_PER_BLOCK` | **Optional** cap on contract creations per block. Default `1`. Same rules. `Other` (blobs, EIP-7702 auth, unknown calldata) shares this cap |
| `MAX_NFT_PER_BLOCK` | **Optional** cap on ERC-721/1155 transfers per block. Default `2`. Same rules |

## Logging

Logs go to the **terminal (stderr)** — there is no log file and nothing is written under
`~/.smartnet/`. stderr keeps stdout clean for command output (`ping` prints the node's JSON
response there).

`VERB` selects the level:

| `VERB` | Level | What you see |
|--------|-------|--------------|
| `0` | errors only | Failures and the non-zero exit they cause |
| `1` | info (default) | Main activity: `TXPOOL_FETCHED`, `STATE_CHECKED`, `PROPOSAL_SEALED`, `PROPOSAL_SENT` (each with the node URL and elapsed time), one `TX_DROPPED` per rejected tx, plus idle outcomes like `NOTHING_TO_SEAL` |
| `2` | debug | The above plus structural detail: header policy, simulation roots, RLP/wire checks, `smartnet_newPayload` round-trip breakdown, mempool selection stats |
| `3` | trace | Everything the dependency stack emits |

## Balance gate

The validator is the strict gate before a tx enters a block. Every *balance* lookup is funnelled
through exactly two dedicated functions in `src/balances.rs` — **no other balance RPC exists in the
crate**:

| Function | Looks up | Transport today |
|----------|----------|-----------------|
| `val_get_curr_bal(provider, owner)` | native balance | `eth_getBalance` |
| `val_get_curr_bal_erc(provider, token, owner)` | ERC-20 balance | `eth_call` of `balanceOf(address)` |

Both are the **serial primitives** — one address, one round trip — and they are RPC today: slow,
but correct. When the node↔validator mesh lands, **only the insides of these two functions change**;
`prefetch_balances`, the gate and every caller keep their signatures.

### Bounded fan-out

Every pending sender costs a state lookup, once per block, *before* the selection picks its
txs — so a mempool full of distinct senders is a lever on the node. Two things bound it, both
inside `src/balances.rs`:

- **Batching.** Lookups go out as JSON-RPC batches of at most 64, so a pass costs `ceil(n / 64)`
  HTTP round trips instead of `n`. A batch the node will not serve (transport failure, incomplete
  response, or a server that refuses batches) falls back for that chunk to the serial primitives
  above — batching can only ever be faster, never the reason a tx is dropped.
- **A ceiling.** At most 4096 lookups of each kind per pass (senders, and ERC-20
  `(token, sender)` pairs). Past it the remaining senders have no entry at all, so the nonce gate
  skips them and the balance gate reads zero: fail-closed, and their txs stay queued in the node's
  pool. It logs `WARN` / `STATE_FANOUT_CAPPED` with `limit` / `requested` / `skipped`. These are
  internal constants, not config keys — a backstop against abuse, not an operator policy.

`prefetch_nonces` lives beside the balances for the same reason and is the one exception to the
"never surfaces an RPC failure" rule below: a transport failure there aborts the run, because an
unusable nonce view means the validator cannot know what a block is allowed to contain.

A tx is included only if:

- **every class** — `native_balance >= value + gas_limit * max_fee_per_gas` (worst case, not the
  effective fee), and
- **an ERC-20 tx additionally** — the calldata must decode to a token amount, and
  `erc20_balance(token, sender) >= amount`.

The check is **per-tx against the current on-chain balance, not cumulative**; `eth_simulateV1`
catches a cumulative shortfall and the failing tx (and the rest of that sender's chain) is revised
out of the final body before sealing.

A failed balance lookup reads as zero, so the tx is dropped — no RPC or transport error is ever
surfaced. Each dropped tx logs one `TX_DROPPED` at `VERB = 1` with the hash, sender and reason.
**The tx stays in the node's txpool**; the validator never removes anything from it.

**Who checks what.** The node's mempool admission is deliberately lenient — any funds above the fee,
nonce 0 / consecutive / future, signature — and is none of the validator's business. The validator
double-checks for a different purpose: current balance, token balance if applicable, nonce 0 or
consecutive only (never a future nonce), signature.

## Selection protocol (per-class caps)

**Chain id first.** The chain id is genesis-determined and DB-stored — read through the
single call fn `smartnet_file_tier::get_chain_id()` (the same value the node, mesh and ops
commands use; never a config key, never a default). Every candidate tx must satisfy
`tx.chain_id() == Some(chain_id)` — this is checked *before* signature
recovery because it is the cheapest and most decisive gate. A tx that declares no chain at all,
or a different one, is dropped with a `TX_DROPPED` at `VERB = 1`, and the rest of that sender's
nonces go with it. Without this the validator would seal blocks the node rejects at execution.

After that, selection applies **per-class caps** so no one class can monopolise a
block. Every tx is classified into exactly one of:

| Class | How it is recognised | Cap key | Default |
|-------|----------------------|---------|---------|
| **Native** | empty calldata, not a create | `MAX_NATIVE_PER_BLOCK` | 64 |
| **Erc20** | selector `transfer` / `transferFrom` / `approve` | `MAX_ERC_20_PER_BLOCK` | 32 |
| **Nft** | ERC-721/1155 `safeTransferFrom` family (`0x42842e0e`, `0xb88d4fde`, `0xf242432a`, `0x2eb2c2d6`) | `MAX_NFT_PER_BLOCK` | 2 |
| **Contract** | contract creation (`to` is empty) | `MAX_CONTRACTS_PER_BLOCK` | 1 |
| **Other** | blobs, EIP-7702 auth, unknown calldata | *shares the contract cap* | — |

**Native is picked by time, not fee.** Native txs are packed in FIFO order (sender address, then
nonce); the tip is never consulted. This is deliberate — plain transfers are not an auction.

**Every other class is fee-priority, but only when its cap actually bites.** If a class has more
candidates than its cap, the sender runs with the highest `max_priority_fee_per_gas` win, with the
sender address as a deterministic tie-break. Below the cap nothing is re-ordered and fee plays no
part at all, so an uncontested block has no MEV surface.

**Nonce safety is preserved across classes.** A sender's txs are always taken as a contiguous
nonce *prefix*, and selection still walks in `(sender, nonce)` order. So a cap-dropped nonce 0
takes that sender's later nonces with it — even when those later txs are a different class that
still has room. Without this the block would carry a nonce gap and the node would reject it at
execution.

## Create / replace keystore

**Node-level, not a validator command**: every node needs a keypair (it is the mesh
identity); the validator merely uses this same keyset. The command lives on the node
binary:

```bash
smartnet keystore [RAW_PRIVATE_KEY_FILE] --config ~/.smartnet/config.toml
# or
smartnet --config ~/.smartnet/config.toml keystore [RAW_PRIVATE_KEY_FILE]
```

The raw-key path is optional: when given, it points at a plaintext private-key file (64 hex
characters, `0x` optional, either case, whitespace tolerated); when omitted, a fresh key is
generated. The key itself is never a command-line argument (shell history and `ps` are leak
vectors, only the file path is). There are **no `--key` or `--password` flags**: `KEY_PATH` and
`PASSWORD_FILE` come from the named config, and the same password file is what boot decrypts
with.

The protocol, in order:

1. Read `KEY_PATH` and `PASSWORD_FILE` from config (both mandatory full paths).
2. Read/derive the key and its address.
3. Name the keystore `UTC--{timestamp}.{nanos}Z--{lowercase address}` (geth-style, no `0x`).
4. Write the encrypted keystore into `KEY_PATH`, refusing to overwrite a non-empty key dir.
   Key replacement is an operator action: clear `KEY_PATH` first.
5. **Delete the source key file** only after step 4 succeeded; if it failed, the plaintext key
   is left in place because it is your only copy.

**config.toml is NEVER written** by this command (or any command). Boot uses exactly one file in
`KEY_PATH`; when several keys must coexist, the optional node-level `COINBASE` selects the one
whose UTC filename carries that address. Every other combination (zero files, several files with
no `COINBASE`, a mismatched `COINBASE`, zero or multiple matches) is a fatal error. With no
`COINBASE`, the address is derived from the decrypted key for internal use only.

Production run (and the mesh identity) read the password from `PASSWORD_FILE` in the
config — create that file by hand before the first boot.

## Production run (default — no subcommand)

```bash
smartnet-validator --config ~/.smartnet/validator/config.toml
```

**Default:** empty txpool → `NOTHING_TO_SEAL` log, process ends naturally (no seal).  
**Intentional empty block** (T0-GATE D): `ALLOW_EMPTY = true` in config.

**Exit codes** — the node binary maps the run outcome (dispatched through
`run_validator_cli`; see `outcome.rs`):

| Code | Meaning |
|------|---------|
| `0` | Sealed and accepted, or a benign idle (`NOTHING_TO_SEAL`, node down, already initialized, canonical-stalled) |
| `1` | Operation failed (config error, RPC failure, rejected payload, …) |
| `2` | Chain id missing / unusable at boot (database: run `smartnet init`) |
| `3` | Genesis signer list missing / unusable at boot |
| `4` | Node chain id and validator view disagree (both come from the single call fn today — the guard stays as a belt) |
| `5` | Our address is not enabled in `validators.jsonl` (`status: true`) |

Optional: `--dump-payload PATH` writes sealed block RLP for `smartnet-test-util`.

Requires the `smartnet` engine RPC endpoint (the engine/auth port — engine methods are no
longer merged into the wallet HTTP/WS transports; `RPC_UNIFIED_VALIDATOR_JSONRPC` was removed
2026-10-28). In-process validation (`SEAL = true`) needs no RPC at all.

## Other subcommands

- `seed-tx` — one native transfer into the txpool (uses the keystore identity + the
  official params: chain id from `get_chain_id()`, endpoint from `HTTP_ADDR`/`HTTP_PORT`,
  injected by the node CLI)
- `ping` — diagnostic `engine_forkchoiceUpdated` (uses the node's own
  `HTTP_ADDR`/`HTTP_PORT` endpoint, injected by the node CLI). **Read-only by
  default** (A6 · M6): sends `head = safe = finalized = current canonical head`. Explicit
  steering requires `--finalize --head <hash>` (plus optional `--safe` / `--finalized`);
  hashes without `--finalize` are refused.

**Key import lives on the node binary**: `smartnet keystore [RAW_PRIVATE_KEY_FILE] --config CONFIG`
(see "Create / replace keystore" above) — every node needs a keypair, so it is not a validator
command.

**Removed:** `run-once`, `--node-url`, `--coinbase`, `--secret-key`, `--secret-key-file`.

## Related crates

- [`calibri-lite`](../calibri-lite/) — `calibri-lite-consensus`, `smartnet-payload-types`
- [`smartnet`](../smartnet/) — execution node + JSON-RPC server

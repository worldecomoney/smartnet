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
`INTERVAL`, the three `CANONICAL_PROBE_*` timings, `RPC_TIMEOUT_MS` and the seed-tx amount and
gas — so an unedited copy gets all the way up to the keys only you can answer:

| Left blank on purpose | Why |
|---|---|
| `KEY_PATH`, `KEY_FILE`, `PASSWORD_FILE` | where your keystore lives |
| `NODE_URL` | your node's RPC endpoint |
| `COINBASE`, `SEED_TX_TO` | your addresses |
| `CHAINID` | **no default chain id** — `0` is a boot error, deliberately, rather than a fallback that could sign for the wrong chain |

Three keys are genuinely optional and do have defaults: `VERB` (log verbosity, `1`) and the four
per-class block caps (`MAX_*_PER_BLOCK`, `64` / `32` / `1` / `2`).

| Key | Meaning |
|-----|---------|
| `KEY_PATH` | Directory for encrypted keystores — an absolute path under `~/.smartnet/`, e.g. `~/.smartnet/validator/secure` (`~` expands via `HOME`; relative paths are rejected) |
| `KEY_FILE` | Filename under `KEY_PATH`. **Empty** = no keystore; `keystore` creates `UTC--…--<address>` and writes this key |
| `PASSWORD_FILE` | File read for the keystore password, e.g. `~/.smartnet/validator/pw.txt`. **Empty** = prompt on the terminal. Written by the `keystore` command from its `--password` path; a relative path resolves against the current working directory |
| `NODE_URL` | Node JSON-RPC URL — no built-in endpoint; empty refuses to start |
| `COINBASE` | Validator identity / block beneficiary; empty and zero address are rejected |
| `CHAINID` | Chain id the validator signs for. **No default** — `0` is a boot error, never silently inherited. **The single source of truth**: cross-checked against the node's `eth_chainId` at startup (a mismatch refuses to seal), and every mempool tx must declare this exact chain id or it is dropped |
| `INTERVAL` | Seconds between mempool polls — for future daemon loop; `0` is rejected |
| `ALLOW_EMPTY` | `true` / `false` — seal empty block when txpool empty (required; `false` = don't) |
| `CANONICAL_PROBE_TIMEOUT_MS` | A5 · M21 — how long a VALID import may take to become the canonical head before the outcome is classified (ms). `0` rejected; must be ≥ the interval |
| `CANONICAL_PROBE_INTERVAL_MS` | A5 · M21 — base delay between canonical-head probes (ms); the backoff doubles it up to `CANONICAL_PROBE_MAX_INTERVAL_MS`. `0` rejected |
| `CANONICAL_PROBE_MAX_INTERVAL_MS` | A5 · M21 — cap on the probe backoff (ms). `0` rejected; must be ≥ the interval |
| `RPC_TIMEOUT_MS` | Timeout for every JSON-RPC request the validator sends (ms). `0` rejected |
| `SEED_TX_TO` | `seed-tx` recipient; empty and zero address are rejected |
| `SEED_TX_VALUE_WEI` | `seed-tx` amount in wei; `0` rejected |
| `SEED_TX_GAS_LIMIT` | `seed-tx` gas limit; `0` rejected |
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

**Chain id first.** `CHAINID` in the config is the validator's single source of truth. Every
candidate tx must satisfy `tx.chain_id() == Some(CHAINID)` — this is checked *before* signature
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

The **only** command that puts a key into the validator:

```bash
smartnet-validator keystore --key ~/.smartnet/validator/key --password ~/.smartnet/validator/pw.txt --config ~/.smartnet/validator/config.toml
```

`--key` points at a plaintext private-key file (64 hex characters, `0x` optional, either case,
whitespace tolerated) — the key itself is never a command-line argument (shell history and `ps`
are leak vectors, only the file path is). There is no random-key generator and no other import
path: generate the key material however you like, put it in a file, hand `keystore` the file.

The protocol, in order:

1. Read the key and derive the address.
2. Name the keystore `UTC--{timestamp}.{nanos}Z--{lowercase address}` (geth-style, no `0x`),
   always — any previous `KEY_FILE` value is ignored.
3. Write the encrypted keystore at `KEY_PATH`, **replacing** an existing one at that name —
   this is the key-change flow: run the same command again with a new key and the config
   follows. An old keystore file under a different name is left alone.
4. Rewrite the config: `KEY_FILE`, `PASSWORD_FILE` (the `--password` path, as given; it is
   read with `~` expansion at run time), `COINBASE` and `SEED_TX_TO` — the addresses all
   `0x` + lowercase hex — leaving every other line (comments included) untouched.
5. **Delete the source key file.** Only after 3 and 4 both succeeded; if either failed, the
   plaintext key is left in place because it is your only copy.

Password handling: a `--password` path with content is used as-is; a missing or empty file
prompts on the terminal (with confirmation) and writes the file — then `PASSWORD_FILE` in the
config already points at it for the production run. A relative password path resolves against
the current working directory.

Production run reads the password from `PASSWORD_FILE` in the config (empty → prompt, run
verbosity permitting) — the `keystore` command above is what keeps that key filled in.

## Production run (default — no subcommand)

```bash
smartnet-validator --config ~/.smartnet/validator/config.toml
smartnet-validator --config ~/.smartnet/validator/config.toml --password ~/.smartnet/validator/secure/password.txt
```

**Default:** empty txpool → `NOTHING_TO_SEAL` log, process ends naturally (no seal).  
**Intentional empty block** (T0-GATE D): `ALLOW_EMPTY = true` in config.

**Exit codes** — `main` maps the run outcome (see `outcome.rs`):

| Code | Meaning |
|------|---------|
| `0` | Sealed and accepted, or a benign idle (`NOTHING_TO_SEAL`, node down, already initialized, canonical-stalled) |
| `1` | Operation failed (config error, RPC failure, rejected payload, …) |
| `2` | Chain id missing / unusable at boot |
| `3` | Genesis signer list missing / unusable at boot |
| `4` | Configured `CHAINID` does not match the node's `eth_chainId` |
| `5` | Our address is not in the genesis signer set |

Optional: `--dump-payload PATH` writes sealed block RLP for `smartnet-test-util`.

Requires the `smartnet` engine RPC endpoint (the engine/auth port — engine methods are no
longer merged into the wallet HTTP/WS transports; `RPC_UNIFIED_VALIDATOR_JSONRPC` was removed
2026-10-28). In-process validation (`SEAL = true`) needs no RPC at all.

## Other subcommands

- `import <PRIVATE_KEY_FILE>` — bring your own key. Reads a 64-hex-character private key (with or
  without `0x`, either case), writes the encrypted keystore, rewrites `KEY_FILE` + `COINBASE` +
  `SEED_TX_TO` in config to the derived address (all lowercase), and **only then** deletes the
  source file. It never overwrites: if `KEY_FILE` is already set, or the target keystore file
  exists, it refuses and changes nothing. The key is read from a file, never from a CLI argument.
  Runs offline, so it skips the full config validation — this is the one command that runs
  before `COINBASE` has a value, because the import is what sets it.
- `seed-tx` — one native transfer into the txpool (uses config + keystore)
- `ping` — diagnostic `engine_forkchoiceUpdated` (uses `NODE_URL` from config). **Read-only by
  default** (A6 · M6): sends `head = safe = finalized = current canonical head`. Explicit
  steering requires `--finalize --head <hash>` (plus optional `--safe` / `--finalized`);
  hashes without `--finalize` are refused.

**Removed:** `run-once`, `--node-url`, `--coinbase`, `--secret-key`, `--secret-key-file`.

## Related crates

- [`calibri-lite`](../calibri-lite/) — `calibri-lite-consensus`, `smartnet-payload-types`
- [`smartnet`](../smartnet/) — execution node + JSON-RPC server

# EVM-ENGINE-CHANGES.md — the chair's mint-mimic (EVM GATE + fn)

> **STATUS: implementation kept, DISABLED in the node/validator build (2026-11-27).**
> The gate was implemented 2026-11-19 as step 1 of the bridge build; import-only historical
> execution corrections were added 2026-11-21. `disable_gate_rewards()` now returns `true`
> unconditionally in `smartnet/vendor/alloy-evm/src/eth/mod.rs` (same pattern as
> `disable_block_rewards()`), so this build never opens the balance-check exception. The
> code is kept for a separate bridge build only. The old gate-behavior tests are removed;
> only negative disabled-boundary tests remain. Everything else in the EVM/executor is
> untouched and must stay untouched.
>
> The bridge protocol itself (mandates, flows IN/OUT) lives in `BRIDGE-DOC.md` /
> `BRIDGE-USR.md`. This file is only about the mechanics of the mint-mimic and how it
> differs from the alternatives. The security document — scope, findings and the
> CURRENT/PLANNED split — is `doc/SECURITY-EVM-GATE.md`; no superiority over the retired
> REWARD mechanics is claimed there or here.

---

## 1. What changed — exactly two pieces

### 1.1 THE balance-check EVM GATE — `EthEvm::transact_raw` (`vendor/alloy-evm/src/eth/mod.rs`)

Per-transaction, around the `transact` call, the kept condition is **INERT in this build**:

```rust
pub const fn disable_gate_rewards() -> bool { true }   // node/validator build

let incoming = self.cfg.disable_balance_check;
let gate_opens = !disable_gate_rewards() && tx.caller == self.block.beneficiary;
self.cfg.disable_balance_check = incoming || gate_opens;
// ... execute ...
self.cfg.disable_balance_check = incoming;   // never sticks
```

- **OFF always here.** `disable_gate_rewards()` returns `true`, so `gate_opens` is false
  for every sender, including the block's `beneficiary`. No config, RPC call, payload
  shape, seal or import path activates the exception in this build.
- **Kept bridge-only behavior (NOT active here).** If a separate bridge build changes the
  opt-out, revm (feature `optional_balance_check`, enabled workspace-wide) skips the
  upfront cost check and floors the caller's balance at the tx value
  (`new_balance.max(tx.value())`) — the recipient is credited in full and the sealer is
  never debited below zero. That is the mint: money appears at the target, dressed as an
  ordinary transfer.
- With the opt-out in force, every transaction takes the ordinary path:
  `ensure_enough_balance` runs as always.

**Determinism.** The gate key is `signature-recovered sender == header beneficiary` —
both values are sealed into the block itself. Every node — import, mesh sync, or a
re-import years later — evaluates the same key; with the opt-out in force the gate opens
for nobody, so all nodes derive the same state roots. There is no config value, no
registry read, and no time dependence in the rule.

### 1.2 THE fn — `smartnet_validator::chair::tx_out` (`crates/node/validator/src/chair.rs`)

```rust
pub fn tx_out(
    chair: &SigningKey,
    chain_id: u64,
    nonce: u64,
    to: Address,
    value: U256,
    data: Bytes,
) -> Result<TxEnvelope>
```

Operator spec: *"make 1 distinct fn that can tx out by chair only ignoring it's own
balance, mimiking minting."* Contract of the fn:

- builds + signs **one native `value + data` transfer** from the chair — the bridge
  mandate issuance (value = the mandate's exact amount, data = the mandate payload,
  carried verbatim), or the 0.01 WEM OUT authorization, or whatever the chair must say on
  chain;
- **never consults any balance** — that is its entire point; the gate settles the money
  side at execution;
- priced at the protocol fixed fee (`SMARTNET_FIXED_GAS_PRICE_WEI`) and carries exactly
  the intrinsic gas of a plain transfer with this calldata (21 000 + 4/16 per byte);
- **deterministic** (RFC 6979): the same inputs always sign to the same bytes, so a
  rebuilt broadcast is byte-identical and at most one of two can ever be mined.

"chair only" is the intended bridge-build contract and never enforced in the fn itself:
the fn signs with exactly the key it is given (the chair's, in the chair's own sealing
path), and the gate would only open for the header beneficiary if that build changed the
opt-out. **In this build the gate never opens.** `check_seal` proves the **seal signer**
is the SELECT chair; the beneficiary matches it by producer convention, not by a
receiving-side check.

### 1.3 The import-mode carrier (2026-11-20 — historical data only)

Historical RLP import sets one flag on **its own** `EthEvmConfig` (`for_import()`), which
copies it into `CfgEnv.disable_balance_check`. The kept gate code ORs that value with the
(now disabled) chair condition and **restores the incoming value** after each transaction:
in this build the chair term is always false, so live/sync stays fully balance-checked
while import mode is `true` for every transaction to replay trusted history. This flag is
a historical-data accommodation, not the GATE. Nonce, base-fee/tip and all other validity
checks stay on, and sync's behavior is byte-identical.

The same import-only flag also resolves the **fee beneficiary** from the clique seal
(`execution_beneficiary_for_header`), because geth credited the recovered signer, never
the zeroed vote-marker coinbase. Live/sync keeps `header.beneficiary` untouched. This
is required for historical state roots to match; see `BUG2B.md`.

The import flag also pins the historical EVM to **`SpecId::ISTANBUL`** (pre-Berlin: no
EIP-2929 cold-access surcharges). The trusted chain predates Berlin; running import on
LONDON added 8,100 gas to the first CREATE-with-storage transaction (block 19,545),
corrupting both the receipts root and every later state root. Live/sync stays LONDON
(type-0 + type-2 support); no opcode, gas table, or executor logic was modified — only
the import EVM's `CfgEnv` spec. Evidence and brute-force receipt proof: `TROUBLE.md` §7.1.

This is a historical-data compatibility accommodation, not a claim the tx would be valid on a modern chain.

Older geth history can contain transactions that pass its lesser checks but fail
today's `value + gas_limit * fee_cap` test (block 6,660,203). The flag exists so that
legacy chain data can be replayed; it is never a general underfunded-transaction
allowance. The two import gates are unchanged (`classify`: previous block hash;
`check_seal`: validator SELECT), and sync keeps every check. Problem and verification
plan: `ISSUE.md`, review: `ISSUE-A.md`.

## 2. How this differs from "block rewards on steroids"

The alternative on the table was an **event-driven block reward**: re-enable the
`post_block_balance_increments` rail and credit mandate amounts at block finalization.
It was rejected, deliberately. The rows below state mechanical differences only — GATE
does not claim to be more secure, faster or otherwise better than REWARD:

| criterion | event-driven block reward | gate + fn (this) |
|---|---|---|
| credit visibility | **silent balance addition** — no transaction record, invisible to explorers/wallets, hard to audit supply | every credit is a **real on-chain transaction** with the mandate payload in `data` — auditable, indexable, enumerable |
| per-event shape | the reward layer must parse block contents (event mining, "rewards on steroids") inside block finalization — receipts/tx plumbing in the never-touch zone | the mandate is just a tx, the answer is just a tx — the protocol needs **zero** new execution machinery |
| standing behavior | once enabled it pays on **every** matching block forever — the biggest objection | nothing happens without a mandate: zero chair txs ⇒ zero mints. There is no standing rule to switch off |
| consensus constant | the amount must agree network-wide or state roots split; `config.toml` is per-node, **genesis is immutable** and cannot carry it | no constant at all — the rule is content-derived (`sender == beneficiary`), deterministic with zero shared config |
| value correctness | reward math (fixed base, per-block, per-event) needs its own formula and validation | the chair sends **exactly** the mandate's verified amount — the mint follows the request 1:1 |
| blast radius | touches the reward/consensus machinery (currently disabled for SmartNet) | one condition in one function; behaviorally inert for every transaction that was valid before |
| supply accounting | mints buried in block finalization | mints are enumerable: scan for transactions from the chair's address |

Or in the operator's words: genesis is immutable, silent money addition is a big issue
("we want all credits live on chain"), and a switched-on reward "keeps going" — this
design addresses all three mechanically. That is a different approach, not a claim of
superior security or performance (see `doc/SECURITY-EVM-GATE.md`).

## 3. Security

**Nothing in this section is active in the node/validator build.** With
`disable_gate_rewards() -> true` the gate never opens: every sender, including the block
beneficiary, is balance-checked as usual. The kept mechanics below describe the
bridge-only build if it ever changes that opt-out. The old gate-behavior tests are gone;
the remaining boundary tests prove that no overdraft or issuance is possible here.

**It does not open an exploit to contracts.** The kept gate would change exactly one
thing: the pre-execution **caller balance check** for the beneficiary's own transactions.
Contract semantics are 100% untouched — ERC-20, DeFi, approvals, routers, gas metering
and every EVM opcode behave exactly as before. A contract can never become the beneficiary
(beneficiary is the block header's field), and no contract code can trigger or widen the
gate — the sender of a transaction is always the signature-recovered EOA, and the gate
compares it against the header field before execution even starts. Nonce checks and
EIP-3607 (no code senders) still apply to the chair exactly as to everyone else.

- **Nobody can activate the gate here.** The opt-out makes `gate_opens` false for every
  sender, so a beneficiary-signed overdraft is refused with *"lack of funds"* by the
  ordinary balance check.
- **The issuance cannot leak into the txpool.** The pool's own balance check stays on
  (`disable_balance_check` is never set on the pool validator; `cost > balance` is
  enforced at admission), so an overdraft issuance submitted via RPC or gossip is
  rejected at the pool gate. `chair::tx_out` is a kept builder with no production caller;
  no path appends its output to a block in this build.
- **No impersonation, no replay.** The kept gate key is two facts that live inside the
  block (recovered sender, header beneficiary). Nobody can forge either. Replaying a chair
  tx in a later block is governed by the ordinary nonce rule; replaying a block is
  governed by `check_seal` (only the current SELECT chair's seals import).
- **No silent mint, no standing rule.** In a bridge build, every mint would be a
  transaction record on chain, carrying the mandate it answers. The flag is re-evaluated
  per transaction and restored — it never sticks across txs, blocks, or restarts. There
  is no mint at all in this build.
- **The trust root is the chair — by bridge-build design.** The chair is the most trusted
  entity: it answers only bridge mandates that are on-net and verifiable (checked against
  the `bridges` list, `status: true`, at seal). A compromised chair key could mint in a
  build where the gate is enabled; that is inherent to the model — which is why the mint
  would be bound to *this* block's chair (below) and not to some general validator
  privilege. None of that applies to the node/validator build.

## 4. The current-block-chair rule (kept key — INACTIVE in this build)

> *"not just any sealer/validator, but current block chair can be only authorized for
> this particular tx."*

A bridge build that changes the opt-out would authorize exactly that, in two layers that
meet at execution. **In this build the gate authorizes nothing.**

1. **Mechanical key (deterministic, in the EVM):** `tx sender == this block's beneficiary`.
   The authorization attaches to the chair of **the block being executed** — a chair's
   issuance is valid only in the block that chair itself seals. In anyone else's block the
   same tx is just an unfunded transfer and is refused. No "any validator, ever" gate
   exists.
2. **Identity chain (in consensus):** `check_seal` — the one entrance both import and
   mesh sync use — proves the **seal signer** is the current SELECT chair for the slot,
   read from `validators.jsonl` (`status: true` only). A `status: false` record, an old
   chair, or a foreign sealer never passes. **CURRENT:** the beneficiary is set to that
   chair's address by the producer (`run_once.rs:870`) but is not validated against the
   signer by receiving nodes. **PLANNED (needs operator ruling):** a receiving-side
   `signer == beneficiary` check for SmartNet-runtime chains (finding G-2).

Why the execution key is the header beneficiary and **not** a live
`get_val_chair` lookup: the registry mutates over time (mesh-synced `validators.jsonl`),
so an execution-time registry read would make a block's state root depend on *when* it is
executed — re-imports would diverge. The header beneficiary is that block's frozen chair
identity; the registry question is answered once, at seal time, where it belongs.

## 5. What this does NOT change

- No block rewards. `disable_block_rewards` stays as it is — SmartNet pays fees only.
- No active GATE. `disable_gate_rewards()` returns `true`; the balance-check exception
  cannot be reached. Gate-behavior tests are removed; negative boundary tests remain.
- No EVM semantics, no executor behavior, no tx types, no fee policy. The pre-existing
  execution regression tests pass unchanged. Historical import selects the Istanbul gas
  schedule through `CfgEnv` only; live/sync semantics are byte-identical.
- No genesis change (genesis is immutable), no new config keys.
- No behavior change for any transaction that was valid before this change.

## 6. Where things live / how to verify

| path | contents |
|---|---|
| `smartnet/vendor/alloy-evm/src/eth/mod.rs` | kept EVM GATE (`EthEvm::transact_raw`) + unconditional `disable_gate_rewards() -> true` and the single boundary predicate |
| `smartnet/crates/node/validator/src/chair.rs` | kept fn (`chair::tx_out`) + unit tests; no production caller |
| `smartnet/Cargo.toml` | — (the `optional_balance_check` feature lives on `vendor/alloy-evm`'s own `revm` dependency, so every workspace that builds it compiles revm with the field) |
| `smartnet/crates/evm/ethereum/tests/execute.rs` | execution / import-mode tests + negative tests proving no GATE issuance |
| `smartnet/crates/evm/ethereum/src/lib.rs` | import-only EVM env: balance-check bypass, seal-signer beneficiary, Istanbul spec (`import_mode_env_credits_recovered_sealer_only` asserts live stays LONDON) |
| `smartnet/crates/engine/primitives/src/ethereum/mod.rs` | `execution_beneficiary_for_header()` helper shared by sync and import |
| `smartnet/crates/node/ethereum/src/import.rs` | opt-in, logging-only F5 / single-block delta diagnostics; no acceptance gate |

Tests (all green):

- `chair::tests::tx_out_is_deterministic` — byte-identical rebuilds; key/data changes change the bytes.
- `chair::tests::tx_out_carries_the_mandate_verbatim` — to/value/data/nonce/fixed fee preserved.
- `chair::tests::tx_out_is_signed_by_the_chair` — the signature recovers to the chair's address.

The chair tests above only pin the kept builder. Negative disabled-boundary tests:

- `gate_cannot_issue_during_block_processing` — beneficiary-signed empty-wallet issuance is refused.
- `historical_replay_never_activates_gate` — live/replay refuses it; import keeps only its own flag;
  both assert `disable_gate_rewards()` and `gate_opens_for()` for the relevant caller/beneficiary cases.

```text
cargo test -p smartnet-validator chair
cargo test -p smartnet-evm-ethereum --test execute
cargo test -p smartnet-evm-ethereum import_mode_env_credits_recovered_sealer_only
```

Historical execution acceptance (PASSED 2026-11-21): fresh block-0 import of
`/home/usr3/Downloads/wem-full.rlp` (read-only) with `SMARTNET_IMPORT_DIAG=1`;
all 21 rows in `F5_REFERENCE.json` match computed state/tx/receipt roots and FS records,
with no import errors and a clean shutdown (`/tmp/opencode/f5_verify3.log`). Diagnostic
is logging-only — never an acceptance gate; the chain roots are the gate.

# EVM-ENGINE-CHANGES.md — the chair's mint-mimic (EVM GATE + fn)

> **STATUS: implemented (2026-11-19), step 1 of the bridge build; import-only historical
> execution corrections added 2026-11-21.** This file documents the ONE sanctioned
> execution-layer exception in SmartNet — the balance-check EVM GATE — and the one chair-side
> fn that uses it. Everything else in the EVM/executor is untouched and must stay
> untouched. Commit `3c3b7b3`.
>
> The bridge protocol itself (mandates, flows IN/OUT) lives in `BRIDGE-DOC.md` /
> `BRIDGE-USR.md`. This file is only about the mechanics of the mint-mimic and how it
> differs from the alternatives. The security document — scope, findings and the
> CURRENT/PLANNED split — is `doc/SECURITY-EVM-GATE.md`; no superiority over the retired
> REWARD mechanics is claimed there or here.

---

## 1. What changed — exactly two pieces

### 1.1 THE balance-check EVM GATE — `EthEvm::transact_raw` (`vendor/alloy-evm/src/eth/mod.rs`)

Per-transaction, around the `transact` call, one condition:

```rust
let gate_opens = tx.caller == self.block.beneficiary;
self.cfg.disable_balance_check = gate_opens;
// ... execute ...
self.cfg.disable_balance_check = false;   // never sticks
```

- **ON** only while executing a transaction sent by **the block's sealer** (the header
  `beneficiary` — the chair's wallet), **OFF** for every other sender, always.
- When ON, revm (feature `optional_balance_check`, enabled workspace-wide) skips the
  upfront cost check and floors the caller's balance at the tx value
  (`new_balance.max(tx.value())`) — the recipient is credited in full and the sealer is
  never debited below zero. That is the mint: money appears at the target, dressed as an
  ordinary transfer.
- When OFF (everyone else), the code path is byte-identical to before this change:
  `ensure_enough_balance` runs as always.

**Determinism.** The gate key is `signature-recovered sender == header beneficiary` —
both values are sealed into the block itself. Every node — import, mesh sync, or a
re-import years later — opens the gate for precisely the same transactions and derives
the same state roots. There is no config value, no registry read, and no time dependence
in the rule.

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

"chair only" is enforced in two mechanical places and never in the fn itself: the fn
signs with exactly the key it is given (the chair's, in the chair's own sealing path),
and the gate only opens for the header beneficiary. `check_seal` proves the **seal
signer** is the SELECT chair; the beneficiary matches it by producer convention, not by
a receiving-side check.

### 1.3 The import-mode carrier (2026-11-20 — historical data only)

Historical RLP import sets one flag on **its own** `EthEvmConfig` (`for_import()`), which
copies it into `CfgEnv.disable_balance_check`. The gate ORs that value with the chair
condition and **restores the incoming value** after each transaction: on live/sync the
incoming value is `false`, so only the chair's own overdraft gets through, exactly as
before; in import mode it is `true` for every transaction. Nonce, base-fee/tip and all
other validity checks stay on, and sync's behavior is byte-identical.

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

**This does not open an exploit to contracts.** The gate changes exactly one thing: the
pre-execution **caller balance check** for the sealer's own transactions. Contract
semantics are 100% untouched — ERC-20, DeFi, approvals, routers, gas metering and every
EVM opcode behave exactly as before. A contract can never become the beneficiary
(beneficiary is the block header's field), and no contract code can trigger or widen the
gate — the sender of a transaction is always the signature-recovered EOA, and the gate
compares it against the header field before execution even starts. Nonce checks and
EIP-3607 (no code senders) still apply to the chair exactly as to everyone else.

- **Ordinary users cannot reach the gate.** A sender that is not the block's beneficiary
  is balance-checked as always — proven by test
  (`the_gate_stays_closed_for_any_other_sender`: the same issuance from any other sender
  is refused with *"lack of funds"*).
- **The issuance cannot leak into the txpool.** The pool's own balance check stays on
  (`disable_balance_check` is never set on the pool validator; `cost > balance` is
  enforced at admission), so an overdraft issuance submitted via RPC or gossip is
  rejected at the pool gate. The chair's fn output is appended **directly** to the body
  of the block the chair itself is sealing — it is valid there and nowhere else. (A
  leaked issuance in another chair's block would fail both that chair's selection balance
  gate and, even if force-included, execution — the gate would be closed. Never do it.)
- **No impersonation, no replay.** The gate key is two facts that live inside the block
  (recovered sender, header beneficiary). Nobody can forge either. Replaying a chair tx
  in a later block is governed by the ordinary nonce rule; replaying a block is governed
  by `check_seal` (only the current SELECT chair's seals import).
- **No silent mint, no standing rule.** Every mint is a transaction record on chain,
  carrying the mandate it answers. The flag is re-evaluated per transaction and reset —
  it never sticks across txs, blocks, or restarts.
- **The trust root is the chair — by design.** The chair is the most trusted entity: it
  answers only bridge mandates that are on-net and verifiable (checked against the
  `bridges` list, `status: true`, at seal). A compromised chair key can mint; that is
  inherent to the model ("after all, adjusting any other way of minting boils down to
  chair/validator approval") — which is why the mint is bound to *this* block's chair
  (below) and not to some general validator privilege, and why the chair key handling
  matters.

## 4. The current-block-chair rule (proposal, confirmed — and already the implemented key)

> *"not just any sealer/validator, but current block chair can be only authorized for
> this particular tx."*

The gate authorizes exactly that, in two layers that meet at execution:

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
- No EVM semantics, no executor behavior, no tx types, no fee policy. The 8 pre-existing
  execution regression tests pass unchanged. Historical import selects the Istanbul gas
  schedule through `CfgEnv` only; live/sync semantics are byte-identical.
- No genesis change (genesis is immutable), no new config keys.
- No behavior change for any transaction that was valid before this change.

## 6. Where things live / how to verify

| path | contents |
|---|---|
| `smartnet/vendor/alloy-evm/src/eth/mod.rs` | THE EVM GATE (`EthEvm::transact_raw`) |
| `smartnet/crates/node/validator/src/chair.rs` | THE fn (`chair::tx_out`) + unit tests |
| `smartnet/Cargo.toml` | — (the `optional_balance_check` feature lives on `vendor/alloy-evm`'s own `revm` dependency, so every workspace that builds it compiles revm with the field) |
| `smartnet/crates/evm/ethereum/tests/execute.rs` | gate tests |
| `smartnet/crates/evm/ethereum/src/lib.rs` | import-only EVM env: balance-check bypass, seal-signer beneficiary, Istanbul spec (`import_mode_env_credits_recovered_sealer_only` asserts live stays LONDON) |
| `smartnet/crates/engine/primitives/src/ethereum/mod.rs` | `execution_beneficiary_for_header()` helper shared by sync and import |
| `smartnet/crates/node/ethereum/src/import.rs` | opt-in, logging-only F5 / single-block delta diagnostics; no acceptance gate |

Tests (all green):

- `chair::tests::tx_out_is_deterministic` — byte-identical rebuilds; key/data changes change the bytes.
- `chair::tests::tx_out_carries_the_mandate_verbatim` — to/value/data/nonce/fixed fee preserved.
- `chair::tests::tx_out_is_signed_by_the_chair` — the signature recovers to the chair's address.
- `chair_issuance_executes_despite_empty_wallet` — the sealer's issuance executes with an
  empty wallet and credits the bridge the full amount.
- `the_gate_stays_closed_for_any_other_sender` — same tx, any other sender: *"lack of funds"*.

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

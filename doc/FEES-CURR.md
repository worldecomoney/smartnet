# Consensus skip-flag semantics — document the intentional SmartNet differences in `smartnet/crates/consensus/seal/src/lib.rs`.

> **STATUS: CURRENT — active design/reference**

`with_smartnet_runtime_profile` currently disables:

* gas-limit-ramp validation
* EIP-1559 / base-fee validation

Document these as intentional for SmartNet's fixed-fee-for-coins model. Do not re-enable them or invent replacements unless the existing design requires it.

Explicitly state:

* Gas-limit ramp is intentionally dead because SmartNet does not use Ethereum's gas-limit adjustment model.
* EIP-1559/base-fee checks are intentionally dead because SmartNet uses fixed fees rather than EIP-1559 dynamic base fees.
* Any future replacement should be a SmartNet fixed-fee validity check, not an Ethereum EIP-1559 check.

Keep consensus behavior unchanged. Both skips are intentional protocol semantics, not bugs:

* Gas-limit ramp check: intentionally disabled because SmartNet carries the parent gas limit unchanged.
* EIP-1559 transition check: intentionally disabled because SmartNet uses a constant fixed `base_fee_per_gas`, not Ethereum's parent-derived EIP-1559 formula.

Do not re-enable either Ethereum check. If additional validation is required later, add explicit SmartNet fixed-fee / fixed-gas-limit checks instead.


# L1 native coin rules — THE operator rules (2026-11-25)

> **STATUS: CURRENT — authoritative.** This consolidates the native-coin rules that keep
> getting re-explained. Consult this section before touching any fee/gas code. Where
> current code differs from a rule, it is marked `[CURRENT]` vs `[RULE]`.

**0. We are not Ethereum. Ethereum/ETH L1 coin rules do not apply to SmartNet's L1
native coin.** Every Ethereum fee-market assumption (gas auctions, fee consistency
between layers, gas-limit policies, mempool fee wars) is void unless explicitly stated
below.

1. **Fee consistency is NEVER checked — zero consistency checks anywhere.** No component
   ever compares one fee value against another: `eth_gasPrice` is not compared to a tx's
   `maxFeePerGas`; neither is compared to any header's `baseFeePerGas`, to the pool's
   pending base fee, or to another transaction's fee; fill paths, quotes, headers, pool
   and blocks are never cross-checked against each other. There is **no** fee-consistency
   gate in RPC, txpool, validator or consensus — and none is wanted. Quotes are
   *reported*, never *verified for agreement*. The only fee check that exists at all is
   the single-value flag expectation of rule 3/4 — that is an expectation, not a
   consistency comparison.
2. **Fees are fixed and reported to the wallet.** One configured price
   (`FIXED_GAS_PRICE_WEI`) is announced over RPC (`eth_gasPrice`, `eth_feeHistory`,
   priority-tip derivation).

**How the fixed fee applies — the pipeline, in order:**

   1. **send to wallet** — the fixed price is announced (rule 2); that is the whole
      wallet step;
   2. **optional fee-limits expectation** — the `TXPOOL_MARGIN` flag (rules 3/4):
      absent/`0` → nobody cares; armed → **validator-only**;
   3. **txpool admission rules** — generic entry rules only (funded-wallet floor,
      transaction type `0x0`/`0x2`, signature/nonce): these are never fee-consistency
      checks, and per rule 4 the fee flag does not belong here.

3. **Whether the reported fee is expected back depends on the fee-limits config flag**
   — implemented as **`TXPOOL_MARGIN`** (optional integer `0..=5`; no `TXPOOL_FEE_*`
   key exists in the tree):
   - **absent or `0`** → we do not care at all: no expectation, no check, nobody;
   - **`1`** → the exact reported fee is expected for the transaction;
   - **`2..5`** → a margin band (`fixed/n .. fixed*n`).
4. **[RULE] The flag is checked by the VALIDATOR only — never by the node.** For every
   flag value: `TXPOOL_MARGIN = 0`/absent → **nobody checks anything** (0+ scale: no
   expectation at any layer); `1..=5` → the expectation (exact, or the band) is checked
   **only by the validator, when it accepts the tx into the block**. The node must
   **not** check it at all — node-side checking is redundant and useless, the node never
   cares about fees.
   `[CURRENT]` Today the flag filter runs at the node's txpool admission seam
   (`entry_gate.rs:98`, default off = "we don't care" already holds) and the validator
   has no exact-fee check. The move to validator-only is ordered in `TODO.md`; do not
   treat the admission-locus descriptions elsewhere in this file as the design.
5. **Native coin has a fixed fee → no mempool fights.** Native transactions are picked
   **FIFO by arrival** (same-timestamp guarded), never by fee. This does **not** apply
   to tokens: ERC-20/other classes remain fee-priority when their caps bite.
6. **Gas limits are NOT checked — by design, in some places, by some entities:**
   - with the fee flag absent/`0`, the **validator never pays attention** to fees or gas
     limits when sealing — fee = 0 is OK, it is not its problem;
   - **node block acceptance does not check gas limits**: `gas_limit = 1 ETH` OK,
     `0` OK, inconsistent with the parent OK — inconsistent = OK, by design;
   - what remains is wire-shape sanity only (`gas_used <= gas_limit`) and the
     selection-time Σ declared gas hard stop that prevents over-filling a block —
     neither is a fee or gas-limit policy check.

**Who checks what — the complete table:**

| Entity | Fee | Gas limit |
|---|---|---|
| Wallet / RPC quote | receives the fixed price; never compared back | n/a |
| Node txpool admission | never fee consistency; generic rules only (fee flag not here per RULE) `[CURRENT]` flag filter still runs here today | no |
| Validator accepting a tx into a block | only the `TXPOOL_MARGIN` expectation when `1..=5`; flag `0`/absent → never | never — fee = 0 OK, gas whatever OK |
| Node block acceptance | no | no — `1 ETH`, `0`, inconsistent: all OK |
| EVM execution | unchanged metering (`gasUsed × gasPrice`); standard revm mechanics, not a consistency check | shape sanity (`gas_used <= gas_limit`) only |

---

# SmartNet Fee Model — Current State

**Date:** 2026-09-18  
**Status:** Implemented, active on-chain  
**Code:** `smartnet/crates/consensus/seal/src/lib.rs`, `calibri-lite/calibri-lite-consensus/src/pre_consensus/runtime_profile.rs`

---

## Executive Summary

SmartNet uses a **fixed-fee-for-coins** model: every ordinary native transfer (21,000 gas) costs a flat fee of **0.005 coins** (~238.1 gwei gas price). This is a protocol constant, not a market. EVM gas *metering* is unchanged — only the price per gas unit is fixed. ERC-20/DeFi transactions keep 100% Ethereum semantics.

---

## Architecture

### Fee Policy vs EVM Execution

```
                    TRANSACTION
                         │
              ┌──────────┴──────────┐
              │                     │
        EVM execution          Fee policy
              │                     │
        normal gas rules       SmartNet native
              │                 fixed fee
              │                     │
              └──────────┬──────────┘
                         ▼
                   state transition
```

- **EVM execution:** Standard REVM / Ethereum opcode and state semantics. Gas metering is unchanged.
- **Fee policy:** SmartNet-specific fixed fee for transactions paying fees in the native coin.
- **ERC-20 / token fee mechanisms:** Untouched. Token contracts, approvals, permits, multicalls, routers are never special-cased.

### Wallet / RPC Compatibility

Wallets see standard Ethereum transaction fields:

```
Wallet / Web3
    │
    │ normal Ethereum transaction
    │ gasLimit, gasPrice / maxFee, maxPriorityFee
    ▼
SmartNet RPC
    │
    ├── eth_gasPrice() → returns fixed gas price
    ├── eth_estimateGas() → returns normal gas estimate
    ├── eth_feeHistory() → returns fixed base fee
    ▼
SmartNet transaction
    │
    ▼
Native execution
    │
    └── SmartNet fixed-fee policy
```

---

## Constants

```rust
/// SmartNet fixed gas price: 5_000_000_000_000_000 / 21_000 = 238_095_238_095 (≈ 238.1 gwei)
pub const SMARTNET_FIXED_GAS_PRICE_WEI: u64 = 238_095_238_095;

/// Effective flat fee for a standard 21,000-gas native transfer: ≈ 0.005 coins.
pub const SMARTNET_FIXED_TRANSFER_FEE_WEI: u128 = 4_999_999_999_995_000;
```

- `0.005 coins = 5_000_000_000_000_000 wei` is not exactly divisible by 21,000.
- The canonical constant is the gas price (integer division).
- The effective transfer fee is derived: `21,000 × 238,095,238,095 = 4,999,999,999,995,000 wei` (0.005 coins minus 5×10⁻¹² coins — invisible at coin precision).

**Runtime source (2026-11-22):** the constants above are only the **pre-boot fallback**. At boot the
operator's `FIXED_GAS_PRICE_WEI` in `~/.smartnet/config.toml` is installed via
`set_fixed_gas_price_wei`; every consumer reads `fixed_gas_price_wei()` — header construction,
seal/validator, EVM env, RPC wallet quotes and (when enabled) the separate txpool admission filter.
`SmartNet` is not Ethereum: there is no second price source. Historical headers keep whatever base
fee they carried when they were sealed; it is never compared to today's configured price.

**Chain identity (2026-11-22):** SmartNet is detected by the validator section in genesis
`extraData` (`is_smartnet_genesis`), the same signal the consensus runtime profile and node
bootstrap use. The legacy `config.clique` object is inert and must never gate fee policy — an
inert-config genesis used to fall through to parent-derived EIP-1559 fees, park exact-fee wallet
txs in the base-fee subpool, and quote the wrong price.

**MARS transaction types:** type `0x0` (legacy) and type `0x2` (EIP-1559 dynamic fee) are both
first-class at every height and may arrive mixed at any time — no fork or timestamp decides which
one a wallet may use. London did not *replace* legacy/access-list txs; it *added* type 2, so no
legacy special case is needed while the EVM spec is London. Wallets are steered to type 2 as the
newer format, but type 0 may never be deprecated. Type `0x1` (Berlin access list) and `0x3+`
(nonexistent here) are warned about and refused at mempool entry (`entry_gate.rs`); the RLP import
path only warns if such a type ever appears in a sealed block — it never fails the import.

**MARS is mixed rules, not fork eras:** Ethereum picks one active fork and applies one rule set;
WEM/MARS lets rules coexist and detects which apply from each header/chain signal. Istanbul headers
(no `baseFeePerGas`), historical fee-era headers (own base fee), and SmartNet-sealed headers
(configured fixed price) may all appear on the same chain. There is no `London@N` switch.

**Flexible per-header rule:** the header itself is the source of truth on import/sync:
`baseFeePerGas` present → use that header's own fee for execution (any historical value, including
zero); missing → Istanbul gas semantics with a zero EVM base fee (preserves legacy token roots).
For *new* blocks and wallet quotes there is no hard-coded fork boundary: the configured fixed
price is used everywhere. If a non-SmartNet spec's parent has no `baseFeePerGas`, the pending-fee
derivation falls back to the configured price rather than returning `None`/`0`.

---

## Consensus Skip Flags

SmartNet disables two Ethereum consensus checks because its fee model differs from Ethereum's dynamic market model. These are deliberate design decisions, not bugs or TODOs.

### `skip_gas_limit_ramp_check` — Intentionally Dead

**Why:** SmartNet does not use Ethereum's gas-limit adjustment model.

- Ethereum adjusts gas limit ±1/1024 per block based on parent gas limit.
- SmartNet copies the parent gas limit unchanged (`child_header_from_parent` → `gas_limit: parent.gas_limit`).
- The ramp check would reject blocks whose gas limit deviates from the parent by more than `GAS_LIMIT_BOUND_DIVISOR`; since SmartNet never changes gas limit, this check is unnecessary.

**Code path:** `validate_against_parent_gas_limit` in `smartnet/crates/consensus/seal/src/validation/mod.rs:372`

### `skip_eip1559_transition_checks` — Intentionally Dead

**Why:** SmartNet uses fixed fees rather than EIP-1559 dynamic base fees.

- Ethereum's `validate_against_parent_eip1559_base_fee` computes the expected base fee from the parent using EIP-1559 elasticity and denominator parameters.
- SmartNet's `child_base_fee_per_gas` returns the operator's configured fixed price
  (`fixed_gas_price_wei()`, from `FIXED_GAS_PRICE_WEI`), identical on every new block.
- The parent-derived EIP-1559 formula would reject every SmartNet block whose base fee is the fixed constant.

**Code path:** `validate_against_parent_eip1559_base_fee` in `smartnet/crates/consensus/seal/src/validation/mod.rs:320`

### Future Replacement — partially implemented

The SmartNet replacements for the skipped Ethereum checks (`beacon_pre_consensus.rs` in
`smartnet/crates/consensus/seal`). They are **SmartNet validity checks**, not re-enabled
Ethereum checks:

- ❌ Fixed base-fee equality in consensus: **removed 2026-11-22**. Imported/synced headers keep
  their own base fee as-is — absent, zero, or any historical value is accepted; header shape is
  decided per block (presence of `baseFeePerGas`), never by a hard-coded Istanbul/London boundary.
  `ConsensusError::SmartNetBaseFeeInvalid` is no longer produced.
- ✅ `FIXED_GAS_PRICE_WEI` is the enforced native price: it feeds new header `baseFeePerGas`,
  the pending txpool base fee, EVM execution and the RPC wallet quote — all from the same
  configured value. `eth_gasPrice` is pinned to it, so a historical header's base fee can never
  make the wallet announce a price the pool/sealer would reject. Type-2 tips are derived from
  the same pending base fee, so `maxFeePerGas` also lands on the fixed total.
- ➖ `TXPOOL_MARGIN` is a **separate, optional txpool-entry filter**, not fee enforcement
  (integer 0..=5: absent/0 = no filter, 1 = exact, `n` = `fixed/n .. fixed*n`). It only decides
  whether a fresh native tx may enter the pool; it never checks a block and never rewrites the
  tx. What revm does with an admitted tx is revm's business.
- ❌ Gas limit continuity: **no check** — operator decision (stripped 2026-11-01).
  Native fee is fixed by construction; ERC-20 keeps its own economy; neither needs an
  import-side gas-limit check. The SmartNet profile performs **no gas-limit validation**
  (the Ethereum ramp stays dead).
- ⏸ Maximum timestamp gap to the parent: **deferred**

---

## Fee Accounting

### Native Transfer (21,000 gas)

```
sender balance
    - SmartNet fixed native fee (4,999,999,999,995,000 wei)
    - value
    = new sender balance
```

### DeFi / ERC-20 Transaction

```
sender
  ↓
normal EVM transaction
  ↓
normal gas accounting (gasUsed × gasPrice)
  ↓
contract execution
  ↓
ERC-20 logic untouched
```

---

## Implementation Details

### Validator (Block Producer)

The validator constructs blocks with:

- `gas_limit`: copied from parent (no ramp)
- `base_fee_per_gas`: `fixed_gas_price_wei()` (the configured `FIXED_GAS_PRICE_WEI`)
- `difficulty`: `2` (Clique in-turn wire constant)

**Code:** `calibri-lite/calibri-lite-consensus/src/pre_consensus/runtime_profile.rs:96-106`

### Node (Block Importer)

The node validates imported blocks with:

- `validate_authored_header()`: checks signer authorization and proposal metadata only — the
  historical `baseFeePerGas` is never checked
- `validate_authored_header_against_parent()`: checks timestamp (Tier 0 sparse time; no
  maximum-gap rule yet — deferred)
- Skip gas-limit ramp check (SmartNet copies gas limit)
- Skip EIP-1559 base-fee checks entirely (SmartNet uses fixed fees; no replacement check on
  historical headers — the separate `TXPOOL_MARGIN` filter above is admission-only)

**Code:** `smartnet/crates/consensus/seal/src/beacon_pre_consensus.rs:24,81` (+ skip flags in `.../src/lib.rs:40-94`)

---

## Block Gas Limit Accounting

The fixed-fee model changes **prices**, not **gas**: block-level gas accounting stays standard
Ethereum. Two guards (production item **A3**, fixed 2026-10-04) make it impossible to seal a
block that exceeds its own gas limit:

1. **Selection-time hard stop** — `select_pending_for_block`
   (`smartnet/crates/node/validator/src/mempool_policy.rs:540`) takes the block gas limit (`child_hdr.gas_limit`, the
   parent-copied constant described under the ramp flag above) and sums the **declared** gas
   limits of the selected body. A tx that would push Σ over the limit is skipped and **closes
   that sender's whole nonce chain for the block** (other senders keep packing), so the block
   never carries a nonce gap. Same conservative declared-limit rule the node's own builders use
   (`smartnet/crates/payload/exec/src/lib.rs:246`,
   `smartnet/crates/rpc/rpc-eth-api/src/helpers/pending_block.rs:290`,
   `smartnet/crates/node/builder/src/engine_util/reorg.rs:283`). Because declared limits are used, a block may
   be *under-filled* — never over-filled.

   *(Gate order inside `select_pending_for_block`: nonce contiguity → signature → **balance** →
   gas-limit hard stop → per-class caps. The balance gate — `value + gas_limit * max_fee_per_gas`
   natively, plus the ERC-20 token balance for token transfers — is documented in
   `smartnet/crates/node/validator/README.md` §Balance gate and `VAL.md` `## Done`. It never changes gas accounting;
   it only decides whether a tx is allowed to be a candidate at all.)*
2. **Pre-seal assertion** — `validate_smartnet_proposal`
   (`calibri-lite/calibri-lite-consensus/src/pre_consensus/proposal_validation.rs`) refuses to
   seal when `header.gas_used > header.gas_limit`
   (`SmartNetValidationError::GasUsedExceedsGasLimit`). That is exactly the check the node
   applies on import (`validate_header_gas`,
   `smartnet/crates/consensus/seal/src/validation/mod.rs:21-33`), so a violation can never reach
   the wire.

`eth_simulateV1` does **not** enforce the block gas limit on the simulated body (unlike the
pending-block builder), which is why guard 1 is the primary one and guard 2 is the belt. The
gas-limit *propagation* warn in `run_once.rs` (`sim_gas_limit` vs `child_hdr.gas_limit`) is a
different failure and stays a warn.

**Not a fee change:** no new constants, no new config keys, EVM gas metering untouched, and the
skipped consensus checks above (gas-limit ramp, EIP-1559 base fee) remain intentionally dead.

---

## What Is NOT Changed

- **EVM gas metering:** `gasUsed` is still computed by the EVM. Contracts, DeFi, and ERC-20 transactions pay `gasUsed × gasPrice` via ordinary EVM metering.
- **Transaction types:** Legacy (type-0) and EIP-1559 (type-2) transactions are supported.
- **Receipt fields:** `gasUsed`, `effectiveGasPrice`, `cumulativeGasUsed` are standard Ethereum.
- **Wallet compatibility:** `eth_gasPrice`, `eth_estimateGas`, `eth_feeHistory` return values consistent with the fixed fee.

---

## References

- `doc/fees.md` — Original design discussion on separating fee policy from EVM execution
- `calibri-lite/calibri-lite-consensus/src/pre_consensus/runtime_profile.rs` — Runtime profile with fixed fee constants
- `smartnet/crates/consensus/seal/src/lib.rs` — Consensus skip flags documentation
- `smartnet/crates/consensus/seal/src/validation/mod.rs` — Ethereum validation functions (skipped in SmartNet)

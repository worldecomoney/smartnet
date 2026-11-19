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
- SmartNet's `child_base_fee_per_gas` returns a protocol constant (`SMARTNET_FIXED_GAS_PRICE_WEI`), identical on every block.
- The parent-derived EIP-1559 formula would reject every SmartNet block whose base fee is the fixed constant.

**Code path:** `validate_against_parent_eip1559_base_fee` in `smartnet/crates/consensus/seal/src/validation/mod.rs:320`

### Future Replacement — partially implemented

The SmartNet replacements for the skipped Ethereum checks (`beacon_pre_consensus.rs` in
`smartnet/crates/consensus/seal`). They are **SmartNet validity checks**, not re-enabled
Ethereum checks:

- ✅ A present `baseFeePerGas` must equal `SMARTNET_FIXED_GAS_PRICE_WEI` (absent stays legal for
  pre SmartNet-era headers) — `ConsensusError::SmartNetBaseFeeInvalid`
- ❌ Gas limit continuity: **no check** — operator decision (stripped 2026-11-01).
  Native fee is fixed by construction; ERC-20 keeps its own economy; neither needs an
  import-side gas-limit check. The SmartNet profile performs **no gas-limit validation**
  (the Ethereum ramp stays dead).
- ⏸ Maximum timestamp gap to the parent: **deferred** — see `24H.md` at the repo root

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
- `base_fee_per_gas`: `SMARTNET_FIXED_GAS_PRICE_WEI` (constant)
- `difficulty`: `2` (Clique in-turn wire constant)

**Code:** `calibri-lite/calibri-lite-consensus/src/pre_consensus/runtime_profile.rs:96-106`

### Node (Block Importer)

The node validates imported blocks with:

- `validate_authored_header()`: checks signer authorization, proposal metadata, and that a
  present `baseFeePerGas` is the SmartNet fixed fee
- `validate_authored_header_against_parent()`: checks timestamp (Tier 0 sparse time; no
  maximum-gap rule yet — deferred, see `24H.md`)
- Skip gas-limit ramp check (SmartNet copies gas limit)
- Skip EIP-1559 base-fee check (SmartNet uses fixed fees; replaced by the fixed-fee check above)

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

# MARS.md — the SmartNet rulebook (one protocol, no city gates)

Status: **IMPLEMENTED** (2026-10-20) · **Doctrine restated 2026-10-28 (operator).**
One custom, always-mixed EIP set — **settled, all needed EIPs implemented, the mixture is
at work and works** (2026-10-28: nothing pending, nothing to fix). No config key, no fork
schedule, no `if city/then` branching, no city or epoch vocabulary anywhere.

Operator directive: we are a private network. What we need is native transfers, ERC-20,
MetaMask, tx types `0x0`/`0x1`/`0x2`, DeFi. Policy: **accept all, all mixed** — one
protocol, applied uniformly, network-wide.

---

## 0. Doctrine (operator statement of record, 2026-10-28)

1. **One plain blockchain, linear block closure.** No `(hard)fork` support at the block
   level — none, ever: no block re-factoring, no epoch/city-gated behavior at any height.
   Not planned in any foreseeable future.
2. **A custom set of EIPs — always mixed.** EIP numbers are the only vocabulary. We add one
   or a few EIPs when we need them, as a flat enablement. **No city names, no per-city
   versions, no schedule, ever.**
3. **revm `SpecId::*` values are upstream engine knobs only.** There is **no MARS version
   that corresponds to `SpecId::SHANGHAI`** (or `LONDON`, `OSAKA`, …) — the label is a
   mechanical setting inside someone else's implementation ladder; it is not our spec, not a
   version, and not a policy statement. "Cities" exist only as inert strings inside the
   upstream type; nothing in SmartNet branches on them (Phase C removes even the shim, §6).

## 0.1 Compatibility vs. network compatibility (operator doctrine)

The key is to distinguish Ethereum compatibility from Ethereum network compatibility.
Smartnet can support Ethereum-compatible transactions, EVM execution, and wallet/DeFi RPC
without promising compatibility with Ethereum's network history, peer protocol, or
hard-fork schedule.

SmartNet must not be treated as a Reth fork. It is influenced by, and based upon, Reth —
but it is heavily rewritten: much removed, much added, much replaced. "Based on"
describes its lineage; "fork" does not describe its relationship to upstream.

### Smartnet protocol upgrades

Smartnet follows its own protocol and upgrade schedule. Although it supports
Ethereum-compatible execution and RPC interfaces, it is not intended to track every
Ethereum network upgrade automatically.

Protocol upgrades are coordinated across Smartnet nodes. Each upgrade must be compatible
with the Smartnet network's defined rules and activation plan. Operators should run a
Smartnet release that supports the required protocol version before an upgrade activates.

Ethereum compatibility means that supported Ethereum tools, wallets, and applications can
interact with Smartnet. It does not mean Smartnet shares Ethereum's consensus rules,
peer-to-peer network, fork schedule, or complete set of client features.

For network upgrades, operators should follow Smartnet's release notes and upgrade
instructions rather than applying Ethereum client upgrade guidance directly.

---

## CURRENT — what the code implements today (2026-10-28)

Evidence pass over the tree (claim → `file:line`). "Not yet" items are deliberate per §0 —
not backlog — unless flagged otherwise.

### Implemented and live

| Rulebook claim | Evidence |
|---|---|
| One pinned revm hook; no schedule/height derivation | `ethereum/evm/src/config.rs` — `revm_spec*()` (mechanical rung label only, §0.3) |
| 0x0 / 0x1 / 0x2 always accepted | validator defaults `eip2718`/`eip1559` = `true` (`transaction-pool/src/validate/eth.rs:1037-1038`); no activation gate on these types |
| EIP-1559 base fee always present & validated | genesis base fee (`chainspec/src/spec.rs:26-29`), `validate_header_base_fee` (`ethereum/consensus/src/lib.rs:305`), presence required (`consensus/src/validation/mod.rs:41`) |
| 0x3 (blob tx) rejected | `.set_eip4844(!blobs_disabled)` with `blobs_disabled = true` (`ethereum/node/src/node.rs:636,660`) |
| 0x4 (set-code) rejected | validation refuses it (`validate/eth.rs` — Prague gate, always false; no activation exists or is planned) — matches rulebook `false` |
| Blob/requests/beacon fields absent — and rejected if present | `BlobGasUsedUnexpected` / `ExcessBlobGasUnexpected` / `ParentBeaconBlockRootUnexpected` / requests-hash checks (`ethereum/consensus/src/lib.rs:319-345`) |
| Flat execution profile at every height | enabled-set behavior never varies by height; the guard shim still exists mechanically (see Not yet) |
| Engine submission = **V1** | `execution_data_from_block_rlp` / `block_to_payload` build `ExecutionPayloadV1` (`ethereum/engine-primitives/src/lib.rs:89-91,124-131`; trait bound `TryInto<ExecutionPayloadV1>` `:62`) |
| `eth_*` + MetaMask surface untouched | operator ruling (§6) — no namespace changes |

### Not yet / by design absent

| Item | Status |
|---|---|
| **4895 withdrawals** | **rulebook §1 says `true`, the live wire carries none**: validator seals `header.withdrawals_root = None` (`validator/src/run_once.rs:758`), fake-block JSON emits `"withdrawals": null` (`:1439`), genesis header "never carries a withdrawals [root]" (`chainspec/src/spec.rs:31`), and consensus **rejects** a present root (`WithdrawalsRootUnexpected`, `consensus/src/lib.rs:308-317`). Implemented = **no**. Discrepancy recorded here only — the EIP table is untouched per operator (2026-10-28); see the §4 note. |
| 4788 / 2935 / 7685 / 7928 / 7825 | not implemented — rulebook `false`; fields are `None` and rejected if present (evidence row above) |
| V3+ payload mechanics, blob market, KZG | absent by design — nothing emits them |
| Phase C shim retirement (`EthereumHardfork` enum, `ChainHardforks`, ~67 `is_*_active_at_*` sites) | **not yet** — postponed/owner-held (`TODO.md` *Mars migration*); behavior is already flat, only the city facade remains |
| Spec bump (§7) | staged, not active — by design |

## 1. The rulebook

The table below is the rulebook: `enabled` is policy. **Settled and live — every EIP
SmartNet needs is already implemented and the mixture in production works; nothing here is
pending.** Rows marked `false` mean *not needed by this network*, not backlog. EIP numbers
are the only vocabulary.
(The earlier in-code `MARS_EIPS` list + `MARS_VERSION` pin that lived in
`smartnet/crates/ethereum/evm/src/config.rs` are gone — that file now holds only the pinned
revm hook; enforcement is at the builder/validation/cfg sites, see §3.)

| EIP | name | enabled |
|---|---|---|
| 2930 | access-list tx (0x1) | **true** |
| 1559 | fee market / type-2 (0x2) | **true** |
| 4895 | withdrawals | **true** |
| 4844 | blob transactions (0x3) | false |
| 4788 | beacon block root | false |
| 2935 | block hash history | false |
| 7685 | requests | false |
| 7702 | set-code tx (0x4) | false |
| 7825 | tx gas limit | false |
| 7928 | block access list | false |

To add an EIP: enable it at its enforcement site (validator builder flag / validation path /
cfg flag), update the table here, and — if the revm feature set changes — the pinned hook in
`config.rs`. Flat enablement; no version bump, no schedule entry, no epoch. (The old
drift-guard test went away with `MARS_EIPS`.)

## 2. Derived spec — one custom EIP set, always mixed

`revm_spec()` / `revm_spec_by_timestamp_and_block_number()`
(`smartnet/crates/ethereum/evm/src/config.rs`) is the single revm hook. What it configures
is the **custom EIP set of §0** — always mixed, connected to no city or epoch, now and
forever, guarded by nothing ("if this city is already reached" does not exist here). Uniform
at block 0 and every height, never fork/height/genesis-derived. The concrete `SpecId::*` the
hook returns is an upstream engine knob (§0.3), not a version of anything.

## 3. Tx types — always on, no gates

- `0x0` (legacy), `0x1` (EIP-2930), `0x2` (EIP-1559): **always accepted**.
- `0x3` (blob) and `0x4` (7702): **always rejected** (no blob market, no KZG verifier).

Wired without gates on the accept path: the txpool builder
(`crates/ethereum/node/src/node.rs:636,660`) sets `.set_eip4844(!blobs_disabled)` with
`blobs_disabled = true` → 0x3 **off**; 0x0/0x1/0x2 **on** (validator defaults `true`,
`validate/eth.rs:1037-1039`). 0x4 stays rejected exactly as the rulebook says — by the live
configuration (validation refuses it; no activation exists or is planned) — working as
intended, nothing to change here (operator 2026-10-28).

## 4. Wire shape — V1, no withdrawals *(note: superseded 2026-10-28 — the original
"V2-only" text below overstates the live wire; current code emits `ExecutionPayloadV1` and
carries no withdrawals, see §CURRENT. Kept for the record of that pass.)*

**Original text of that pass:**

- Header carries `withdrawals_root` (empty-list root) and empty `withdrawals` in the body.
- `blob_gas_used` / `excess_blob_gas` / `parent_beacon_block_root` / `requests_hash` /
  `block_access_list_hash` / `slot_number` are **None** — V3/V4 mechanics are not backed in.
- Submission (`execution_data_from_block_rlp` / `block_to_payload` in
  `crates/ethereum/engine-primitives/src/lib.rs`) emits **V2** when the body carries withdrawals,
  **V1** otherwise — never V3/V4.

## 5. Done this pass

- The revm rung label is pinned mechanically (engine knob only — **no MARS version
  corresponds to any `SpecId::*`**; earlier notes calling this a "SHANGHAI spec" conflated
  upstream naming with the rulebook).
- Validator seals empty `withdrawals` + `withdrawals_root`; payload checkpoints mirror the real
  node path. *(Superseded: today's seal sets `withdrawals_root = None` / `withdrawals: null` —
  §CURRENT.)*
- Submission made version-aware (V2). Explicit tx-type rulebook (0/1/2 on, 3/4 off).
- Verified: `smartnet-validator` 97, `smartnet-chainspec` 19, `smartnet-evm-ethereum` 11+8,
  `smartnet-node-ethereum` 7 tests pass; `cargo check -p smartnet --bin smartnet` clean.

## 6. Remaining (Phase C — remove the legacy city facade)

Behavior no longer branches on cities, but the legacy `EthereumHardfork` enum / `ChainHardforks`
schedule and its `is_*_active_at_*` call sites (~23 files, 67 sites) remain as a compatibility
shim. The follow-up is mechanical: collapse those call sites to the pinned spec / flat policy
constants, then retire the city enum and schedule entirely (§0.2 — no cities, ever).
(2026-10-28: tracked in `TODO.md` as *Mars migration — postponed, owner-held*; the in-code
`MARS_EIPS` table it mentioned no longer exists.) `eth_*` RPC names, tx types 0x0/0x1/0x2, and
MetaMask behavior stay untouched (operator ruling).

## 7. Staged (not active) — the spec bump

`config.rs` carries a staged-bump note: raising the engine rung beyond today's minimum
requires wire/payload shapes (the V3+-era fields) the validator does not produce yet. Per
operator (2026-10-28) that work — and any residual activation-shim removal — stays parked in
`TODO.md` (*Mars migration*, postponed/owner-held). When it happens: change the pinned knob +
validator shapes, E2E-gated. Still §0: this would be an EIP-set change, flat and always-on —
never a fork, never a city, never a version.

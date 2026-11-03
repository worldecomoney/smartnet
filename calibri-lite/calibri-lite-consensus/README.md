# calibri-lite-consensus

Single crate for SmartNet **pre-consensus** block admission and CALIBRI stubs.

## Submodules

| Path | Role |
|------|------|
| **`pre_consensus/`** | **Active:** `SmartNetRuntimeProfile`, proposal metadata, seal, header policy, genesis signer parse |
| **`consensus/calibri/`** | Chair/quorum from genesis; future BT/minority report stubs |

Legacy Clique replay, snapshot store, epoch checkpoints, and related modules were **removed** (2026-05-22).

## Entrypoints

```rust
use calibri_lite_consensus::{
    SmartNetRuntimeProfile,
    validate_pre_consensus_header,
    validate_pre_consensus_timestamp_against_parent,
    get_quorum_addresses,
};
```

Used by `smartnet-ethereum-consensus` (`beacon_pre_consensus.rs`) and `smartnet-validator`.

## Out of scope

- REVM / execution (`smartnet-ethereum-evm`)
- Wallet RPC / MetaMask paths
- Genesis JSON `clique` key names (opaque config)

See [../../README.md](../../README.md) and `current-state.txt`.

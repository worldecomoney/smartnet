# SmartNet Compatibility Matrix

> **STATUS: CURRENT artifact — 2026-11-24**
>
> This matrix is evidence-backed. Statuses use the definitions below and do not claim more than the proof cited.
> Primary run artifact: [`TEST-RESULTS.md`](TEST-RESULTS.md) (TR-2026-11-24-01) with curated lines in [`LOGS.md`](LOGS.md).
> Source evidence is review-only unless a named test is confirmed in that artifact.

## Status definitions

| Status | Meaning |
| --- | --- |
| `Tested (unit)` | A **named test ran and passed** in TR-2026-11-24-01. Confirms that specific behavior, not a full e2e flow. |
| `Tested (historical)` | A dated artifact exists from a previous run; not re-run in the current audit. |
| `Tested` | A dated run artifact exists for the environment/scenario as stated. |
| `Implemented` | Source/config exists; no qualifying named test run or run artifact found for the claimed scenario. |
| `Not supported` | Deliberately unsupported by design — not a gap to be fixed. |
| `Not tested` | No qualifying artifact found. Do not read as "known broken". |

## Matrix

| Feature / scenario | Status | Evidence | Last checked |
| --- | --- | --- | --- |
| Ethereum mainnet/testnet network compatibility | `Not supported` | `doc/ARCHITECTURE.md` (system boundary, removed-by-design); no P2P/KAD, no forks/city gates, fixed native fee | 2026-11-24 source/docs review |
| Native legacy transaction `0x0` | `Tested (unit)` | `entry_gate::tests::mars_tx_types_allow_legacy_and_dynamic_fee_only` — accepts `0x0` (TR-2026-11-24-01) | TR-2026-11-24-01 |
| EIP-1559 transaction `0x2` | `Tested (unit)` | Same named test — accepts `0x2`; shared RPC/mesh submit seam in `smartnet/crates/rpc/rpc/src/eth/helpers/transaction.rs` | TR-2026-11-24-01 |
| Transaction type `0x1` | `Tested (unit)` (rejected at admission) / `Not supported` | Same named test — asserts `0x1`/`0x3+` refused; import path only warns. `doc/MARS.md` acceptance claim is stale. | TR-2026-11-24-01 |
| Chain ID enforcement | `Tested (unit)` | `mempool_policy::tests::chain_id_mismatch_is_rejected_and_latches_the_sender`, `mempool_policy::tests::chain_id_matching_config_is_admitted` | TR-2026-11-24-01 |
| Fixed native coin fee | `Tested (unit)` | `fixed_fee::tests::*` (5 tests, e.g. `smartnet_next_base_fee_is_fixed_for_any_parent`, `constants_match_derivation`); `entry_gate::tests::margin_one_requires_the_exact_fixed_fee`, `margin_zero_or_absent_never_checks_a_fee`, `margin_two_through_five_are_symmetric_up_and_down` | TR-2026-11-24-01 |
| ERC-20 gate / contract-filter behavior | `Tested (unit)` | `mempool_policy::tests::erc20_gate_*` (6 tests, e.g. `requires_the_token_balance_not_just_native`); `rtbc::tests::*` (14); `rtbc_prototype::*` (14) | TR-2026-11-24-01 |
| Full on-chain ERC-20 transfer / DeFi V2 flow | `Not tested` | Only gate/filter unit logic ran; no chain-level ERC-20/DeFi run artifact found | 2026-11-24 artifact review |
| HTTP-only RPC transport config (no WS JSON-RPC) | `Tested (unit)` | `config::tests::test_transport_rpc_module_config`, `test_transport_rpc_module_trim_config`, `test_unique_rpc_modules`, `test_rpc_server_config` — assert `ws().is_none()` / `ws_address().is_none()` | TR-2026-11-24-01 |
| WebSocket JSON-RPC | `Not supported` | Config never mounts WS RPC; tests above assert no WS transport. Stale comments in `smartnet/crates/node/core/src/args/rpc_server.rs` do not override binding code. | TR-2026-11-24-01 |
| Binary WebSocket Mesh — config-hash gate and in-process inject | `Tested (unit)` | `ws::tests::config_hash_gate_accepts_equal_and_rejects_changed_file`, `ws::tests::tx_injects_are_not_serialized_by_the_block_gate`, `ws::tests::block_injects_serialize_across_concurrent_callers`, `config_hash::tests::*` (3), `mesh::tests::gate_requires_port_at_least_1024` | TR-2026-11-24-01 |
| Live two-node / peer-to-peer Mesh e2e | `Not tested` | Component tests ran; no dated multi-node Mesh run artifact | 2026-11-24 artifact review |
| MetaMask-compatible HTTP surface (`eth_*` names, `0x0`/`0x2`) | `Implemented` | Method registration in RPC builder; tx-type rows above are `Tested (unit)`. No live wallet probe in the artifact. | 2026-11-24 source review |
| MetaMask browser e2e | `Not tested` | No browser/wallet run artifact found | 2026-11-24 artifact review |
| Solidity compilers `0.5.16`, `0.6.12`, `0.7.0`, `0.7.6`, `0.8.26` | `Tested (historical)` | [`SC-FILTER.md`](SC-FILTER.md) dated 2026-11-03: optimizer on/off and `--evm-version london`; 26/26 pass, 43 fixture variants / 19 fixtures. `0.8.20+` default PUSH0 needs `london`. Not re-run in this audit. | 2026-11-03 |
| JSON-RPC method inventory | `Implemented` | Source-extracted namespaces and methods (below). No live `rpc_modules` probe in the artifact. | 2026-11-24 source extraction |
| Multi-validator counts / 67% acceptance / 33% rejection | `Not tested` | Current runtime is interim `quorum = 1` with one-seal chair check; no shipped BFT/2-of-3 decision path or multi-node artifact. 3/16/17 counts are demo policy only, not runtime. | 2026-11-24 source/docs review |
| Validator disconnect / quorum loss / reconnect e2e | `Not tested` | No multi-node artifact. Current interim consensus has no minority-report behavior. | 2026-11-24 artifact review |
| Network partition / catch-up recovery e2e | `Not tested` | Logic-level tests ran (`persistence::tests::*`, `recovery::tests::*`, 18 recovery hits in TR-2026-11-24-01); no multi-node e2e artifact. | TR-2026-11-24-01 (unit only) |
| Debian 13 x86_64 / Rust `1.98.0` build and tests | `Tested` | TR-2026-11-24-01: both workspaces exit `0`, 1410 passed; exact env recorded in `TEST-RESULTS.md` | TR-2026-11-24-01 |
| macOS / Windows / other Rust versions | `Not tested` | Only the Debian 13 + Rust 1.98.0 environment was run | 2026-11-24 artifact review |
| Fuzzing | `Not run` | No fuzz campaign executed | 2026-11-24 |
| Benchmarks / performance claims | `Not published` | No benchmark methodology or artifact | 2026-11-24 |

## Evidence anchor — named tests confirmed in TR-2026-11-24-01

Every `Tested (unit)` row above maps to one or more of these confirmed `... ok` lines:

```text
entry_gate::tests::mars_tx_types_allow_legacy_and_dynamic_fee_only
entry_gate::tests::margin_one_requires_the_exact_fixed_fee
entry_gate::tests::margin_zero_or_absent_never_checks_a_fee
entry_gate::tests::margin_two_through_five_are_symmetric_up_and_down
mempool_policy::tests::chain_id_mismatch_is_rejected_and_latches_the_sender
mempool_policy::tests::chain_id_matching_config_is_admitted
mempool_policy::tests::erc20_gate_*                         (6 tests)
fixed_fee::tests::*                                         (5 tests)
rtbc::tests::*                                              (14 tests)
rtbc_prototype::*                                           (14 tests)
config_hash::tests::*                                       (3 tests)
ws::tests::config_hash_gate_accepts_equal_and_rejects_changed_file
ws::tests::tx_injects_are_not_serialized_by_the_block_gate
ws::tests::block_injects_serialize_across_concurrent_callers
mesh::tests::gate_requires_port_at_least_1024
config::tests::test_transport_rpc_module_config
config::tests::test_transport_rpc_module_trim_config
config::tests::test_unique_rpc_modules
config::tests::test_rpc_server_config
persistence::tests::* / recovery::tests::*                  (logic-level recovery)
```

Full lines are in [`LOGS.md`](LOGS.md). Source locations: `smartnet/crates/rpc/rpc-eth-api/src/entry_gate.rs`, `smartnet/crates/node/validator/src/mempool_policy.rs`, `smartnet/crates/chainspec/src/fixed_fee.rs`, `smartnet/crates/evm/evm/src/` (`rtbc.rs`, `rtbc_prototype.rs`), `smartnet/crates/net/mesh/`, `smartnet/crates/rpc/rpc-builder/src/config.rs`.

## JSON-RPC method inventory (source-extracted)

Allowed namespaces are `eth`, `net`, `web3`, `rpc`, and conditional `txpool` (`RPC_MEMPOOL`); other namespaces are hard-blocked. Names are preserved for wallet compatibility — do not rename `eth_*` methods.

- **`eth_*`:** `eth_protocolVersion`, `eth_syncing`, `eth_coinbase`, `eth_accounts`, `eth_blockNumber`, `eth_chainId`, `eth_getBlockByHash`, `eth_getBlockByNumber`, `eth_getBlockTransactionCountByHash`, `eth_getBlockTransactionCountByNumber`, `eth_getUncleCountByBlockHash`, `eth_getUncleCountByBlockNumber`, `eth_getBlockReceipts`, `eth_getUncleByBlockHashAndIndex`, `eth_getUncleByBlockNumberAndIndex`, `eth_getRawTransactionByHash`, `eth_getTransactionByHash`, `eth_getRawTransactionByBlockHashAndIndex`, `eth_getTransactionByBlockHashAndIndex`, `eth_getRawTransactionByBlockNumberAndIndex`, `eth_getTransactionByBlockNumberAndIndex`, `eth_getTransactionBySenderAndNonce`, `eth_getTransactionReceipt`, `eth_getBalance`, `eth_getStorageAt`, `eth_getStorageValues`, `eth_getTransactionCount`, `eth_getCode`, `eth_getHeaderByNumber`, `eth_getHeaderByHash`, `eth_simulateV1`, `eth_call`, `eth_fillTransaction`, `eth_callMany`, `eth_createAccessList`, `eth_estimateGas`, `eth_gasPrice`, `eth_getAccount`, `eth_maxPriorityFeePerGas`, `eth_blobBaseFee`, `eth_feeHistory`, `eth_mining`, `eth_hashrate`, `eth_getWork`, `eth_submitHashrate`, `eth_submitWork`, `eth_sendTransaction`, `eth_sendRawTransaction`, `eth_sendRawTransactionSync`, `eth_sign`, `eth_signTransaction`, `eth_signTypedData`, `eth_getProof`, `eth_getAccountInfo`, `eth_getBlockAccessListByBlockHash`, `eth_getBlockAccessListByBlockNumber`, `eth_getBlockAccessListRaw`, `eth_newFilter`, `eth_newBlockFilter`, `eth_newPendingTransactionFilter`, `eth_getFilterChanges`, `eth_getFilterLogs`, `eth_uninstallFilter`, `eth_getLogs`, `eth_subscribe`, `eth_unsubscribe`, `eth_callBundle`, `eth_sendBundle`, `eth_cancelBundle`, `eth_sendPrivateTransaction`, `eth_sendPrivateRawTransaction`, `eth_cancelPrivateTransaction`, `eth_config`.
- **`net_*`:** `net_version`, `net_peerCount`, `net_listening`.
- **`web3_*`:** `web3_clientVersion`, `web3_sha3` (plus conditional `web3_nodeInfo`).
- **`rpc_*`:** `rpc_modules`.
- **`txpool_*` (conditional):** `txpool_status`, `txpool_inspect`, `txpool_contentFrom`, `txpool_content`.

Notes: `eth_subscribe`/`eth_unsubscribe` are registered API methods, but **no WS JSON-RPC transport is mounted**, so they have no JSON-RPC WebSocket endpoint. `eth_sendRawTransactionConditional` is declared in an extension but is **not** registered in the builder — do not list it as supported.

## Stale contradictions resolved

| Conflict | Resolution |
| --- | --- |
| `doc/MARS.md` says `0x1` is always accepted | Stale. Current code refuses `0x1` at admission; `Tested (unit)` above. Trust `doc/FEES-CURR.md` and `doc/DOC-INDEX.md`. |
| Old WS comments in `smartnet/crates/node/core/src/args/rpc_server.rs` | Stale. HTTP-only binding code and `config.rs` tests are decisive: no WS JSON-RPC. |
| `doc/quorum-doc.md` describes `⌈2k/3⌉` quorum | Design only. Current runtime is interim `quorum = 1` with one-seal chair check. No BFT/minority-report claim. |
| `doc/FEES-CURR.md` cites old consensus crate paths | Fee rules are current; cite current code paths (`fixed_fee.rs`, `entry_gate.rs`) instead of the old paths. |

## What this matrix does not claim

- No browser/MetaMask, multi-node, partition, or 3+ validator e2e evidence exists in TR-2026-11-24-01.
- `Tested (unit)` confirms a named behavior, not a full production flow.
- Historical Solidity evidence (2026-11-03) was not re-run here.
- Consensus changes remain behind explicit operator approval; see [`IMP-TODO.md`](../IMP-TODO.md).

# SmartNet Test Results

> **STATUS: CURRENT artifact — 2026-11-24**
>
> This page records a real, reproducible run. It is the evidence target for `Tested` claims in [`COMPATIBILITY.md`](COMPATIBILITY.md). Source test presence alone is not evidence; the run below is.

## TR-2026-11-24-01 — full workspace tests

| Field | Value |
| --- | --- |
| Date (UTC) | `2026-11-24T10:03:15Z` |
| Code commit tested | `c8ddd2c551855cb236a545738065fa9fa0f7a26f` (`c8ddd2c`) |
| Rust | `rustc 1.98.0 (88d9e12ae 2026-08-18)` |
| OS | `Linux iphnone24 6.12.105+deb13-amd64 #1 SMP PREEMPT_DYNAMIC Debian 6.12.105-1 (2026-08-24) x86_64 GNU/Linux` |
| Workspace dir | `/home/usr3/dev/smartnet/30-bridge/smartnet` |
| Command 1 | `cargo test --offline --workspace` |
| Command 2 | `cargo test --offline --workspace --manifest-path ../calibri-lite/Cargo.toml` |
| Exit status | `0` for both commands |
| Log | [`LOGS.md`](LOGS.md) — curated actual log lines |

This run happened after the clean-shutdown fix and Phase 1 docs, at commit `c8ddd2c`. Later documentation-only commits do not change the tested code.

## Totals

| Workspace | Unit passed | Integration passed | Doc passed | Failed | Ignored | Test binaries / suites |
| --- | ---: | ---: | ---: | ---: | ---: | ---: |
| `smartnet` | 900 | 412 | 36 | 0 | 22 | 159 |
| `calibri-lite` | 62 | 0 | 0 | 0 | 0 | 4 |
| **Total** | **962** | **412** | **36** | **0** | **22** | **163** |

`smartnet` total passed: **1348**. `calibri-lite` total passed: **62**. Combined passed: **1410**.

The 22 ignored entries are not failures: 21 are ignored doctests and 1 is an ignored integration test. No test was filtered out.

## What this run covers

Representative test groups that actually ran in this artifact:

| Area | Evidence in the run |
| --- | --- |
| Transaction type gate | `entry_gate::tests::mars_tx_types_allow_legacy_and_dynamic_fee_only` and margin tests |
| Chain ID enforcement | `mempool_policy::tests::chain_id_mismatch_is_rejected_and_latches_the_sender`, `chain_id_matching_config_is_admitted` |
| Fixed native fee | `fixed_fee::tests::*`, `beacon_pre_consensus::tests::unsealed_chain_skips_fixed_fee_check` |
| ERC-20 / contract filter | `mempool_policy::tests::erc20_gate_*`, `rtbc::tests::*`, `rtbc_prototype::*` |
| Mesh config hash / gate | `config_hash::tests::*`, `ws::tests::config_hash_gate_accepts_equal_and_rejects_changed_file`, `mesh::tests::gate_requires_port_at_least_1024` |
| HTTP-only RPC config | `config::tests::test_transport_rpc_module_config`, `test_transport_rpc_module_trim_config`, `test_unique_rpc_modules`, `test_rpc_server_config` |
| Boot/config validation | CLI, bootstrap file, keystore, and node config tests across `smartnet-cli` and `smartnet-node-ethereum` |
| Storage/recovery logic | file-tier recovery and engine persistence tests in the workspace run |

Full per-suite lines are in [`LOGS.md`](LOGS.md).

## What this run does not prove

Do not upgrade these to `Tested` from this artifact:

| Scenario | Status | Reason |
| --- | --- | --- |
| MetaMask browser e2e | Not tested | No browser/wallet run artifact |
| 3+ validator network demo / 67% acceptance / 33% rejection | Not tested | No multi-node network artifact; current runtime is interim `quorum = 1` |
| Validator disconnection / quorum loss / reconnect e2e | Not tested | Component tests exist; no multi-node e2e run |
| Network partition recovery e2e | Not tested | No dated multi-node artifact |
| Fuzzing | Not run | No fuzz campaign was executed |
| Benchmarks | Not published | No benchmark methodology or artifact in this run |
| macOS / Windows | Not tested | Only Debian 13 x86_64 was run |
| Other Rust versions | Not tested | Only pinned Rust `1.98.0` was run |

## Historical evidence, not current artifact

- `doc/SC-FILTER.md` contains a dated 2026-11-03 Solidity compiler validation (solc `0.5.16`, `0.6.12`, `0.7.0`, `0.7.6`, `0.8.26`, with optimizer/London variants). Treat it as historical evidence, not as re-run in TR-2026-11-24-01.
- `MESH-FIX.md` contains older mesh test counts and explicitly leaves manual two-node e2e to the operator. Superseded by `doc/MESH-PAYLOAD-PROTOCOL.md` for protocol truth.

## Reproduce

```bash
cd /home/usr3/dev/smartnet/30-bridge/smartnet
cargo test --offline --workspace
cargo test --offline --workspace --manifest-path ../calibri-lite/Cargo.toml
```

Record the commit (`git rev-parse HEAD`), Rust version (`rustc --version`), OS (`uname -a`), and exit status. If any value differs, publish a new dated entry instead of editing this one.

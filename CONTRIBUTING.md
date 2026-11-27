# Contributing — pointer

## Read first

- [`AGENTS.md`](AGENTS.md) — standing rules: frozen core, removed-by-design list, KISS, fresh-file rule, commit rule.
- [`README.md`](README.md) — build and run.

## Work rules

- Do not restore removed-by-design subsystems (P2P/KAD, JWT/auth RPC, WS JSON-RPC, MDBX/RocksDB, forks/city gates, BOA/js).
- Do not change EVM/executor, contract core, ERC-20 behavior/fees, DeFi V2, `eth_*` names, tx types `0x0`/`0x2`, or Web3 wallet behavior.
- Do not claim `verified`/`Tested` without a dated run artifact in [`doc/TEST-RESULTS.md`](doc/TEST-RESULTS.md).
- Put shipped behavior under `CURRENT` and superseded material under `OLD / PLANNED` in durable docs.

## Before opening a PR

Run the checks, record the commit/env/exit status, use the PR template.

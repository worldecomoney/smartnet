# SmartNet Public Roadmap

> **STATUS: CURRENT — 2026-11-24**
>
> This is the public status view. It reflects shipped code and current docs, not future intent.
> Authority map: [`DOC-INDEX.md`](DOC-INDEX.md). Work plan with importance/difficulty: [`IMP-TODO.md`](../IMP-TODO.md).
> Consensus changes remain behind explicit operator approval.

## Implemented

- **Single-binary node** (engine + RPC + mesh + in-process validator) — one process and one config; `bin/smartnet` is the production binary.
- **HTTP-only `eth_*` JSON-RPC; WebSocket is binary Mesh** — wallet surface preserved, JSON-RPC WS intentionally never mounted.
- **Tx admission gate: `0x0` and `0x2` only; `0x1`/`0x3+` refused** — narrow shared RPC/mesh submit seam; MetaMask behavior unchanged.
- **Fixed native coin fee; ERC-20/DeFi fee market untouched** — `FIXED_GAS_PRICE_WEI` for natives, `TXPOOL_MARGIN` only an entry filter; no contract special-casing.
- **EVM/executor, contract core, ERC-20 behavior, DeFi V2** — frozen core kept as is; no rewrite attempted.
- **Closed-list Mesh protocol** — signed HELLO identity, `active && valid` allowlist, config-hash gate, sync/rumor/in-process inject; no P2P/KAD/NAT.
- **Both-file boot guard and endpoint self-check** — `validators.jsonl` and `mainnet.jsonl` required; no loopback/default fallback; mismatches are hard errors.
- **Clean shutdown on bootstrap errors** — graceful shutdown fires on success and error; clean marker registered after recovery, so failed boots do not cause a false rewind.
- **File tier + Fjall storage** — one write protocol and one crash-recovery path; MDBX/RocksDB removed by design.
- **Interim single-signature seal gate (`quorum = 1`)** — one recovered signer must equal the SELECT chair; this is bootstrap behavior, not BFT.
- **Durable public docs** — architecture, design decisions, doc index, security architecture, compatibility matrix, test evidence, and logs exist under `doc/`.
- **Dated test artifact TR-2026-11-24-01** — both workspaces pass; exact commit, command, environment, and limitations recorded.

## Under validation

- **Compatibility matrix** — every `Tested (unit)` row is tied to a named test in TR-2026-11-24-01; browser, multi-node, and e2e rows are explicitly `Not tested`.
- **Security architecture / threat model** — source-backed engineering model with explicit "what we do not protect" list; no independent audit and no guarantee of encryption or BFT.
- **CI on default branch and PRs** — `master` push/PR jobs run check, test, fmt, clippy, and typos; tag CI, published coverage, and dependency/audit gates are not in place.
- **Historical Solidity compiler evidence** — dated 2026-11-03 results are preserved as `Tested (historical)` in `doc/SC-FILTER.md`; not re-run in the current artifact.

## Planned

- **LITE multi-validator demo/harness (IMP-03)** — deferred by operator decision; LITE only, select tests, no consensus changes and no claim of shipped 67%/33% runtime behavior.
- **Continuous multi-node CI job** — only after a reproducible local demo is green.
- **Published coverage from the existing Makefile target** — method and artifact not yet published.
- **`cargo deny` / `cargo audit` in CI** — `smartnet/deny.toml` exists locally; CI integration not done.
- **SBOM and signed release provenance** — only when a real release process exists.
- **Owner assignment for `.github/CODEOWNERS`** — placeholder exists; no GitHub remote or org is configured, so no owner is asserted.
- **Any consensus-quorum change** — requires explicit operator approval; current runtime stays interim `quorum = 1`.

**Postponed — Mesh Explorer:** moved to a separate project; do not dilute this repo's evidence package.

**Out of scope — independent audit:** requires the docs and reproducible evidence above first; not scheduled here.

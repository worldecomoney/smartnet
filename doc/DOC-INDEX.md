# SmartNet Doc Index

> **STATUS: CURRENT — 2026-11-24**
>
> This page is the map of which `.md` file is authoritative for which topic.
> Root `.md` files are temporary by `AGENTS.md`; durable docs belong in `doc/`.

## 1. Authority rules

1. **Code wins over stale docs.** If a doc contradicts current source/config, the code is correct until the doc is updated.
2. **Younger data wins on contradiction.** This is the operator fresh-file rule in `AGENTS.md`. Timestamps below are observed file mtimes, not proof of creation date, but they are the current ordering signal.
3. **One topic, one authority.** Prefer the file in the table below; treat other files as historical or partial.
4. **No unverified claims.** Do not mark something `verified` without a dated test artifact.
5. **CURRENT / OLD split.** When updating a durable doc, keep shipped behavior under `CURRENT` and preserve superseded material under `OLD / PLANNED` with a clear non-normative note.

## 2. Authoritative docs

| Topic | Authority | Scope | Freshness signal |
| --- | --- | --- | --- |
| Project entry / build / run | [`README.md`](../README.md) | Current entry point; paths, build, run, config key pointers | 2026-11-22 |
| Repo rules | [`AGENTS.md`](../AGENTS.md) | Governance, removed-by-design list, frozen core, fresh-file rule | 2026-11-22 |
| Mesh wire protocol | [`MESH-PAYLOAD-PROTOCOL.md`](MESH-PAYLOAD-PROTOCOL.md) | CURRENT layers, opcodes, identity, config gate, sync/rumor; OLD history | 2026-11-24 |
| Architecture | [`ARCHITECTURE.md`](ARCHITECTURE.md) | Components, flows, Reth-derived vs custom, removed-by-design | 2026-11-24 |
| Boot sequence | [`BOOTSTRAP.md`](BOOTSTRAP.md) | CURRENT startup order: config, keystore, init/import arms, mesh sync/rumor, RPC bind, heartbeat | 2026-11-24 |
| Design decisions | [`DESIGN-DECISIONS.md`](DESIGN-DECISIONS.md) | Decision/trade-off log with must-not-restore markers | 2026-11-24 |
| Storage engine / DB + FS split / RLP separation | [`STORAGE.md`](STORAGE.md) | CURRENT source-audited write/read seam, recovery, RLP ingress separation, operator import footprint (original 2026-11-02 audit corrected) | 2026-11-24 |
| Transaction origins | [`ORIGIN.md`](ORIGIN.md) | Origin labels; wallet/mesh txs enter as `External` | 2026-11-22 |
| Txpool cleansing | [`TXPOOL-CLENSING-ON-BLOCK-PROP-ACCEPT.md`](TXPOOL-CLENSING-ON-BLOCK-PROP-ACCEPT.md) | Post-canonical pool cleansing and TTL behavior | 2026-11-24 |
| Fees | [`FEES-CURR.md`](FEES-CURR.md) | Fixed native fee, ERC-20 fee behavior, MARS mixed rules | 2026-11-22 |
| Quorum design | [`quorum-doc.md`](quorum-doc.md) | Future chair/quorum/BT/minority-report design only | 2026-11-18 |
| Security architecture / threat model | [`SECURITY_ARCHITECTURE.md`](SECURITY_ARCHITECTURE.md) | CURRENT protections, limitations, trust boundaries, compromised-peer distinction | 2026-11-24 |
| Compatibility matrix | [`COMPATIBILITY.md`](COMPATIBILITY.md) | Evidence-backed feature/status matrix anchored to TR-2026-11-24-01 | 2026-11-24 |
| Test evidence | [`TEST-RESULTS.md`](TEST-RESULTS.md) | Dated run artifacts with commit, command, environment, counts, limitations | 2026-11-24 |
| Test logs | [`LOGS.md`](LOGS.md) | Curated actual log lines for the dated run | 2026-11-24 |
| Public roadmap | [`ROADMAP.md`](ROADMAP.md) | Implemented / Under validation / Planned status view | 2026-11-24 |
| Changelog | [`CHANGELOG.md`](CHANGELOG.md) | Dated history derived from git; no release provenance claimed, can be ommited/removed/replaced | 2026-11-24 |
| Doc index | [`DOC-INDEX.md`](DOC-INDEX.md) | This page | 2026-11-24 |

## 3. Historical / partial docs

Do not use these as authority where they conflict with a newer doc or current code.

| File | Why partial | Current caveat |
| --- | --- | --- |
| [`MARS.md`](MARS.md) | Older mixed-rule notes | Its `0x1` acceptance claim is stale; current code refuses `0x1`. Some cited crate paths are old. |
| [`SECURITY.md`](SECURITY.md) | Old mempool/DeFi security material | Not an authoritative mesh threat model. Current authority: `SECURITY_ARCHITECTURE.md`. |
| [`PENDING-QUEUED.md`](PENDING-QUEUED.md) | Old queued-item notes | Historical only. |
| [`quorum-doc.md`](quorum-doc.md) | Design/future material | Current runtime is interim `quorum = 1`; no shipped BFT/minority reports. |
| [`FEES-CURR.md`](FEES-CURR.md) | Current topic authority, but some path references are old | Fee rules are current; verify code paths before citing them. |

## 4. Root temporary files

Per `AGENTS.md`, these are not durable docs. They may contain fresh operator input, but their facts must be moved into `doc/` before being treated as lasting documentation.

| File | Nature | Authority |
| --- | --- | --- |
| `AGENTS.md` | Standing repo governance | Yes, for rules |
| `README.md` | Public entry point | Yes, for entry/build/run |
| `SMARTNET-IMPROVE.md` | Fresh improvement source note (2026-11-24) | Source material for `IMP-TODO.md` |
| `IMP-TODO.md` | Fresh operator work plan | Source material for roadmap/TODO |
| `TOD.md` | Large task/bridge notes | Working notes only |
| `MESH-FIX.md` | Historical mesh fix record | Superseded by `MESH-PAYLOAD-PROTOCOL.md` |
| `1.md` | Historical external review | Not authority; no clone/tests |
| `BRIDGE-DOC.md`, `BRIDGE-USR.md` | Bridge working notes | Not architecture authority |

## 5. Planned durable docs


| Planned file | Purpose | Status |
| --- | --- | --- |
| `doc/QUORUM-LITE-DEMO.md` | LITE select-test demo; no consensus changes | Deferred by operator decision (IMP-03) |

## 6. Known contradictions to watch

- `doc/MARS.md` says `0x1` is accepted; current code refuses `0x1`. Trust the code and `doc/FEES-CURR.md`.
- `MESH-FIX.md` calls itself the newest mesh source, but it is older than `doc/MESH-PAYLOAD-PROTOCOL.md`; trust the protocol page.
- `doc/quorum-doc.md` describes `⌈2k/3⌉`; current runtime uses interim single-signature `quorum = 1` with one-seal chair checking. Do not present the design as shipped behavior.
- Historical docs may use `docs/` paths; this repo uses `doc/` as the surviving documentation directory.

## 7. Update rule

When adding or changing a durable doc:

1. Put shipped behavior under `CURRENT`.
2. Move superseded material under `OLD / PLANNED`.
3. Add or update the authority row in this index.
4. If the change affects a frozen invariant or removed-by-design item, check [`DESIGN-DECISIONS.md`](DESIGN-DECISIONS.md) first.

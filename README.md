# SmartNet

> **STATUS: CURRENT — project entry point**
>
> Single-binary EVM/DeFi-compatible execution network. This client has been extensively modified and does not feature any Ethereum legacy, P2P or "Swiss-knife" tools. 
> SmartNet primary objective is to facilitate the rapid deployment of fast, secure private EVM/DeFi compatible network.

## Docs

Start with the [doc index](doc/DOC-INDEX.md) for authority and freshness. The main durable pages are:

- [Architecture](doc/ARCHITECTURE.md) — components, runtime flows, Reth-derived vs custom layers, removed-by-design list.
- [Boot sequence](doc/BOOTSTRAP.md) — current startup order from keystore to heartbeat.
- [Design decisions](doc/DESIGN-DECISIONS.md) — trade-offs, consequences, and must-not-restore markers.
- [Storage engine](doc/STORAGE.md) — DB + FS split, RLP separation, recovery, and import footprint.
- [Mesh payload protocol](doc/MESH-PAYLOAD-PROTOCOL.md) — CURRENT/OLD binary mesh wire spec.
- [Security architecture](doc/SECURITY_ARCHITECTURE.md) — threat model, current protections, and explicit limitations.
- [Compatibility matrix](doc/COMPATIBILITY.md) — evidence-backed feature/status matrix.
- [Test results](doc/TEST-RESULTS.md) — dated run artifacts and what they do not prove.
- [Roadmap](doc/ROADMAP.md) — implemented / under validation / planned.
- [Changelog](doc/CHANGELOG.md) — dated history derived from git.

## ENV
Developed and tested with Debian 13

| Path | Role |
|------|------|
| [`smartnet/`](smartnet/) | Node app (engine + RPC + mesh + in-process validator) → `bin/smartnet` |
| [`calibri-lite/`](calibri-lite/) | Shared protocol libs (no binaries) |
| [`tools/`](tools/) | Rust workload tools (`txsend`, `tbench`) |
| [`bin/`](bin/) | **Production binary** after `make build` |
| `~/.smartnet/config.toml` | Single config for both roles (installed by `make install` from `smartnet/example.config.toml`) |

## Build

```bash
make build              # → bin/smartnet (calibri-lite + smartnet, offline — cached deps)
make online             # first build / after adding or updating deps (fetches into the offline cache)
make install            # config as invoking user + binary → /usr/local/bin/smartnet (needs sudo)
make clean              # bin/ binaries + heavy debug artifacts (keeps release cache for offline rebuild)
make clear              # full clean: binaries + ALL target dirs (drops the offline cache)
make offline            # alias of make build (offline is the default)
make check              # cargo check across all workspaces (offline)
cargo build --release   # same as make build (root hook crate)
```

Per-app (unified semantics — `clean` keeps the release/offline cache, `clear` drops it):

```bash
make -C smartnet build      # → bin/smartnet
make -C smartnet clean      # bin/smartnet only (keeps the cargo cache)
make -C smartnet clear      # bin/smartnet + all of smartnet/target
make -C calibri-lite clean  # calibri-lite/target/debug only
make -C calibri-lite clear  # all of calibri-lite/target
```

`smartnet-test-util` (optional): `make -C smartnet build-test-util` — stays under `smartnet/target/`, not `bin/`.

> **Build note.** `make build` is the authoritative build (it rebuilds the two workspaces
> incrementally). The root `cargo build --release` is only a convenience hook that shells out to
> `make build`; Cargo re-runs it only when the root package or a Makefile changes, **not** when a
> source file under `smartnet/` or `calibri-lite/` changes. Use `make build` after editing
> their sources.

## Run

```bash
# With the default config (~/.smartnet/config.toml):
smartnet

# With an explicit config:
smartnet --config /path/to/config.toml
```

Key switches inside the config (see `smartnet/example.config.toml` for the full annotated
template): `SEAL = true` runs the validator in-process; `NODE_HEARTBEAT` (ms) is the one heartbeat every service re-uses;
mesh links come from the closed-list peers file (`mainnet.jsonl` style) and keep themselves
alive (PING/PONG + bounded reconnect).

## License

SmartNet product code is **MIT** ([LICENSE](LICENSE)). Third-party dependencies keep their upstream licenses.

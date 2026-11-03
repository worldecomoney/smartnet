# SmartNet

> **STATUS: CURRENT — project entry point**
>
> Single-binary EVM/DeFi-compatible execution network influenced by the upstream client, Reth V 2.1, it is not a fork, 
> this client has been extensively modified and does not feature any Ethereum legacy, P2P or "Swiss-knife" tools. 
> Its primary objective is to facilitate the rapid deployment of fast, secure private EVM/DeFi compatible network.
> The validator is **merged into the node** — one process, one config, one heartbeat
> (`HEARTBEAT_MS`), sealing switched on/off with `SEAL` in the config. No `smartnet-validator`
> binary exists anymore.

| Path | Role |
|------|------|
| [`smartnet/`](smartnet/) | Node app (engine + RPC + mesh + in-process validator) → `bin/smartnet` |
| [`calibri-lite/`](calibri-lite/) | Shared protocol libs (no binaries) |
| [`tools/`](tools/) | Rust workload tools (`txsend`, `tbench`) |
| [`bin/`](bin/) | **Production binary** after `make build` |
| `~/.smartnet/config.toml` | Single config for both roles (installed by `make install` from `smartnet/example.config.toml`) |

## Build

```bash
make build              # → bin/smartnet (calibri-lite + smartnet, caches deps)
make install            # config as invoking user + binary → /usr/local/sbin/smartnet (needs sudo)
make clean              # bin/ binaries + heavy debug artifacts (keeps release cache for offline rebuild)
make clear              # full clean: binaries + ALL target dirs (drops the offline cache)
make offline            # build offline from the cached release deps
make check              # cargo check across all workspaces
cargo build --release   # same as make build (root hook crate)
```

Per-app (only touches that app's target + its bin file):

```bash
make -C smartnet build    # → bin/smartnet
make -C smartnet clean
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
template): `SEAL = true` runs the validator in-process; `HEARTBEAT_MS` sets the seal cadence;
mesh links come from the closed-list peers file (`mainnet.jsonl` style) and keep themselves
alive (PING/PONG + bounded reconnect).

## License

SmartNet product code is **MIT** ([LICENSE](LICENSE)). Third-party dependencies keep their upstream licenses.

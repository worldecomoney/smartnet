# tools — Rust tx tooling (the only tooling; `go-tools/` deleted)

A transaction sender (`txsend`, native + ERC-20 + deploy) and a benchmark (`tbench`)
that times **every meaningful step** (sign → encode → RPC round trip) and reports
TPS + latency percentiles.

## KEY EXCEPTION (read this first)

**This directory deliberately hard-codes throwaway TEST keys** derived from the original
`go-tools/1/seed` (dev mnemonic *"energy square tooth hub feature ordinary easily between
risk mosquito remind gesture"*, chain id 315 — the local load-test chain only).

- **Funny keys are OK here; real keys are not.** Never copy a real key into `tools/`.
- Tools are verified **only** against the local test chain with these test keys. If it works
  with funny keys it will work with real ones — real keys are never touched, never tested,
  never committed (see repo `PRIVATE-KEY.md` for the real-key process).
- Every key in `src/keys.rs` is marked `TEST-ONLY`. That file is the single source.

## Go-tools parity (VERIFIED 2026-10-21 — `go-tools/` since deleted)

Full replacement proven against the live chain (`http://127.0.0.1:8546`, validator);
every step accepted with `rejected=0` and on-chain settlement confirmed:

| go-tools (chainkit / evmtest)     | txsend equivalent                          | verified                                                    |
| -------------------------------- | ------------------------------------------ | ----------------------------------------------------------- |
| `status`                          | `status`                                   | head / basefee / gasprice / balance; `txpool_status` n/a    |
| `send`                            | `native`                                   | `--count --value --gas-price auto --gas-limit 0 --secure`   |
| (evmtest env loop)                | `native` with `TXS` / `SLEEP_MS`           | `TXS=2 SLEEP_MS=0 txsend native`                            |
| `deploy <file>` (wait receipt)    | `deploy <file> --gas-limit 0`              | receipt `status=0x1`, gasUsed `0x260aef`, head advanced     |
| `deploy` (fire)                   | `deploy <file> --no-wait`                  | hash returned, mined without waiting                        |
| `erc20 -token` (estimate gas)     | `erc20 --contract`                         | estimated `104728`, `--secure` balance gates, settled `1e30 − 3` |
| — (no Go equivalent)              | `erc20-balance`, `balance`                 | `eth_call` / `eth_getBalance`                               |

**Gas parity:** `--gas-limit 0` or absent = estimate once, reuse (chainkit semantics).
The WEMX token transfer costs ~105k gas — a fixed 60000 OOGs and can never seal.

## Build / run

```bash
cd tools
cargo build --release            # offline-safe: CARGO_NET_OFFLINE=true if needed
./target/release/txsend  native --count 3           # like the old evmtest loop
./target/release/txsend  balance
./target/release/tbench  --txs 100000 --conns 8     # TPS + per-step timing report
```

## Tools

| bin      | purpose                                                                |
| -------- | ---------------------------------------------------------------------- |
| `txsend` | place txs: `native`, `erc20` (transfer), `deploy` (bytecode file), `balance`, `erc20-balance`, `status` |
| `tbench` | concurrent load: per-step benchmark (sign / encode / rpc), TPS, percentiles |

RPC defaults to `http://127.0.0.1:8545` (override with `--rpc` / `TXSEND_RPC`).
Gas price is the chain-fixed `238095238095` wei; chain id 315.

## Notes

- `tbench --stripes N` spreads load over the first N seed keys (each needs a funded
  genesis alloc before use); default `1` = the single funder key (TEST-ONLY).
- ERC-20 bench needs a deployed token first: `txsend deploy <bytecode.hex>`.
- `TXSEND_DUMP=1` prints every request body (RPC framing debug).
- Point load tests at the **validator** RPC directly to bypass lossy tx gossip (see
  repo `doc/MESH-PAYLOAD-PROTOCOL.md` for the current mesh/gossip behavior).

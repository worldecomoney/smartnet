# Transaction origin, TTL override, and the queued-tx investigation (2026-11-22)

Status: **TTL override applied; full removal of the origin label postponed pending study.**
This file records the findings so the decision can be revisited without re-deriving it.

## 1. The stuck transaction

- Tx `0xad439fb3f03760b25ed98e881ea22563c830d30aa5325618a1cd934fd7abd466`, sender/chair
  `0x24bf191a358c09e79b9ead2d8999d08590b5af31`, nonce `0xb` (11).
- Chain nonce was `0x3` at observation (`latest` and `pending` both), tip `0xafc8`.
- Nonces 4–10 are missing, so nonce 11 cannot execute. `gate_executable_nonces` can only build a
  contiguous prefix; the tx stays in `queued`. `TXPOOL_QUEUED_FOLDED` is advisory only, not a seal
  decision. `ALLOW_EMPTY = false` meant nothing was sealed.
- A wallet cancel at nonce 11 can replace the queued tx, but it cannot close the gap. Further wallet
  sends get nonce 12, 13, … and also queue behind it. MetaMask will not pick nonce 4 automatically.

## 2. Config key split (operator ruling)

### Three txpool keys — node-owned, every node parses/pins them

- `TXPOOL_QUEUED_TTL_SECS = 600` — used verbatim; boot validation `1..=600`; anything else is a
  boot error, no silent clamp. 600 s is both default and ceiling.
- `TXPOOL_PENDING_TTL_SECS = 0` — no cap; when `> 0`, pending txs older than the cap are swept.
- `TXPOOL_VAL_MAX_TXS = 512` — absent/blank keeps 512, `0` rejected.

### Six validator-only keys

`SEAL`, `ALLOW_EMPTY`, `MAX_NATIVE_PER_BLOCK`, `MAX_ERC_20_PER_BLOCK`,
`MAX_CONTRACTS_PER_BLOCK`, `MAX_NFT_PER_BLOCK`.

`TXPOOL_VAL_MAX_TXS` is the **validator's own pick count**: at most N txs from the snapshot it
fetches enter one seal pass (oldest first, FIFO), applied at `run_once.rs:594`. It is not a node-side
allotment. The node has no inclusion criteria — `txpool_content()` serves the full pool, uncapped —
and the validator alone decides which tx to include or skip. Config location is node-side; the
consumer and the count remain the validator's.

## 3. TTL override (applied — commit `cb3fcad`)

Observation: the queued tx was not evicted. The initial diagnosis blamed the origin exemption —
the old predicate was `tx.origin.is_external() || no_local_exemptions`, with
`no_local_exemptions` fed by `--txpool.no-locals` (default false). **That diagnosis was wrong.**
Code truth, traced end to end:

- Mesh rumors enter at `net/mesh/src/ws.rs:907` (`RumorKind::Tx => rpc.send_raw_transaction`) →
  `NodeInjector::send_raw_transaction` (`node/ethereum/src/mesh.rs:135-136`) →
  `EthApiServer::send_raw_transaction` (`rpc/rpc-eth-api/src/core.rs:878-885`) →
  `EthTransactions::send_raw_transaction` (`rpc/rpc-eth-api/src/helpers/transaction.rs:79-97`)
  → `send_transaction(TransactionOrigin::External, ...)` at line **95**.
- Wallet/MetaMask submission also uses `eth_sendRawTransaction`, so it is `External` too.
- `TransactionOrigin::Local` is assigned only at `helpers/transaction.rs:499`, inside the
  node-signed `eth_sendTransaction` request path; `core.rs:868-874` states SmartNet has no local
  signers and directs clients to `eth_sendRawTransaction`.

"Local inject" in `mesh/src/inject.rs` and `mesh.rs:13-15` means *in-process delivery* (no loopback
socket), **not** the txpool origin tag. No mesh or wallet tx is `Local`, so the old exemption never
applied to them and cannot explain the non-eviction. The observation is instead consistent with a
binary still running the original queued default of 8 h (`b3eb7be`: `8 * 60 * 60` =
28800 s), before the node-owned 600 s policy was introduced.

The change was kept because it removes a branch with no meaning in this network, not because a
local-labeled tx was actually being exempted. Applied change:

- `txpool/src/maintain.rs`: `is_stale_by_ttl` now checks age only — the origin test and the
  `no_local_exemptions` parameter are gone. Behavior for `External` txs is unchanged; a
  hypothetical `Local` tx now ages out too.
- `MaintainPoolConfig.no_local_exemptions` removed; node builder no longer passes it.
- The 60 s timer tick and the canonical-event sweep both call the same `sweep_stale_by_ttl`.
- Expected eviction: ~10–11 min from pool-insert time (600 s TTL + up to one 60 s tick).
- TTL sweep logs at `debug`, so the repeating `INFO TXPOOL_TOTAL` / `TXPOOL_QUEUED_FOLDED` lines
  (validator beat, ~600 ms) do not show eviction. Verify by checking the hash leaves the pool.

Deployment caveat: requires `make build` + coordinated restart. The local-tx backup/reload task
still selects txs by origin and can reinsert a tx from a backup written before eviction, so use a
clean shutdown and be aware a stale backup can resurrect one.

## 4. Same-fee native cancel (applied earlier)

- Node policy switch `native_same_fee_cancel` (default on) feeds `PoolConfig`; the pool skips the
  +10% underpriced check only for a plain native transfer replaced by a native zero-value, empty
  calldata, non-blob, non-auth tx at the same sender+nonce with no fee field lowered.
- "0 fee cancelation" means **no extra fee / no new fee bump**, never a literal 0 wei price. Fixed
  native fees and fixed-price policy are unchanged.
- ERC-20/calldata, contract creation, blob and lower-fee replacements keep the +10% bump.
- Scope limit: only helps a tx at an executable nonce. It does not close a nonce gap, and if the
  wallet refuses to broadcast at the same fee, the node never sees a cancel. No drop-by-hash RPC was
  exposed, and the former `admin_*` RPC namespace has since been removed entirely.

## 5. Remaining origin-label findings (postponed — study before removal)

No mesh-side "strip the local tag on onboarding" patch is needed: the onboarding path already
submits with `TransactionOrigin::External` (see §3). Do not change `NodeInjector` for that purpose.

The label still exists and has these behavioral uses:

- `txpool/src/config.rs` and `validate/eth.rs:492` — `LocalTransactionConfig::is_local` gives local
  txs price/eviction exemptions. Redundant with fixed fees, but it is behavior.
- `validate/eth.rs:647-652` — propagation: `Private` never propagated, `Local` configurable via
  `--txpool.no-local-transactions-propagation`, `External` always propagated. Mesh has its own
  rumor propagation; whether this flag is truly redundant needs confirmation.
- `maintain.rs` backup task — selects/restores txs by origin via `get_transactions_by_origin(Local)`.
- Node args — `--txpool.no-locals`, `--txpool.locals`, the propagation flag.
- `TransactionOrigin` (Local/External/Private) is defined in `txpool/src/traits.rs` and threaded
  through admission, validation, listeners, batching, backup and tests: roughly 208 references in
  txpool, 7 in RPC, 20 in node crates (~235 total).

Options considered:

1. **Stop here (chosen).** TTL is origin-agnostic; other local-label behavior unchanged.
2. Strip only the behavioral uses (exemptions, propagation switch, origin-based backup), leave the
   enum as inert API plumbing — same practical behavior, smaller blast radius.
3. Delete `TransactionOrigin` entirely — literal removal, large signature/test refactor, higher
   regression risk on the `eth_sendRawTransaction` path used by MetaMask. No behavioral gain over
   option 2.

Do not start option 2 or 3 without a fresh decision. Any change must preserve `eth_*` RPC names,
tx types `0x0`/`0x2`, MetaMask behavior, fixed native fees, mesh, and consensus.

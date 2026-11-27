# Post-block-acceptance txpool cleansing (2026-11-24)

Status: **investigation / confirmation only, no code changed.** This answers the specific question:
when a block proposal arrives and the node still holds in its txpool transactions that are already
inside that block, exactly how are those txs removed after acceptance?

All line numbers are from commit `74f9019`. The paths below were verified by reading the code, not
inferred from RPC behavior.

## 0. Short answer

The proposer/validator does **not** delete anything from the pool. Neither does `newPayload`, the
payload validator, or the block builder. Cleansing is a **node-local, post-canonicalization**
reaction:

```text
block proposal (newPayload / in-process validator)
  → engine executes + inserts the block
  → block becomes CANONICAL (MakeCanonical or FCU apply_chain_update)
  → CanonicalStateNotification { mined_transactions, changed_accounts }
  → provider canonical-state stream
  → txpool maintenance task
  → handle_canonical_commit / handle_canonical_reorg
  → CanonicalStateUpdate
  → Pool::on_canonical_state_change
  → TxPool::on_canonical_state_change
       (a) prune each mined tx by HASH
       (b) invalidate txs whose nonce < new state nonce
       (c) recompute fees / allowance → promote or demote survivors
  → plus an origin-agnostic TTL sweep on the same event
```

Mechanism (a) is the direct answer: every hash in the accepted block is looked up in the pool and
pruned. Mechanism (b) is the account-state backstop. TTL is a third, independent leftover sweep.

Important: a `VALID` block that is **not promoted to canonical** does not cleanse the pool.

---

## 1. Proposal arrival — no pool mutation at this stage

Two entry points carry a proposed block into a node:

- Remote / RPC: `EngineExtensionsApi::new_payload`
  (`smartnet/crates/rpc/rpc-engine-api/src/engine_extensions.rs:31`), shared by
  `smartnet_newPayload` / `wem_newPayload`. Structured payload or `BlockRlp` is decoded at `:47-69`
  and handed to `beacon_engine_handle.new_payload(...)` at `:83-85`.
- In-process validator: `ValidatorIo::submit_block_rlp`
  (`smartnet/crates/node/ethereum/src/validator.rs:210`) decodes the sealed RLP and calls
  `self.engine.new_payload(execution_data_from_block_rlp(block), true, true)` at `:215-219`.

Neither path calls `remove_transactions`, `prune_transaction_by_hash`, or
`on_canonical_state_change`. They only feed the engine.

Inside the engine tree (`smartnet/crates/engine/tree/src/tree/mod.rs`):

- `on_new_payload` at `:724`; the `SMARTNET_NEW_PAYLOAD` debug line is at `:759-770`;
- `ConsensusEngineEvent::BlockReceived` at `:771-772`;
- invalid-ancestor short circuit at `:776-780`;
- `try_insert_payload` (backfill idle) or `try_buffer_payload` at `:787-791`;
- execution/insertion at `:820` → `:2718` → `:2759` → `:2863` (`insert_executed`).

Execution and tree insertion do **not** touch the txpool. This is why the cleansing is described as
*post-acceptance*, not *at acceptance*.

## 2. The trigger that actually matters: canonical promotion

The txpool listens to canonical-chain notifications, not to `VALID` status by itself.

In `on_new_payload`, after a valid outcome, a `TreeEvent::MakeCanonical` is scheduled when either
condition holds (`smartnet/crates/engine/tree/src/tree/mod.rs:793-810`):

1. the block is the Engine **sync target** head (`is_sync_target_head`), or
2. SmartNet linear-extension mode: the block is `VALID`, backfill is idle, and its
   `parent_hash == canonical_block_hash()`.

Condition 2 is hard-wired on for this network in
`smartnet/crates/node/core/src/args/engine.rs:516-549`, with
`.with_auto_canonical_new_payload_on_linear_extend(true)` at `:549`. So a normal validator block that
extends the tip becomes canonical immediately from the same `newPayload` call — no separate FCU is
required.

The FCU path reaches the same machinery: `apply_chain_update` at `tree/mod.rs:1121`, then
`on_new_head` → `on_canonical_chain_update` at `:1157-1159`.

Tree event dispatch is `on_maybe_tree_event` (`:1807`) → `on_tree_event` (`:1818`) →
`TreeAction::MakeCanonical` → `make_canonical` (`:1798`). Finally
`on_canonical_chain_update` (`:2500`) updates the in-memory canonical state and calls
`notify_canon_state` at `:2529`, emitting `CanonicalChainCommitted` at `:2532`.

## 3. What the notification carries

`NewCanonicalChain::to_chain_notification`
(`smartnet/crates/chain-state/src/in_memory.rs:955`, chains built at `:968`) produces either
`CanonStateNotification::Commit { new }` or `Reorg { old, new }`.

The two fields the pool cares about:

- **mined transaction hashes**: `Chain::transaction_hashes()`
  (`smartnet/crates/evm/execution-types/src/chain.rs:443`)
- **changed accounts**: `ExecutionOutcome::changed_accounts()`
  (`smartnet/crates/evm/execution-types/src/execution_outcome.rs:386`)

They are relayed into `CanonicalStateUpdate`
(`smartnet/crates/txpool/src/traits.rs:961-978`):

```rust
pub struct CanonicalStateUpdate<'a, B: Block> {
    pub new_tip: &'a SealedBlock<B>,
    pub pending_block_base_fee: u64,
    pub pending_block_blob_fee: Option<u128>,
    pub changed_accounts: Vec<ChangedAccount>,
    pub mined_transactions: Vec<B256>,   // <-- hashes of txs in the accepted block(s)
    pub update_kind: PoolUpdateKind,
}
```

## 4. Node wiring: canonical stream → pool maintenance

- Provider subscription: `BlockchainProvider::subscribe_to_canonical_state`
  (`smartnet/crates/storage/provider/src/providers/blockchain_provider.rs:661`).
- Pool build/spawn: `EthereumPoolBuilder::build_pool`
  (`smartnet/crates/node/ethereum/src/node.rs:645-694`).
- The actual handoff is `spawn_pool_maintenance_task`
  (`smartnet/crates/node/builder/src/components/pool.rs:272`):
  - `:283` `let chain_events = ctx.provider().canonical_state_stream();`
  - `:288` reads the pinned node-owned policy `smartnet_node_core::txpool_policy::txpool_policy()`;
  - `:289-303` spawns the `"txpool maintenance task"` with the provider client, the pool, the
    canonical stream, and `MaintainPoolConfig { queued_ttl, pending_ttl, ttl_sweep_interval, ..Default::default() }`.

The comment at `:286-287` states the ownership rule explicitly: every node owns and cleanses its
own txpool; TTL policy is pushed down from the node, not hidden in the pool crate. The node-owned
keys and defaults live in `smartnet/crates/node/core/src/txpool_policy.rs:10-55` (queued 600 s,
pending `None`, sweep 60 s, val-max 512) and are pinned in
`smartnet/crates/node/cli/src/app.rs:1076-1095`.

## 5. Maintenance dispatch

`maintain_transaction_pool` (`smartnet/crates/txpool/src/maintain.rs:198`) is the loop:

- `:227-241` sets initial `BlockInfo` from the latest header;
- `:270-276` on `Drifted`, treats all senders as dirty;
- `:337-339` timer tick → `sweep_stale_by_ttl`;
- `:347-352` the first canonical event after startup forces `Drifted` for revalidation;
- `:380-382` **every canonical event also runs `sweep_stale_by_ttl`** before commit/reorg handling;
- `:384-395` `Reorg` → `handle_canonical_reorg`; `:396+` `Commit` → `handle_canonical_commit`.

### 5.1 Commit path (the normal case for the user's scenario)

`handle_canonical_commit` (`maintain.rs:561`):

- `:583-589` computes next-block base/blob fees from the new tip;
- `:600-619` **depth gate**: `depth = tip.number().abs_diff(pool_info.last_seen_block_number)`;
  if `depth > max_update_depth` (default 64), the pool is marked `Drifted`, only `set_block_info`
  is called, and this pass returns — mined hashes in that deep segment are *not* removed here;
- `:621-626` collects changed accounts and clears their dirty flags;
- `:628` `let mined_transactions = blocks.transaction_hashes().collect();`
- `:630-636` if the commit does not connect to the pool's last-seen block hash, mark `Drifted`;
- `:639-647` build `CanonicalStateUpdate { ..., mined_transactions, update_kind: Commit }` and call
  `pool.on_canonical_state_change(update)`.

### 5.2 Reorg path

`handle_canonical_reorg` (`maintain.rs:424`) does the same hash removal for the **new** chain
(`:502-503`, `:533-542`, `update_kind: Reorg`), then re-injects txs that were mined only on the old
chain (`:507-530` → `:544-551` `pool.add_external_transactions(...)`). Note at `:547-551`: the
original origin is no longer known, so re-injected txs come back as `External`.

## 6. Pool-level removal — the two mechanisms

### 6.1 Outer wrapper

`Pool::on_canonical_state_change` (`smartnet/crates/txpool/src/pool/mod.rs:527`):

- `:534` `validator.on_new_head_block(new_tip)` — fork tracking / validation limits, not deletion;
- `:536` converts changed accounts via `changed_senders` (`:248-259`). The `?` on
  `identifiers.sender_id(&address)?` means accounts not already tracked by the pool are **skipped**;
- `:539-544` calls the inner `pool.write().on_canonical_state_change(...)`;
- `:547` deletes blobs for discarded txs; `:550` notifies listeners.

### 6.2 Inner pool: hash prune, then account invalidation

`TxPool::on_canonical_state_change` (`smartnet/crates/txpool/src/pool/txpool.rs:651-698`):

```rust
// Remove all transaction that were included in the block
for tx_hash in &mined_transactions {
    if self.prune_transaction_by_hash(tx_hash).is_some() {
        removed_txs_count += 1;
    }
}
```

at `:663-667`, followed by the `removed_transactions` metric (`:670`), fee updates
(`:672-684`), `set_block_info` (`:687`), and the outcome (`:692-697`). `_update_kind` is deliberately
unused at `:656`: commit and reorg share the same inner removal mechanics.

`prune_transaction_by_hash` (`:1141-1147`):

```rust
let (tx, pool) = self.all_transactions.remove_transaction_by_hash(tx_hash)?;
self.remove_from_subpool(pool, tx.id())
```

The comment at `:1135-1140` says this is the primitive intended for txs included in a block:
descendants are *not* parked as they would be for a generic removal, because their ancestor is now
on-chain.

### 6.3 Nonce/account invalidation (backstop)

`AllTransactions::update` (`txpool.rs:1533`) walks each tracked sender. At `:1565-1573`:

```rust
if let Some(info) = changed_accounts.get(&id.sender) {
    if id.nonce < info.state_nonce {
        updates.push(PoolUpdate { id: ..., current: tx.subpool,
                                  destination: Destination::Discard });
        continue 'transactions;
    }
```

`process_updates` (`:999-1009`) applies `Destination::Discard` via `prune_transaction_by_id` (`:1152`).
Balance changes do not remove transactions by themselves — they promote/demote/queue based on
allowance and fee conditions (`:1582+`).

### 6.4 TTL is a separate fallback, not the mined-tx mechanism

`is_stale_by_ttl` (`maintain.rs:96-109`) is `now.duration_since(tx.timestamp) > ttl`, with an explicit
comment that origin is irrelevant. `sweep_stale_by_ttl` (`:111-154`) sweeps queued past `queued_ttl`
and pending only when `pending_ttl` is `Some`, then `pool.remove_transactions(all)` at `:150`.
It runs on the timer (`:337-339`) and on every canonical event (`:380-382`). A mined tx is normally
gone via hash prune before TTL matters; TTL catches leftovers when the chain stalls or a sweep was
skipped.

---

## 7. The exact scenario, step by step

Node holds tx `T` (pending or queued). A proposal arrives containing `T`.

1. `newPayload` arrives; the block is executed and inserted. **Pool still has `T`.**
2. Block is valid and linearly extends the tip → `MakeCanonical` (`tree/mod.rs:793-810`), or the
   FCU head update path does the same (`tree/mod.rs:1157-1159`). **Pool still has `T`.**
3. `notify_canon_state` emits a commit notification whose `mined_transactions` contains `T`'s hash
   and whose `changed_accounts` contains `T`'s sender.
4. The maintenance task receives it, runs the TTL sweep, then `handle_canonical_commit` collects
   the hashes and calls `on_canonical_state_change`.
5. `TxPool::on_canonical_state_change` loops the mined hashes; `prune_transaction_by_hash(T.hash)`
   removes `T` from `all_transactions` and from its subpool.
6. The same update rechecks the sender: any remaining tx of that sender with
   `nonce < state_nonce` is discarded (`:1565-1573`), and survivors are promoted/demoted on the new
   fee/allowance state.
7. Listeners are notified (`pool/mod.rs:550`); `txpool_content` thereafter no longer shows `T`.

Properties worth stating explicitly:

- **Origin is irrelevant.** Hash prune and nonce invalidation do not look at `Local`/`External`;
  TTL was made origin-agnostic in `cb3fcad`. Mesh rumors and MetaMask both enter via
  `eth_sendRawTransaction` as `External` (`doc/ORIGIN.md` §3).
- **Validator is not special.** The proposing validator's own node cleanses itself through the same
  canonical stream; every other node does the same independently. There is no broadcast of "please
  drop this tx" and no proposer-side pool edit.
- **Queued txs are covered too.** If the block contains an earlier nonce of the same sender, the
  account update can promote a previously queued successor; if a queued tx's own nonce is now below
  the state nonce, it is discarded. If it is neither included nor stale, it stays queued (or is
  re-evaluated under the new nonce gap).
- **No literal transaction-type handling.** The path is by hash and sender nonce only; native,
  ERC-20, contract-deploy and DeFi txs all follow it identically. Fixed native fees and `eth_*`
  names are untouched.

## 8. Conditions where cleansing is absent or delayed

These are the caveats found; they are properties of the current code, not bugs claimed here.

1. **Non-canonical `VALID` block**: `MakeCanonical` is scheduled only for sync-target head or
   linear extension (`tree/mod.rs:793-810`). A `VALID` side fork does not emit a canonical
   notification, so the pool is not cleansed by it.
2. **Backfill/buffering**: `try_buffer_payload` is used when `backfill_sync_state` is not idle
   (`tree/mod.rs:787-791`), which suppresses immediate canonical promotion.
3. **Deep commits**: `depth > 64` skips hash removal for this pass (`maintain.rs:600-619`); the pool
   is marked `Drifted` and later account reloads may clean by nonce.
4. **Known-sender-only nonce invalidation**: `changed_senders` uses `sender_id(...)?`
   (`pool/mod.rs:248-259`); a sender unknown to the pool cannot be invalidated by account state.
5. **Timing**: hash prune only works if the tx is still in the pool when the notification is
   applied. A tx that arrives after the event (or was already evicted) simply finds no match.
6. **Reorg re-injection loses origin**: old-chain-only txs are re-added as `External`
   (`maintain.rs:544-551`).
7. **`update_kind` ignored by inner pool** (`txpool.rs:656`): no special reorg-specific deletion.
8. **RLP import bypass**: `smartnet/crates/node/ethereum/src/import.rs:1-31` writes directly to
   storage, with no `newPayload`, engine queue, tree insert or canonical notification. Imported
   blocks therefore produce no per-block `mined_transactions` cleanup; only later canonical events
   or drift-based account reloads can cleanse by nonce for tracked senders.
9. **Validator snapshot is non-destructive**: `val_seq_20_fetch_txpool`
   (`smartnet/crates/node/validator/src/run_once.rs:530`) reads `txpool_content` (a read-only RPC at
   `smartnet/crates/rpc/rpc/src/txpool.rs:42,64`), folds queued into candidates at `:573`, and caps
   only the snapshot at `:588-594`. The comment at `:592` says dropped txs stay in the node's pool
   for a later block. `val_seq_80_submit` (`:958`, `:968-971`) only submits the block.
   `payload_validator.rs` and `rpc/src/validation.rs` have no txpool access. `best_txs.mark_invalid`
   in builder iteration is iterator-local.

## 9. Tests that touch this behavior

- `smartnet/crates/txpool/src/pool/txpool.rs`: `test_on_canonical_state_change_no_double_processing`
  (`:2529`), `test_canonical_state_change_with_basefee_update_regression` (`:2581`),
  `test_basefee_decrease_with_empty_senders` (`:2671`),
  `test_basefee_decrease_account_makes_unfunded` (`:2708`), `discard_nonce_too_low` (`:3162`),
  `test_remove_transactions` (`:3264`), `test_remove_transactions_middle_pending_hash` (`:3303`),
  `test_remove_transactions_and_descendants` (`:3345`),
  `test_remove_transactions_by_sender` (`:3418`).
- `smartnet/crates/storage/provider/src/providers/blockchain_provider.rs:1120`
  `test_canon_state_subscriptions`.
- `smartnet/crates/engine/tree/src/tree/tests.rs:771` `test_tree_state_on_new_head_reorg`, plus
  make-canonical / new-payload tests near `:2101` and `:2139`.
- `smartnet/crates/node/core/src/txpool_policy.rs` and `smartnet/crates/node/cli/src/app.rs:1497`
  cover TTL/sweep/policy defaults.

No focused integration test was found that asserts the whole chain
`canonical notification → mined_transactions → on_canonical_state_change → hash removal` through
the maintenance task. The lower-level pool tests and provider subscription tests exist, but that
exact end-to-end assertion is missing.

## 10. Assessment: can we call this "all good"?

Verdict: **the live validator/proposal path is good; we cannot sign "no issues in all cases."**
The four items below are exactly what prevents an unconditional sign-off. None is known to break
the normal flow exercised in E2E, and no code was changed for this investigation.

1. **Deep catch-up skips hash removal.** If the pool is more than 64 blocks behind,
   `handle_canonical_commit` returns early after only `set_block_info` (`maintain.rs:600-619`).
   Cleanup then depends on the `Drifted` account reload and nonce invalidation
   (`nonce < state_nonce`) for *tracked* senders. In practice that still removes included txs, but it
   is a fallback mechanism, not the same hash-prune guarantee as the normal path. (See §8 item 3.)

2. **A `VALID` non-canonical block does not cleanse the pool.** `MakeCanonical` is scheduled only
   for sync-target head or linear extension (`tree/mod.rs:793-810`). This is arguably correct —
   those txs may still be valid for the canonical chain — but it means "block accepted" is not the
   same as "pool cleansed"; only "block made canonical" triggers it. (See §8 item 1.)

3. **RLP import bypasses the engine/canonical path entirely.** The import tool writes straight to
   storage with no `newPayload`, engine queue, tree insert or canonical notification
   (`node/ethereum/src/import.rs:1-31`), so imported blocks produce no per-block
   `mined_transactions` cleanup. This is by design for the offline import tool, but it is a real
   exception if blocks are ever fed that way on a live node. (See §8 item 8.)

4. **No focused integration test** asserts `canonical notification → mined_transactions →
   on_canonical_state_change → hash removal` through the maintenance task. The lower-level pool and
   provider-subscription tests exist, so a regression in the wiring would be invisible until
   runtime. (See §9 above.)

Recommendation: treat the current behavior as correct for production. If an unconditional "no
issues" statement is required, the cheap fix for item 4 is an integration test over that chain —
not a txpool code change. The remaining three items are known boundary conditions of the current
design and were recorded, not patched.

## 11. File index

| Concern | File |
| --- | --- |
| RPC / validator block entry | `smartnet/crates/rpc/rpc-engine-api/src/engine_extensions.rs`, `smartnet/crates/node/ethereum/src/validator.rs` |
| Acceptance + canonicalization | `smartnet/crates/engine/tree/src/tree/mod.rs`, `smartnet/crates/node/core/src/args/engine.rs` |
| Canonical notification content | `smartnet/crates/chain-state/src/in_memory.rs`, `smartnet/crates/evm/execution-types/src/chain.rs`, `.../execution_outcome.rs`, `smartnet/crates/txpool/src/traits.rs` |
| Stream → maintenance wiring | `smartnet/crates/storage/provider/src/providers/blockchain_provider.rs`, `smartnet/crates/node/builder/src/components/pool.rs`, `smartnet/crates/node/ethereum/src/node.rs` |
| Commit/reorg handlers + TTL sweep | `smartnet/crates/txpool/src/maintain.rs` |
| Outer/inner pool removal | `smartnet/crates/txpool/src/pool/mod.rs`, `smartnet/crates/txpool/src/pool/txpool.rs` |
| Node-owned policy | `smartnet/crates/node/core/src/txpool_policy.rs`, `smartnet/crates/node/cli/src/app.rs` |
| Validator snapshot/submit | `smartnet/crates/node/validator/src/run_once.rs`, `smartnet/crates/rpc/rpc/src/txpool.rs` |
| Import bypass | `smartnet/crates/node/ethereum/src/import.rs` |

Related reading: `doc/ORIGIN.md` (origin labels, TTL override, same-fee cancel),
`doc/PENDING-QUEUED.md` (pending vs queued concepts), `doc/EVM-ENGINE-CHANGES.md` (engine wiring).

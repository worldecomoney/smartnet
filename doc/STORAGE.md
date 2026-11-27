# STORAGE.md — one storage engine, RLP separation, and the DB + FS split

> **STATUS: CURRENT — source-audited 2026-11-24** (original audit 2026-11-02; stale
> claims corrected in place; see §0 and §5).
> Authority map: [`DOC-INDEX.md`](DOC-INDEX.md). Code wins where any page disagrees
> with source. Current companions: [`ARCHITECTURE.md`](ARCHITECTURE.md) (component
> view), [`MESH-PAYLOAD-PROTOCOL.md`](MESH-PAYLOAD-PROTOCOL.md) (wire RLP),
> [`BOOTSTRAP.md`](BOOTSTRAP.md) (boot recovery).
> Question investigated: *do we have a single encapsulated storage engine so callers
> never choose between FS and DB — block arrives → one write; query → one read — with
> the DB/FS split happening only inside the engine?*
> **Original 2026-11-02 result:** Phases 1–4b of the StorageEngine plan implemented —
> the write/recovery protocol lives in `StorageEngine`, and the stage-pipeline + FCU
> unwind paths were removed (§3.1 rule still enforced).
> **2026-11-24 corrections:** write order is trim-then-commit; seal tasks fsync in
> the background; `save_genesis_payload` / `write_bytecode` are module free functions;
> unwind has conditional alignment/truncation; bytecode has FS seams outside
> `DatabaseProvider`. The old companion names (`FJALL.md`, `XCURR.md`,
> `doc/storage/fs-block-shard.md`) do not exist in this repo.

---

## 0. RLP / storage separation

RLP is an **ingress and encoding concern**, not a storage tier. The layers are separate:

| Layer | RLP role | Evidence |
|---|---|---|
| Wire / payload | `SmartNetNewPayloadInput::BlockRlp(Bytes)` carries a full block as RLP | `calibri-lite/smartnet-payload-types/src/lib.rs:30-42` |
| Binary mesh | RLP frames ride as opaque payload bytes under mesh opcodes | `smartnet/crates/net/mesh/src/protocol.rs:312-321`; [`MESH-PAYLOAD-PROTOCOL.md`](MESH-PAYLOAD-PROTOCOL.md) |
| RPC | `newPayload` decodes RLP before submitting to the engine | `smartnet/crates/rpc/rpc-engine-api/src/engine_extensions.rs:47-68,132-178` |
| Mesh inject | RLP decoded, then submitted through the engine API | `smartnet/crates/node/ethereum/src/mesh.rs:154-162` |
| Chain import | RLP items decoded one at a time, then written via `StorageEngine::save_blocks` | `smartnet/crates/node/ethereum/src/import.rs:483-488` |
| Validator | builds RLP for the engine submit path | `smartnet/crates/node/validator/src/run_once.rs:916-923` |
| Storage | receives decoded `BlockRecords` and performs the DB/FS split | `smartnet/crates/storage/provider/src/storage_engine.rs:192` |

All ingress paths converge on `StorageEngine::save_blocks` — the only production
point that decides DB vs FS. Import calls it directly, bypassing the persistence
channel; RPC/mesh/validator reach it through the engine persistence service.

The **file tier stores JSON records, not RLP blocks**:
`smartnet/crates/storage/file-tier/src/records.rs:7-14` says so explicitly,
`writer.rs:101-137` appends JSON lines, and `smartnet/crates/fs-util/src/lib.rs:435-439`
serializes with `serde_json`. The DB stores RLP-encoded trie nodes
(`smartnet/crates/storage/trie/common/src/trie_node_v2.rs:119-137`) but no payload
bytes (`provider.rs:1140-1142`). The precise claim is therefore **no raw RLP blocks
in the file tier**, not "no RLP bytes anywhere on disk".

Reads are routed by private `DatabaseProvider::file_tier_base` helpers
(`provider.rs:199,1148-1437`); RPC/mesh/stages/validator contain zero `file_tier`
references in production code. Bytecode is the documented exception:
`write_bytecodes` branches tier-on → FS vs tier-off → `Bytecodes` table
(`provider.rs:600-628`), and some state providers read FS bytecode directly
(`providers/state/mod.rs:17-22`, `latest.rs:273`, `historical.rs:582`). Token
metadata/holders are also written to FS in the provider (`provider.rs:527-535`).

## Verdict

**Yes — one engine owns both stores; block/receipt/header callers never see the split.**

- **Reads:** callers query storage-api traits (`BlockReader`, `ReceiptProvider`,
  `HeaderProvider`, …); the split lives inside `DatabaseProvider`'s private
  `file_tier_*` routing helpers. Zero `file_tier` references in RPC / validator /
  mesh / stages. Bytecode is the exception (see caveat and §0).
- **Writes:** `StorageEngine::save_blocks` owns the whole protocol: extract records
  → FS append (page cache) → DB index/state + finalized/safe + **changeset trim** →
  one `provider_rw.commit()` / Fjall `SyncData`. The persistence service delegates;
  it adds service concerns (channel loop, metrics, pruner) and only **re-queues**
  deferred finalized/safe markers that the engine computed.
- **Genesis:** `save_genesis_payload` and `write_bytecode` are **module free functions**
  in `storage_engine.rs`, reached through the engine write path — not `StorageEngine`
  methods.
- **Boot/shutdown:** `StorageEngine::recover_on_boot` / `StorageEngine::shutdown_clean`
  — `launch/engine.rs` delegates.
- **Unwind:** `StorageEngine::restore_unwind(2)` is the **only** unwind (§3.1). It
  hard-wires the −2 FS line cut, then conditionally aligns the file tier and
  truncates the DB to `min(file_tip, head)`.

**Caveat (small, internal):** `DatabaseProvider::write_bytecodes` still contains the
tier branch (tier on → FS, tier off → `Bytecodes` table). It is reached *through*
the engine's write path (`save_block_state_and_index` / genesis helper) — no external
caller chooses a store for bytecode — but the branch itself sits in the provider, not
in `StorageEngine`. Bytecode reads also bypass `DatabaseProvider` in a few state
provider paths, and token metadata/holders are written to FS from the provider. These
are known internal seams, not caller-facing block/receipt/header leaks.

---

## 1. What exists today

```
               CALLERS (never see the split — reads)
    RPC · validator · mesh · stages
               │  storage-api traits (BlockReader, ReceiptProvider, HeaderProvider, …)
               ▼
    ┌─────────────────────────────────────────────────────┐
    │ DatabaseProvider          ← THE read seam            │
    │   file_tier_base: Option<&'static Path>              │
    │   file_tier_bundle / verified_file_tier_bundle /     │
    │   file_tier_bundle_in_chain / file_tier_bundles_in_range │
    │   (all private routing helpers)                     │
    └──────────────┬──────────────────────┬───────────────┘
                   │ payload              │ state / index / control
                   ▼                      ▼
          smartnet_file_tier         smartnet-db (Fjall)
          (blocks/{S}/{Y:02})        (<datadir>/fjall)

               WRITERS (the split lives inside StorageEngine)
    PersistenceService ──delegates──► StorageEngine::save_blocks
       1. extract_block(...)                 ← engine (FS record format)
       2. append_file_tier_and_seal(...)     ← engine (FS write, page cache)
       3. save_block_state_and_index(...)    ← engine (DB write, trim included)
       4. commit()                           ← engine (DB SyncData durability point)
    init/genesis.rs ──► save_genesis_payload + write_bytecode (module free functions)
    launch/engine.rs ──► StorageEngine::recover_on_boot / shutdown_clean
   (sealed shards are fsynced by background seal tasks, not by save_blocks)
```

### Read side (encapsulated ✅)

| Layer | Knows about the split? |
|---|---|
| RPC / validator / mesh / stages | **No** — 0 `file_tier` references; only storage-api traits |
| storage-api traits (`BlockReader`, `ReceiptProvider`, …) | **No** — payload-agnostic signatures |
| `DatabaseProvider` | **Yes** — `file_tier_base` field + private routing helpers; every payload read goes through them |
| `file-tier` / `db` crates | Each owns only its own store |

The routing rules live entirely inside `DatabaseProvider`:
- `in_file_tier(n)` — height ≤ head && tier enabled
- `file_tier_bundle(n)` — point payload read (tier-gated + canonical-hash verified)
- `file_tier_bundle_in_chain(n)` — same, for heights already known canonical
- `file_tier_bundles_in_range(a..=b)` — single-pass range read + verify
- Genesis / tier-off special cases (`file_tier_base.is_none() && n == 0`)

**Callers query "the storage"; the provider decides which store answers.**

### Write side (encapsulated ✅ — since Phase 1)

The block-write protocol is owned by **`StorageEngine`**
(`storage/provider/src/storage_engine.rs`):

| Step | Who | Store |
|---|---|---|
| Extract `BlockRecords` (stamp `ft` = first tx num) | `StorageEngine::save_blocks` | — (in memory) |
| Append `.jsonl` + seal on boundary | `StorageEngine::save_blocks` → `append_file_tier_and_seal` | **FS** (page cache) |
| Write index + state + trie + checkpoints, compute deferred finalized/safe, trim changeset window | `StorageEngine::save_blocks` → `save_block_state_and_index` | **DB** |
| One `provider_rw.commit()` / Fjall `SyncData` — the DB durability point | `StorageEngine::save_blocks` → `commit` | **DB** |
| Re-queue deferred finalized/safe markers; pruner | `PersistenceService::on_save_blocks` (service concerns only) | **DB** |

Background seal tasks fsync sealed shards asynchronously
(`smartnet/crates/storage/file-tier/src/seal.rs`); the "single fsync" claim applies
only to the DB commit path, not to all I/O in `save_blocks`.

Genesis (`init/genesis.rs`) calls the module free functions `save_genesis_payload` +
`write_bytecode` — no duplicated both-stores knowledge.

### Recovery / shutdown (encapsulated ✅ — since Phase 3)

| Site | Delegates to |
|---|---|
| `launch/engine.rs::bootstrap_seq_20_recovery` | `StorageEngine::recover_on_boot` (reconcile seals → clean-flag check; skip unwind when clean, cold path only → `restore_unwind(2)`) |
| `launch/engine.rs::final_protocol` | waits for all other graceful tasks → `StorageEngine::shutdown_clean` (flush FS → `SHUTDOWN_CLEAN` + timestamp last) |
| `persistence.rs::recover_file_tier` | thin wrapper → `StorageEngine::restore_unwind` |
| `persistence.rs::take_shutdown_clean` | thin wrapper → `StorageEngine::take_shutdown_clean` |

---

## 2. Inventory: where the split leaks

| # | Location | Status |
|---|---|---|
| W1 | `engine/persistence.rs::on_save_blocks` | **RESOLVED** (Phase 1) — delegates to `StorageEngine::save_blocks`; service keeps only channel/metrics/markers/pruner |
| W2 | `init/genesis.rs` | **RESOLVED** (Phase 2) — uses `save_genesis_payload` + `write_bytecode` |
| W3 | `DatabaseProvider::save_block_state_and_index` | Inherent method on the provider, called *by* the engine — internal, not a caller leak |
| W4 | `DatabaseProvider::write_bytecodes` tier branch | **Internal seam** — reached only via the engine's write path; no external caller chooses. Kept (see Verdict caveat) |
| W5 | `launch/engine.rs` boot/shutdown | **RESOLVED** (Phase 3) — delegates to `recover_on_boot` / `shutdown_clean` |
| R1 | `DatabaseProvider` (`file_tier_base` refs) | Read split **is** encapsulated here — the seam, not a leak |

Read callers (RPC/validator/mesh/stages): **no leaks** — source-audited 2026-11-24: 0 `file_tier` references in production code.

---

## 3. What "one engine" means here (implemented)

The desired contract, restated against the current code — **now implemented as a
concrete struct** (design choice: not a trait, KISS):

```
StorageEngine<N> (storage/provider/src/storage_engine.rs)   // exists today
    fn save_blocks(&mut self, blocks, finalized, safe)  // FS append + DB commit (W1 inside)
    // W2 inside: module free functions, NOT methods
    fn save_genesis_payload(...) / fn write_bytecode(...)
    fn recover_on_boot(&self, base) -> Result<u64>      // W5 inside (cold → restore_unwind(2))
    fn shutdown_clean(&self, base) -> Result<()>        // W5 inside
    fn restore_unwind(&self, base) -> Result<u64>       // n == 2 ONLY — see §3.1
    (+ append_file_tier_and_seal, join_seal_tasks, take_shutdown_clean)
```

- **Read half:** unchanged — provider routing behind storage-api traits.
- **Write half:** absorbed — no caller outside the engine sequences FS-then-DB or
  calls `smartnet_file_tier::append_*` in production (test code excepted).
- **Split stays inside:** the strict disjoint-store rule (formerly called XCURR;
  there is no `XCURR.md` file in this repo) is unchanged — the engine is the only
  production type allowed to touch both stores.

### 3.1 `restore_unwind(2)` — the only unwind (operator rule)

**Rule: `restore_unwind(2)` is the only unwind that exists. Nothing else may unwind —
SmartNet does not support unwind in any other form. No other rewind/unwind path may
exist in the code.** (`n` is hard-wired to 2; there is no general `restore_unwind(n)`.)

**Purpose — recovery only.** After a **cold restart** (no `SHUTDOWN_CLEAN`), the tail of
the store may be torn: an `.jsonl` never closed, a partial last line, an open writer that
died mid-append. The last block may be incomplete or unreadable, so **no queries are
served until `restore_unwind(2)` has run**. Simple rule: **cold shutdown detected → −2 blocks
back** — the FS line cut is unconditional, with no corruption checks and no
"last block looks fine so keep it". **Precise nuance (2026-11-24):** only that −2 cut
is unconditional; the rest of the path is conditional — it anchors on
`bx = file_tip.min(head)`, calls file-tier alignment only when needed, and truncates
the DB only when the DB head is above `bx`.

**Surgical protocol (`StorageEngine::restore_unwind`):**

1. **FS:** remove the last **2 whole lines** from the active `b.jsonl`
   (`remove_last_lines`, never drops genesis) — the torn last line + the one
   previous (the single-block rule means the previous line is the last complete block).
   Align `txs`/`logs`/`4626` to the resulting `BX`. Seal-boundary
   edge cases: fully sealed shard → nothing removed, `BX` = sealed last block; emptying
   the active shard → fall back to previous sealed shard's last block.
2. **Read `BX`** = last complete block that remains.
3. **DB:** `remove_block_and_execution_above(BX)` — truncate state, index, changesets
   above `BX` (guarded: if DB head ≤ `BX`, nothing to truncate). Commit.
4. Re-sync resumes at `BX + 1`.

Call chain: `launch/engine.rs` (cold detected) → `StorageEngine::recover_on_boot` →
`StorageEngine::restore_unwind` → `smartnet_file_tier::remove_last_lines` +
`DatabaseProvider::remove_block_and_execution_above`.

**Rule check — what was removed (Phases 4a + 4b):**

| Path (removed) | When | What it did |
|---|---|---|
| Stage-pipeline unwind (`ControlFlow::Unwind`, `Pipeline::unwind`, every stage's `fn unwind`, `UnwindInput`/`UnwindOutput`, `post_unwind_commit`, `take_state_above`) | Phase 4b | On Validation/Execution block errors the pipeline rewound stages to a computed depth. **Now:** recoverable stage errors retry; any other error **drops the pipeline run** (fails the run — never unwinds). |
| FCU canonical unwind (`handle_canonical_chain_unwind`, `remove_blocks`, `PersistenceAction::RemoveBlocksAbove`, `allow_unwind_canonical_header`) | Phase 4a | Disk reorg to FCU head. **Now:** `find_disk_reorg` mismatch is **fatal** (`AdvancePersistenceError::DiskReorg`) — no unwind. |
| `take_state_above` (state-writer trait + impl) | Phase 4b | Reconstructed `ExecutionOutcome` above a point. Only caller was stage unwind → removed. |

**What remains (sanctioned `restore_unwind(2)` primitives only):**

| Path | Role |
|---|---|
| `StorageEngine::restore_unwind` | The only unwind entry (cold-shutdown recovery) |
| `DatabaseProvider::remove_block_and_execution_above` | Step 3 DB truncation primitive (also used by one provider test) |
| `remove_blocks_above` / `remove_bodies_above` / `unwind_trie_state_from` / `remove_state_above` | Internals of the above; called only through it |
| `recover_file_tier` (persistence.rs) | Thin wrapper → `restore_unwind` |

All other rewind/unwind paths that existed at audit time are **gone**. The rule is
enforced: grep for stage `fn unwind`, `ControlFlow::Unwind`, `UnwindInput`,
`take_state_above`, `handle_canonical_chain_unwind` → zero hits in production code.

---

## 4. Summary table

| Question | Answer |
|---|---|
| One class/engine for both stores? | **Yes** — `StorageEngine` (concrete struct, `storage_engine.rs`) |
| Do query callers choose a store? | **No** — they use provider traits; split is inside `DatabaseProvider` ✅ |
| Do write callers choose a store? | **No** — `PersistenceService` delegates to `StorageEngine::save_blocks` ✅ |
| Where does the split live (reads)? | `DatabaseProvider` private helpers (`file_tier_bundle*`) |
| Where does the split live (writes)? | `StorageEngine::save_blocks` / `save_genesis_payload` |
| Recovery/shutdown split? | Inside `StorageEngine::recover_on_boot` / `shutdown_clean` ✅ |
| Smallest remaining seam? | `write_bytecodes` tier branch inside the provider (internal only) |
| Unwind paths? | **One:** `restore_unwind(2)` — everything else removed (§3.1) |
| Is storage RLP? | **No raw RLP blocks in the file tier** — JSON records there; DB has RLP-encoded trie nodes (§0) |

---

## 5. Operator-reported import footprint (2026-11-24)

**Operator-reported, not independently re-run here.** Recorded as reported:

- Input: a **5.7 GB** RLP export file (operator wrote "RRLP") with **almost 10M
  blocks** — a whole-network snapshot.
- Result after import: the entire `data/` directory is **2.4 GB**, about **2× smaller**
  than the RLP import file.
- Operator's description: **hot data, ready to be queried by explorer**.

Read against §0: the RLP file is an archive/wire-format snapshot; the imported `data/`
is the live split store (JSON file-tier records + Fjall index/state) that serves normal
reads directly. No compression mechanism or size ratio is claimed as an invariant —
these are one operator-reported run, not a benchmark or a dated test artifact.

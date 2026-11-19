# HEARTBEAT.md — one timer per node: mesh · bridge · validator

> **STATUS: IMPLEMENTED (2026-11-14).** The single heartbeat runs in every node
> (see `smartnet/crates/node/full/src/node.rs`, task `node-heartbeat`).
> Everything a node does on a schedule happens here — there is no second timer,
> no private bridge timer, no private mesh timer.

---

## 1. The rule

One node = **one loop, one interval**:

```text
loop {
    mesh();                 // links ping on this tick
    if bridge_is_armed { bridge_beat() }
    if validator_armed { validator_beat() }
    sleep(NODE_HB)          // the one interval
}
```

The operator's own words: *"1 HB per node, multi-purposed"*. The consequence is
simple to reason about: a bridge beat, a seal beat and a mesh keepalive always
happen **in the same order, on the same clock**, so no service can run "between"
two ticks of another one.

## 2. Cadence — `NODE_HEARTBEAT`

| situation | interval |
|---|---|
| `NODE_HB` key set in config | that many milliseconds |
| not set, node is a **bridge** node | **2000 ms** (scan + confirmations need a calm pace) |
| not set, any other node | **500 ms** (validators must stay inside the 1 s SELECT chair window) |

Single-purpose configs make bridge and validator mutually exclusive (see §4), so a
node never has to pick between two numbers.

If a tick takes longer than the interval (a slow RPC), the next tick waits for the
straggler instead of bursting (`MissedTickBehavior::Delay`) — a loaded node slows
down, it never queues up work behind itself.

## 3. What rides one tick

| hook | who | what it does on its tick |
|---|---|---|
| **mesh** | every node | bumps a sequence counter; the mesh service watches that counter and pings its links — links are kept alive by the node's clock, not by a timer of their own |
| **bridge** | `BRIDGE = true` nodes | one full bridge beat (§5 below) |
| **validator** | nodes with a validator section | one seal beat: read the pool, select, simulate, seal |

A failing beat is **logged and retried on the next tick** — the heartbeat task is
"critical" for visibility, but a bad beat never takes the node down and never stops
the other hooks.

## 4. Boot: which hooks are allowed to arm (checked before the loop starts)

These are one-time checks, run before the loop begins:

1. **`SEAL` and `BRIDGE` in one config is refused** — a single node cannot be both
   the chair and the bridge, so boot stops with an explanation.
2. **`BRIDGE = true` with an unreadable `bridges.json` fails the boot** — a bridge
   that cannot read its allowlist would scan nothing, so it must not start
   (fail-closed).
3. **Every pair address must have its keystore file present** (`bridge-keystore/`,
   `PASSWORD_FILE` set). File presence only — nothing is opened or decrypted at
   boot. If one is missing the bridge hook simply **does not arm** this run, with a
   loud warning naming the missing addresses; the node itself keeps running
   (§3.1 of `BRIDGE-FIXED.md`).
4. The **node key** is loaded once and becomes the signer for signaling (the 0.91 /
   0.92 request) and for the deposit delivery. Pair keys are *not* loaded here —
   they are decrypted only at the moment a burn or a release needs them.

## 5. One bridge beat, step by step

The bridge keeps everything it knows in RAM (a record store) backed by one
`…-bridge-buffer.jsonl` file per network. One beat is:

1. **Drop what is finished and older than 48 hours.** The drop needs **both**
   conditions: **48 h old *and* status `4`**. A record that is still open
   (`0`…`3) is never dropped by age — it stays however long it takes to resolve,
   even beyond 48 h, and is worked again below until it reaches `4`. Anything
   finished and older is already reflected on chain; the record store only ever
   works with the current window.
2. **For each configured network (own chain included):**
   1. **Read the head** over that network's `rpcUrls` (ordered failover). One call
      per network per beat — this is the fixed cost of a quiet beat.
   2. **Compute the mature frontier**: `head − minConfBlocks`. Blocks above that
      line are still moving (reorgs) and are never scanned.
   3. **If there is nothing new, stop here.** The chain has not advanced past what
      was already seen — no further calls for that network (this is the normal
      state between blocks).
   4. **Fetch every not-yet-seen block** from `last_scanned + 1` up to the
      frontier, in one pass: ERC-20 `Transfer` logs for the watched token
      contracts, plus full blocks for plain coin transactions. Every transaction
      that touches a **watched address** becomes a candidate ("hit").
   5. **Read the network's current last block again** (right before these hits
      are considered — never a number remembered from earlier in the beat), then
      **run each hit through the accept checks — block range first, hash later**
      (the buffer only remembers 48 h, so a replayed old transfer must be stopped
      by the range, not by a memory that forgot it):
      1. **`tx_age_check` — the admission window, both ways** (an old tx is never
         admitted, so it can never be picked). It answers a code and a tx is picked
         **only on `1`**: `0` = too young (shallower than `minConfBlocks`), `1` =
         normal, admission granted, `2` = too old. Both bounds come from
         `bridges.json` **by `chainId`**; the two ways of being old are the hit
         block's own timestamp (**younger than 24 h** — if the node sends none this
         way does not run) and the distance from that fresh last block (no more than
         **`maxConfBlocks`**) — both run when both are available, either one alone is
         normally enough. Timestamp too old → ignored, no record; too deep → refused
         as expired with a terminal stage-4 record (written only for an identity the
         store has never seen — a window verdict never rewrites a record that exists);
      2. the rest of the depth checks against that fresh last block: at or below the
         head (a hit ahead of the head is refused as stale);
      3. **dedup** — an already-known identity is rejected outright;
      4. the pair's direction flag, instrument/value checks, the minimum;
      5. finally a receipt check on the source chain.
      A hit that fits and passes all of them becomes a **record at stage 0** —
      written to RAM and to disk *before* anything else happens, so no accepted
      transfer can ever be lost to a later failure. A failed last-block read is a
      transient failure: nothing is picked up, the beat tries again next time.
3. **Process stage — this runs even when the scan found nothing new.** Every
   record that is not finished is worked again this beat:
   - **on our own line**: post the 0.91/0.92 request for records that do not have
     one yet (stage 0 → 1), build the deposit delivery once the record has
     reached the point where the money is in our hands, and — while a withdrawal
     waits at stage 1 — look for the chair's 0.01 authorization in the block of
     our own request and burn `amountIn` with the pair key (stage 1 → 2);
   - **on a foreign line**: build the release for a withdrawal that has passed its
     gates (stage 2 → 3), signed with that pair's key — never before the burn
     receipt, and never a part of it: the pair must cover the full transfer or
     the record waits at stage 2.
4. **One chain's failure never stops the others.** A dead endpoint only fails its
   own line; every other network keeps scanning, and the failed one retries next
   beat.

### Why nothing can be lost

A record is written at detection, before any check. Every later step (RPC error,
revert, missing answer, a key that will not decrypt) leaves the record where it
was, writes the reason into the record's `lastErr` (and the same reason reaches
the log through that beat's failure line), and is retried **next beat** — that is
what "all open
transfers are worked every heartbeat until they are finished" means in practice.
Nothing self-heals, nothing is refunded, nothing is closed early.

### Restart / lost memory

The RAM store is the working set; the per-network files are a cache. If RAM is
lost, the node re-reads the recent chain on a cold boot — and only hits younger
than 24 h are still eligible, so a transfer that turns up late is not picked up —
while the dedup gate prevents any transfer from being processed twice. Open
records are read back from the files and keep being worked, however old they
are. The chain is the authority; the files are just a head start.

## 6. What a beat deliberately does NOT do

- No second timer, no "bridge-only" or "mesh-only" interval.
- No work on immature blocks (above `head − minConfBlocks`).
- No scanning of a network whose head has not moved (fast-stop).
- No key material at boot beyond the node key — pair keys are decrypted per use
  and forgotten.
- No consensus, no state mutation: the bridge only ever sends ordinary
  transactions, which the validator executes like anyone else's.

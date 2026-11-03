# Why Common Attacks Don't Apply to SmartNet

> **STATUS: HISTORICAL (re-statused 2026-11-01).** The mechanisms named below (BTVA,
> float genesis, bridge tx, chair selection from `(prev_block_hash, timestamp)`) were
> never implemented — zero code references remain. This document is kept only as
> historical reasoning. **The current rulebook is [`MARS.md`](MARS.md).** Do not treat
> this file as an active design reference until it is rewritten against the current
> rulebook.

## Weak Subjectivity Checkpoint (WSC) Attack

**Why it doesn't apply:** WSC attacks exploit PoS chains that select the canonical chain by **counting historical votes** from multiple forks . An attacker with old validator keys builds an alternative fork from a past point, and new nodes can't tell it's invalid because they can't see the historical votes .

**SmartNet's immunity:**
- **No vote counting.** BTVA selects the chair deterministically from `(prev_block_hash, timestamp)`. There is no "which chain has more votes" question.
- **Float genesis + bridge tx.** The signed bridge transaction serves the same purpose as a WSC — it defines a recent signed checkpoint as truth. But it's **explicit and on-chain**, not a social assumption.
- **Retention window.** History beyond ~14 days is discarded. A long-range fork from genesis 0 is impossible — the blocks are gone.

## Other Common Attacks That Don't Apply

| Attack | Why It Fails on SmartNet |
|---|---|
| **Long-Range Attack** | Requires historical vote counting and retained history from genesis. SmartNet has neither — the chain is linear, and the retention window discards old blocks . |
| **Nothing-at-Stake** | Requires validators to vote on multiple competing forks without cost. SmartNet has **no forks** under normal operation, and BTVA assigns a single chair per block . |
| **Selfish Mining / Block Withholding** | Requires a miner to secretly build a competing chain and release it later. SmartNet's single-chair model and deterministic assignment prevent competing chains . |
| **Sybil Attack** | Requires forging many identities to subvert the network. SmartNet's mesh is **whitelisted** (admissionID-gated), so the participant set is known and closed . |
| **Eclipse Attack** | Requires monopolizing a victim's peer connections. SmartNet's mesh is a **closed list** — nodes only talk to configured peers, not discovered ones . |

## Summary

SmartNet's design eliminates the structural preconditions for WSC, long-range, nothing-at-stake, selfish-mining, Sybil, and eclipse attacks. The common thread: **no vote counting, no forks, no open peer discovery, and no retained history from genesis.** The bridge transaction and float genesis are SmartNet's answer to the problem WSC solves — but implemented explicitly and on-chain rather than through social trust assumptions.

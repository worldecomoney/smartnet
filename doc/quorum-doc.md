# `SELECT` — Full Documentation (v5)

````markdown
# `SELECT` — Deterministic Chair & Quorum Selection

**A position-free, purpose-tagged, entropy-keyed selection primitive.**

---

## 1. Overview

`SELECT` picks a **chair** and a **quorum** from a set of addresses, deterministically, using two-source entropy:

```
entropy = (block_hash, unix_timestamp)
```

The same `(purpose, block_hash, unix_timestamp, unique_address_set)` **always** yields the same output — the same chair at index `0`, the same quorum at `1..`, in the same order — regardless of:

- the input array's order,
- duplicate entries in the input,
- the machine, OS, Rust version, or programming language.

This is the property that makes the primitive **auditable**, **cross-verifiable on-chain**, and **immune to the strategic behaviors that afflict regular PoS**.

`SELECT` is a **selection primitive**, not a consensus protocol. It replaces **leader election** in a consensus stack and composes cleanly with any BFT, longest-chain, or DAG finality layer.

**The case for `SELECT` is not "here are some extra features." It is: three structural problems in today's PoS are solvable with a single change to the selection primitive — and solving those three closes a dozen adjacent pain points for free.** See §5.

---

## 1.1 The pipeline

`SELECT` is one step in a four-step pipeline. Each step is a clean, stateless, verifiable component.

```
┌────────────────────────────────────────────────────────────────────┐
│  STEP 1 — PoS deposit layer                                        │
│  Sybil resistance lives here.                                      │
│  Produces: a fixed, stake-gated address set for this epoch.        │
└──────────────────────────────┬─────────────────────────────────────┘
                               │ fixed set
                               ▼
┌────────────────────────────────────────────────────────────────────┐
│  STEP 2 — SELECT                                                   │
│  input:  (purpose, block_hash, unix_ts, set)                       │
│  output: [chair, q1, q2, …, qk]     ← arr[0] is chair              │
│  Pure function. No state. No votes.                                │
└──────────────────────────────┬─────────────────────────────────────┘
                               │ chair + quorum
                               ▼
┌────────────────────────────────────────────────────────────────────┐
│  STEP 3 — Signing and seal                                         │
│  • Chair proposes block to quorum members.                         │
│  • Each quorum member checks + signs independently.                │
│  • Chair collects Yep signatures.                                  │
│  • Chair checks: count >= threshold ?                              │
│  • If yes: seal block (chair sig + quorum sigs) and broadcast.     │
│  • If no: block dies. SELECT rotates on next second.               │
│                                                                    │
│  Single-validator regime (N ≤ 16):                                │
│  • No quorum exists → no BFT count → chair signs and seals alone.  │
│                                                                    │
│  BFT here is a SIGNATURE COUNT, not a protocol round.              │
└──────────────────────────────┬─────────────────────────────────────┘
                               │ sealed block
                               ▼
┌────────────────────────────────────────────────────────────────────┐
│  STEP 4 — Accept / finality                                        │
│  Any receiving node re-verifies (statelessly):                     │
│  • Recompute chair + quorum via SELECT.                            │
│  • Verify chair signature.                                         │
│  • Verify quorum signatures.                                       │
│  • Only signatures from the SELECTED quorum count.                 │
│  • Recount threshold.                                              │
│  • Accept or reject.                                               │
└────────────────────────────────────────────────────────────────────┘
```

**Every step is stateless or near-stateless. Every step is verifiable from public data. No step requires a multi-round consensus protocol except the finality rule the deployment chooses.**

---

## 2. The API

```rust
pub enum Purpose { Chair, Bridge, Audit, Other(&'static str) }

pub struct Entropy { pub block_hash: [u8; 32], pub unix_ts: u64 }

pub fn chair_entropy(block_hash: [u8; 32], current_timestamp: u64) -> Entropy;
pub fn current_slot() -> u64;

pub fn committee_size(n: usize) -> usize;

pub fn select_chair_and_quorum(
    purpose: Purpose,
    entropy: &Entropy,
    addrs: &[String],
) -> Vec<String>;

pub fn current_chair(
    purpose: Purpose,
    block_hash: [u8; 32],
    addrs: &[String],
) -> Option<String>;
```

### 2.1 Return shape

| Unique addresses `N` | Returned array | Length |
|---|---|---|
| `N = 0` | `[]` | `0` |
| `1 ≤ N ≤ 16` | `[chair]` | `1` |
| `N > 16` | `[chair, q₁, q₂, …, qₖ]` | `1 + round(3N / 16)` |

**Invariants — always true:**

- `arr[0]` is the **chair**.
- `arr[1..]` is the **quorum** (empty when `N ≤ 16`).
- The array is **score-ordered**: `arr[0]` has the smallest score, `arr[1]` the next, etc.
- All elements are distinct.
- All elements are members of the input set (after deduplication).

### 2.2 Sizing examples

| `N` | Returned length | Chair + Quorum |
|---|---|---|
| `16` | `1` | chair only |
| `17` | `4` | chair + 3 |
| `32` | `7` | chair + 6 |
| `64` | `13` | chair + 12 |
| `100` | `20` | chair + 19 |
| `1000` | `189` | chair + 188 |

---

## 3. How it works

The algorithm is a **min-hash selection** with three properties layered on top:

1. **Purpose-tagged entropy** — different use cases produce independent results.
2. **Two-source entropy** — `block_hash` binds to the chain, `unix_timestamp` provides liveness.
3. **One-round collision recovery** — astronomically rare score ties are broken deterministically.

### 3.1 Step-by-step

```
Input: purpose P, entropy E = (block_hash, unix_ts), address array A

Step 1 — Canonicalize A
    set = sort_unstable(dedup(A))
    → Removes order-dependence and duplicate-dependence.

Step 2 — Score every address in `set`
    For each addr ∈ set:
        score(addr) = Keccak256(
              "select/v4"
            ‖ len(P)     ‖ P
            ‖ block_hash
            ‖ unix_ts
            ‖ len(addr)  ‖ addr
        )

    Each variable-width field is length-prefixed
    → No concatenation ambiguity, no field confusion.

Step 3 — Sort by (score, addr)
    → Total order. Deterministic. Tie-safety guaranteed.

Step 4 — Handle rare tie at the cutoff
    If score[ k-1 ] == score[ k ]:
        Rehash the tied block with domain "select/v4-rehash"
        Re-sort by rehash
        If rehash still ties (≈ 2⁻²⁵⁶): lexicographic fallback

Step 5 — Return
    k = committee_size(|set|)
    Return first k addresses in score order
    → [chair, q1, ..., q_{k-1}]
```

### 3.2 Why length-prefix every variable field

Without length-prefix, an attacker could force a concatenation collision:

```
purpose = "ab",  addr = "c"    →  bytes = "abc"
purpose = "a",   addr = "bc"   →  bytes = "abc"    ← SAME
```

`len(field) ‖ field` makes the two encodings distinct. Fixed-width fields (`block_hash`, `unix_ts`) need no prefix — their length is implied by the type.

### 3.3 Why one Keccak round is enough

Two independent 256-bit hashes colliding simultaneously has probability `2⁻²⁵⁶` per pair. For any realistic `N` (even `N = 10⁹`), the probability that two addresses collide on **both** the primary and the rehash score is below `10⁻⁶⁰`. One rehash round is provably sufficient.

### 3.4 Liveness — the timestamp is *observed*, not *chosen*

```
chair(T) = select_chair_and_quorum(
    Purpose::Chair,
    Entropy { block_hash, unix_ts: T },
    addrs
)[0]
```

where `T = current_slot()` — read from the system clock.

**Nobody sets `T` manually.** The clock advances on its own. Every second, `T` moves; every second, the chair may rotate. If the current chair is unresponsive, the protocol does nothing — **time advances, and the next chair takes over.**

There is no "operator bumps timestamp" recovery path, so there is no race between nodes that disagree about when to bump. Wall-clock seconds are agreed to within sub-second tolerance with NTP, and `SLOT_SECONDS ≥ 1` gives every node the same `T` with overwhelming probability. See **§6.2** for the exact clock-tolerance rule.

---

## 4. Guarantees

### 4.1 Determinism

Given equal `(purpose, block_hash, unix_ts, unique address set)`:

- The output `Vec<String>` is **byte-identical**.
- The chair at `arr[0]` is identical.
- The quorum at `arr[1..]` is identical, in the same order.

This holds across machines, OSs, Rust versions, and languages (given the same Keccak-256 implementation).

### 4.2 Position-freedom

```
select_chair_and_quorum(P, E, [a, b, c])  ==
select_chair_and_quorum(P, E, [c, b, a])  ==
select_chair_and_quorum(P, E, [b, c, a, b, a])
```

All three return the same result. Input order and duplicate content are erased before hashing.

### 4.3 Purpose independence

```
select_chair_and_quorum(Purpose::Chair,  E, addrs)  ≠
select_chair_and_quorum(Purpose::Bridge, E, addrs)
```

with overwhelming probability. The purpose tag is length-prefixed into the hash, so `"bridg"` and `"bridge"` and `"chair"` all produce independent selection streams.

### 4.4 Avalanche

A single bit change in **either** `block_hash` or `unix_ts` produces a new selection with probability `1 − 1/N`:

- `N = 10` → ~90% chance of a different chair per bit flip.
- `N = 100` → ~99% chance.

### 4.5 Uniformity

Over many values of `(block_hash, unix_ts)`, every address in the set wins the chair with probability exactly `1/N`. There is no bias toward any address based on its string, its hash, or its position in the set.

### 4.6 Liveness

If the current chair is unresponsive:

- `current_slot()` advances every second.
- On the next second, `select_chair_and_quorum` returns a different chair with probability `1 − 1/N`.
- Within `k` seconds, the probability of at least one alternative chair is `1 − (1/N)^k`.

For `N = 10`, `k = 20` seconds → `1 − 10⁻²⁰`. Liveness is effectively guaranteed by the passage of time alone.

---

## 5. Three problems PoS has — and how `SELECT` solves them

Regular Proof-of-Stake works. But it ships with three structural problems that every deployment has learned to live with — and solving them closes a dozen adjacent issues for free.

### The three problems

| # | Problem | How PoS lives with it today | How `SELECT` solves it |
|---|---|---|---|
| 1 | **Multiple valid proposers per slot** | Fork races, equivocation, "I didn't see the block" disputes | **Exactly one chair per second.** No mess. |
| 2 | **Ganging / coordinated attack surface** | RANDAO grinding, predictable schedules, stake snowball | **Uniform over set + last-block entropy + wall-clock.** Ganging is structurally unprofitable. |
| 3 | **Backwards validation** | Re-verifying a historic selection requires the full validator registry at that height | **Pure function of `(block_hash, unix_ts, set)`.** Re-verify any past chair from block history alone. |

The rest of §5 walks through each, then lists the adjacent issues that `SELECT` closes as a consequence.

### 5.1 Problem 1 — Always one block sender / proposer. No mess.

**What PoS does today.** In most stake-weighted PoS, a slot may have **multiple eligible proposers**:

- **Ethereum:** one proposer per slot, but **multiple attesters** whose votes decide the head. Fork races are resolved by attestation weight — i.e., by a second, parallel election.
- **Tendermint / CometBFT:** a proposer plus a committee of validators who must reach 2/3+ byzantine agreement. Leader election is one step among many; a failed leader triggers a new round, which is another election.
- **Solana:** a leader schedule with rotating producers, but **turbine-based propagation** means multiple nodes may broadcast the same slot's work simultaneously.

In each case, **more than one party can produce a candidate block for the same logical slot.** The consensus layer must then decide which one wins. That decision is:

- **Expensive** — attestation rounds, view changes, timeouts.
- **Adversarial** — an attacker who controls many attesters can force a fork, or a vote-split, or a stalled round.
- **Lossy** — honest nodes on different sides of a fork waste the slot.

**How `SELECT` changes this.**

`SELECT` selects **one chair** for each `(block_hash, unix_ts)`. The chair is the **only** party authorized to propose for that second. There is:

- No second eligible proposer.
- No attester layer to elect one.
- No view-change to trigger a new leader.
- No fork race at the proposal layer, because there is only ever one proposal.

If the chair produces a valid block → it wins the slot. If the chair fails to produce (offline, malicious, censored) → the next second's chair takes over. The transition is **stateless** (no vote, no view change) and **automatic** (the clock moves).

**What this eliminates:**

- ❌ Fork races at the proposal layer.
- ❌ Equivocation (one chair, one second — producing two blocks means one is provably invalid).
- ❌ "Did you see block X?" disputes (there is only ever one block per slot).
- ❌ Leader-election round-trips (view changes, timeouts, vote re-runs).
- ❌ Proposer-withholding attacks at the leader level (the chair is chosen by entropy, not by a round of voting).

**Net effect:** the proposal layer becomes **single-writer per slot**. The "mess" of multi-proposer PoS is gone at the selection layer. Consensus can now be a **simpler protocol** because it inherits single-writer semantics.

**Single-validator regime (N ≤ 16): cryptographic determinism.** `SELECT` returns `[chair]`. The chair has one private key. One key produces one signature over a given block. There is **no multisig, no BFT count, no aggregation**. The block is either validly signed by the chair, or it isn't. Cryptographic determinism at the proposal layer — not just "one proposer by rule," but "one proposer by math."

### 5.2 Problem 2 — Ganging attack is hardened by construction.

"Ganging" is the family of attacks where **coordinated parties** exploit some structure in the selection rule to gain more than their share of block production. It includes several known sub-attacks:

| Sub-attack | Where it hits regular PoS |
|---|---|
| **RANDAO grinding** | A proposer can withhold a block to bias the next slot's randomness |
| **Predictable schedules** | The next `k` proposers are known, enabling targeted DoS and bribery |
| **Stake snowball** | Large validators gain more slots → more rewards → more stake → repeat |
| **Cartel voting** | A coordinated set of validators can vote as a bloc to bias selection |
| **Nothing-at-stake forking** | Signing on multiple forks is free, so a gang can spawn many candidate chains |
| **Vote splitting** | Multiple proposers let a gang split honest votes across forks |

**How `SELECT` closes each one:**

1. **RANDAO grinding → Keccak preimage.** The entropy is `(block_hash, unix_ts)` — there is no RANDAO to manipulate. To bias the next chair, a party must find a `block_hash` such that `argmin` favors them. That is a `2²⁵⁶` preimage search. No party can do it.

2. **Predictable schedules → one-slot horizon.** The chair depends on `block_hash`, which is fixed only by the parent block. So the next chair is knowable **at most one block ahead** (and often only a fraction of a second ahead, once `unix_ts` is also considered). DoS and bribery windows collapse from an epoch to a slot.

3. **Stake snowball → uniform over set.** Selection is not stake-weighted. A validator with 1000× another's stake gets the **same** chair probability per address. More stake cannot buy more slots. Cartels must run more **addresses** (linear cost), not more stake (superlinear payoff).

4. **Cartel voting → no voting at the selection layer.** There is no round of votes to bias. The chair is a pure function of public data. A cartel can only control a fraction of the set, and their fraction of slots equals their fraction of addresses.

5. **Nothing-at-stake → work-rate parity.** Building a competing chain means producing **every block on that chain** at the same wall-clock rate as the honest chain. Historical stake does not accelerate a fork — only **producing valid blocks** does, one per second, from a selected chair.

6. **Vote splitting → one proposer.** With one chair per second, there is one proposal, so there is nothing to split.

**Net effect:** the surface for coordinated selection attacks is reduced from a half-dozen vectors to **one** (epoch-boundary set manipulation, §6.4), and that one has **the same cost as honest joining**.

#### 5.2.1 Cartel formation — a sub-case of ganging

Stake-weighted PoS naturally concentrates power: large validators become larger because they have more selection slots, which yields more rewards, which yields more stake. Over time, this produces **cartels** — a small set of validators controlling >33% or >50% of selection slots.

How `SELECT` differs:

- Selection is **uniform over the set**, not proportional to stake. A validator with 1000× the stake of another has the **same** chair probability per set membership.
- Cartels cannot convert stake into selection power. They can only expand the **set** of addresses they control, which is a linear cost in addresses (each address costs the same to operate, regardless of its stake).
- Adding more addresses to the set has a **diminishing** return: at `N` addresses, each additional address increases chair probability by `1/N`, and `N` grows linearly with cost. So a cartel aiming for `p` chair probability must control `p·N` addresses at cost `p·N · c`, which scales linearly with the desired probability — **no superlinear advantage from capital**.

**Net effect:** cartel formation is **linear in cost**, not sublinear. A cartel with 2× the honest set's addresses gets 2× the chair slots — not 10× or 100×. This is a fundamental change to the incentive structure of stake concentration.

#### 5.2.2 Selection-gated signature authority

This is the property that makes the security model fundamentally different from traditional BFT.

**Traditional BFT signature acceptance:**

```
Block B is valid if:
    |{signers of B}| >= ⌈2k/3⌉
    AND all signers ∈ current validator set
```

Any validator can sign any block. Control of a validator's key is sufficient to sign for **every** block it sees. The protocol only detects misbehavior **after the fact** (slashing for equivocation).

**`SELECT` signature acceptance:**

```
Block B is valid if:
    [chair, q1, …, qk] = SELECT(purpose, B.parent_hash, B.unix_ts, set)
    AND B.chair_sig is a valid signature by chair over hash(B)
    AND every quorum signature is by some qi ∈ [q1..qk]
    AND |{valid quorum sigs}| >= threshold(k)
    AND no quorum member signed a different block for the same (purpose, parent_hash, unix_ts)
```

A signature is valid **if and only if** it was produced by an address that `SELECT` named in that block's quorum. **Control of a validator's key is necessary but not sufficient** — the address must also be selected for that specific slot.

**What this means for an attacker:**

- **Control of `f` addresses does not let you sign for arbitrary slots.** Only for slots where `SELECT` includes you in the quorum.
- **You cannot predict the quorum in advance.** The quorum depends on `parent_hash`, which is only known after the parent block is sealed — typically sub-second before your slot.
- **You cannot sign two different blocks for the same slot** without producing provably attributable equivocation (both signatures are yours, over the same `(purpose, parent_hash, unix_ts)`).
- **You cannot push "your preferred BFT outcome."** There is no BFT round — there is only a count over the specific quorum's signatures.

**Probability of controlling a specific slot's quorum:**

Given `f` controlled addresses out of `N`, `k = committee_size(N)` quorum members, and threshold `⌈2k/3⌉`:

```
P(control threshold quorum for a specific slot) =
    Σ_{j = threshold..k} [ C(f, j) · C(N−f, k−j) / C(N, k) ]
```

For realistic parameters (`N = 100`, `k = 19`, `f = 33`, threshold = 13): `P ≈ 10⁻⁶`. **One in a million, per slot.** And the attacker **cannot know** whether their addresses will be in the quorum until the parent block seals.

**The security model shifts:**

| Traditional PoS | `SELECT` |
|---|---|
| Punish misbehavior **after** it happens (slashing) | Misbehavior is **structurally rejected** at verification |
| Any validator can sign any block | Only `SELECT`'s quorum can sign for that block |
| Byzantine tolerance `f < k/3` on the whole set | Byzantine tolerance `f < k/3` **and** selection-gated on each slot |
| Long-lived attack surface (fixed committee) | One-second attack surface (entropy-chosen quorum) |

**The `SELECT` security guarantee is not weaker than traditional BFT — it is stronger.** Traditional BFT says "you need `f < k/3` Byzantine validators to attack a slot." `SELECT` says "you need `f < k/3` Byzantine validators **AND** those specific validators must be in this slot's selected quorum." The second condition is exponentially unlikely for any specific slot.

### 5.3 Problem 3 — Backwards validation is trivial.

**What PoS does today.** To verify that a block at height `H` was produced by the correct proposer, a validator today must:

- Reconstruct the validator set **at height `H`** — including deposits, withdrawals, slashing events, and registry updates **up to `H`**.
- Re-derive the RANDAO **state at `H`** — which is the cumulative XOR of every RANDAO reveal up to `H`.
- Run the stake-weighted sampling procedure **with the exact same parameters** used at `H` (which are often not stable across versions).

This means **backwards validation requires a full historical replay.** A light client cannot verify a historical block's producer without trusting some external authority — the "weak subjectivity" checkpoint.

**How `SELECT` changes this.**

A historical chair for slot `T` at height `H` is verifiable from **three pieces of data**:

1. `block_hash(H−1)` — the parent block hash at that height. **Read from the block header.**
2. `unix_ts(H)` — the timestamp of block `H`. **Read from the block header.**
3. The **address set at epoch `H`** — a snapshot, available from the epoch's deposit record.

From these three, any node — **including a light client** — can compute the chair of `H` exactly as the network did, in **O(N)** time, with no state, no RANDAO reconstruction, no registry replay, no version compatibility.

**What this enables:**

- ✅ **Light-client verification of historical proposals.** A phone with the block header can verify "this block was produced by the correct chair at its height" in milliseconds.
- ✅ **Cold-chain forensics.** An auditor can verify a chain's entire proposer history from headers alone, with no node.
- ✅ **Cross-chain bridges.** A bridge contract on chain X can verify a block from chain Y's history by re-computing the chair on-chain — no oracle, no signature scheme, just Keccak.
- ✅ **Version-stable audits.** The selection function is versioned (`v4`), so a given epoch's chairs are recomputable **forever**, with no protocol drift.

**Net effect:** backwards validation is **O(N) work on public data**, not "O(entire history) work requiring a full node." This changes the security assumption from *trust an archive node* to *trust Keccak*, which is not a trust assumption at all.

**The block carries its own proof.** Because the seal contains the chair sig and quorum sigs, **any node can verify the block's authority from the header alone** — including whether the quorum threshold was met. There is no "check the BFT outcome from the network" step. The BFT outcome is **inside the block**.

### 5.4 The three problems together

The three fixes are **not independent** — they reinforce each other:

- **One proposer per slot** removes the fork races that ganging exploits → ganging surface shrinks.
- **Uniform selection** removes the stake snowball that makes ganging profitable → the incentive to gang disappears.
- **Backwards validation** removes the "trust the archive" hole that lets a gang rewrite history → any fork is detectable by any light client.

Pick any two fixes alone and there's still a hole. Together they close the triangle:

```
        ┌───────────────────┐
        │  One proposer     │
        │  per slot         │
        └─────────┬─────────┘
                  │
       fork races │ ganging
       disappear  │ vectors
                  │ disappear
                  ▼
        ┌───────────────────┐
        │  Uniform over     │
        │  set              │
        └─────────┬─────────┘
                  │
     stake snowball│ backwards
     disappears   │ validation
                  │ becomes O(N)
                  ▼
        ┌───────────────────┐
        │  Backwards        │
        │  validation       │
        └───────────────────┘
```

### 5.5 Adjacent problems that also close

The three-problem case is already worth the change. But solving them **for free** closes a handful of well-known PoS pain points that are usually handled by ad-hoc patches:

| Adjacent problem | Regular PoS workaround | `SELECT` |
|---|---|---|
| **Long-range attacks** | Weak subjectivity checkpoints, "trust a recent state" | Fork must be produced at **the same work rate** as the honest chain — no historical stake advantage |
| **Mempool censorship by proposers** | MEV-Boost, private mempools, encrypted tx | Chair is one second long — censorship window shrinks proportionally |
| **MEV reordering** | PBS, order-flow auctions | Chair has one slot; no multi-block reordering within a single chair |
| **Validator liveness disputes** | Timeouts, view-change votes | Time advances — no vote needed; the chair simply rotates |
| **Finality gadget complexity** | Casper FFG, Tendermint commits, HotStuff | `SELECT` composes with **any** finality gadget — no gadget is *required* by the selection layer |
| **Slashing complexity** | Per-message slashing conditions, equivocation proofs | One chair, one second → equivocation is **trivially provable** (two blocks with same `(block_hash, unix_ts)`) |
| **Scheduled downtime attacks** | Randomize schedules, split epochs | Chair is unknown until the parent is sealed |
| **Vote-buying markets** | Pre-commitment schemes, VDFs | Future chairs are not knowable — no market to sell them |
| **State bloat from validator registry** | Periodic pruning, Merkle snapshots | Set is a per-epoch snapshot — no growing registry to prune |

Each one of these is **a live operational complaint** in production PoS deployments. None of them is the headline case, but **they all vanish** when the three headline problems are solved by the selection primitive.

### 5.6 The full comparison, one table

| Problem | Regular PoS | `SELECT` |
|---|---|---|
| **Multiple proposers per slot** | Yes → forks, equivocation, disputes | **No** — one chair per second |
| **Ganging / grinding** | RANDAO, schedules, snowball, cartels | **Structurally unprofitable** — preimage cost, uniform over set, one-slot horizon |
| **Backwards validation** | Full registry replay, weak subjectivity | **O(N) from headers only** — light clients can verify |
| **Signature authority** | Any validator can sign any block | **Only `SELECT`'s quorum can sign for that slot** |
| Long-range attacks | Weak subjectivity | **Work-rate parity** with honest chain |
| Equivocation | Provable, but common | **Impossible** at the selection layer |
| Leader-election round-trips | Per-slot or per-epoch | **Zero** — chair is a pure function |
| Predictable schedule | Epoch ahead | **≤ 1 slot ahead** |
| Cartel snowball | Sublinear cost → superlinear payoff | **Linear cost → linear payoff** |
| MEV reordering | PBS, auctions | **One slot per chair** |
| Slashing complexity | Complex equivocation proofs | **Trivial** — same `(block_hash, unix_ts)`, two blocks |
| Light-client historical verification | Trust an archive | **Verify from headers** |
| Node state for selection | Validator registry, RANDAO state | **Block hash + clock** |
| BFT rounds | Multiple rounds, view changes | **One count: `sigs >= threshold`** |

### 5.7 The one-paragraph case

> **`SELECT` fixes three problems that regular PoS lives with today.** First, it restores **single-writer semantics** to block proposal — exactly one chair per second, no fork races, no equivocation, no multi-proposer mess. Second, it **hardens ganging attacks** — selection is uniform over the set, entropy is fixed by the parent block and wall-clock, and grinding requires a `2²⁵⁶` preimage search, so coordinated parties gain **no structural advantage** over honest ones. **Further, signatures are selection-gated: only the quorum that `SELECT` names for a slot can sign for that slot, so corruption of `f` addresses does not grant the ability to sign for arbitrary blocks — the attacker must have those specific addresses selected for that specific slot, which is exponentially unlikely and unknowable in advance.** Third, it makes **backwards validation trivial** — any historical chair is verifiable from three public pieces of data, in O(N) time, **including by a light client**. Solving these three problems closes a dozen adjacent PoS pain points for free: long-range attacks, predictable schedules, cartel snowball, equivocation complexity, MEV reordering, and light-client historical verification. **The case is not that `SELECT` adds features — it is that `SELECT` removes entire classes of problems by changing the selection primitive.**

### 5.8 What still requires work above `SELECT`

For completeness (cross-references §6):

- **Stake-gated Sybil resistance** — provided by the deposit layer (§6.1).
- **±2 s clock synchronization** — a standard distributed-systems premise (§6.2).
- **Consensus integration** — `SELECT` replaces leader election only (§6.3).
- **Fixed per-epoch set snapshot** — ensures set stability (§6.4).
- **Sound fork choice** — inherited from the consensus layer (§6.5).

These are **integration requirements**, not weaknesses of the primitive. Every distributed protocol has them; `SELECT` makes them **explicit and minimal**.

### 5.9 The three problems in one sentence each

- **Problem 1 — One proposer per slot:** `SELECT` produces exactly one chair per second; the multi-proposer mess is gone.
- **Problem 2 — Ganging hardened:** uniform over set + parent-block entropy + wall-clock + preimage cost + **selection-gated signatures** = ganging is **structurally unprofitable and structurally contained**, not just "detected and slashed."
- **Problem 3 — Backwards validation:** any historical chair is recomputable from `(block_hash, unix_ts, epoch set)` in O(N) — **light clients can verify history without trusting an archive**.

---

## 6. Boundaries and layering

`SELECT` is a **selection primitive**, not a standalone protocol. Its guarantees are stated against a **fixed, honest input set** and a **bounded clock**. This section makes the layering explicit.

### 6.1 Sybil resistance — provided by the PoS layer above

`SELECT` picks uniformly over the **set of addresses it is given**. It does not defend against an attacker who can freely create addresses, because that is not its job.

**The correct layering:**

```
┌─────────────────────────────────────────┐
│  PoS deposit / registration layer       │  ← Sybil resistance lives here
│  (stake-gated address set)              │
└──────────────────┬──────────────────────┘
                   │  fixed address set
                   ▼
┌─────────────────────────────────────────┐
│  SELECT                                 │  ← uniform selection over the set
│  (chair + quorum for purpose P)         │
└──────────────────┬──────────────────────┘
                   │  chair + quorum
                   ▼
┌─────────────────────────────────────────┐
│  Consensus layer                        │  ← block production, fork choice
└─────────────────────────────────────────┘
```

`SELECT` selects uniformly over the **output** of the PoS layer — i.e., over the addresses that have already been admitted by stake-based Sybil resistance.

**Why this is a security *gain*, not a loss:**

- Regular PoS combines Sybil resistance **and** selection weighting in one step (stake → selection probability). This couples the two: to change your selection weight, you change your stake.
- `SELECT` **decouples** them. Sybil resistance is still `stake > 0` (one address, one deposit — fixed). Selection is uniform over the resulting set. Once you're in, you don't get more chair slots by depositing more.
- This closes §5.2.1 (cartel formation) without weakening Sybil resistance: a cartel must run `p·N` addresses at cost `p·N·c` to reach chair probability `p`. Cost is **linear in selection power**, not sublinear. **Stake can no longer snowball into selection power.**

**Deployment requirement:** the address set must be **fixed at a given block** and derived from **deposit history**, not from current balances. This is what prevents §6.4.

### 6.2 Clock synchronization — bounded, not eliminated, and easily met

Every node must agree on `floor(now_secs / SLOT_SECONDS)`. With `SLOT_SECONDS = 1` and a **2-second grace window**, a node whose clock is within ±2 s of the reference produces a valid selection on the correct slot.

**Grace-window rule:**

> During the interval `[T, T + 2)`, a node accepts chair selections derived from slot `T` **or** slot `T − 1` (if `T > 0`). It rejects slots outside that range.

This handles:

- **NTP jitter** — sub-second disagreement (typical) → both nodes agree on `T`.
- **Slow sync** — a node whose clock lags by 1–2 seconds still produces valid work on `T − 1`.
- **Server drift** — a node that is off by > 2 s is out of spec, and (correctly) fails.

**Why this is not a softness in `SELECT`:** a node with a > 2 s clock error has already lost consensus at **every** layer — block timestamps, wall-clock events, TLS certificate validity, log correlation. The clock is a **foundational assumption**, not a `SELECT`-specific risk. If a deployment cannot maintain ±2 s sync, it cannot run a distributed consensus protocol at all — `SELECT` is not the weak link.

**Two acceptable designs:**

1. **Strict:** nodes reject work outside ±2 s; misconfigured nodes are ejected. Assumes NTP discipline.
2. **Adaptive:** a node that observes its clock is out of spec enters a **sync state** — it follows the reference timestamp from a quorum of peers instead of its local clock. This is how PoW chains handle "my clock is wrong" — they treat the network time as authoritative.

**Recommended:** strict ±2 s + optional adaptive fallback to peer-quorum time. This makes clock skew a **recoverable** condition rather than a **fatal** one.

### 6.3 Consensus integration — `SELECT` is a component, not an alternative

`SELECT` determines **who proposes** a block. It does not determine **whether** a block is valid, **how** conflicts are resolved, or **when** a block is final. Those belong to the consensus layer.

**Correct framing:**

> `SELECT` is a **building block of consensus** — the component that answers "who is the chair for slot `T`?" It is designed to plug into an existing consensus protocol (BFT, longest-chain, DAG, etc.), replacing only the **leader election** subprotocol.

#### 6.3.1 BFT is a signature count, not a protocol round

In a traditional BFT protocol (Tendermint, HotStuff, PBFT), consensus is a **multi-round distributed protocol**: nodes exchange `prepare`, `precommit`, `commit` messages, maintain round state, handle view changes, and retry on timeout. It is a state machine with many transitions.

In `SELECT`, BFT is **arithmetic over signatures**:

```
BFT check = count(quorum_sigs) >= threshold
```

**Why this works:**

- `SELECT` gives a **deterministic quorum**. There is no need to *elect* a committee — the committee is named in advance by entropy.
- There is **only one block** to vote on — the chair's proposal. There is no competing candidate to choose between.
- The only question is: **did enough quorum members sign?** That is a count, not a protocol.
- The chair's signature over the block is a valid signature for that specific `(purpose, parent_hash, unix_ts)` — it cannot be confused with a signature for a different block.

**Threshold:**

| Threshold | Byzantine tolerance | When to use |
|---|---|---|
| `⌈2k/3⌉` (default) | Up to `f < k/3` Byzantine | Standard BFT — matches Tendermint, HotStuff, Casper |
| `⌈7k/8⌉` (optional) | Up to `f < k/8` Byzantine | Higher-stakes purposes (e.g., bridge signing) |

`⌈2k/3⌉` is the **mathematical sweet spot** — the tightest threshold that guarantees safety against `f < k/3` Byzantine faults. Going higher **does not add safety**; it only narrows the set of honest configurations that can complete.

**Recommended:** `⌈2k/3⌉` by default, `⌈7k/8⌉` per-purpose when a deployment needs stricter finality (e.g., bridge operations).

**Per-purpose threshold in code:**

```rust
impl Purpose {
    pub fn threshold(&self, k: usize) -> usize {
        match self {
            Purpose::Bridge => (7 * k + 7) / 8,  // ceil(7k/8)
            _               => (2 * k + 2) / 3,  // ceil(2k/3)
        }
    }
}
```

**Single-validator skip:**

For `N ≤ 16`, `k = 0` — quorum is empty. There is no BFT count. The chair's signature alone seals the block. **Cryptographic determinism at the proposal layer.**

#### 6.3.2 Block seal structure

A sealed block contains:

```
Block {
    // Standard fields
    parent_hash: [u8; 32],      // the block_hash used by SELECT
    unix_ts:     u64,           // the unix_ts used by SELECT
    purpose:     Purpose,       // "chair" for normal proposals
    transactions: [...],

    // Seal — proves proposer authority
    chair: Address,
    chair_sig: Signature,       // chair signs the block hash

    // Quorum proof (empty if single-validator regime)
    quorum_sigs: Vec<(Address, Signature)>,
    // Note: quorum = [q1, …, qk] from SELECT, so verifier can
    // recompute the expected quorum and check each sig.
}
```

**Why this structure:**

- **Self-contained.** The block carries its own proof of authority. Any node can verify without network access.
- **Selection-gated.** Every quorum signature is from an address in `SELECT`'s output for this slot. A signature from a validator that was **not selected** for this slot is **rejected**.
- **Deterministic.** The block that gets sealed is unique for its `(purpose, parent_hash, unix_ts)`. No competing sealed blocks are possible from honest nodes.

#### 6.3.3 Verifier pseudocode

Any receiving node verifies a sealed block statelessly:

```rust
fn verify(block) -> bool {
    // 1. Recompute the only valid chair + quorum for this block's slot
    let [chair, q1..qk] = SELECT(block.purpose, block.parent_hash, block.unix_ts, set);

    // 2. Chair must be who the entropy selected
    if block.chair != chair {
        return false;
    }

    // 3. Chair's signature must be valid
    if !verify_sig(chair, block.chair_sig, hash(block)) {
        return false;
    }

    // 4. Single-validator case: no quorum to check
    if k == 0 {
        return true;
    }

    // 5. Every quorum signature MUST be from an address in [q1..qk].
    //    Signatures from validators not selected for this slot are REJECTED.
    for (addr, sig) in block.quorum_sigs {
        if addr ∉ {q1..qk} {
            return false;
        }
        if !verify_sig(addr, sig, hash(block)) {
            return false;
        }
    }

    // 6. Quorum threshold must be met
    let threshold = block.purpose.threshold(k);
    if block.quorum_sigs.len() < threshold {
        return false;
    }

    true
}
```

**Line 5 is the crucial one.** A valid validator that was not selected for this slot is **rejected** — not counted, not tolerated, not slashed-later. **Rejected at verification time.** This is the entire security argument for selection-gated signatures.

#### 6.3.4 What the consensus layer must still provide

- **Block validity rules** — signature checks, transaction validity, state transitions.
- **Fork choice** — which chain is canonical when two chains diverge (see §6.5).
- **Finality** — when a block is irreversibly committed (chain-specific rule).
- **Slashing / ejection** — what happens when a chair produces an invalid block or fails to produce anything.

`SELECT` is to consensus what a hash function is to a Merkle tree: a **reusable, well-defined primitive** that composes cleanly with a larger protocol.

### 6.4 Deposit fixed — closes the set-manipulation vector

If addresses are tied to **fixed deposits**, the set is a **snapshot** at a given block, not a live quantity. This closes set-manipulation as an attack vector.

**The fixed-deposit rule:**

- Addresses are derived from **deposit events** (stake-locked accounts).
- The set is **snapshotted at a given block height** — say, the start of an epoch.
- Deposits **after** the snapshot do not enter the current epoch's set.
- Withdrawals **after** the snapshot do not leave it until the next snapshot.

**Why this matters for `SELECT`:**

- The selection is over a **static set for the entire epoch**. No address can join or leave mid-epoch to game the chair.
- The `block_hash` used in the entropy is the **parent block of the current slot**, not the snapshot block. This is fine — the set is fixed, the entropy moves.
- Grinding is bounded by epoch boundaries. An attacker cannot "wait for a favorable slot within the epoch" — they can only choose which **epoch** to join, which is a public, fixed-rate event. **Statistical grinding is reduced to once per epoch, not once per slot.**

**Combined with §5.2:** stake grinding is neutralized (selection is uniform over set), and set grinding is neutralized (set is fixed per epoch). Only **epoch-boundary grinding** remains, and it has the same cost as honest joining — no advantage.

### 6.5 Parent hash — honest as long as it is canonical

`SELECT` binds to `block_hash`, which is the parent block's hash. The claim that this is "always honest" holds **if and only if** the fork choice is sound:

- **The parent is canonical** — every honest node agrees on which block is the parent of slot `T`.
- **The parent is retrievable** — any node can recompute its hash from the block history.

**What the fork choice rule actually is:**

A **fork choice rule** answers one question, over and over: *"Right now, which chain is the canonical chain?"*

Every blockchain has one:

| Chain | Fork choice rule |
|---|---|
| Bitcoin | Longest chain (most cumulative work) |
| Ethereum | LMD-GHOST (heaviest attested chain) + Casper FFG finality |
| Tendermint | 2/3+ precommit votes at a height |
| Solana | Tower BFT (vote-weighted, with lockout) |
| A DAG | Whatever rule the DAG defines |

**`SELECT` does not contain a fork choice.** It has no notion of "the current chain." It's a pure function: `(block_hash, unix_ts, addrs) → chair`. It just does the math.

**What "fork choice is inherited" means concretely:**

Imagine two nodes, Alice and Bob, both running the same `SELECT` code, both seeing the same address set, both with correctly synced clocks. They should agree on the chair — **if and only if** they agree on `block_hash`.

```
At height H = 1000:

Alice's view:                          Bob's view:
  Block 999 = 0xAaaa...                Block 999 = 0xAaaa...
  Block 1000a = 0xBb11...  ← first     Block 1000b = 0xBb22...  ← first

Alice computes:                        Bob computes:
  parent = 0xBb11...                     parent = 0xBb22...
  chair  = select(0xBb11, ts, addrs)     chair  = select(0xBb22, ts, addrs)
         = "carol"                              = "dave"    ← DIFFERENT
```

Both nodes ran `SELECT` correctly. Both followed the spec exactly. Both produced a valid, deterministic chair for **their view of the chain**. But they disagree on **who the chair of block 1001 is** — because they disagree on **what block 1000 was**.

`SELECT` is **not at fault**. It cannot be. It was given two different inputs and (correctly) produced two different outputs. The fault is that Alice and Bob haven't converged on a canonical block 1000.

**Once they converge** — once the fork choice rule kicks in and both agree that 1000a is canonical — they'll both compute the same chair for block 1001. **Convergence of the chair is downstream of convergence of the fork choice.**

**What `SELECT` adds to the fork choice problem: nothing.**

`SELECT` is **stateless** and **pure**. It doesn't vote, doesn't produce candidate blocks, doesn't hold a preference. It **cannot introduce** a fork, **cannot worsen** a fork, and **cannot delay** fork resolution. It just reads what the fork choice tells it and computes.

**What `SELECT` inherits from the fork choice: everything.**

If the fork choice is sound and converged → `SELECT` gives consistent, verifiable chairs on all honest nodes. If the fork choice is broken or in flux → `SELECT` gives conflicting chairs on nodes with different views. `SELECT`'s security is **bounded above** by the fork choice's security.

**The complete picture — fork choice and BFT in `SELECT`:**

```
┌─────────────────────────────────────────────────────────────────┐
│  BFT happens INSIDE the block (at seal time)                    │
│  — chair collects quorum sigs, counts them, seals.              │
│  — verifier re-counts them.                                     │
│  — outcome is IN THE BLOCK.                                     │
└─────────────────────────────────────────────────────────────────┘
                              │
                              ▼
┌─────────────────────────────────────────────────────────────────┐
│  Fork choice happens AT SEAL CHECK (when a block arrives)       │
│  — verifier checks parent_hash against its canonical view.      │
│  — if parent is canonical → block is on the honest chain.       │
│  — if parent is not canonical → block is on a fork.             │
│  — fork choice rule decides which chain wins.                   │
└─────────────────────────────────────────────────────────────────┘
```

**The practical consequence:** as long as **any two honest nodes agree** on the current parent block, they agree on the chair. `SELECT` adds **no new attack surface** to the fork choice; it simply reads its output.

### 6.6 Revised risk table

| Concern | Treatment |
|---|---|
| **Sybil resistance** | Delegated to PoS deposit layer — `SELECT` selects uniformly over the stake-gated set |
| **Clock sync** | Bounded to ±2 s with grace window; adaptive fallback available |
| **Consensus integration** | Explicitly a component — replaces only leader election, composes with any BFT / longest-chain / DAG |
| **Set manipulation** | Fixed per epoch — deposits snapshot at epoch start, no mid-epoch join/leave |
| **Parent hash honesty** | Honest iff fork choice is honest — same assumption as every chain-based protocol |
| **Grinding** | Reduced to once per epoch (epoch boundary), not once per slot |
| **Equivocation** | Impossible (one chair, one second) — provable in one line |
| **Long-range** | Cost = work-rate parity with honest chain |
| **BFT rounds** | Eliminated — BFT is a signature count, not a protocol round |
| **Signature authority** | Selection-gated: only `SELECT`'s quorum can sign for that slot |

### 6.7 What actually remains as a limitation

After all corrections, three genuine limitations remain:

**1. Clock is a foundational assumption.**

A node with > 2 s clock error has bigger problems than `SELECT`. Not unique to `SELECT`; shared with every distributed protocol. Mitigated by NTP + optional adaptive peer-time. A deployment that cannot maintain ±2 s sync cannot run distributed consensus at all — `SELECT` is not the weak link.

**2. Fork choice is inherited, not owned.**

`SELECT` takes `block_hash` as **input**. It does not decide which `block_hash` is canonical — that's the fork choice rule's job, which lives at the consensus layer above.

Concretely: if Alice thinks the parent of slot `T` is `0xBb11` and Bob thinks it's `0xBb22`, they will (correctly) compute two different chairs for slot `T` — because they gave `SELECT` two different inputs. `SELECT` **cannot detect** that they disagree, and **cannot fix** it; it faithfully computes what it was asked to compute.

**`SELECT` does not worsen this.** It is stateless, pure, and passive — it doesn't vote, doesn't propose, doesn't prefer one fork over another. It inherits exactly the fork-choice reliability of the protocol it's embedded in. Where a stake-weighted PoS proposer *can* actively introduce equivocation, `SELECT` **cannot**: it doesn't produce blocks, only picks who does.

**Practical consequence:** `SELECT`'s security is **bounded above** by the security of the fork choice. A deployment with a sound, converged fork choice gets fully consistent chairs. A deployment with a broken or contested fork choice gets chairs that reflect that — but `SELECT` is not the source of the problem and not the way to solve it. **Fix fork choice at the consensus layer; use `SELECT` for what it does well.**

**3. The set is fixed per epoch.**

This is a **feature** for security (no mid-epoch grinding) and a **tradeoff** for liveness (a cartel cannot join mid-epoch to overtake; but an honest majority that grows mid-epoch cannot help either).

If dynamic set membership is required, use a **sub-epoch** (e.g., re-snapshot every `M` blocks) with `M` chosen for the deployment's latency tolerance. This reduces the fixed-set window without opening mid-slot grinding.

### 6.8 Bottom line

`SELECT` is not a full consensus protocol. It is a **selection primitive** designed to be inserted into a PoS consensus stack. Its security properties depend on:

- A **stake-gated address set** (§6.1) — provided by the PoS layer.
- A **±2 s clock assumption** (§6.2) — a standard distributed-systems premise.
- **Consensus integration** (§6.3) — `SELECT` replaces leader election, nothing else.
- **Fixed per-epoch deposit snapshot** (§6.4) — ensures set stability.
- **Sound fork choice** (§6.5) — inherited from the consensus layer.

Given these, `SELECT` provides:

- ✅ Uniform, grind-resistant chair selection.
- ✅ Purpose-independent committee streams.
- ✅ Deterministic, cross-verifiable output (on-chain or off-chain).
- ✅ Automatic liveness via wall-clock advancement.
- ✅ No equivocation, no operator-driven recovery, no cartel snowball.
- ✅ **Selection-gated signature authority** — only the entropy-selected quorum can sign for a block.
- ✅ **BFT as a signature count** — no protocol rounds, no view changes.
- ✅ Byte-identical reproducibility from `(purpose, block_hash, unix_ts, unique set)`.

The remaining risks are **infrastructure-layer assumptions** (clock, fork choice, stake-gated set), not weaknesses of the selection primitive itself.

### 6.9 One-sentence summary

> `SELECT` provides uniform, grind-resistant, purpose-tagged chair selection over a **stake-gated, per-epoch-fixed address set**, using **wall-clock-bound two-source entropy**, with **±2 s clock tolerance**. It replaces **only leader election** in a consensus stack, uses **selection-gated signature authority** to ensure only the entropy-selected quorum can sign for a block, and reduces BFT to a **single signature count** over those signatures. Its guarantees are **byte-identical reproducibility** of the chair-and-quorum vector given `(purpose, block_hash, unix_ts, unique address set)`.

---

## 7. Reference implementation

### 7.1 Selecting a chair for the current slot

```rust
use selector::{chair_entropy, current_slot, select_chair_and_quorum, Purpose};

let block_hash: [u8; 32] = /* parent block hash */;
let entropy = chair_entropy(block_hash, current_slot());

let c = select_chair_and_quorum(Purpose::Chair, &entropy, &validators);
let chair  = &c[0];           // guaranteed present
let quorum = &c[1..];         // empty if validators.len() <= 16
```

### 7.2 Selecting a different committee for a different purpose

```rust
let bridge = select_chair_and_quorum(Purpose::Bridge, &entropy, &validators);
let audit  = select_chair_and_quorum(Purpose::Audit,  &entropy, &validators);
```

Same entropy, different committees. Each purpose is an independent selection stream.

### 7.3 Sealing a block (chair side)

```rust
fn seal(purpose, parent_hash, unix_ts, set, transactions) -> Option<SealedBlock> {
    let c = select_chair_and_quorum(purpose, &Entropy::new(parent_hash, unix_ts), set);
    let chair = c[0].clone();
    let quorum = &c[1..];

    let block = Block { purpose, parent_hash, unix_ts, transactions, chair, ... };

    let chair_sig = sign(chair_key, hash(&block));

    // Single-validator regime: no quorum, no BFT count
    if quorum.is_empty() {
        return Some(SealedBlock { block, chair_sig, quorum_sigs: vec![] });
    }

    // Multi-validator regime: collect quorum signatures
    let mut quorum_sigs = vec![];
    for q in quorum {
        if let Some(sig) = request_signature(q, &block) {
            quorum_sigs.push((q.clone(), sig));
        }
    }

    // BFT = count(Yep) >= threshold
    let threshold = purpose.threshold(quorum.len());
    if quorum_sigs.len() < threshold {
        return None;  // block dies; next second's chair takes over
    }

    Some(SealedBlock { block, chair_sig, quorum_sigs })
}
```

### 7.4 Verifying a sealed block (any node side)

See §6.3.3 for the full pseudocode. Key invariants:

- Chair must match `SELECT`'s output.
- Every quorum signature must be from a `SELECT`-named quorum member.
- Threshold must be met (`⌈2k/3⌉` default, `⌈7k/8⌉` optional).
- Single-validator case: chair sig alone is sufficient.

### 7.5 On-chain verification

Because `SELECT` uses only Keccak-256, it can be verified **natively on-chain** — no oracle, no signature scheme, no custom crypto:

```solidity
function verifyChairAndQuorum(
    bytes32 parentHash,
    uint64  unixTs,
    string  purpose,
    string[] calldata addrs,
    string calldata claimedChair,
    bytes[] calldata quorumSigs,
    string[] calldata quorumSigners
) public pure returns (bool) {
    // 1. Recompute chair + quorum via SELECT (Keccak-based min-hash).
    // 2. Verify claimedChair == SELECT chair.
    // 3. Verify every quorumSigner ∈ SELECT quorum.
    // 4. Verify every signature.
    // 5. Verify count >= threshold.
}
```

The on-chain cost is dominated by iterating `addrs` and verifying signatures — no external trust required.

---

## 8. Design decisions log

| Decision | Rationale |
|---|---|
| **Uniform over set**, not stake-weighted | Removes stake concentration attacks |
| **Two-source entropy** (`block_hash`, `unix_ts`) | `block_hash` binds to chain; `unix_ts` provides liveness |
| **Timestamp observed**, not chosen | Eliminates operator-driven recovery race |
| **Purpose tag** in hash | Enables independent selection streams from same entropy |
| **Length-prefix** every variable field | Prevents concatenation ambiguity |
| **Score + rehash** on tie | One-round recovery, provably sufficient |
| **Lexicographic** final fallback | Deterministic termination in the vanishing case both hashes tie |
| **Chair at `[0]`**, quorum at `[1..]` | Single flat array, no struct, no ambiguity |
| **`N ≤ 16` returns 1** | Avoids awkward quorum of 1 for small sets; cryptographic determinism |
| **`N > 16` returns `1 + round(3N/16)`** | Matches spec: 32 → 7, 64 → 13; empirically well-tested sizes |
| **Score-ordered output** | Preserves rank information for fallback (chair → quorum → next quorum) |
| **Keccak-256** | Widely deployed, NIST-standardized (SHA-3), fast, preimage-resistant |
| **Chair collects quorum sigs, then seals** | Preserves "one sealed block per slot" — the anti-multi-proposer property |
| **BFT = signature count** | No protocol rounds, no view changes, no state machines |
| **Selection-gated signature authority** | Only `SELECT`'s quorum can sign for that slot — structural containment of corruption |
| **`⌈2k/3⌉` default threshold** | Mathematical sweet spot — tightest threshold that guarantees safety against `f < k/3` |
| **Per-purpose thresholds** | Bridge uses `⌈7k/8⌉` for higher-stakes finality; chair uses `⌈2k/3⌉` |
| **Single-validator skip** | `k = 0` → no BFT count, chair sig alone seals block |

---

## 9. Summary

`SELECT` is a **position-free, purpose-tagged, entropy-keyed** selection primitive for choosing a chair and quorum.

Its security advantages over regular PoS derive from **four orthogonal properties**:

1. **Uniform over set** (not stake-weighted) — eliminates stake concentration and cartel formation incentives.
2. **Wall-clock entropy** (not operator-triggered) — eliminates liveness races and gives automatic recovery.
3. **Pure-function reproducibility** (Keccak over typed bytes) — enables on-chain verification and cross-language auditability.
4. **Selection-gated signature authority** — only the entropy-selected quorum can sign for a block; corruption of addresses is necessary but not sufficient to sign arbitrarily.

**The three headline problems it solves:**

- **One proposer per slot** — single-writer semantics, no fork races, no equivocation.
- **Ganging hardened** — uniform selection + one-slot entropy horizon + selection-gated signatures = coordinated attacks are structurally unprofitable and structurally contained.
- **Backwards validation** — any historical chair is recomputable from headers alone in O(N); light clients can verify history.

It is not a consensus protocol. It is a **selection primitive** — one small, correct piece that a consensus layer can build on. The guarantees are strong, the boundaries are enumerated (§6), and the implementation is small enough to audit in an afternoon.

**Same `(purpose, block_hash, unix_ts, unique set)` → same `[chair, quorum…]`, byte for byte. That is the whole contract.**

**And: only the entropy-selected quorum can sign for a block. That is the whole security model.**
````

---

**Changes in v5 from v4:**

- **§1.1 — The pipeline** — new section showing the 4-step flow (PoS deposit → SELECT → sign/seal → accept/finality).
- **§5.1 addendum** — single-validator cryptographic determinism.
- **§5.2.2 — Selection-gated signature authority** — new section, the security property you named.
- **§5.6 table** — added "Signature authority" and "BFT rounds" rows.
- **§5.7 one-paragraph case** — updated to include selection-gating.
- **§5.9** — updated problem 2 to mention selection-gating.
- **§6.3.1 — BFT as signature count** — new subsection, with threshold table and per-purpose config.
- **§6.3.2 — Block seal structure** — new subsection.
- **§6.3.3 — Verifier pseudocode** — new subsection with the `addr ∉ {q1..qk} → reject` line highlighted.
- **§6.3.4 — What consensus must still provide** — extracted from old §6.3.
- **§6.5 — Fork choice clarified** — the concrete Alice/Bob example, "what `SELECT` adds: nothing" / "what it inherits: everything," plus the diagram of BFT-inside-block vs fork-choice-at-seal-check.
- **§6.6 risk table** — added BFT rounds and signature authority rows.
- **§6.7.1 / §6.7.2 / §6.7.3** — three named remaining limitations with expanded 6.7.2.
- **§6.9 — updated one-sentence summary** including selection-gated signatures and BFT-as-count.
- **§7.3 / §7.4 / §7.5** — added sealing pseudocode, verifier reference, and on-chain verification.
- **§8 — 5 new design decisions** (chair collects then seals, BFT = count, selection-gated sigs, per-purpose thresholds, single-validator skip).
- **§9 — Summary updated** to four orthogonal properties (added selection-gated signature authority) plus three headline problems.

- in case of quorum BFT should be checked by node as well upon acceptance (sig yes count vs no)

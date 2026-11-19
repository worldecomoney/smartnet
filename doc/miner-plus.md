Transaction-native signaling, `validators.jsonl` as the validator registry, heartbeat-driven lifecycle checks, and no separate miner-management infrastructure.

There are a few details worth making explicit, particularly the meaning of `status`, who is eligible to promote whom, and how rejection differs from transaction queuing.

# Calibri-Lite — Validator Lifecycle & Transaction Signaling

Protocol specification · Draft

## 1. Purpose

This document defines how validators are promoted, disabled, re-enabled and dismissed from the SmartNet network.

Validator management is transaction-native. All requests are communicated through ordinary network transactions using the existing self-transfer signaling convention.

No dedicated miner-management service, separate communication channel or privileged genesis-validator mechanism is introduced.

## 2. Validator registry

The current validator registry is maintained in:

```
validators.jsonl
```

Each record follows this format:

```
{"address":"0x...","status":null}
```

Status definitions:

| Status  | Meaning                                                       |
| ------- | ------------------------------------------------------------- |
| `null`  | Not explicitly disabled; normal membership evaluation applies |
| `true`  | Enabled                                                       |
| `false` | Disabled                                                      |

The registry is authoritative for the local validator's membership and operational eligibility.

Genesis validators receive no special protection or exemption from lifecycle operations.

## 3. Validator promotion

A prospective validator initiates its own promotion request.

It submits a regular signed self-transfer transaction:

| Field   | Value                       |
| ------- | --------------------------- |
| From    | Requesting address          |
| To      | Requesting address          |
| Value   | `0.91 WEM` (fixed)          |
| Data    | `0xff91`                    |
| Meaning | Validator seeking promotion |

`0xff91` is the Calibri-Lite command identifier for `VALIDATOR_PROMOTE`.

### Processing

When an existing validator receives the request from its txpool:

1. Recognize the fixed signaling value and command identifier.
2. Identify the requesting address from the transaction sender.
3. Check the requesting address against `validators.jsonl`.
4. Check that the requester is eligible and not disabled.
5. Check that the requester is not already an active validator.
6. Check that the request is not a duplicate.
7. If all conditions are satisfied, process the promotion according to the consensus rules.

Once accepted and included in a canonical block, the requesting node becomes an authorized validator.

A rejected request produces no membership change.

## 4. Validator lifecycle and heartbeat

Every validator begins its operational lifecycle with a heartbeat-triggered self-check.

The sequence is:

1. Registry integrity: Verify the `validators.jsonl` hash against the hash distributed through the mesh.
2. Membership enforcement: Inspect registry statuses and disable any validator marked `false` if it is not already disabled.
3. Operational eligibility: Verify that the local node is authorized and enabled.
4. Transaction processing: Proceed with normal txpool processing and Calibri-Lite signaling recognition.

Registry hash distribution is currently handled manually. A future protocol revision will define its automated distribution and synchronization procedure.

A validator must not operate using a registry whose integrity cannot be established.

## 5. Validator dismissal and suspension

An existing validator can be targeted for removal or temporary suspension through another self-transfer signaling transaction.

The requester sends:

| Field | Value                                         |
| ----- | --------------------------------------------- |
| From  | Requesting validator                          |
| To    | Requesting validator                          |
| Value | Fixed Calibri-Lite signaling amount           |
| Data  | Command identifier + target validator address |

The target address is encoded as exactly 20 bytes.

The command identifiers and their corresponding signaling values must be reserved in the Calibri-Lite protocol.

Suggested operations:

| Operation           | Effect                                     |
| ------------------- | ------------------------------------------ |
| `VALIDATOR_DISABLE` | Temporarily disables the target            |
| `VALIDATOR_ENABLE`  | Re-enables a previously disabled validator |
| `VALIDATOR_DEMOTE`  | Removes validator membership               |

All operations follow the same eligibility, validation and consensus-approval principles.

There are no genesis-validator exemptions. Every validator is subject to the same rules.

## 6. Transaction signaling convention

Calibri-Lite formalizes the existing SmartNet self-transfer signaling mechanism.

A signaling transaction is identified by its combination of:

- Sender and recipient addresses.
- Fixed signaling amount.
- Command identifier and optional payload in transaction data.

The command parser must recognize exact combinations. Ordinary self-transfers remain ordinary transactions.

This mechanism is shared by:

- Bridge-to-validator communication.
- Validator promotion.
- Validator suspension and reactivation.
- Validator removal.
- Future Calibri-Lite protocol commands.

Each command has its own identifier, payload definition and processing rules.

## 7. Immediate transaction rejection

Validators must have an explicit transaction rejection mechanism.

Transaction handling must distinguish three outcomes:

| Outcome | Meaning                                      |
| ------- | -------------------------------------------- |
| Accept  | Transaction is eligible for processing       |
| Queue   | Transaction is temporarily deferred          |
| Reject  | Transaction is invalid or explicitly refused |

A rejected transaction must not remain queued or be repeatedly reconsidered.

Rejection must produce a definitive result and remove the transaction from the relevant txpool.

Importantly, local txpool rejection and consensus-level transaction invalidity are different things. A validator's local refusal to include a transaction does not automatically make it invalid for every other validator. Network-wide rejection requires deterministic protocol rules.

## 8. Protocol principles

Calibri-Lite follows these principles:

1. No dedicated miner-management process or communication channel.
2. No privileged genesis validators.
3. Validator management uses existing transaction propagation.
4. Membership changes are consensus-controlled.
5. Registry integrity is checked through the existing mesh.
6. Validator lifecycle checks are heartbeat-triggered.
7. Transaction signaling uses fixed, protocol-defined identifiers and amounts.
8. Invalid transactions are rejected, not indefinitely postponed.

## 9. Implementation notes and decisions to finalize

There are three details I'd explicitly settle before implementation:

A. Registry status semantics

I recommend:

- `null`: registered candidate, not yet promoted.
- `true`: active validator.
- `false`: disabled or dismissed validator.

This makes the promotion check straightforward: the candidate must exist in the registry, have `status: null`, and not be the receiving validator itself.

B. Registry updates

The registry must reflect canonical consensus decisions. A local validator must not independently promote or dismiss someone by editing its JSONL file. Registry changes must be deterministic and verifiable across nodes.

C. Rejection semantics

A hard rejection should be an explicit txpool outcome, separate from temporary conditions such as nonce gaps. This avoids accidentally turning every temporarily unprocessable transaction into a permanent rejection.

My main architectural observation: this design reuses three things SmartNet already has: ordinary transactions, mesh propagation and heartbeat-driven validator lifecycle management. Calibri-Lite adds the common interpretation and governance rules rather than introducing another infrastructure layer.

One thing I'd particularly preserve is the existing bridge self-transfer convention. Governance and bridge communication should share the same signaling envelope, with independent command handlers.

Understood. That simplifies the governance mechanism significantly. We should remove the separate voting/approval stage entirely.

Transaction inclusion is the acceptance decision.

### Revised promotion rules

1. Candidate submits the promotion self-transfer:
   - Value: `0.91 WEM`
   - Data: `0xff91`
2. Existing validators inspect the request in their txpool.
3. Each validator independently decides whether to reject or allow the transaction.
4. If a validator includes the request in a block, that block's inclusion constitutes its acceptance.
5. Once the transaction is included in the canonical chain, the candidate becomes an authorized validator.

No additional vote, approval transaction or second confirmation is required.

### Important consensus detail

Because blocks are shared, the promotion must be deterministic:

- A validator that mines the promotion transaction accepts it.
- Validators validating that block must recognize the promotion and update their canonical validator state.
- The new validator becomes eligible to seal from the defined activation point.
- A rejected request produces no membership change.

This also means the miner has meaningful authority over its own block contents, but no permanent privilege: genesis validators are subject to exactly the same rules.

I'd update the central Calibri-Lite principle to:

> Inclusion is acceptance. A valid governance request included in a canonical block constitutes the accepting validator's decision. No separate voting or approval mechanism is required.

One further detail: if different validators can independently reject the same request, that is fine as long as rejection is local and inclusion is the acceptance event. Consensus validation must still ensure that the resulting validator-set transition is deterministic.

Yes. This gives Calibri-Lite a generic signaling envelope rather than a collection of special transaction formats.

I'd formalize it as follows.

## Calibri-Lite — Native Transaction Signaling

A native transaction satisfying both conditions is considered a potential Calibri-Lite signal:

- `from == to` (self-transfer).
- `data` is non-empty.

The validator inspects its data before treating it as an ordinary transaction.

### 1. Data encoding

Your proposed structure:

```
{COMMAND: 4 bytes}
{TARGET: 20 bytes, optional}
{VALUE: 32 bytes, optional}
```

Conceptually:

```
RLP([
    command,       // 4 bytes
    target,        // optional 20-byte address
    value          // optional 32-byte uint256
])
```

I'd distinguish between the RLP structure and the fixed-width fields inside it. This avoids ambiguity when optional fields are omitted.

### 2. Example commands

| Command       | Target   | Meaning                     |
| ------------- | -------- | --------------------------- |
| `0xff91`      | None     | Request promotion of sender |
| Reserved code | 20 bytes | Disable target              |
| Reserved code | 20 bytes | Enable target               |
| Reserved code | 20 bytes | Demote target               |

The signaling transaction's fixed value can remain `0.91 WEM` for promotion, with other commands using their defined signaling values if needed.

### 3. Processing rules

```
Native transaction
       |
       v
from == to && data != empty?
       |
   Yes | No
       |
       v
Calibri-Lite parser
       |
       v
Recognized command?
       |
   Yes | No
       |
       v
Command handler
```

An unrecognized command should not accidentally trigger governance operations.

### 4. Two details I'd lock down

- Command width: exactly 4 bytes, network byte order.
- Address width: exactly 20 bytes, raw address bytes, no `0x` prefix.

Also, optional fields need command-specific schemas. A promotion command should not accept a target address, while a demotion command requires one.

One important correction: if you intend actual Ethereum transaction `data` to contain this structure, define its RLP encoding precisely. RLP itself is not a delimiter-based format; each field must be encoded as an RLP item, with canonical decoding and exact field-count validation.

This is a good foundation for a reusable Calibri-Lite command system, covering bridge signaling and validator governance without introducing new transaction types.

Exactly. That's the fundamental architectural reason for choosing transactions.

We're not using transactions merely as a convenient transport. We're using them because the communication itself is part of the blockchain's permanent, ordered history.

That gives Calibri-Lite several properties almost for free:

1. Persistence: Every accepted command is recorded on-chain.
2. Ordering: Commands have an unambiguous canonical execution order.
3. Auditability: Anyone can inspect when a validator was promoted, disabled or removed.
4. Replayability: A node rebuilding state can reconstruct validator membership by replaying canonical history.
5. Consistency: All nodes derive the same governance state from the same transactions.
6. Compatibility: Existing transaction propagation, validation, txpool and block execution remain in use.

## The architectural principle

I'd put this directly into the specification:

> Calibri-Lite uses native self-transfer transactions as its communication and signaling mechanism. These transactions are not merely messages transported through the network; they are persistent protocol instructions recorded in canonical blockchain history. Their inclusion, ordering and execution provide an auditable record of protocol activity and allow deterministic reconstruction of protocol state.

And one distinction is important:

- A transaction sitting in a txpool is a pending request.
- A transaction included in a block is an executed protocol instruction.
- A transaction in a canonical block is part of the authoritative history.


## To clarify: once validator sees 0.91 WEM + non empty data, he tries to decode data if success, act on it, sign or reject based on validators.jsonl file content..
once node see this tx mined to block, node acts immediately, and add/pause/remove validators according with transaction! All changes effective immediately after tx mined to block.
For now, no chair/quorum affection, quorum = 1, chair = rnd. if more than 1 validators chair is determined by formula..

This also means we don't need a separate governance database, proposal journal or event transport. The validator registry can be reconstructed from canonical history, with `validators.jsonl` serving as its maintained local representation.

One design principle worth protecting: Calibri-Lite should never create a second source of truth alongside the blockchain.

# SECURITY-EVM-GATE.md — EVM GATE, successor to EVM block REWARD

> Mechanics reference: `doc/EVM-ENGINE-CHANGES.md`.
>
> Status convention used below: **[DESIGN]** = stated design intent, **[CURRENT]** =
> present implementation, **[PLANNED]** = not yet wired. Only implemented mechanics and
> the design claim are described here; the implementation map, collected findings and
> verification notes live outside this document.

---

## 0. Scope of the security claim

**GATE can reasonably claim that it is not inherently less secure by design than
Ethereum's current PoS withdrawal mechanism.**

GATE uses a different authorization model, not necessarily a weaker one:

- **Ethereum:** consensus-layer authorization determines valid withdrawals; execution
  applies them through a dedicated system-level mechanism.
- **GATE:** chair authorization is carried through a signed transaction, with the
  receiving node expected to validate the chair and the mandate.

Neither model is inherently less secure. Both depend on correctly enforcing their
authorization rules. Two claims are kept distinct:

1. **Defensible now:** GATE's design does not inherently introduce a security
   disadvantage compared with Ethereum's withdrawal mechanism.
2. **Not yet established:** GATE has security equal or superior to Ethereum in
   implementation and operation.

The wording used for that design-level claim is:

> **GATE is designed to provide a security model comparable in principle to Ethereum's
> PoS withdrawal mechanism, using explicit chair authorization and mandate-based
> issuance rather than consensus-layer withdrawal processing.**

The comparison concerns the authorization model for an on-chain balance credit, not the
surrounding consensus architecture. Ethereum PoS validator rewards are newly issued on
the consensus layer; EIP-4895 withdrawals move accumulated balances into execution-layer
accounts. GATE's credit is neither operation exactly — it is closest to the
execution-layer credit of a consensus-authorized withdrawal, while also serving as an
issuance mechanism.

---

## 1. REWARD → GATE: the three points

The traditional EVM block **REWARD** mechanism is disabled for SmartNet. Its successor
is the **EVM GATE**: a per-transaction, per-block balance-check exception in execution,
paired with the chair's transaction builder. The distinction is mechanical:

1. **REWARD is silent, unguarded, and unlimited.**

2. **GATE is guarded, explicitly initiated, and limited by dedicated receipt input from
   the blockchain.**

3. **GATE is a transaction** that leaves a permanent record on the blockchain. It is
   transparent, auditable, and bounded.

Status of those points against the current tree: (1) and (3) are **[CURRENT]** mechanics
(§3). Point (2)'s "guarded / explicitly initiated" half is **[CURRENT]** at the execution
layer (seal-gated, per-tx, nonce-restricted); the "limited by dedicated receipt input"
half is **[DESIGN] / [PLANNED]** — the chair-side mandate/receipt wiring is not yet
implemented. A receipt input that is not yet checked cannot be claimed as a current
limit.

## 2. Important clarification

**EVM GATE does not claim to be more secure or better in any way than the traditional
REWARD mechanism. Its only claim is that it is not inferior to the existing EVM REWARD
mechanics and security. It represents a different approach, nothing more. No claims of
superior security, performance, or functionality are made.**

The same neutrality applies to the Ethereum comparison in §0 and to every comparison in
`doc/EVM-ENGINE-CHANGES.md` §2: design-comparable in principle, mechanical differences
only, no ranking and no implementation-equality claim.

---

## 3. What the GATE is — current mechanics

- **[CURRENT]** One condition in `EthEvm::transact_raw`
  (`smartnet/vendor/alloy-evm/src/eth/mod.rs:222-256`): `gate_opens = tx.caller ==
  block.beneficiary`; `CfgEnv.disable_balance_check` is OR-ed with the import-mode flag,
  then **restored** after each transaction. It never sticks across txs, blocks or
  restarts.
- **[CURRENT]** With the check off, revm credits the recipient in full and floors the
  caller's balance at the tx value; nonce checks and EIP-3607 still apply. Contract/EVM
  semantics are otherwise unchanged.
- **[CURRENT]** `chair::tx_out` (`smartnet/crates/node/validator/src/chair.rs:37`) builds
  and signs one native `value + data` transfer with RFC 6979 determinism (a rebuilt
  broadcast is byte-identical, so at most one of the two can land). It has **no
  production caller yet**.
- **[CURRENT]** The producer sets `header.beneficiary = args.fee_receiver`
  (`node/validator/src/run_once.rs:870`); the seal signer is checked by `check_seal`
  (`calibri-lite/.../select.rs:212`) against `get_val_chair` (`select.rs:175`,
  `status: true` validators only). The two agree by producer convention, not by a
  receiving-side validation.
- **[CURRENT]** Import mode sets `disable_balance_check = true` for *every* transaction
  to replay trusted geth history; that flag is a historical-data accommodation, not a
  live GATE behavior.
- **[PLANNED]** Dedicated receipt input: the receipt-derived limit is design intent and
  is not yet wired.

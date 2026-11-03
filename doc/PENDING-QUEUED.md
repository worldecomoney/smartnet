## Ethereum txpool: pending vs queued

### 1. Core concepts

1. Txpool is a node-local collection of transactions received but not yet included in the canonical chain. Different nodes can have different pools.
2. Pending means executable transactions, generally with a continuous nonce sequence starting at the sender's current state nonce.
3. Queued means transactions held for future execution, most commonly because of a missing nonce.
4. Example: account nonce is `5`; transactions with nonces `5`, `6`, and `8` arrive. Nonces `5` and `6` are pending; `8` is queued because nonce `7` is missing.
5. Transaction order is enforced per sender, not globally. Miners/validators choose among eligible transactions according to block-building policies.
6. Pending does not mean guaranteed inclusion; it means the transaction is eligible for consideration.


### 2. Native coin transactions

7. A native transfer carries a native-currency `value` and usually targets an externally owned account or contract.
8. The sender must have enough balance to cover the transaction's value and required gas expenditure under the applicable validation rules.
9. The transaction must have a valid signature, correct chain ID, acceptable nonce, valid gas parameters, and sufficient gas limit.
10. A nonce gap generally places an otherwise acceptable transaction in the queued pool.
11. If the missing nonce arrives and the sequence becomes executable, queued transactions can be promoted to pending.
12. If the sender's balance becomes insufficient, transactions may be rejected, dropped, or demoted, depending on the circumstances and client implementation.
13. If a transaction's nonce is lower than the current canonical account nonce, it is stale and cannot execute again on that chain.
14. Transactions competing for the same sender and nonce are alternatives; normally only one can be included.

### 3. ERC-20 transactions

15. An ERC-20 transfer is an ordinary Ethereum transaction, not a separate transaction type in the txpool.
16. The transaction's native `value` is usually zero, while `to` points to the token contract and `input` contains the encoded function call and arguments.
17. The sender pays gas in the chain's native currency, not in the ERC-20 token being transferred.
18. Therefore, a sender can own plenty of tokens but still be unable to submit or execute a transfer because they lack native currency for gas.
19. The txpool generally does not execute every token contract call to verify the sender's token balance before accepting it.
20. Consequently, a transaction may be pending even if its eventual contract execution will revert due to insufficient tokens, failed authorization, paused transfers, or other contract conditions.
21. Token balances and allowances are contract state, not ordinary account balances checked by the basic txpool.
22. ERC-20 transactions use the sender's same account nonce sequence as native transfers and other transactions sent by that account.
23. A missing nonce blocks later transactions regardless of whether those transactions are native transfers, ERC-20 calls, or DeFi interactions.

### 4. Promotion and demotion

24. Promotion: a missing earlier nonce arrives, making a queued sequence contiguous.
25. Promotion: the chain head changes, and updated state makes a previously ineligible transaction executable.
26. Demotion: a previously pending transaction becomes non-executable after a state update, such as a balance change or reorganization.
27. Removal: a transaction is included in the canonical chain, becomes stale, is replaced, expires under pool policy, or is evicted due to pool limits.
28. A reorganization can return previously included transactions to consideration, subject to current validation and pool policy.
29. If a transaction at nonce `7` is removed or becomes unusable, later transactions from that sender may remain queued behind the gap.
30. Clients may discard transactions rather than preserve them indefinitely. Pool capacity, lifetime, fee thresholds, and eviction policies are implementation-specific.


### 5. Replacement and fees

31. A sender can replace a transaction by submitting another with the same sender and nonce.
32. The replacement generally needs to meet the client's required fee increase; Geth has a configurable `--txpool.pricebump` setting.
33. Under legacy transactions, the relevant fee field is `gasPrice`; under EIP-1559, fee-cap and priority-fee rules apply.
34. A replacement can change the destination, native value, or contract calldata—not just the fee.
35. Replacing a transaction does not guarantee that the new one will be mined.
36. A transaction with an insufficient fee may be rejected or removed under local pool policy, even if its nonce and balance are otherwise valid.

### 6. Relationship with DeFi

37. DeFi operations—swaps, lending, liquidity provision, and token approvals—are generally ordinary EVM transactions calling smart contracts.
38. A swap can be pending while the market changes; when executed, it may succeed, revert, or fail slippage checks.
39. An approval transaction and a subsequent swap from the same sender normally have sequential nonces, so the swap cannot execute before the approval transaction if the approval has the earlier nonce.
40. However, a successful approval does not guarantee a successful swap: token balances, liquidity, prices, and contract conditions can change.
41. A transaction's pool status describes its execution eligibility, not its expected DeFi outcome.

### 7. Practical implementation summary

42. Maintain one nonce-ordered transaction sequence per sender.
43. Classify transactions using the canonical state nonce and the pool's projected executable nonce.
44. Validate intrinsic transaction rules, signatures, fee parameters, gas limits, and native balance requirements.
45. Keep nonce-gap transactions in a future queue when otherwise acceptable.
46. Promote queued transactions when gaps close and all required checks pass.
47. Reconcile pending and queued transactions after every canonical head update or reorganization.
48. Remove stale transactions and revalidate transactions affected by changed account balances or fee conditions.
49. Treat contract execution failures separately from basic txpool admission: a transaction can be eligible for a block and still revert.
50. Never treat a node's pending pool as consensus state; the canonical chain is authoritative.

Reference: [Geth txpool API documentation](https://geth.ethereum.org/docs/interacting-with-geth/rpc/ns-txpool) and [Geth txpool configuration](https://geth.ethereum.org/docs/fundamentals/command-line-options).

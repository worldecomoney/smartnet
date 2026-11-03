# How validator picks transactions from txpool:

One pass over the txpool, classify each transaction as encountered, collect eligible candidates, and stop when the block limits are reached or the pool is exhausted.

## Suggested validator selection algorithm

### 1. Scan once

For each transaction encountered:

1. Perform basic validation and check sender/nonce eligibility.
2. Classify using transaction fields and cached contract metadata where available.
3. Apply the rules below.
4. Skip transactions that don't qualify or whose category has reached its limit.
5. Stop when all hard limits are reached or the pool is exhausted.

### 2. Selection rules

| Category             | Selection                                                                       | Limit            |
| -------------------- | ------------------------------------------------------------------------------- | ---------------- |
| Native transfers     | Accept in encounter order; approximate FIFO if the pool iteration order permits | 64              |
| ERC-20 transfers     | Accept in encounter order; no sorting, no fee-ranking pass                      | 32               |
| Contract creation    | Accept the first eligible one encountered                                       | 1                |
| NFT transactions     | Accept the first eligible one encountered                                       | 2                |
| Other contract calls | Skip for now, or define a separate quota                                        | Policy-dependent |

This is deliberately not strict FIFO or Geth-style fee ordering. Both would require additional ordering logic or data structures that you're trying to avoid.

### 3. Important implementation details

- Pool iteration order: If the txpool exposes an iterator, use it directly. Do not copy the pool into a sorted slice just to establish ordering. The observed order may not be stable or meaningful.
- Classification cost: Check `to == nil` first for contract creation. For other transactions, use cached contract metadata and calldata selectors as classification hints. Unknown contracts should fall into the general-contract category rather than being assumed to be ERC-20 or NFT.
- Nonce dependencies: A transaction with a future nonce cannot be included until preceding transactions from that sender are included. Skip it for this block, but don't necessarily remove it from the pool.
- Hard limits: Stop scanning early only when every required category is full. If one category fills early, continue scanning for candidates in the remaining categories.
- Execution validity: Basic validation during scanning is not a substitute for execution against the evolving block state.

Yes, we want to priotorize older transactions. Nonce ordering: FIFO must mean oldest eligible transaction, not blindly newest transaction. A sender's nonce \(n+1\) cannot be included before nonce \(n\).

# Total txs one seal pass processes: the pool snapshot is truncated to the oldest N
# pending txs (pool arrival, FIFO) before the state prefetch and the selection read it.
# Optional — absent/blank keeps 512, 0 is rejected. Per-class narrowing (native / heavy /
# …) comes later; the per-class caps below already bound what enters a block.
TXPOOL_VAL_MAX_TXS = 512

# Per-class caps: what each class may contribute to one sealed block (native transfer /
# ERC-20 transfer / contract create / NFT). Absent or blank = the defaults below; 0 is
# rejected (a zero cap would silently select nothing for that class).
MAX_NATIVE_PER_BLOCK = 64
MAX_ERC_20_PER_BLOCK = 32
MAX_CONTRACTS_PER_BLOCK = 1
MAX_NFT_PER_BLOCK = 2

So, validator run for loop to read any TXPOOL_VAL_MAX_TXS or until txpool is empty, inside this loop all processing happens, only one loop.
This loop results to proposaed bclock or nothing.

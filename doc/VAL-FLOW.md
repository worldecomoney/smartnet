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

# All three TXPOOL_* keys are node-owned config (every node parses/pins them):
# TXPOOL_QUEUED_TTL_SECS = 600
# TXPOOL_PENDING_TTL_SECS = 0

# Validator's own pick count: at most N txs from the snapshot it fetches are picked for
# one seal pass (oldest first, FIFO) before the state prefetch and the selection read it.
# The node has NO inclusion criteria — it serves the full pool, uncapped by this key;
# the validator alone decides which tx to include or skip. Config location is node-side,
# but the count and its application remain the validator's.
# Optional — absent/blank keeps 512, 0 is rejected.
TXPOOL_VAL_MAX_TXS = 512

# Per-class caps: what each class may contribute to one sealed block (native transfer /
# ERC-20 transfer / contract create / NFT). Absent or blank = the defaults below; 0 is
# rejected (a zero cap would silently select nothing for that class).
MAX_NATIVE_PER_BLOCK = 64
MAX_ERC_20_PER_BLOCK = 32
MAX_CONTRACTS_PER_BLOCK = 1
MAX_NFT_PER_BLOCK = 2

So, validator runs a loop over its fetched pool snapshot up to TXPOOL_VAL_MAX_TXS (or until the pool is empty); inside this loop all processing happens, only one loop. The node never pre-filters or pre-caps that set — it has no inclusion criteria.

Pool cleansing (TXPOOL_QUEUED_TTL_SECS / TXPOOL_PENDING_TTL_SECS) applies to every tx regardless of origin. SmartNet has no P2P: mesh delivers rumors through the node's own RPC semantics, so the pool's local/external label carries no meaning and is not consulted by the TTL sweep.
This loop results to proposaed bclock or nothing.

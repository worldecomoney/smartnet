# WHAT WE ARE

- A separate, private, open network derived from Ethereum — not committed to Ethereum legacy.
- We support a subset of the EVM: native txs, ERC-20, contract deploy, and basic DeFi. A few popular web3 wallets, that's it.
- We have Istanbul legacy. We have separated layers for RLP, RPC, and storage. We not do fork's. That makes us different from Ethereum.
- calibri-lite supports QUORUM_THRESHOLD_INTERIM" threshold=1, or calibri_lite{quorum: 1} meaning we can go with one single validator.. good for bootstrap

# WHAT WE ARE NOT

- Not Ethereum.
- SmartNet will fail on real Ethereum testnet/mainnet, we not support p2p/kad, forks, we use fixed fees for coins, etc,
- We do not follow any Ethereum path, legacy, or convention. We are 100% separated.

# WHAT IS REMOVED BY DESIGN AND MUST NEVER BE RESTORED OR ASSUMED AS AN ISSUE:

- P2P and KAD, NAT traversal, replaced by Mesh
- JWT and Auth/Secure RPC, replaced by Mesh protocol
- MDBX and RocksDB, replaced by Fjall and FileSystem
- BOA, js stuff, no need to be replaced
- WebSocket RPC Protocol, changed with WebSocket Mesh Protocol
- Ethereum network Fork and legacy support, most of it is gone
- there's more
- All Cities like Istanbul, London, etc.. is gone, now it's all Mars! Support is mixed.

# 1 - MUST KEEP AS IS — NEVER TOUCH !!!

- EVM and executor functionality !
- Smart-contract core !
- ERC-20 token EVM behavior !
- ERC-20 token fees !
- DeFi V2 compatibility !
- **Never**: touch `eth_*` RPC names , tx types 0x0/0x2, MetaMask behavior.
- Fixed fees for native ETH like coin transfers and validator queue for natives
- Keep as is ERC-20 market economy, validator EVM normal way.
- keep all calibri-lite as is unless explicit permission is given to work with consensus protocol.

# 2 - DO NOT

- do not over-engineer
- do not fix what is not broken
- do not replace working blocks with your "better" design
- do not invent anything, consult, play along with the team
- do not do shady/background fixes without reporting first no matter how small, collect all to todo first
- do not do stuff that was not asked from you, do not try to be just helpful, follow designer lead, play along with the team
- do not abuse build, it still does some network activity, do only when many changes is done

# Special cases, IMPORTANT:

 - this repo is very heavily modified Reth v2.1, it is not a fork of Reth per-se, it is inspired by and used as a base to
 - we follow KISS ideology, DO NOT over-engineer anything, smart things always simple
build most efficient client for a private EVM compatible network, no Ethereum legacy must be preserved, it is not an Ethereum node.
 - when you read md files make sure you check their creation time, fresh files data always outweigh the old file data in case of contradiction!
 - if/when operator asks to compress md files use younger data prevails rule to construct/compile new file.
 - commit rule: no partial commits unless absolutely necessary; after each and any meaningful green build (tests + lint pass), do one full `commit All` of the whole repo — never leave changes uncommitted across a green checkpoint.


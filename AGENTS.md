# IMPORTANT-NEVER-DO-NOT-TOUCH.md

> **STATUS: CURRENT — active design/reference**

# WHAT WE ARE

- A separate, private, open network derived from Ethereum — not committed to Ethereum legacy.
- We support a subset of the EVM: native txs, ERC-20, contract deploy, and basic DeFi. A few popular web3 wallets, that's it.
- We have Istanbul legacy. We have separated layers for RLP, RPC, and storage. We not do fork's. That makes us different from Ethereum.
- calibri-lite supports QUORUM_THRESHOLD_INTERIM" threshold=1, or calibri_lite{quorum: 1} meaning we can go with one single validator.. good for bootstrap

# WHAT WE ARE NOT

- Not Ethereum.
- SmartNet will fail on real Ethereum testnet/mainnet, we not support p2p/kad, forks, we use fixed fees for coins, etc,
- We do not follow any Ethereum path, legacy, or convention. We are 100% separated.
- We bind only to the 2 Ethereum wallets we choose to support, and to the basic DeFi rules we choose to support.
- We compile with solc. That does not make us Ethereum.

# MUST KEEP AS IS — NEVER TOUCH

- EVM functionality
- Smart-contract core
- ERC-20 token EVM behavior
- ERC-20 token fees
- DeFi V2 compatibility
- **Never**: touch `eth_*` RPC names , tx types 0x0/0x2, MetaMask behavior.
- Fixed fees for native ETH like coin transfers and validator queve for natives
- Keep as is ERC-20 market economy, validator EVM normal way.
- keep all calibri-lite as is unless explicid permission is given to work with consensus protocol.

# WHAT IS REMOVED BY DESIGN AND MUST NEVER BE RESTORED OR ASSUMED AS AN ISSUE:

- P2P and KAD, NAT traversal, replaced by Mesh
- JWT and Auth/Secure RPC, replaced by Mesh protocol
- MDBX and RocksDB, replcaed by Fjall and FileSystem
- BOA, js stuff, no need to be replaced
- WebSocket RPC Protocol, changed with WebSocket Mesh Protocol
- Ethereum network Fork and leghacy support, most of it is gone
- there's more
- All Cities like Istanbul, London, etc.. is gone, now it's all Mars! Support is mixed.

# DO NOT

- over-engenner
- fix what is not broken
- replace working blocks with your "better" design without proposal first
- do shady/background fixes without reporting first no matter how small, collect all to todo first

# Special cases, IMPORTANT:

> **STATUS: CURRENT — standing environment/working rules (offline builds, no build abuse, newer-doc-wins)**

 - we are on meterred connection, all crates are presnt, use --offline to build
 - do not abuse build, it still does some network activity, do only when many changes is done, and really need to test, otherwise ask operator
 - this repo is not yet on GH, it is a very heavily modified Reth v2.1, it is not a fork of Reth per-se, it is inspied by and used as a base to 
build most efficient client for a private EVM compatible network, no Ethereum legacy must be preserved, it is not an Ethereum node.
 - when you read md files make sure you check their creation time, fresh files data always outweight the old file data in case of contradiction!
 - if/when operator asks to compress md files use yonger data prevails rule to construct/compile new file.
 - we follow KISS ideology, DO NOT over-engeneer anything, smart thigs always simple!
 - do not invent anything, consult, we are team!
 - do bit do stuff that was not asked from you, do not try to be just helpful, follow designer lead, play along with the team!
 - commit rule: no partial commits unless absolutely necessary; after each and any meaningful green build (tests + lint pass), do one full `commit All` of the whole repo — never leave changes uncommitted across a green checkpoint.


# Features we expect already implemented here, including but not limited to!

 - 2 DataBase systems, 1 - FileSystem, for now, we decide to keep immutable data in files, shards are wery well optimized so it works, 2 - Fjall - LSM/KV database, to store rest, like state, hot window, some bas esettings

//! Closed-list all-to-all mesh — `SmartNet`'s devp2p/kademlia replacement.
//!
//! The mesh is deliberately small and binary:
//!
//! - **Peer list**: a flat JSONL file (`~/.smartnet/mainnet.jsonl`), one peer per line, re-read on
//!   every spread so operators can hot-reload membership. No discovery, no strangers — the file
//!   *is* the trust list; every connection must pass the Layer-2 identity handshake against it.
//! - **[`protocol`]**: one binary envelope (`version`, `opcode`, `length`) and the stable v1 opcode
//!   registry (control `PING`/`PONG_OK`/`PING_OK`, `HELLO_PING`/`HELLO_PONG`,
//!   `BOOTSTRAP_PEERS_PING`/`PONG`, sync status/fetch, block and tx announce dialogs).
//! - **[`identity`]**: the signed `HELLO` handshake — proves control of an allowlisted Ethereum
//!   key in one round trip; the responder keeps a bounded nonce replay cache.
//! - **[`link`]**: one persistent `WebSocket` per peer, reused for every protocol message. The link
//!   connects once (handshake first) with bounded backoff, keeps the line alive with `PING`/`PONG_OK`,
//!   and reconnects when it drops — no connect-and-disconnect per message, no UDP, no secondary
//!   transport.
//! - **[`spread`]**: on a local event the origin queues the rumor on each target's link and waits
//!   for delivery acks.
//! - **[`ws`]**: the only transport. Binary frames over `ws://ip:port/mesh`; text/JSON frames,
//!   wrong versions, unknown opcodes, and oversized frames are rejected.
//! - **[`seen`] / [`store`]**: duplicate suppression plus a short-lived copy of object bytes so
//!   peers can fetch what was announced.
//! - **[`inject`]**: local injection + chain reads (`eth_sendRawTransaction`,
//!   `smartnet_newPayload`, tip, block RLP); this is node import, not mesh wire.
//! - **[`keystore`]**: the one shared loader that decrypts a node's identity keystore at
//!   bootstrap (headless, `PASSWORD_FILE`-only, memory-only).
//!
//! Layers:
//! - [`protocol`]: binary framing and message encoding/decoding.
//! - [`rumor`]: object kind, stable hash, and canonical bytes.
//! - [`identity`]: signed handshake, digest, verification, and the nonce replay cache.
//! - [`peers`]: tolerant JSONL peer-file parser and self-skip filtering.
//! - [`seen`]: TTL-bounded, cap-bounded "have I seen this object hash" cache.
//! - [`store`]: short-lived full-object cache for counter-requests.
//! - [`link`]: per-peer connection pool (one socket per peer, backoff, keepalive).
//! - [`ws`]: `WebSocket` server, link session, one-shot ping/bootstrap exchange.
//! - [`spread`]: fanout to the peer file's active targets over the link pool.
//! - [`inject`]: self-call of the node's local RPC to deliver a received object.

/// Local JSON-RPC injection of a received object into this node.
pub mod inject;
/// Layer-2 identity: the signed `HELLO` handshake, digest, and verification.
pub mod identity;
/// Persistent per-peer links: one `WebSocket` per peer, reused for every message.
pub mod link;
/// Peer-file (`mainnet.jsonl`) parsing and self-skip filtering.
pub mod peers;
/// The one shared node-identity keystore loader (headless, `PASSWORD_FILE`-only).
pub mod keystore;
/// Pre-authentication read budget — byte cap on a connection until its `HELLO` verifies.
pub mod preauth;
/// Binary mesh protocol v1 (envelope, opcodes, announce/get/data).
pub mod protocol;
/// The wire unit of gossip, its stable hash, and canonical bytes.
pub mod rumor;
/// TTL-bounded cache of object hashes we originated, injected, or forwarded.
pub mod seen;
/// Fanout of rumors to the peer file's active targets over the link pool.
pub mod spread;
/// Short-lived full-object cache used to answer `RUMOR_GET`.
pub mod store;
/// Node-level boot readiness: tip ≥ max reported peer tip, no pull armed.
pub mod sync_gate;
/// Binary `WebSocket` transport, server, link session, and bootstrap exchange.
pub mod ws;

//! Binary mesh protocol v1: one envelope, one opcode registry, no JSON.
//!
//! Every `WebSocket` binary message is a single frame:
//!
//! ```text
//! version: u8 (1)
//! opcode:  u8
//! length:  u32 big-endian (payload bytes)
//! payload: `length` bytes
//! ```
//!
//! The registry is grouped by role (see `doc/tmp/MESH-PAYLOAD-PROTOCOL-NEW.md`):
//! `0x00–0x0F` control (keepalive, generic acks, identity handshake, bootstrap),
//! `0x10–0x1F` application request/`PING` messages, `0x20–0x2F` application
//! response/`PONG` messages; inside the application ranges block-related messages use
//! low digits `0–7` and transaction-related messages use `8–F`. Every opcode is
//! globally unique and reserved slots stay reserved.

use alloy_primitives::{keccak256, Bytes};
use std::{fmt, time::Duration};

/// Wire protocol version.
pub const VERSION: u8 = 1;
/// Envelope header length (`version + opcode + length`).
pub const HEADER_LEN: usize = 6;
/// Default maximum encoded message size (32 MiB). An operator can raise or lower it with
/// the `MESH_MAX_MSG_MB` config key (megabytes). A sealed block's full RLP grows with the
/// txs it carries, so the ceiling must comfortably exceed the largest block a node will
/// ever gossip — 32 MiB is the shipped default and fits every block under the block gas
/// cap with margin. This is the default only: the running limit is [`Limits`], threaded
/// from the operator config.
pub const DEFAULT_MAX_MSG_MB: usize = 32;

/// Operator-tunable mesh limits, threaded from config so a memory-hungry knob is never a
/// hidden constant.
///
/// The values bound the mesh's in-memory footprint on a small (2 GB) box: the largest
/// term is `max_message_bytes × concurrent_injects` (one connection's inject pool can
/// hold up to that many bytes of payload data in flight). The shipped defaults keep that
/// product well under ~256 MiB, leaving the rest of the budget for the DB, the EVM
/// caches, and the txpool.
///
/// Override with the config keys `MESH_MAX_MSG_MB`, `MESH_INJECT_SLOTS`, and
/// `MESH_LINK_QUEUE`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Limits {
    /// Maximum encoded protocol frame size, in bytes. `MESH_MAX_MSG_MB` (MB) × 1 MiB.
    pub max_message_bytes: usize,
    /// Concurrent local injects per accepted connection. Each holds one data payload,
    /// so this multiplies the worst-case inject memory by `max_message_bytes`.
    pub concurrent_injects: usize,
    /// Per-link rumor queue depth — one peer's backlog between spreads and the wire.
    /// Bounded memory on the outbound side.
    pub link_queue_capacity: usize,
    /// Client keepalive ping cadence on a persistent link. `MESH_HEARTBEAT` (ms) —
    /// the ping fires after half of this passes with no outbound traffic.
    pub keepalive: Duration,
    /// One shared silence bound, both ends: no inbound frames for this long = dead
    /// peer (server closes, client link exits and reconnects). Derived as
    /// `keepalive + SILENCE_MARGIN` so the connection always survives at least one
    /// full ping period plus round-trip jitter before either side declares death.
    pub silence: Duration,
}

/// Default client keepalive cadence (`MESH_HEARTBEAT` absent → this): ping after ~half
/// of it passes with no outbound traffic. Fast enough to track a ~1 s (soon ~100 ms)
/// block cadence, cheap on quiet chains (two tiny frames per second per idle link).
pub const DEFAULT_KEEPALIVE_INTERVAL: Duration = Duration::from_millis(500);
/// Margin on top of the keepalive cadence for the silence bound: the connection
/// survives at least one full ping period (plus round-trip jitter) before either end
/// declares the peer dead — no accidental drops on a live link.
pub const SILENCE_MARGIN: Duration = Duration::from_millis(300);
/// Default silence bound (derived): [`DEFAULT_KEEPALIVE_INTERVAL`] + [`SILENCE_MARGIN`].
pub const DEFAULT_SILENCE_TIMEOUT: Duration = Duration::from_millis(500 + 300);

impl Default for Limits {
    fn default() -> Self {
        Self {
            max_message_bytes: DEFAULT_MAX_MSG_MB * 1024 * 1024,
            concurrent_injects: 64,
            link_queue_capacity: 1024,
            keepalive: DEFAULT_KEEPALIVE_INTERVAL,
            silence: DEFAULT_SILENCE_TIMEOUT,
        }
    }
}

impl Limits {
    /// Builds limits from the operator config key `MESH_MAX_MSG_MB` (megabytes). `None`
    /// uses [`DEFAULT_MAX_MSG_MB`]. The other knobs keep their defaults.
    pub fn with_max_msg_mb(max_msg_mb: Option<usize>) -> Self {
        let mut limits = Self::default();
        if let Some(mb) = max_msg_mb {
            limits.max_message_bytes = mb * 1024 * 1024;
        }
        limits
    }
}

/// Stable v1 opcode registry. See `doc/tmp/MESH-PAYLOAD-PROTOCOL-NEW.md`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum Opcode {
    // ── 0x00–0x0F control ──────────────────────────────────────────────
    /// Keepalive probe (empty payload).
    Ping = 0x01,
    // 0x02 reserved — the old `PONG`, superseded by `PONG_OK`.
    /// Receipt after a data-delivery `PONG` (empty payload).
    PingOk = 0x03,
    /// Generic success ack; also the keepalive answer (empty payload).
    PongOk = 0x04,
    // 0x05 reserved.
    /// Reject/terminate: sender closes the connection right after (empty payload).
    PongDrop = 0x06,
    /// Initiator identity handshake: identity + fresh nonce + signature (Layer 2).
    HelloPing = 0x07,
    /// Responder identity handshake: identity + echoed nonce + signature (Layer 2).
    HelloPong = 0x08,
    /// Peer-list request (empty payload).
    BootstrapPeersPing = 0x09,
    /// Peer-list response: `list_hash` + records.
    BootstrapPeersPong = 0x0A,
    // 0x0B–0x0F reserved (control expansion).
    // ── 0x10–0x1F application requests ─────────────────────────────────
    /// "What is your last block?" — carries the sender's own tip as a hint.
    SyncBlockPing = 0x10,
    /// Fetch one full block by number.
    SyncBlockRlpPing = 0x11,
    /// Announce a block by number (not hash).
    RumorBlocksPing = 0x12,
    // 0x13–0x17 reserved (block expansion).
    /// Announce a transaction by hash.
    RumorTxPing = 0x18,
    /// Request a transaction by hash.
    RumorTxRequestPing = 0x19,
    // 0x1A–0x1F reserved (tx expansion).
    // ── 0x20–0x2F application responses ────────────────────────────────
    /// "My last block is N."
    SyncBlockPong = 0x20,
    /// Full block RLP for a requested number.
    SyncBlockRlpPong = 0x21,
    // 0x22 reserved (`RUMOR_BLOCKS_PONG` slot — acks use `PONG_OK`).
    // 0x23–0x27 reserved (block expansion).
    // 0x28 reserved (`RUMOR_TX_PONG` slot — acks use `PONG_OK`).
    /// Requested transaction bytes.
    RumorTxRequestPong = 0x29,
    // 0x2A–0x2F reserved (tx expansion).
}

impl Opcode {
    /// Parses a wire opcode.
    pub const fn from_u8(value: u8) -> Result<Self, ProtocolError> {
        match value {
            0x01 => Ok(Self::Ping),
            0x03 => Ok(Self::PingOk),
            0x04 => Ok(Self::PongOk),
            0x06 => Ok(Self::PongDrop),
            0x07 => Ok(Self::HelloPing),
            0x08 => Ok(Self::HelloPong),
            0x09 => Ok(Self::BootstrapPeersPing),
            0x0A => Ok(Self::BootstrapPeersPong),
            0x10 => Ok(Self::SyncBlockPing),
            0x11 => Ok(Self::SyncBlockRlpPing),
            0x12 => Ok(Self::RumorBlocksPing),
            0x18 => Ok(Self::RumorTxPing),
            0x19 => Ok(Self::RumorTxRequestPing),
            0x20 => Ok(Self::SyncBlockPong),
            0x21 => Ok(Self::SyncBlockRlpPong),
            0x29 => Ok(Self::RumorTxRequestPong),
            other => Err(ProtocolError::UnknownOpcode(other)),
        }
    }
}

/// Fixed binary payload of `HELLO_PING` (`0x07`) / `HELLO_PONG` (`0x08`) — Layer 2
/// identity handshake. Both opcodes carry the identical 178-byte layout; the opcode
/// (in the envelope header, not the payload) says which direction it is.
///
/// Layout (178 bytes, fixed): `version(1) ‖ network_id(8) ‖ initiator(20) ‖ responder(20)
/// ‖ i_nonce(32) ‖ r_nonce(32) ‖ signature(65)`. Fields are zeroed per side as documented
/// in `identity::hello_digest`; the signature covers the domain-separated digest, not this
/// raw payload.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HelloMessage {
    /// Protocol version (must equal [`VERSION`]).
    pub version: u8,
    /// Network / chain id the sender is on.
    pub network_id: u64,
    /// Initiator Ethereum address (always present).
    pub initiator: [u8; 20],
    /// Responder Ethereum address (zeroed in `HELLO_PING`).
    pub responder: [u8; 20],
    /// Initiator nonce (fresh in `HELLO_PING`; echoed verbatim in `HELLO_PONG`).
    pub initiator_nonce: [u8; 32],
    /// Responder nonce (zeroed in `HELLO_PING`; fresh in `HELLO_PONG`).
    pub responder_nonce: [u8; 32],
    /// 65-byte recoverable secp256k1 signature `r ‖ s ‖ v` (v ∈ {0,1}).
    pub signature: [u8; 65],
}

impl HelloMessage {
    /// Fixed payload length in bytes.
    pub const LEN: usize = 1 + 8 + 20 + 20 + 32 + 32 + 65;

    fn encode(&self, out: &mut Vec<u8>) {
        out.push(self.version);
        out.extend_from_slice(&self.network_id.to_be_bytes());
        out.extend_from_slice(&self.initiator);
        out.extend_from_slice(&self.responder);
        out.extend_from_slice(&self.initiator_nonce);
        out.extend_from_slice(&self.responder_nonce);
        out.extend_from_slice(&self.signature);
    }

    fn decode(payload: &[u8]) -> Result<Self, ProtocolError> {
        if payload.len() != Self::LEN {
            return Err(ProtocolError::Malformed("HELLO payload must be 178 bytes"));
        }
        let version = payload[0];
        let network_id = u64::from_be_bytes(payload[1..9].try_into().expect("8 bytes"));
        let mut initiator = [0u8; 20];
        initiator.copy_from_slice(&payload[9..29]);
        let mut responder = [0u8; 20];
        responder.copy_from_slice(&payload[29..49]);
        let mut initiator_nonce = [0u8; 32];
        initiator_nonce.copy_from_slice(&payload[49..81]);
        let mut responder_nonce = [0u8; 32];
        responder_nonce.copy_from_slice(&payload[81..113]);
        let mut signature = [0u8; 65];
        signature.copy_from_slice(&payload[113..178]);
        Ok(Self {
            version,
            network_id,
            initiator,
            responder,
            initiator_nonce,
            responder_nonce,
            signature,
        })
    }
}

/// One binary mesh message.
///
/// Dialogs are stateless and self-describing: `PING` → `PONG` (optionally closed by
/// `PING_OK` after a data-delivery `PONG`). Control-range messages are always empty;
/// payload messages carry fixed-width fields plus optional bytes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Message {
    // ── Layer 1 control (empty payloads) ───────────────────────────────
    /// Keepalive probe.
    Ping,
    /// Receipt after a data-delivery `PONG` (terminates that dialog).
    PingOk,
    /// Generic success ack; also the keepalive answer.
    PongOk,
    /// Reject/terminate — the sender closes the connection right after.
    PongDrop,
    // ── Layer 2 identity ───────────────────────────────────────────────
    /// Initiator identity handshake (fresh nonce).
    HelloPing(HelloMessage),
    /// Responder identity handshake (echoed nonce + own fresh nonce).
    HelloPong(HelloMessage),
    // ── Layer 3 bootstrap ──────────────────────────────────────────────
    /// Peer-list request (empty payload).
    BootstrapPeersPing,
    /// Peer-list response; the requester compares it against its own canonical list
    /// and keeps it only when identical.
    BootstrapPeersPong {
        /// Canonical hash of the enclosed records.
        list_hash: [u8; 32],
        /// The peer list.
        peers: Vec<PeerRecord>,
    },
    // ── Layer 4 sync ───────────────────────────────────────────────────
    /// "What is your last block?" — carries the sender's own tip as a hint.
    SyncBlockPing {
        /// Sender's current canonical tip (0 = genesis only).
        last_block: u64,
    },
    /// "My last block is N."
    SyncBlockPong {
        /// Responder's current canonical tip.
        last_block: u64,
    },
    /// Fetch one full block by number.
    SyncBlockRlpPing {
        /// Block height to fetch.
        block_nr: u64,
    },
    /// Full block RLP for a requested number.
    SyncBlockRlpPong {
        /// The block height this payload answers.
        block_nr: u64,
        /// Full block RLP.
        block: Bytes,
    },
    // ── Layer 5 block rumor ────────────────────────────────────────────
    /// Announce a block by number (not hash). Answer: `PONG_OK` or
    /// `SyncBlockRlpPing` (backwards fetch allowed, no mode switch).
    RumorBlocksPing {
        /// Announced block height.
        block_nr: u64,
    },
    // ── Layer 6 tx rumor ───────────────────────────────────────────────
    /// Announce a transaction by hash. Answer: `PONG_OK` or
    /// [`Message::RumorTxRequestPing`].
    RumorTxPing {
        /// Announced transaction hash.
        tx_hash: [u8; 32],
    },
    /// "Send me that tx."
    RumorTxRequestPing {
        /// Requested transaction hash.
        tx_hash: [u8; 32],
    },
    /// "txHash sent" — requested transaction bytes; closed by `PingOk`.
    RumorTxRequestPong {
        /// The transaction hash this payload answers.
        tx_hash: [u8; 32],
        /// Full transaction bytes (EIP-2718 envelope).
        tx: Bytes,
    },
}

/// Why a frame could not be encoded or decoded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProtocolError {
    /// The frame is shorter than the fixed envelope.
    TooShort,
    /// The envelope names an unsupported protocol version.
    UnsupportedVersion(u8),
    /// The opcode is not in the v1 registry.
    UnknownOpcode(u8),
    /// The object kind is not in the v1 registry.
    UnknownKind(u8),
    /// The declared payload exceeds the configured [`Limits::max_message_bytes`].
    TooLarge(usize),
    /// The declared payload length does not match the frame.
    LengthMismatch {
        /// Bytes named by the envelope header.
        declared: usize,
        /// Bytes actually present in the frame.
        actual: usize,
    },
    /// A field is truncated or has an invalid length.
    Malformed(&'static str),
    /// Bytes remain after the message body.
    TrailingBytes,
    /// A string field is not valid UTF-8.
    Utf8,
}

impl fmt::Display for ProtocolError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::TooShort => f.write_str("frame shorter than the protocol header"),
            Self::UnsupportedVersion(version) => {
                write!(f, "unsupported protocol version {version}")
            }
            Self::UnknownOpcode(opcode) => write!(f, "unknown opcode 0x{opcode:02x}"),
            Self::UnknownKind(kind) => write!(f, "unknown object kind {kind}"),
            Self::TooLarge(size) => write!(f, "message too large: {size} bytes"),
            Self::LengthMismatch { declared, actual } => {
                write!(f, "declared payload {declared} bytes, frame carries {actual}")
            }
            Self::Malformed(reason) => write!(f, "malformed message: {reason}"),
            Self::TrailingBytes => f.write_str("trailing bytes after message body"),
            Self::Utf8 => f.write_str("string field is not UTF-8"),
        }
    }
}

impl std::error::Error for ProtocolError {}

impl Message {
    /// Encodes against the default [`Limits`] ceiling (used by tests and call sites that
    /// have no operator override in hand).
    pub fn encode(&self) -> Result<Vec<u8>, ProtocolError> {
        self.encode_with_limit(Limits::default().max_message_bytes)
    }

    /// Decodes against the default [`Limits`] ceiling.
    pub fn decode(frame: &[u8]) -> Result<Self, ProtocolError> {
        Self::decode_with_limit(frame, Limits::default().max_message_bytes)
    }

    /// Encodes the message as one binary frame, rejecting a payload over `limit` bytes.
    pub fn encode_with_limit(&self, limit: usize) -> Result<Vec<u8>, ProtocolError> {
        let mut payload = Vec::new();
        let opcode = match self {
            Self::Ping => Opcode::Ping,
            Self::PingOk => Opcode::PingOk,
            Self::PongOk => Opcode::PongOk,
            Self::PongDrop => Opcode::PongDrop,
            Self::HelloPing(hello) => {
                hello.encode(&mut payload);
                Opcode::HelloPing
            }
            Self::HelloPong(hello) => {
                hello.encode(&mut payload);
                Opcode::HelloPong
            }
            Self::BootstrapPeersPing => Opcode::BootstrapPeersPing,
            Self::BootstrapPeersPong { list_hash, peers } => {
                let records = encode_peer_records(peers)?;
                let count = u16::try_from(peers.len())
                    .map_err(|_| ProtocolError::Malformed("too many peers"))?;
                payload.extend_from_slice(list_hash);
                payload.extend_from_slice(&count.to_be_bytes());
                payload.extend_from_slice(&records);
                Opcode::BootstrapPeersPong
            }
            Self::SyncBlockPing { last_block } => {
                payload.extend_from_slice(&last_block.to_be_bytes());
                Opcode::SyncBlockPing
            }
            Self::SyncBlockPong { last_block } => {
                payload.extend_from_slice(&last_block.to_be_bytes());
                Opcode::SyncBlockPong
            }
            Self::SyncBlockRlpPing { block_nr } => {
                payload.extend_from_slice(&block_nr.to_be_bytes());
                Opcode::SyncBlockRlpPing
            }
            Self::SyncBlockRlpPong { block_nr, block } => {
                payload.extend_from_slice(&block_nr.to_be_bytes());
                payload.extend_from_slice(block);
                Opcode::SyncBlockRlpPong
            }
            Self::RumorBlocksPing { block_nr } => {
                payload.extend_from_slice(&block_nr.to_be_bytes());
                Opcode::RumorBlocksPing
            }
            Self::RumorTxPing { tx_hash } => {
                payload.extend_from_slice(tx_hash);
                Opcode::RumorTxPing
            }
            Self::RumorTxRequestPing { tx_hash } => {
                payload.extend_from_slice(tx_hash);
                Opcode::RumorTxRequestPing
            }
            Self::RumorTxRequestPong { tx_hash, tx } => {
                payload.extend_from_slice(tx_hash);
                payload.extend_from_slice(tx);
                Opcode::RumorTxRequestPong
            }
        };
        if payload.len() > limit {
            return Err(ProtocolError::TooLarge(payload.len()));
        }

        let mut frame = Vec::with_capacity(HEADER_LEN + payload.len());
        frame.push(VERSION);
        frame.push(opcode as u8);
        frame.extend_from_slice(&(payload.len() as u32).to_be_bytes());
        frame.extend_from_slice(&payload);
        Ok(frame)
    }

    /// Decodes exactly one binary frame against `limit` bytes. Trailing bytes are rejected.
    pub fn decode_with_limit(frame: &[u8], limit: usize) -> Result<Self, ProtocolError> {
        if frame.len() < HEADER_LEN {
            return Err(ProtocolError::TooShort);
        }
        let version = frame[0];
        if version != VERSION {
            return Err(ProtocolError::UnsupportedVersion(version));
        }
        let opcode = Opcode::from_u8(frame[1])?;
        let declared = u32::from_be_bytes([frame[2], frame[3], frame[4], frame[5]]) as usize;
        let actual = frame.len() - HEADER_LEN;
        if declared != actual {
            return Err(ProtocolError::LengthMismatch { declared, actual });
        }
        if declared > limit {
            return Err(ProtocolError::TooLarge(declared));
        }
        let payload = &frame[HEADER_LEN..];

        match opcode {
            Opcode::Ping => {
                Self::require_empty(payload, "PING payload must be empty")?;
                Ok(Self::Ping)
            }
            Opcode::PingOk => {
                Self::require_empty(payload, "PING_OK payload must be empty")?;
                Ok(Self::PingOk)
            }
            Opcode::PongOk => {
                Self::require_empty(payload, "PONG_OK payload must be empty")?;
                Ok(Self::PongOk)
            }
            Opcode::PongDrop => {
                Self::require_empty(payload, "PONG_DROP payload must be empty")?;
                Ok(Self::PongDrop)
            }
            Opcode::HelloPing => Ok(Self::HelloPing(HelloMessage::decode(payload)?)),
            Opcode::HelloPong => Ok(Self::HelloPong(HelloMessage::decode(payload)?)),
            Opcode::BootstrapPeersPing => {
                Self::require_empty(payload, "BOOTSTRAP_PEERS_PING payload must be empty")?;
                Ok(Self::BootstrapPeersPing)
            }
            Opcode::BootstrapPeersPong => {
                if payload.len() < 34 {
                    return Err(ProtocolError::Malformed("bootstrap response is truncated"));
                }
                let mut list_hash = [0u8; 32];
                list_hash.copy_from_slice(&payload[..32]);
                let count = u16::from_be_bytes([payload[32], payload[33]]) as usize;
                let mut rest = &payload[34..];
                let mut peers = Vec::with_capacity(count);
                for _ in 0..count {
                    let (peer, remaining) = decode_peer_record(rest)?;
                    peers.push(peer);
                    rest = remaining;
                }
                if !rest.is_empty() {
                    return Err(ProtocolError::TrailingBytes);
                }
                Ok(Self::BootstrapPeersPong { list_hash, peers })
            }
            Opcode::SyncBlockPing => Ok(Self::SyncBlockPing {
                last_block: take_u64(payload, "SYNC_BLOCK_PING payload must be 8 bytes")?,
            }),
            Opcode::SyncBlockPong => Ok(Self::SyncBlockPong {
                last_block: take_u64(payload, "SYNC_BLOCK_PONG payload must be 8 bytes")?,
            }),
            Opcode::SyncBlockRlpPing => Ok(Self::SyncBlockRlpPing {
                block_nr: take_u64(payload, "SYNC_BLOCK_RLP_PING payload must be 8 bytes")?,
            }),
            Opcode::SyncBlockRlpPong => {
                if payload.len() <= 8 {
                    return Err(ProtocolError::Malformed(
                        "SYNC_BLOCK_RLP_PONG missing block RLP",
                    ));
                }
                let block_nr = u64::from_be_bytes(payload[..8].try_into().expect("8 bytes"));
                Ok(Self::SyncBlockRlpPong {
                    block_nr,
                    block: Bytes::copy_from_slice(&payload[8..]),
                })
            }
            Opcode::RumorBlocksPing => Ok(Self::RumorBlocksPing {
                block_nr: take_u64(payload, "RUMOR_BLOCKS_PING payload must be 8 bytes")?,
            }),
            Opcode::RumorTxPing => Ok(Self::RumorTxPing {
                tx_hash: take_32(payload, "RUMOR_TX_PING payload must be 32 bytes")?,
            }),
            Opcode::RumorTxRequestPing => Ok(Self::RumorTxRequestPing {
                tx_hash: take_32(payload, "RUMOR_TX_REQUEST_PING payload must be 32 bytes")?,
            }),
            Opcode::RumorTxRequestPong => {
                if payload.len() <= 32 {
                    return Err(ProtocolError::Malformed(
                        "RUMOR_TX_REQUEST_PONG missing tx bytes",
                    ));
                }
                let mut tx_hash = [0u8; 32];
                tx_hash.copy_from_slice(&payload[..32]);
                Ok(Self::RumorTxRequestPong {
                    tx_hash,
                    tx: Bytes::copy_from_slice(&payload[32..]),
                })
            }
        }
    }

    /// Enforces the empty-payload rule for the control opcodes.
    const fn require_empty(payload: &[u8], error: &'static str) -> Result<(), ProtocolError> {
        if !payload.is_empty() {
            return Err(ProtocolError::Malformed(error));
        }
        Ok(())
    }
}

fn take_u64(payload: &[u8], error: &'static str) -> Result<u64, ProtocolError> {
    if payload.len() != 8 {
        return Err(ProtocolError::Malformed(error));
    }
    Ok(u64::from_be_bytes(payload.try_into().expect("8 bytes")))
}

const fn take_32(payload: &[u8], error: &'static str) -> Result<[u8; 32], ProtocolError> {
    if payload.len() != 32 {
        return Err(ProtocolError::Malformed(error));
    }
    let mut out = [0u8; 32];
    out.copy_from_slice(payload);
    Ok(out)
}

/// Encodes peer records in canonical order for hashing and bootstrap delivery.
pub fn encode_peer_records(peers: &[PeerRecord]) -> Result<Vec<u8>, ProtocolError> {
    let mut out = Vec::new();
    for peer in peers {
        let ip = peer.ip.as_bytes();
        let key = peer.key_id.as_bytes();
        let ip_len =
            u8::try_from(ip.len()).map_err(|_| ProtocolError::Malformed("peer ip too long"))?;
        let key_len =
            u8::try_from(key.len()).map_err(|_| ProtocolError::Malformed("peer key too long"))?;
        out.push(ip_len);
        out.extend_from_slice(ip);
        out.extend_from_slice(&peer.port.to_be_bytes());
        let flags = u8::from(peer.active) | (u8::from(peer.valid) << 1);
        out.push(flags);
        out.push(key_len);
        out.extend_from_slice(key);
    }
    Ok(out)
}

/// Canonical hash of a peer list.
pub fn peer_list_hash(peers: &[PeerRecord]) -> Result<[u8; 32], ProtocolError> {
    Ok(keccak256(encode_peer_records(peers)?).0)
}

/// One peer in a bootstrap response. `url` is intentionally not carried: it is unused.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PeerRecord {
    /// Free-form operator label.
    pub key_id: String,
    /// Host/IP the peer listens on.
    pub ip: String,
    /// `WebSocket` mesh port.
    pub port: u16,
    /// Whether the peer accepts rumors.
    pub active: bool,
    /// Whether the peer is still a valid mesh member.
    pub valid: bool,
}

fn decode_peer_record(input: &[u8]) -> Result<(PeerRecord, &[u8]), ProtocolError> {
    let mut rest = input;
    let ip_len = take_u8(&mut rest)? as usize;
    let ip = take(&mut rest, ip_len)?;
    let port = take_u16(&mut rest)?;
    let flags = take_u8(&mut rest)?;
    if flags & !0b11 != 0 {
        return Err(ProtocolError::Malformed("unknown peer flags"));
    }
    let key_len = take_u8(&mut rest)? as usize;
    let key = take(&mut rest, key_len)?;
    let ip = std::str::from_utf8(ip).map_err(|_| ProtocolError::Utf8)?.to_string();
    let key_id = std::str::from_utf8(key).map_err(|_| ProtocolError::Utf8)?.to_string();
    Ok((PeerRecord { key_id, ip, port, active: flags & 1 != 0, valid: flags & 2 != 0 }, rest))
}

const fn take<'a>(input: &mut &'a [u8], len: usize) -> Result<&'a [u8], ProtocolError> {
    if input.len() < len {
        return Err(ProtocolError::Malformed("truncated field"));
    }
    let (head, tail) = input.split_at(len);
    *input = tail;
    Ok(head)
}

fn take_u8(input: &mut &[u8]) -> Result<u8, ProtocolError> {
    Ok(take(input, 1)?[0])
}

fn take_u16(input: &mut &[u8]) -> Result<u16, ProtocolError> {
    let bytes = take(input, 2)?;
    Ok(u16::from_be_bytes([bytes[0], bytes[1]]))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hello() -> HelloMessage {
        HelloMessage {
            version: VERSION,
            network_id: 7,
            initiator: [0x11; 20],
            responder: [0x22; 20],
            initiator_nonce: [0x33; 32],
            responder_nonce: [0x44; 32],
            signature: [0x55; 65],
        }
    }

    #[test]
    fn ping_is_a_six_byte_binary_frame() {
        assert_eq!(Message::Ping.encode().unwrap(), vec![1, 0x01, 0, 0, 0, 0]);
        assert_eq!(Message::decode(&[1, 0x01, 0, 0, 0, 0]).unwrap(), Message::Ping);
    }

    #[test]
    fn control_acks_are_empty_frames() {
        for message in [Message::PingOk, Message::PongOk, Message::PongDrop] {
            let encoded = message.encode().unwrap();
            assert_eq!(encoded.len(), HEADER_LEN);
            assert_eq!(Message::decode(&encoded).unwrap(), message);
        }
        // A non-empty control payload is a hard decode error.
        let mut frame = Message::PongOk.encode().unwrap();
        frame.extend_from_slice(&[0]);
        frame[2..6].copy_from_slice(&1u32.to_be_bytes());
        assert_eq!(
            Message::decode(&frame),
            Err(ProtocolError::Malformed("PONG_OK payload must be empty"))
        );
    }

    #[test]
    fn keepalive_dialog_is_ping_then_pong_ok() {
        // The amended registry: the liveness answer is `PONG_OK` (`0x04`), not `PONG`.
        let encoded = Message::PongOk.encode().unwrap();
        assert_eq!(encoded[1], 0x04);
        // The old `PONG` slot stays reserved.
        assert!(Opcode::from_u8(0x02).is_err());
    }

    #[test]
    fn sync_status_roundtrip_as_u64() {
        for message in [
            Message::SyncBlockPing { last_block: 0 },
            Message::SyncBlockPing { last_block: 120 },
            Message::SyncBlockPong { last_block: u64::MAX },
        ] {
            let encoded = message.encode().unwrap();
            // 6-byte envelope + 8-byte u64 payload.
            assert_eq!(encoded.len(), 14);
            assert_eq!(Message::decode(&encoded).unwrap(), message);
        }
    }

    #[test]
    fn sync_block_rlp_roundtrip_carries_number_and_bytes() {
        let message = Message::SyncBlockRlpPong {
            block_nr: 101,
            block: Bytes::from(vec![0xDE, 0xAD, 0xBE, 0xEF]),
        };
        let encoded = message.encode().unwrap();
        assert_eq!(encoded[1], Opcode::SyncBlockRlpPong as u8);
        assert_eq!(Message::decode(&encoded).unwrap(), message);

        // The request side is a bare number.
        let request = Message::SyncBlockRlpPing { block_nr: 101 };
        let encoded = request.encode().unwrap();
        assert_eq!(encoded.len(), 14);
        assert_eq!(Message::decode(&encoded).unwrap(), request);
    }

    #[test]
    fn sync_block_rlp_pong_rejects_missing_block_bytes() {
        // 8-byte number and nothing after it.
        let mut frame = Message::SyncBlockRlpPing { block_nr: 9 }.encode().unwrap();
        frame[1] = Opcode::SyncBlockRlpPong as u8;
        assert_eq!(
            Message::decode(&frame),
            Err(ProtocolError::Malformed("SYNC_BLOCK_RLP_PONG missing block RLP"))
        );
    }

    #[test]
    fn rumor_blocks_announce_is_blocknr_not_hash() {
        let message = Message::RumorBlocksPing { block_nr: 120 };
        let encoded = message.encode().unwrap();
        // 6-byte envelope + 8-byte BE u64 — no hash, no kind byte.
        assert_eq!(&encoded[..6], &[1, 0x12, 0, 0, 0, 8]);
        assert_eq!(&encoded[6..], &120u64.to_be_bytes());
        assert_eq!(Message::decode(&encoded).unwrap(), message);
    }

    #[test]
    fn tx_dialog_roundtrip() {
        let hash = [7u8; 32];
        for message in [
            Message::RumorTxPing { tx_hash: hash },
            Message::RumorTxRequestPing { tx_hash: hash },
            Message::RumorTxRequestPong { tx_hash: hash, tx: Bytes::from(vec![0, 1, 2, 0xFF]) },
        ] {
            let encoded = message.encode().unwrap();
            assert_eq!(Message::decode(&encoded).unwrap(), message);
        }
        // The request pong carries hash + bytes and must not decode when truncated to
        // the bare hash.
        let mut frame = Message::RumorTxRequestPong { tx_hash: hash, tx: Bytes::new() }
            .encode()
            .unwrap();
        // Re-encode with empty tx is invalid on the wire (decode requires > 32 bytes).
        let declared = (frame.len() - HEADER_LEN) as u32;
        frame[2..6].copy_from_slice(&declared.to_be_bytes());
        assert_eq!(
            Message::decode(&frame),
            Err(ProtocolError::Malformed("RUMOR_TX_REQUEST_PONG missing tx bytes"))
        );
    }

    #[test]
    fn hello_ping_and_pong_are_distinct_opcodes() {
        let message = hello();
        let ping = Message::HelloPing(message).encode().unwrap();
        let pong = Message::HelloPong(message).encode().unwrap();
        assert_eq!(ping[1], 0x07);
        assert_eq!(pong[1], 0x08);
        assert_eq!(Message::decode(&ping).unwrap(), Message::HelloPing(message));
        assert_eq!(Message::decode(&pong).unwrap(), Message::HelloPong(message));
        // Same payload, distinct opcode — that is the amendment's point: both handshake
        // stages are checked and must never share an opcode.
        assert_eq!(&ping[HEADER_LEN..], &pong[HEADER_LEN..]);
    }

    #[test]
    fn hello_rejects_wrong_payload_length() {
        let mut encoded = Message::HelloPing(hello()).encode().unwrap();
        encoded.pop();
        // Fix up the declared length so we exercise the HELLO-specific length check,
        // not the generic envelope LengthMismatch.
        let new_len = (encoded.len() - HEADER_LEN) as u32;
        encoded[2..6].copy_from_slice(&new_len.to_be_bytes());
        assert!(matches!(
            Message::decode(&encoded),
            Err(ProtocolError::Malformed("HELLO payload must be 178 bytes"))
        ));
    }

    #[test]
    fn opcode_registry_is_complete_and_reserved_stays_reserved() {
        for (byte, expected) in [
            (0x01, Opcode::Ping),
            (0x03, Opcode::PingOk),
            (0x04, Opcode::PongOk),
            (0x06, Opcode::PongDrop),
            (0x07, Opcode::HelloPing),
            (0x08, Opcode::HelloPong),
            (0x09, Opcode::BootstrapPeersPing),
            (0x0A, Opcode::BootstrapPeersPong),
            (0x10, Opcode::SyncBlockPing),
            (0x11, Opcode::SyncBlockRlpPing),
            (0x12, Opcode::RumorBlocksPing),
            (0x18, Opcode::RumorTxPing),
            (0x19, Opcode::RumorTxRequestPing),
            (0x20, Opcode::SyncBlockPong),
            (0x21, Opcode::SyncBlockRlpPong),
            (0x29, Opcode::RumorTxRequestPong),
        ] {
            assert_eq!(Opcode::from_u8(byte).unwrap(), expected);
            assert_eq!(expected as u8, byte);
        }
        // Reserved slots stay reserved: superseded/never-assigned control, control
        // expansion, block expansion, tx expansion, and the *_PONG slots.
        for reserved in [
            0x00, 0x02, 0x05, 0x0B, 0x0C, 0x0D, 0x0E, 0x0F, // control
            0x13, 0x14, 0x15, 0x16, 0x17, 0x1A, 0x1B, 0x1C, 0x1D, 0x1E, 0x1F, // requests
            0x22, 0x23, 0x24, 0x25, 0x26, 0x27, 0x28, // responses (block + RUMOR_*_PONG)
            0x2A, 0x2B, 0x2C, 0x2D, 0x2E, 0x2F, 0x30, 0x7F,
        ] {
            assert_eq!(Opcode::from_u8(reserved), Err(ProtocolError::UnknownOpcode(reserved)));
        }
    }

    #[test]
    fn bootstrap_roundtrip_and_hash() {
        let peers = vec![
            PeerRecord {
                key_id: "node-a".into(),
                ip: "127.0.0.1".into(),
                port: 30811,
                active: true,
                valid: true,
            },
            PeerRecord {
                key_id: "node-b".into(),
                ip: "::1".into(),
                port: 30812,
                active: true,
                valid: false,
            },
        ];
        let list_hash = peer_list_hash(&peers).unwrap();
        let message = Message::BootstrapPeersPong { list_hash, peers };
        let encoded = message.encode().unwrap();
        assert_eq!(encoded[1], Opcode::BootstrapPeersPong as u8);
        assert_eq!(Message::decode(&encoded).unwrap(), message);

        // The request is an empty frame — the old known_hash field is gone (strict
        // list equality made it redundant).
        let request = Message::BootstrapPeersPing.encode().unwrap();
        assert_eq!(request.len(), HEADER_LEN);
        assert_eq!(Message::decode(&request).unwrap(), Message::BootstrapPeersPing);
    }

    #[test]
    fn wrong_version_and_opcode_are_rejected() {
        assert_eq!(Message::decode(&[2, 1, 0, 0, 0, 0]), Err(ProtocolError::UnsupportedVersion(2)));
        assert_eq!(
            Message::decode(&[1, 0x7F, 0, 0, 0, 0]),
            Err(ProtocolError::UnknownOpcode(0x7F))
        );
        // Reserved control slots decode as unknown opcodes, not as legacy messages.
        assert_eq!(
            Message::decode(&[1, 0x02, 0, 0, 0, 0]),
            Err(ProtocolError::UnknownOpcode(0x02))
        );
    }

    #[test]
    fn length_mismatch_truncation_and_trailing_bytes_are_rejected() {
        assert_eq!(
            Message::decode(&[1, 1, 0, 0, 0, 1]),
            Err(ProtocolError::LengthMismatch { declared: 1, actual: 0 })
        );
        assert_eq!(Message::decode(&[1, 1]), Err(ProtocolError::TooShort));
        assert_eq!(
            Message::decode(&[1, 1, 0, 0, 0, 1, 0]),
            Err(ProtocolError::Malformed("PING payload must be empty"))
        );
        let mut frame = Message::Ping.encode().unwrap();
        frame.extend_from_slice(&[0]);
        frame[2..6].copy_from_slice(&1u32.to_be_bytes());
        assert_eq!(
            Message::decode(&frame),
            Err(ProtocolError::Malformed("PING payload must be empty"))
        );
        // A wrong-width u64 field is a hard error, not a silently truncated number.
        let mut frame = Message::SyncBlockPing { last_block: 1 }.encode().unwrap();
        frame.pop();
        frame[2..6].copy_from_slice(&7u32.to_be_bytes());
        assert_eq!(
            Message::decode(&frame),
            Err(ProtocolError::Malformed("SYNC_BLOCK_PING payload must be 8 bytes"))
        );
    }

    #[test]
    fn text_and_json_are_not_protocol_messages() {
        assert!(Message::decode(br#"{"kind":"tx","data":"0x01"}"#).is_err());
        assert!(Message::decode(b"\x01\x12\x00\x00\x00\x02{}").is_err());
    }
}

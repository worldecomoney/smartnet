//! THE signaling wire format (`BRIDGE-DOC.md` §9) — **one definition used by all sides**:
//! the bridge that builds a request, the chair that answers it, and the bridge that
//! matches the answer against its open record. Pure data — no EVM, no transport, no
//! policy. They cannot disagree because they cannot get this layout from anywhere else.
//!
//! A signal is an **ordinary native transaction** whose **value** discriminates it:
//!
//! | value | meaning |
//! |---|---|
//! | `0.91 WEM` | IN request (bridge-IN deposit) — the bridge's self-transfer to itself |
//! | `0.92 WEM` | OUT request (bridge-OUT withdrawal) — the bridge's self-transfer to itself |
//! | `0.01 WEM` | the chair's OUT authorization (burn confirmation) — to the bridge |
//!
//! and whose **data** is one fixed payload: [`SIGNAL_SELECTOR`] followed by eight 32-byte
//! words — `origin`, `from`, `amount`, `dec`, `token_wem`, `token_foreign`, `timestamp`,
//! `prev_block_hash`. The chair's answer carries the request's data verbatim.

use alloy_primitives::{Address, Bytes, B256, U256};
use alloc::vec::Vec;

/// The canonical signature of the payload — the eight words of §9, in order. The
/// selector is the ordinary ABI function selector of this signature.
pub const SIGNAL_SIGNATURE: &str =
    "bridgeSignal(uint256,address,uint256,uint256,address,address,uint256,bytes32)";

/// The 4-byte tag at the head of every signal's data:
/// `keccak256(SIGNAL_SIGNATURE)[..4]` — an ordinary ABI function selector, derived once
/// and pinned by the `selector_is_the_canonical_function_selector` test. (The bridge
/// itself never hashes anything; this is just the tag bytes.)
pub const SIGNAL_SELECTOR: [u8; 4] = [0x8e, 0x53, 0x22, 0x1e];

/// IN request (bridge-IN deposit): the bridge's `0.91 WEM` self-transfer to itself.
pub const IN_REQUEST_WEI: u128 = 910_000_000_000_000_000;

/// OUT request (bridge-OUT withdrawal): the bridge's `0.92 WEM` self-transfer to itself,
/// carrying the user's X.
pub const OUT_REQUEST_WEI: u128 = 920_000_000_000_000_000;

/// The chair's OUT authorization — the **burn confirmation**: `0.01 WEM` to the bridge's
/// node address. Permission to burn exactly the record's X — **never a mint**, no other
/// meaning anywhere.
pub const OUT_AUTHORIZATION_WEI: u128 = 10_000_000_000_000_000;

/// What a signal's value says it is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SignalKind {
    /// `0.91 WEM` — bridge-IN deposit request; the chair's answer mints `amount` to the
    /// bridge's node address.
    InRequest,
    /// `0.92 WEM` — bridge-OUT withdrawal request; the chair's answer authorizes burning
    /// the user's X.
    OutRequest,
    /// `0.01 WEM` — the chair's OUT authorization (burn confirmation).
    OutAuthorization,
}

/// Classifies an ordinary native transaction by its **value** alone (§9: the value
/// discriminates the signal). `None` for any other amount — ordinary payments are not
/// signals.
pub fn signal_kind(value: U256) -> Option<SignalKind> {
    match value {
        v if v == U256::from(IN_REQUEST_WEI) => Some(SignalKind::InRequest),
        v if v == U256::from(OUT_REQUEST_WEI) => Some(SignalKind::OutRequest),
        v if v == U256::from(OUT_AUTHORIZATION_WEI) => Some(SignalKind::OutAuthorization),
        _ => None,
    }
}

/// The fixed payload: the selector plus eight 32-byte words, in order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Signal {
    /// 1 · `origin` — source chain of the transfer (the foreign chain for IN, our chain
    /// for OUT).
    pub origin: u64,
    /// 2 · `from` — the transfer's sender: **and the recipient** — the delivery (IN) and
    /// the release (OUT) both pay this address.
    pub from: Address,
    /// 3 · `amount` — the destination-side amount in base units: `amountOut` for IN, the
    /// user's X (`amountIn`) for OUT.
    pub amount: U256,
    /// 4 · `dec` — the decimals `amount` is expressed in.
    pub dec: u8,
    /// 5 · `token_wem` — the contract on our chain (`Address::ZERO` for the coin).
    pub token_wem: Address,
    /// 6 · `token_foreign` — the contract on the foreign chain (`Address::ZERO` for
    /// native).
    pub token_foreign: Address,
    /// 7 · `timestamp` — unix seconds at build.
    pub timestamp: u64,
    /// 8 · `prev_block_hash` — our chain's head hash at build: the parent of the block
    /// this transaction lands in.
    pub prev_block_hash: B256,
}

impl Signal {
    /// Encodes the signal: [`SIGNAL_SELECTOR`] + the eight words, in order. Deterministic
    /// — the chair's answer carries these bytes **verbatim**.
    pub fn encode(&self) -> Bytes {
        let mut out = Vec::with_capacity(4 + 8 * 32);
        out.extend_from_slice(&SIGNAL_SELECTOR);
        out.extend_from_slice(&word_u64(self.origin));
        out.extend_from_slice(&word_address(self.from));
        out.extend_from_slice(&self.amount.to_be_bytes::<32>());
        out.extend_from_slice(&word_u64(u64::from(self.dec)));
        out.extend_from_slice(&word_address(self.token_wem));
        out.extend_from_slice(&word_address(self.token_foreign));
        out.extend_from_slice(&word_u64(self.timestamp));
        out.extend_from_slice(self.prev_block_hash.as_slice());
        out.into()
    }

    /// Strict decode — `None` on anything that is not exactly one well-formed signal:
    /// wrong length, wrong selector, a dirty address word (high 12 bytes set), a dirty
    /// `u64` word, or `dec` that does not fit. Garbage never becomes a signal.
    pub fn decode(data: &[u8]) -> Option<Self> {
        if data.len() != 4 + 8 * 32 {
            return None;
        }
        if data[0..4] != SIGNAL_SELECTOR {
            return None;
        }
        let word = |i: usize| &data[4 + i * 32..4 + (i + 1) * 32];

        Some(Self {
            origin: word_to_u64(word(0))?,
            from: word_to_address(word(1))?,
            amount: U256::from_be_slice(word(2)),
            dec: u8::try_from(word_to_u64(word(3))?).ok()?,
            token_wem: word_to_address(word(4))?,
            token_foreign: word_to_address(word(5))?,
            timestamp: word_to_u64(word(6))?,
            prev_block_hash: B256::from_slice(word(7)),
        })
    }
}

fn word_u64(v: u64) -> [u8; 32] {
    U256::from(v).to_be_bytes::<32>()
}

fn word_address(a: Address) -> [u8; 32] {
    let mut w = [0u8; 32];
    w[12..].copy_from_slice(a.as_slice());
    w
}

fn word_to_u64(w: &[u8]) -> Option<u64> {
    if w.len() != 32 || w[..24].iter().any(|b| *b != 0) {
        return None;
    }
    Some(u64::from_be_bytes(w[24..32].try_into().ok()?))
}

fn word_to_address(w: &[u8]) -> Option<Address> {
    if w.len() != 32 || w[..12].iter().any(|b| *b != 0) {
        return None;
    }
    Some(Address::from_slice(&w[12..]))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> Signal {
        Signal {
            origin: 56,
            from: Address::repeat_byte(0xAA),
            amount: U256::from(1_000_000_000_000_000_000u64),
            dec: 18,
            token_wem: Address::ZERO,
            token_foreign: Address::repeat_byte(0xCC),
            timestamp: 1_794_700_000,
            prev_block_hash: B256::repeat_byte(0x77),
        }
    }

    #[test]
    fn selector_is_the_canonical_function_selector() {
        assert_eq!(
            SIGNAL_SELECTOR,
            alloy_primitives::keccak256(SIGNAL_SIGNATURE)[..4],
            "the selector must be keccak256(SIGNAL_SIGNATURE)[..4]"
        );
    }

    #[test]
    fn round_trip_is_exact() {
        let s = sample();
        let decoded = Signal::decode(&s.encode()).expect("the encoded signal must decode");
        assert_eq!(decoded, s);
    }

    #[test]
    fn layout_is_selector_plus_eight_words_in_order() {
        let s = sample();
        let data = s.encode();

        assert_eq!(data.len(), 4 + 8 * 32);
        assert_eq!(&data[0..4], &SIGNAL_SELECTOR, "the selector heads the payload");
        // word 2 (`from`) is a left-padded 20-byte address
        assert_eq!(&data[4 + 32..4 + 64][..12], &[0u8; 12], "address words are left-padded");
        assert_eq!(&data[4 + 32 + 12..4 + 64], Address::repeat_byte(0xAA).as_slice());
        // word 8 (`prev_block_hash`) closes the payload
        assert_eq!(&data[4 + 7 * 32..], B256::repeat_byte(0x77).as_slice());
    }

    #[test]
    fn garbage_never_becomes_a_signal() {
        let good = sample().encode();

        // wrong length
        let mut short = good.to_vec();
        short.pop();
        assert!(Signal::decode(&short).is_none());
        let mut long = good.to_vec();
        long.push(0);
        assert!(Signal::decode(&long).is_none());

        // wrong selector
        let mut wrong_tag = good.to_vec();
        wrong_tag[0] = b'X';
        assert!(Signal::decode(&wrong_tag).is_none());

        // dirty address word: high byte of `from`'s word set
        let mut dirty_addr = good.to_vec();
        dirty_addr[4 + 32] = 1;
        assert!(Signal::decode(&dirty_addr).is_none());

        // dirty u64 word: high byte of `origin`'s word set
        let mut dirty_u64 = good.to_vec();
        dirty_u64[4] = 1;
        assert!(Signal::decode(&dirty_u64).is_none());

        // dec does not fit a u8 (word 4 holds 511)
        let mut raw = sample().encode().to_vec();
        raw[4 + 3 * 32 + 31] = 0xFF;
        raw[4 + 3 * 32 + 30] = 0x01;
        assert!(Signal::decode(&raw).is_none());
    }

    #[test]
    fn the_three_values_classify_and_nothing_else_does() {
        assert_eq!(signal_kind(U256::from(IN_REQUEST_WEI)), Some(SignalKind::InRequest));
        assert_eq!(signal_kind(U256::from(OUT_REQUEST_WEI)), Some(SignalKind::OutRequest));
        assert_eq!(
            signal_kind(U256::from(OUT_AUTHORIZATION_WEI)),
            Some(SignalKind::OutAuthorization)
        );
        for ordinary in [U256::ZERO, U256::from(1u64), U256::from(IN_REQUEST_WEI) + U256::from(1u64)] {
            assert_eq!(signal_kind(ordinary), None, "ordinary payments are not signals");
        }
    }

    #[test]
    fn the_markers_are_exactly_0_91_0_92_and_0_01_wem() {
        let wem = |n: u128, frac: u128| n * 1_000_000_000_000_000_000 + frac * 10_000_000_000_000_000;
        assert_eq!(IN_REQUEST_WEI, wem(0, 91));
        assert_eq!(OUT_REQUEST_WEI, wem(0, 92));
        assert_eq!(OUT_AUTHORIZATION_WEI, wem(0, 1));
    }
}

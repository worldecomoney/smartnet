//! `SELECT` — deterministic, position-free selection of a chair + quorum,
//! keyed by purpose-tagged (block_hash + unix_timestamp) entropy.
//!
//! Spec: `quorum-doc.md` / drop-in source: `chair-select.md`. Contract for
//! [`select_chair_and_quorum`]:
//!   - N ≤ 16 : returns `[chair]` — length 1.
//!   - N > 16 : returns `[chair, q1, ..., qk]` with k = round(3N/16).
//!   - arr[0]   is ALWAYS the chair.
//!   - arr[1..] is the quorum (empty for N ≤ 16).
//!   - arr is score-ordered: arr[0] has the smallest score.
//!
//! The hash is Keccak-256 over a length-prefixed preimage (domain `select/v4`),
//! computed with [`alloy_primitives::keccak256`] — byte-identical to the
//! reference `sha3::Keccak256` incremental construction, no extra dependency.
//!
//! Note: addresses are compared as raw byte-strings. If case-insensitivity
//! matters (e.g. checksummed Ethereum hex), normalize before calling — see
//! [`normalize_eth`].

use alloy_primitives::{hex, keccak256, Address, B256};
use std::time::{SystemTime, UNIX_EPOCH};

// ---------------------------------------------------------------------------
// Purpose
// ---------------------------------------------------------------------------

/// Why we are selecting. Different purposes over the same entropy yield
/// independent selections.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Purpose {
    Chair,
    Bridge,
    Audit,
    /// Escape hatch for a new purpose. Still hashed, still distinct.
    Other(&'static str),
}

impl Purpose {
    fn tag(self) -> &'static [u8] {
        match self {
            Purpose::Chair => b"chair",
            Purpose::Bridge => b"bridge",
            Purpose::Audit => b"audit",
            Purpose::Other(s) => s.as_bytes(),
        }
    }
}

// ---------------------------------------------------------------------------
// Entropy
// ---------------------------------------------------------------------------

/// Full entropy: parent block hash + Unix timestamp (seconds).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Entropy {
    pub block_hash: [u8; 32],
    pub unix_ts: u64,
}

impl Entropy {
    pub fn new(block_hash: [u8; 32], unix_ts: u64) -> Self {
        Self { block_hash, unix_ts }
    }
}

/// Slot length in seconds. Every `SLOT_SECONDS`, the chair may rotate.
pub const SLOT_SECONDS: u64 = 1;

/// Read the current slot from the system clock. The ONLY approved way
/// to derive `unix_ts` at runtime — never set by an operator.
pub fn current_slot() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).expect("system clock before 1970").as_secs() /
        SLOT_SECONDS
}

/// Convenience constructor used everywhere.
pub fn chair_entropy(block_hash: [u8; 32], current_timestamp: u64) -> Entropy {
    Entropy::new(block_hash, current_timestamp)
}

// ---------------------------------------------------------------------------
// Scorers
// ---------------------------------------------------------------------------

/// Length-prefixed preimage shared by [`score`] and [`score_rehash`].
fn preimage(domain: &[u8], purpose: Purpose, e: &Entropy, addr: &str) -> Vec<u8> {
    let pt = purpose.tag();
    let mut buf = Vec::with_capacity(domain.len() + 8 + pt.len() + 32 + 8 + 8 + addr.len());
    buf.extend_from_slice(domain);
    buf.extend_from_slice(&(pt.len() as u64).to_be_bytes());
    buf.extend_from_slice(pt);
    buf.extend_from_slice(&e.block_hash);
    buf.extend_from_slice(&e.unix_ts.to_be_bytes());
    buf.extend_from_slice(&(addr.len() as u64).to_be_bytes());
    buf.extend_from_slice(addr.as_bytes());
    buf
}

#[inline]
pub fn score(purpose: Purpose, e: &Entropy, addr: &str) -> [u8; 32] {
    keccak256(preimage(b"select/v4", purpose, e, addr)).0
}

#[inline]
pub fn score_rehash(purpose: Purpose, e: &Entropy, addr: &str) -> [u8; 32] {
    keccak256(preimage(b"select/v4-rehash", purpose, e, addr)).0
}

// ---------------------------------------------------------------------------
// Canonicalization
// ---------------------------------------------------------------------------

fn canonical(addrs: &[String]) -> Vec<&String> {
    let mut set: Vec<&String> = addrs.iter().collect();
    set.sort_unstable();
    set.dedup();
    set
}

// ---------------------------------------------------------------------------
// Sizing rule
// ---------------------------------------------------------------------------

/// Number of addresses returned for a set of size `n` (after dedupe).
/// n ≤ 16 → 1 (chair only). n > 16 → 1 + round(3n/16).
pub fn committee_size(n: usize) -> usize {
    if n <= 16 {
        1
    } else {
        1 + (3 * n + 8) / 16
    }
}

// ---------------------------------------------------------------------------
// Primary API
// ---------------------------------------------------------------------------

/// Return `[chair, q1, ..., qk]` where `chair = arr[0]` and `arr[1..]` is quorum.
/// Score-ordered. Position-free. Deterministic on `(purpose, entropy, set)`.
///
/// Note: addresses are compared as raw byte-strings. If case-insensitivity
/// matters (e.g. checksummed Ethereum hex), normalize before calling — see
/// [`normalize_eth`].
pub fn select_chair_and_quorum(
    purpose: Purpose,
    entropy: &Entropy,
    addrs: &[String],
) -> Vec<String> {
    let set = canonical(addrs);
    if set.is_empty() {
        return Vec::new();
    }
    let k = committee_size(set.len());
    select_k_ordered(purpose, entropy, &set, k)
}

/// THE chair function (`get_val_chair`, former SELECT): the one place that answers
/// "who is the chair for this slot". **Pure math — no async, no I/O per call** (operator,
/// 2026-11-19).
///
/// 1. the validator registry is loaded from `validators.jsonl` **with statuses**, once
///    (first load wins; later calls answer from RAM);
/// 2. only `status: true` records take part in the calculation — a `status: false`
///    record can never become chair (fail closed: no enabled validator ⇒ `None`);
/// 3. **1-validator detour**: with exactly one enabled validator that validator IS the
///    chair for every input — no keccak, no rotation, fast;
/// 4. otherwise keccak entropy over the real `(block_hash, ts)` (quorum-doc §3.4) feeds
///    the score-ordered pick.
///
/// `purpose` is `""`/`"chair"`, `"bridge"` or `"audit"`; anything else fails closed
/// with **no chair**.
pub fn get_val_chair(block_hash: B256, ts: u64, purpose: &str) -> Option<Address> {
    use crate::registry::{default_validators_file, get_val_all, load_validators};
    use core::sync::atomic::{AtomicBool, Ordering};

    static LOADED: AtomicBool = AtomicBool::new(false);
    if !LOADED.load(Ordering::SeqCst) {
        // Step 1 — the registry, statuses included. The file is read once per process;
        // a missing/unreadable file leaves the array empty, which fails closed below.
        let _ = load_validators(&default_validators_file());
        LOADED.store(true, Ordering::SeqCst);
    }

    // Step 2 — `status: true` only (`is_enabled` is exactly `status == Some(true)`).
    let enabled: Vec<Address> =
        get_val_all().into_iter().filter(|r| r.is_enabled()).map(|r| r.address).collect();

    // Step 3 — the 1-validator math detour.
    if let [only] = enabled.as_slice() {
        return Some(*only);
    }
    if enabled.is_empty() {
        return None;
    }

    let selected = match purpose {
        "" | "chair" => Purpose::Chair,
        "bridge" => Purpose::Bridge,
        "audit" => Purpose::Audit,
        _ => return None,
    };

    // Step 4 — keccak math over the real block hash + ts.
    let keys: Vec<String> = enabled.iter().map(addr_key).collect();
    select_one(selected, &chair_entropy(block_hash.0, ts), &keys)
        .and_then(|picked| parse_addr(&picked))
}

/// THE seal gate — the one entrance both the import's range executor and mesh sync's
/// header validation check a seal against (operator, 2026-11-19: "make sure all import
/// and mesh sync both go through this same single entrance").
///
/// Exactly two things: the seal's signature recovers, and the recovered signer is the
/// SELECT chair for this slot — [`get_val_chair`], which only ever answers with a
/// `status: true` validator of `validators.jsonl`. Nothing else is decided here: chain
/// position is checked separately, and there is no fee rule and no genesis branch on
/// this gate. Returns the signer on pass; a reason naming both addresses on refuse.
pub fn check_seal(header: &alloy_consensus::Header) -> Result<Address, String> {
    let signer = crate::recover_calibri_lite_signer(header)
        .map_err(|err| format!("seal signature invalid: {err}"))?;
    let chair = get_val_chair(header.parent_hash, header.timestamp, "").ok_or_else(|| {
        "validators.jsonl holds no `status: true` validator — sealed blocks rejected          (fail closed)"
            .to_string()
    })?;
    if signer != chair {
        return Err(format!(
            "sealed by {signer}, but the chair for this slot is {chair} — rejected"
        ));
    }
    Ok(signer)
}

/// Convenience: chair for the current slot.
pub fn current_chair(purpose: Purpose, block_hash: [u8; 32], addrs: &[String]) -> Option<String> {
    let e = chair_entropy(block_hash, current_slot());
    select_chair_and_quorum(purpose, &e, addrs).into_iter().next()
}

// ---------------------------------------------------------------------------
// Internals: score-ordered pick
// ---------------------------------------------------------------------------

fn select_k_ordered(purpose: Purpose, e: &Entropy, set: &[&String], k: usize) -> Vec<String> {
    if k == 0 || set.is_empty() {
        return Vec::new();
    }
    let k = k.min(set.len());

    let mut scored: Vec<([u8; 32], &String)> =
        set.iter().map(|a| (score(purpose, e, a), *a)).collect();
    scored.sort_unstable_by(|x, y| x.0.cmp(&y.0).then(x.1.cmp(y.1)));

    // Tie at the cutoff → rehash the tied boundary block, lex fallback.
    if k < scored.len() && scored[k - 1].0 == scored[k].0 {
        let boundary = scored[k - 1].0;
        let start = scored.iter().position(|(s, _)| *s == boundary).unwrap();
        let end = scored.iter().rposition(|(s, _)| *s == boundary).unwrap() + 1;

        let mut tied: Vec<([u8; 32], &String)> =
            scored[start..end].iter().map(|(_, a)| (score_rehash(purpose, e, a), *a)).collect();
        tied.sort_unstable_by(|x, y| x.0.cmp(&y.0).then(x.1.cmp(y.1)));

        for (i, (_, a)) in tied.into_iter().enumerate() {
            scored[start + i] = (boundary, a);
        }
    }

    scored.into_iter().take(k).map(|(_, a)| a.clone()).collect()
}

// ---------------------------------------------------------------------------
// Standalone select_one (kept for compatibility)
// ---------------------------------------------------------------------------

pub fn select_one(purpose: Purpose, entropy: &Entropy, addrs: &[String]) -> Option<String> {
    select_chair_and_quorum(purpose, entropy, addrs).into_iter().next()
}

// ---------------------------------------------------------------------------
// Standalone select_k (lexicographically sorted for canonical comparison)
// ---------------------------------------------------------------------------

pub fn select_k(purpose: Purpose, entropy: &Entropy, addrs: &[String], k: usize) -> Vec<String> {
    let set = canonical(addrs);
    if k == 0 || set.is_empty() {
        return Vec::new();
    }
    let k = k.min(set.len());
    let picked = select_k_ordered(purpose, entropy, &set, k);
    let mut out = picked;
    out.sort_unstable();
    out
}

// ---------------------------------------------------------------------------
// Address helpers (canonical `0x` + lowercase hex — what SELECT hashes)
// ---------------------------------------------------------------------------

/// Canonical SELECT key for an address: `0x` + lowercase hex.
///
/// Every node must produce the SAME string for the same address, because the
/// string bytes are what the score hashes. Lowercase, prefix always present.
pub fn addr_key(addr: &Address) -> String {
    format!("0x{}", hex::encode(addr.as_slice()))
}

/// Parse a hex address (with or without `0x`, any case) back into [`Address`].
pub fn parse_addr(s: &str) -> Option<Address> {
    let s = s.trim();
    let bare = s.strip_prefix("0x").or_else(|| s.strip_prefix("0X")).unwrap_or(s);
    if bare.len() != 40 {
        return None;
    }
    let bytes = hex::decode(bare).ok()?;
    Some(Address::from_slice(&bytes))
}

/// Normalize an address string for SELECT input: trim + lowercase.
pub fn normalize_eth(s: &str) -> String {
    s.trim().to_lowercase()
}

// ---------------------------------------------------------------------------
// Typed convenience: chair over an enabled-validator set
// ---------------------------------------------------------------------------

/// Chair for `(parent_hash, unix_ts)` over the enabled-validator set.
///
/// - empty set → `None` (fail closed: no chair, nothing may seal).
/// - exactly one enabled validator → that validator directly (SELECT would return the same single
///   address for any entropy — shortcut skips the hash).
/// - otherwise → `SELECT(Purpose::Chair, entropy)[0]`, the only address authorized to propose the
///   block carrying exactly this parent hash and timestamp.
///
/// The returned chair is always a member of `enabled`.
pub fn chair_for(enabled: &[Address], parent_hash: B256, unix_ts: u64) -> Option<Address> {
    if enabled.is_empty() {
        return None;
    }
    if enabled.len() == 1 {
        return Some(enabled[0]);
    }
    let keys: Vec<String> = enabled.iter().map(addr_key).collect();
    let picked =
        select_chair_and_quorum(Purpose::Chair, &Entropy::new(parent_hash.0, unix_ts), &keys);
    picked.first().and_then(|s| parse_addr(s))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn addrs(n: usize) -> Vec<String> {
        (0..n).map(|i| format!("0x{i:040x}")).collect()
    }
    fn ent(h: u8, ts: u64) -> Entropy {
        chair_entropy([h; 32], ts)
    }

    // ------------------------------------------------------------------
    // Sizing rule
    // ------------------------------------------------------------------

    #[test]
    fn committee_size_matches_spec() {
        for n in 0..=16 {
            assert_eq!(committee_size(n), 1, "n={n}");
        }
        assert_eq!(committee_size(16), 1);
        assert_eq!(committee_size(17), 1 + (51 + 8) / 16); // 4
        assert_eq!(committee_size(32), 7); // chair + 6
        assert_eq!(committee_size(64), 13); // chair + 12
        assert_eq!(committee_size(100), 20);
        assert_eq!(committee_size(1000), 189);
    }

    // ------------------------------------------------------------------
    // Contract: length, chair-at-0, quorum-at-1.., distinct, score-ordered
    // ------------------------------------------------------------------

    #[test]
    fn length_matches_committee_size() {
        let e = ent(0x11, 1);
        for n in [1usize, 5, 15, 16, 17, 32, 64, 100] {
            let a = addrs(n);
            let out = select_chair_and_quorum(Purpose::Chair, &e, &a);
            assert_eq!(out.len(), committee_size(n), "n={n}");
        }
    }

    #[test]
    fn n_le_16_returns_chair_only() {
        let e = ent(0x22, 2);
        for n in [1usize, 2, 10, 15, 16] {
            let a = addrs(n);
            let out = select_chair_and_quorum(Purpose::Chair, &e, &a);
            assert_eq!(out.len(), 1, "n={n} must be chair only");
        }
    }

    #[test]
    fn n_32_returns_7_and_n_64_returns_13() {
        let e = ent(0x33, 3);
        assert_eq!(select_chair_and_quorum(Purpose::Chair, &e, &addrs(32)).len(), 7);
        assert_eq!(select_chair_and_quorum(Purpose::Chair, &e, &addrs(64)).len(), 13);
    }

    #[test]
    fn chair_is_index_zero_and_member() {
        let e = ent(0x44, 4);
        for n in [1usize, 3, 16, 17, 32, 64, 100] {
            let a = addrs(n);
            let out = select_chair_and_quorum(Purpose::Chair, &e, &a);
            assert!(!out.is_empty());
            assert!(a.contains(&out[0]), "chair not in input for n={n}");
        }
    }

    #[test]
    fn output_is_score_ordered_and_distinct() {
        let a = addrs(64);
        let e = ent(0x55, 5);
        let out = select_chair_and_quorum(Purpose::Chair, &e, &a);
        // strictly increasing scores
        for w in out.windows(2) {
            let s0 = score(Purpose::Chair, &e, &w[0]);
            let s1 = score(Purpose::Chair, &e, &w[1]);
            assert!(s0 < s1, "not strictly score-ordered");
        }
        // no duplicates
        let set: std::collections::HashSet<_> = out.iter().collect();
        assert_eq!(set.len(), out.len());
    }

    // ------------------------------------------------------------------
    // Determinism + position-free
    // ------------------------------------------------------------------

    #[test]
    fn deterministic() {
        let a = addrs(64);
        let e = ent(0x66, 6);
        let r = select_chair_and_quorum(Purpose::Chair, &e, &a);
        for _ in 0..50 {
            assert_eq!(r, select_chair_and_quorum(Purpose::Chair, &e, &a));
        }
    }

    #[test]
    fn position_free() {
        let base = addrs(50);
        let e = ent(0x77, 7);
        let expected = select_chair_and_quorum(Purpose::Chair, &e, &base);
        for rot in [1usize, 7, 25, 49] {
            let mut r = base.clone();
            r.rotate_left(rot);
            assert_eq!(expected, select_chair_and_quorum(Purpose::Chair, &e, &r));
        }
    }

    #[test]
    fn dedupe_safe() {
        let a = addrs(20);
        let mut d = a.clone();
        d.extend(a.iter().cloned());
        let e = ent(0x88, 8);
        assert_eq!(
            select_chair_and_quorum(Purpose::Chair, &e, &a),
            select_chair_and_quorum(Purpose::Chair, &e, &d),
        );
    }

    // ------------------------------------------------------------------
    // Purpose independence
    // ------------------------------------------------------------------

    #[test]
    fn purposes_select_independently() {
        // Different purpose → different chair overwhelmingly often.
        let a = addrs(64);
        let mut agree = 0u32;
        for t in 0..500u64 {
            let e = ent(0x99, t);
            let c1 = select_chair_and_quorum(Purpose::Chair, &e, &a)[0].clone();
            let c2 = select_chair_and_quorum(Purpose::Bridge, &e, &a)[0].clone();
            if c1 == c2 {
                agree += 1;
            }
        }
        let rate = agree as f64 / 500.0;
        // N=64 → ~1.6% expected
        assert!(rate < 0.05, "purposes agree too often: {rate:.3}");
    }

    // ------------------------------------------------------------------
    // Clock-driven liveness
    // ------------------------------------------------------------------

    #[test]
    fn clock_advance_rotates_chair() {
        let a = addrs(10);
        let mut differ = 0u32;
        for t in 0..1000u64 {
            let c1 = select_chair_and_quorum(Purpose::Chair, &ent(0xAA, t), &a)[0].clone();
            let c2 = select_chair_and_quorum(Purpose::Chair, &ent(0xAA, t + 1), &a)[0].clone();
            if c1 != c2 {
                differ += 1;
            }
        }
        assert!(differ > 850, "only {differ}/1000 rotate");
    }

    #[test]
    fn stuck_chair_recovers_within_seconds() {
        let a = addrs(10);
        let stuck = select_chair_and_quorum(Purpose::Chair, &ent(0xBB, 1000), &a)[0].clone();
        let mut recovered = false;
        for dt in 1..=20u64 {
            let w = select_chair_and_quorum(Purpose::Chair, &ent(0xBB, 1000 + dt), &a)[0].clone();
            if w != stuck {
                recovered = true;
                break;
            }
        }
        assert!(recovered);
    }

    #[test]
    fn current_chair_reads_the_clock() {
        let a = addrs(10);
        let h = [0xCC; 32];
        let now = current_slot();
        let expected =
            select_chair_and_quorum(Purpose::Chair, &chair_entropy(h, now), &a)[0].clone();
        let got = current_chair(Purpose::Chair, h, &a).unwrap();
        // There is a tiny race here if the second ticks between calls;
        // accept either the expected chair or the next-slot chair.
        let next =
            select_chair_and_quorum(Purpose::Chair, &chair_entropy(h, now + 1), &a)[0].clone();
        assert!(got == expected || got == next);
    }

    // ------------------------------------------------------------------
    // Address helpers + typed chair_for
    // ------------------------------------------------------------------

    #[test]
    fn addr_key_and_parse_roundtrip() {
        let a = address_from_hex("24bf191a358c09e79b9ead2d8999d08590b5af31");
        let key = addr_key(&a);
        assert_eq!(key, "0x24bf191a358c09e79b9ead2d8999d08590b5af31");
        assert_eq!(parse_addr(&key), Some(a));
        // with 0x, any case, and bare hex all parse to the same address
        assert_eq!(parse_addr("0x24BF191A358C09E79B9EAD2D8999D08590B5AF31"), Some(a));
        assert_eq!(parse_addr("24bf191a358c09e79b9ead2d8999d08590b5af31"), Some(a));
        assert_eq!(parse_addr("0x1234"), None);
        assert_eq!(parse_addr("garbage"), None);
    }

    #[test]
    fn normalize_eth_lowercases_and_trims() {
        assert_eq!(normalize_eth("  0xABcD  "), "0xabcd");
    }

    fn address_from_hex(bare40: &str) -> Address {
        parse_addr(bare40).expect("valid test address")
    }

    #[test]
    fn chair_for_empty_set_is_none() {
        let parent = B256::ZERO;
        assert_eq!(chair_for(&[], parent, 1), None);
    }

    #[test]
    fn chair_for_single_validator_is_direct_and_entropy_independent() {
        let only = address_from_hex("24bf191a358c09e79b9ead2d8999d08590b5af31");
        let parent = B256::from([0x77u8; 32]);
        // Same answer for every (parent, ts) — the N=1 shortcut.
        for ts in [0u64, 1, 42, 1_700_000_000] {
            assert_eq!(chair_for(&[only], parent, ts), Some(only));
        }
        let other_parent = B256::from([0x88u8; 32]);
        assert_eq!(chair_for(&[only], other_parent, 9), Some(only));
    }

    #[test]
    fn chair_for_multi_matches_select_and_is_a_member() {
        let enabled: Vec<Address> =
            (0..5u8).map(|i| address_from_hex(&format!("{i:040x}"))).collect();
        let parent = B256::from([0x11u8; 32]);
        let ts = 1_700_000_000;
        let chair = chair_for(&enabled, parent, ts).expect("non-empty set has a chair");
        assert!(enabled.contains(&chair), "chair must be a member");

        // Same input → same chair (deterministic), different ts → different chair
        // with overwhelming probability for this set.
        assert_eq!(chair_for(&enabled, parent, ts), Some(chair));
        let keys: Vec<String> = enabled.iter().map(addr_key).collect();
        let via_select =
            select_chair_and_quorum(Purpose::Chair, &Entropy::new(parent.0, ts), &keys);
        assert_eq!(addr_key(&chair), via_select[0]);
    }

    #[test]
    fn chair_for_never_returns_a_non_member() {
        for ts in 0..50u64 {
            let enabled: Vec<Address> =
                (0..3u8).map(|i| address_from_hex(&format!("{i:040x}"))).collect();
            let chair = chair_for(&enabled, B256::from([9u8; 32]), ts).expect("chair exists");
            assert!(enabled.contains(&chair));
        }
    }
}

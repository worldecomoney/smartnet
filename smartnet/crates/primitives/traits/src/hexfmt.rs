//! One place for hex strings: `0x…` in, `0x…` out — added **once**, removed
//! **once**, everywhere an address, key, hash or amount is rendered or parsed.
//!
//! `alloy_primitives` types disagree about the prefix: `Bytes: LowerHex` already
//! prints `0x…`, integers and `B256` do not. A naive `format!("0x{x}")` therefore
//! sent `"0x0x…"` on the wire once (rejected by a real node, invisible to
//! `contains` in tests). Every hex string this crate builds or strips goes
//! through these three functions instead.

use std::fmt::LowerHex;

/// `value` as a hex string with **exactly one** `0x` prefix — whatever the
/// type's own `LowerHex` does (`Bytes` comes out unchanged, `U256`/`u128`/`B256`
/// get the prefix they don't print themselves).
pub fn prefixed<T: LowerHex + ?Sized>(value: &T) -> String {
    ensure_0x(&format!("{value:x}"))
}

/// `hex` as a `0x…` string — for hex **text** that is already written out
/// (the string twin of [`prefixed`]): adds one prefix, never two.
pub fn ensure_0x(hex: &str) -> String {
    if hex.starts_with("0x") || hex.starts_with("0X") {
        hex.to_string()
    } else {
        format!("0x{hex}")
    }
}

/// `value` as **bare** hex, never prefixed — file-name suffixes, topics, and
/// any equality that compares hex as text.
pub fn unprefixed<T: LowerHex + ?Sized>(value: &T) -> String {
    strip_0x(&format!("{value:x}")).to_string()
}

/// Removes **one** leading `0x`/`0X` if present (and only that — never eats
/// into the hex itself, so a bare string passes through untouched).
pub fn strip_0x(hex: &str) -> &str {
    hex.strip_prefix("0x").or_else(|| hex.strip_prefix("0X")).unwrap_or(hex)
}

#[cfg(all(test, feature = "std"))]
mod tests {
    use super::*;
    use alloy_primitives::{address, Bytes, B256};

    #[test]
    fn prefixed_adds_the_prefix_once_whatever_the_type_prints() {
        // Integers / fixed words print bare …
        assert_eq!(prefixed(&0x2au8), "0x2a");
        assert_eq!(prefixed(&0x2au64), "0x2a");
        assert_eq!(prefixed(&B256::repeat_byte(0xab)), format!("0x{}", "ab".repeat(32)));
        assert_eq!(
            prefixed(&address!("1111111111111111111111111111111111111111")),
            "0x1111111111111111111111111111111111111111"
        );
        // … `Bytes` already prints one — so it must NOT get a second.
        let data = Bytes::from_static(&[0xde, 0xad, 0xbe, 0xef]);
        assert_eq!(prefixed(&data), "0xdeadbeef", "no `0x0x`");
        assert!(!prefixed(&data).starts_with("0x0x"));
        // A hand-written prefix is kept as is.
        assert_eq!(strip_0x(&prefixed(&data)), "deadbeef");
    }

    #[test]
    fn unprefixed_and_strip_remove_exactly_one_prefix() {
        assert_eq!(unprefixed(&0x2au8), "2a");
        assert_eq!(unprefixed(&B256::repeat_byte(0x00)), "00".repeat(32));
        let token = address!("55d398326f99059ff775485246999027b3197955");
        assert_eq!(unprefixed(&token), "55d398326f99059ff775485246999027b3197955");

        assert_eq!(strip_0x("0xdead"), "dead");
        assert_eq!(strip_0x("0Xdead"), "dead");
        assert_eq!(strip_0x("dead"), "dead", "bare passes through");
        assert_eq!(strip_0x("0x0xdead"), "0xdead", "one prefix only");
        assert_eq!(strip_0x(""), "");
    }

    #[test]
    fn round_trip_between_the_two_forms() {
        let hash = B256::repeat_byte(0x5c);
        assert_eq!(ensure_0x(strip_0x(&prefixed(&hash))), prefixed(&hash));
        assert_eq!(strip_0x(&prefixed(&hash)), unprefixed(&hash));
        // Case survives the round trip (clients send lowercase).
        assert_eq!(ensure_0x(strip_0x("0xDEAD")), "0xDEAD");
        assert_eq!(ensure_0x("dead"), "0xdead", "adds what is missing");
        assert_eq!(ensure_0x("0xdead"), "0xdead", "never doubles");
        assert_eq!(unprefixed(&Bytes::from_static(&[0xff])), "ff");
    }
}

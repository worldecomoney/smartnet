//! Money math (`BRIDGE-DOC.md` §3.5 / §10, spec locked in `TOD.md` 2.4).
//!
//! Pure integer arithmetic on `U256`: no I/O, no RPC, no EVM, no prices or rates.
//! All configured fees and minimums are plain WEM strings (18 decimals); amounts are
//! base units in the instrument's own decimals, carried on the record as digit strings.
//! Every operation that cannot be represented exactly returns `None` — the caller must
//! then keep the reason on the record and retry next beat; nothing is ever sent short.

use alloy_primitives::U256;
use smartnet_primitives_traits::bridge_config::{parse_wem_decimal, BridgesFile, Network};

/// The highest decimal count allowed by config validation (integer scaling must not wrap).
const MAX_DECIMALS: u8 = 77;
/// WEM has 18 decimals — the unit of every configured fee and minimum.
const WEM_DECIMALS: u8 = 18;

/// 10^`exp` in checked `U256` arithmetic. `exp` is always within `MAX_DECIMALS`.
fn pow10(exp: u8) -> Option<U256> {
    if exp > MAX_DECIMALS {
        return None;
    }
    Some(U256::from(10).pow(U256::from(exp as u64)))
}

/// A WEM string as 18-decimal wei. Absent or empty means none (zero); a malformed
/// value fails closed instead of silently becoming a free transfer.
fn wem_value(raw: &Option<String>) -> Option<U256> {
    match raw {
        None => Some(U256::ZERO),
        Some(s) if s.is_empty() => Some(U256::ZERO),
        Some(s) => parse_wem_decimal(s),
    }
}

/// `bridge_fee` + the landing network's `network_fee`, both fixed WEM, summed in
/// 18-decimal wei. `None` means malformed config or overflow — never a zero fee.
pub fn total_fee_wei(file: &BridgesFile, landing: &Network) -> Option<U256> {
    wem_value(&file.bridge_fee)?.checked_add(wem_value(&landing.network_fee)?)
}

/// Scale 18-decimal WEM wei into the receiving instrument's decimals by integer math.
/// Down-scaling floors (the fraction stays with the bridge); up-scaling is checked.
pub fn fee_in_decimals(fee_wei: U256, dec_out: u8) -> Option<U256> {
    if dec_out > MAX_DECIMALS {
        return None;
    }
    match dec_out.cmp(&WEM_DECIMALS) {
        std::cmp::Ordering::Equal => Some(fee_wei),
        std::cmp::Ordering::Less => fee_wei.checked_div(pow10(WEM_DECIMALS - dec_out)?),
        std::cmp::Ordering::Greater => fee_wei.checked_mul(pow10(dec_out - WEM_DECIMALS)?),
    }
}

/// What the user receives: gross `amountOut` minus both fees, in receiving decimals.
/// `None` means the amount cannot cover the fees (or the math overflowed) — nothing
/// is sent, and the caller records the reason for a retry.
pub fn net_receivable(amount_out: U256, fee_wei: U256, dec_out: u8) -> Option<U256> {
    amount_out.checked_sub(fee_in_decimals(fee_wei, dec_out)?)
}

/// Scale a base-unit amount from its own decimals to 18-decimal WEM by integer math.
/// Down-scaling floors; up-scaling is checked.
fn amount_in_wem(amount: U256, dec_in: u8) -> Option<U256> {
    if dec_in > MAX_DECIMALS {
        return None;
    }
    match dec_in.cmp(&WEM_DECIMALS) {
        std::cmp::Ordering::Equal => Some(amount),
        std::cmp::Ordering::Less => amount.checked_mul(pow10(WEM_DECIMALS - dec_in)?),
        std::cmp::Ordering::Greater => amount.checked_div(pow10(dec_in - WEM_DECIMALS)?),
    }
}

/// The landing network's `minAmount` gate. The minimum is WEM (18 decimals); the
/// incoming gross amount is converted to WEM first, then compared: below → `false`,
/// equal or above → `true`. Absent or `"0"` means no minimum. `None` means the
/// comparison could not be made — reject, never admit on a math failure.
pub fn min_amount_ok(amount: U256, dec_in: u8, landing: &Network) -> Option<bool> {
    let min = wem_value(&landing.min_amount)?;
    if min.is_zero() {
        return Some(true);
    }
    Some(amount_in_wem(amount, dec_in)? >= min)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    fn file(bridge_fee: Option<&str>) -> BridgesFile {
        BridgesFile {
            bridge_fee: bridge_fee.map(str::to_string),
            burn_to: None,
            pairs: BTreeMap::new(),
            networks: BTreeMap::new(),
        }
    }

    fn landing(network_fee: Option<&str>, min_amount: Option<&str>) -> Network {
        Network {
            chain_id: "0x38".to_string(),
            alias: "BSC".to_string(),
            chain_name: "BNB Smart Chain".to_string(),
            is_own: false,
            rpc_urls: vec!["https://example.invalid".to_string()],
            block_explorer_urls: None,
            network_fee: network_fee.map(str::to_string),
            min_amount: min_amount.map(str::to_string),
            min_conf_blocks: 1,
            max_conf_blocks: 2,
            bridges: None,
            instruments: BTreeMap::new(),
        }
    }

    fn wem(whole: u64) -> U256 {
        U256::from(whole) * U256::from(10).pow(U256::from(WEM_DECIMALS as u64))
    }

    #[test]
    fn absent_and_empty_fees_are_zero() {
        for bridge in [None, Some("")] {
            for network in [None, Some("")] {
                assert_eq!(total_fee_wei(&file(bridge), &landing(network, None)), Some(U256::ZERO));
            }
        }
    }

    #[test]
    fn bridge_fee_and_landing_network_fee_add_in_wei() {
        let f = file(Some("0.10"));
        let net = landing(Some("5.00"), None);
        assert_eq!(total_fee_wei(&f, &net), Some(wem(5) + wem(1) / U256::from(10)));
        let own = landing(Some("0.01"), None);
        assert_eq!(
            total_fee_wei(&f, &own),
            Some(wem(1) / U256::from(10) + wem(1) / U256::from(100))
        );
    }

    #[test]
    fn malformed_or_overflowing_fees_fail_closed() {
        assert_eq!(total_fee_wei(&file(Some("1e5")), &landing(None, None)), None);
        assert_eq!(total_fee_wei(&file(None), &landing(Some("1.2.3"), None)), None);
        assert_eq!(
            total_fee_wei(&file(Some("0.10")), &landing(Some("0.01"), None)),
            Some(wem(1) / U256::from(10) + wem(1) / U256::from(100))
        );
    }

    #[test]
    fn fee_scales_down_to_receiving_decimals_by_floor() {
        let fee = wem(5) + wem(1) / U256::from(10); // 5.10 WEM
        assert_eq!(fee_in_decimals(fee, 6), Some(U256::from(5_100_000u64)));
        assert_eq!(fee_in_decimals(wem(1) / U256::from(10), 6), Some(U256::from(100_000u64)));
        assert_eq!(
            fee_in_decimals(U256::from(1u64), 6),
            Some(U256::ZERO),
            "fraction stays with the bridge"
        );
    }

    #[test]
    fn fee_scales_identity_up_and_fails_closed_on_overflow() {
        let fee = wem(1);
        assert_eq!(fee_in_decimals(fee, 18), Some(fee));
        assert_eq!(fee_in_decimals(fee, 20), Some(fee * U256::from(100)));
        assert_eq!(fee_in_decimals(fee, 78), None);
        assert_eq!(fee_in_decimals(U256::MAX, 20), None);
    }

    #[test]
    fn net_receivable_subtracts_and_never_underflows() {
        let fee = wem(1);
        assert_eq!(net_receivable(wem(3), fee, 18), Some(wem(2)));
        assert_eq!(net_receivable(wem(1) - U256::from(1u64), fee, 18), None);
        assert_eq!(net_receivable(wem(3), U256::ZERO, 18), Some(wem(3)));
        assert_eq!(
            net_receivable(U256::from(3_000_000u64), fee, 6),
            Some(U256::from(2_000_000u64))
        );
    }

    #[test]
    fn min_amount_compares_in_wem_after_floor_scaling() {
        let net = landing(None, Some("1.00"));
        assert_eq!(min_amount_ok(U256::from(1_000_000u64), 6, &net), Some(true));
        assert_eq!(min_amount_ok(U256::from(999_999u64), 6, &net), Some(false));
        let big = landing(None, Some("100"));
        let amount_20dec = wem(100) * U256::from(100); // 100 tokens at 20 decimals
        assert_eq!(min_amount_ok(amount_20dec, 20, &big), Some(true));
        assert_eq!(
            min_amount_ok(amount_20dec - U256::from(1u64), 20, &big),
            Some(false),
            "one base unit short floors below the WEM minimum"
        );
        assert_eq!(min_amount_ok(wem(1), 18, &landing(None, Some("0"))), Some(true));
        assert_eq!(min_amount_ok(wem(1), 18, &landing(None, None)), Some(true));
    }
}

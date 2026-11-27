//! THE chair mint-mimic builder — kept for the separate bridge build, DISABLED here.
//!
//! **Status (2026-11-27):** `disable_gate_rewards()` is unconditionally `true`, so the EVM
//! GATE in `EthEvm::transact_raw` never opens in this node/validator build. `tx_out` is
//! kept as a plain signing builder with **no production caller**; it cannot mint, and any
//! beneficiary-signed overdraft is refused by the ordinary balance check.
//!
//! Bridge-build intent (not active here): against a bridge mandate that is on-net and
//! verifiable the chair builds and signs an **ordinary native transaction** to the bridge's
//! node address — `value` = the mandate's exact amount, `data` = the mandate payload — and
//! appends it to the block it is sealing. If that build explicitly changes the opt-out, the
//! GATE would let it through whatever the chair holds: the recipient credited and the chair
//! never debited below zero — mimicking minting, as a real on-chain transaction record.
//!
//! "chair only" would be enforced in two mechanical places, never in here: this fn signs
//! with exactly the key it is given (the chair's), and the gate would only open for the
//! header beneficiary. `check_seal` proves the block's **seal signer** is the SELECT chair;
//! the beneficiary equals that chair by producer convention, **not** by a receiving-side
//! check. Mandate verification / the dedicated receipt input is the design intent of the
//! GATE; the bridge-side wiring is still pending (`BRIDGE-TODO.md` §3, finding G-3).

use alloy_consensus::{SignableTransaction, TxEnvelope, TxLegacy};
use alloy_primitives::{Address, Bytes, Signature, TxKind, U256};
use eyre::Result;
use k256::ecdsa::SigningKey;
use smartnet_chainspec::fixed_gas_price_wei;

/// The kept bridge builder (operator: "make 1 distinct fn that can tx out by chair only
/// ignoring it's own balance, mimiking minting") — build and sign one native
/// `value + data` transfer from the chair: a bridge mandate issuance, the 0.01 WEM
/// authorization, whatever the chair must say on chain.
///
/// The fn **never consults any balance**; only a bridge build with an enabled gate would
/// settle the money side at execution. Here it is just a signer. The tx is priced at the
/// protocol fixed fee and carries exactly the intrinsic gas of a plain transfer with this
/// calldata, so nothing is ever over-paid.
///
/// Deterministic: the same inputs always sign to the same bytes (RFC 6979), so a rebuilt
/// broadcast is byte-identical and at most one of two can ever be mined.
pub fn tx_out(
    chair: &SigningKey,
    chain_id: u64,
    nonce: u64,
    to: Address,
    value: U256,
    data: Bytes,
) -> Result<TxEnvelope> {
    // Intrinsic gas of a plain value transfer with calldata: 21_000 plus 4 per zero byte
    // and 16 per non-zero byte. A transfer runs no code, so this is the exact spend.
    let mut gas_limit: u64 = 21_000;
    for b in &data {
        gas_limit = gas_limit.saturating_add(if *b == 0 { 4 } else { 16 });
    }

    let tx = TxLegacy {
        chain_id: Some(chain_id),
        nonce,
        gas_price: u128::from(fixed_gas_price_wei()),
        gas_limit,
        to: TxKind::Call(to),
        value,
        input: data,
    };

    let sighash = tx.signature_hash();
    let mut digest = [0u8; 32];
    digest.copy_from_slice(sighash.as_slice());
    let (sig, rec_id) = chair.sign_prehash_recoverable(&digest)?;
    let signature = Signature::from_signature_and_parity(sig, rec_id.is_y_odd());

    Ok(TxEnvelope::from(tx.into_signed(signature)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::run_once::signer_address;
    use alloy_eips::eip2718::Encodable2718;

    const BRIDGE: Address = Address::repeat_byte(0xBB);

    fn chair(byte: u8) -> SigningKey {
        SigningKey::from_slice(&[byte; 32]).expect("valid secret")
    }

    fn issuance(key: &SigningKey, data: &'static [u8]) -> TxEnvelope {
        tx_out(
            key,
            315,
            7,
            BRIDGE,
            U256::from(10).pow(U256::from(18)),
            Bytes::from_static(data),
        )
        .expect("sign")
    }

    #[test]
    fn tx_out_is_deterministic() {
        let a = issuance(&chair(0x22), b"mandate-1");
        let b = issuance(&chair(0x22), b"mandate-1");
        assert_eq!(
            a.encoded_2718(),
            b.encoded_2718(),
            "the bridge rebuilds a lost broadcast and must get byte-identical bytes"
        );

        let other_key = issuance(&chair(0x33), b"mandate-1");
        assert_ne!(a.encoded_2718(), other_key.encoded_2718());
        let other_data = issuance(&chair(0x22), b"mandate-2");
        assert_ne!(a.encoded_2718(), other_data.encoded_2718());
    }

    #[test]
    fn tx_out_carries_the_mandate_verbatim() {
        let env = issuance(&chair(0x22), b"mandate-1");
        let TxEnvelope::Legacy(signed) = &env else {
            panic!("the issuance is a legacy value transfer")
        };
        let tx = signed.tx();

        assert_eq!(tx.chain_id, Some(315));
        assert_eq!(tx.nonce, 7);
        assert_eq!(tx.to, TxKind::Call(BRIDGE));
        assert_eq!(tx.value, U256::from(10).pow(U256::from(18)));
        assert_eq!(tx.input, Bytes::from_static(b"mandate-1"));
        assert_eq!(tx.gas_price, u128::from(fixed_gas_price_wei()), "fixed-fee policy");
        assert!(tx.gas_limit >= 21_000, "the issuance covers its own intrinsic gas");
    }

    #[test]
    fn tx_out_is_signed_by_the_chair() {
        let key = chair(0x22);
        let env = issuance(&key, b"mandate-1");
        let TxEnvelope::Legacy(signed) = &env else {
            panic!("the issuance is a legacy value transfer")
        };

        let recovered = signed
            .signature()
            .recover_address_from_prehash(&signed.tx().signature_hash())
            .expect("the signature must recover");
        assert_eq!(recovered, signer_address(&key), "only the chair's key may sign it");
    }
}

//! HMAC-SHA2 providers for [`rustls`](rustls::crypto::hmac::Hmac).

use core::fmt;

use hmac::{Hmac, Mac};
use rustls::crypto::hmac::{Hmac as RustlsHmac, Key as RustlsKey, Tag};
use sha2::{Sha256, Sha384};

/// HMAC-SHA256 provider.
#[derive(Debug, Clone, Copy, Default)]
pub struct HmacSha256;

/// HMAC-SHA384 provider.
#[derive(Debug, Clone, Copy, Default)]
pub struct HmacSha384;

/// HMAC-SHA256 provider instance.
pub static HMAC_SHA256: HmacSha256 = HmacSha256;
/// HMAC-SHA384 provider instance.
pub static HMAC_SHA384: HmacSha384 = HmacSha384;

impl RustlsHmac for HmacSha256 {
    fn with_key(&self, key: &[u8]) -> Box<dyn RustlsKey> {
        Box::new(HmacKey256(
            Hmac::<Sha256>::new_from_slice(key).expect("HMAC accepts keys of any length"),
        ))
    }

    fn hash_output_len(&self) -> usize {
        32
    }

    fn fips(&self) -> bool {
        false
    }
}

impl RustlsHmac for HmacSha384 {
    fn with_key(&self, key: &[u8]) -> Box<dyn RustlsKey> {
        Box::new(HmacKey384(
            Hmac::<Sha384>::new_from_slice(key).expect("HMAC accepts keys of any length"),
        ))
    }

    fn hash_output_len(&self) -> usize {
        48
    }

    fn fips(&self) -> bool {
        false
    }
}

struct HmacKey256(Hmac<Sha256>);

impl fmt::Debug for HmacKey256 {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("HmacKey256").finish_non_exhaustive()
    }
}

impl RustlsKey for HmacKey256 {
    fn sign_concat(&self, first: &[u8], middle: &[&[u8]], last: &[u8]) -> Tag {
        let mut mac = self.0.clone();
        mac.update(first);
        for part in middle {
            mac.update(part);
        }
        mac.update(last);
        let tag = mac.finalize().into_bytes();
        Tag::new(&tag)
    }

    fn tag_len(&self) -> usize {
        32
    }
}

struct HmacKey384(Hmac<Sha384>);

impl fmt::Debug for HmacKey384 {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("HmacKey384").finish_non_exhaustive()
    }
}

impl RustlsKey for HmacKey384 {
    fn sign_concat(&self, first: &[u8], middle: &[&[u8]], last: &[u8]) -> Tag {
        let mut mac = self.0.clone();
        mac.update(first);
        for part in middle {
            mac.update(part);
        }
        mac.update(last);
        let tag = mac.finalize().into_bytes();
        Tag::new(&tag)
    }

    fn tag_len(&self) -> usize {
        48
    }
}

#[cfg(test)]
mod tests {
    use rustls::crypto::tls13::{Hkdf, HkdfUsingHmac};

    use super::*;

    fn unhex(s: &str) -> Vec<u8> {
        (0..s.len()).step_by(2).map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap()).collect()
    }

    // RFC 5869 test vectors (HMAC-SHA-256).

    #[test]
    fn rfc5869_case_1() {
        let ikm = [0x0b_u8; 22];
        let salt: Vec<u8> = (0x00_u8..=0x0c).collect();
        let info: Vec<u8> = (0_u8..10).map(|i| 0xf0 + i).collect();

        let hkdf = HkdfUsingHmac(&HMAC_SHA256);
        let expander = hkdf.extract_from_secret(Some(&salt), &ikm);
        let mut okm = [0_u8; 42];
        expander.expand_slice(&[&info], &mut okm).unwrap();

        assert_eq!(
            okm.to_vec(),
            unhex(
                "3cb25f25faacd57a90434f64d0362f2a\
                 2d2d0a90cf1a5a4c5db02d56ecc4c5bf\
                 34007208d5b887185865"
            )
        );
    }

    #[test]
    fn rfc5869_case_2_long_inputs() {
        let ikm: Vec<u8> = (0x00_u8..=0x4f).collect();
        let salt: Vec<u8> = (0x60_u8..=0xaf).collect();
        let info: Vec<u8> = (0xb0_u8..=0xff).collect();

        let hkdf = HkdfUsingHmac(&HMAC_SHA256);
        let expander = hkdf.extract_from_secret(Some(&salt), &ikm);
        let mut okm = [0_u8; 82];
        expander.expand_slice(&[&info], &mut okm).unwrap();

        assert_eq!(
            okm.to_vec(),
            unhex(
                "b11e398dc80327a1c8e7f78c596a4934\
                 4f012eda2d4efad8a050cc4c19afa97c\
                 59045a99cac7827271cb41c65e590e09\
                 da3275600c2f09b8367793a9aca3db71\
                 cc30c58179ec3e87c14c01d5c1f3434f\
                 1d87"
            )
        );
    }

    #[test]
    fn rfc5869_case_3_empty_salt_and_info() {
        let ikm = [0x0b_u8; 22];
        let info: &[u8] = b"";

        let hkdf = HkdfUsingHmac(&HMAC_SHA256);
        let expander = hkdf.extract_from_secret(None, &ikm);
        let mut okm = [0_u8; 42];
        expander.expand_slice(&[info], &mut okm).unwrap();

        assert_eq!(
            okm.to_vec(),
            unhex(
                "8da4e775a563c18f715f802a063c5a31\
                 b8a11f5c5ee1879ec3454e5f3c738d2d\
                 9d201395faa4b61a96c8"
            )
        );
    }

    #[test]
    fn tag_len_matches_hash_output_len() {
        assert_eq!(HMAC_SHA256.with_key(b"key").tag_len(), 32);
        assert_eq!(HMAC_SHA384.with_key(b"key").tag_len(), 48);
        assert_eq!(HMAC_SHA256.hash_output_len(), 32);
        assert_eq!(HMAC_SHA384.hash_output_len(), 48);
    }
}

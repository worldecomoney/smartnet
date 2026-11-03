//! TLS1.3 cipher suites backed by pure-Rust primitives.

use rustls::{
    crypto::tls13::HkdfUsingHmac, CipherSuite, CipherSuiteCommon, SupportedCipherSuite,
    Tls13CipherSuite,
};

use crate::{aead, hash, hmac};

/// `TLS_AES_256_GCM_SHA384` cipher suite.
pub static TLS13_AES_256_GCM_SHA384: SupportedCipherSuite =
    SupportedCipherSuite::Tls13(&SUITE_AES_256_GCM_SHA384);

static SUITE_AES_256_GCM_SHA384: Tls13CipherSuite = Tls13CipherSuite {
    common: CipherSuiteCommon {
        suite: CipherSuite::TLS13_AES_256_GCM_SHA384,
        hash_provider: &hash::SHA384,
        // ref: <https://www.ietf.org/archive/id/draft-irtf-cfrg-aead-limits-08.html#section-5.2.1>
        confidentiality_limit: 1 << 24,
    },
    hkdf_provider: &HkdfUsingHmac(&hmac::HMAC_SHA384),
    aead_alg: &aead::AES_256_GCM,
    quic: None,
};

/// `TLS_AES_128_GCM_SHA256` cipher suite.
pub static TLS13_AES_128_GCM_SHA256: SupportedCipherSuite =
    SupportedCipherSuite::Tls13(&SUITE_AES_128_GCM_SHA256);

static SUITE_AES_128_GCM_SHA256: Tls13CipherSuite = Tls13CipherSuite {
    common: CipherSuiteCommon {
        suite: CipherSuite::TLS13_AES_128_GCM_SHA256,
        hash_provider: &hash::SHA256,
        // ref: <https://www.ietf.org/archive/id/draft-irtf-cfrg-aead-limits-08.html#section-5.2.1>
        confidentiality_limit: 1 << 24,
    },
    hkdf_provider: &HkdfUsingHmac(&hmac::HMAC_SHA256),
    aead_alg: &aead::AES_128_GCM,
    quic: None,
};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn suites_expose_expected_ids() {
        assert_eq!(SUITE_AES_256_GCM_SHA384.common.suite, CipherSuite::TLS13_AES_256_GCM_SHA384);
        assert_eq!(SUITE_AES_256_GCM_SHA384.common.hash_provider.output_len(), 48);
        assert_eq!(SUITE_AES_256_GCM_SHA384.aead_alg.key_len(), 32);
        assert!(SUITE_AES_256_GCM_SHA384.quic.is_none());

        assert_eq!(SUITE_AES_128_GCM_SHA256.common.suite, CipherSuite::TLS13_AES_128_GCM_SHA256);
        assert_eq!(SUITE_AES_128_GCM_SHA256.common.hash_provider.output_len(), 32);
        assert_eq!(SUITE_AES_128_GCM_SHA256.aead_alg.key_len(), 16);
        assert!(SUITE_AES_128_GCM_SHA256.quic.is_none());

        assert!(matches!(TLS13_AES_256_GCM_SHA384, SupportedCipherSuite::Tls13(_)));
        assert!(matches!(TLS13_AES_128_GCM_SHA256, SupportedCipherSuite::Tls13(_)));
    }
}

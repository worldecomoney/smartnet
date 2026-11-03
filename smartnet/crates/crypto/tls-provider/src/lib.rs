//! Pure-Rust [`rustls`](https://docs.rs/rustls) `CryptoProvider` for `SmartNet`.
//!
//! This crate supplies the process-level TLS crypto provider for the node so
//! that HTTPS traffic (e.g. syncing from `https://rpc2.wemblockchain.com`) is
//! backed exclusively by pure-Rust crates — no `aws-lc-sys`, no `ring`, no C
//! toolchain at build time:
//!
//! | Primitive            | Implementation            |
//! |----------------------|---------------------------|
//! | Hash (SHA-2)         | [`sha2`]                  |
//! | HMAC / HKDF          | [`hmac`] via rustls' HKDF |
//! | AEAD (AES-GCM)       | [`aes-gcm`]               |
//! | Key exchange         | [`x25519-dalek`], [`p256`] |
//! | Certificate verify   | [`p256`], [`p384`], [`rsa`], [`ed25519-dalek`] |
//!
//! Call [`install_default`] once at process startup — before any rustls
//! `ClientConfig` is built — otherwise rustls panics looking for a provider.
//!
//! [`sha2`]: https://docs.rs/sha2
//! [`hmac`]: https://docs.rs/hmac
//! [`aes-gcm`]: https://docs.rs/aes-gcm
//! [`p256`]: https://docs.rs/p256
//! [`p384`]: https://docs.rs/p384
//! [`rsa`]: https://docs.rs/rsa
//! [`ed25519-dalek`]: https://docs.rs/ed25519-dalek
//! [`x25519-dalek`]: https://docs.rs/x25519-dalek
#![forbid(unsafe_code)]

pub mod aead;
pub mod hash;
pub mod hmac;
pub mod kx;
pub mod sign;
pub mod tls13;

use std::sync::Arc;

use rustls::{
    crypto::{
        CryptoProvider, GetRandomFailed, KeyProvider, SecureRandom, SupportedKxGroup,
        WebPkiSupportedAlgorithms,
    },
    pki_types::PrivateKeyDer,
    sign::SigningKey,
    Error, SignatureScheme,
};

/// The cipher suites offered by this provider (TLS1.3 only).
pub static DEFAULT_CIPHER_SUITES: &[rustls::SupportedCipherSuite] =
    &[tls13::TLS13_AES_256_GCM_SHA384, tls13::TLS13_AES_128_GCM_SHA256];

/// The key exchange groups offered by this provider (X25519 and secp256r1).
pub static DEFAULT_KX_GROUPS: &[&dyn SupportedKxGroup] = &[kx::X25519, kx::SECP256R1];

/// Signature verification algorithms, mirroring the rustls webpki tables.
static SUPPORTED_SIG_ALGS: WebPkiSupportedAlgorithms = WebPkiSupportedAlgorithms {
    all: &[
        sign::ECDSA_P256_SHA256,
        sign::ECDSA_P256_SHA384,
        sign::ECDSA_P256_SHA512,
        sign::ECDSA_P384_SHA384,
        sign::ED25519,
        sign::RSA_PSS_2048_8192_SHA256,
        sign::RSA_PSS_2048_8192_SHA384,
        sign::RSA_PSS_2048_8192_SHA512,
        sign::RSA_PKCS1_2048_8192_SHA256,
        sign::RSA_PKCS1_2048_8192_SHA384,
        sign::RSA_PKCS1_2048_8192_SHA512,
        sign::RSA_PKCS1_2048_8192_SHA256_ABSENT_PARAMS,
        sign::RSA_PKCS1_2048_8192_SHA384_ABSENT_PARAMS,
        sign::RSA_PKCS1_2048_8192_SHA512_ABSENT_PARAMS,
    ],
    mapping: &[
        // For TLS1.3 the curve is fixed by the SignatureScheme; for TLS1.2
        // candidates are tried in order.
        (SignatureScheme::ECDSA_NISTP256_SHA256, &[sign::ECDSA_P256_SHA256]),
        (SignatureScheme::ECDSA_NISTP384_SHA384, &[sign::ECDSA_P384_SHA384]),
        (SignatureScheme::ED25519, &[sign::ED25519]),
        (SignatureScheme::RSA_PSS_SHA512, &[sign::RSA_PSS_2048_8192_SHA512]),
        (SignatureScheme::RSA_PSS_SHA384, &[sign::RSA_PSS_2048_8192_SHA384]),
        (SignatureScheme::RSA_PSS_SHA256, &[sign::RSA_PSS_2048_8192_SHA256]),
        (SignatureScheme::RSA_PKCS1_SHA512, &[sign::RSA_PKCS1_2048_8192_SHA512]),
        (SignatureScheme::RSA_PKCS1_SHA384, &[sign::RSA_PKCS1_2048_8192_SHA384]),
        (SignatureScheme::RSA_PKCS1_SHA256, &[sign::RSA_PKCS1_2048_8192_SHA256]),
    ],
};

/// Process crypto: randomness and private key loading (PKCS#8 ECDSA P-256).
#[derive(Debug)]
struct SmartNetCrypto;

impl SecureRandom for SmartNetCrypto {
    fn fill(&self, buf: &mut [u8]) -> Result<(), GetRandomFailed> {
        getrandom::getrandom(buf).map_err(|_| GetRandomFailed)
    }
}

impl KeyProvider for SmartNetCrypto {
    fn load_private_key(
        &self,
        _key_der: PrivateKeyDer<'static>,
    ) -> Result<Arc<dyn SigningKey>, Error> {
        // The node is a TLS *client* only — inbound TLS is nginx's job (see
        // doc/cert.md). No private keys are ever loaded; the X.509 generation
        // module that once fed `ServerConfig::with_single_cert` has been removed.
        Err(Error::General("no private-key loading: SmartNet never serves TLS".to_string()))
    }
}

/// Build the `SmartNet` [`CryptoProvider`].
pub fn default_provider() -> CryptoProvider {
    CryptoProvider {
        cipher_suites: DEFAULT_CIPHER_SUITES.to_vec(),
        kx_groups: DEFAULT_KX_GROUPS.to_vec(),
        signature_verification_algorithms: SUPPORTED_SIG_ALGS,
        secure_random: &SmartNetCrypto,
        key_provider: &SmartNetCrypto,
    }
}

/// Install the `SmartNet` provider as the process-wide rustls default.
///
/// Must be called once at startup, before any rustls client or server config
/// is constructed. Returns the previously installed provider if one is already
/// set.
pub fn install_default() -> Result<(), Arc<CryptoProvider>> {
    default_provider().install_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn provider_assembles_and_installs() {
        let provider = default_provider();
        assert_eq!(provider.cipher_suites.len(), 2);
        assert_eq!(provider.kx_groups.len(), 2);
        assert!(!provider.signature_verification_algorithms.all.is_empty());

        // Installing twice is allowed to fail (someone won the race); the
        // process default must be set either way.
        let _ = provider.install_default();
        assert!(CryptoProvider::get_default().is_some());
    }
}

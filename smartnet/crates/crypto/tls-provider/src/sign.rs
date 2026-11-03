//! Signature verification algorithms (ECDSA P-256/P-384, RSA PKCS#1/PSS, Ed25519).
//!
//! Mirrors the algorithm tables of rustls' ring/aws-lc-rs providers, but every
//! verification path is implemented with pure-Rust crates (`p256`, `p384`,
//! `rsa`, `ed25519-dalek`).

use ed25519_dalek::VerifyingKey as Ed25519VerifyingKey;
use p256::{
    ecdsa::VerifyingKey as P256VerifyingKey, elliptic_curve::sec1::FromEncodedPoint, EncodedPoint,
    PublicKey,
};
use p384::{
    ecdsa::VerifyingKey as P384VerifyingKey, EncodedPoint as P384EncodedPoint,
    PublicKey as P384PublicKey,
};
use rsa::{pkcs1::DecodeRsaPublicKey, traits::PublicKeyParts, RsaPublicKey};
use rustls::pki_types::{
    alg_id, AlgorithmIdentifier, InvalidSignature, SignatureVerificationAlgorithm,
};
use sha2::{Digest, Sha256, Sha384, Sha512};
use signature::{hazmat::PrehashVerifier, Verifier};

/// Which verification algorithm an algorithm entry applies to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum VerifyKind {
    EcdsaP256Sha256,
    EcdsaP256Sha384,
    EcdsaP256Sha512,
    EcdsaP384Sha384,
    RsaPkcs1Sha256,
    RsaPkcs1Sha384,
    RsaPkcs1Sha512,
    RsaPssSha256,
    RsaPssSha384,
    RsaPssSha512,
    Ed25519,
}

#[derive(Debug)]
struct PureAlg {
    public_key_alg_id: AlgorithmIdentifier,
    signature_alg_id: AlgorithmIdentifier,
    kind: VerifyKind,
}

fn hash_digest(kind: VerifyKind, message: &[u8]) -> Vec<u8> {
    match kind {
        VerifyKind::EcdsaP256Sha256 | VerifyKind::RsaPkcs1Sha256 | VerifyKind::RsaPssSha256 => {
            Sha256::digest(message).to_vec()
        }
        VerifyKind::EcdsaP256Sha384 |
        VerifyKind::EcdsaP384Sha384 |
        VerifyKind::RsaPkcs1Sha384 |
        VerifyKind::RsaPssSha384 => Sha384::digest(message).to_vec(),
        VerifyKind::EcdsaP256Sha512 | VerifyKind::RsaPkcs1Sha512 | VerifyKind::RsaPssSha512 => {
            Sha512::digest(message).to_vec()
        }
        VerifyKind::Ed25519 => Vec::new(),
    }
}

impl SignatureVerificationAlgorithm for PureAlg {
    fn verify_signature(
        &self,
        public_key: &[u8],
        message: &[u8],
        signature: &[u8],
    ) -> Result<(), InvalidSignature> {
        match self.kind {
            VerifyKind::Ed25519 => {
                let pk: &[u8; 32] = public_key.try_into().map_err(|_| InvalidSignature)?;
                let verifying_key =
                    Ed25519VerifyingKey::from_bytes(pk).map_err(|_| InvalidSignature)?;
                let sig =
                    ed25519_dalek::Signature::try_from(signature).map_err(|_| InvalidSignature)?;
                verifying_key.verify(message, &sig).map_err(|_| InvalidSignature)
            }

            VerifyKind::EcdsaP384Sha384 => {
                match public_key.first() {
                    Some(0x02..=0x04) => {}
                    _ => return Err(InvalidSignature),
                }
                let encoded =
                    P384EncodedPoint::from_bytes(public_key).map_err(|_| InvalidSignature)?;
                let public =
                    Option::<P384PublicKey>::from(P384PublicKey::from_encoded_point(&encoded))
                        .ok_or(InvalidSignature)?;
                let verifying_key = P384VerifyingKey::from(&public);
                let sig =
                    p384::ecdsa::Signature::from_der(signature).map_err(|_| InvalidSignature)?;
                let digest = hash_digest(self.kind, message);
                verifying_key.verify_prehash(&digest, &sig).map_err(|_| InvalidSignature)
            }

            kind @ (VerifyKind::EcdsaP256Sha256 |
            VerifyKind::EcdsaP256Sha384 |
            VerifyKind::EcdsaP256Sha512) => {
                // Restrict the allowed encodings of EC public keys
                // (RFC 5480 section 2.2).
                match public_key.first() {
                    Some(0x02..=0x04) => {}
                    _ => return Err(InvalidSignature),
                }
                let encoded = EncodedPoint::from_bytes(public_key).map_err(|_| InvalidSignature)?;
                let public = Option::<PublicKey>::from(PublicKey::from_encoded_point(&encoded))
                    .ok_or(InvalidSignature)?;
                let verifying_key = P256VerifyingKey::from(&public);
                let sig =
                    p256::ecdsa::Signature::from_der(signature).map_err(|_| InvalidSignature)?;
                let digest = hash_digest(kind, message);
                verifying_key.verify_prehash(&digest, &sig).map_err(|_| InvalidSignature)
            }

            kind @ (VerifyKind::RsaPkcs1Sha256 |
            VerifyKind::RsaPkcs1Sha384 |
            VerifyKind::RsaPkcs1Sha512) => {
                let pk = RsaPublicKey::from_pkcs1_der(public_key).map_err(|_| InvalidSignature)?;
                if !(2048..=8192).contains(&(pk.size() * 8)) {
                    return Err(InvalidSignature);
                }
                let sig =
                    rsa::pkcs1v15::Signature::try_from(signature).map_err(|_| InvalidSignature)?;
                let digest = hash_digest(kind, message);
                match kind {
                    VerifyKind::RsaPkcs1Sha256 => rsa::pkcs1v15::VerifyingKey::<Sha256>::new(pk)
                        .verify_prehash(&digest, &sig)
                        .map_err(|_| InvalidSignature),
                    VerifyKind::RsaPkcs1Sha384 => rsa::pkcs1v15::VerifyingKey::<Sha384>::new(pk)
                        .verify_prehash(&digest, &sig)
                        .map_err(|_| InvalidSignature),
                    VerifyKind::RsaPkcs1Sha512 => rsa::pkcs1v15::VerifyingKey::<Sha512>::new(pk)
                        .verify_prehash(&digest, &sig)
                        .map_err(|_| InvalidSignature),
                    _ => Err(InvalidSignature),
                }
            }

            kind @ (VerifyKind::RsaPssSha256 |
            VerifyKind::RsaPssSha384 |
            VerifyKind::RsaPssSha512) => {
                let pk = RsaPublicKey::from_pkcs1_der(public_key).map_err(|_| InvalidSignature)?;
                if !(2048..=8192).contains(&(pk.size() * 8)) {
                    return Err(InvalidSignature);
                }
                let sig = rsa::pss::Signature::try_from(signature).map_err(|_| InvalidSignature)?;
                let digest = hash_digest(kind, message);
                match kind {
                    VerifyKind::RsaPssSha256 => rsa::pss::VerifyingKey::<Sha256>::new(pk)
                        .verify_prehash(&digest, &sig)
                        .map_err(|_| InvalidSignature),
                    VerifyKind::RsaPssSha384 => rsa::pss::VerifyingKey::<Sha384>::new(pk)
                        .verify_prehash(&digest, &sig)
                        .map_err(|_| InvalidSignature),
                    VerifyKind::RsaPssSha512 => rsa::pss::VerifyingKey::<Sha512>::new(pk)
                        .verify_prehash(&digest, &sig)
                        .map_err(|_| InvalidSignature),
                    _ => Err(InvalidSignature),
                }
            }
        }
    }

    fn public_key_alg_id(&self) -> AlgorithmIdentifier {
        self.public_key_alg_id
    }

    fn signature_alg_id(&self) -> AlgorithmIdentifier {
        self.signature_alg_id
    }

    fn fips(&self) -> bool {
        false
    }
}

/// ECDSA signatures over P-256 with SHA-256.
pub static ECDSA_P256_SHA256: &dyn SignatureVerificationAlgorithm = &PureAlg {
    public_key_alg_id: alg_id::ECDSA_P256,
    signature_alg_id: alg_id::ECDSA_SHA256,
    kind: VerifyKind::EcdsaP256Sha256,
};

/// ECDSA signatures over P-256 with SHA-384.
pub static ECDSA_P256_SHA384: &dyn SignatureVerificationAlgorithm = &PureAlg {
    public_key_alg_id: alg_id::ECDSA_P256,
    signature_alg_id: alg_id::ECDSA_SHA384,
    kind: VerifyKind::EcdsaP256Sha384,
};

/// ECDSA signatures over P-256 with SHA-512.
pub static ECDSA_P256_SHA512: &dyn SignatureVerificationAlgorithm = &PureAlg {
    public_key_alg_id: alg_id::ECDSA_P256,
    signature_alg_id: alg_id::ECDSA_SHA512,
    kind: VerifyKind::EcdsaP256Sha512,
};

/// ECDSA signatures over P-384 with SHA-384.
pub static ECDSA_P384_SHA384: &dyn SignatureVerificationAlgorithm = &PureAlg {
    public_key_alg_id: alg_id::ECDSA_P384,
    signature_alg_id: alg_id::ECDSA_SHA384,
    kind: VerifyKind::EcdsaP384Sha384,
};

/// RSA PKCS#1 v1.5 signatures with SHA-256 for 2048-8192 bit keys.
pub static RSA_PKCS1_2048_8192_SHA256: &dyn SignatureVerificationAlgorithm = &PureAlg {
    public_key_alg_id: alg_id::RSA_ENCRYPTION,
    signature_alg_id: alg_id::RSA_PKCS1_SHA256,
    kind: VerifyKind::RsaPkcs1Sha256,
};

/// RSA PKCS#1 v1.5 signatures with SHA-384 for 2048-8192 bit keys.
pub static RSA_PKCS1_2048_8192_SHA384: &dyn SignatureVerificationAlgorithm = &PureAlg {
    public_key_alg_id: alg_id::RSA_ENCRYPTION,
    signature_alg_id: alg_id::RSA_PKCS1_SHA384,
    kind: VerifyKind::RsaPkcs1Sha384,
};

/// RSA PKCS#1 v1.5 signatures with SHA-512 for 2048-8192 bit keys.
pub static RSA_PKCS1_2048_8192_SHA512: &dyn SignatureVerificationAlgorithm = &PureAlg {
    public_key_alg_id: alg_id::RSA_ENCRYPTION,
    signature_alg_id: alg_id::RSA_PKCS1_SHA512,
    kind: VerifyKind::RsaPkcs1Sha512,
};

/// SHA-256 OID only, for certificates with illegally absent parameters
/// (RFC 4055 section 2.1).
const RSA_PKCS1_SHA256_ABSENT_PARAMS_ASN1: &[u8] =
    &[0x06, 0x09, 0x2a, 0x86, 0x48, 0x86, 0xf7, 0x0d, 0x01, 0x01, 0x0b];

/// SHA-384 OID only, for certificates with illegally absent parameters
/// (RFC 4055 section 2.1).
const RSA_PKCS1_SHA384_ABSENT_PARAMS_ASN1: &[u8] =
    &[0x06, 0x09, 0x2a, 0x86, 0x48, 0x86, 0xf7, 0x0d, 0x01, 0x01, 0x0c];

/// SHA-512 OID only, for certificates with illegally absent parameters
/// (RFC 4055 section 2.1).
const RSA_PKCS1_SHA512_ABSENT_PARAMS_ASN1: &[u8] =
    &[0x06, 0x09, 0x2a, 0x86, 0x48, 0x86, 0xf7, 0x0d, 0x01, 0x01, 0x0d];

/// Like [`RSA_PKCS1_2048_8192_SHA256`], but for signatures whose
/// `AlgorithmIdentifier` omits the mandatory NULL parameters.
pub static RSA_PKCS1_2048_8192_SHA256_ABSENT_PARAMS: &dyn SignatureVerificationAlgorithm =
    &PureAlg {
        public_key_alg_id: alg_id::RSA_ENCRYPTION,
        signature_alg_id: AlgorithmIdentifier::from_slice(RSA_PKCS1_SHA256_ABSENT_PARAMS_ASN1),
        kind: VerifyKind::RsaPkcs1Sha256,
    };

/// Like [`RSA_PKCS1_2048_8192_SHA384`], but for signatures whose
/// `AlgorithmIdentifier` omits the mandatory NULL parameters.
pub static RSA_PKCS1_2048_8192_SHA384_ABSENT_PARAMS: &dyn SignatureVerificationAlgorithm =
    &PureAlg {
        public_key_alg_id: alg_id::RSA_ENCRYPTION,
        signature_alg_id: AlgorithmIdentifier::from_slice(RSA_PKCS1_SHA384_ABSENT_PARAMS_ASN1),
        kind: VerifyKind::RsaPkcs1Sha384,
    };

/// Like [`RSA_PKCS1_2048_8192_SHA512`], but for signatures whose
/// `AlgorithmIdentifier` omits the mandatory NULL parameters.
pub static RSA_PKCS1_2048_8192_SHA512_ABSENT_PARAMS: &dyn SignatureVerificationAlgorithm =
    &PureAlg {
        public_key_alg_id: alg_id::RSA_ENCRYPTION,
        signature_alg_id: AlgorithmIdentifier::from_slice(RSA_PKCS1_SHA512_ABSENT_PARAMS_ASN1),
        kind: VerifyKind::RsaPkcs1Sha512,
    };

/// RSA-PSS signatures with SHA-256 for 2048-8192 bit rsaEncryption keys.
pub static RSA_PSS_2048_8192_SHA256: &dyn SignatureVerificationAlgorithm = &PureAlg {
    public_key_alg_id: alg_id::RSA_ENCRYPTION,
    signature_alg_id: alg_id::RSA_PSS_SHA256,
    kind: VerifyKind::RsaPssSha256,
};

/// RSA-PSS signatures with SHA-384 for 2048-8192 bit rsaEncryption keys.
pub static RSA_PSS_2048_8192_SHA384: &dyn SignatureVerificationAlgorithm = &PureAlg {
    public_key_alg_id: alg_id::RSA_ENCRYPTION,
    signature_alg_id: alg_id::RSA_PSS_SHA384,
    kind: VerifyKind::RsaPssSha384,
};

/// RSA-PSS signatures with SHA-512 for 2048-8192 bit rsaEncryption keys.
pub static RSA_PSS_2048_8192_SHA512: &dyn SignatureVerificationAlgorithm = &PureAlg {
    public_key_alg_id: alg_id::RSA_ENCRYPTION,
    signature_alg_id: alg_id::RSA_PSS_SHA512,
    kind: VerifyKind::RsaPssSha512,
};

/// Ed25519 signatures (RFC 8410).
pub static ED25519: &dyn SignatureVerificationAlgorithm = &PureAlg {
    public_key_alg_id: alg_id::ED25519,
    signature_alg_id: alg_id::ED25519,
    kind: VerifyKind::Ed25519,
};

#[cfg(test)]
mod tests {
    use p256::ecdsa::signature::Signer;
    use rsa::pkcs1::EncodeRsaPublicKey as _;
    use signature::{RandomizedSigner, SignatureEncoding};

    use super::*;

    #[test]
    fn ecdsa_p256_sha256_roundtrip_and_rejections() {
        let sk = p256::ecdsa::SigningKey::from_bytes(&p256::FieldBytes::from([3_u8; 32])).unwrap();
        let msg = b"certificate transparency";
        let sig: p256::ecdsa::Signature = Signer::sign(&sk, msg);
        let sig_der = sig.to_der();

        let vk_bytes = sk.verifying_key().to_encoded_point(false).as_bytes().to_vec();

        ECDSA_P256_SHA256.verify_signature(&vk_bytes, msg, sig_der.as_ref()).unwrap();

        // Tampered message.
        assert!(ECDSA_P256_SHA256
            .verify_signature(&vk_bytes, b"certificate  transparency", sig_der.as_ref())
            .is_err());
        // Wrong digest algorithm (signature was over SHA-256).
        assert!(ECDSA_P256_SHA384.verify_signature(&vk_bytes, msg, sig_der.as_ref()).is_err());
        // Rejects unknown first octet / invalid points.
        assert!(ECDSA_P256_SHA256.verify_signature(&[0x05; 65], msg, sig_der.as_ref()).is_err());
        assert!(ECDSA_P256_SHA256.verify_signature(&[0x04; 65], msg, sig_der.as_ref()).is_err());
        // Rejects truncated signatures.
        assert!(ECDSA_P256_SHA256
            .verify_signature(&vk_bytes, msg, &sig_der.as_ref()[..10])
            .is_err());
    }

    #[test]
    fn ecdsa_p384_sha384_roundtrip_and_rejections() {
        let sk = p384::ecdsa::SigningKey::from_bytes(&p384::FieldBytes::from([3_u8; 48])).unwrap();
        let msg = b"certificate transparency on P-384";
        let sig: p384::ecdsa::Signature = Signer::sign(&sk, msg);
        let sig_der = sig.to_der();

        let vk_bytes = sk.verifying_key().to_encoded_point(false).as_bytes().to_vec();

        ECDSA_P384_SHA384.verify_signature(&vk_bytes, msg, sig_der.as_ref()).unwrap();

        // Tampered message.
        assert!(ECDSA_P384_SHA384
            .verify_signature(&vk_bytes, b"certificate  transparency on P-384", sig_der.as_ref())
            .is_err());
        // The P-256 entry must not accept a P-384 public key.
        assert!(ECDSA_P256_SHA256.verify_signature(&vk_bytes, msg, sig_der.as_ref()).is_err());
        // Rejects unknown first octet / invalid points.
        assert!(ECDSA_P384_SHA384.verify_signature(&[0x05; 97], msg, sig_der.as_ref()).is_err());
        assert!(ECDSA_P384_SHA384.verify_signature(&[0x04; 97], msg, sig_der.as_ref()).is_err());
        // Rejects truncated signatures.
        assert!(ECDSA_P384_SHA384
            .verify_signature(&vk_bytes, msg, &sig_der.as_ref()[..10])
            .is_err());
    }

    #[test]
    fn rsa_pkcs1_and_pss_roundtrip_and_rejections() {
        let mut rng = rand_08::thread_rng();
        let sk = rsa::RsaPrivateKey::new(&mut rng, 2048).unwrap();
        let pk = rsa::RsaPublicKey::from(&sk);
        let pub_bytes = pk.to_pkcs1_der().unwrap().as_ref().to_vec();

        let msg = b"rsa all the way down";

        let signer = rsa::pkcs1v15::SigningKey::<Sha256>::new(sk.clone());
        let sig = signer.sign(msg);
        RSA_PKCS1_2048_8192_SHA256
            .verify_signature(&pub_bytes, msg, sig.to_bytes().as_ref())
            .unwrap();
        assert!(RSA_PKCS1_2048_8192_SHA256
            .verify_signature(&pub_bytes, b"rsa all the way up", sig.to_bytes().as_ref())
            .is_err());
        assert!(RSA_PKCS1_2048_8192_SHA384
            .verify_signature(&pub_bytes, msg, sig.to_bytes().as_ref())
            .is_err());
        // Absent-params entry verifies the same signature bytes.
        RSA_PKCS1_2048_8192_SHA256_ABSENT_PARAMS
            .verify_signature(&pub_bytes, msg, sig.to_bytes().as_ref())
            .unwrap();

        let pss_signer = rsa::pss::SigningKey::<Sha256>::new(sk);
        let pss_sig = pss_signer.sign_with_rng(&mut rng, msg);
        RSA_PSS_2048_8192_SHA256
            .verify_signature(&pub_bytes, msg, pss_sig.to_bytes().as_ref())
            .unwrap();
        assert!(RSA_PSS_2048_8192_SHA256
            .verify_signature(&pub_bytes, b"rsa all the way up", pss_sig.to_bytes().as_ref())
            .is_err());
        assert!(RSA_PSS_2048_8192_SHA384
            .verify_signature(&pub_bytes, msg, pss_sig.to_bytes().as_ref())
            .is_err());

        // Undersized / malformed keys and payloads are rejected.
        assert!(RSA_PKCS1_2048_8192_SHA256
            .verify_signature(&[0x30; 30], msg, sig.to_bytes().as_ref())
            .is_err());
        assert!(RSA_PKCS1_2048_8192_SHA256
            .verify_signature(&pub_bytes, msg, &sig.to_bytes()[..10])
            .is_err());
    }

    #[test]
    fn ed25519_roundtrip_and_rejections() {
        let sk = ed25519_dalek::SigningKey::from_bytes(&[7_u8; 32]);
        let msg = b"ed25519 pure";
        let sig = sk.sign(msg);
        let vk_bytes = sk.verifying_key().to_bytes().to_vec();

        ED25519.verify_signature(&vk_bytes, msg, sig.to_bytes().as_ref()).unwrap();

        assert!(ED25519
            .verify_signature(&vk_bytes, b"ed25519 impure", sig.to_bytes().as_ref())
            .is_err());
        assert!(ED25519.verify_signature(&[0_u8; 31], msg, sig.to_bytes().as_ref()).is_err());
        assert!(ED25519.verify_signature(&vk_bytes, msg, &sig.to_bytes()[..63]).is_err());
    }

    #[test]
    fn algorithm_identifiers_match_webpki_tables() {
        assert_eq!(ECDSA_P256_SHA256.public_key_alg_id(), alg_id::ECDSA_P256);
        assert_eq!(ECDSA_P256_SHA256.signature_alg_id(), alg_id::ECDSA_SHA256);
        assert_eq!(ED25519.public_key_alg_id(), alg_id::ED25519);
        assert_eq!(ED25519.signature_alg_id(), alg_id::ED25519);
        assert_eq!(RSA_PKCS1_2048_8192_SHA256.public_key_alg_id(), alg_id::RSA_ENCRYPTION);
        assert_eq!(RSA_PSS_2048_8192_SHA256.signature_alg_id(), alg_id::RSA_PSS_SHA256);
        // The absent-params variant carries the bare OID.
        assert_eq!(
            RSA_PKCS1_2048_8192_SHA256_ABSENT_PARAMS.signature_alg_id(),
            AlgorithmIdentifier::from_slice(RSA_PKCS1_SHA256_ABSENT_PARAMS_ASN1)
        );
    }
}

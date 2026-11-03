//! X25519 (Curve25519) and secp256r1 (NIST P-256) ephemeral key exchange.

use core::fmt;

use p256::{
    ecdh::EphemeralSecret,
    elliptic_curve::sec1::{FromEncodedPoint, ToEncodedPoint},
    EncodedPoint, PublicKey,
};
use rustls::{
    crypto::{ActiveKeyExchange, SharedSecret, SupportedKxGroup},
    Error, NamedGroup, PeerMisbehaved,
};
use x25519_dalek::{
    EphemeralSecret as X25519Secret, PublicKey as X25519PublicKey,
    SharedSecret as X25519SharedSecret,
};

/// X25519 (Curve25519) key exchange group.
pub static X25519: &dyn SupportedKxGroup = &X25519Group;

/// The X25519 key exchange group.
#[derive(Debug, Clone, Copy, Default)]
pub struct X25519Group;

impl SupportedKxGroup for X25519Group {
    fn start(&self) -> Result<Box<dyn ActiveKeyExchange>, Error> {
        let secret = X25519Secret::random();
        let public = X25519PublicKey::from(&secret);
        Ok(Box::new(X25519KeyExchange { secret, pub_key: public.to_bytes().to_vec() }))
    }

    fn name(&self) -> NamedGroup {
        NamedGroup::X25519
    }

    fn fips(&self) -> bool {
        false
    }
}

struct X25519KeyExchange {
    secret: X25519Secret,
    pub_key: Vec<u8>,
}

impl fmt::Debug for X25519KeyExchange {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("X25519KeyExchange").field("pub_key", &self.pub_key).finish_non_exhaustive()
    }
}

impl ActiveKeyExchange for X25519KeyExchange {
    fn complete(self: Box<Self>, peer_pub_key: &[u8]) -> Result<SharedSecret, Error> {
        let bytes: [u8; 32] =
            peer_pub_key.try_into().map_err(|_| PeerMisbehaved::InvalidKeyShare)?;
        let their_public = X25519PublicKey::from(bytes);
        let Self { secret, .. } = *self;
        let shared: X25519SharedSecret = secret.diffie_hellman(&their_public);
        // Reject non-contributory key shares (e.g. the all-zero public key).
        if !shared.was_contributory() {
            return Err(PeerMisbehaved::InvalidKeyShare.into());
        }
        Ok(SharedSecret::from(&shared.to_bytes()[..]))
    }

    fn group(&self) -> NamedGroup {
        NamedGroup::X25519
    }

    fn pub_key(&self) -> &[u8] {
        &self.pub_key
    }
}

/// secp256r1 (NIST P-256) key exchange group.
pub static SECP256R1: &dyn SupportedKxGroup = &Secp256r1;

/// The secp256r1 key exchange group.
#[derive(Debug, Clone, Copy, Default)]
pub struct Secp256r1;

impl SupportedKxGroup for Secp256r1 {
    fn start(&self) -> Result<Box<dyn ActiveKeyExchange>, Error> {
        let secret = EphemeralSecret::random(&mut rand_08::thread_rng());
        let public = secret.public_key().to_encoded_point(false);
        Ok(Box::new(KeyExchange { secret, pub_key: public.as_bytes().to_vec() }))
    }

    fn name(&self) -> NamedGroup {
        NamedGroup::secp256r1
    }

    fn fips(&self) -> bool {
        false
    }
}

struct KeyExchange {
    secret: EphemeralSecret,
    pub_key: Vec<u8>,
}

impl fmt::Debug for KeyExchange {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("KeyExchange").field("pub_key", &self.pub_key).finish_non_exhaustive()
    }
}

impl ActiveKeyExchange for KeyExchange {
    fn complete(self: Box<Self>, peer_pub_key: &[u8]) -> Result<SharedSecret, Error> {
        // TLS1.3 requires the uncompressed point encoding for secp256r1
        // (RFC 8446 section 4.2.8.2 / RFC 5480 section 2.2).
        if !matches!(peer_pub_key.first(), Some(0x04)) {
            return Err(PeerMisbehaved::InvalidKeyShare.into());
        }
        let encoded =
            EncodedPoint::from_bytes(peer_pub_key).map_err(|_| PeerMisbehaved::InvalidKeyShare)?;
        if encoded.is_identity() {
            return Err(PeerMisbehaved::InvalidKeyShare.into());
        }
        let their_public = Option::<PublicKey>::from(PublicKey::from_encoded_point(&encoded))
            .ok_or(PeerMisbehaved::InvalidKeyShare)?;
        let shared = self.secret.diffie_hellman(&their_public);
        Ok(SharedSecret::from(&shared.raw_secret_bytes()[..]))
    }

    fn group(&self) -> NamedGroup {
        NamedGroup::secp256r1
    }

    fn pub_key(&self) -> &[u8] {
        &self.pub_key
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn x25519_mutual_key_exchange_agrees() {
        let alice = X25519.start().unwrap();
        let bob = X25519.start().unwrap();

        let alice_pub = alice.pub_key().to_vec();
        let bob_pub = bob.pub_key().to_vec();

        assert_eq!(alice_pub.len(), 32);
        assert_eq!(alice.group(), NamedGroup::X25519);
        assert_eq!(X25519.name(), NamedGroup::X25519);

        let alice_secret = alice.complete(&bob_pub).unwrap();
        let bob_secret = bob.complete(&alice_pub).unwrap();

        assert_eq!(
            alice_secret.secret_bytes(),
            bob_secret.secret_bytes(),
            "both sides must derive the same ECDH shared secret"
        );
        assert_eq!(alice_secret.secret_bytes().len(), 32);
    }

    #[test]
    fn x25519_rejects_invalid_peer_keys() {
        // Wrong lengths.
        let kx = X25519.start().unwrap();
        assert!(kx.complete(&[0u8; 31]).is_err());
        let kx = X25519.start().unwrap();
        assert!(kx.complete(&[0u8; 33]).is_err());
        let kx = X25519.start().unwrap();
        assert!(kx.complete(&[]).is_err());

        // The all-zero public key is valid Curve25519 input but produces a
        // non-contributory shared secret and must be rejected.
        let kx = X25519.start().unwrap();
        assert!(kx.complete(&[0u8; 32]).is_err());
    }

    #[test]
    fn mutual_key_exchange_agrees() {
        let alice = SECP256R1.start().unwrap();
        let bob = SECP256R1.start().unwrap();

        let alice_pub = alice.pub_key().to_vec();
        let bob_pub = bob.pub_key().to_vec();

        assert_eq!(alice_pub.len(), 65);
        assert_eq!(alice_pub[0], 0x04);
        assert_eq!(alice.group(), NamedGroup::secp256r1);

        let alice_secret = alice.complete(&bob_pub).unwrap();
        let bob_secret = bob.complete(&alice_pub).unwrap();

        assert_eq!(
            alice_secret.secret_bytes(),
            bob_secret.secret_bytes(),
            "both sides must derive the same ECDH shared secret"
        );
        assert_eq!(alice_secret.secret_bytes().len(), 32);
    }

    #[test]
    fn rejects_non_uncompressed_or_invalid_peer_keys() {
        // Compressed encoding is not allowed by TLS1.3 for this group.
        let kx = SECP256R1.start().unwrap();
        let mut compressed = kx.pub_key().to_vec();
        compressed[0] = 0x02;
        assert!(kx.complete(&compressed).is_err());

        // Wrong length.
        let kx = SECP256R1.start().unwrap();
        assert!(kx.complete(&[0x04; 64]).is_err());

        // Correct shape but not a valid curve point.
        let kx = SECP256R1.start().unwrap();
        assert!(kx.complete(&[0x04; 65]).is_err());

        // Empty input.
        let kx = SECP256R1.start().unwrap();
        assert!(kx.complete(&[]).is_err());
    }
}

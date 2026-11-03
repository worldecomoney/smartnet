//! AES-GCM record protection for TLS1.3.

use aes_gcm::{
    aead::{consts::U12, Aead, AeadCore, KeyInit, Nonce as AeadNonce, Payload},
    Aes128Gcm, Aes256Gcm,
};
use rustls::{
    crypto::cipher::{
        make_tls13_aad, AeadKey, InboundOpaqueMessage, InboundPlainMessage, Iv, MessageDecrypter,
        MessageEncrypter, Nonce, OutboundOpaqueMessage, OutboundPlainMessage, PrefixedPayload,
        Tls13AeadAlgorithm, UnsupportedOperationError,
    },
    ConnectionTrafficSecrets, ContentType, Error, ProtocolVersion,
};

const TAG_LEN: usize = 16;

/// `TLS_AES_128_GCM_SHA256` record protection (RFC 8446 / RFC 5116).
#[derive(Debug, Clone, Copy, Default)]
pub struct Tls13Aes128Gcm;

/// `TLS_AES_256_GCM_SHA384` record protection (RFC 8446 / RFC 5116).
#[derive(Debug, Clone, Copy, Default)]
pub struct Tls13Aes256Gcm;

/// AES-128-GCM AEAD instance.
pub static AES_128_GCM: Tls13Aes128Gcm = Tls13Aes128Gcm;
/// AES-256-GCM AEAD instance.
pub static AES_256_GCM: Tls13Aes256Gcm = Tls13Aes256Gcm;

impl Tls13AeadAlgorithm for Tls13Aes128Gcm {
    fn encrypter(&self, key: AeadKey, iv: Iv) -> Box<dyn MessageEncrypter> {
        Box::new(Tls13Encrypter::<Aes128Gcm>::new(&key, iv))
    }

    fn decrypter(&self, key: AeadKey, iv: Iv) -> Box<dyn MessageDecrypter> {
        Box::new(Tls13Decrypter::<Aes128Gcm>::new(&key, iv))
    }

    fn key_len(&self) -> usize {
        16
    }

    fn extract_keys(
        &self,
        key: AeadKey,
        iv: Iv,
    ) -> Result<ConnectionTrafficSecrets, UnsupportedOperationError> {
        Ok(ConnectionTrafficSecrets::Aes128Gcm { key, iv })
    }

    fn fips(&self) -> bool {
        false
    }
}

impl Tls13AeadAlgorithm for Tls13Aes256Gcm {
    fn encrypter(&self, key: AeadKey, iv: Iv) -> Box<dyn MessageEncrypter> {
        Box::new(Tls13Encrypter::<Aes256Gcm>::new(&key, iv))
    }

    fn decrypter(&self, key: AeadKey, iv: Iv) -> Box<dyn MessageDecrypter> {
        Box::new(Tls13Decrypter::<Aes256Gcm>::new(&key, iv))
    }

    fn key_len(&self) -> usize {
        32
    }

    fn extract_keys(
        &self,
        key: AeadKey,
        iv: Iv,
    ) -> Result<ConnectionTrafficSecrets, UnsupportedOperationError> {
        Ok(ConnectionTrafficSecrets::Aes256Gcm { key, iv })
    }

    fn fips(&self) -> bool {
        false
    }
}

struct Tls13Encrypter<C> {
    key: C,
    iv: Iv,
}

impl<C: KeyInit> Tls13Encrypter<C> {
    fn new(key: &AeadKey, iv: Iv) -> Self {
        Self {
            // The caller arranges that `key` is `key_len()` bytes.
            key: C::new_from_slice(key.as_ref())
                .expect("caller arranges that `key` is `key_len()` bytes"),
            iv,
        }
    }
}

impl<C> MessageEncrypter for Tls13Encrypter<C>
where
    C: Aead + AeadCore<NonceSize = U12> + KeyInit + Send + Sync + 'static,
{
    fn encrypt(
        &mut self,
        msg: OutboundPlainMessage<'_>,
        seq: u64,
    ) -> Result<OutboundOpaqueMessage, Error> {
        let total_len = self.encrypted_payload_len(msg.payload.len());
        let nonce = AeadNonce::<C>::from(Nonce::new(&self.iv, seq).0);
        let aad = make_tls13_aad(total_len);

        let mut plain: Vec<u8> = msg.payload.to_vec();
        plain.extend_from_slice(&msg.typ.to_array());

        let ciphertext = self
            .key
            .encrypt(&nonce, Payload { msg: &plain, aad: &aad })
            .map_err(|_| Error::EncryptError)?;

        let mut payload = PrefixedPayload::with_capacity(total_len);
        payload.extend_from_slice(&ciphertext);

        Ok(OutboundOpaqueMessage::new(
            ContentType::ApplicationData,
            // TLS1.3 application data records use TLSv1_2 as the legacy record version
            // (RFC 8446 section 5.1).
            ProtocolVersion::TLSv1_2,
            payload,
        ))
    }

    fn encrypted_payload_len(&self, payload_len: usize) -> usize {
        payload_len + 1 + TAG_LEN
    }
}

struct Tls13Decrypter<C> {
    key: C,
    iv: Iv,
}

impl<C: KeyInit> Tls13Decrypter<C> {
    fn new(key: &AeadKey, iv: Iv) -> Self {
        Self {
            // The caller arranges that `key` is `key_len()` bytes.
            key: C::new_from_slice(key.as_ref())
                .expect("caller arranges that `key` is `key_len()` bytes"),
            iv,
        }
    }
}

impl<C> MessageDecrypter for Tls13Decrypter<C>
where
    C: Aead + AeadCore<NonceSize = U12> + KeyInit + Send + Sync + 'static,
{
    fn decrypt<'a>(
        &mut self,
        mut msg: InboundOpaqueMessage<'a>,
        seq: u64,
    ) -> Result<InboundPlainMessage<'a>, Error> {
        let payload = &mut msg.payload;
        if payload.len() < TAG_LEN {
            return Err(Error::DecryptError);
        }

        let nonce = AeadNonce::<C>::from(Nonce::new(&self.iv, seq).0);
        let aad = make_tls13_aad(payload.len());
        let plaintext = self
            .key
            .decrypt(&nonce, Payload { msg: &payload[..], aad: &aad })
            .map_err(|_| Error::DecryptError)?;

        payload[..plaintext.len()].copy_from_slice(&plaintext);
        payload.truncate(plaintext.len());

        msg.into_tls13_unpadded_message()
    }
}

#[cfg(test)]
mod tests {
    use rustls::crypto::cipher::OutboundChunks;

    use super::*;

    fn plain_message(data: &[u8]) -> OutboundPlainMessage<'_> {
        OutboundPlainMessage {
            typ: ContentType::ApplicationData,
            version: ProtocolVersion::TLSv1_2,
            payload: OutboundChunks::from(data),
        }
    }

    #[test]
    fn aes256_encrypt_decrypt_roundtrip() {
        let mut encrypter = AES_256_GCM.encrypter(AeadKey::from([7_u8; 32]), Iv::from([9_u8; 12]));
        let mut decrypter = AES_256_GCM.decrypter(AeadKey::from([7_u8; 32]), Iv::from([9_u8; 12]));

        let data = b"hello smartnet tls1.3 record layer";
        assert_eq!(encrypter.encrypted_payload_len(data.len()), data.len() + 1 + TAG_LEN);

        let opaque = encrypter.encrypt(plain_message(data), 5).unwrap();
        assert_eq!(opaque.typ, ContentType::ApplicationData);
        let mut ciphertext = opaque.payload.as_ref().to_vec();
        assert_eq!(ciphertext.len(), data.len() + 1 + TAG_LEN);

        let inbound = InboundOpaqueMessage::new(
            ContentType::ApplicationData,
            ProtocolVersion::TLSv1_2,
            &mut ciphertext,
        );
        let plain = decrypter.decrypt(inbound, 5).unwrap();
        assert_eq!(plain.typ, ContentType::ApplicationData);
        assert_eq!(plain.payload, data.as_slice());
    }

    #[test]
    fn tampered_ciphertext_is_rejected() {
        let mut encrypter = AES_256_GCM.encrypter(AeadKey::from([7_u8; 32]), Iv::from([9_u8; 12]));
        let mut decrypter = AES_256_GCM.decrypter(AeadKey::from([7_u8; 32]), Iv::from([9_u8; 12]));

        let data = b"authenticity matters";
        let opaque = encrypter.encrypt(plain_message(data), 0).unwrap();
        let mut ciphertext = opaque.payload.as_ref().to_vec();
        ciphertext[3] ^= 0x01;

        let inbound = InboundOpaqueMessage::new(
            ContentType::ApplicationData,
            ProtocolVersion::TLSv1_2,
            &mut ciphertext,
        );
        assert!(matches!(decrypter.decrypt(inbound, 0), Err(Error::DecryptError)));
    }

    #[test]
    fn wrong_sequence_number_is_rejected() {
        let mut encrypter = AES_256_GCM.encrypter(AeadKey::from([7_u8; 32]), Iv::from([9_u8; 12]));
        let mut decrypter = AES_256_GCM.decrypter(AeadKey::from([7_u8; 32]), Iv::from([9_u8; 12]));

        let opaque = encrypter.encrypt(plain_message(b"seq matters"), 1).unwrap();
        let mut ciphertext = opaque.payload.as_ref().to_vec();

        let inbound = InboundOpaqueMessage::new(
            ContentType::ApplicationData,
            ProtocolVersion::TLSv1_2,
            &mut ciphertext,
        );
        assert!(matches!(decrypter.decrypt(inbound, 2), Err(Error::DecryptError)));
    }

    #[test]
    fn short_records_are_rejected_before_decryption() {
        let mut decrypter = AES_256_GCM.decrypter(AeadKey::from([7_u8; 32]), Iv::from([9_u8; 12]));
        let mut short = vec![0_u8; TAG_LEN - 1];
        let inbound = InboundOpaqueMessage::new(
            ContentType::ApplicationData,
            ProtocolVersion::TLSv1_2,
            &mut short,
        );
        assert!(matches!(decrypter.decrypt(inbound, 0), Err(Error::DecryptError)));
    }
}

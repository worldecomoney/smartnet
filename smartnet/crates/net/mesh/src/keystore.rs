//! Node identity keystore wrapper.
//!
//! This module contains no decryption code. The ONE encrypted-keystore decryption function is
//! `smartnet_validator::keystore_util::load_secret_from_keystore`; node boot and every test use
//! that same function. Here we only wrap the in-memory encrypted material as a mesh [`Identity`].
//!
//! `SEAL = true` gates *sealing* (a validator behavior); it does not gate identity. Every node —
//! validator or not — loads its mesh identity at bootstrap from an encrypted Ethereum JSON v3
//! keystore. A node with no verifiable identity does not join the mesh ("broken = no have").
//!
//! Headless by contract: the password comes **only** from `PASSWORD_FILE`, never a TTY prompt.
//! Missing keystore, plaintext key file, wrong password, empty/missing password file, or a
//! `KEY_PATH` holding more than one file without `COINBASE` selecting one are all fatal: the
//! caller aborts boot. `COINBASE` is an optional node-level selector; it never changes identity.

use alloy_primitives::Address;
use eyre::Result;
use smartnet_validator::keystore_util::KeystoreMaterial;
use std::path::Path;

use crate::identity::Identity;

/// Loads the node's mesh identity from the encrypted JSON v3 keystore named by the config.
///
/// `key_dir` is `KEY_PATH`; the keystore is gathered (exactly one file) or selected by the
/// optional node-level `COINBASE` address in its UTC filename. `password_file` is the operator's
/// `PASSWORD_FILE`. The shared loader puts both file contents into RAM; this wrapper only builds
/// [`Identity`]. No decrypted signing key is retained.
pub fn load_identity_from_keystore(
    key_dir: &Path,
    password_file: &Path,
    network_id: u64,
    coinbase: Option<Address>,
) -> Result<Identity> {
    let material = KeystoreMaterial::load(key_dir, password_file, coinbase)?;
    Ok(Identity::new(material, network_id))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::identity::secret_key_address;
    use alloy_primitives::B256;
    use alloy_signer_local::PrivateKeySigner;
    use rand_08::rngs::OsRng;
    use smartnet_validator::keystore_util::load_secret_from_keystore;
    use std::path::{Path, PathBuf};

    const KEY_HEX: &str = "0x59c6995e998f97a5a0044966f0945389dc9e86dae88c7a8412f4603b6b78690d";
    const KEY_ADDRESS: &str = "0x70997970c51812dc3a010c7d01b50e0d17dc79c8";
    const OTHER_KEY_HEX: &str =
        "0xac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80";

    fn key_from_hex(hex: &str) -> k256::ecdsa::SigningKey {
        let bytes: B256 = hex.parse().expect("valid key hex");
        k256::ecdsa::SigningKey::from_slice(bytes.as_slice()).expect("valid scalar")
    }

    fn test_key() -> k256::ecdsa::SigningKey {
        key_from_hex(KEY_HEX)
    }

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir()
            .join(format!("smartnet-mesh-keystore-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("create temp dir");
        dir
    }

    /// Test fixture builder only: encryption is not the single shared decryption path.
    fn write_keystore(dir: &Path, key_file: &str, key: &k256::ecdsa::SigningKey, password: &str) {
        std::fs::create_dir_all(dir).expect("create dir");
        let bytes = key.to_bytes();
        let mut rng = OsRng;
        PrivateKeySigner::encrypt_keystore(
            dir,
            &mut rng,
            bytes,
            password.as_bytes(),
            Some(key_file),
        )
        .expect("encrypt keystore");
    }

    #[test]
    fn round_trip_decrypts_to_the_same_address() {
        let dir = temp_dir("roundtrip");
        write_keystore(&dir, "key.json", &test_key(), "pw");
        let raw = std::fs::read(dir.join("key.json")).expect("read keystore bytes");
        let loaded = load_secret_from_keystore(&raw, b"pw").expect("decrypt");
        assert_eq!(format!("{:#x}", secret_key_address(&loaded)), KEY_ADDRESS);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn refuses_plaintext_and_wrong_password() {
        // A bare hex key is not an encrypted keystore.
        let err = load_secret_from_keystore(KEY_HEX.as_bytes(), b"pw").unwrap_err();
        assert!(err.to_string().contains("plaintext"), "got: {err:?}");

        let dir = temp_dir("refuse");
        write_keystore(&dir, "key.json", &test_key(), "right");
        let raw = std::fs::read(dir.join("key.json")).expect("read keystore bytes");
        let err = load_secret_from_keystore(&raw, b"wrong").unwrap_err();
        assert!(err.to_string().contains("failed to decrypt"), "got: {err:?}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn load_identity_from_keystore_is_password_file_only_and_gathers_the_key() {
        let dir = temp_dir("identity");
        let key = test_key();
        let key_dir = dir.join("keys");
        std::fs::create_dir(&key_dir).expect("key dir");
        write_keystore(&key_dir, "operator-chose-this-name.json", &key, "pw");
        let pw = dir.join("pw.txt");
        std::fs::write(&pw, "pw\n").expect("write pw");

        let id = load_identity_from_keystore(&key_dir, &pw, 7, None).expect("load");
        assert_eq!(format!("{:#x}", id.address()), KEY_ADDRESS);
        assert_eq!(id.network_id(), 7);

        let err = load_identity_from_keystore(&key_dir, &dir.join("nope.txt"), 7, None)
            .unwrap_err()
            .to_string();
        assert!(err.contains("no interactive prompt"), "got: {err}");

        let empty = dir.join("empty.txt");
        std::fs::write(&empty, "").expect("write empty");
        let err = load_identity_from_keystore(&key_dir, &empty, 7, None).unwrap_err().to_string();
        assert!(err.contains("blank"), "got: {err}");

        let none = dir.join("none");
        std::fs::create_dir(&none).expect("empty dir");
        let err = load_identity_from_keystore(&none, &pw, 7, None).unwrap_err().to_string();
        assert!(err.contains("no keystore"), "got: {err}");

        std::fs::write(key_dir.join("stray.txt"), "x").expect("stray");
        let err = load_identity_from_keystore(&key_dir, &pw, 7, None).unwrap_err().to_string();
        assert!(err.contains("expected exactly 1"), "got: {err}");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn coinbase_selects_one_keystore_among_many() {
        let dir = temp_dir("coinbase-select");
        let key_dir = dir.join("keys");
        std::fs::create_dir(&key_dir).expect("key dir");
        let pw = dir.join("pw.txt");
        std::fs::write(&pw, "pw\n").expect("write pw");

        let first = test_key();
        write_keystore(&key_dir, &format!("UTC--0000--{}", &KEY_ADDRESS[2..]), &first, "pw");
        let other = key_from_hex(OTHER_KEY_HEX);
        let other_addr = secret_key_address(&other);
        let other_suffix = format!("{other_addr:#x}")[2..].to_string();
        write_keystore(&key_dir, &format!("UTC--0001--{other_suffix}"), &other, "pw");

        let id = load_identity_from_keystore(&key_dir, &pw, 7, Some(other_addr)).expect("select");
        assert_eq!(format!("{:#x}", id.address()), format!("{other_addr:#x}"));
        assert_eq!(id.network_id(), 7);

        let err = load_identity_from_keystore(&key_dir, &pw, 7, None).unwrap_err().to_string();
        assert!(err.contains("expected exactly 1"), "got: {err}");
        let err = load_identity_from_keystore(&key_dir, &pw, 7, Some(Address::repeat_byte(0x11)))
            .unwrap_err()
            .to_string();
        assert!(err.contains("COINBASE"), "got: {err}");

        let _ = std::fs::remove_dir_all(&dir);
    }
}

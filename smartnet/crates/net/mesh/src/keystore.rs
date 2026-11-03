//! The one shared keystore loader for node identity (see `doc/tmp/MESH-PAYLOAD-PROTOCOL-NEW.md`).
//!
//! Every node — validator or not — loads its mesh identity key at bootstrap from an
//! encrypted Ethereum JSON v3 keystore. `SEAL = true` gates *sealing* (a validator
//! behavior); it does not gate identity. A node with no verifiable identity does not join
//! the mesh ("broken = no have").
//!
//! Headless by contract: the password comes **only** from `PASSWORD_FILE`. There is no TTY
//! prompt on the bootstrap path — a reboot must never block on a human. Missing keystore,
//! plaintext key file, wrong password, empty/missing password file, or an empty `KEY_FILE`
//! are all fatal: the caller aborts the boot.
//!
//! The decrypted key lives in memory only, decrypted once, and is never re-persisted or
//! logged. `Debug` on [`crate::identity::Identity`] redacts the key.

use alloy_primitives::Address;
use alloy_signer_local::PrivateKeySigner;
use eyre::{Context, Result};
use k256::ecdsa::SigningKey;
use std::path::Path;

use crate::identity::{secret_key_address, Identity};

/// Loads the node's mesh identity from an encrypted JSON v3 keystore.
///
/// `key_dir` + `key_file` name the keystore (`KEY_PATH` / `KEY_FILE`); `password_file` is
/// the operator's `PASSWORD_FILE`. The password is read from that file only — never
/// prompted. Any failure is an error the caller treats as fatal for boot.
///
/// This is the single decrypt path for all node/validator functionality; there is no
/// separate test-only loader. Returns the in-memory [`Identity`], memory-only.
pub fn load_identity_from_keystore(
    key_dir: &Path,
    key_file: &str,
    password_file: &Path,
    network_id: u64,
) -> Result<Identity> {
    if key_file.trim().is_empty() {
        eyre::bail!(
            "KEY_FILE is empty (keystore not created); run `smartnet-validator keystore` first"
        );
    }
    let password = read_password_file(password_file)?;
    let keystore_path = key_dir.join(key_file);
    let key = load_secret_from_keystore(&keystore_path, &password)?;
    Ok(Identity::new(key, network_id))
}

/// Reads the keystore password from a file, refusing a blank one. Headless: no prompt.
fn read_password_file(password_file: &Path) -> Result<String> {
    let raw = std::fs::read(password_file).with_context(|| {
        format!(
            "read PASSWORD_FILE {} failed — bootstrap requires a password file, no \
             interactive prompt",
            password_file.display()
        )
    })?;
    let password = String::from_utf8_lossy(&raw)
        .trim_end_matches(['\n', '\r'])
        .to_string();
    if password.is_empty() {
        eyre::bail!(
            "PASSWORD_FILE {} is empty — refusing to boot with a blank keystore password",
            password_file.display()
        );
    }
    Ok(password)
}

/// Decrypts an encrypted Ethereum JSON v3 keystore to a [`SigningKey`].
///
/// Refuses a plaintext (bare-hex) key file: only encrypted JSON keystores are accepted,
/// same as geth. A missing file, a plaintext key, or a wrong/corrupted password are all
/// errors.
pub fn load_secret_from_keystore(keystore_path: &Path, password: &str) -> Result<SigningKey> {
    if !keystore_path.is_file() {
        eyre::bail!(
            "keystore {} is missing — create it with `smartnet-validator keystore`",
            keystore_path.display()
        );
    }
    if looks_like_plain_hex_key_file(keystore_path) {
        eyre::bail!(
            "keystore {} is a plaintext private key — must be encrypted JSON, not a \
             plaintext key file",
            keystore_path.display()
        );
    }
    let signer = PrivateKeySigner::decrypt_keystore(keystore_path, password.as_bytes())
        .map_err(|e| eyre::eyre!("failed to decrypt keystore {}: {e}", keystore_path.display()))?;
    let bytes = signer.to_bytes();
    let mut arr = [0u8; 32];
    arr.copy_from_slice(bytes.as_slice());
    let key = SigningKey::from_slice(&arr).map_err(|e| eyre::eyre!("invalid key: {e}"))?;
    // Log the derived address (never the key) so an operator can line it up with the
    // peers-file `key_id` allowlist.
    let address: Address = secret_key_address(&key);
    tracing::info!(
        target: "smartnet::mesh",
        keystore = %keystore_path.display(),
        address = %format!("{address:#x}"),
        "node identity keystore decrypted (memory-only)"
    );
    Ok(key)
}

/// A bare-hex (or `0x`-prefixed) 32-byte key file — refused: identity must be encrypted.
fn looks_like_plain_hex_key_file(path: &Path) -> bool {
    let Ok(raw) = std::fs::read_to_string(path) else {
        return false;
    };
    let trimmed = raw.trim();
    if trimmed.starts_with('{') {
        return false;
    }
    let hex = trimmed.strip_prefix("0x").unwrap_or(trimmed);
    hex.len() == 64 && hex.chars().all(|c| c.is_ascii_hexdigit())
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_signer_local::PrivateKeySigner;
    use k256::ecdsa::SigningKey;
    use rand_08::rngs::OsRng;
    use std::path::{Path, PathBuf};

    const KEY_HEX: &str = "0x59c6995e998f97a5a0044966f0945389dc9e86dae88c7a8412f4603b6b78690d";
    const KEY_ADDRESS: &str = "0x70997970c51812dc3a010c7d01b50e0d17dc79c8";

    fn test_key() -> SigningKey {
        let b: [u8; 32] = {
            let hex = KEY_HEX.trim_start_matches("0x");
            let mut out = [0u8; 32];
            for i in 0..32 {
                out[i] = u8::from_str_radix(&hex[i * 2..i * 2 + 2], 16).expect("hex byte");
            }
            out
        };
        SigningKey::from_slice(&b).expect("valid scalar")
    }

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir()
            .join(format!("smartnet-mesh-keystore-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("create temp dir");
        dir
    }

    /// Encrypt a key to an Ethereum JSON v3 keystore (test fixture builder).
    fn write_keystore(dir: &Path, key_file: &str, key: &SigningKey, password: &str) {
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
        let loaded = load_secret_from_keystore(&dir.join("key.json"), "pw").expect("decrypt");
        assert_eq!(format!("{:#x}", secret_key_address(&loaded)), KEY_ADDRESS);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn refuses_missing_keystore_plaintext_and_wrong_password() {
        let dir = temp_dir("refuse");
        let key = test_key();

        let err =
            load_secret_from_keystore(&dir.join("nope.json"), "pw").unwrap_err().to_string();
        assert!(err.contains("missing"), "got: {err}");

        let plain = dir.join("plain.txt");
        std::fs::write(&plain, KEY_HEX).expect("write plaintext");
        let err = load_secret_from_keystore(&plain, "pw").unwrap_err().to_string();
        assert!(err.contains("plaintext"), "got: {err}");

        write_keystore(&dir, "key.json", &key, "right");
        let err =
            load_secret_from_keystore(&dir.join("key.json"), "wrong").unwrap_err().to_string();
        assert!(err.contains("failed to decrypt"), "got: {err}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn load_identity_from_keystore_is_password_file_only_and_fatal() {
        let dir = temp_dir("identity");
        let key = test_key();
        write_keystore(&dir, "key.json", &key, "pw");
        let pw = dir.join("pw.txt");
        std::fs::write(&pw, "pw\n").expect("write pw");

        let id = load_identity_from_keystore(&dir, "key.json", &pw, 7).expect("load");
        assert_eq!(format!("{:#x}", id.address()), KEY_ADDRESS);
        assert_eq!(id.network_id(), 7);

        // Missing password file → fatal (no interactive prompt).
        let err = load_identity_from_keystore(&dir, "key.json", &dir.join("nope.txt"), 7)
            .unwrap_err()
            .to_string();
        assert!(err.contains("no interactive prompt"), "got: {err}");

        // Empty password file → fatal (never boot with a blank password).
        let empty = dir.join("empty.txt");
        std::fs::write(&empty, "").expect("write empty");
        let err = load_identity_from_keystore(&dir, "key.json", &empty, 7)
            .unwrap_err()
            .to_string();
        assert!(err.contains("blank"), "got: {err}");

        // Empty KEY_FILE → fatal (keystore not created).
        let err = load_identity_from_keystore(&dir, "", &pw, 7)
            .unwrap_err()
            .to_string();
        assert!(err.contains("KEY_FILE is empty"), "got: {err}");

        let _ = std::fs::remove_dir_all(&dir);
    }
}

//! `keystore` subcommand — the only way a key enters the validator.
//!
//! `smartnet keystore --key <PRIVATE_KEY_FILE> --password <PW_FILE>` turns an operator's
//! plaintext key file into an encrypted keystore **in `KEY_PATH`**. The protocol, in order:
//!
//! 1. Derive the address from the key (optional `0x`, any case accepted). The key never appears as
//!    a CLI argument value (shell-history/`ps` leak vector) — only the *file path* does.
//! 2. Name the keystore `UTC--{timestamp}.{nanos}Z--{lowercase address, no 0x}`. That name is this
//!    command's own business: **nothing in the node ever names a key file** — boot GATHERS the
//!    single file sitting in `KEY_PATH` (`keystore_util::get_encrypted_key`), so any name works.
//! 3. Refuse to write when `KEY_PATH` already holds a file — **no overwrite, no
//!    auto-deletion** (the same analogy as `smartnet init` refusing a non-empty data dir):
//!    clearing the path is the operator's job, so a key change starts with an empty dir.
//! 4. Write the encrypted keystore into the now-empty `KEY_PATH`.
//! 5. **Only then** delete the plaintext source. If any earlier step failed, the source key is
//!    left alone — it is the operator's only copy and a failed import must not eat it.
//!
//! **`config.toml` is never edited** (user-in #2): `KEY_FILE` is gone from the schema, so there is
//! nothing left here to keep in sync — the keystore is found by gathering `KEY_PATH` at boot.

use std::path::Path;

use alloy_primitives::Address;
use eyre::{bail, Context, Result};
use k256::ecdsa::SigningKey;
use tracing::info;

use crate::{
    config::load_import_config,
    keystore_util::{init_keystore_at, resolve_password_for_init, utc_keystore_filename},
    outcome::{from_eyre, ValidatorStatus},
    run_once::signer_address,
    Cli,
};

/// Decode a 32-byte hex private key, with or without a `0x`/`0X` prefix, in either case.
///
/// Deliberately hand-rolled rather than pulled from a hex crate: it is 32 bytes, and the error
/// messages are for an operator who just pasted a key, not for a library user.
fn decode_hex_key(raw: &str) -> Result<[u8; 32]> {
    let text = raw.trim();
    let text = text.strip_prefix("0x").or_else(|| text.strip_prefix("0X")).unwrap_or(text);
    if text.len() != 64 {
        bail!(
            "private key must be 64 hex characters (32 bytes), got {} after the 0x prefix",
            text.len()
        );
    }
    let bytes = text.as_bytes();
    let mut out = [0u8; 32];
    for (i, pair) in bytes.as_chunks::<2>().0.iter().enumerate() {
        let hi = (pair[0] as char).to_digit(16).ok_or_else(|| {
            eyre::eyre!("private key contains a non-hex character {:?}", pair[0] as char)
        })?;
        let lo = (pair[1] as char).to_digit(16).ok_or_else(|| {
            eyre::eyre!("private key contains a non-hex character {:?}", pair[1] as char)
        })?;
        out[i] = ((hi << 4) | lo) as u8;
    }
    Ok(out)
}

/// Parse a plaintext key file's contents into a signing key.
pub(crate) fn parse_private_key(raw: &str) -> Result<SigningKey> {
    let bytes = decode_hex_key(raw)?;
    SigningKey::from_slice(&bytes).context("private key is not a valid secp256k1 scalar")
}

/// Address in the form the config must carry: `0x` + lowercase hex (`VAL.md`: "ALL records must
/// be lowercase").
fn lowercase_address(address: Address) -> String {
    format!("{address:#x}")
}

pub(crate) fn keystore_import_cmd(
    cli: &Cli,
    private_key_file: &Path,
    password_file: &Path,
) -> ValidatorStatus {
    let cfg_path = match cli.resolved_config() {
        Ok(p) => p,
        Err(e) => return from_eyre(e),
    };
    let cfg = match load_import_config(&cfg_path) {
        Ok(c) => c,
        Err(e) => return from_eyre(e),
    };

    let secret = match std::fs::read_to_string(private_key_file)
        .with_context(|| format!("read private key file {}", private_key_file.display()))
    {
        Ok(s) => s,
        Err(e) => return from_eyre(e),
    };
    let secret = match parse_private_key(&secret) {
        Ok(k) => k,
        Err(e) => return from_eyre(e),
    };
    let address = signer_address(&secret);
    let address_lower = lowercase_address(address);

    // The target name is the conventional `UTC--…--<address>` one. It is written into
    // `KEY_PATH` and never read back by name: boot gathers whatever single file is there.
    let key_file = utc_keystore_filename(address);

    let password = match resolve_password_for_init(Some(password_file)) {
        Ok(p) => p,
        Err(e) => return from_eyre(e),
    };

    // `KEY_PATH` must be EMPTY (operator rule — same analogy as `smartnet init` refusing a
    // non-empty data dir): this command **never overwrites and never deletes**. Clearing
    // the path is the operator's job; a non-empty one is an error exit, never a warning.
    if let Ok(entries) = std::fs::read_dir(&cfg.key_dir) {
        let existing: Vec<String> = entries
            .filter_map(Result::ok)
            .map(|entry| entry.path().display().to_string())
            .collect();
        if !existing.is_empty() {
            return from_eyre(eyre::eyre!(
                "keystore not empty — {} already holds {} file(s): delete them yourself \
                 first (no overwriting, no auto-deletion)",
                cfg.key_dir.display(),
                existing.len()
            ));
        }
    }


    // Step 3 — the encrypted keystore.
    if let Err(e) = init_keystore_at(&cfg.key_dir, &key_file, &secret, &password) {
        return from_eyre(e);
    }

    // Step 4 — only now is the plaintext key redundant. A failure here is reported loudly:
    // the keystore is already correct (config.toml was never touched), so the operator just
    // deletes the file.
    if let Err(e) = std::fs::remove_file(private_key_file).with_context(|| {
        format!(
            "imported, but could not delete the source key {} — delete it manually",
            private_key_file.display()
        )
    }) {
        return from_eyre(e);
    }

    info!(
        target: "smartnet_validator",
        event = "KEYSTORE_OK",
        address = %address_lower,
        key_dir = %cfg.key_dir.display(),
        key_file = %key_file,
        config = %cfg_path.display(),
        password_file = %password_file.display(),
        source_deleted = %private_key_file.display(),
        "private key imported: encrypted keystore written, config updated, source file deleted"
    );
    ValidatorStatus::Ok
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::keystore_util::load_secret_from_keystore;

    /// A fixed key so the derived address is stable across runs.
    const KEY_HEX: &str = "0x59c6995e998f97a5a0044966f0945389dc9e86dae88c7a8412f4603b6b78690d";
    /// The address of the key above (the well-known second Hardhat account).
    const KEY_ADDRESS: &str = "0x70997970c51812dc3a010c7d01b50e0d17dc79c8";

    #[test]
    fn accepts_prefix_any_case_and_surrounding_whitespace() {
        for raw in [
            KEY_HEX,
            &KEY_HEX[2..],
            &KEY_HEX.to_uppercase().replace("0X", "0x"),
            &format!("  {KEY_HEX}\n"),
            &KEY_HEX.to_uppercase(),
        ] {
            let key = parse_private_key(raw).unwrap_or_else(|e| panic!("{raw:?} must parse: {e}"));
            assert_eq!(
                lowercase_address(signer_address(&key)),
                KEY_ADDRESS,
                "wrong address for {raw:?}"
            );
        }
    }

    #[test]
    fn rejects_bad_length_bad_hex_and_zero_scalar() {
        assert!(
            parse_private_key("0xdeadbeef").unwrap_err().to_string().contains("64 hex characters"),
            "short key must name the required length"
        );
        assert!(
            parse_private_key(&format!("{}zz", &KEY_HEX[..64]))
                .unwrap_err()
                .to_string()
                .contains("non-hex character"),
            "non-hex input must be named"
        );
        // Valid length, all zeros — not a valid scalar.
        assert!(parse_private_key(&"0".repeat(64)).is_err(), "the zero scalar must be rejected");
    }

    #[test]
    fn address_is_written_lowercase_with_0x() {
        let parsed = parse_private_key(KEY_HEX).unwrap();
        assert_eq!(lowercase_address(signer_address(&parsed)), KEY_ADDRESS);
        // Round-trips through the config parser.
        let back: Address = KEY_ADDRESS.parse().expect("lowercase address parses");
        assert_eq!(lowercase_address(back), KEY_ADDRESS);
    }

    /// End-to-end: keystore written into `KEY_PATH`, **`config.toml` never edited**,
    /// source deleted — and the key comes back by GATHERING (any filename, exactly one file).
    #[test]
    fn keystore_import_writes_keystore_never_edits_config_and_deletes_source() {
        use crate::config::load_validator_config;

        let dir =
            std::env::temp_dir().join(format!("smartnet-val-import-e2e-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("temp dir");

        let cfg_path = dir.join("config.toml");
        let source = dir.join("private_key.txt");
        std::fs::write(&source, KEY_HEX).expect("write source key");
        let config_before = format!(
            "KEY_PATH = \"{}\"\nPASSWORD_FILE = \"\"\nSEAL = true\n",
            dir.join("keys").display()
        );
        std::fs::write(&cfg_path, &config_before).expect("write config");

        // Run the real command once — password file already holds the password.
        let password_file = dir.join("password.txt");
        std::fs::write(&password_file, "test-password").expect("write password");
        let cli = crate::Cli { command: None, config: Some(cfg_path.clone()), dump_payload: None };
        let status = keystore_import_cmd(&cli, &source, &password_file);
        assert!(matches!(status, ValidatorStatus::Ok), "keystore import must succeed: {status:?}");

        // Source is gone, the keystore is in KEY_PATH, and `config.toml` is untouched.
        assert!(!source.exists(), "source key must be deleted");
        let config_after = std::fs::read_to_string(&cfg_path).expect("read config back");
        assert_eq!(config_after, config_before, "config.toml must NEVER be edited");
        assert!(!config_after.contains("KEY_FILE"), "KEY_FILE is gone from the schema");

        let cfg = load_validator_config(&cfg_path).expect("config reloads");
        // GATHERED, never named: the single file in KEY_PATH, whatever it is called.
        let gathered =
            crate::keystore_util::get_encrypted_key(&cfg.key_dir).expect("exactly one keystore");
        assert!(
            gathered.file_name().is_some(),
            "gathered path must name a file: {gathered:?}"
        );
        // The keystore really does hold this key, and is encrypted, not plaintext.
        let loaded = load_secret_from_keystore(&gathered, "test-password")
            .unwrap_or_else(|e| panic!("keystore reads back: {e:?}"));
        assert_eq!(lowercase_address(signer_address(&loaded)), KEY_ADDRESS);
        let on_disk = std::fs::read_to_string(&gathered).expect("read json");
        assert!(on_disk.contains("crypto"), "must be an encrypted keystore: {on_disk}");

        // A key change is the OPERATOR's job: `KEY_PATH` must be cleared first — this
        // command **never overwrites and never deletes** (same analogy as `smartnet init`
        // refusing a non-empty data dir). The second run must therefore be refused, and
        // it must leave everything exactly as it was.
        let source3 = dir.join("private_key_3.txt");
        std::fs::write(
            &source3,
            "0x1111111111111111111111111111111111111111111111111111111111111111",
        )
        .expect("write third key");
        let status = keystore_import_cmd(&cli, &source3, &password_file);
        let message = format!("{status:?}");
        assert!(
            !matches!(status, ValidatorStatus::Ok),
            "a non-empty KEY_PATH must be refused: {message}"
        );
        assert!(
            message.contains("keystore not empty"),
            "the refusal must name the reason: {message}"
        );
        // Nothing moved: the old keystore is still the only file, the new source key is
        // NOT deleted (no auto-deletion), and `config.toml` is byte-identical.
        assert!(source3.exists(), "a refused run must never eat the operator's source key");
        let cfg = load_validator_config(&cfg_path).expect("config reloads");
        assert_eq!(
            std::fs::read_to_string(&cfg_path).expect("config back"),
            config_before,
            "config.toml must NEVER be edited, not even on a refused key change"
        );
        let gathered =
            crate::keystore_util::get_encrypted_key(&cfg.key_dir).expect("exactly one keystore");
        let loaded = load_secret_from_keystore(&gathered, "test-password")
            .unwrap_or_else(|e| panic!("unchanged keystore reads back: {e:?}"));
        assert_eq!(lowercase_address(signer_address(&loaded)), KEY_ADDRESS);

        let _ = std::fs::remove_dir_all(&dir);
    }
}

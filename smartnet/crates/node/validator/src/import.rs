//! `keystore` subcommand — the only way a key enters the validator.
//!
//! `smartnet-validator keystore --key <PRIVATE_KEY_FILE> --password <PW_FILE>` turns an operator's
//! plaintext key file into an encrypted keystore and rewrites the config to match. The protocol,
//! in order:
//!
//! 1. Derive the address from the key (optional `0x`, any case accepted). The key never appears as
//!    a CLI argument value (shell-history/`ps` leak vector) — only the *file path* does.
//! 2. Name the keystore `UTC--{timestamp}.{nanos}Z--{lowercase address, no 0x}` — always, ignoring
//!    any `KEY_FILE` previously in the config.
//! 3. Write the encrypted keystore, **replacing** an existing one at the same name (this is the
//!    key-change flow: the same address implies the same key, so the overwrite is safe).
//! 4. Rewrite the config: `KEY_FILE` and `PASSWORD_FILE` only - the coinbase and the seed recipient
//!    are DERIVED from the private key and never live in config.toml.
//! 5. **Only then** delete the plaintext source. If any earlier step failed, the source key is left
//!    alone — it is the operator's only copy and a failed import must not eat it.
//!
//! The validator has no database, so `config.toml` *is* the state; editing it is the point.

use std::path::Path;

use alloy_primitives::Address;
use eyre::{bail, Context, Result};
use k256::ecdsa::SigningKey;
use tracing::info;

use crate::{
    config::{load_import_config, set_config_values},
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

    // The target name is always the conventional `UTC--…--<address>` one — the key-change
    // flow ignores whatever `KEY_FILE` held before (it is rewritten below).
    let key_file = utc_keystore_filename(address);

    let password = match resolve_password_for_init(Some(password_file)) {
        Ok(p) => p,
        Err(e) => return from_eyre(e),
    };

    // Belt and braces on the file itself: `init_keystore_at` also refuses overwrites by
    // default, but here the same address *is* the same key, so removing the stale target
    // first is the key-change behavior the command exists for. Only reached once the
    // password is resolved — a mid-flow failure must not disturb the old keystore.
    let target = cfg.key_dir.join(&key_file);
    if target.exists() &&
        let Err(e) = std::fs::remove_file(&target)
            .with_context(|| format!("remove replaced keystore {}", target.display()))
    {
        return from_eyre(e);
    }

    // Step 3 — the encrypted keystore.
    if let Err(e) = init_keystore_at(&cfg.key_dir, &key_file, &secret, &password) {
        return from_eyre(e);
    }

    // Step 4 — config: KEY_FILE and PASSWORD_FILE only. The coinbase is DERIVED from the
    // private key (never a config key anymore) and the seed recipient is derived too —
    // the merged flow stores them in the database, not in config.toml.
    if let Err(e) = set_config_values(
        &cfg_path,
        &[("KEY_FILE", key_file.as_str()), ("PASSWORD_FILE", &password_file.to_string_lossy())],
    ) {
        return from_eyre(e);
    }

    // Step 5 — only now is the plaintext key redundant. A failure here is reported loudly:
    // the keystore and config are already correct, so the operator just deletes the file.
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
    use crate::{config::apply_config_updates, keystore_util::load_secret_from_keystore};

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

    /// The config rewrite preserves everything it is not asked to touch, and appends
    /// missing keys.
    #[test]
    fn config_rewrite_sets_the_four_keys_and_keeps_the_rest() {
        let raw = "# comment\nKEY_PATH = \"/home/u/.smartnet/validator/secure\"\n\
                   KEY_FILE = \"\"\nCOINBASE = \"0x0000000000000000000000000000000000000000\"\n\
                   SEED_TX_TO = \"0x0000000000000000000000000000000000000000\"\n\
                   PASSWORD_FILE = \"\"\nNODE_URL = \"http://127.0.0.1:8545\"\n";
        let out = apply_config_updates(
            raw,
            &[
                ("KEY_FILE", "UTC--1--aabb"),
                ("PASSWORD_FILE", "~/.smartnet/validator/pw.txt"),
                ("COINBASE", KEY_ADDRESS),
                ("SEED_TX_TO", KEY_ADDRESS),
            ],
        );
        assert!(
            out.contains("KEY_PATH = \"/home/u/.smartnet/validator/secure\""),
            "comment/key kept:\n{out}"
        );
        assert!(out.contains("# comment"), "comment kept");
        assert!(out.contains("KEY_FILE = \"UTC--1--aabb\""));
        assert!(
            out.contains("PASSWORD_FILE = \"~/.smartnet/validator/pw.txt\""),
            "PASSWORD_FILE written:\n{out}"
        );
        assert!(out.contains(&format!("COINBASE = \"{KEY_ADDRESS}\"")));
        assert!(out.contains(&format!("SEED_TX_TO = \"{KEY_ADDRESS}\"")));
        assert!(out.contains("NODE_URL = \"http://127.0.0.1:8545\""), "untouched key kept");
        assert!(!out.contains("0x0000000000000000000000000000000000000000"), "replaced");
    }

    /// A key absent from the file is appended rather than silently skipped.
    #[test]
    fn config_rewrite_appends_missing_keys() {
        let out = apply_config_updates("KEY_PATH = \"/abs\"\n", &[("COINBASE", KEY_ADDRESS)]);
        assert!(out.starts_with("KEY_PATH = \"/abs\"\n"), "original kept:\n{out}");
        assert!(out.contains(&format!("COINBASE = \"{KEY_ADDRESS}\"")), "appended:\n{out}");
    }

    /// End-to-end: keystore written, config rewritten, source deleted, and changed again.
    #[test]
    fn keystore_import_writes_keystore_rewrites_config_and_deletes_source() {
        use crate::config::load_validator_config;

        let dir =
            std::env::temp_dir().join(format!("smartnet-val-import-e2e-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("temp dir");

        let cfg_path = dir.join("config.toml");
        let source = dir.join("private_key.txt");
        std::fs::write(&source, KEY_HEX).expect("write source key");
        std::fs::write(
            &cfg_path,
            format!(
                "KEY_PATH = \"{}\"\nKEY_FILE = \"\"\nPASSWORD_FILE = \"\"\nSEAL = true\n",
                dir.join("keys").display()
            ),
        )
        .expect("write config");

        // Run the real command once — password file already holds the password.
        let password_file = dir.join("password.txt");
        std::fs::write(&password_file, "test-password").expect("write password");
        let cli = crate::Cli { command: None, config: Some(cfg_path.clone()), dump_payload: None };
        let status = keystore_import_cmd(&cli, &source, &password_file);
        assert!(matches!(status, ValidatorStatus::Ok), "keystore import must succeed: {status:?}");

        // Source is gone, keystore exists, config points at the right address and password.
        assert!(!source.exists(), "source key must be deleted");
        let cfg = load_validator_config(&cfg_path).expect("config reloads");
        assert!(!cfg.key_file.trim().is_empty(), "KEY_FILE written");
        assert!(
            cfg.key_file.starts_with("UTC--"),
            "generated file must be UTC--…--<address>, got {:?}",
            cfg.key_file
        );
        assert!(
            cfg.key_dir.join(&cfg.key_file).is_file(),
            "keystore exists at {:?}",
            cfg.key_dir.join(&cfg.key_file)
        );
        assert_eq!(
            cfg.password_file.expect("PASSWORD_FILE written"),
            password_file,
            "path to password updated"
        );
        // The keystore really does hold this key, and is encrypted, not plaintext.
        let loaded = load_secret_from_keystore(&cfg.key_dir.join(&cfg.key_file), "test-password")
            .unwrap_or_else(|e| panic!("keystore reads back: {e:?}"));
        assert_eq!(lowercase_address(signer_address(&loaded)), KEY_ADDRESS);
        let on_disk = std::fs::read_to_string(cfg.key_dir.join(&cfg.key_file)).expect("read json");
        assert!(on_disk.contains("crypto"), "must be an encrypted keystore: {on_disk}");

        // Key change: the same command again with a *different* key must replace the
        // keystore and rewrite the config — this is the flow that never worked before.
        let source3 = dir.join("private_key_3.txt");
        std::fs::write(
            &source3,
            "0x1111111111111111111111111111111111111111111111111111111111111111",
        )
        .expect("write third key");
        let status = keystore_import_cmd(&cli, &source3, &password_file);
        assert!(matches!(status, ValidatorStatus::Ok), "key change must succeed: {status:?}");
        let address3 = lowercase_address(signer_address(
            &parse_private_key("1111111111111111111111111111111111111111111111111111111111111111")
                .expect("valid scalar"),
        ));
        // The config rewrites KEY_FILE (UTC--…--<new address>) — the derived identity follows.
        let cfg = load_validator_config(&cfg_path).expect("config reloads after change");
        assert!(
            cfg.key_file.contains(&address3[2..]),
            "KEY_FILE follows the new key: {:?}",
            cfg.key_file
        );
        assert!(cfg.key_dir.join(&cfg.key_file).is_file(), "new keystore exists after the change");
        assert!(!source3.exists(), "source deleted after the change too");
        // The new key's keystore decrypts with the same password file.
        let loaded = load_secret_from_keystore(&cfg.key_dir.join(&cfg.key_file), "test-password")
            .unwrap_or_else(|e| panic!("changed keystore reads back: {e:?}"));
        assert_eq!(lowercase_address(signer_address(&loaded)), address3);

        let _ = std::fs::remove_dir_all(&dir);
    }
}

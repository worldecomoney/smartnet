//! Encrypted Ethereum JSON keystore (version 3) via `eth-keystore` / `alloy-signer-local`.
//! Plaintext private keys are rejected.

use alloy_primitives::Address;
use alloy_signer_local::PrivateKeySigner;
use eyre::{Context, Result};
use k256::ecdsa::SigningKey;
use rand_08::rngs::OsRng;
use std::path::Path;

use crate::{outcome::ValidatorStatus, run_once::signer_address};

/// Resolve password for **`keystore` creation**: prompt when needed; may create the password file
/// (with confirm).
pub(crate) fn resolve_password_for_init(password_file: Option<&Path>) -> Result<String> {
    resolve_password(password_file, true, true)
}

/// Resolve password for normal run: single prompt only; never creates the password file.
pub(crate) fn resolve_password_for_run(password_file: Option<&Path>) -> Result<String> {
    resolve_password(password_file, false, false)
}

fn resolve_password(
    password_file: Option<&Path>,
    allow_create_file: bool,
    confirm_on_prompt: bool,
) -> Result<String> {
    let Some(path) = password_file else {
        return prompt_password("Keystore password: ", confirm_on_prompt);
    };

    match std::fs::read_to_string(path) {
        Ok(contents) if contents.trim().is_empty() => {
            let password = prompt_password(
                &format!("Password file {} is empty — enter keystore password: ", path.display()),
                confirm_on_prompt,
            )?;
            if allow_create_file {
                write_password_file(path, &password)?;
            }
            Ok(password)
        }
        Ok(contents) => Ok(contents.trim().to_string()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            let password = prompt_password(
                &format!("Password file {} not found — enter keystore password: ", path.display()),
                confirm_on_prompt,
            )?;
            if allow_create_file {
                write_password_file(path, &password)?;
            }
            Ok(password)
        }
        Err(e) => Err(e).with_context(|| format!("read password file {}", path.display())),
    }
}

fn write_password_file(path: &Path, password: &str) -> Result<()> {
    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("create password file parent {}", parent.display()))?;
        }
    // Owner-only (0600): readable solely by the account running the validator —
    // no root required, just no other users on the host. Refuse to clobber.
    #[cfg(unix)]
    {
        use std::{io::Write, os::unix::fs::OpenOptionsExt};
        let mut file = std::fs::OpenOptions::new()
            .mode(0o600)
            .write(true)
            .create_new(true)
            .open(path)
            .with_context(|| format!("create password file {}", path.display()))?;
        file.write_all(format!("{password}\n").as_bytes())
            .with_context(|| format!("write password file {}", path.display()))?;
        Ok(())
    }
    #[cfg(not(unix))]
    std::fs::write(path, format!("{password}\n"))
        .with_context(|| format!("write password file {}", path.display()))?;
}

fn prompt_password(prompt: &str, confirm: bool) -> Result<String> {
    eprint!("{prompt}");
    let first = rpassword::read_password().wrap_err("read password from terminal")?;
    if first.is_empty() {
        eyre::bail!("password must not be empty");
    }
    if !confirm {
        return Ok(first);
    }
    eprint!("Confirm password: ");
    let second = rpassword::read_password().wrap_err("read password confirmation from terminal")?;
    if first != second {
        eyre::bail!("passwords do not match");
    }
    Ok(first)
}

/// Create encrypted keystore at `key_dir` / `key_file` (must not exist).
pub(crate) fn init_keystore_at(
    key_dir: &Path,
    key_file: &str,
    secret: &SigningKey,
    password: &str,
) -> Result<Address> {
    if key_file.trim().is_empty() {
        eyre::bail!("KEY_FILE must not be empty when writing keystore");
    }
    if key_file.contains('/') || key_file.contains('\\') {
        eyre::bail!("KEY_FILE must be a filename only, not a path: {key_file}");
    }

    std::fs::create_dir_all(key_dir)
        .with_context(|| format!("create KEY_PATH directory {}", key_dir.display()))?;

    let keystore_path = key_dir.join(key_file);
    if keystore_path.exists() {
        eyre::bail!(
            "keystore already exists at {} (refusing to overwrite)",
            keystore_path.display()
        );
    }

    // Keys are always stored as encrypted Ethereum JSON keystores (same strategy as geth).
    // Plaintext private keys are never written; `load_secret_from_keystore` refuses to read
    // anything that is not an encrypted keystore.
    let sk_bytes = secret.to_bytes();
    let mut rng = OsRng;
    PrivateKeySigner::encrypt_keystore(
        key_dir,
        &mut rng,
        sk_bytes,
        password.as_bytes(),
        Some(key_file),
    )
    .map_err(|e| eyre::eyre!("encrypt keystore: {e}"))?;

    let written = key_dir.join(key_file);
    if !written.is_file() {
        eyre::bail!(
            "keystore expected at {} after keystore-write but file is missing",
            written.display()
        );
    }

    Ok(signer_address(secret))
}

/// Load signing key from encrypted JSON keystore.
/// Headless identity loader for cross-node/validator use: reads the password from
/// `PASSWORD_FILE` only (no TTY prompt), decrypts via [`load_secret_from_keystore`], and
/// returns the in-memory signing key. Missing password file, empty password, missing or
/// undecryptable keystore, or a non-keystore (plain-hex) file are all fatal — the caller
/// must abort bootstrap ("broken = no have"; we never boot with a guessed or blank key).
///
/// Returns `eyre::Result` (not the validator-internal `ValidatorStatus`) so this can be
/// called from outside the validator crate. The decrypted key is memory-only and never
/// re-persisted.
pub fn load_identity_from_config(
    key_dir: &Path,
    key_file: &str,
    password_file: &Path,
) -> Result<SigningKey> {
    if key_file.trim().is_empty() {
        eyre::bail!(
            "KEY_FILE is empty (keystore not created); run `smartnet-validator keystore` first"
        );
    }
    let password_bytes = std::fs::read(password_file).with_context(|| {
        format!(
            "read PASSWORD_FILE {} failed — headless bootstrap requires a password file, no interactive prompt",
            password_file.display()
        )
    })?;
    let password =
        String::from_utf8_lossy(&password_bytes).trim_end_matches(['\n', '\r']).to_string();
    if password.is_empty() {
        eyre::bail!(
            "PASSWORD_FILE {} is empty — refusing to boot with a blank keystore password",
            password_file.display()
        );
    }
    let keystore_path = key_dir.join(key_file);
    load_secret_from_keystore(&keystore_path, &password).map_err(|status| match status {
        ValidatorStatus::Failed { message } => eyre::eyre!(message),
        other => eyre::eyre!("keystore load failed: {other:?}"),
    })
}

pub(crate) fn load_secret_from_keystore(
    keystore_path: &Path,
    password: &str,
) -> Result<SigningKey, ValidatorStatus> {
    if !keystore_path.is_file() {
        return Err(ValidatorStatus::Failed {
            message: format!(
                "keystore {} is missing — run `smartnet-validator keystore` first",
                keystore_path.display()
            ),
        });
    }
    if looks_like_plain_hex_key_file(keystore_path) {
        return Err(ValidatorStatus::Failed {
            message: "keystore must be encrypted JSON, not a plaintext private key file".into(),
        });
    }

    let signer = match PrivateKeySigner::decrypt_keystore(keystore_path, password.as_bytes()) {
        Ok(s) => s,
        Err(_) => {
            return Err(ValidatorStatus::Failed {
                message: format!(
                    "failed to decrypt keystore {} — wrong password or corrupted file",
                    keystore_path.display()
                ),
            });
        }
    };
    let bytes = signer.to_bytes();
    let mut arr = [0u8; 32];
    arr.copy_from_slice(bytes.as_slice());
    SigningKey::from_slice(&arr)
        .map_err(|e| ValidatorStatus::Failed { message: format!("invalid key in keystore: {e}") })
}

/// Keystore filename: `UTC--{timestamp}--{address_lower_hex}`.
pub(crate) fn utc_keystore_filename(address: Address) -> String {
    let now = chrono::Utc::now();
    let ts = now.format("%Y-%m-%dT%H-%M-%S");
    let subsec = format!("{:09}", now.timestamp_subsec_nanos());
    format!("UTC--{ts}.{subsec}Z--{}", address.to_string().trim_start_matches("0x").to_lowercase())
}

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
    use alloy_primitives::B256;
    use std::path::PathBuf;

    /// Fixed key so the derived address is stable across runs (same as `import.rs`).
    const KEY_HEX: &str = "0x59c6995e998f97a5a0044966f0945389dc9e86dae88c7a8412f4603b6b78690d";
    /// The address of the key above (second Hardhat account), lowercase.
    const KEY_ADDRESS: &str = "0x70997970c51812dc3a010c7d01b50e0d17dc79c8";

    fn test_key() -> SigningKey {
        let b: B256 = KEY_HEX.parse().expect("valid key hex");
        SigningKey::from_slice(b.as_slice()).expect("valid scalar")
    }

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir()
            .join(format!("smartnet-val-keystore-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("create temp dir");
        dir
    }

    /// Encrypt then decrypt round-trips to the same address, and the file is JSON (not plaintext).
    #[test]
    fn keystore_round_trip_encrypt_then_decrypt() {
        let dir = temp_dir("roundtrip");
        let key = test_key();
        let addr = init_keystore_at(&dir, "key.json", &key, "pw").expect("init keystore");
        assert_eq!(format!("{addr:#x}"), KEY_ADDRESS);

        // The written file is an encrypted JSON keystore, never a bare hex key.
        let raw = std::fs::read_to_string(dir.join("key.json")).expect("read keystore");
        assert!(raw.trim_start().starts_with('{'), "must be JSON keystore");

        // Decrypting with the same password yields the same signing address.
        let loaded = load_secret_from_keystore(&dir.join("key.json"), "pw").expect("decrypt");
        assert_eq!(format!("{:#x}", signer_address(&loaded)), KEY_ADDRESS);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// `KEY_FILE` must be a bare filename — empty or any path separator is refused.
    #[test]
    fn init_keystore_rejects_empty_and_path_key_file() {
        let dir = temp_dir("reject-name");
        let key = test_key();
        assert!(init_keystore_at(&dir, "", &key, "pw").is_err(), "empty refused");
        assert!(
            init_keystore_at(&dir, "sub/key.json", &key, "pw").is_err(),
            "forward slash refused"
        );
        assert!(init_keystore_at(&dir, "sub\\key.json", &key, "pw").is_err(), "back slash refused");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A keystore already on disk is never overwritten.
    #[test]
    fn init_keystore_refuses_to_overwrite() {
        let dir = temp_dir("overwrite");
        let key = test_key();
        init_keystore_at(&dir, "key.json", &key, "pw").expect("first init");
        let err = init_keystore_at(&dir, "key.json", &key, "pw").unwrap_err();
        assert!(err.to_string().contains("already exists"), "got: {err}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Loading rejects a missing file, a plaintext hex key, and a wrong password.
    #[test]
    fn load_secret_rejects_missing_plaintext_and_wrong_password() {
        let dir = temp_dir("load");
        let key = test_key();

        // Missing keystore.
        let ValidatorStatus::Failed { message } =
            load_secret_from_keystore(&dir.join("nope.json"), "pw").unwrap_err()
        else {
            panic!("missing keystore must be Failed");
        };
        assert!(message.contains("missing"), "got: {message}");

        // A bare hex key file (not an encrypted keystore) is refused.
        let plain = dir.join("plain.txt");
        std::fs::write(&plain, KEY_HEX).expect("write plaintext");
        let ValidatorStatus::Failed { message } =
            load_secret_from_keystore(&plain, "pw").unwrap_err()
        else {
            panic!("plaintext key must be Failed");
        };
        assert!(message.contains("plaintext"), "got: {message}");

        // Encrypted keystore but the wrong password.
        init_keystore_at(&dir, "key.json", &key, "right").expect("init");
        let ValidatorStatus::Failed { message } =
            load_secret_from_keystore(&dir.join("key.json"), "wrong").unwrap_err()
        else {
            panic!("wrong password must be Failed");
        };
        assert!(message.contains("failed to decrypt"), "got: {message}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The generated keystore filename has the `UTC--…--<lowercase address>` shape.
    #[test]
    fn utc_filename_has_expected_shape() {
        let addr: Address = KEY_ADDRESS.parse().expect("valid address");
        let name = utc_keystore_filename(addr);
        assert!(name.starts_with("UTC--"), "got: {name}");
        assert!(name.contains("70997970c51812dc3a010c7d01b50e0d17dc79c8"),
            "lowercase address expected in {name}"
        );
    }

    /// Headless loader: password file only (no prompt), fatal on missing file, empty
    /// password, empty `KEY_FILE`, or missing keystore. Decrypt round-trips to the right address.
    #[test]
    fn load_identity_from_config_is_headless_and_fatal() {
        let dir = temp_dir("identity");
        let key = test_key();
        let key_file = "key.json";
        init_keystore_at(&dir, key_file, &key, "pw").expect("init keystore");

        // Happy path: password file present and non-empty → same address, no prompt.
        let pw = dir.join("pw.txt");
        std::fs::write(&pw, "pw\n").expect("write pw");
        let loaded = load_identity_from_config(&dir, key_file, &pw).expect("load");
        assert_eq!(format!("{:#x}", signer_address(&loaded)), KEY_ADDRESS);

        // Missing password file → fatal, mentions "no interactive prompt".
        let err = load_identity_from_config(&dir, key_file, &dir.join("nope.txt"))
            .unwrap_err()
            .to_string();
        assert!(err.contains("no interactive prompt"), "got: {err}");

        // Empty password file → fatal (never boot with a blank password).
        let empty = dir.join("empty.txt");
        std::fs::write(&empty, "").expect("write empty");
        let err = load_identity_from_config(&dir, key_file, &empty)
            .unwrap_err()
            .to_string();
        assert!(err.contains("blank"), "got: {err}");

        // Empty KEY_FILE → fatal (keystore not created).
        let err = load_identity_from_config(&dir, "", &pw).unwrap_err().to_string();
        assert!(err.contains("KEY_FILE is empty"), "got: {err}");

        // Missing keystore with a valid password file → fatal.
        let err = load_identity_from_config(&dir, "nope.json", &pw)
            .unwrap_err()
            .to_string();
        assert!(err.contains("missing"), "got: {err}");
        let _ = std::fs::remove_dir_all(&dir);
    }
}

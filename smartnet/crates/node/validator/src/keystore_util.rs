//! Encrypted Ethereum JSON keystore (version 3) via `eth-keystore` primitives.
//! Plaintext private keys are rejected.
//!
//! Security contract: the process keeps only the two file contents in memory — encrypted
//! keystore bytes and password bytes — in [`KeystoreMaterial`]. No long-lived `SigningKey` is
//! stored anywhere. The single decryption function [`load_secret_from_keystore`] is called only
//! around an actual signing operation, and the resulting key is dropped immediately.

use aes::{
    cipher::{InnerIvInit, KeyInit, StreamCipherCore},
    Aes128,
};
use alloy_primitives::Address;
use alloy_signer_local::PrivateKeySigner;
use ctr::{flavors::Ctr128BE, CtrCore};
use eth_keystore::{EthKeystore, KdfparamsType};
use eyre::{Context, Result};
use hmac::Hmac;
use k256::ecdsa::SigningKey;
use pbkdf2::pbkdf2;
use rand_08::rngs::OsRng;
use scrypt::{scrypt, Params as ScryptParams};
use sha2::Sha256;
use sha3::{Digest, Keccak256};
use std::path::{Path, PathBuf};
use zeroize::Zeroizing;

use crate::run_once::signer_address;

type Aes128Ctr = CtrCore<Aes128, Ctr128BE>;

/// Resolve password for **`keystore` creation**: prompt when needed; may create the password file
/// (with confirm). Runtime never uses this path — boot is headless and reads `PASSWORD_FILE`.
pub(crate) fn resolve_password_for_init(password_file: Option<&Path>) -> Result<String> {
    resolve_password(password_file)
}

fn resolve_password(password_file: Option<&Path>) -> Result<String> {
    let Some(path) = password_file else {
        return prompt_password("Keystore password: ", true);
    };

    match std::fs::read_to_string(path) {
        Ok(contents) if contents.trim().is_empty() => {
            let password = prompt_password(
                &format!("Password file {} is empty — enter keystore password: ", path.display()),
                true,
            )?;
            write_password_file(path, &password)?;
            Ok(password)
        }
        Ok(contents) => Ok(contents.trim().to_string()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            let password = prompt_password(
                &format!("Password file {} not found — enter keystore password: ", path.display()),
                true,
            )?;
            write_password_file(path, &password)?;
            Ok(password)
        }
        Err(e) => Err(e).with_context(|| format!("read password file {}", path.display())),
    }
}

fn write_password_file(path: &Path, password: &str) -> Result<()> {
    if let Some(parent) = path.parent() &&
        !parent.as_os_str().is_empty()
    {
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

/// The two file contents a node holds in RAM: the encrypted keystore and the password.
///
/// There is deliberately no `SigningKey` field. The public address is derived once at load time
/// and kept because it is not secret. Every signature call goes through [`Self::decrypt`], which
/// uses the ONE decrypt function and drops the key as soon as the signature is made. A RAM scan
/// therefore finds no naked 32-byte private key to lift.
#[derive(Clone)]
pub struct KeystoreMaterial {
    encrypted: Vec<u8>,
    password: Zeroizing<Vec<u8>>,
    address: Address,
}

impl std::fmt::Debug for KeystoreMaterial {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("KeystoreMaterial")
            .field("address", &format_args!("{:#x}", self.address))
            .finish_non_exhaustive()
    }
}

impl KeystoreMaterial {
    /// Load both files into RAM: the selected encrypted keystore and the password file. This is
    /// the headless runtime path — a missing or blank password file is fatal, never a prompt.
    ///
    /// `coinbase` is the optional node-level selector for a UTC filename; it never becomes an
    /// identity. The address here is derived from the key and used only in memory.
    pub fn load(key_dir: &Path, password_file: &Path, coinbase: Option<Address>) -> Result<Self> {
        let password = read_password_bytes(password_file)?;
        let keystore_path = get_encrypted_key(key_dir, coinbase)?;
        let encrypted = read_keystore_file(&keystore_path)?;
        Self::from_encrypted_bytes(encrypted, password.to_vec())
    }

    /// Build material from already-read bytes. This is also the test entry point: a test fixture
    /// encrypts a key to bytes and hands both buffers here; it never stores a bare `SigningKey`.
    pub fn from_encrypted_bytes(encrypted: Vec<u8>, password: Vec<u8>) -> Result<Self> {
        let password = Zeroizing::new(password);
        if password.is_empty() {
            eyre::bail!("keystore password must not be blank");
        }
        let address = {
            let key = load_secret_from_keystore(&encrypted, &password)?;
            signer_address(&key)
        };
        Ok(Self { encrypted, password, address })
    }

    /// Public address derived at load time. Safe to keep: it is not secret.
    pub const fn address(&self) -> Address {
        self.address
    }

    /// The ONE way to obtain a signing key: decrypt from the two in-memory buffers. The returned
    /// key must be dropped as soon as the signature is produced — never stored in a struct field.
    pub fn decrypt(&self) -> Result<SigningKey> {
        load_secret_from_keystore(&self.encrypted, &self.password)
    }

    /// Decrypt just long enough for `sign`, then drop the key. This is the preferred call shape
    /// for synchronous signing paths.
    pub fn with_key<T>(&self, sign: impl FnOnce(&SigningKey) -> T) -> Result<T> {
        let key = self.decrypt()?;
        Ok(sign(&key))
    }
}

fn read_password_bytes(path: &Path) -> Result<Zeroizing<Vec<u8>>> {
    let raw = Zeroizing::new(std::fs::read(path).with_context(|| {
        format!(
            "read PASSWORD_FILE {} failed — headless bootstrap requires a password file, no interactive prompt",
            path.display()
        )
    })?);
    let end = raw.iter().rposition(|b| *b != b'\n' && *b != b'\r').map_or(0, |i| i + 1);
    let password = Zeroizing::new(raw[..end].to_vec());
    if password.is_empty() {
        eyre::bail!(
            "PASSWORD_FILE {} is empty — refusing to boot with a blank keystore password",
            path.display()
        );
    }
    Ok(password)
}

fn read_keystore_file(path: &Path) -> Result<Vec<u8>> {
    if !path.is_file() {
        eyre::bail!("keystore {} is missing — run `smartnet keystore` first", path.display());
    }
    std::fs::read(path).with_context(|| format!("read keystore {}", path.display()))
}

/// Create encrypted keystore at `key_dir` / `key_file` (must not exist).
pub(crate) fn init_keystore_at(
    key_dir: &Path,
    key_file: &str,
    secret: &SigningKey,
    password: &str,
) -> Result<Address> {
    if key_file.trim().is_empty() {
        eyre::bail!("keystore filename must not be empty when writing");
    }
    if key_file.contains('/') || key_file.contains('\\') {
        eyre::bail!("keystore name must be a bare filename, not a path: {key_file}");
    }

    std::fs::create_dir_all(key_dir)
        .with_context(|| format!("create KEY_PATH directory {}", key_dir.display()))?;

    // The WRITER names its own file — `key_file` here is this function's parameter, not
    // a config key. Reading is the other half of user-in #2: every *reader* GATHERS the
    // single file in `key_dir` instead of naming one.
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

/// Pick the encrypted keystore inside `key_dir`.
///
/// The rule lives in `smartnet-fs-util` so every node role uses the same node-level `COINBASE`
/// selector: with it, exactly one matching UTC filename is picked and other files are ignored;
/// without it, `key_dir` must hold exactly one file. Everything else is a fatal error.
pub fn get_encrypted_key(key_dir: &Path, coinbase: Option<Address>) -> Result<PathBuf> {
    smartnet_fs_util::keystore_selector::select_keystore(key_dir, coinbase)
}

/// The ONE encrypted-keystore decryption function in `SmartNet`. Every caller — node boot, mesh
/// identity, and tests — must use this function; no other module implements keystore crypto.
///
/// It takes the already-loaded encrypted JSON bytes and password bytes, decrypts in place, and
/// returns a transient `SigningKey`. The key is never persisted, logged, or kept by this module.
pub fn load_secret_from_keystore(encrypted: &[u8], password: &[u8]) -> Result<SigningKey> {
    let text = std::str::from_utf8(encrypted).context("keystore is not UTF-8 JSON")?;
    if !text.trim_start().starts_with('{') {
        eyre::bail!("keystore must be encrypted JSON, not a plaintext private key file");
    }
    let keystore: EthKeystore =
        serde_json::from_str(text).context("parse encrypted keystore JSON")?;
    if keystore.version != 3 {
        eyre::bail!("unsupported keystore version {}", keystore.version);
    }
    if keystore.crypto.cipher != "aes-128-ctr" {
        eyre::bail!("unsupported keystore cipher {}", keystore.crypto.cipher);
    }

    let derived = Zeroizing::new(derive_key(&keystore.crypto.kdfparams, password)?);
    let mut mac = Keccak256::new();
    mac.update(&derived[16..32]);
    mac.update(&keystore.crypto.ciphertext);
    if mac.finalize().as_slice() != keystore.crypto.mac.as_slice() {
        eyre::bail!("failed to decrypt keystore — wrong password or corrupted file");
    }

    let iv = &keystore.crypto.cipherparams.iv;
    if iv.len() < 16 {
        eyre::bail!("invalid keystore IV length {}", iv.len());
    }
    let mut plaintext = Zeroizing::new(keystore.crypto.ciphertext.clone());
    let cipher = Aes128Ctr::inner_iv_slice_init(
        Aes128::new_from_slice(&derived[..16])
            .map_err(|e| eyre::eyre!("invalid keystore AES key: {e}"))?,
        &iv[..16],
    )
    .map_err(|e| eyre::eyre!("invalid keystore AES IV: {e}"))?;
    cipher.apply_keystream_partial(plaintext.as_mut_slice().into());

    let bytes: [u8; 32] = plaintext
        .as_slice()
        .try_into()
        .map_err(|_| eyre::eyre!("invalid decrypted key length — expected 32 bytes"))?;
    SigningKey::from_slice(&bytes).context("invalid key in keystore")
}

fn derive_key(kdf: &KdfparamsType, password: &[u8]) -> Result<Vec<u8>> {
    match kdf {
        KdfparamsType::Pbkdf2 { c, dklen, prf, salt } => {
            if prf != "hmac-sha256" {
                eyre::bail!("unsupported keystore KDF PRF {prf}");
            }
            let mut key = vec![0u8; usize::from(*dklen)];
            pbkdf2::<Hmac<Sha256>>(password, salt, *c, &mut key);
            Ok(key)
        }
        KdfparamsType::Scrypt { dklen, n, p, r, salt } => {
            if !n.is_power_of_two() {
                eyre::bail!("invalid scrypt N {n} — must be a power of two");
            }
            let params = ScryptParams::new(n.trailing_zeros() as u8, *r, *p)
                .map_err(|e| eyre::eyre!("invalid scrypt params: {e}"))?;
            let mut key = vec![0u8; usize::from(*dklen)];
            scrypt(password, salt, &params, &mut key)
                .map_err(|e| eyre::eyre!("scrypt failed: {e}"))?;
            Ok(key)
        }
    }
}

/// Keystore filename: `UTC--{timestamp}--{address_lower_hex}`.
pub(crate) fn utc_keystore_filename(address: Address) -> String {
    let now = chrono::Utc::now();
    let ts = now.format("%Y-%m-%dT%H-%M-%S");
    let subsec = format!("{:09}", now.timestamp_subsec_nanos());
    format!("UTC--{ts}.{subsec}Z--{}", address.to_string().trim_start_matches("0x").to_lowercase())
}

/// Test-only: build material from a bare signing key by encrypting it through the real writer,
/// loading it back through the real reader, then deleting the scratch files. No test stores a
/// long-lived `SigningKey` in production types.
#[cfg(test)]
pub(crate) fn test_material(secret: &SigningKey) -> KeystoreMaterial {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let dir = std::env::temp_dir().join(format!(
        "smartnet-material-{}-{}",
        std::process::id(),
        COUNTER.fetch_add(1, Ordering::Relaxed)
    ));
    let _ = std::fs::remove_dir_all(&dir);
    init_keystore_at(&dir, "key.json", secret, "test-password").expect("encrypt test key");
    let encrypted = std::fs::read(dir.join("key.json")).expect("read test keystore bytes");
    let _ = std::fs::remove_dir_all(&dir);
    // Do not put a password file in the keystore dir: the gather rule correctly requires exactly
    // one file there. The fixture hands the already-encrypted bytes to the same material loader
    // production uses after reading the two files.
    KeystoreMaterial::from_encrypted_bytes(encrypted, b"test-password".to_vec())
        .expect("build test material")
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_primitives::B256;

    /// Fixed key so the derived address is stable across runs (same as `import.rs`).
    const KEY_HEX: &str = "0x59c6995e998f97a5a0044966f0945389dc9e86dae88c7a8412f4603b6b78690d";
    /// The address of the key above (second Hardhat account), lowercase.
    const KEY_ADDRESS: &str = "0x70997970c51812dc3a010c7d01b50e0d17dc79c8";
    /// A second fixed key for multi-keystore selection tests (first Hardhat account).
    const OTHER_KEY_HEX: &str =
        "0xac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80";

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
        let raw = std::fs::read(dir.join("key.json")).expect("read keystore");
        assert!(raw.starts_with(b"{"), "must be JSON keystore");

        // Decrypting the in-memory bytes yields the same signing address.
        let loaded = load_secret_from_keystore(&raw, b"pw").expect("decrypt");
        assert_eq!(format!("{:#x}", signer_address(&loaded)), KEY_ADDRESS);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The keystore name must be a bare filename — empty or any path separator is refused.
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

    /// Decryption rejects plaintext bytes and a wrong password. Missing files are rejected while
    /// reading the material, before decryption.
    #[test]
    fn load_secret_rejects_plaintext_and_wrong_password() {
        let dir = temp_dir("load");
        let key = test_key();

        // A bare hex key file (not an encrypted keystore) is refused.
        let err = load_secret_from_keystore(KEY_HEX.as_bytes(), b"pw").unwrap_err();
        assert!(err.to_string().contains("plaintext"), "got: {err:?}");

        // Encrypted keystore but the wrong password.
        init_keystore_at(&dir, "key.json", &key, "right").expect("init");
        let raw = std::fs::read(dir.join("key.json")).expect("read keystore");
        let err = load_secret_from_keystore(&raw, b"wrong").unwrap_err();
        assert!(err.to_string().contains("failed to decrypt"), "got: {err:?}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The generated keystore filename has the `UTC--…--<lowercase address>` shape.
    #[test]
    fn utc_filename_has_expected_shape() {
        let addr: Address = KEY_ADDRESS.parse().expect("valid address");
        let name = utc_keystore_filename(addr);
        assert!(name.starts_with("UTC--"), "got: {name}");
        assert!(
            name.contains("70997970c51812dc3a010c7d01b50e0d17dc79c8"),
            "lowercase address expected in {name}"
        );
    }

    /// Headless loader: password file only (no prompt), fatal on a missing/empty
    /// password, on a `KEY_PATH` that does not hold **exactly one** file, or on a
    /// missing keystore. The key is GATHERED — the loader never takes a filename.
    #[test]
    fn material_load_is_headless_and_gathers_the_key() {
        let dir = temp_dir("identity");
        // The keystore lives in its own directory: the gather rule is "exactly one file
        // in KEY_PATH", so the password files stay OUTSIDE it.
        let key_dir = dir.join("keys");
        std::fs::create_dir(&key_dir).expect("key dir");
        // ANY filename is accepted — nothing points at a literal one.
        init_keystore_at(&key_dir, "operator-chose-this-name.json", &test_key(), "pw")
            .expect("init keystore");

        // Happy path: password file present and non-empty → same address, no prompt.
        let pw = dir.join("pw.txt");
        std::fs::write(&pw, "pw\n").expect("write pw");
        let loaded = KeystoreMaterial::load(&key_dir, &pw, None).expect("load");
        assert_eq!(format!("{:#x}", loaded.address()), KEY_ADDRESS);
        // The material holds no signing key: it must derive the same address from bytes on demand.
        let derived = loaded.decrypt().expect("transient decrypt");
        assert_eq!(format!("{:#x}", signer_address(&derived)), KEY_ADDRESS);
        drop(derived);

        // Missing password file → fatal, mentions "no interactive prompt".
        let err =
            KeystoreMaterial::load(&key_dir, &dir.join("nope.txt"), None).unwrap_err().to_string();
        assert!(err.contains("no interactive prompt"), "got: {err}");

        // Empty password file → fatal (never boot with a blank password).
        let empty = dir.join("empty.txt");
        std::fs::write(&empty, "").expect("write empty");
        let err = KeystoreMaterial::load(&key_dir, &empty, None).unwrap_err().to_string();
        assert!(err.contains("blank"), "got: {err}");

        // Zero files → error exit, named (the keystore was never created).
        let none = dir.join("none");
        std::fs::create_dir(&none).expect("empty dir");
        let err = KeystoreMaterial::load(&none, &pw, None).unwrap_err().to_string();
        assert!(err.contains("no keystore"), "got: {err}");

        // More than one file → error exit: ambiguous, never guessed by order or luck.
        std::fs::write(key_dir.join("stray.txt"), "x").expect("stray");
        let err = KeystoreMaterial::load(&key_dir, &pw, None).unwrap_err().to_string();
        assert!(err.contains("expected exactly 1"), "got: {err}");

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// `COINBASE` selects one keystore by the address in its UTC filename; the other files in
    /// `KEY_PATH` are ignored. Without it the old "exactly one file" rule still applies.
    #[test]
    fn coinbase_selects_one_keystore_among_many() {
        let dir = temp_dir("coinbase-select");
        let key_dir = dir.join("keys");
        std::fs::create_dir(&key_dir).expect("key dir");
        let pw = dir.join("pw.txt");
        std::fs::write(&pw, "pw\n").expect("write pw");

        let first = test_key();
        let first_addr = signer_address(&first);
        let other_b256: B256 = OTHER_KEY_HEX.parse().expect("valid key hex");
        let other = SigningKey::from_slice(other_b256.as_slice()).expect("valid scalar");
        let other_addr = signer_address(&other);

        init_keystore_at(&key_dir, &utc_keystore_filename(first_addr), &first, "pw")
            .expect("first keystore");
        init_keystore_at(&key_dir, &utc_keystore_filename(other_addr), &other, "pw")
            .expect("second keystore");

        let picked = get_encrypted_key(&key_dir, Some(other_addr)).expect("COINBASE picks one");
        let picked_name = picked.file_name().and_then(|n| n.to_str()).expect("utf8 filename");
        let expected_suffix = format!("{other_addr:#x}")[2..].to_string();
        assert!(picked_name.ends_with(&expected_suffix), "got: {picked_name}");
        let loaded = KeystoreMaterial::load(&key_dir, &pw, Some(other_addr)).expect("load");
        assert_eq!(loaded.address(), other_addr);

        let err = get_encrypted_key(&key_dir, None).unwrap_err().to_string();
        assert!(err.contains("expected exactly 1"), "got: {err}");
        let err =
            get_encrypted_key(&key_dir, Some(Address::repeat_byte(0x11))).unwrap_err().to_string();
        assert!(err.contains("COINBASE"), "got: {err}");

        init_keystore_at(&key_dir, &format!("UTC--duplicate--{expected_suffix}"), &other, "pw")
            .expect("duplicate keystore");
        let err = get_encrypted_key(&key_dir, Some(other_addr)).unwrap_err().to_string();
        assert!(err.contains("more than one keystore"), "got: {err}");

        let _ = std::fs::remove_dir_all(&dir);
    }
}

# Private key handling

SmartNet is a private EVM network. A node's signing key is an encrypted Ethereum JSON v3 keystore;
raw private keys never become part of the node's persistent state.

## Commands are separate

- `smartnet keystore [RAW_PRIVATE_KEY_FILE] --config CONFIG` (or
  `smartnet --config CONFIG keystore [RAW_PRIVATE_KEY_FILE]`) creates or imports the encrypted key.
  The raw key file is optional; without it, a fresh key is generated. The raw source is deleted only
  after the encrypted keystore has been written successfully. `CONFIG` is never edited.
- `smartnet init ...` initializes genesis and database state only. It never handles a private key
  and never accepts one in genesis.

## Keystore storage and selection

- Keystores are written under `KEY_PATH` as `UTC--<timestamp>--<address>` files. The writer chooses
  the filename; no config key names a key file.
- `config.toml` is immutable input. No command writes the encrypted filename, an address, or any
  other value back into it. The older rule that said to write the encrypted filename into config is
  superseded and removed.
- `COINBASE` is an optional node-level keystore selector for every role, whether `SEAL` is true or
  false. It is not an identity, fee receiver, sealer, mesh ID, or consensus input.
- Exactly two states are valid; everything else is a fatal error:
  1. `COINBASE` is set and exactly one file in `KEY_PATH` has that address in its UTC filename.
     Other files are ignored.
  2. `COINBASE` is absent or blank and `KEY_PATH` contains exactly one file.
- With `COINBASE` set, zero matches, multiple matches, or one file with a different address is a
  contradiction and aborts boot with an explicit error. Without `COINBASE`, zero files or more than
  one file aborts boot. There is no ordering, guessing, or silent fallback.
- The address is derived from the decrypted key for internal use only. It is public and safe to
  keep in memory, but it is never written back to `config.toml`.

## Decryption rules

- There is exactly one encrypted-keystore decryption function:
  `smartnet_validator::keystore_util::load_secret_from_keystore`. All production callers and all
  tests use that function. No module or test calls `PrivateKeySigner::decrypt_keystore` directly or
  implements another decryption helper.
- Password material comes only from `PASSWORD_FILE`. A headless boot never prompts. The password
  file is the only file allowed to contain password bytes; it is created owner-only (`0600`) and
  never clobbered. Its content is never copied to another file, database row, log, metric, RPC
  response, cache, or static.
- At runtime the node keeps only two buffers: the encrypted keystore bytes and the password bytes,
  held together in `KeystoreMaterial`. The password buffer is zeroized on drop. There is no
  long-lived `SigningKey` field in `KeystoreMaterial`, the validator, the mesh identity, or any
  other runtime struct.
- Decryption is transient. `KeystoreMaterial::with_key` decrypts only for the signature being made
  and drops the key as soon as the call returns; `seedTx` uses `decrypt()` locally and drops the key
  and wallet immediately after signing. The derived address is kept because it is public.
- Decrypting this way is cheap — normally milliseconds for one keystore — so there is no reason to
  keep a naked 32-byte private key resident to avoid the work. A plain RAM scan cannot lift that
  key: an attacker has to find both separate buffers and replay the keystore decryption, or catch
  the brief signing window. This is not a promise against full process compromise; it removes the
  easy target.
- A plaintext or hex key file passed as a keystore is rejected explicitly.

## Genesis

Genesis never carries a private key. `smartnet init` rejects a genesis whose parsed accounts or
verbatim raw JSON contain `private_key`, with an explicit error before any database write. It never
strips or filters the field silently.

## Audit result

- No raw key or password content is written to files, the database, logs, tracing, metrics, RPC,
  caches, statics, or thread-local storage. The only allowed writes are the encrypted keystore and
  the password file itself.
- `RunOnceArgs` and mesh `Identity` have no exposed key material in `Debug`; they hold the encrypted
  buffers, not a decrypted signer.
- Addresses may appear in logs, config, or the database. Key bytes and password bytes may not.

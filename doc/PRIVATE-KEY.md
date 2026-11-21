# This is a small document describes how private key must be handled

 - the keypair generated at the time of node initiation with:
  smartnode keystore {PATH_TO_RAW_OPEN_KEY_FILE} {PATH_TO_PASSWORD_FILE}
  that keyfile contain raw unencrypted key with or without 0x prefix
  Processing key:
  Raw key is used only once, at the time when smartnode keystroke command is given
  this command encrypts raw key and stores encrypted file under config path
  filename format is time-address
  on successful encrypted file storage raw unencrypted file must be deleted
  encrypted key filename must be written back to config
  
  Runtime handling:
  
  On every bootstrap node must load encrypted key to memory, along with password file from config
  and keep both in memory, use decryption every time private key is needed, must never keep raw 
  unencrypted file in memory or naywhere else!
 
  Encrypted key and password loads in RAM at bottstrap only, and only once, never re-occur. Example: var encrypted_key_in_ram_var = getFileContent( getEncryptedKeyFilePath() ) var password_in_ram_var  = getFileContent( config_path_to_password_flie ) 
  
  There should be just one main function that does key file decryption. Example tmpPkey =  decryptPkey({encrypted_key_in_ram_var}, {password_in_ram_var}) After op sign is done, tmpPkey = null/0/''

  Tests must use very same function and not write any different functions or re-implement this function.
  No test can implement decryption of encrypted key file using different functions.
  

  Decryted/raw private key must never be stored anywhere! not in files not in DB!
  Password file and/or it's content can not be stored anywhere else or copied to other files, places, or BD!
  
## Suspicion (investigated — NOT substantiated, 2026-10-22)

 - Claim: decoded raw key and/or password file content may NEVER be stored in DB to "speed things up".
 - Full trace of the working tree found no code path that persists, logs, caches, or transmits the decrypted key, the password-file content, or the raw unencrypted key file. Key material is decrypted once at bootstrap and lives only in memory for the process lifetime.
 - What was checked (all clean):
   - Decrypted-key flow: `validator/src/lib.rs` `load()` → `RunOnceArgs.secret_key` (memory
     only, never serialized; `k256`/`alloy` `Debug` is redacted).
   - Password content: sole reader `keystore_util.rs` (local `String`, dropped after decrypt);
     only ever written to the password file itself (`0600`, `create_new`).
   - Raw key file: read once at `import.rs`, encrypted, source deleted; `load_secret_from_keystore`
     explicitly refuses plaintext key files.
   - DB writers: validator does zero DB writes (`import.rs`: "The validator has no database");
     `Settings`/`Metadata` rows carry addresses + genesis spec only — no key/password row.
   - Caches: no `static`/`OnceLock`/`DashMap`/thread-local holds a secret; `DevSigner` (which
     could) is constructed empty in production.
   - Logging/metrics/RPC/tracing: zero macros format key/password bytes; no RPC method returns
     them; `admin_nodeInfo` exposes addresses/roster only.
   - Genesis `private_key`: the `GenesisAccount` codec has an optional `private_key` field, but
     nothing populates it and no genesis/JSON/JSONL in the repo carries one — so the verbatim
     `GENESIS_SPEC` DB row can never contain a secret as the tree stands.
 - Note-level hardening applied: removed `#[derive(Debug)]` from `RunOnceArgs` (it holds
   `secret_key`; today's `Debug` impls are redacted, but the derive was a latent foot-gun).
 - Remaining (defensive, low): consider rejecting a genesis that carries `private_key` at init,
   so the `GENESIS_SPEC` row can never hold a secret even if one is handed in later.

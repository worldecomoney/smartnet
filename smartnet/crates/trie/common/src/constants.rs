/// The maximum size of RLP encoded trie account in bytes.
/// 2 (header) + 4 * 1 (field lens) + 8 (nonce) + 32 * 3 (balance, storage root, code hash)
pub const TRIE_ACCOUNT_RLP_MAX_SIZE: usize = 110;


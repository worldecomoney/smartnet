//! Tracking of keys having been added and removed from the tries.

use crate::HashedPostState;
use alloy_primitives::{map::B256Map, B256};
use alloy_trie::proof::AddedRemovedKeys;

/// Tracks added and removed keys across account and storage tries.
#[derive(Debug, Clone)]
pub struct MultiAddedRemovedKeys {
    /// Added and removed accounts.
    pub account: AddedRemovedKeys,
    /// Added and removed storage keys for each account.
    pub storages: B256Map<AddedRemovedKeys>,
}

/// Returns [`AddedRemovedKeys`] with default parameters. This is necessary while we are not yet
/// tracking added keys.
pub fn default_added_removed_keys() -> AddedRemovedKeys {
    AddedRemovedKeys::default().with_assume_added(true)
}

impl Default for MultiAddedRemovedKeys {
    fn default() -> Self {
        Self::new()
    }
}

impl MultiAddedRemovedKeys {
    /// Returns a new instance.
    pub fn new() -> Self {
        Self { account: default_added_removed_keys(), storages: Default::default() }
    }

    /// Updates the set of removed keys based on a [`HashedPostState`].
    ///
    /// Storage keys set to [`alloy_primitives::U256::ZERO`] are added to the set for their
    /// respective account. Keys set to any other value are removed from their respective
    /// account.
    pub fn update_with_state(&mut self, update: &HashedPostState) {
        for (hashed_address, storage) in &update.storages {
            let account = update
                .accounts
                .get(hashed_address)
                .map(|entry| entry.unwrap_or_default())
                .unwrap_or_default();

            if storage.wiped {
                self.storages.remove(hashed_address);
                if account.is_empty() {
                    self.account.insert_removed(*hashed_address);
                }
                continue;
            }

            let storage_removed_keys =
                self.storages.entry(*hashed_address).or_insert_with(default_added_removed_keys);

            for (key, val) in &storage.storage {
                if val.is_zero() {
                    storage_removed_keys.insert_removed(*key);
                } else {
                    storage_removed_keys.remove_removed(key);
                }
            }

            if !account.is_empty() {
                self.account.remove_removed(hashed_address);
            }
        }
    }

    /// Returns a [`AddedRemovedKeys`] for the storage trie of a particular account, if any.
    pub fn get_storage(&self, hashed_address: &B256) -> Option<&AddedRemovedKeys> {
        self.storages.get(hashed_address)
    }

    /// Returns an [`AddedRemovedKeys`] for tracking account-level changes.
    pub const fn get_accounts(&self) -> &AddedRemovedKeys {
        &self.account
    }

    /// Marks an account as existing, and therefore having storage.
    pub fn touch_accounts(&mut self, addresses: impl Iterator<Item = B256>) {
        for address in addresses {
            self.storages.entry(address).or_insert_with(default_added_removed_keys);
        }
    }
}


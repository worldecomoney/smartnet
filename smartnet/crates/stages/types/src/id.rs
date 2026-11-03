use alloc::vec::Vec;
#[cfg(feature = "std")]
use std::{collections::HashMap, sync::OnceLock};

/// Stage IDs for all known stages.
///
/// For custom stages, use [`StageId::Other`]
#[expect(missing_docs)]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum StageId {
    #[deprecated(
        note = "MerkleChangeSets stage has been removed; kept for DB checkpoint compatibility"
    )]
    MerkleChangeSets,
    Era,
    Headers,
    Bodies,
    SenderRecovery,
    Execution,
    PruneSenderRecovery,
    MerkleUnwind,
    AccountHashing,
    StorageHashing,
    MerkleExecute,
    TransactionLookup,
    IndexStorageHistory,
    IndexAccountHistory,
    Prune,
    Finish,
    /// Other custom stage with a provided string identifier.
    Other(&'static str),
}

/// One-time-allocated stage ids encoded as raw Vecs, useful for database
/// clients to reference them for queries instead of encoding anew per query
/// (sad heap allocation required).
#[cfg(feature = "std")]
static ENCODED_STAGE_IDS: OnceLock<HashMap<StageId, Vec<u8>>> = OnceLock::new();

impl StageId {
    /// All supported Stages
    pub const ALL: [Self; 15] = [
        Self::Era,
        Self::Headers,
        Self::Bodies,
        Self::SenderRecovery,
        Self::Execution,
        Self::PruneSenderRecovery,
        Self::MerkleUnwind,
        Self::AccountHashing,
        Self::StorageHashing,
        Self::MerkleExecute,
        Self::TransactionLookup,
        Self::IndexStorageHistory,
        Self::IndexAccountHistory,
        Self::Prune,
        Self::Finish,
    ];

    /// Stages that require state.
    pub const STATE_REQUIRED: [Self; 9] = [
        Self::Execution,
        Self::PruneSenderRecovery,
        Self::MerkleUnwind,
        Self::AccountHashing,
        Self::StorageHashing,
        Self::MerkleExecute,
        Self::IndexStorageHistory,
        Self::IndexAccountHistory,
        Self::Prune,
    ];

    /// Return stage id formatted as string.
    pub const fn as_str(&self) -> &str {
        match self {
            #[expect(deprecated)]
            Self::MerkleChangeSets => "MerkleChangeSets",
            Self::Era => "Era",
            Self::Headers => "Headers",
            Self::Bodies => "Bodies",
            Self::SenderRecovery => "SenderRecovery",
            Self::Execution => "Execution",
            Self::PruneSenderRecovery => "PruneSenderRecovery",
            Self::MerkleUnwind => "MerkleUnwind",
            Self::AccountHashing => "AccountHashing",
            Self::StorageHashing => "StorageHashing",
            Self::MerkleExecute => "MerkleExecute",
            Self::TransactionLookup => "TransactionLookup",
            Self::IndexAccountHistory => "IndexAccountHistory",
            Self::IndexStorageHistory => "IndexStorageHistory",
            Self::Prune => "Prune",
            Self::Finish => "Finish",
            Self::Other(s) => s,
        }
    }

    /// Returns true if it's a downloading stage [`StageId::Headers`] or [`StageId::Bodies`]
    pub const fn is_downloading_stage(&self) -> bool {
        matches!(self, Self::Era | Self::Headers | Self::Bodies)
    }

    /// Returns `true` if it's [`TransactionLookup`](StageId::TransactionLookup) stage.
    pub const fn is_tx_lookup(&self) -> bool {
        matches!(self, Self::TransactionLookup)
    }

    /// Returns true indicating if it's the finish stage [`StageId::Finish`]
    pub const fn is_finish(&self) -> bool {
        matches!(self, Self::Finish)
    }

    /// Get a pre-encoded raw Vec, for example, to be used as the DB key for
    /// `tables::StageCheckpoints` and `tables::StageCheckpointProgresses`
    pub fn get_pre_encoded(&self) -> Option<&Vec<u8>> {
        #[cfg(not(feature = "std"))]
        {
            None
        }
        #[cfg(feature = "std")]
        ENCODED_STAGE_IDS
            .get_or_init(|| {
                let mut map = HashMap::with_capacity(Self::ALL.len());
                for stage_id in Self::ALL {
                    map.insert(stage_id, stage_id.to_string().into_bytes());
                }
                map
            })
            .get(self)
    }
}

impl core::fmt::Display for StageId {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "{}", self.as_str())
    }
}

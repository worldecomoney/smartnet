//! Schema version and migration log models.

use alloy_primitives::Bytes;

/// The `SchemaInfo` row: the singleton schema-version record that replaces the former
/// `database.version` sidecar file (SQLITE.md §11 A `schema_info`, §13.5 3B.4).
///
/// Exactly one row exists, keyed by the `SchemaInfoKey::Singleton` key next to the table
/// declaration in `smartnet-db-api`. The version lives in the same store it describes,
/// so a datadir can never carry a version that disagrees with its keyspaces.
#[derive(Clone, Eq, PartialEq, Debug, Default)]
#[cfg_attr(any(test, feature = "arbitrary"), derive(arbitrary::Arbitrary))]
#[cfg_attr(any(test, feature = "smartnet-codec"), smartnet_codecs::add_arbitrary_tests(compact))]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct StoredSchemaInfo {
    /// The schema version recorded on fresh init; compared against `DB_VERSION` on open.
    /// Fresh-init-only: a mismatch is refused, never migrated.
    pub schema_version: u64,
}

#[cfg(any(test, feature = "smartnet-codec"))]
impl smartnet_codecs::Compact for StoredSchemaInfo {
    fn to_compact<B>(&self, buf: &mut B) -> usize
    where
        B: bytes::BufMut + AsMut<[u8]>,
    {
        self.schema_version.to_compact(buf)
    }

    fn from_compact(buf: &[u8], _len: usize) -> (Self, &[u8]) {
        // The version is the only field, so it consumes the whole buffer tail.
        let (schema_version, buf) = u64::from_compact(buf, buf.len());
        (Self { schema_version }, buf)
    }
}

#[cfg(any(test, feature = "smartnet-codec"))]
smartnet_codecs::impl_compression_for_compact!(StoredSchemaInfo);

/// The `Migrations` row: one applied schema migration (SQLITE.md §11 A `migrations`,
/// §13.5 3B.4).
///
/// Empty under the fresh-init-only policy — present so a future migration framework has
/// its log. The description is encoded last so it may consume the buffer tail.
#[derive(Clone, Eq, PartialEq, Debug)]
#[cfg_attr(any(test, feature = "arbitrary"), derive(arbitrary::Arbitrary))]
#[cfg_attr(any(test, feature = "smartnet-codec"), derive(smartnet_codecs::Compact))]
#[cfg_attr(any(test, feature = "smartnet-codec"), smartnet_codecs::add_arbitrary_tests(compact))]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct StoredMigration {
    /// Schema version this migration brought the database to (the `Migrations` key).
    pub schema_version: u64,
    /// The migration text. Encoded last so it may consume the buffer tail.
    pub description: Bytes,
}

#[cfg(any(test, feature = "smartnet-codec"))]
smartnet_codecs::impl_compression_for_compact!(StoredMigration);

use crate::{
    either_writer::{AuxDbBatchArg, AuxDbRefArg, RawAuxDbBatch},
    providers::AuxDbProvider,
};
use smartnet_storage_api::StorageSettingsCache;
use smartnet_storage_errors::provider::ProviderResult;
use std::path::PathBuf;

/// `aux DB` provider factory.
///
/// This trait provides access to the `aux DB` provider
pub trait AuxDbProviderFactory {
    /// Returns the `aux DB` provider.
    fn aux_db_provider(&self) -> AuxDbProvider;

    /// Adds a pending `aux DB` batch to be committed when this provider is committed.
    ///
    /// This allows deferring `aux DB` commits to happen at the same time as the primary database
    /// and static file commits, ensuring atomicity across all storage backends.
    fn set_pending_aux_db_batch(&self, batch: RawAuxDbBatch);

    /// Takes all pending `aux DB` batches and commits them.
    ///
    /// This drains the pending batches from the lock and commits each one using the `aux DB`
    /// provider. Can be called before flush to persist `aux DB` writes independently of the
    /// full commit path.
    fn commit_pending_aux_db_batches(&self) -> ProviderResult<()>;

    /// Executes a closure with a `aux DB` point-in-time snapshot for consistent reads.
    ///
    /// This helper encapsulates `aux DB` access for read operations.
    /// Without an `aux DB`, this skips creating the snapshot entirely, avoiding unnecessary
    /// overhead, and passes `None` to the closure.
    ///
    /// Unlike a transaction-based approach, this works in both read-only and read-write
    /// modes since the snapshot provides a consistent view of the data at the time it
    /// was created.
    fn with_aux_db_snapshot<F, R>(&self, f: F) -> ProviderResult<R>
    where
        Self: StorageSettingsCache,
        F: FnOnce(AuxDbRefArg<'_>) -> ProviderResult<R>,
    {
        f(None)
    }

    /// Executes a closure with a `aux DB` batch, automatically registering it for commit.
    ///
    /// This helper encapsulates all the cfg-gated `aux DB` batch handling.
    fn with_aux_db_batch<F, R>(&self, f: F) -> ProviderResult<R>
    where
        Self: StorageSettingsCache,
        F: FnOnce(AuxDbBatchArg<'_>) -> ProviderResult<(R, Option<RawAuxDbBatch>)>,
    {
        // Without an `aux DB`, execute against a local no-op batch and skip pending aux DB
        // commit registration entirely.
        let aux_db = AuxDbProvider::builder(PathBuf::new()).build().unwrap();
        let batch = aux_db.batch();
        let (result, _) = f(batch)?;
        Ok(result)
    }

    /// Executes a closure with a `aux DB` batch that auto-commits on threshold.
    ///
    /// Unlike [`Self::with_aux_db_batch`], this uses a batch that automatically commits
    /// when it exceeds the size threshold, preventing OOM during large bulk writes.
    /// The consistency check on startup heals any crash between auto-commits.
    fn with_aux_db_batch_auto_commit<F, R>(&self, f: F) -> ProviderResult<R>
    where
        Self: StorageSettingsCache,
        F: FnOnce(AuxDbBatchArg<'_>) -> ProviderResult<(R, Option<RawAuxDbBatch>)>,
    {
        let aux_db = AuxDbProvider::builder(PathBuf::new()).build().unwrap();
        let batch = aux_db.batch_with_auto_commit();
        let (result, _) = f(batch)?;
        Ok(result)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use smartnet_db_api::models::StorageSettings;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// Mock `aux DB` provider that tracks snapshot creation calls.
    struct MockAuxDbProvider {
        tx_call_count: AtomicUsize,
    }

    impl MockAuxDbProvider {
        const fn new() -> Self {
            Self { tx_call_count: AtomicUsize::new(0) }
        }

        fn tx_call_count(&self) -> usize {
            self.tx_call_count.load(Ordering::SeqCst)
        }

        fn increment_tx_count(&self) {
            self.tx_call_count.fetch_add(1, Ordering::SeqCst);
        }
    }

    /// Test provider that implements [`AuxDbProviderFactory`] + [`StorageSettingsCache`].
    struct TestProvider {
        settings: StorageSettings,
        mock_aux_db: MockAuxDbProvider,
        temp_dir: tempfile::TempDir,
    }

    impl TestProvider {
        fn new(settings: StorageSettings) -> Self {
            Self {
                settings,
                mock_aux_db: MockAuxDbProvider::new(),
                temp_dir: tempfile::TempDir::new().unwrap(),
            }
        }

        fn tx_call_count(&self) -> usize {
            self.mock_aux_db.tx_call_count()
        }
    }

    impl StorageSettingsCache for TestProvider {
        fn cached_storage_settings(&self) -> StorageSettings {
            self.settings
        }

        fn set_storage_settings_cache(&self, _settings: StorageSettings) {}
    }

    impl AuxDbProviderFactory for TestProvider {
        fn aux_db_provider(&self) -> AuxDbProvider {
            self.mock_aux_db.increment_tx_count();
            AuxDbProvider::new(self.temp_dir.path()).unwrap()
        }

        fn set_pending_aux_db_batch(&self, _batch: RawAuxDbBatch) {}

        fn commit_pending_aux_db_batches(&self) -> ProviderResult<()> {
            Ok(())
        }
    }

    #[test]
    fn test_legacy_settings_skip_aux_db_snapshot() {
        let provider = TestProvider::new(StorageSettings::v1());

        let result = provider.with_aux_db_snapshot(|aux_db| {
            assert!(aux_db.is_none(), "legacy settings should pass None");
            Ok(42)
        });

        assert_eq!(result.unwrap(), 42);
        assert_eq!(
            provider.tx_call_count(),
            0,
            "should not create aux DB provider for legacy settings"
        );
    }
}

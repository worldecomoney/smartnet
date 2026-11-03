//! Storage lock utils.

use smartnet_storage_errors::lockfile::StorageLockError;
use std::{
    path::{Path, PathBuf},
    process,
    sync::{Arc, OnceLock},
};
use sysinfo::{ProcessRefreshKind, RefreshKind, System};

/// File lock name.
const LOCKFILE_NAME: &str = "lock";

/// A file lock for a storage directory to ensure exclusive read-write access across different
/// processes.
///
/// This lock stores the PID of the process holding it and is released (deleted) on a graceful
/// shutdown. On resuming from a crash, the stored PID helps verify that no other process holds the
/// lock.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StorageLock(Arc<StorageLockInner>);

impl StorageLock {
    /// Tries to acquire a write lock on the target directory, returning [`StorageLockError`] if
    /// unsuccessful.
    ///
    /// Note: In-process exclusivity is not on scope. If called from the same process (or another
    /// with the same PID), it will succeed.
    pub fn try_acquire(path: &Path) -> Result<Self, StorageLockError> {
        Self::try_acquire_file_lock(path)
    }

    /// Acquire a file write lock.
    ///
    /// Correctness comes from the **atomic create** (`O_EXCL`), not from the liveness
    /// check: two starters racing the check both proceed to the create and exactly one
    /// wins; the loser re-evaluates the winner's liveness and either reports `Taken` or
    /// (stale/corrupt file, our own leftover) clears it and retries once (XCURR H-S6).
    fn try_acquire_file_lock(path: &Path) -> Result<Self, StorageLockError> {
        let file_path = path.join(LOCKFILE_NAME);
        if let Some(parent) = file_path.parent() {
            smartnet_fs_util::create_dir_all(parent).map_err(StorageLockError::other)?;
        }
        let own_pid = process::id() as usize;

        // Fast path: a live foreign holder — reject before attempting the create.
        if let Some(process_lock) = ProcessUID::parse(&file_path)? &&
            process_lock.pid != own_pid &&
            process_lock.is_active()
        {
            smartnet_tracing::tracing::error!(
                target: "smartnet::db::lockfile",
                path = ?file_path,
                pid = process_lock.pid,
                start_time = process_lock.start_time,
                "Storage lock already taken."
            );
            return Err(StorageLockError::Taken(process_lock.pid));
        }

        let mut retry_after_stale_clear = true;
        loop {
            match std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&file_path)
            {
                Ok(_) => {
                    // The file is ours exclusively — write the holder identity.
                    ProcessUID::own().write(&file_path)?;
                    return Ok(Self(Arc::new(StorageLockInner { file_path })));
                }
                Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => {
                    // Lost the create race (or a stale/same-pid file exists): whoever
                    // created it wins if it is alive and not us.
                    if let Some(process_lock) = ProcessUID::parse(&file_path)? &&
                        process_lock.pid != own_pid &&
                        process_lock.is_active()
                    {
                        return Err(StorageLockError::Taken(process_lock.pid));
                    }
                    if retry_after_stale_clear {
                        // Dead pid, corrupt content, or our own leftover — clear and
                        // retry the atomic create exactly once.
                        retry_after_stale_clear = false;
                        let _ = smartnet_fs_util::remove_file(&file_path);
                        continue;
                    }
                    // Another creator won between our removal and this attempt.
                    let taken_pid = ProcessUID::parse(&file_path)?
                        .map(|lock| lock.pid)
                        .unwrap_or(own_pid);
                    return Err(StorageLockError::Taken(taken_pid));
                }
                Err(err) => return Err(StorageLockError::other(err)),
            }
        }
    }
}

impl Drop for StorageLockInner {
    fn drop(&mut self) {
        {
            let file_path = &self.file_path;
            if file_path.exists() {
                if let Ok(Some(process_uid)) = ProcessUID::parse(file_path) {
                    // Only remove if the lock file belongs to our process
                    if process_uid.pid == process::id() as usize {
                        if let Err(err) = smartnet_fs_util::remove_file(file_path) {
                            smartnet_tracing::tracing::error!(%err, "Failed to delete lock file");
                        }
                    } else {
                        smartnet_tracing::tracing::warn!(
                            "Lock file belongs to different process (PID: {}), not removing",
                            process_uid.pid
                        );
                    }
                } else {
                    // If we can't parse the lock file, still try to remove it
                    // as it might be corrupted or from a previous run
                    if let Err(err) = smartnet_fs_util::remove_file(file_path) {
                        smartnet_tracing::tracing::error!(%err, "Failed to delete lock file");
                    }
                }
            }
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
struct StorageLockInner {
    file_path: PathBuf,
}

#[derive(Clone, Debug)]
struct ProcessUID {
    /// OS process identifier
    pid: usize,
    /// Process start time
    start_time: u64,
}

impl ProcessUID {
    /// Creates [`Self`] for the provided PID.
    fn new(pid: usize) -> Option<Self> {
        let mut system = System::new();
        let pid2 = sysinfo::Pid::from(pid);
        system.refresh_processes_specifics(
            sysinfo::ProcessesToUpdate::Some(&[pid2]),
            true,
            ProcessRefreshKind::nothing(),
        );
        system.process(pid2).map(|process| Self { pid, start_time: process.start_time() })
    }

    /// Creates [`Self`] from own process.
    fn own() -> Self {
        static CACHE: OnceLock<ProcessUID> = OnceLock::new();
        CACHE.get_or_init(|| Self::new(process::id() as usize).expect("own process")).clone()
    }

    /// Parses [`Self`] from a file.
    fn parse(path: &Path) -> Result<Option<Self>, StorageLockError> {
        if path.exists() &&
            let Ok(contents) = smartnet_fs_util::read_to_string(path)
        {
            let mut lines = contents.lines();
            if let (Some(Ok(pid)), Some(Ok(start_time))) = (
                lines.next().map(str::trim).map(str::parse),
                lines.next().map(str::trim).map(str::parse),
            ) {
                return Ok(Some(Self { pid, start_time }));
            }
        }
        Ok(None)
    }

    /// Whether a process with this `pid` and `start_time` exists.
    fn is_active(&self) -> bool {
        System::new_with_specifics(
            RefreshKind::nothing().with_processes(ProcessRefreshKind::nothing()),
        )
        .process(self.pid.into())
        .is_some_and(|p| p.start_time() == self.start_time)
    }

    /// Writes `pid` and `start_time` to a file.
    fn write(&self, path: &Path) -> Result<(), StorageLockError> {
        smartnet_fs_util::write(path, format!("{}\n{}", self.pid, self.start_time))
            .map_err(StorageLockError::other)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Fresh acquire succeeds, writes the holder file, and the drop removes it.
    #[test]
    fn fresh_acquire_writes_and_release_removes_lock() {
        let dir = tempfile::tempdir().expect("tempdir");
        let lock = StorageLock::try_acquire(dir.path()).expect("fresh acquire");
        assert!(dir.path().join(LOCKFILE_NAME).exists());
        drop(lock);
        assert!(!dir.path().join(LOCKFILE_NAME).exists());
    }

    /// A corrupt/unparseable leftover is treated as stale: cleared and taken over.
    #[test]
    fn corrupt_lock_file_is_taken_over() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(dir.path().join(LOCKFILE_NAME), "not-a-lock").expect("write junk");
        StorageLock::try_acquire(dir.path()).expect("corrupt file must be taken over");
    }

    /// A dead pid in the lock file is stale: cleared and taken over.
    #[test]
    fn dead_pid_lock_file_is_taken_over() {
        let dir = tempfile::tempdir().expect("tempdir");
        // PID above any plausible process — `is_active` finds no such process.
        std::fs::write(dir.path().join(LOCKFILE_NAME), format!("{}\n0\n", u32::MAX))
            .expect("write stale");
        StorageLock::try_acquire(dir.path()).expect("dead-pid file must be taken over");
    }

    /// A live foreign holder is rejected with `Taken` (real second process: `sleep`).
    /// Same-process re-acquire is deliberately allowed (documented, out of scope).
    #[test]
    fn live_foreign_holder_is_taken() {
        let dir = tempfile::tempdir().expect("tempdir");
        let child = std::process::Command::new("sleep")
            .arg("5")
            .spawn()
            .expect("spawn live holder");
        let pid2 = sysinfo::Pid::from(child.id() as usize);
        let system = System::new_with_specifics(
            RefreshKind::nothing().with_processes(ProcessRefreshKind::nothing()),
        );
        let start_time = system
            .process(pid2)
            .expect("child must be visible")
            .start_time();
        std::fs::write(
            dir.path().join(LOCKFILE_NAME),
            format!("{}\n{}\n", child.id(), start_time),
        )
        .expect("write live lock");

        let err = StorageLock::try_acquire(dir.path()).expect_err("must reject live holder");
        assert!(matches!(err, StorageLockError::Taken(_)));

        let mut child = child;
        let _ = child.kill();
        let _ = child.wait();
        drop(system);
    }
}

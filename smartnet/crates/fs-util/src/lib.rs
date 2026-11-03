//! Wrapper for `std::fs` methods

#![cfg_attr(not(test), warn(unused_crate_dependencies))]
use flate2::{write::GzEncoder, Compression};
use serde::{de::DeserializeOwned, Serialize};
use std::{
    fs::{self, File, OpenOptions, ReadDir},
    io::{self, BufWriter, Error, Write},
    path::{Path, PathBuf},
};

/// Result alias for [`FsPathError`].
pub type Result<T> = std::result::Result<T, FsPathError>;

/// Various error variants for `std::fs` operations that serve as an addition to the `io::Error`
/// which does not provide any information about the path.
#[derive(Debug, thiserror::Error)]
pub enum FsPathError {
    /// Error variant for failed write operation with additional path context.
    #[error("failed to write to {path:?}: {source}")]
    Write {
        /// The source `io::Error`.
        source: io::Error,
        /// The path related to the operation.
        path: PathBuf,
    },

    /// Error variant for failed read operation with additional path context.
    #[error("failed to read from {path:?}: {source}")]
    Read {
        /// The source `io::Error`.
        source: io::Error,
        /// The path related to the operation.
        path: PathBuf,
    },

    /// Error variant for failed read link operation with additional path context.
    #[error("failed to read link {path:?}: {source}")]
    ReadLink {
        /// The source `io::Error`.
        source: io::Error,
        /// The path related to the operation.
        path: PathBuf,
    },

    /// Error variant for failed file creation operation with additional path context.
    #[error("failed to create file {path:?}: {source}")]
    CreateFile {
        /// The source `io::Error`.
        source: io::Error,
        /// The path related to the operation.
        path: PathBuf,
    },

    /// Error variant for failed file removal operation with additional path context.
    #[error("failed to remove file {path:?}: {source}")]
    RemoveFile {
        /// The source `io::Error`.
        source: io::Error,
        /// The path related to the operation.
        path: PathBuf,
    },

    /// Error variant for failed directory creation operation with additional path context.
    #[error("failed to create dir {path:?}: {source}")]
    CreateDir {
        /// The source `io::Error`.
        source: io::Error,
        /// The path related to the operation.
        path: PathBuf,
    },

    /// Error variant for failed directory removal operation with additional path context.
    #[error("failed to remove dir {path:?}: {source}")]
    RemoveDir {
        /// The source `io::Error`.
        source: io::Error,
        /// The path related to the operation.
        path: PathBuf,
    },

    /// Error variant for failed directory read operation with additional path context.
    #[error("failed to read dir {path:?}: {source}")]
    ReadDir {
        /// The source `io::Error`.
        source: io::Error,
        /// The path related to the operation.
        path: PathBuf,
    },

    /// Error variant for failed file renaming operation with additional path context.
    #[error("failed to rename {from:?} to {to:?}: {source}")]
    Rename {
        /// The source `io::Error`.
        source: io::Error,
        /// The original path.
        from: PathBuf,
        /// The target path.
        to: PathBuf,
    },

    /// Error variant for failed file opening operation with additional path context.
    #[error("failed to open file {path:?}: {source}")]
    Open {
        /// The source `io::Error`.
        source: io::Error,
        /// The path related to the operation.
        path: PathBuf,
    },

    /// Error variant for failed file read as JSON operation with additional path context.
    #[error("failed to parse json file: {path:?}: {source}")]
    ReadJson {
        /// The source `serde_json::Error`.
        source: serde_json::Error,
        /// The path related to the operation.
        path: PathBuf,
    },

    /// Error variant for failed JSON write to file operation with additional path context.
    #[error("failed to write to json file: {path:?}: {source}")]
    WriteJson {
        /// The source `serde_json::Error`.
        source: serde_json::Error,
        /// The path related to the operation.
        path: PathBuf,
    },

    /// Error variant for failed file metadata operation with additional path context.
    #[error("failed to get metadata for {path:?}: {source}")]
    Metadata {
        /// The source `io::Error`.
        source: io::Error,
        /// The path related to the operation.
        path: PathBuf,
    },
    /// Error variant for failed fsync operation with additional path context.
    #[error("failed to sync path {path:?}: {source}")]
    Fsync {
        /// The source `io::Error`.
        source: io::Error,
        /// The path related to the operation.
        path: PathBuf,
    },
}

impl FsPathError {
    /// Returns the complementary error variant for [`std::fs::write`].
    pub fn write(source: io::Error, path: impl Into<PathBuf>) -> Self {
        Self::Write { source, path: path.into() }
    }

    /// Returns the complementary error variant for [`std::fs::read`].
    pub fn read(source: io::Error, path: impl Into<PathBuf>) -> Self {
        Self::Read { source, path: path.into() }
    }

    /// Returns the complementary error variant for [`std::fs::read_link`].
    pub fn read_link(source: io::Error, path: impl Into<PathBuf>) -> Self {
        Self::ReadLink { source, path: path.into() }
    }

    /// Returns the complementary error variant for [`std::fs::File::create`].
    pub fn create_file(source: io::Error, path: impl Into<PathBuf>) -> Self {
        Self::CreateFile { source, path: path.into() }
    }

    /// Returns the complementary error variant for [`std::fs::remove_file`].
    pub fn remove_file(source: io::Error, path: impl Into<PathBuf>) -> Self {
        Self::RemoveFile { source, path: path.into() }
    }

    /// Returns the complementary error variant for [`std::fs::create_dir`].
    pub fn create_dir(source: io::Error, path: impl Into<PathBuf>) -> Self {
        Self::CreateDir { source, path: path.into() }
    }

    /// Returns the complementary error variant for [`std::fs::remove_dir`].
    pub fn remove_dir(source: io::Error, path: impl Into<PathBuf>) -> Self {
        Self::RemoveDir { source, path: path.into() }
    }

    /// Returns the complementary error variant for [`std::fs::read_dir`].
    pub fn read_dir(source: io::Error, path: impl Into<PathBuf>) -> Self {
        Self::ReadDir { source, path: path.into() }
    }

    /// Returns the complementary error variant for [`std::fs::File::open`].
    pub fn open(source: io::Error, path: impl Into<PathBuf>) -> Self {
        Self::Open { source, path: path.into() }
    }

    /// Returns the complementary error variant for [`std::fs::rename`].
    pub fn rename(source: io::Error, from: impl Into<PathBuf>, to: impl Into<PathBuf>) -> Self {
        Self::Rename { source, from: from.into(), to: to.into() }
    }

    /// Returns the complementary error variant for [`std::fs::File::metadata`].
    pub fn metadata(source: io::Error, path: impl Into<PathBuf>) -> Self {
        Self::Metadata { source, path: path.into() }
    }

    /// Returns the complementary error variant for `fsync`.
    pub fn fsync(source: io::Error, path: impl Into<PathBuf>) -> Self {
        Self::Fsync { source, path: path.into() }
    }
}

/// Wrapper for [`File::open`].
pub fn open(path: impl AsRef<Path>) -> Result<File> {
    let path = path.as_ref();
    File::open(path).map_err(|err| FsPathError::open(err, path))
}

/// Wrapper for `std::fs::read_to_string`
pub fn read_to_string(path: impl AsRef<Path>) -> Result<String> {
    let path = path.as_ref();
    fs::read_to_string(path).map_err(|err| FsPathError::read(err, path))
}

/// Read the entire contents of a file into a bytes vector.
///
/// Wrapper for `std::fs::read`
pub fn read(path: impl AsRef<Path>) -> Result<Vec<u8>> {
    let path = path.as_ref();
    fs::read(path).map_err(|err| FsPathError::read(err, path))
}

/// Wrapper for `std::fs::read_link`
pub fn read_link(path: impl AsRef<Path>) -> Result<PathBuf> {
    let path = path.as_ref();
    fs::read_link(path).map_err(|err| FsPathError::read_link(err, path))
}

/// Wrapper for `std::fs::write`
pub fn write(path: impl AsRef<Path>, contents: impl AsRef<[u8]>) -> Result<()> {
    let path = path.as_ref();
    fs::write(path, contents).map_err(|err| FsPathError::write(err, path))
}

/// Wrapper for `std::fs::remove_dir_all`
pub fn remove_dir_all(path: impl AsRef<Path>) -> Result<()> {
    let path = path.as_ref();
    fs::remove_dir_all(path).map_err(|err| FsPathError::remove_dir(err, path))
}

/// Wrapper for `File::create`.
pub fn create_file(path: impl AsRef<Path>) -> Result<fs::File> {
    let path = path.as_ref();
    File::create(path).map_err(|err| FsPathError::create_file(err, path))
}

/// Wrapper for `std::fs::remove_file`
pub fn remove_file(path: impl AsRef<Path>) -> Result<()> {
    let path = path.as_ref();
    fs::remove_file(path).map_err(|err| FsPathError::remove_file(err, path))
}

/// Removes a file at the given path, ignoring the error if the file does not exist
/// (`ErrorKind::NotFound`).
pub fn remove_file_if_exists(path: impl AsRef<Path>) -> Result<()> {
    match remove_file(&path) {
        Err(FsPathError::RemoveFile { source, .. }) if source.kind() == io::ErrorKind::NotFound => {
            Ok(())
        }
        result => result,
    }
}

/// Wrapper for `std::fs::create_dir_all`
pub fn create_dir_all(path: impl AsRef<Path>) -> Result<()> {
    let path = path.as_ref();
    fs::create_dir_all(path).map_err(|err| FsPathError::create_dir(err, path))
}

/// Wrapper for `std::fs::read_dir`
pub fn read_dir(path: impl AsRef<Path>) -> Result<ReadDir> {
    let path = path.as_ref();
    fs::read_dir(path).map_err(|err| FsPathError::read_dir(err, path))
}

/// Wrapper for `std::fs::rename`
pub fn rename(from: impl AsRef<Path>, to: impl AsRef<Path>) -> Result<()> {
    let from = from.as_ref();
    let to = to.as_ref();
    fs::rename(from, to).map_err(|err| FsPathError::rename(err, from, to))
}

/// Fsyncs a directory so the names *in* it survive a crash.
///
/// A rename is two halves: the directory entry changes, and (for a freshly written file) the
/// file's contents hit the device. [`atomic_write_file`] and the file-tier seal do the second
/// with `sync_all()` on the file — but that says nothing about the name. Until the directory
/// itself is fsynced, a crash can undo the rename: the `.tmp` sibling comes back and the final
/// path never existed. Call this after any create, rename or removal that must be durable.
pub fn fsync_dir(dir: impl AsRef<Path>) -> Result<()> {
    let dir = dir.as_ref();
    OpenOptions::new()
        .read(true)
        .open(dir)
        .map_err(|err| FsPathError::open(err, dir))?
        .sync_all()
        .map_err(|err| FsPathError::fsync(err, dir))
}

/// [`fsync_dir`] on the directory that *contains* `path` — the one whose entry a create,
/// rename or removal of `path` just changed.
///
/// A path with no parent directory component (a bare file name, or the root) has nothing to
/// sync and is a no-op rather than an error.
pub fn fsync_parent(path: impl AsRef<Path>) -> Result<()> {
    let path = path.as_ref();
    match path.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => fsync_dir(parent),
        _ => Ok(()),
    }
}

/// Wrapper for `std::fs::metadata`
pub fn metadata(path: impl AsRef<Path>) -> Result<fs::Metadata> {
    let path = path.as_ref();
    fs::metadata(path).map_err(|err| FsPathError::metadata(err, path))
}

/// Reads the JSON file and deserialize it into the provided type.
pub fn read_json_file<T: DeserializeOwned>(path: &Path) -> Result<T> {
    // read the file into a byte array first
    // https://github.com/serde-rs/json/issues/160
    let bytes = read(path)?;
    serde_json::from_slice(&bytes)
        .map_err(|source| FsPathError::ReadJson { source, path: path.into() })
}

/// Writes the object as a JSON object.
pub fn write_json_file<T: Serialize>(path: &Path, obj: &T) -> Result<()> {
    let file = create_file(path)?;
    let mut writer = BufWriter::new(file);
    serde_json::to_writer_pretty(&mut writer, obj)
        .map_err(|source| FsPathError::WriteJson { source, path: path.into() })?;
    writer.flush().map_err(|e| FsPathError::write(e, path))
}

/// Writes atomically to file.
///
/// 1. Creates a temporary file with a `.tmp` extension in the same file directory.
/// 2. Writes content with `write_fn`.
/// 3. Fsyncs the temp file to disk.
/// 4. Renames the temp file to the target path.
/// 5. Fsyncs the file directory.
///
/// Atomic writes are hard:
/// * <https://github.com/worldecomoney/smartnet/issues/8622>
/// * <https://users.rust-lang.org/t/how-to-write-replace-files-atomically/42821/13>
pub fn atomic_write_file<F, E>(file_path: &Path, write_fn: F) -> Result<()>
where
    F: FnOnce(&mut File) -> std::result::Result<(), E>,
    E: Into<Box<dyn core::error::Error + Send + Sync>>,
{
    let mut tmp_path = file_path.to_path_buf();
    tmp_path.set_extension("tmp");

    // Write to the temporary file
    let mut file =
        File::create(&tmp_path).map_err(|err| FsPathError::create_file(err, &tmp_path))?;

    // Execute the write function and handle errors properly
    // If write_fn fails, we need to clean up the temporary file before returning
    match write_fn(&mut file) {
        Ok(()) => {
            // Success - continue with the atomic operation
        }
        Err(err) => {
            // Clean up the temporary file before returning the error
            let _ = fs::remove_file(&tmp_path);
            return Err(FsPathError::Write { source: Error::other(err.into()), path: tmp_path });
        }
    }

    // fsync() file
    file.sync_all().map_err(|err| FsPathError::fsync(err, &tmp_path))?;

    // Rename file, not move
    rename(&tmp_path, file_path)?;

    // fsync() directory — the rename is not durable until the name is on the device.
    fsync_parent(file_path)?;

    Ok(())
}

/// Appends one JSON object per line to a plain JSONL file (no compression). This is the
/// active-shard writer (AMEND1): active shards are plain JSONL; only sealed shards are gzip.
///
/// The file is opened once, lazily on the first append (so a file that never receives a record
/// is never created), and lines are **buffered in memory**; [`Self::flush`] pushes them to the
/// OS. The batch writer flushes once per save batch (before the DB commit) and the seal step
/// flushes before compressing, so the file holds complete lines (plus possibly a partial last
/// line after a crash) — which is what makes line-based recovery trivial and keeps hot reads
/// decompression-free. Buffering replaces the former per-record `open`/`write`/`close`, which
/// made the append path syscall-bound on tx-heavy blocks.
pub struct JsonlAppender {
    path: PathBuf,
    /// The append-only handle, opened on the first write. `None` until then, so a file with
    /// no records is never created.
    file: Option<BufWriter<File>>,
    /// Whether lines have been appended since the last successful flush. Set by an append,
    /// cleared by [`Self::flush`] — it is what lets [`Self::flush_if_dirty`] report whether
    /// *this* call actually pushed the batch's writes to the OS (the per-file "landed"
    /// measurement the persistence log reports).
    dirty: bool,
}

impl std::fmt::Debug for JsonlAppender {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("JsonlAppender").field("path", &self.path).finish()
    }
}

impl JsonlAppender {
    /// Returns an appender for `path`, creating the parent directory. The file itself is opened
    /// append-only on the first [`Self::append_json_line`] / [`Self::append_bytes_line`].
    pub fn open(path: impl Into<PathBuf>) -> Result<Self> {
        let path = path.into();
        if let Some(parent) = path.parent() {
            create_dir_all(parent)?;
        }
        Ok(Self { path, file: None, dirty: false })
    }

    /// Appends one JSON Lines record (a serialized object followed by `\n`). The line is
    /// buffered; it reaches the OS on [`Self::flush`] (or when the appender is dropped).
    pub fn append_json_line<T: Serialize>(&mut self, value: &T) -> Result<()> {
        let mut line = serde_json::to_vec(value)
            .map_err(|source| FsPathError::WriteJson { source, path: self.path.clone() })?;
        line.push(b'\n');
        self.append_line(&line)
    }

    /// Appends raw bytes followed by `\n`.
    pub fn append_bytes_line(&mut self, bytes: &[u8]) -> Result<()> {
        let mut line = Vec::with_capacity(bytes.len() + 1);
        line.extend_from_slice(bytes);
        line.push(b'\n');
        self.append_line(&line)
    }

    /// Writes one line into the buffer, opening the file append-only on first use.
    fn append_line(&mut self, line: &[u8]) -> Result<()> {
        let file = match self.file.as_mut() {
            Some(file) => file,
            None => {
                let file = OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(&self.path)
                    .map_err(|err| FsPathError::open(err, &self.path))?;
                self.file.insert(BufWriter::new(file))
            }
        };
        file.write_all(line).map_err(|err| FsPathError::write(err, &self.path))?;
        self.dirty = true;
        Ok(())
    }

    /// Flushes the buffered lines to the OS (no fsync). Called once per save batch (before the
    /// DB commit) and at the seal step.
    pub fn flush(&mut self) -> Result<()> {
        self.flush_if_dirty()?;
        Ok(())
    }

    /// Flushes only when lines were appended since the last flush, and reports whether this
    /// call pushed writes to the OS. The persistence log uses the `true` instants as the
    /// per-file "last write landed" marks — a file with no lines this batch reports `false`
    /// (nothing landed, nothing to measure).
    pub fn flush_if_dirty(&mut self) -> Result<bool> {
        if !self.dirty {
            return Ok(false);
        }
        if let Some(file) = self.file.as_mut() {
            file.flush().map_err(|err| FsPathError::write(err, &self.path))?;
        }
        self.dirty = false;
        Ok(true)
    }

    /// Seal-step hook: flush the buffer and return the path. The file stays at its `.jsonl`
    /// name; compressing it is the engine's seal step.
    pub fn finish(mut self) -> Result<PathBuf> {
        self.flush()?;
        Ok(self.path)
    }

    /// The path this appender writes to.
    pub fn path(&self) -> &Path {
        &self.path
    }
}

/// Compresses `src` into `dst` in one gzip pass (streaming), fsyncs `dst`, and leaves `src`
/// untouched. Used by the shard seal: `.jsonl` → `.gz`.
pub fn gzip_file(src: impl AsRef<Path>, dst: impl AsRef<Path>) -> Result<()> {
    let src = src.as_ref();
    let dst = dst.as_ref();
    let mut input = File::open(src).map_err(|err| FsPathError::open(err, src))?;
    let out = File::create(dst).map_err(|err| FsPathError::create_file(err, dst))?;
    let mut encoder = GzEncoder::new(out, Compression::default());
    std::io::copy(&mut input, &mut encoder).map_err(|err| FsPathError::write(err, dst))?;
    let out = encoder.finish().map_err(|err| FsPathError::write(Error::other(err), dst))?;
    out.sync_all().map_err(|err| FsPathError::fsync(err, dst))?;
    Ok(())
}

#[cfg(test)]
mod jsonl_appender_tests {
    use super::*;

    fn read_all_lines(path: &Path) -> Vec<String> {
        let out = std::fs::read_to_string(path).unwrap();
        out.lines().map(str::to_string).collect()
    }

    /// Appends are buffered; a flush writes complete plain lines (no gzip, no member).
    #[test]
    fn each_append_is_a_readable_line() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("b.jsonl");

        let mut appender = JsonlAppender::open(&path).unwrap();
        for i in 0..3u32 {
            appender.append_json_line(&serde_json::json!({ "b": i })).unwrap();
            // Buffered: the line reaches the OS on flush, one complete line per append.
            appender.flush().unwrap();
            let lines = read_all_lines(&path);
            assert_eq!(lines.len(), (i + 1) as usize, "line {i} must be on disk after flush");
            assert!(lines[0].contains("\"b\":0"));
        }
        let lines = read_all_lines(&path);
        assert!(lines[2].contains("\"b\":2"));
    }

    /// Appends across two sessions (fresh handle, same path) concatenate via `O_APPEND`.
    #[test]
    fn reopen_and_continue_preserves_all_lines() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("txs.jsonl");

        {
            let mut appender = JsonlAppender::open(&path).unwrap();
            appender.append_json_line(&serde_json::json!({ "h": "0x1" })).unwrap();
        }
        {
            let mut appender = JsonlAppender::open(&path).unwrap();
            appender.append_json_line(&serde_json::json!({ "h": "0x2" })).unwrap();
            appender.finish().unwrap();
        }

        let lines = read_all_lines(&path);
        assert_eq!(lines.len(), 2);
        assert!(lines[0].contains("0x1"));
        assert!(lines[1].contains("0x2"));
    }

    /// `finish()` is the seal-step hook: it flushes the buffer and returns the path.
    #[test]
    fn finish_returns_path() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("4626.jsonl");

        let mut appender = JsonlAppender::open(&path).unwrap();
        appender.append_json_line(&serde_json::json!({ "b": 7, "ev": [] })).unwrap();
        let finished = appender.finish().unwrap();
        assert_eq!(finished, path);
        assert!(path.exists());
        assert!(read_all_lines(&path)[0].contains("\"b\":7"));
    }

    /// `append_bytes_line` writes raw bytes plus newline.
    #[test]
    fn append_bytes_line_writes_bytes() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("raw.jsonl");

        let mut appender = JsonlAppender::open(&path).unwrap();
        appender.append_bytes_line(b"{\"a\":1}").unwrap();
        appender.append_bytes_line(b"{\"a\":2}").unwrap();
        appender.finish().unwrap();

        assert_eq!(read_all_lines(&path), vec![r#"{"a":1}"#.to_string(), r#"{"a":2}"#.to_string()]);
    }

    /// `gzip_file` compresses a plain JSONL file into a gzip file that decodes back.
    #[test]
    fn gzip_file_round_trips() {
        use flate2::read::MultiGzDecoder;
        use std::io::Read;

        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("b.jsonl");
        let dst = dir.path().join("b.gz");
        std::fs::write(&src, "{\"b\":1}\n{\"b\":2}\n").unwrap();

        gzip_file(&src, &dst).unwrap();
        let mut decoder = MultiGzDecoder::new(File::open(&dst).unwrap());
        let mut out = String::new();
        decoder.read_to_string(&mut out).unwrap();
        assert_eq!(out, "{\"b\":1}\n{\"b\":2}\n");
    }
}
#[cfg(test)]
mod fsync_dir_tests {
    use super::*;

    /// Syncing a real directory succeeds; that is the whole contract of the helper — the
    /// durability itself is the filesystem's, so the test pins the paths it accepts.
    #[test]
    fn fsync_dir_syncs_a_real_directory_and_fails_on_a_missing_one() {
        let dir = std::env::temp_dir().join(format!("fs-util-fsync-dir-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        assert!(fsync_dir(&dir).is_ok(), "existing directory must sync");
        assert!(fsync_dir(dir.join("nope")).is_err(), "missing directory must not pass silently");

        // A file, not a directory: opening it read-only succeeds on Unix and syncing is a
        // no-op there, so only the missing path is asserted as an error above.
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn fsync_parent_ignores_a_path_with_no_directory_component() {
        assert!(fsync_parent("bare-file-name").is_ok());
        assert!(fsync_parent("").is_ok());
        assert!(fsync_parent("/").is_ok());
    }

    #[test]
    fn fsync_parent_follows_the_directory_of_a_nested_path() {
        let dir = std::env::temp_dir().join(format!("fs-util-fsync-parent-{}", std::process::id()));
        let nested = dir.join("a").join("b");
        fs::create_dir_all(&nested).unwrap();
        assert!(fsync_parent(nested.join("file.gz")).is_ok());
        fs::remove_dir_all(&dir).unwrap();
    }

    /// The path `atomic_write_file` relies on: content fsynced, then the directory entry.
    #[test]
    fn atomic_write_file_leaves_nothing_behind_but_the_target() {
        let dir = std::env::temp_dir().join(format!("fs-util-atomic-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let target = dir.join("x.json");
        atomic_write_file(&target, |f| f.write_all(b"hello")).unwrap();
        assert_eq!(fs::read(&target).unwrap(), b"hello");
        assert!(!target.with_extension("tmp").exists(), "tmp must be gone after the rename");
        fs::remove_dir_all(&dir).unwrap();
    }
}

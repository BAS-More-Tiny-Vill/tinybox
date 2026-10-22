//! Where a [`JailRegistry`](super::JailRegistry) keeps its index.

use std::fs;
use std::io;
use std::path::PathBuf;

/// Persistence for the registry's serialized index.
///
/// The registry owns the index shape and serializes it; a store only keeps the
/// bytes. That keeps the file format and the document-port format identical, so
/// moving between them is a copy.
pub trait IndexStore: Send + Sync + std::fmt::Debug + 'static {
    /// The stored index, or `None` when nothing was written yet.
    ///
    /// # Errors
    ///
    /// Returns an error when the index exists but cannot be read.
    fn load(&self) -> io::Result<Option<Vec<u8>>>;

    /// Replace the stored index with `bytes`.
    ///
    /// # Errors
    ///
    /// Returns an error when the index cannot be persisted.
    fn save(&self, bytes: &[u8]) -> io::Result<()>;
}

/// The original on-disk index: one JSON file, replaced by write-temp + rename.
#[derive(Debug, Clone)]
pub struct FileIndexStore {
    path: PathBuf,
}

impl FileIndexStore {
    /// An index file at `path`.
    #[must_use]
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }

    fn tmp_path(&self) -> PathBuf {
        let mut name = self.path.file_name().unwrap_or_default().to_owned();
        name.push(".tmp");
        self.path.with_file_name(name)
    }
}

impl IndexStore for FileIndexStore {
    fn load(&self) -> io::Result<Option<Vec<u8>>> {
        match fs::read(&self.path) {
            Ok(raw) => {
                log::debug!(
                    "[cwd_jail] registry.open loading index {}",
                    self.path.display()
                );
                Ok(Some(raw))
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(error),
        }
    }

    /// Atomic-rename write. Falls back to a direct write on Windows if
    /// rename-over fails (older NTFS refused rename over an existing file).
    fn save(&self, bytes: &[u8]) -> io::Result<()> {
        let tmp = self.tmp_path();
        fs::write(&tmp, bytes)?;
        match fs::rename(&tmp, &self.path) {
            Ok(()) => Ok(()),
            Err(error) => {
                log::debug!(
                    "[cwd_jail] registry.persist rename failed ({error}); falling back to overwrite"
                );
                fs::write(&self.path, bytes)?;
                let _ = fs::remove_file(&tmp);
                Ok(())
            }
        }
    }
}

#[cfg(test)]
#[path = "registry_index_store_tests.rs"]
#[allow(clippy::expect_used, clippy::unwrap_used)]
mod tests;

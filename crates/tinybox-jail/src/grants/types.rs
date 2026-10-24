//! Canonical local grant results.
use std::path::PathBuf;

/// Paths the jail is granted beyond the workspace root, all canonical and
/// existing.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct JailGrants {
    /// Canonical read and executable paths.
    pub read_only: Vec<PathBuf>,
    /// Canonical writable paths.
    pub read_write: Vec<PathBuf>,
}

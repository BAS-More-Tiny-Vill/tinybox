//! Bounded, fail-closed discovery of protected descendant names.
use super::Builder;
use std::path::Path;

impl Builder<'_> {
    /// Inspect recursive grants at discovery time. Never follow nested
    /// symlinks: recursive kernel rules cover this directory tree, not targets
    /// outside it. Check symlink names themselves before inspecting their type.
    /// Carry the requested spelling too, so an aliased `.cargo` still protects
    /// its `credentials.toml` child. Errors and traversal limits fail closed.
    pub(super) fn subtree_is_clear(
        &self,
        requested: &Path,
        canonical: &Path,
        max_entries: usize,
        max_depth: usize,
    ) -> std::io::Result<bool> {
        if self.protected.components.is_empty() && self.protected.component_pairs.is_empty() {
            return Ok(true);
        }
        if !std::fs::metadata(canonical)?.is_dir() {
            return Ok(true);
        }
        let mut stack = vec![(
            std::fs::read_dir(canonical)?,
            canonical.to_path_buf(),
            requested.to_path_buf(),
        )];
        let mut visited = 0;
        while let Some((entries, physical, logical)) = stack.last_mut() {
            let Some(entry) = entries.next() else {
                stack.pop();
                continue;
            };
            let entry = entry?;
            if visited == max_entries {
                return Err(std::io::Error::other(
                    "protected-path inspection entry limit exceeded",
                ));
            }
            visited += 1;
            let physical_child = physical.join(entry.file_name());
            let logical_child = logical.join(entry.file_name());
            if self.has_protected_components(&physical_child)
                || self.has_protected_components(&logical_child)
            {
                return Ok(false);
            }
            let file_type = entry.file_type()?;
            // A pair-prefix symlink could alias a directory elsewhere inside
            // this same grant, exposing its protected child under the alias.
            // Reject it conservatively rather than following links or cycles.
            if file_type.is_symlink()
                && self.protected.component_pairs.iter().any(|pair| {
                    entry
                        .file_name()
                        .to_string_lossy()
                        .eq_ignore_ascii_case(&pair[0])
                })
            {
                return Ok(false);
            }
            if file_type.is_dir() {
                if stack.len() >= max_depth {
                    return Err(std::io::Error::other(
                        "protected-path inspection depth limit exceeded",
                    ));
                }
                stack.push((
                    std::fs::read_dir(&physical_child)?,
                    physical_child,
                    logical_child,
                ));
            }
        }
        Ok(true)
    }
}

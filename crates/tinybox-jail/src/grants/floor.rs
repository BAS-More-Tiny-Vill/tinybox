//! Protected root spellings and component-boundary matching.
use super::Builder;
use std::path::Path;

impl Builder<'_> {
    pub(super) fn refuses_path(&self, path: &Path) -> bool {
        let normalized = normalized_floor_path(&path.to_string_lossy());
        // Procfs is reserved for the dedicated read-only toggle, including
        // ancestors whose recursive grant would expose it.
        let exposes_proc = Path::new("/proc").exists()
            && (same_or_descendant(&normalized, "/proc")
                || same_or_descendant("/proc", &normalized));
        self.is_protected(path) || self.exposes_credentials(path) || exposes_proc
    }

    /// True when `path` is, contains, or sits inside a credential directory.
    pub(super) fn exposes_credentials(&self, path: &Path) -> bool {
        let path = normalized_floor_path(&path.to_string_lossy());
        self.protected.credential_roots.iter().any(|root| {
            root_spellings(Path::new(root))
                .any(|cred| same_or_descendant(&path, &cred) || same_or_descendant(&cred, &path))
        })
    }

    pub(super) fn has_protected_components(&self, path: &Path) -> bool {
        let normalized = normalized_floor_path(&path.to_string_lossy());
        let components: Vec<_> = normalized
            .split('/')
            .filter(|part| !part.is_empty())
            .collect();
        components.iter().any(|part| {
            self.protected
                .components
                .iter()
                .any(|blocked| part.eq_ignore_ascii_case(blocked))
        }) || components.windows(2).any(|pair| {
            self.protected.component_pairs.iter().any(|blocked| {
                pair[0].eq_ignore_ascii_case(&blocked[0])
                    && pair[1].eq_ignore_ascii_case(&blocked[1])
            })
        })
    }

    pub(super) fn is_protected(&self, path: &Path) -> bool {
        let normalized = normalized_floor_path(&path.to_string_lossy());
        self.has_protected_components(path)
            || self.protected.prefixes.iter().any(|prefix| {
                root_spellings(Path::new(prefix)).any(|prefix| {
                    same_or_descendant(&normalized, &prefix)
                        || same_or_descendant(&prefix, &normalized)
                })
            })
    }
}

// A configured protected root may itself be a symlink (for example /etc on
// macOS). Preserve its requested spelling and its existing resolved target.
pub(super) fn root_spellings(root: &Path) -> impl Iterator<Item = String> {
    let location = root.parent().and_then(|parent| {
        let mut ancestor = parent;
        let mut missing = Vec::new();
        let mut canonical = loop {
            if let Ok(canonical) = ancestor.canonicalize() {
                break canonical;
            }
            missing.push(ancestor.file_name()?);
            ancestor = ancestor.parent()?;
        };
        for component in missing.into_iter().rev() {
            canonical.push(component);
        }
        canonical.push(root.file_name()?);
        Some(normalized_floor_path(&canonical.to_string_lossy()))
    });
    std::iter::once(normalized_floor_path(&root.to_string_lossy()))
        .chain(location)
        .chain(
            root.canonicalize()
                .ok()
                .map(|canonical| normalized_floor_path(&canonical.to_string_lossy())),
        )
}

// Windows canonical paths may carry an extended drive or UNC namespace;
// compare the underlying filesystem path, regardless of spelling or case.
pub(super) fn normalized_floor_path(path: &str) -> String {
    let normalized = path.to_ascii_lowercase().replace('\\', "/");
    if let Some(rest) = normalized.strip_prefix("//?/unc/") {
        format!("//{rest}")
    } else {
        normalized
            .strip_prefix("//?/")
            .unwrap_or(&normalized)
            .to_owned()
    }
}

pub(super) fn same_or_descendant(candidate: &str, parent: &str) -> bool {
    let root = parent.trim_end_matches('/');
    if root.is_empty() {
        return parent.starts_with('/') && candidate.starts_with('/');
    }
    candidate == root
        || candidate
            .strip_prefix(root)
            .is_some_and(|rest| rest.starts_with('/'))
}

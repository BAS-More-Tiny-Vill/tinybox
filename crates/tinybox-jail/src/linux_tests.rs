//! Tests for the Linux Landlock backend.

use super::*;
use std::path::Path;

#[test]
fn landlock_spawns_with_configured_system_read_paths() -> std::io::Result<()> {
    let root = tempfile::tempdir()?;
    let mut jail = Jail::new(root.path(), "landlock-test");
    for path in ["/usr", "/bin", "/lib", "/lib64"] {
        if Path::new(path).exists() {
            jail = jail.add_read_only(path);
        }
    }

    let backend = LandlockBackend::new();
    if !backend.is_available() {
        let error = backend
            .spawn(&jail, Command::new("/usr/bin/true"))
            .err()
            .map(|error| error.kind());
        assert_eq!(error, Some(std::io::ErrorKind::PermissionDenied));
        return Ok(());
    }

    let mut child = backend.spawn(&jail, Command::new("/usr/bin/true"))?;
    assert!(child.wait()?.success());
    Ok(())
}

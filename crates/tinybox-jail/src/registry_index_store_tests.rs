use super::*;

#[test]
fn a_missing_index_file_loads_as_absent() {
    let dir = tempfile::tempdir().expect("dir");
    let store = FileIndexStore::new(dir.path().join("index.json"));
    assert_eq!(store.load().expect("load"), None);
}

#[test]
fn a_saved_index_loads_back_and_leaves_no_temp_file() {
    let dir = tempfile::tempdir().expect("dir");
    let store = FileIndexStore::new(dir.path().join("index.json"));
    store.save(b"{\"a\":1}").expect("save");
    store.save(b"{\"a\":2}").expect("replace");
    assert_eq!(store.load().expect("load"), Some(b"{\"a\":2}".to_vec()));
    assert!(!dir.path().join("index.json.tmp").exists());
}

#[test]
fn an_unreadable_index_is_an_error_not_absence() {
    let dir = tempfile::tempdir().expect("dir");
    // A directory where the file should be cannot be read as one.
    let path = dir.path().join("index.json");
    fs::create_dir(&path).expect("mkdir");
    assert!(FileIndexStore::new(path).load().is_err());
}

#[test]
fn a_save_that_cannot_land_reports_the_failure() {
    let dir = tempfile::tempdir().expect("dir");
    let path = dir.path().join("index.json");
    fs::create_dir(&path).expect("mkdir");
    assert!(FileIndexStore::new(path).save(b"{}").is_err());
}

#[cfg(unix)]
#[test]
fn a_refused_rename_falls_back_to_overwriting_in_place() {
    use std::os::unix::fs::PermissionsExt;

    let dir = tempfile::tempdir().expect("dir");
    let path = dir.path().join("index.json");
    fs::write(&path, b"old").expect("seed");
    fs::write(dir.path().join("index.json.tmp"), b"").expect("tmp");
    // Existing files stay writable, but nothing can be renamed inside.
    fs::set_permissions(dir.path(), fs::Permissions::from_mode(0o555)).expect("chmod");

    let store = FileIndexStore::new(&path);
    let saved = store.save(b"new");
    fs::set_permissions(dir.path(), fs::Permissions::from_mode(0o755)).expect("restore");

    saved.expect("save");
    assert_eq!(fs::read(&path).expect("read"), b"new");
}

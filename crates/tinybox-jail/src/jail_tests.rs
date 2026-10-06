use super::*;

#[test]
fn defaults_are_permissive() {
    let j = Jail::new("/tmp", "x");
    assert!(j.allow_net);
    assert!(j.allow_subprocess);
    assert_eq!(j.label, "x");
    assert_eq!(j.read_only, Vec::<PathBuf>::new());
}

#[test]
fn deny_net_is_idempotent() {
    let j = Jail::new("/tmp", "x").deny_net().deny_net();
    assert!(!j.allow_net);
}

#[test]
fn deny_subprocess_is_idempotent() {
    let j = Jail::new("/tmp", "x").deny_subprocess().deny_subprocess();
    assert!(!j.allow_subprocess);
}

#[test]
fn add_read_only_appends_in_order() {
    let j = Jail::new("/tmp", "x")
        .add_read_only("/a")
        .add_read_only("/b")
        .add_read_only("/c");
    assert_eq!(j.read_only.len(), 3);
    assert_eq!(j.read_only[0], PathBuf::from("/a"));
    assert_eq!(j.read_only[2], PathBuf::from("/c"));
}

#[test]
fn canonicalize_resolves_real_path() {
    let dir = std::env::temp_dir();
    let mut j = Jail::new(&dir, "x");
    j.canonicalize().unwrap();
    // After canonicalize, root has no `..` and resolves to a real path.
    assert!(j.root.is_absolute());
    assert!(j.root.exists());
}

#[test]
fn canonicalize_swallows_missing_read_only() {
    // read_only entries that don't exist are silently dropped from
    // canonicalization (they stay as-is). Verify no panic.
    let dir = std::env::temp_dir();
    let mut j = Jail::new(&dir, "x").add_read_only("/this/never/existed");
    j.canonicalize().unwrap();
    assert_eq!(j.read_only.len(), 1);
}

#[test]
fn canonicalize_errors_on_missing_root() {
    let mut j = Jail::new("/no/such/root/here", "x");
    let err = j.canonicalize().unwrap_err();
    assert_eq!(err.kind(), std::io::ErrorKind::NotFound);
}

#[test]
fn defaults_have_no_extra_read_write_paths() {
    let j = Jail::new("/tmp", "x");
    assert_eq!(j.read_write, Vec::<PathBuf>::new());
}

#[test]
fn add_read_write_appends_in_order_and_leaves_read_only_alone() {
    let j = Jail::new("/tmp", "x")
        .add_read_write("/a")
        .add_read_write("/b");
    assert_eq!(j.read_write, vec![PathBuf::from("/a"), PathBuf::from("/b")]);
    assert_eq!(j.read_only, Vec::<PathBuf>::new());
    assert_eq!(j.root, PathBuf::from("/tmp"));
}

#[test]
fn canonicalize_resolves_read_write_paths() {
    let root = tempfile::tempdir().unwrap();
    let extra = tempfile::tempdir().unwrap();
    let dotted = extra.path().join("sub").join("..");
    std::fs::create_dir_all(extra.path().join("sub")).unwrap();
    let mut j = Jail::new(root.path(), "x").add_read_write(&dotted);
    j.canonicalize().unwrap();
    assert_eq!(j.read_write, vec![extra.path().canonicalize().unwrap()]);
}

#[test]
fn canonicalize_keeps_missing_read_write_as_is() {
    let root = tempfile::tempdir().unwrap();
    let mut j = Jail::new(root.path(), "x").add_read_write("/this/never/existed");
    j.canonicalize().unwrap();
    assert_eq!(j.read_write, vec![PathBuf::from("/this/never/existed")]);
}

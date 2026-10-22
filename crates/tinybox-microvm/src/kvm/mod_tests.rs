use super::*;

#[test]
fn open_results_map_to_distinct_statuses() {
    assert_eq!(KvmStatus::from_open(Ok(())), KvmStatus::Available);
    assert_eq!(
        KvmStatus::from_open(Err(ErrorKind::NotFound)),
        KvmStatus::Missing
    );
    assert_eq!(
        KvmStatus::from_open(Err(ErrorKind::PermissionDenied)),
        KvmStatus::PermissionDenied
    );
    assert_eq!(
        KvmStatus::from_open(Err(ErrorKind::Other)),
        KvmStatus::Unusable
    );
}

#[test]
fn only_an_available_device_needs_no_advice() {
    assert!(KvmStatus::Available.is_available());
    assert_eq!(KvmStatus::Available.advice(), None);
    for status in [
        KvmStatus::Missing,
        KvmStatus::PermissionDenied,
        KvmStatus::Unusable,
    ] {
        assert!(!status.is_available());
        assert!(status.advice().is_some());
    }
}

#[test]
fn a_nonexistent_device_path_is_missing() {
    let dir = std::env::temp_dir().join("tinybox-no-such-kvm-device");
    assert_eq!(probe_device(&dir), KvmStatus::Missing);
}

#[test]
fn a_plain_writable_file_probes_as_available() -> std::io::Result<()> {
    let file = tempfile::NamedTempFile::new()?;
    assert_eq!(probe_device(file.path()), KvmStatus::Available);
    Ok(())
}

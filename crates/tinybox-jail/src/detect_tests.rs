//! Tests for backend selection and the unavailable-backend result.

use super::*;

#[test]
fn unavailable_backend_rejects_spawning() {
    let backend = UnsupportedBackend;
    assert_eq!(backend.name(), "unsupported");
    assert!(!backend.is_available());
    let error = backend
        .spawn(&Jail::new(".", "unsupported"), Command::new("true"))
        .err()
        .map(|error| error.kind());
    assert_eq!(error, Some(std::io::ErrorKind::Unsupported));
}

#[test]
fn backend_detection_returns_a_backend() {
    assert_ne!(pick_backend().name(), "");
}

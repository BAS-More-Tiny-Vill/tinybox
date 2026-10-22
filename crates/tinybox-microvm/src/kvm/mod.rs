//! Discoverability for the hardware prerequisite of the microVM backend.
//!
//! Firecracker needs `/dev/kvm` opened read-write. A missing device and a
//! device the caller may not open fail the same way at boot, so [`probe`]
//! checks first and says which it is and what to do about it.

use std::io::ErrorKind;
use std::path::Path;

/// The path Firecracker opens.
pub const KVM_DEVICE: &str = "/dev/kvm";

/// Whether this machine can run a microVM.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KvmStatus {
    /// The device opens read-write.
    Available,
    /// There is no `/dev/kvm`: no virtualization support, the module is not
    /// loaded, or the machine is itself a guest without nested virtualization.
    Missing,
    /// The device exists but this user may not open it.
    PermissionDenied,
    /// The device exists but could not be opened for another reason.
    Unusable,
}

impl KvmStatus {
    /// Classify the result of opening the device read-write.
    #[must_use]
    pub fn from_open(result: Result<(), ErrorKind>) -> Self {
        match result {
            Ok(()) => Self::Available,
            Err(ErrorKind::NotFound) => Self::Missing,
            Err(ErrorKind::PermissionDenied) => Self::PermissionDenied,
            Err(_) => Self::Unusable,
        }
    }

    /// Whether a microVM can be expected to boot.
    #[must_use]
    pub const fn is_available(self) -> bool {
        matches!(self, Self::Available)
    }

    /// What to do about it, or `None` when KVM is available.
    #[must_use]
    pub const fn advice(self) -> Option<&'static str> {
        match self {
            Self::Available => None,
            Self::Missing => Some(
                "/dev/kvm does not exist; enable virtualization, `modprobe kvm_intel` or \
                 `kvm_amd`, or run on a host with nested virtualization",
            ),
            Self::PermissionDenied => Some(
                "/dev/kvm is not accessible to this user; add the user to the `kvm` group \
                 and log in again",
            ),
            Self::Unusable => Some("/dev/kvm could not be opened; check dmesg and kernel support"),
        }
    }
}

/// Probe `device` by opening it read-write, as Firecracker will.
#[must_use]
pub fn probe_device(device: &Path) -> KvmStatus {
    KvmStatus::from_open(
        std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(device)
            .map(drop)
            .map_err(|error| error.kind()),
    )
}

/// Probe the real `/dev/kvm`.
#[must_use]
pub fn probe() -> KvmStatus {
    probe_device(Path::new(KVM_DEVICE))
}

#[cfg(test)]
#[path = "mod_tests.rs"]
mod tests;

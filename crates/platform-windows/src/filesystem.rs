//! File-system features are queried from a pinned handle, not a drive letter.
use super::{IdentityKind, VolumeInformation};
use serde::Serialize;
use windows_sys::Win32::System::SystemServices::{FILE_NAMED_STREAMS, FILE_READ_ONLY_VOLUME};

#[derive(Debug, Clone, Serialize)]
pub struct CopyCapabilities {
    pub filesystem: String,
    pub named_streams: Option<bool>,
    pub read_only: Option<bool>,
    /// None means the driver decides the limit, not an unlimited-size promise.
    pub maximum_file_bytes: Option<u64>,
    pub maximum_component_length: Option<u32>,
}

impl VolumeInformation {
    pub fn copy_capabilities(&self) -> CopyCapabilities {
        CopyCapabilities {
            filesystem: self.filesystem.clone(),
            named_streams: Some(self.flags & FILE_NAMED_STREAMS != 0),
            read_only: Some(self.flags & FILE_READ_ONLY_VOLUME != 0),
            maximum_file_bytes: if ["FAT", "FAT12", "FAT16", "FAT32"]
                .iter()
                .any(|name| self.filesystem.eq_ignore_ascii_case(name))
            {
                Some(u32::MAX as u64)
            } else {
                None
            },
            maximum_component_length: (self.maximum_component_length != 0)
                .then_some(self.maximum_component_length),
        }
    }
}

impl CopyCapabilities {
    pub fn unknown() -> Self {
        Self {
            filesystem: "unknown".into(),
            named_streams: None,
            read_only: None,
            maximum_file_bytes: None,
            maximum_component_length: None,
        }
    }

    pub fn identity_kind(&self) -> IdentityKind {
        if ["NTFS", "ReFS"]
            .iter()
            .any(|name| self.filesystem.eq_ignore_ascii_case(name))
        {
            IdentityKind::Native128
        } else if self.filesystem.eq_ignore_ascii_case("exFAT") {
            IdentityKind::ExFatSource
        } else if self.maximum_file_bytes == Some(u32::MAX as u64) {
            IdentityKind::FatSource
        } else {
            IdentityKind::PathSource
        }
    }

    pub fn validate_streams(&self, streams: &[super::Stream]) -> std::io::Result<()> {
        use std::io::{Error, ErrorKind};
        if self.named_streams == Some(false) && streams.iter().any(|s| !s.name.is_empty()) {
            return Err(Error::new(
                ErrorKind::Unsupported,
                format!(
                    "{} не підтримує додаткові потоки NTFS (ADS). Копіювання не розпочато, щоб не втратити ці дані.",
                    self.filesystem
                ),
            ));
        }
        if let Some(maximum) = self.maximum_file_bytes
            && streams.iter().any(|s| s.size > maximum)
        {
            return Err(Error::new(
                ErrorKind::Unsupported,
                format!(
                    "FAT32 не підтримує файл понад 4 ГіБ − 1 байт (носій {}). Виберіть NTFS або exFAT.",
                    self.filesystem
                ),
            ));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn filesystem_profiles_keep_exfat_large_and_driver_features_distinct() {
        for name in [
            "NTFS",
            "ReFS",
            "FAT32",
            "exFAT",
            "UDF",
            "CDFS",
            "third-party",
        ] {
            let profile = VolumeInformation {
                filesystem: name.into(),
                flags: 0,
                serial: 1,
                maximum_component_length: 255,
            }
            .copy_capabilities();
            let large = [crate::Stream {
                name: "".into(),
                size: u32::MAX as u64 + 1,
            }];
            assert_eq!(
                profile.validate_streams(&large).is_ok(),
                name != "FAT32",
                "{name}"
            );
            assert_eq!(
                profile.identity_kind().is_path_scoped(),
                !matches!(name, "NTFS" | "ReFS")
            );
            let named = [crate::Stream {
                name: ":note:$DATA".into(),
                size: 7,
            }];
            assert!(profile.validate_streams(&named).is_err());
            let advertised = VolumeInformation {
                filesystem: name.into(),
                flags: FILE_NAMED_STREAMS | FILE_READ_ONLY_VOLUME,
                serial: 1,
                maximum_component_length: 255,
            }
            .copy_capabilities();
            assert!(advertised.validate_streams(&named).is_ok());
            assert_eq!(advertised.read_only, Some(true));
        }
        assert_eq!(CopyCapabilities::unknown().named_streams, None);
        assert_eq!(CopyCapabilities::unknown().maximum_component_length, None);
    }
}

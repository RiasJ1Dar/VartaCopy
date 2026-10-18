//! Versioned transfer intent. Decoding never opens paths or starts an operation.
use crate::{MAX_REQUEST_BYTES, MAX_SOURCES};
use serde::{Deserialize, Serialize};
use std::{
    ffi::OsString,
    io,
    os::windows::ffi::{OsStrExt, OsStringExt},
    path::{Path, PathBuf},
};
use windows_sys::Win32::Security::Cryptography::{
    BCRYPT_USE_SYSTEM_PREFERRED_RNG, BCryptGenRandom,
};

pub const COPYDATA_TRANSFER_MAGIC: usize = 0x56435232;
pub const TRANSFER_PROTOCOL_VERSION: u32 = 2;
pub const MAX_PATH_UNITS: usize = 32767;

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TransferOperation {
    Copy,
    Move,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TransferOrigin {
    RightDrag,
    ContextPaste,
    AppDrop,
    AppPaste,
    AppForm,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TransferRequest {
    pub id: [u8; 16],
    pub operation: TransferOperation,
    pub origin: TransferOrigin,
    pub sources: Vec<PathBuf>,
    pub destination: PathBuf,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct WireRequest {
    version: u32,
    id: [u8; 16],
    operation: TransferOperation,
    origin: TransferOrigin,
    sources: Vec<Vec<u16>>,
    destination: Vec<u16>,
}

/// Generate a request identity with the operating system's CSPRNG (BCryptGenRandom).
/// Retain this identity when retrying delivery; a receiver must deduplicate before execution.
pub fn new_transfer_id() -> io::Result<[u8; 16]> {
    let mut id = [0; 16];
    // SAFETY: The system RNG writes exactly the provided initialized output buffer.
    let status = unsafe {
        BCryptGenRandom(
            std::ptr::null_mut(),
            id.as_mut_ptr(),
            id.len() as u32,
            BCRYPT_USE_SYSTEM_PREFERRED_RNG,
        )
    };
    if status < 0 {
        return Err(io::Error::other(format!(
            "BCryptGenRandom failed: {status:#x}"
        )));
    }
    if id == [0; 16] {
        return Err(invalid("OS RNG returned an invalid zero request identity"));
    }
    Ok(id)
}

pub fn encode_transfer(request: &TransferRequest) -> io::Result<Vec<u8>> {
    validate_request(request)?;
    let wire = WireRequest {
        version: TRANSFER_PROTOCOL_VERSION,
        id: request.id,
        operation: request.operation,
        origin: request.origin,
        sources: request.sources.iter().map(|p| units(p)).collect(),
        destination: units(&request.destination),
    };
    let bytes = serde_json::to_vec(&wire)?;
    if bytes.len() > MAX_REQUEST_BYTES {
        return Err(invalid("transfer request exceeds 1 MiB"));
    }
    Ok(bytes)
}

pub fn decode_transfer(bytes: &[u8]) -> io::Result<TransferRequest> {
    if bytes.len() > MAX_REQUEST_BYTES {
        return Err(invalid("transfer request exceeds 1 MiB"));
    }
    let wire: WireRequest = serde_json::from_slice(bytes)?;
    if wire.version != TRANSFER_PROTOCOL_VERSION {
        return Err(invalid("unsupported transfer protocol version"));
    }
    let request = TransferRequest {
        id: wire.id,
        operation: wire.operation,
        origin: wire.origin,
        sources: wire.sources.iter().map(|p| from_units(p)).collect(),
        destination: from_units(&wire.destination),
    };
    validate_request(&request)?;
    Ok(request)
}

fn validate_request(request: &TransferRequest) -> io::Result<()> {
    if request.id == [0; 16] {
        return Err(invalid("transfer request identity must be nonzero"));
    }
    validate_sources(&request.sources)?;
    validate_path(&request.destination, false)
}

pub(crate) fn validate_sources(sources: &[PathBuf]) -> io::Result<()> {
    if sources.is_empty() || sources.len() > MAX_SOURCES {
        return Err(invalid("transfer requires 1..512 sources"));
    }
    for source in sources {
        validate_path(source, true)?;
    }
    Ok(())
}

fn validate_path(path: &Path, source: bool) -> io::Result<()> {
    let path = units(path);
    if path.len() > MAX_PATH_UNITS || path.contains(&0) {
        return Err(invalid("invalid native path length or embedded NUL"));
    }
    let sep = |c| c == b'\\' as u16 || c == b'/' as u16;
    let drive = if path.starts_with(&[92, 92, 63, 92]) {
        4
    } else {
        0
    };
    if path.len() < drive + 3
        || !matches!(path[drive], 65..=90 | 97..=122)
        || path[drive + 1] != b':' as u16
        || !sep(path[drive + 2])
        || (drive == 4 && path[drive + 2] != b'\\' as u16)
    {
        return Err(invalid(
            "only absolute local drive and VerbatimDisk paths are supported",
        ));
    }
    let mut has_name = false;
    for component in path[drive + 3..].split(|&c| sep(c)) {
        if component == [46, 46] {
            return Err(invalid("parent traversal is not permitted"));
        }
        if !component.is_empty() && component != [46] {
            has_name = true;
        }
    }
    if source && !has_name {
        return Err(invalid("a drive root cannot be a transfer source"));
    }
    Ok(())
}

fn units(path: &Path) -> Vec<u16> {
    path.as_os_str().encode_wide().collect()
}
fn from_units(path: &[u16]) -> PathBuf {
    PathBuf::from(OsString::from_wide(path))
}
fn invalid(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, message)
}

#[cfg(test)]
mod tests {
    use super::*;
    fn request() -> TransferRequest {
        TransferRequest {
            id: [1; 16],
            operation: TransferOperation::Move,
            origin: TransferOrigin::ContextPaste,
            sources: vec![PathBuf::from(r"C:\дані\file")],
            destination: PathBuf::from(r"D:\target"),
        }
    }
    #[test]
    fn native_utf16_round_trip_and_all_intents() {
        let mut r = request();
        let mut path = r"\\?\C:\дані\".encode_utf16().collect::<Vec<_>>();
        path.extend([0xd800, 0xdc00, 0xdfff]);
        r.sources = vec![from_units(&path)];
        for operation in [TransferOperation::Copy, TransferOperation::Move] {
            for origin in [
                TransferOrigin::RightDrag,
                TransferOrigin::ContextPaste,
                TransferOrigin::AppDrop,
                TransferOrigin::AppPaste,
            ] {
                r.operation = operation;
                r.origin = origin;
                assert_eq!(decode_transfer(&encode_transfer(&r).unwrap()).unwrap(), r);
            }
        }
    }
    #[test]
    fn reject_unsafe_paths_counts_and_zero_identity() {
        let mut r = request();
        for path in [
            r"C:\",
            r"\\?\C:\",
            r"C:\.\",
            "relative",
            r"C:relative",
            r"\rooted",
            r"\\server\share\file",
            r"\\?\UNC\server\share\file",
            r"\\.\C:\file",
            r"C:\a\..\b",
            r"\\?\C:\a\..\b",
            "C:\\a\0b",
        ] {
            r.sources = vec![PathBuf::from(path)];
            assert!(encode_transfer(&r).is_err(), "accepted {path:?}");
        }
        r.sources = vec![];
        assert!(encode_transfer(&r).is_err());
        r.sources = vec![PathBuf::from(r"C:\file"); MAX_SOURCES + 1];
        assert!(encode_transfer(&r).is_err());
        r = request();
        r.id = [0; 16];
        assert!(encode_transfer(&r).is_err());
        r = request();
        r.destination = PathBuf::from(r"C:\a\..\b");
        assert!(encode_transfer(&r).is_err());
        r.destination = PathBuf::from(r"D:\");
        assert!(encode_transfer(&r).is_ok());
    }
    #[test]
    fn reject_malformed_unknown_and_oversize_requests() {
        let value = serde_json::to_value(WireRequest {
            version: 2,
            id: [1; 16],
            operation: TransferOperation::Copy,
            origin: TransferOrigin::AppDrop,
            sources: vec![units(Path::new(r"C:\file"))],
            destination: units(Path::new(r"D:\")),
        })
        .unwrap();
        for (field, bad) in [
            ("version", serde_json::json!(1)),
            ("operation", serde_json::json!("link")),
            ("origin", serde_json::json!("future")),
            ("extra", serde_json::json!(true)),
            ("sources", serde_json::json!([[67, 58, 92, 0]])),
        ] {
            let mut bad_value = value.clone();
            bad_value[field] = bad;
            assert!(decode_transfer(&serde_json::to_vec(&bad_value).unwrap()).is_err());
        }
        assert!(decode_transfer(b"{").is_err());
        assert!(decode_transfer(&vec![b' '; MAX_REQUEST_BYTES + 1]).is_err());
        let mut r = request();
        r.sources = vec![PathBuf::from(format!("C:\\{}", "x".repeat(MAX_PATH_UNITS))); 1];
        assert!(encode_transfer(&r).is_err());
        r.sources = vec![PathBuf::from(format!("C:\\{}", "x".repeat(1000))); MAX_SOURCES];
        assert!(encode_transfer(&r).is_err());
    }
}

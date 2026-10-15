//! Small shell adapter. Requests prepare drafts; no request starts copying.
pub mod instance;
pub mod registration;
use std::{
    ffi::OsString,
    io,
    os::windows::ffi::{OsStrExt, OsStringExt},
    path::PathBuf,
};
pub const MAX_SOURCES: usize = 512;
pub const MAX_REQUEST_BYTES: usize = 1024 * 1024;
pub const COPYDATA_MAGIC: usize = 0x56435231;
pub fn encode(paths: &[PathBuf]) -> io::Result<Vec<u8>> {
    validate(paths)?;
    let wire: Vec<Vec<u16>> = paths
        .iter()
        .map(|p| p.as_os_str().encode_wide().collect())
        .collect();
    let bytes = serde_json::to_vec(&wire)?;
    if bytes.len() > MAX_REQUEST_BYTES {
        return Err(io::Error::other("shell request exceeds 1 MiB"));
    }
    Ok(bytes)
}
pub fn decode(bytes: &[u8]) -> io::Result<Vec<PathBuf>> {
    if bytes.len() > MAX_REQUEST_BYTES {
        return Err(io::Error::other("shell request exceeds 1 MiB"));
    }
    let wire: Vec<Vec<u16>> = serde_json::from_slice(bytes)?;
    let paths = wire
        .iter()
        .map(|s| PathBuf::from(OsString::from_wide(s)))
        .collect::<Vec<_>>();
    validate(&paths)?;
    Ok(paths)
}
fn validate(paths: &[PathBuf]) -> io::Result<()> {
    if paths.len() > MAX_SOURCES
        || paths.iter().any(|p| {
            let units = p.as_os_str().encode_wide().collect::<Vec<_>>();
            !p.is_absolute() || units.contains(&0) || units.len() > 32767
        })
    {
        return Err(io::Error::other(
            "invalid shell selection (absolute paths, up to 512 roots)",
        ));
    }
    Ok(())
}
pub(crate) fn wide(text: impl AsRef<std::ffi::OsStr>) -> Vec<u16> {
    text.as_ref().encode_wide().chain(Some(0)).collect()
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn requests_preserve_native_utf16_and_reject_malformed_or_unbounded_input() {
        let mut path: Vec<u16> = r"D:\дані\".encode_utf16().collect();
        path.extend([0xD800, b'x' as u16]);
        let paths = vec![PathBuf::from(OsString::from_wide(&path))];
        assert_eq!(decode(&encode(&paths).unwrap()).unwrap(), paths);
        assert!(encode(&[PathBuf::from("relative")]).is_err());
        assert!(encode(&vec![paths[0].clone(); 513]).is_err());
        assert!(decode(&vec![b' '; MAX_REQUEST_BYTES + 1]).is_err());
        assert!(decode(br#"[[0,1]]"#).is_err());
        assert!(decode(b"[").is_err());
        assert!(encode(&[]).is_ok()); // An empty request only raises the existing window.
    }
}

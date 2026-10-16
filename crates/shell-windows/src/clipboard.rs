//! Read-only Shell clipboard capture. Capturing intent never acknowledges a move.
use crate::{
    MAX_REQUEST_BYTES, MAX_SOURCES,
    transfer::{MAX_PATH_UNITS, TransferOperation, validate_sources},
};
use std::{ffi::OsString, io, os::windows::ffi::OsStringExt, path::PathBuf};
use windows_sys::Win32::{
    Foundation::{GetLastError, HGLOBAL, SetLastError},
    System::{
        DataExchange::{
            CloseClipboard, GetClipboardData, IsClipboardFormatAvailable, OpenClipboard,
            RegisterClipboardFormatW,
        },
        Memory::{GlobalLock, GlobalSize, GlobalUnlock},
    },
    UI::Shell::{DROPFILES, DragQueryFileW, HDROP},
};

const CF_HDROP: u32 = 15;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ClipboardSnapshot {
    pub sources: Vec<PathBuf>,
    pub preferred_operation: TransferOperation,
}

/// Capture a bounded owned snapshot of CF_HDROP and Preferred DropEffect.
/// Does not clear the clipboard or send PerformedDropEffect/PasteSucceeded.
pub fn capture_clipboard() -> io::Result<ClipboardSnapshot> {
    let name = crate::wide("Preferred DropEffect");
    // SAFETY: A valid NUL-terminated format name; registration does not alter clipboard contents.
    let preferred_format = unsafe { RegisterClipboardFormatW(name.as_ptr()) };
    if preferred_format == 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: No owner window is needed for a read-only clipboard session.
    if unsafe { OpenClipboard(std::ptr::null_mut()) } == 0 {
        return Err(io::Error::last_os_error());
    }
    let guard = ClipboardGuard;
    let result = (|| {
        // SAFETY: Only checks format availability while this thread holds the clipboard open.
        if unsafe { IsClipboardFormatAvailable(CF_HDROP) } == 0 {
            return Err(invalid("clipboard has no CF_HDROP file selection"));
        }
        // SAFETY: The clipboard is open and ownership of its handle remains with Windows.
        let hdrop = unsafe { GetClipboardData(CF_HDROP) };
        if hdrop.is_null() {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: Reads only availability under the same open session.
        let preferred = if unsafe { IsClipboardFormatAvailable(preferred_format) } != 0 {
            // SAFETY: The clipboard is open; this handle is borrowed and never freed here.
            let handle = unsafe { GetClipboardData(preferred_format) };
            if handle.is_null() {
                return Err(io::Error::last_os_error());
            }
            // SAFETY: A clipboard HGLOBAL remains alive until this session closes.
            let bytes = unsafe { read_global(handle, 64) }?;
            if bytes.len() < 4 {
                return Err(invalid("truncated Preferred DropEffect DWORD"));
            }
            Some(u32::from_le_bytes(
                bytes[..4].try_into().expect("four bytes"),
            ))
        } else {
            None
        };
        // SAFETY: The borrowed CF_HDROP handle remains alive and immutable in this clipboard session.
        unsafe { snapshot_from_hdrop(hdrop, preferred) }
    })();
    let close = guard.close();
    match (result, close) {
        (Ok(snapshot), Ok(())) => Ok(snapshot),
        (Err(error), Ok(())) | (Ok(_), Err(error)) => Err(error),
        (Err(error), Err(close)) => Err(io::Error::other(format!(
            "{error}; CloseClipboard: {close}"
        ))),
    }
}

struct ClipboardGuard;
impl ClipboardGuard {
    fn close(self) -> io::Result<()> {
        // The API is attempted exactly once; Drop is only a panic-unwind fallback.
        std::mem::forget(self);
        // SAFETY: This guard represents the successful OpenClipboard on this thread.
        if unsafe { CloseClipboard() } == 0 {
            Err(io::Error::last_os_error())
        } else {
            Ok(())
        }
    }
}
impl Drop for ClipboardGuard {
    fn drop(&mut self) {
        // SAFETY: Panic-unwind fallback for this thread's successful OpenClipboard.
        if unsafe { CloseClipboard() } == 0 {
            eprintln!(
                "VartaCopy: CloseClipboard failed: {}",
                io::Error::last_os_error()
            );
        }
    }
}

/// Read an owned snapshot from a borrowed Unicode Shell HDROP and optional effect DWORD.
/// Only the single COPY (1) and MOVE (2) flags are supported; absence defaults to COPY.
///
/// # Safety
/// `hdrop` must be a live HGLOBAL containing CF_HDROP data, immutable for this call.
/// The caller retains ownership and must not release or replace the object during capture.
pub unsafe fn snapshot_from_hdrop(
    hdrop: HDROP,
    preferred_effect: Option<u32>,
) -> io::Result<ClipboardSnapshot> {
    let preferred_operation = operation_from_effect(preferred_effect)?;
    // SAFETY: The caller guarantees the HGLOBAL's validity and immutable lifetime.
    let bytes = unsafe { read_global(hdrop, MAX_REQUEST_BYTES) }?;
    let expected = parse_hdrop(&bytes)?;
    // SAFETY: The complete bounded DROPFILES layout and double-NUL list were validated above.
    let count = unsafe { DragQueryFileW(hdrop, u32::MAX, std::ptr::null_mut(), 0) } as usize;
    if count != expected.len() {
        return Err(invalid("HDROP count disagrees with validated Unicode list"));
    }
    let mut sources = Vec::with_capacity(count);
    for (index, expected_units) in expected.iter().enumerate() {
        // SAFETY: index is within the validated file list; null output queries required length.
        let length =
            unsafe { DragQueryFileW(hdrop, index as u32, std::ptr::null_mut(), 0) } as usize;
        if length == 0 || length > MAX_PATH_UNITS || length != expected_units.len() {
            return Err(invalid("invalid native HDROP filename length"));
        }
        let mut buffer = vec![0; length + 1];
        // SAFETY: The buffer has the queried length plus NUL; index and object remain valid.
        let written = unsafe {
            DragQueryFileW(
                hdrop,
                index as u32,
                buffer.as_mut_ptr(),
                buffer.len() as u32,
            )
        } as usize;
        if written != length || buffer[length] != 0 || buffer[..length] != **expected_units {
            return Err(invalid("HDROP filename changed or was truncated"));
        }
        sources.push(PathBuf::from(OsString::from_wide(&buffer[..length])));
    }
    validate_sources(&sources)?;
    Ok(ClipboardSnapshot {
        sources,
        preferred_operation,
    })
}

fn operation_from_effect(effect: Option<u32>) -> io::Result<TransferOperation> {
    match effect {
        None | Some(1) => Ok(TransferOperation::Copy),
        Some(2) => Ok(TransferOperation::Move),
        Some(_) => Err(invalid("unsupported or ambiguous Preferred DropEffect")),
    }
}

unsafe fn read_global(handle: HGLOBAL, limit: usize) -> io::Result<Vec<u8>> {
    // SAFETY: The caller guarantees a live immutable global memory handle.
    let size = unsafe { GlobalSize(handle) };
    if size == 0 || size > limit {
        return Err(invalid("empty or oversized Shell global memory object"));
    }
    // SAFETY: The caller guarantees the live HGLOBAL; locking gives access to its declared size.
    let pointer = unsafe { GlobalLock(handle) };
    if pointer.is_null() {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: GlobalLock succeeded and GlobalSize bounded the readable bytes; copy while locked.
    let bytes = unsafe { std::slice::from_raw_parts(pointer.cast::<u8>(), size) }.to_vec();
    // SAFETY: This function owns exactly one successful lock of the still-live object.
    unsafe { unlock_global(handle) }?;
    Ok(bytes)
}

unsafe fn unlock_global(handle: HGLOBAL) -> io::Result<()> {
    // SAFETY: Resetting last error distinguishes a successful final unlock (returns zero) from failure.
    unsafe { SetLastError(0) };
    // SAFETY: This function owns exactly one successful lock of the still-live object.
    if unsafe { GlobalUnlock(handle) } == 0 {
        // SAFETY: Reads only the current thread's error after GlobalUnlock.
        let error = unsafe { GetLastError() };
        if error != 0 {
            return Err(io::Error::from_raw_os_error(error as i32));
        }
    }
    Ok(())
}

fn parse_hdrop(bytes: &[u8]) -> io::Result<Vec<Vec<u16>>> {
    if bytes.len() < std::mem::size_of::<DROPFILES>() {
        return Err(invalid("truncated DROPFILES header"));
    }
    // SAFETY: The bounded byte buffer contains a full DROPFILES; unaligned input is explicitly supported.
    let header = unsafe { std::ptr::read_unaligned(bytes.as_ptr().cast::<DROPFILES>()) };
    let offset = header.pFiles as usize;
    if header.fWide == 0
        || offset < std::mem::size_of::<DROPFILES>()
        || offset > bytes.len()
        || !offset.is_multiple_of(2)
    {
        return Err(invalid("HDROP requires a valid Unicode DROPFILES offset"));
    }
    let payload = &bytes[offset..];
    if !payload.len().is_multiple_of(2) {
        return Err(invalid("unaligned Unicode HDROP payload"));
    }
    let units = payload
        .chunks_exact(2)
        .map(|pair| u16::from_le_bytes([pair[0], pair[1]]))
        .collect::<Vec<_>>();
    let mut paths = Vec::new();
    let mut start = 0;
    for (index, &unit) in units.iter().enumerate() {
        if unit != 0 {
            if index - start >= MAX_PATH_UNITS {
                return Err(invalid("HDROP filename exceeds native path bound"));
            }
            continue;
        }
        if index == start {
            // The double NUL terminates the list; GlobalSize can include allocation padding.
            if paths.is_empty() {
                return Err(invalid("empty or malformed HDROP selection"));
            }
            return Ok(paths);
        }
        if paths.len() == MAX_SOURCES {
            return Err(invalid("HDROP selection exceeds 512 sources"));
        }
        paths.push(units[start..index].to_vec());
        start = index + 1;
    }
    Err(invalid("HDROP list is missing its double NUL terminator"))
}

fn invalid(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}

#[cfg(test)]
mod tests {
    use super::*;
    use windows_sys::Win32::{
        Foundation::GlobalFree,
        System::Memory::{GMEM_MOVEABLE, GMEM_ZEROINIT, GlobalAlloc},
    };
    struct TestGlobal(HGLOBAL);
    impl TestGlobal {
        fn new(bytes: &[u8]) -> Self {
            // SAFETY: A nonzero bounded allocation, owned by this synthetic test fixture.
            let handle = unsafe { GlobalAlloc(GMEM_MOVEABLE | GMEM_ZEROINIT, bytes.len()) };
            assert!(!handle.is_null());
            // SAFETY: The newly allocated valid global object is owned by this fixture.
            let pointer = unsafe { GlobalLock(handle) };
            assert!(!pointer.is_null());
            // SAFETY: The allocation has bytes.len() writable bytes and the source is disjoint.
            unsafe {
                std::ptr::copy_nonoverlapping(bytes.as_ptr(), pointer.cast::<u8>(), bytes.len());
            }
            // SAFETY: The fixture owns exactly the successful lock taken above.
            unsafe { unlock_global(handle) }.unwrap();
            Self(handle)
        }
    }
    impl Drop for TestGlobal {
        fn drop(&mut self) {
            // SAFETY: The fixture exclusively owns the unlocked allocation.
            assert!(unsafe { GlobalFree(self.0) }.is_null());
        }
    }
    fn hdrop(paths: &[Vec<u16>]) -> Vec<u8> {
        let mut bytes = vec![0; std::mem::size_of::<DROPFILES>()];
        let offset = bytes.len() as u32;
        bytes[..4].copy_from_slice(&offset.to_le_bytes());
        bytes[16..20].copy_from_slice(&1i32.to_le_bytes());
        for path in paths {
            for unit in path.iter().copied().chain(Some(0)) {
                bytes.extend(unit.to_le_bytes());
            }
        }
        bytes.extend(0u16.to_le_bytes());
        bytes
    }
    #[test]
    fn synthetic_global_hdrop_is_lossless_and_read_only() {
        let mut path = r"C:\дані\".encode_utf16().collect::<Vec<_>>();
        path.extend([0xd800, 0xdfff]);
        let bytes = hdrop(&[path.clone()]);
        let global = TestGlobal::new(&bytes);
        for (effect, operation) in [
            (None, TransferOperation::Copy),
            (Some(1), TransferOperation::Copy),
            (Some(2), TransferOperation::Move),
        ] {
            // SAFETY: This fixture owns the complete immutable synthetic Unicode HDROP.
            let snapshot = unsafe { snapshot_from_hdrop(global.0, effect) }.unwrap();
            assert_eq!(
                snapshot.sources,
                vec![PathBuf::from(OsString::from_wide(&path))]
            );
            assert_eq!(snapshot.preferred_operation, operation);
        }
        // SAFETY: Fixture remains alive; this only verifies the adapter did not alter its memory.
        let captured_bytes = unsafe { read_global(global.0, MAX_REQUEST_BYTES) }.unwrap();
        assert_eq!(&captured_bytes[..bytes.len()], &bytes);
    }
    #[test]
    fn unsupported_and_ambiguous_effects_fail_closed() {
        for effect in [0, 3, 4, 5, 6, 7, 0x80000000, u32::MAX] {
            assert!(operation_from_effect(Some(effect)).is_err());
        }
    }
    #[test]
    fn malformed_oversize_and_unsafe_sources_are_rejected() {
        for bytes in [
            vec![],
            vec![0; 19],
            hdrop(&[]),
            hdrop(&[vec![65; MAX_PATH_UNITS + 1]]),
            hdrop(&vec![r"C:\file".encode_utf16().collect(); MAX_SOURCES + 1]),
        ] {
            assert!(parse_hdrop(&bytes).is_err());
        }
        let valid = hdrop(&[r"C:\file".encode_utf16().collect()]);
        let mut truncated = valid.clone();
        truncated.truncate(truncated.len() - 2);
        assert!(parse_hdrop(&truncated).is_err());
        let mut ansi = valid.clone();
        ansi[16..20].fill(0);
        assert!(parse_hdrop(&ansi).is_err());
        let mut offset = valid.clone();
        offset[..4].copy_from_slice(&u32::MAX.to_le_bytes());
        assert!(parse_hdrop(&offset).is_err());
        for path in [r"C:\", r"C:\a\..\b", r"\\server\share\file"] {
            let global = TestGlobal::new(&hdrop(&[path.encode_utf16().collect()]));
            // SAFETY: Complete fixture HDROP; only the path syntax is deliberately invalid.
            assert!(unsafe { snapshot_from_hdrop(global.0, None) }.is_err());
        }
        let oversized = TestGlobal::new(&vec![0; MAX_REQUEST_BYTES + 1]);
        // SAFETY: A valid allocation deliberately exceeding the adapter's read bound.
        assert!(unsafe { snapshot_from_hdrop(oversized.0, None) }.is_err());
    }
}

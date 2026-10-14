//! Windows file primitives. Publication is by handle and never replaces a name.
#![cfg(windows)]

#[cfg(test)]
mod publication_tests {
    use super::*;
    use std::io::Write;
    #[test]
    fn rename_buffer_terminates_every_utf16_alignment() {
        let root = tempfile::tempdir().unwrap();
        let canonical = root.path().canonicalize().unwrap();
        for length in 1..=16 {
            let name = format!("{}-дані.bin", "x".repeat(length));
            let target = canonical.join(name);
            let mut stage = Stage::create(&canonical).unwrap();
            stage.file().write_all(b"payload").unwrap();
            stage.file().sync_all().unwrap();
            stage.publish(&target).unwrap();
            drop(stage);
            assert_eq!(
                std::fs::read(&target).unwrap(),
                b"payload",
                "{}",
                target.display()
            );
        }
        assert_eq!(std::fs::read_dir(&canonical).unwrap().count(), 16);
    }
}

use std::ffi::{OsStr, OsString};
use std::fs::{File, OpenOptions};
use std::io;
use std::mem::{offset_of, size_of};
use std::os::windows::{ffi::OsStrExt, fs::OpenOptionsExt, io::AsRawHandle};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use windows_sys::Win32::Foundation::{ERROR_HANDLE_EOF, HANDLE, INVALID_HANDLE_VALUE};
use windows_sys::Win32::Storage::FileSystem::*;

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Identity {
    pub volume_serial: u64,
    pub file_id: [u8; 16],
}

/// Display is lossy by design; native units preserve every Windows path code unit.
#[derive(Debug, Clone, Serialize)]
pub struct PathRecord {
    pub display: String,
    pub windows_utf16: Vec<u16>,
}

impl From<&Path> for PathRecord {
    fn from(path: &Path) -> Self {
        Self {
            display: path.display().to_string(),
            windows_utf16: path.as_os_str().encode_wide().collect(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Stream {
    /// Empty for the unnamed data stream, otherwise the full :name:$DATA suffix.
    #[serde(with = "native_os")]
    pub name: OsString,
    pub size: u64,
}

mod native_os {
    use super::*;
    use serde::{Deserializer, Serializer};
    use std::os::windows::ffi::OsStringExt;
    pub fn serialize<S: Serializer>(value: &OsStr, serializer: S) -> Result<S::Ok, S::Error> {
        value
            .encode_wide()
            .collect::<Vec<u16>>()
            .serialize(serializer)
    }
    pub fn deserialize<'de, D: Deserializer<'de>>(deserializer: D) -> Result<OsString, D::Error> {
        let units = Vec::<u16>::deserialize(deserializer)?;
        if units.len() > 32767 || units.contains(&0) {
            return Err(serde::de::Error::custom("invalid native string"));
        }
        Ok(OsString::from_wide(&units))
    }
}

/// Metadata-only handle pins an object name without denying SQLite/data writes.
pub fn guard_file(path: &Path) -> io::Result<File> {
    let file = OpenOptions::new()
        .access_mode(FILE_READ_ATTRIBUTES)
        .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE)
        .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT)
        .open(path)?;
    if is_reparse(&file.metadata()?) || !file.metadata()?.is_file() {
        return Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "ordinary file required",
        ));
    }
    Ok(file)
}

pub fn exclusive_lock(path: &Path) -> io::Result<File> {
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .share_mode(0)
        .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT)
        .open(path)?;
    if is_reparse(&file.metadata()?) || !file.metadata()?.is_file() {
        return Err(io::Error::other("lock must be an ordinary file"));
    }
    Ok(file)
}

pub fn link_count(file: &File) -> io::Result<u32> {
    // SAFETY: this output struct contains only integers and integer structs.
    let mut info: BY_HANDLE_FILE_INFORMATION = unsafe { std::mem::zeroed() };
    // SAFETY: file and correctly sized output pointer remain valid.
    if unsafe { GetFileInformationByHandle(file.as_raw_handle() as HANDLE, &mut info) } == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(info.nNumberOfLinks)
}

/// SQLite WAL storage is restricted to a local fixed volume.
pub fn require_local_fixed_volume(path: &Path) -> io::Result<()> {
    let name = wide(path.as_os_str())?;
    let mut root = vec![0u16; 32768];
    // SAFETY: both buffers are valid and root has the advertised capacity.
    if unsafe { GetVolumePathNameW(name.as_ptr(), root.as_mut_ptr(), root.len() as u32) } == 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: GetVolumePathNameW wrote a NUL-terminated volume root.
    if unsafe { GetDriveTypeW(root.as_ptr()) } != 3 {
        // DRIVE_FIXED
        return Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "journal requires a local fixed volume",
        ));
    }
    Ok(())
}

fn wide(value: &OsStr) -> io::Result<Vec<u16>> {
    let mut units: Vec<u16> = value.encode_wide().collect();
    if units.contains(&0) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "NUL in Windows path",
        ));
    }
    units.push(0);
    Ok(units)
}

pub fn is_reparse(metadata: &std::fs::Metadata) -> bool {
    use std::os::windows::fs::MetadataExt;
    metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
}

pub fn open_source(path: &Path) -> io::Result<File> {
    let file = OpenOptions::new()
        .read(true)
        .share_mode(FILE_SHARE_READ)
        .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT | FILE_FLAG_SEQUENTIAL_SCAN)
        .open(path)?;
    if is_reparse(&file.metadata()?) {
        return Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "reparse point refused",
        ));
    }
    Ok(file)
}

/// Open an existing directory without following its final reparse point.
/// Denying DELETE sharing pins its name while descendants are accessed.
pub fn open_directory(path: &Path, write_attributes: bool) -> io::Result<File> {
    let access = FILE_READ_ATTRIBUTES
        | if write_attributes {
            FILE_WRITE_ATTRIBUTES
        } else {
            0
        };
    let file = OpenOptions::new()
        .access_mode(access)
        .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT)
        .open(path)?;
    let metadata = file.metadata()?;
    if is_reparse(&metadata) || !metadata.is_dir() {
        return Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "expected an ordinary directory; reparse points are refused",
        ));
    }
    Ok(file)
}

/// Keep each canonical ancestor from being renamed while a transfer uses it.
pub fn guard_parents(file: &Path) -> io::Result<Vec<File>> {
    let parent = file
        .parent()
        .ok_or_else(|| io::Error::other("path has no parent"))?;
    parent
        .ancestors()
        .map(|directory| {
            let guard = OpenOptions::new()
                .access_mode(FILE_READ_ATTRIBUTES)
                .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE)
                .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT)
                .open(directory)?;
            if is_reparse(&guard.metadata()?) {
                return Err(io::Error::new(
                    io::ErrorKind::Unsupported,
                    "reparse parent refused",
                ));
            }
            Ok(guard)
        })
        .collect()
}

pub fn identity(file: &File) -> io::Result<Identity> {
    // SAFETY: this Windows output struct contains only integer fields.
    let mut info: FILE_ID_INFO = unsafe { std::mem::zeroed() };
    // SAFETY: info is a writable FILE_ID_INFO of the exact advertised size.
    let ok = unsafe {
        GetFileInformationByHandleEx(
            file.as_raw_handle() as HANDLE,
            FileIdInfo,
            (&mut info as *mut FILE_ID_INFO).cast(),
            size_of::<FILE_ID_INFO>() as u32,
        )
    };
    if ok == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(Identity {
        volume_serial: info.VolumeSerialNumber,
        file_id: info.FileId.Identifier,
    })
}

pub fn stream_path(base: &Path, suffix: &OsStr) -> PathBuf {
    let mut result = base.as_os_str().to_os_string();
    result.push(suffix);
    PathBuf::from(result)
}

pub fn streams(path: &Path) -> io::Result<Vec<Stream>> {
    let name = wide(path.as_os_str())?;
    // SAFETY: this Windows output struct contains only integers and arrays.
    let mut data: WIN32_FIND_STREAM_DATA = unsafe { std::mem::zeroed() };
    // SAFETY: name is NUL-terminated; data is a correctly sized writable buffer.
    let handle = unsafe {
        FindFirstStreamW(
            name.as_ptr(),
            FindStreamInfoStandard,
            (&mut data as *mut WIN32_FIND_STREAM_DATA).cast(),
            0,
        )
    };
    if handle == INVALID_HANDLE_VALUE {
        let error = io::Error::last_os_error();
        // Directories have no unnamed $DATA stream. EOF is an empty manifest,
        // whereas an unsupported filesystem remains an explicit error.
        if error.raw_os_error() == Some(ERROR_HANDLE_EOF as i32) {
            return Ok(Vec::new());
        }
        return Err(error);
    }
    let result = (|| {
        let mut output = Vec::new();
        loop {
            let len = data
                .cStreamName
                .iter()
                .position(|x| *x == 0)
                .ok_or_else(|| io::Error::other("unterminated stream name"))?;
            use std::os::windows::ffi::OsStringExt;
            let mut name = OsString::from_wide(&data.cStreamName[..len]);
            if name == "::$DATA" {
                name.clear();
            }
            if data.StreamSize < 0 {
                return Err(io::Error::other("negative stream size"));
            }
            output.push(Stream {
                name,
                size: data.StreamSize as u64,
            });
            if output.len() > 256 {
                return Err(io::Error::other("more than 256 streams: refused"));
            }
            // SAFETY: handle remains valid and data has the required layout.
            if unsafe { FindNextStreamW(handle, (&mut data as *mut WIN32_FIND_STREAM_DATA).cast()) }
                == 0
            {
                let error = io::Error::last_os_error();
                if error.raw_os_error() != Some(ERROR_HANDLE_EOF as i32) {
                    return Err(error);
                }
                break;
            }
        }
        output.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(output)
    })();
    // SAFETY: handle came from FindFirstStreamW and is closed exactly once.
    if unsafe { FindClose(handle) } == 0 {
        return Err(io::Error::last_os_error());
    }
    result
}

pub struct Stage {
    file: File,
    path: PathBuf,
    published: bool,
    delete_requested: bool,
    cleanup_on_drop: bool,
}

impl Stage {
    pub fn create(parent: &Path) -> io::Result<Self> {
        let temporary = tempfile::Builder::new()
            .prefix(".vartacopy-")
            .suffix(".part")
            .make_in(parent, |path| {
                OpenOptions::new()
                    .create_new(true)
                    .read(true)
                    .write(true)
                    .access_mode(0x8000_0000 | 0x4000_0000 | 0x0001_0000) // READ | WRITE | DELETE
                    .share_mode(FILE_SHARE_READ)
                    .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT)
                    .open(path)
            })?;
        let (file, path) = temporary.into_parts();
        let path = path.keep().map_err(|e| e.error)?;
        Ok(Self {
            file,
            path,
            published: false,
            delete_requested: false,
            cleanup_on_drop: true,
        })
    }

    pub fn file(&mut self) -> &mut File {
        &mut self.file
    }

    /// Adopt only an exact recorded staging identity. Never clean a mismatching file.
    pub fn reopen(path: &Path, expected: &Identity) -> io::Result<Self> {
        // Clear readonly only on the exact owned object, then recheck identity
        // after reopening for data writes. No write is made on a mismatching handle.
        let attributes = OpenOptions::new()
            .access_mode(FILE_READ_ATTRIBUTES | FILE_WRITE_ATTRIBUTES)
            .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE)
            .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT)
            .open(path)?;
        if is_reparse(&attributes.metadata()?)
            || identity(&attributes)? != *expected
            || link_count(&attributes)? != 1
        {
            return Err(io::Error::other(
                "staging identity mismatch; attributes unchanged",
            ));
        }
        use std::os::windows::fs::MetadataExt;
        if attributes.metadata()?.file_attributes() & FILE_ATTRIBUTE_READONLY != 0 {
            // SAFETY: integer-only struct; zero timestamps leave them unchanged.
            let mut basic: FILE_BASIC_INFO = unsafe { std::mem::zeroed() };
            basic.FileAttributes = FILE_ATTRIBUTE_NORMAL;
            // SAFETY: writable-attributes owned handle and exact output layout.
            if unsafe {
                SetFileInformationByHandle(
                    attributes.as_raw_handle() as HANDLE,
                    FileBasicInfo,
                    (&basic as *const FILE_BASIC_INFO).cast(),
                    size_of::<FILE_BASIC_INFO>() as u32,
                )
            } == 0
            {
                return Err(io::Error::last_os_error());
            }
        }
        drop(attributes);
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .access_mode(0x8000_0000 | 0x4000_0000 | 0x0001_0000)
            .share_mode(FILE_SHARE_READ)
            .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT)
            .open(path)?;
        if is_reparse(&file.metadata()?) || identity(&file)? != *expected || link_count(&file)? != 1
        {
            return Err(io::Error::other(
                "staging identity mismatch; file was not adopted",
            ));
        }
        Ok(Self {
            file,
            path: path.to_path_buf(),
            published: false,
            delete_requested: false,
            cleanup_on_drop: false,
        })
    }

    /// Once ownership is journaled, retain the stage for explicit recovery.
    pub fn preserve(&mut self) {
        self.cleanup_on_drop = false;
    }
    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn create_stream(&self, name: &OsStr) -> io::Result<File> {
        OpenOptions::new()
            .create_new(true)
            .read(true)
            .write(true)
            .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE)
            .open(stream_path(&self.path, name))
    }

    pub fn open_stream(&self, name: &OsStr) -> io::Result<File> {
        OpenOptions::new()
            .read(true)
            .write(true)
            .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE)
            .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT)
            .open(stream_path(&self.path, name))
    }

    pub fn publish(&mut self, destination: &Path) -> io::Result<()> {
        let target = wide(destination.as_os_str())?;
        let bytes = (target.len() - 1) * size_of::<u16>();
        // Win32 requires a terminating NUL even though FileNameLength excludes it.
        // Include it explicitly: alignment padding is not guaranteed to provide one.
        let total = offset_of!(FILE_RENAME_INFO, FileName) + target.len() * size_of::<u16>();
        let total_u32 =
            u32::try_from(total).map_err(|_| io::Error::other("rename buffer too large"))?;
        let mut storage = vec![0u64; total.max(size_of::<FILE_RENAME_INFO>()).div_ceil(8)];
        // SAFETY: storage is sufficiently large and aligned for FILE_RENAME_INFO;
        // FileName is the variable trailing array. No replace flag is set.
        let ok = unsafe {
            let info = storage.as_mut_ptr().cast::<FILE_RENAME_INFO>();
            (*info).Anonymous.ReplaceIfExists = false;
            (*info).RootDirectory = std::ptr::null_mut();
            (*info).FileNameLength = bytes as u32;
            std::ptr::copy_nonoverlapping(
                target.as_ptr(),
                (*info).FileName.as_mut_ptr(),
                target.len(),
            );
            SetFileInformationByHandle(
                self.file.as_raw_handle() as HANDLE,
                FileRenameInfo,
                info.cast(),
                total_u32,
            )
        };
        if ok == 0 {
            return Err(io::Error::last_os_error());
        }
        self.path = destination.to_path_buf();
        self.published = true;
        self.file.sync_all()
    }

    pub fn discard(&mut self) -> io::Result<()> {
        if self.published || self.delete_requested {
            return Ok(());
        }
        // SAFETY: zero leaves timestamps unchanged; NORMAL clears READONLY on our stage.
        let mut basic: FILE_BASIC_INFO = unsafe { std::mem::zeroed() };
        basic.FileAttributes = FILE_ATTRIBUTE_NORMAL;
        // SAFETY: the writable owned handle and buffer are valid for this call.
        if unsafe {
            SetFileInformationByHandle(
                self.file.as_raw_handle() as HANDLE,
                FileBasicInfo,
                (&basic as *const FILE_BASIC_INFO).cast(),
                size_of::<FILE_BASIC_INFO>() as u32,
            )
        } == 0
        {
            return Err(io::Error::last_os_error());
        }
        let info = FILE_DISPOSITION_INFO { DeleteFile: true };
        // SAFETY: info is valid; the handle was opened with DELETE access.
        if unsafe {
            SetFileInformationByHandle(
                self.file.as_raw_handle() as HANDLE,
                FileDispositionInfo,
                (&info as *const FILE_DISPOSITION_INFO).cast(),
                size_of::<FILE_DISPOSITION_INFO>() as u32,
            )
        } == 0
        {
            return Err(io::Error::last_os_error());
        }
        self.delete_requested = true;
        Ok(())
    }
}

impl Drop for Stage {
    fn drop(&mut self) {
        if !self.cleanup_on_drop {
            return;
        }
        if let Err(error) = self.discard() {
            eprintln!(
                "vartacopy: staging cleanup failed for {}: {error}",
                self.path.display()
            );
        }
    }
}

pub fn copy_basic_metadata(source: &File, destination: &File) -> io::Result<()> {
    set_basic_metadata(source, destination)?;
    destination.sync_all()
}

/// Apply directory times/flags after children. This makes no directory-fsync
/// or power-loss durability claim; it does not flush a directory handle.
pub fn copy_directory_metadata(source: &File, destination: &File) -> io::Result<()> {
    set_basic_metadata(source, destination)
}

fn set_basic_metadata(source: &File, destination: &File) -> io::Result<()> {
    // SAFETY: this Windows output struct contains only integer fields.
    let mut info: FILE_BASIC_INFO = unsafe { std::mem::zeroed() };
    // SAFETY: info is a writable buffer with the correct layout and size.
    if unsafe {
        GetFileInformationByHandleEx(
            source.as_raw_handle() as HANDLE,
            FileBasicInfo,
            (&mut info as *mut FILE_BASIC_INFO).cast(),
            size_of::<FILE_BASIC_INFO>() as u32,
        )
    } == 0
    {
        return Err(io::Error::last_os_error());
    }
    info.ChangeTime = 0;
    info.FileAttributes &= FILE_ATTRIBUTE_READONLY
        | FILE_ATTRIBUTE_HIDDEN
        | FILE_ATTRIBUTE_SYSTEM
        | FILE_ATTRIBUTE_ARCHIVE;
    if info.FileAttributes == 0 {
        info.FileAttributes = FILE_ATTRIBUTE_NORMAL;
    }
    // SAFETY: destination is open for writing; non-copyable attribute flags are masked.
    if unsafe {
        SetFileInformationByHandle(
            destination.as_raw_handle() as HANDLE,
            FileBasicInfo,
            (&info as *const FILE_BASIC_INFO).cast(),
            size_of::<FILE_BASIC_INFO>() as u32,
        )
    } == 0
    {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

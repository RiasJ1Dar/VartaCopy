use std::{ffi::OsString, os::windows::ffi::OsStringExt};
use windows::{
    Win32::{
        Foundation::{E_INVALIDARG, E_POINTER},
        Storage::FileSystem::{
            FILE_ATTRIBUTE_DIRECTORY, FILE_ATTRIBUTE_OFFLINE, FILE_ATTRIBUTE_REPARSE_POINT,
            GetDriveTypeW, GetFileAttributesW, INVALID_FILE_ATTRIBUTES,
        },
        System::Com::CoTaskMemFree,
        UI::Shell::{IShellItemArray, SIGDN_FILESYSPATH},
    },
    core::{HRESULT, PCWSTR, PWSTR},
};

struct TaskString(PWSTR);

impl Drop for TaskString {
    fn drop(&mut self) {
        // SAFETY: GetDisplayName transfers a CoTaskMem allocation to the caller.
        unsafe { CoTaskMemFree(Some(self.0.0.cast())) };
    }
}

/// Conservative DOS/extended-DOS syntax only; never accepts UNC, device paths,
/// drive roots, ADS, dot components or ambiguous separators. Preserve all UTF-16.
pub(crate) fn local_components(path: &[u16]) -> Result<Vec<usize>, HRESULT> {
    let start = if path.starts_with(&[92, 92, 63, 92]) {
        4
    } else {
        0
    };
    let path_part = path.get(start..).ok_or(E_INVALIDARG)?;
    if path_part.len() < 4
        || !matches!(path_part[0], 65..=90 | 97..=122)
        || path_part[1] != 58
        || path_part[2] != 92
    {
        return Err(E_INVALIDARG);
    }
    let mut ends = Vec::new();
    let mut component_start = start + 3;
    for end in (component_start..=path.len()).filter(|&i| i == path.len() || path[i] == 92) {
        let part = &path[component_start..end];
        if part.is_empty()
            || part == [46]
            || part == [46, 46]
            || part
                .iter()
                .any(|c| matches!(c, 0..=31 | 34 | 42 | 47 | 58 | 60 | 62 | 63 | 124))
            || matches!(part.last(), Some(32 | 46))
        {
            return Err(E_INVALIDARG);
        }
        ends.push(end);
        component_start = end + 1;
    }
    Ok(ends)
}

pub(crate) fn ordinary_local_folder(path: &[u16]) -> Result<(), HRESULT> {
    let ends = local_components(path)?;
    let start = if path.starts_with(&[92, 92, 63, 92]) {
        4
    } else {
        0
    };
    let root = [path[start], 58, 92, 0];
    // SAFETY: root is a terminated local drive path. Reject remote/unknown drives
    // before any file metadata query. SDK DRIVE_REMOVABLE/FIXED/RAMDISK = 2/3/6.
    if !matches!(unsafe { GetDriveTypeW(PCWSTR(root.as_ptr())) }, 2 | 3 | 6) {
        return Err(E_INVALIDARG);
    }
    // Walk only ancestors, top-down, so a junction/symlink is rejected before
    // querying a descendant through it. Never enumerate a directory or read data.
    for end in ends {
        let mut prefix = path[..end].to_vec();
        prefix.push(0);
        // SAFETY: prefix is a terminated local path with validated syntax.
        let attrs = unsafe { GetFileAttributesW(PCWSTR(prefix.as_ptr())) };
        if attrs == INVALID_FILE_ATTRIBUTES {
            return Err(windows::core::Error::from_thread().code());
        }
        if attrs & FILE_ATTRIBUTE_DIRECTORY.0 == 0
            || attrs & (FILE_ATTRIBUTE_REPARSE_POINT.0 | FILE_ATTRIBUTE_OFFLINE.0) != 0
        {
            return Err(E_INVALIDARG);
        }
    }
    Ok(())
}

pub(crate) fn selection_path(items: &IShellItemArray) -> Result<OsString, HRESULT> {
    // SAFETY: the borrowed Shell array is live for the active COM callback.
    if unsafe { items.GetCount() }.map_err(|e| e.code())? != 1 {
        return Err(E_INVALIDARG);
    }
    // SAFETY: index 0 is valid for the verified single-item array.
    let item = unsafe { items.GetItemAt(0) }.map_err(|e| e.code())?;
    // SAFETY: SIGDN_FILESYSPATH requests the filesystem path; virtual items fail.
    let name = TaskString(unsafe { item.GetDisplayName(SIGDN_FILESYSPATH) }.map_err(|e| e.code())?);
    if name.0.is_null() {
        return Err(E_POINTER);
    }
    // SAFETY: successful GetDisplayName returns a terminated UTF-16 allocation.
    let wide = unsafe { name.0.as_wide() };
    ordinary_local_folder(wide)?;
    Ok(OsString::from_wide(wide))
}

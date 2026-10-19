//! Acquire owned native paths while the caller's data object and PIDL are valid.
use std::{
    ffi::OsString,
    os::windows::ffi::{OsStrExt, OsStringExt},
    path::PathBuf,
};
use vartacopy_shell_windows::{
    MAX_REQUEST_BYTES, MAX_SOURCES,
    transfer::{
        MAX_PATH_UNITS, TransferOperation, TransferOrigin, TransferRequest, encode_transfer,
        new_transfer_id,
    },
};
use windows::{
    Win32::{
        Foundation::E_INVALIDARG,
        System::Com::{
            CoTaskMemFree, DVASPECT_CONTENT, FORMATETC, IDataObject, STGMEDIUM, TYMED_HGLOBAL,
        },
        System::Ole::ReleaseStgMedium,
        UI::Shell::{
            Common::ITEMIDLIST, DragQueryFileW, HDROP, IShellItem, IShellItemArray,
            SHCreateItemFromIDList, SHCreateShellItemArrayFromDataObject, SIGDN_FILESYSPATH,
        },
    },
    core::{PWSTR, Result},
};

struct Name(PWSTR);
impl Drop for Name {
    fn drop(&mut self) {
        // SAFETY: GetDisplayName transfers this COM allocation to the caller.
        unsafe { CoTaskMemFree(Some(self.0.0.cast())) };
    }
}
fn path(item: &IShellItem, budget: &mut usize) -> Result<PathBuf> {
    // SAFETY: The shell item is a live interface on the caller's apartment.
    let name = Name(unsafe { item.GetDisplayName(SIGDN_FILESYSPATH)? });
    // SAFETY: GetDisplayName guarantees a valid NUL-terminated allocation.
    let units = unsafe { name.0.as_wide() };
    account(units.len(), budget)?;
    Ok(PathBuf::from(OsString::from_wide(units)))
}
fn account(count: usize, budget: &mut usize) -> Result<()> {
    *budget = budget.checked_add(count).ok_or(E_INVALIDARG)?;
    if count == 0 || count > MAX_PATH_UNITS || *budget > MAX_REQUEST_BYTES / 2 {
        return Err(E_INVALIDARG.into());
    }
    Ok(())
}
struct Medium(STGMEDIUM);
impl Drop for Medium {
    fn drop(&mut self) {
        // SAFETY: IDataObject::GetData returned ownership of this storage medium.
        unsafe { ReleaseStgMedium(&mut self.0) };
    }
}
fn hdrop(data: &IDataObject, budget: &mut usize) -> Result<Vec<PathBuf>> {
    let format = FORMATETC {
        cfFormat: 15,
        ptd: std::ptr::null_mut(),
        dwAspect: DVASPECT_CONTENT.0,
        lindex: -1,
        tymed: TYMED_HGLOBAL.0 as u32,
    };
    // SAFETY: The format descriptor is initialized; GetData returns owned storage.
    let medium = Medium(unsafe { data.GetData(&format)? });
    if medium.0.tymed != TYMED_HGLOBAL.0 as u32 {
        return Err(E_INVALIDARG.into());
    }
    // SAFETY: The tag was checked; CF_HDROP defines the HGLOBAL layout for DragQueryFileW.
    let handle = HDROP(unsafe { medium.0.u.hGlobal }.0);
    // SAFETY: The storage medium remains owned until every query completes.
    let count = unsafe { DragQueryFileW(handle, u32::MAX, None) };
    if count == 0 || count as usize > MAX_SOURCES {
        return Err(E_INVALIDARG.into());
    }
    let mut paths = Vec::with_capacity(count as usize);
    for i in 0..count {
        // SAFETY: Query the length of one valid entry in the owned drop structure.
        let len = unsafe { DragQueryFileW(handle, i, None) } as usize;
        account(len, budget)?;
        let mut units = vec![0; len + 1];
        // SAFETY: The allocated buffer includes space for the terminator.
        if unsafe { DragQueryFileW(handle, i, Some(&mut units)) } as usize != len {
            return Err(E_INVALIDARG.into());
        }
        paths.push(PathBuf::from(OsString::from_wide(&units[..len])));
    }
    Ok(paths)
}
/// # Safety
/// `folder` must be the caller-owned absolute PIDL supplied to Initialize.
pub unsafe fn destination(folder: *const ITEMIDLIST) -> Result<PathBuf> {
    if folder.is_null() {
        return Err(E_INVALIDARG.into());
    }
    let mut budget = 0;
    // SAFETY: The caller guarantees the PIDL is valid for this callback.
    let target: IShellItem = unsafe { SHCreateItemFromIDList(folder)? };
    path(&target, &mut budget)
}
pub fn capture(destination: PathBuf, data: &IDataObject) -> Result<TransferRequest> {
    let mut budget = 0;
    account(destination.as_os_str().encode_wide().count(), &mut budget)?;
    // SAFETY: The caller lends a valid IDataObject on this same apartment.
    let sources = match unsafe { SHCreateShellItemArrayFromDataObject::<_, IShellItemArray>(data) }
    {
        Ok(array) => {
            // SAFETY: This is a live shell item array returned by the Shell.
            let count = unsafe { array.GetCount()? };
            if count == 0 || count as usize > MAX_SOURCES {
                return Err(E_INVALIDARG.into());
            }
            let mut result = Vec::with_capacity(count as usize);
            for i in 0..count {
                // SAFETY: i is within the shell array's reported bounds.
                result.push(path(&unsafe { array.GetItemAt(i)? }, &mut budget)?);
            }
            result
        }
        Err(array_error) => hdrop(data, &mut budget).map_err(|drop_error| {
            windows::core::Error::new(
                drop_error.code(),
                format!("Unsupported shell selection: array={array_error}; CF_HDROP={drop_error}"),
            )
        })?,
    };
    let request = TransferRequest {
        id: new_transfer_id()
            .map_err(|e| windows::core::Error::new(E_INVALIDARG, e.to_string()))?,
        operation: TransferOperation::Copy,
        origin: TransferOrigin::Explorer,
        sources,
        destination,
    };
    encode_transfer(&request)
        .map_err(|e| windows::core::Error::new(E_INVALIDARG, e.to_string()))?;
    Ok(request)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::windows::ffi::OsStrExt;
    use windows::{
        Win32::{
            System::Com::{COINIT_APARTMENTTHREADED, CoInitializeEx, CoUninitialize},
            UI::Shell::{
                BHID_DataObject, SHCreateItemFromParsingName, SHCreateShellItemArrayFromShellItem,
                SHParseDisplayName,
            },
        },
        core::PCWSTR,
    };
    #[test]
    fn real_data_object_capture_and_hdrop_keep_unicode_paths_without_clipboard_access() {
        let root = tempfile::tempdir().unwrap();
        let source = root.path().join("дані-😀.txt");
        std::fs::write(&source, b"capture fixture").unwrap();
        let destination = root.path().join("target");
        std::fs::create_dir(&destination).unwrap();
        let wide = |p: &std::path::Path| {
            p.as_os_str()
                .encode_wide()
                .chain(Some(0))
                .collect::<Vec<_>>()
        };
        struct Apartment;
        impl Drop for Apartment {
            fn drop(&mut self) {
                /* SAFETY: balances this thread's successful initialization. */
                unsafe { CoUninitialize() };
            }
        }
        // SAFETY: This test owns an STA and every interface/PIDL is released before it ends.
        unsafe {
            CoInitializeEx(None, COINIT_APARTMENTTHREADED).ok().unwrap();
            let _apartment = Apartment;
            let item: IShellItem =
                SHCreateItemFromParsingName(PCWSTR(wide(&source).as_ptr()), None).unwrap();
            let array: IShellItemArray = SHCreateShellItemArrayFromShellItem(&item).unwrap();
            let data: IDataObject = array.BindToHandler(None, &BHID_DataObject).unwrap();
            let mut pidl = std::ptr::null_mut();
            SHParseDisplayName(
                PCWSTR(wide(&destination).as_ptr()),
                None,
                &mut pidl,
                0,
                None,
            )
            .unwrap();
            let target = super::destination(pidl);
            CoTaskMemFree(Some(pidl.cast()));
            let captured = capture(target.unwrap(), &data).unwrap();
            assert_eq!(captured.sources.len(), 1);
            assert_eq!(
                captured.sources[0].canonicalize().unwrap(),
                source.canonicalize().unwrap()
            );
            assert_eq!(
                captured.destination.canonicalize().unwrap(),
                destination.canonicalize().unwrap()
            );
            let paths = hdrop(&data, &mut 0).unwrap();
            assert_eq!(
                paths[0].canonicalize().unwrap(),
                source.canonicalize().unwrap()
            );
            assert!(super::destination(std::ptr::null()).is_err());
        }
        assert_eq!(std::fs::read(&source).unwrap(), b"capture fixture");
        assert_eq!(std::fs::read_dir(&destination).unwrap().count(), 0);
    }
}

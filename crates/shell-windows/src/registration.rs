//! Explicit per-user classic Explorer verbs. Never changes default copy handlers.
use crate::wide;
use std::{ffi::OsString, io, os::windows::ffi::OsStringExt, path::Path};
use windows_sys::Win32::{Foundation::*, System::Registry::*};
const OWNER: &str = "VartaCopy.Explorer.v1";
const VERBS: [&str; 1] = [r"AllFilesystemObjects\shell\VartaCopy.Copy"];
const ROOT: &str = r"Software\Classes";
struct Key(HKEY);
impl Drop for Key {
    fn drop(&mut self) {
        // SAFETY: Owns the opened/created key handle, never a predefined hive.
        let code = unsafe { RegCloseKey(self.0) };
        if code != ERROR_SUCCESS {
            eprintln!("VartaCopy: registry handle close failed: {code}");
        }
    }
}
fn check(code: u32) -> io::Result<()> {
    if code == ERROR_SUCCESS {
        Ok(())
    } else {
        Err(io::Error::from_raw_os_error(code as i32))
    }
}
fn open(path: &str) -> io::Result<Option<Key>> {
    let mut key = std::ptr::null_mut();
    // SAFETY: Predefined user hive, valid NUL-terminated path and output handle pointer.
    let code = unsafe {
        RegOpenKeyExW(
            HKEY_CURRENT_USER,
            wide(path).as_ptr(),
            0,
            KEY_READ | KEY_WRITE,
            &mut key,
        )
    };
    if code == ERROR_FILE_NOT_FOUND {
        return Ok(None);
    }
    check(code)?;
    Ok(Some(Key(key)))
}
fn create(path: &str) -> io::Result<Key> {
    let mut key = std::ptr::null_mut();
    // SAFETY: Creates only a named per-user product key; no inherited or borrowed handles are returned.
    check(unsafe {
        RegCreateKeyExW(
            HKEY_CURRENT_USER,
            wide(path).as_ptr(),
            0,
            std::ptr::null(),
            0,
            KEY_READ | KEY_WRITE,
            std::ptr::null(),
            &mut key,
            std::ptr::null_mut(),
        )
    })?;
    Ok(Key(key))
}
fn read(key: &Key, name: &str) -> io::Result<Option<Vec<u16>>> {
    let name = wide(name);
    let mut size = 0;
    // SAFETY: Query the required size of a REG_SZ value without writing data.
    let code = unsafe {
        RegGetValueW(
            key.0,
            std::ptr::null(),
            name.as_ptr(),
            RRF_RT_REG_SZ,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            &mut size,
        )
    };
    if code == ERROR_FILE_NOT_FOUND {
        return Ok(None);
    }
    check(code)?;
    if size > 256 * 1024 || size % 2 != 0 {
        return Err(io::Error::other("invalid Explorer registration value size"));
    }
    let mut value = vec![0u16; size as usize / 2 + 1];
    // SAFETY: Aligned UTF-16 buffer at least as large as the returned byte size.
    check(unsafe {
        RegGetValueW(
            key.0,
            std::ptr::null(),
            name.as_ptr(),
            RRF_RT_REG_SZ,
            std::ptr::null_mut(),
            value.as_mut_ptr().cast(),
            &mut size,
        )
    })?;
    value.truncate(size as usize / 2);
    if value.last() == Some(&0) {
        value.pop();
    }
    Ok(Some(value))
}
fn set(key: &Key, name: &str, value: impl AsRef<std::ffi::OsStr>) -> io::Result<()> {
    let value = wide(value);
    let name = wide(name);
    // SAFETY: Both inputs are NUL-terminated, data byte length includes the UTF-16 terminator.
    check(unsafe {
        RegSetValueExW(
            key.0,
            name.as_ptr(),
            0,
            REG_SZ,
            value.as_ptr().cast(),
            (value.len() * 2) as u32,
        )
    })
}
fn paths(root: &str) -> Vec<String> {
    VERBS.iter().map(|v| format!("{root}\\{v}")).collect()
}
fn command(gui: &Path) -> io::Result<OsString> {
    let gui = shell_exe(gui)?;
    if !gui.is_file() {
        return Err(io::Error::other("GUI executable is missing"));
    }
    let mut value = OsString::from("\"");
    value.push(gui);
    value.push("\" --source \"%1\"");
    Ok(value)
}
fn shell_exe(gui: &Path) -> io::Result<std::path::PathBuf> {
    use std::os::windows::ffi::OsStrExt;
    let path = gui.canonicalize()?;
    let units = path.as_os_str().encode_wide().collect::<Vec<_>>();
    let dos: Vec<u16> = r"\\?\".encode_utf16().collect();
    let unc: Vec<u16> = r"\\?\UNC\".encode_utf16().collect();
    if units.starts_with(&unc) {
        let native = std::iter::repeat_n(b'\\' as u16, 2)
            .chain(units[unc.len()..].iter().copied())
            .collect::<Vec<_>>();
        Ok(OsString::from_wide(&native).into())
    } else if units.starts_with(&dos) && units.get(5) == Some(&(b':' as u16)) {
        Ok(OsString::from_wide(&units[4..]).into())
    } else {
        Ok(path)
    }
}
fn check_owned(paths: &[String]) -> io::Result<()> {
    for path in paths {
        if let Some(key) = open(path)?
            && read(&key, "VartaCopyOwner")?.as_deref()
                != Some(&wide(OWNER)[..OWNER.encode_utf16().count()])
        {
            return Err(io::Error::other(format!(
                "refusing to replace an unowned Explorer entry: {path}"
            )));
        }
    }
    Ok(())
}
pub fn install(gui: &Path) -> io::Result<()> {
    install_at(ROOT, gui)?;
    notify();
    Ok(())
}
fn install_at(root: &str, gui: &Path) -> io::Result<()> {
    let command = command(gui)?;
    let paths = paths(root);
    check_owned(&paths)?;
    let result = (|| {
        for path in &paths {
            let key = create(path)?;
            set(&key, "VartaCopyOwner", OWNER)?;
            set(&key, "", "Копіювати через VartaCopy…")?;
            set(&key, "MultiSelectModel", "Player")?;
            set(&key, "NeverDefault", "")?;
            let mut icon = OsString::from("\"");
            icon.push(shell_exe(gui)?);
            icon.push("\",0");
            set(&key, "Icon", icon)?;
            set(&create(&format!("{path}\\command"))?, "", &command)?;
        }
        Ok(())
    })();
    if let Err(error) = result {
        return match remove_at(root, None) {
            Ok(()) => Err(error),
            Err(cleanup) => Err(io::Error::other(format!(
                "Explorer registration failed: {error}; cleanup also failed: {cleanup}"
            ))),
        };
    }
    Ok(())
}
pub fn remove(gui: &Path) -> io::Result<()> {
    remove_at(ROOT, Some(&command(gui)?))?;
    notify();
    Ok(())
}
fn notify() {
    use windows_sys::Win32::UI::Shell::{
        SHCNE_ASSOCCHANGED, SHCNF_FLUSH, SHCNF_IDLIST, SHChangeNotify,
    };
    // SAFETY: Documented association-change broadcast has no item payload and does not restart Explorer.
    unsafe {
        SHChangeNotify(
            SHCNE_ASSOCCHANGED as i32,
            SHCNF_IDLIST | SHCNF_FLUSH,
            std::ptr::null(),
            std::ptr::null(),
        );
    }
}
fn remove_at(root: &str, expected: Option<&OsString>) -> io::Result<()> {
    let paths = paths(root);
    check_owned(&paths)?;
    for path in &paths {
        if open(path)?.is_some()
            && let Some(expected) = expected
        {
            let key = open(&format!("{path}\\command"))?
                .ok_or_else(|| io::Error::other("incomplete Explorer entry"))?;
            if read(&key, "")?.map(|v| OsString::from_wide(&v)).as_ref() != Some(expected) {
                return Err(io::Error::other(
                    "a different VartaCopy installation owns the Explorer menu",
                ));
            }
        }
    }
    for path in paths {
        // SAFETY: Preflight checked ownership and path is limited to these exact product verb subtrees.
        let code = unsafe { RegDeleteTreeW(HKEY_CURRENT_USER, wide(&path).as_ptr()) };
        if code != ERROR_FILE_NOT_FOUND {
            check(code)?;
        }
    }
    Ok(())
}
pub fn installed(gui: &Path) -> io::Result<bool> {
    let expected = command(gui)?;
    for path in paths(ROOT) {
        let Some(key) = open(&format!("{path}\\command"))? else {
            return Ok(false);
        };
        if read(&key, "")?.map(|v| OsString::from_wide(&v)).as_ref() != Some(&expected) {
            return Ok(false);
        }
    }
    Ok(true)
}
#[cfg(test)]
mod tests {
    use super::*;
    use std::os::windows::ffi::OsStrExt;
    #[test]
    fn registration_quotes_native_paths_is_idempotent_and_preserves_foreign_keys() {
        let root = format!(r"Software\VartaCopy\Tests\{}", std::process::id());
        let dir = tempfile::tempdir().unwrap();
        let gui = dir.path().join("test gui.exe");
        std::fs::write(&gui, b"test").unwrap();
        let foreign = paths(&root)[0].clone();
        set(&create(&foreign).unwrap(), "", "foreign").unwrap();
        assert!(install_at(&root, &gui).is_err());
        assert!(
            open(&format!("{}\\command", paths(&root)[0]))
                .unwrap()
                .is_none()
        );
        assert_eq!(
            read(&open(&foreign).unwrap().unwrap(), "")
                .unwrap()
                .unwrap(),
            "foreign".encode_utf16().collect::<Vec<_>>()
        );
        // SAFETY: Deletes only this test's unique temporary subtree, never the real Explorer registration.
        check(unsafe { RegDeleteTreeW(HKEY_CURRENT_USER, wide(&root).as_ptr()) }).unwrap();
        install_at(&root, &gui).unwrap();
        install_at(&root, &gui).unwrap();
        let expected = command(&gui).unwrap();
        let key = open(&format!("{}\\command", paths(&root)[0]))
            .unwrap()
            .unwrap();
        assert_eq!(
            read(&key, "").unwrap().unwrap(),
            expected.as_os_str().encode_wide().collect::<Vec<_>>()
        );
        drop(key);
        remove_at(&root, Some(&expected)).unwrap();
        remove_at(&root, Some(&expected)).unwrap();
        // SAFETY: Remove empty parents of the same temporary registry fixture only.
        check(unsafe { RegDeleteTreeW(HKEY_CURRENT_USER, wide(&root).as_ptr()) }).unwrap();
    }
}

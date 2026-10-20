//! Narrow 64-bit registry access; durable rollback is managed by the operation journal.
use crate::wide;
use std::{
    collections::{BTreeMap, BTreeSet},
    ffi::OsString,
    io,
    os::windows::ffi::OsStringExt,
    ptr,
};
use windows_sys::Win32::{Foundation::*, System::Registry::*};

pub type Values = BTreeMap<String, Vec<u16>>;
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Snapshot {
    pub values: Values,
    pub children: BTreeSet<String>,
}
struct Key(HKEY);
impl Drop for Key {
    fn drop(&mut self) {
        // SAFETY: This is our uniquely owned open key, not a predefined hive.
        let code = unsafe { RegCloseKey(self.0) };
        if code != ERROR_SUCCESS {
            eprintln!("VartaCopy integration registry close failed: {code}");
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
pub struct Registry {
    hive: HKEY,
    prefix: String,
    writable: bool,
}
impl Registry {
    pub fn identity(&self) -> (&'static str, &str) {
        (
            if self.hive == HKEY_LOCAL_MACHINE {
                "machine"
            } else if self.hive == HKEY_CURRENT_USER {
                "user"
            } else {
                "unsupported"
            },
            &self.prefix,
        )
    }
    pub fn read(hive: HKEY, prefix: String) -> Self {
        Self {
            hive,
            prefix,
            writable: false,
        }
    }
    pub fn begin(hive: HKEY, prefix: String) -> io::Result<Self> {
        if prefix.contains('\0') || prefix.len() > 4096 {
            return Err(io::Error::other("Invalid registry namespace"));
        }
        Ok(Self {
            hive,
            prefix,
            writable: true,
        })
    }
    fn path(&self, path: &str) -> Vec<u16> {
        wide(if self.prefix.is_empty() {
            path.to_owned()
        } else {
            format!("{}\\{path}", self.prefix)
        })
    }
    fn open(&self, path: &str, create: bool) -> io::Result<Option<Key>> {
        let mut key = ptr::null_mut();
        let path = self.path(path);
        if create && !self.writable {
            return Err(io::Error::other("Read-only registry context"));
        }
        // SAFETY: Predefined hive, bounded terminated path, valid output. No global hive remapping.
        let code = unsafe {
            if create {
                RegCreateKeyExW(
                    self.hive,
                    path.as_ptr(),
                    0,
                    ptr::null(),
                    0,
                    KEY_READ | KEY_WRITE | KEY_WOW64_64KEY,
                    ptr::null(),
                    &mut key,
                    ptr::null_mut(),
                )
            } else {
                RegOpenKeyExW(
                    self.hive,
                    path.as_ptr(),
                    0,
                    KEY_READ | KEY_WOW64_64KEY | if self.writable { KEY_WRITE } else { 0 },
                    &mut key,
                )
            }
        };
        if code == ERROR_FILE_NOT_FOUND {
            return Ok(None);
        }
        check(code)?;
        Ok(Some(Key(key)))
    }
    pub fn value(&self, path: &str, name: &str) -> io::Result<Option<Vec<u16>>> {
        let Some(key) = self.open(path, false)? else {
            return Ok(None);
        };
        let name = wide(name);
        let mut size = 0;
        let mut kind = 0;
        // SAFETY: Size-only query; output scalars are writable, input strings live.
        let code = unsafe {
            RegQueryValueExW(
                key.0,
                name.as_ptr(),
                ptr::null(),
                &mut kind,
                ptr::null_mut(),
                &mut size,
            )
        };
        if code == ERROR_FILE_NOT_FOUND {
            return Ok(None);
        }
        check(code)?;
        if kind != REG_SZ || size > 65536 || !size.is_multiple_of(2) {
            return Err(io::Error::other("Unexpected integration registry value"));
        }
        let mut units = vec![0; size as usize / 2];
        // SAFETY: UTF-16 buffer matches the queried size. A concurrent growth is rejected by the API.
        check(unsafe {
            RegQueryValueExW(
                key.0,
                name.as_ptr(),
                ptr::null(),
                &mut kind,
                units.as_mut_ptr().cast(),
                &mut size,
            )
        })?;
        if kind != REG_SZ || size == 0 || !size.is_multiple_of(2) {
            return Err(io::Error::other("Integration registry value changed"));
        }
        units.truncate(size as usize / 2);
        if units.pop() != Some(0) || units.contains(&0) {
            return Err(io::Error::other("Malformed integration registry string"));
        }
        Ok(Some(units))
    }
    pub fn snapshot(&self, path: &str) -> io::Result<Option<Snapshot>> {
        let Some(key) = self.open(path, false)? else {
            return Ok(None);
        };
        let mut values = BTreeMap::new();
        let mut children = BTreeSet::new();
        for index in 0..1024 {
            let mut name = vec![0; 32768];
            let mut count = name.len() as u32;
            // SAFETY: Enumeration only writes the provided bounded name buffer/count.
            let code = unsafe {
                RegEnumValueW(
                    key.0,
                    index,
                    name.as_mut_ptr(),
                    &mut count,
                    ptr::null(),
                    ptr::null_mut(),
                    ptr::null_mut(),
                    ptr::null_mut(),
                )
            };
            if code == ERROR_NO_MORE_ITEMS {
                break;
            }
            check(code)?;
            let name = String::from_utf16(&name[..count as usize]).map_err(io::Error::other)?;
            let value = self
                .value(path, &name)?
                .ok_or_else(|| io::Error::other("Integration value disappeared"))?;
            values.insert(name, value);
            if index == 1023 {
                return Err(io::Error::other("Too many integration values"));
            }
        }
        for index in 0..1024 {
            let mut name = vec![0; 256];
            let mut count = name.len() as u32;
            // SAFETY: Enumeration only writes the provided bounded subkey-name buffer/count.
            let code = unsafe {
                RegEnumKeyExW(
                    key.0,
                    index,
                    name.as_mut_ptr(),
                    &mut count,
                    ptr::null(),
                    ptr::null_mut(),
                    ptr::null_mut(),
                    ptr::null_mut(),
                )
            };
            if code == ERROR_NO_MORE_ITEMS {
                break;
            }
            check(code)?;
            children.insert(String::from_utf16(&name[..count as usize]).map_err(io::Error::other)?);
            if index == 1023 {
                return Err(io::Error::other("Too many integration subkeys"));
            }
        }
        Ok(Some(Snapshot { values, children }))
    }
    pub fn set(&self, path: &str, name: &str, units: &[u16]) -> io::Result<()> {
        let key = self
            .open(path, true)?
            .ok_or_else(|| io::Error::other("Registry creation returned no key"))?;
        let value: Vec<u16> = units.iter().copied().chain(Some(0)).collect();
        // SAFETY: Strings/data are live, byte length includes the UTF-16 terminator.
        check(unsafe {
            RegSetValueExW(
                key.0,
                wide(name).as_ptr(),
                0,
                REG_SZ,
                value.as_ptr().cast(),
                (value.len() * 2) as u32,
            )
        })
    }
    pub fn create(&self, path: &str) -> io::Result<()> {
        self.open(path, true)?
            .ok_or_else(|| io::Error::other("Missing created key"))?;
        Ok(())
    }
    pub fn delete_value(&self, path: &str, name: &str) -> io::Result<()> {
        let Some(key) = self.open(path, false)? else {
            return Ok(());
        };
        // SAFETY: Only the explicitly named value, through the transacted handle.
        let code = unsafe { RegDeleteValueW(key.0, wide(name).as_ptr()) };
        if code == ERROR_FILE_NOT_FOUND {
            Ok(())
        } else {
            check(code)
        }
    }
    pub fn delete_key(&self, path: &str) -> io::Result<()> {
        if !self.writable {
            return Err(io::Error::other("Read-only registry context"));
        }
        // SAFETY: Caller preflights exact ownership. Non-recursive delete refuses foreign children.
        let code =
            unsafe { RegDeleteKeyExW(self.hive, self.path(path).as_ptr(), KEY_WOW64_64KEY, 0) };
        if code == ERROR_FILE_NOT_FOUND {
            Ok(())
        } else {
            check(code)
        }
    }
    pub fn flush(&self, path: &str) -> io::Result<()> {
        if let Some(key) = self.open(path, false)? {
            // SAFETY: Flush only through a valid open key, after the journal has verified final state.
            check(unsafe { RegFlushKey(key.0) })?;
        }
        Ok(())
    }
    #[cfg(test)]
    pub fn commit(self) -> io::Result<()> {
        self.flush(super::APPROVED)
    }
}
pub fn native(units: &[u16]) -> std::path::PathBuf {
    OsString::from_wide(units).into()
}

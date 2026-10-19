//! Scope Common Controls v6 to our modal call, independently of the host manifest.
use windows::{
    Win32::{
        Foundation::{E_FAIL, FreeLibrary, HANDLE, HMODULE},
        System::{
            ApplicationInstallationAndServicing::{
                ACTCTXW, ActivateActCtx, CreateActCtxW, DeactivateActCtx, ReleaseActCtx,
            },
            LibraryLoader::{
                GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS,
                GET_MODULE_HANDLE_EX_FLAG_UNCHANGED_REFCOUNT, GetModuleHandleExW, GetProcAddress,
                LOAD_LIBRARY_SEARCH_SYSTEM32, LoadLibraryExW,
            },
            WindowsProgramming::{ACTCTX_FLAG_HMODULE_VALID, ACTCTX_FLAG_RESOURCE_NAME_VALID},
        },
        UI::Controls::TASKDIALOGCONFIG,
    },
    core::{BOOL, HRESULT, PCWSTR, Result, s, w},
};
type TaskDialog =
    unsafe extern "system" fn(*const TASKDIALOGCONFIG, *mut i32, *mut i32, *mut BOOL) -> HRESULT;
pub struct Controls {
    module: Option<HMODULE>,
    context: HANDLE,
    cookie: usize,
    // Activation contexts are thread-local and must be restored on their creating thread.
    _thread: std::marker::PhantomData<std::rc::Rc<()>>,
}
impl Controls {
    pub fn load() -> Result<Self> {
        let mut module = HMODULE::default();
        // SAFETY: FROM_ADDRESS treats this value as an address in our module, not text.
        unsafe {
            GetModuleHandleExW(
                GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS
                    | GET_MODULE_HANDLE_EX_FLAG_UNCHANGED_REFCOUNT,
                PCWSTR(Self::load as *const () as *const u16),
                &mut module,
            )?;
        }
        let descriptor = ACTCTXW {
            cbSize: std::mem::size_of::<ACTCTXW>() as u32,
            dwFlags: ACTCTX_FLAG_HMODULE_VALID | ACTCTX_FLAG_RESOURCE_NAME_VALID,
            hModule: module,
            // MAKEINTRESOURCEW(2): a tagged resource identifier, never dereferenced as text.
            lpResourceName: PCWSTR(std::ptr::without_provenance(2)),
            ..Default::default()
        };
        Self::create(&descriptor)
    }
    fn create(descriptor: &ACTCTXW) -> Result<Self> {
        // SAFETY: All descriptor pointers remain live during creation. The handle is owned.
        let context = unsafe { CreateActCtxW(descriptor)? };
        let mut cookie = 0;
        // SAFETY: Activate a live context on this thread; restore in Drop on the same thread.
        if let Err(error) = unsafe { ActivateActCtx(Some(context), &mut cookie) } {
            // SAFETY: Activation failed; release the sole owned handle.
            unsafe {
                ReleaseActCtx(context);
            }
            return Err(error);
        }
        let mut controls = Self {
            module: None,
            context,
            cookie,
            _thread: std::marker::PhantomData,
        };
        // SAFETY: The active manifest selects v6; search only trusted Windows system paths.
        controls.module = Some(unsafe {
            LoadLibraryExW(w!("comctl32.dll"), None, LOAD_LIBRARY_SEARCH_SYSTEM32)?
        });
        controls.entry()?;
        Ok(controls)
    }
    fn entry(&self) -> Result<TaskDialog> {
        // SAFETY: The module is retained until this object's Drop.
        let entry = unsafe { GetProcAddress(self.module.ok_or(E_FAIL)?, s!("TaskDialogIndirect")) }
            .ok_or(E_FAIL)?;
        // SAFETY: The documented export has exactly this system ABI and signature.
        Ok(unsafe {
            std::mem::transmute::<unsafe extern "system" fn() -> isize, TaskDialog>(entry)
        })
    }
    /// # Safety
    /// Every string/button in `config` must remain live until the modal dialog returns.
    pub unsafe fn show(&self, config: &TASKDIALOGCONFIG, selected: &mut i32) -> Result<()> {
        // SAFETY: The caller guarantees descriptor validity; outputs are valid or optional NULL.
        unsafe { self.entry()?(config, selected, std::ptr::null_mut(), std::ptr::null_mut()).ok() }
    }
}
impl Drop for Controls {
    fn drop(&mut self) {
        if let Some(module) = self.module {
            // SAFETY: Balances exactly one LoadLibrary call, after the modal function returned.
            if let Err(error) = unsafe { FreeLibrary(module) } {
                eprintln!("VartaCopy controls release failed: {error}");
            }
        }
        // SAFETY: LIFO balance for activation on this same thread; no thread handoff occurs.
        if let Err(error) = unsafe { DeactivateActCtx(0, self.cookie) } {
            eprintln!("VartaCopy context restore failed: {error}");
        }
        // SAFETY: Releases our owned reference; the OS retains any still-active reference.
        unsafe {
            ReleaseActCtx(self.context);
        }
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn v6_export_resolves_without_a_host_manifest_or_showing_any_window() {
        let manifest = concat!(env!("CARGO_MANIFEST_DIR"), "/common-controls.manifest")
            .encode_utf16()
            .chain(Some(0))
            .collect::<Vec<_>>();
        let descriptor = ACTCTXW {
            cbSize: std::mem::size_of::<ACTCTXW>() as u32,
            lpSource: PCWSTR(manifest.as_ptr()),
            ..Default::default()
        };
        let controls = Controls::create(&descriptor).unwrap();
        controls.entry().unwrap();
    }
}

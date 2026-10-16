use std::{
    ffi::c_void,
    panic::{AssertUnwindSafe, catch_unwind},
    ptr,
    sync::atomic::{AtomicU32, AtomicUsize, Ordering},
};
use windows::{
    Win32::{
        Foundation::{
            CLASS_E_CLASSNOTAVAILABLE, CLASS_E_NOAGGREGATION, E_FAIL, E_INVALIDARG, E_NOINTERFACE,
            E_NOTIMPL, E_OUTOFMEMORY, E_POINTER, S_FALSE, S_OK,
        },
        System::Com::{CoTaskMemAlloc, IClassFactory, IClassFactory_Vtbl},
        UI::Shell::{
            ECS_ENABLED, ECS_HIDDEN, IExplorerCommand, IExplorerCommand_Vtbl, IShellItemArray,
        },
    },
    core::{BOOL, GUID, HRESULT, IUnknown, IUnknown_Vtbl, Interface, PWSTR},
};

static OBJECTS: AtomicUsize = AtomicUsize::new(0);
static LOCKS: AtomicUsize = AtomicUsize::new(0);
const E_PENDING: HRESULT = HRESULT(0x8000000A_u32 as i32);

// Each class has exactly one interface identity and an IUnknown prefix. There is
// no aggregation, custom marshaling, background worker or shared mutable payload.
#[repr(C)]
struct Object {
    vtable: *const c_void,
    refs: AtomicU32,
    iid: GUID,
}

fn ffi(body: impl FnOnce() -> HRESULT) -> HRESULT {
    match catch_unwind(AssertUnwindSafe(body)) {
        Ok(result) => result,
        Err(_) => E_FAIL,
    }
}

fn new_object(factory: bool) -> *mut c_void {
    let (vtable, iid) = if factory {
        (
            &FACTORY as *const IClassFactory_Vtbl as *const c_void,
            IClassFactory::IID,
        )
    } else {
        (
            &COMMAND as *const IExplorerCommand_Vtbl as *const c_void,
            IExplorerCommand::IID,
        )
    };
    let object = Box::new(Object {
        vtable,
        refs: AtomicU32::new(1),
        iid,
    });
    OBJECTS.fetch_add(1, Ordering::SeqCst);
    Box::into_raw(object).cast()
}

unsafe extern "system" fn query(
    this: *mut c_void,
    iid: *const GUID,
    out: *mut *mut c_void,
) -> HRESULT {
    ffi(|| {
        if out.is_null() {
            return E_POINTER;
        }
        // SAFETY: COM caller supplies writable out storage; always initialize it.
        unsafe { *out = ptr::null_mut() };
        if this.is_null() || iid.is_null() {
            return E_POINTER;
        }
        // SAFETY: this is an active Object and iid is a readable GUID by contract.
        let object = unsafe { &*this.cast::<Object>() };
        // SAFETY: iid was checked non-null; valid input storage is caller-owned.
        if unsafe { *iid } != object.iid && unsafe { *iid } != IUnknown::IID {
            return E_NOINTERFACE;
        }
        // SAFETY: a valid live object gains one reference before being returned.
        unsafe {
            add_ref(this);
            *out = this;
        }
        S_OK
    })
}

unsafe extern "system" fn add_ref(this: *mut c_void) -> u32 {
    if this.is_null() {
        return 0;
    }
    // SAFETY: this identifies an Object while the caller owns a reference.
    let old = unsafe { &*this.cast::<Object>() }
        .refs
        .fetch_add(1, Ordering::Relaxed);
    // Integer-returning IUnknown methods cannot report HRESULT. Abort rather than
    // wrap a reference count or unwind across COM on an impossible overflow.
    if old == u32::MAX {
        std::process::abort();
    }
    old + 1
}

unsafe extern "system" fn release(this: *mut c_void) -> u32 {
    if this.is_null() {
        return 0;
    }
    // SAFETY: caller releases a live reference to an Object.
    let old = unsafe { &*this.cast::<Object>() }
        .refs
        .fetch_sub(1, Ordering::Release);
    if old == 0 {
        std::process::abort();
    }
    if old == 1 {
        std::sync::atomic::fence(Ordering::Acquire);
        // SAFETY: final reference uniquely owns the allocation; Object has no
        // user Drop implementation or other code that could unwind.
        drop(unsafe { Box::from_raw(this.cast::<Object>()) });
        OBJECTS.fetch_sub(1, Ordering::SeqCst);
    }
    old - 1
}

unsafe extern "system" fn create_instance(
    this: *mut c_void,
    outer: *mut c_void,
    iid: *const GUID,
    out: *mut *mut c_void,
) -> HRESULT {
    ffi(|| {
        if out.is_null() {
            return E_POINTER;
        }
        // SAFETY: caller supplied writable output pointer.
        unsafe { *out = ptr::null_mut() };
        if this.is_null() || iid.is_null() {
            return E_POINTER;
        }
        if !outer.is_null() {
            return CLASS_E_NOAGGREGATION;
        }
        // SAFETY: iid is readable COM input storage after null checking.
        if unsafe { *iid } != IUnknown::IID && unsafe { *iid } != IExplorerCommand::IID {
            return E_NOINTERFACE;
        }
        // SAFETY: output receives the initial owning reference.
        unsafe { *out = new_object(false) };
        S_OK
    })
}

unsafe extern "system" fn lock_server(this: *mut c_void, lock: BOOL) -> HRESULT {
    ffi(|| {
        if this.is_null() {
            return E_POINTER;
        }
        if lock.as_bool() {
            let old = LOCKS.fetch_add(1, Ordering::SeqCst);
            if old == usize::MAX {
                std::process::abort();
            }
        } else if LOCKS
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |n| n.checked_sub(1))
            .is_err()
        {
            return E_INVALIDARG;
        }
        S_OK
    })
}

unsafe fn string_out(out: *mut PWSTR, value: &[u16]) -> HRESULT {
    if out.is_null() {
        return E_POINTER;
    }
    // SAFETY: caller supplies writable COM output storage.
    unsafe { *out = PWSTR::null() };
    let Some(bytes) = value.len().checked_add(1).and_then(|n| n.checked_mul(2)) else {
        return E_OUTOFMEMORY;
    };
    // SAFETY: CoTaskMemAlloc allocates bytes for UTF-16 including the terminator.
    let allocated = unsafe { CoTaskMemAlloc(bytes) }.cast::<u16>();
    if allocated.is_null() {
        return E_OUTOFMEMORY;
    }
    // SAFETY: allocation has value.len()+1 units; the source is disjoint.
    unsafe {
        ptr::copy_nonoverlapping(value.as_ptr(), allocated, value.len());
        *allocated.add(value.len()) = 0;
        *out = PWSTR(allocated);
    }
    S_OK
}

unsafe extern "system" fn title(this: *mut c_void, _: *mut c_void, out: *mut PWSTR) -> HRESULT {
    ffi(|| {
        if this.is_null() {
            return E_POINTER;
        }
        // SAFETY: string_out validates output and returns a COM allocation.
        unsafe {
            string_out(
                out,
                &crate::COMMAND_TITLE.encode_utf16().collect::<Vec<_>>(),
            )
        }
    })
}

unsafe extern "system" fn icon(this: *mut c_void, _: *mut c_void, out: *mut PWSTR) -> HRESULT {
    ffi(|| {
        if out.is_null() {
            return E_POINTER;
        }
        // SAFETY: initialize caller's writable output even if module lookup fails.
        unsafe { *out = PWSTR::null() };
        if this.is_null() {
            return E_POINTER;
        }
        // This slice supplies no native icon resource. Package identity assets
        // remain available to Explorer; never return an invalid icon location.
        E_NOTIMPL
    })
}

unsafe extern "system" fn tooltip(this: *mut c_void, _: *mut c_void, out: *mut PWSTR) -> HRESULT {
    ffi(|| {
        if out.is_null() {
            return E_POINTER;
        }
        // SAFETY: output is caller-owned writable storage.
        unsafe { *out = PWSTR::null() };
        if this.is_null() { E_POINTER } else { E_NOTIMPL }
    })
}

unsafe extern "system" fn canonical(this: *mut c_void, out: *mut GUID) -> HRESULT {
    ffi(|| {
        if out.is_null() {
            return E_POINTER;
        }
        // SAFETY: out is writable GUID storage, initialized before any failure.
        unsafe { *out = GUID::zeroed() };
        if this.is_null() {
            return E_POINTER;
        }
        // SAFETY: caller owns out storage.
        unsafe { *out = crate::COMMAND_CLSID };
        S_OK
    })
}

unsafe extern "system" fn state(
    this: *mut c_void,
    items: *mut c_void,
    slow: BOOL,
    out: *mut u32,
) -> HRESULT {
    ffi(|| {
        if out.is_null() {
            return E_POINTER;
        }
        // SAFETY: initialize writable output, fail closed on all failures/panics.
        unsafe { *out = ECS_HIDDEN.0 as u32 };
        if this.is_null() {
            return E_POINTER;
        }
        // SAFETY: COM owns the array for this call; borrowed conversion adds no ref.
        let Some(items) = (unsafe { IShellItemArray::from_raw_borrowed(&items) }) else {
            return S_OK;
        };
        // SAFETY: live borrowed Shell array, with no disk scan or launch here.
        match unsafe { items.GetCount() } {
            Ok(1) => {}
            Ok(_) => return S_OK,
            Err(error) => return error.code(),
        }
        if !slow.as_bool() {
            return E_PENDING;
        }
        match crate::selection::selection_path(items) {
            Ok(_) => {
                // SAFETY: caller owns writable output.
                unsafe { *out = ECS_ENABLED.0 as u32 };
                S_OK
            }
            Err(e) if e == E_INVALIDARG => S_OK,
            Err(e) => e,
        }
    })
}

unsafe extern "system" fn invoke(this: *mut c_void, items: *mut c_void, _: *mut c_void) -> HRESULT {
    ffi(|| {
        if this.is_null() {
            return E_POINTER;
        }
        // SAFETY: the selection array is borrowed for the duration of Invoke.
        let Some(items) = (unsafe { IShellItemArray::from_raw_borrowed(&items) }) else {
            return E_INVALIDARG;
        };
        // Revalidate selection now: never reuse a previous GetState decision.
        let source = match crate::selection::selection_path(items) {
            Ok(p) => p,
            Err(e) => return e,
        };
        let module = match crate::launch::module_path() {
            Ok(p) => p,
            Err(e) => return e,
        };
        let gui = match crate::launch::gui_from_module(&module) {
            Ok(p) => p,
            Err(e) => return e,
        };
        match crate::launch::launch(&gui, &source) {
            Ok(()) => S_OK,
            Err(e) => e,
        }
    })
}

unsafe extern "system" fn flags(this: *mut c_void, out: *mut u32) -> HRESULT {
    ffi(|| {
        if out.is_null() {
            return E_POINTER;
        }
        // SAFETY: output is writable; ECF_DEFAULT has value zero.
        unsafe { *out = 0 };
        if this.is_null() { E_POINTER } else { S_OK }
    })
}

unsafe extern "system" fn subcommands(this: *mut c_void, out: *mut *mut c_void) -> HRESULT {
    ffi(|| {
        if out.is_null() {
            return E_POINTER;
        }
        // SAFETY: output is writable COM pointer storage.
        unsafe { *out = ptr::null_mut() };
        if this.is_null() { E_POINTER } else { E_NOTIMPL }
    })
}

static FACTORY: IClassFactory_Vtbl = IClassFactory_Vtbl {
    base__: IUnknown_Vtbl {
        QueryInterface: query,
        AddRef: add_ref,
        Release: release,
    },
    CreateInstance: create_instance,
    LockServer: lock_server,
};

static COMMAND: IExplorerCommand_Vtbl = IExplorerCommand_Vtbl {
    base__: IUnknown_Vtbl {
        QueryInterface: query,
        AddRef: add_ref,
        Release: release,
    },
    GetTitle: title,
    GetIcon: icon,
    GetToolTip: tooltip,
    GetCanonicalName: canonical,
    GetState: state,
    Invoke: invoke,
    GetFlags: flags,
    EnumSubCommands: subcommands,
};

/// Obtain the command's class factory without registry access.
///
/// # Safety
/// Non-null input GUIDs must be readable, and `out` must be writable pointer
/// storage, as required by COM's DllGetClassObject contract.
#[unsafe(no_mangle)]
pub unsafe extern "system" fn DllGetClassObject(
    clsid: *const GUID,
    iid: *const GUID,
    out: *mut *mut c_void,
) -> HRESULT {
    ffi(|| {
        if out.is_null() {
            return E_POINTER;
        }
        // SAFETY: caller supplies writable output pointer storage.
        unsafe { *out = ptr::null_mut() };
        if clsid.is_null() || iid.is_null() {
            return E_POINTER;
        }
        // SAFETY: GUID storage is readable by the export's caller contract.
        if unsafe { *clsid } != crate::COMMAND_CLSID {
            return CLASS_E_CLASSNOTAVAILABLE;
        }
        // SAFETY: iid is readable by the same contract.
        if unsafe { *iid } != IClassFactory::IID && unsafe { *iid } != IUnknown::IID {
            return E_NOINTERFACE;
        }
        // SAFETY: initialized output receives the initial owning reference.
        unsafe { *out = new_object(true) };
        S_OK
    })
}

/// Report whether all factory/command references and server locks were released.
#[unsafe(no_mangle)]
pub extern "system" fn DllCanUnloadNow() -> HRESULT {
    ffi(|| {
        if OBJECTS.load(Ordering::SeqCst) == 0 && LOCKS.load(Ordering::SeqCst) == 0 {
            S_OK
        } else {
            S_FALSE
        }
    })
}

#[cfg(test)]
pub(crate) fn catches_panic() -> HRESULT {
    ffi(|| panic!("synthetic FFI panic"))
}

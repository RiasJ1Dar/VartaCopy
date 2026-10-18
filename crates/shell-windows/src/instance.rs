use crate::{COPYDATA_MAGIC, encode, wide};
use std::{
    io,
    marker::PhantomData,
    os::windows::ffi::OsStrExt,
    path::{Path, PathBuf},
    rc::Rc,
    thread,
    time::{Duration, Instant},
};
use windows_sys::Win32::{
    Foundation::*,
    Security::*,
    System::{DataExchange::COPYDATASTRUCT, Threading::*},
    UI::WindowsAndMessaging::*,
};

pub struct Instance {
    mutex: HANDLE,
    marker: Vec<u16>,
    _thread: PhantomData<Rc<()>>,
}
fn logon_id() -> io::Result<String> {
    let mut token = std::ptr::null_mut();
    // SAFETY: Valid output pointer; request only query access to this process token.
    if unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) } == 0 {
        return Err(io::Error::last_os_error());
    }
    let mut stats = TOKEN_STATISTICS::default();
    let mut size = 0;
    // SAFETY: Initialized, properly aligned TOKEN_STATISTICS output with its exact size.
    let ok = unsafe {
        GetTokenInformation(
            token,
            TokenStatistics,
            (&mut stats as *mut TOKEN_STATISTICS).cast(),
            std::mem::size_of::<TOKEN_STATISTICS>() as u32,
            &mut size,
        )
    };
    let failure = if ok == 0 {
        Some(io::Error::last_os_error())
    } else {
        None
    };
    // SAFETY: This function exclusively owns the successfully opened token handle.
    if unsafe { CloseHandle(token) } == 0 {
        return Err(io::Error::last_os_error());
    }
    if let Some(e) = failure {
        return Err(e);
    }
    Ok(format!(
        "{:08x}{:08x}",
        stats.AuthenticationId.HighPart, stats.AuthenticationId.LowPart
    ))
}
impl Instance {
    /// Elect one GUI for this journal and logon. Secondary launches deliver a draft then exit.
    pub fn enter(journal: &Path, paths: &[PathBuf]) -> io::Result<Option<Self>> {
        Self::enter_message(journal, COPYDATA_MAGIC, &encode(paths)?)
    }
    /// Forward a complete intent without selecting an engine or starting work.
    pub fn enter_transfer(
        journal: &Path,
        request: &crate::transfer::TransferRequest,
    ) -> io::Result<Option<Self>> {
        Self::enter_message(
            journal,
            crate::transfer::COPYDATA_TRANSFER_MAGIC,
            &crate::transfer::encode_transfer(request)?,
        )
    }
    fn enter_message(journal: &Path, magic: usize, bytes: &[u8]) -> io::Result<Option<Self>> {
        let absolute = std::path::absolute(journal)?;
        let parent = absolute
            .parent()
            .ok_or_else(|| io::Error::other("journal has no parent"))?
            .canonicalize()?;
        let name = absolute
            .file_name()
            .ok_or_else(|| io::Error::other("journal has no filename"))?;
        let normalized = parent.join(name);
        let journal_key_bytes = normalized
            .as_os_str()
            .encode_wide()
            .flat_map(|c| {
                // ASCII case aliases are common for command-line journal paths; native UTF-16 is retained.
                let c = if (b'a' as u16..=b'z' as u16).contains(&c) {
                    c - 32
                } else {
                    c
                };
                c.to_le_bytes()
            })
            .collect::<Vec<_>>();
        let key = format!(
            "VartaCopy.{}.{}",
            logon_id()?,
            blake3::hash(&journal_key_bytes).to_hex()
        );
        let marker = wide(&key);
        // SAFETY: NUL terminated name; default token DACL, mutex is scoped to this logon/session and journal.
        let mutex =
            unsafe { CreateMutexW(std::ptr::null(), 0, wide(format!("Local\\{key}")).as_ptr()) };
        if mutex.is_null() {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: Newly opened valid mutex, nonblocking acquisition on this same thread.
        let result = unsafe { WaitForSingleObject(mutex, 0) };
        if result == WAIT_OBJECT_0 || result == WAIT_ABANDONED {
            Ok(Some(Self {
                mutex,
                marker,
                _thread: PhantomData,
            }))
        } else {
            let error = if result == WAIT_TIMEOUT {
                None
            } else {
                Some(io::Error::last_os_error())
            };
            // SAFETY: The caller did not acquire this mutex; close only its own handle.
            if unsafe { CloseHandle(mutex) } == 0 {
                return Err(io::Error::last_os_error());
            }
            if let Some(e) = error {
                return Err(e);
            }
            forward(&marker, magic, bytes)?;
            Ok(None)
        }
    }
    pub fn marker(&self) -> &[u16] {
        &self.marker
    }
}
impl Drop for Instance {
    fn drop(&mut self) {
        // SAFETY: !Send/!Sync guard releases its own mutex on the owning thread, then closes its handle.
        unsafe {
            if ReleaseMutex(self.mutex) == 0 {
                eprintln!(
                    "VartaCopy: cannot release GUI instance mutex: {}",
                    io::Error::last_os_error()
                );
            }
            if CloseHandle(self.mutex) == 0 {
                eprintln!(
                    "VartaCopy: cannot close GUI instance mutex: {}",
                    io::Error::last_os_error()
                );
            }
        }
    }
}
struct Search<'a> {
    marker: &'a [u16],
    window: HWND,
}
unsafe extern "system" fn find(window: HWND, data: LPARAM) -> i32 {
    // SAFETY: EnumWindows invokes synchronously with our live Search pointer.
    let search = unsafe { &mut *(data as *mut Search<'_>) };
    // SAFETY: NUL terminated property name; only reads the enumerated HWND.
    if !unsafe { GetPropW(window, search.marker.as_ptr()) }.is_null() {
        search.window = window;
        0
    } else {
        1
    }
}
fn forward(marker: &[u16], magic: usize, bytes: &[u8]) -> io::Result<()> {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let mut search = Search {
            marker,
            window: std::ptr::null_mut(),
        };
        // SAFETY: Synchronous callback and valid mutable Search for this enumeration only.
        unsafe { EnumWindows(Some(find), (&mut search as *mut Search<'_>) as LPARAM) };
        if !search.window.is_null() {
            let mut pid = 0;
            // SAFETY: Reads the receiving HWND owner and permits that process to focus its own window.
            unsafe {
                GetWindowThreadProcessId(search.window, &mut pid);
                AllowSetForegroundWindow(pid);
            }
            let data = COPYDATASTRUCT {
                dwData: magic,
                cbData: bytes.len() as u32,
                lpData: bytes.as_ptr() as _,
            };
            let mut ack = 0;
            // SAFETY: Windows marshals WM_COPYDATA across processes; payload and structure remain live for the call.
            let ok = unsafe {
                SendMessageTimeoutW(
                    search.window,
                    WM_COPYDATA,
                    0,
                    (&data as *const COPYDATASTRUCT) as LPARAM,
                    SMTO_ABORTIFHUNG | SMTO_BLOCK,
                    3000,
                    &mut ack,
                )
            };
            if ok == 0 {
                return Err(io::Error::other(
                    "existing VartaCopy window did not answer; request was not confirmed",
                ));
            }
            if ack != 1 {
                return Err(io::Error::other(
                    "VartaCopy draft inbox is full or request was rejected",
                ));
            }
            return Ok(());
        }
        if Instant::now() >= deadline {
            return Err(io::Error::other(
                "existing VartaCopy instance is starting or closing; retry the command",
            ));
        }
        thread::sleep(Duration::from_millis(25));
    }
}
pub struct Endpoint {
    window: HWND,
    marker: Vec<u16>,
    _thread: PhantomData<Rc<()>>,
}
impl Endpoint {
    /// # Safety
    /// The HWND must belong to the calling UI thread and outlive this endpoint.
    pub unsafe fn attach(window: HWND, marker: &[u16]) -> io::Result<Self> {
        if marker.last() != Some(&0) {
            return Err(io::Error::other("unterminated endpoint name"));
        }
        // SAFETY: Only a marker property is stored on the owned GUI HWND; no pointer is exposed.
        if unsafe { SetPropW(window, marker.as_ptr(), 1usize as HANDLE) } == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(Self {
            window,
            marker: marker.to_vec(),
            _thread: PhantomData,
        })
    }
}
impl Drop for Endpoint {
    fn drop(&mut self) {
        // SAFETY: Remove this endpoint's property only; destruction also removes properties automatically.
        unsafe {
            if IsWindow(self.window) != 0
                && RemovePropW(self.window, self.marker.as_ptr()).is_null()
            {
                eprintln!(
                    "VartaCopy: cannot remove shell endpoint: {}",
                    io::Error::last_os_error()
                );
            }
        }
    }
}

//! Notification icon owned by the GUI thread; no hooks, polling or background process.
use eframe::egui;
use std::{
    cell::{Cell, RefCell},
    io,
    marker::PhantomData,
    rc::Rc,
};
use windows_sys::Win32::{
    Foundation::*,
    System::LibraryLoader::GetModuleHandleW,
    UI::{Shell::*, WindowsAndMessaging::*},
};
const CALLBACK: u32 = WM_APP + 0x473;
const ICON: u32 = 1;
// Shellapi.h: NIN_KEYSELECT is the macro (NIN_SELECT | NINF_KEY).
const KEY_SELECT: u32 = NIN_SELECT | 1;
fn wide(value: &str) -> Vec<u16> {
    value.encode_utf16().chain(std::iter::once(0)).collect()
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Action {
    Show,
    Exit,
}
struct Data {
    window: HWND,
    icon: HICON,
    ctx: egui::Context,
    taskbar_created: u32,
    added: Cell<bool>,
    destroyed: Cell<bool>,
    pending: Cell<Option<Action>>,
    error: RefCell<Option<String>>,
}
impl Data {
    fn notification(&self) -> NOTIFYICONDATAW {
        let mut data = NOTIFYICONDATAW {
            cbSize: std::mem::size_of::<NOTIFYICONDATAW>() as u32,
            hWnd: self.window,
            uID: ICON,
            uFlags: NIF_MESSAGE | NIF_ICON | NIF_TIP | NIF_SHOWTIP,
            uCallbackMessage: CALLBACK,
            hIcon: self.icon,
            ..Default::default()
        };
        let tip = wide("VartaCopy — копіювання та переміщення");
        data.szTip[..tip.len()].copy_from_slice(&tip);
        data
    }
    fn add(&self) -> io::Result<()> {
        let mut data = self.notification();
        // SAFETY: Correctly sized structure, live GUI HWND and owned icon.
        if unsafe { Shell_NotifyIconW(NIM_ADD, &data) } == 0 {
            return Err(io::Error::other(
                "Windows не додала значок VartaCopy у трей.",
            ));
        }
        self.added.set(true);
        data.Anonymous.uVersion = NOTIFYICON_VERSION_4;
        // SAFETY: Configure the exact icon just added; no other icons are modified.
        if unsafe { Shell_NotifyIconW(NIM_SETVERSION, &data) } == 0 {
            self.remove();
            return Err(io::Error::other(
                "Windows не підтвердила керування значком трея.",
            ));
        }
        Ok(())
    }
    fn remove(&self) {
        if self.added.replace(false) {
            // SAFETY: Delete only our HWND/uID pair, before destroying its HICON.
            if unsafe { Shell_NotifyIconW(NIM_DELETE, &self.notification()) } == 0 {
                eprintln!(
                    "VartaCopy: notification icon removal was not confirmed (Explorer may have exited)"
                );
            }
        }
    }
    fn show(&self) {
        self.ctx
            .send_viewport_cmd(egui::ViewportCommand::Visible(true));
        self.ctx
            .send_viewport_cmd(egui::ViewportCommand::Minimized(false));
        // SAFETY: Own top-level HWND on its GUI thread. ShowWindow returns PRIOR visibility,
        // not success. Restore natively so even a hidden window can process the repaint.
        unsafe {
            ShowWindow(self.window, SW_RESTORE);
        }
        // SAFETY: Requested only by the person's tray action or recovery of a missing tray.
        if unsafe { SetForegroundWindow(self.window) } == 0 {
            eprintln!("VartaCopy: Windows declined foreground focus; the window was restored");
        }
        self.ctx.send_viewport_cmd(egui::ViewportCommand::Focus);
        self.ctx.request_repaint();
    }
    fn action(&self, action: Action) {
        // Exit has priority over repeated activation events, without an unbounded queue.
        if self.pending.get() != Some(Action::Exit) {
            self.pending.set(Some(action));
        }
        self.show();
    }
    fn fail(&self, error: io::Error) {
        *self.error.borrow_mut() = Some(error.to_string());
        self.show();
    }
    fn menu(&self) -> io::Result<()> {
        // SAFETY: Creates an empty menu owned by this stack frame.
        let menu = unsafe { CreatePopupMenu() };
        if menu.is_null() {
            return Err(io::Error::last_os_error());
        }
        struct Menu(HMENU);
        impl Drop for Menu {
            fn drop(&mut self) {
                // SAFETY: This menu is uniquely owned and no longer being tracked.
                if unsafe { DestroyMenu(self.0) } == 0 {
                    eprintln!(
                        "VartaCopy: tray menu destruction failed: {}",
                        io::Error::last_os_error()
                    );
                }
            }
        }
        let menu = Menu(menu);
        for (id, label) in [(1, "&Відкрити VartaCopy"), (2, "&Вийти")] {
            // SAFETY: Live menu, constant command ID, terminated label copied by Windows.
            if unsafe { AppendMenuW(menu.0, MF_STRING, id, wide(label).as_ptr()) } == 0 {
                return Err(io::Error::last_os_error());
            }
        }
        let id = NOTIFYICONIDENTIFIER {
            cbSize: std::mem::size_of::<NOTIFYICONIDENTIFIER>() as u32,
            hWnd: self.window,
            uID: ICON,
            ..Default::default()
        };
        let mut rect = RECT::default();
        // SAFETY: Look up only our icon. Anchoring at the icon also supports keyboard activation.
        let result = unsafe { Shell_NotifyIconGetRect(&id, &mut rect) };
        if result < 0 {
            return Err(io::Error::other(format!(
                "Tray position unavailable: {result:#x}"
            )));
        }
        // SAFETY: Standard notification-menu foreground handshake on the owning GUI thread.
        if unsafe { SetForegroundWindow(self.window) } == 0 {
            eprintln!("VartaCopy: tray menu foreground request declined");
        }
        // SAFETY: Clear the thread's error before a call where zero can also mean cancellation.
        unsafe {
            SetLastError(0);
        }
        // SAFETY: Owned menu and HWND; native modal menu handles mouse, keyboard and Escape.
        let selected = unsafe {
            TrackPopupMenuEx(
                menu.0,
                TPM_RETURNCMD | TPM_NONOTIFY | TPM_RIGHTBUTTON,
                rect.left,
                rect.bottom,
                self.window,
                std::ptr::null(),
            )
        };
        let error = io::Error::last_os_error();
        // SAFETY: Required post-menu handshake for notification menus; no file operation.
        if unsafe { PostMessageW(self.window, WM_NULL, 0, 0) } == 0 {
            return Err(io::Error::last_os_error());
        }
        match selected {
            1 => self.action(Action::Show),
            2 => self.action(Action::Exit),
            0 if error.raw_os_error() != Some(0) => return Err(error),
            _ => {}
        }
        // SAFETY: Return keyboard focus to this notification icon after menu dismissal.
        if unsafe { Shell_NotifyIconW(NIM_SETFOCUS, &self.notification()) } == 0 {
            eprintln!("VartaCopy: tray focus return was not confirmed");
        }
        Ok(())
    }
}
impl Drop for Data {
    fn drop(&mut self) {
        self.remove();
        // SAFETY: LoadImage without LR_SHARED returns an owned icon, retained until removal.
        if unsafe { DestroyIcon(self.icon) } == 0 {
            eprintln!(
                "VartaCopy: tray icon destruction failed: {}",
                io::Error::last_os_error()
            );
        }
    }
}
pub(super) struct Tray {
    data: Option<Box<Data>>,
    _thread: PhantomData<Rc<()>>,
}
impl Tray {
    pub fn attach(window: HWND, ctx: egui::Context) -> io::Result<Self> {
        // SAFETY: Query the current module, whose embedded icon has resource ID 1.
        let module = unsafe { GetModuleHandleW(std::ptr::null()) };
        if module.is_null() {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: Existing module/icon group; Windows chooses from our multiple icon sizes.
        // User32 avoids requiring a process-wide Common Controls activation context.
        let icon = unsafe {
            LoadImageW(
                module,
                1usize as _,
                IMAGE_ICON,
                GetSystemMetrics(SM_CXSMICON),
                GetSystemMetrics(SM_CYSMICON),
                LR_DEFAULTCOLOR,
            )
        } as HICON;
        if icon.is_null() {
            return Err(io::Error::last_os_error());
        }
        let mut data = Box::new(Data {
            window,
            icon,
            ctx,
            taskbar_created: 0,
            added: Cell::new(false),
            destroyed: Cell::new(false),
            pending: Cell::new(None),
            error: RefCell::new(None),
        });
        // SAFETY: Register a notification name only; no broadcast or Explorer restart is performed.
        data.taskbar_created = unsafe { RegisterWindowMessageW(wide("TaskbarCreated").as_ptr()) };
        if data.taskbar_created == 0 {
            return Err(io::Error::last_os_error());
        }
        let ptr = (&*data as *const Data) as usize;
        // SAFETY: Stable Box, attached and removed on this same GUI thread.
        if unsafe { SetWindowSubclass(window, Some(callback), ptr, ptr) } == 0 {
            return Err(io::Error::other("Не вдалося підключити керування треєм."));
        }
        let tray = Self {
            data: Some(data),
            _thread: PhantomData,
        };
        tray.data.as_ref().unwrap().add()?;
        Ok(tray)
    }
    pub fn available(&self) -> bool {
        self.data.as_ref().is_some_and(|d| d.added.get())
    }
    pub fn take_action(&self) -> Option<Action> {
        self.data.as_ref().and_then(|d| d.pending.take())
    }
    pub fn take_error(&self) -> Option<String> {
        self.data.as_ref().and_then(|d| d.error.borrow_mut().take())
    }
}
impl Drop for Tray {
    fn drop(&mut self) {
        if let Some(data) = self.data.take() {
            data.remove();
            let ptr = (&*data as *const Data) as usize;
            if !data.destroyed.get()
                // SAFETY: Callback allocation is kept alive until successful removal on its UI thread.
                && unsafe {RemoveWindowSubclass(data.window,Some(callback),ptr)}==0
            {
                eprintln!("VartaCopy: tray callback removal failed; retaining live callback data");
                Box::leak(data);
            }
        }
    }
}
unsafe extern "system" fn callback(
    window: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
    id: usize,
    reference: usize,
) -> LRESULT {
    // SAFETY: Stable registration allocation; removal and destruction are bound to the GUI thread.
    let data = unsafe { &*(reference as *const Data) };
    let handled = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        if message == data.taskbar_created {
            data.added.set(false); // Explorer lost the old icon along with its taskbar.
            if let Err(error) = data.add() {
                data.fail(error);
            }
        } else if message == CALLBACK {
            match (lparam as u32) & 0xffff {
                NIN_SELECT | KEY_SELECT => data.action(Action::Show),
                WM_CONTEXTMENU => {
                    if let Err(error) = data.menu() {
                        data.fail(error);
                    }
                }
                _ => {}
            }
            return true;
        } else if message == WM_NCDESTROY {
            data.remove();
            data.destroyed.set(true);
            // SAFETY: Remove only this callback before the HWND finishes destruction.
            if unsafe { RemoveWindowSubclass(window, Some(callback), id) } == 0 {
                eprintln!("VartaCopy: tray callback removal during destruction failed");
            }
        }
        false
    }));
    match handled {
        Ok(true) => return 0,
        Ok(false) => {}
        Err(_) => eprintln!("VartaCopy: tray callback panicked"),
    }
    // SAFETY: Preserve the underlying GUI and every other subclass's message processing.
    unsafe { DefSubclassProc(window, message, wparam, lparam) }
}

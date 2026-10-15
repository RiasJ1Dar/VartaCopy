//! Read system accessibility preferences and observe native notifications without polling.
use super::*;
use std::{
    io,
    marker::PhantomData,
    rc::Rc,
    sync::atomic::{AtomicBool, Ordering},
};
use windows_sys::Win32::{
    Foundation::{HWND, LPARAM, LRESULT, WPARAM},
    Graphics::Gdi::*,
    UI::{
        Accessibility::*,
        Shell::{DefSubclassProc, RemoveWindowSubclass, SetWindowSubclass},
        WindowsAndMessaging::*,
    },
};
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct Palette {
    pub window: egui::Color32,
    pub text: egui::Color32,
    pub button: egui::Color32,
    pub button_text: egui::Color32,
    pub selected: egui::Color32,
    pub selected_text: egui::Color32,
    pub link: egui::Color32,
}
fn color(value: u32) -> egui::Color32 {
    egui::Color32::from_rgb(value as u8, (value >> 8) as u8, (value >> 16) as u8)
}
impl Palette {
    pub fn read() -> io::Result<Option<Self>> {
        let mut settings = HIGHCONTRASTW {
            cbSize: std::mem::size_of::<HIGHCONTRASTW>() as u32,
            ..Default::default()
        };
        // SAFETY: SPI_GETHIGHCONTRAST writes the initialized, correctly sized output structure.
        if unsafe {
            SystemParametersInfoW(
                SPI_GETHIGHCONTRAST,
                settings.cbSize,
                (&mut settings as *mut HIGHCONTRASTW).cast(),
                0,
            )
        } == 0
        {
            return Err(io::Error::last_os_error());
        }
        if settings.dwFlags & HCF_HIGHCONTRASTON == 0 {
            return Ok(None);
        }
        // SAFETY: GetSysColor reads process-independent system colors; all indexes are documented constants.
        unsafe {
            Ok(Some(Self {
                window: color(GetSysColor(COLOR_WINDOW)),
                text: color(GetSysColor(COLOR_WINDOWTEXT)),
                button: color(GetSysColor(COLOR_BTNFACE)),
                button_text: color(GetSysColor(COLOR_BTNTEXT)),
                selected: color(GetSysColor(COLOR_HIGHLIGHT)),
                selected_text: color(GetSysColor(COLOR_HIGHLIGHTTEXT)),
                link: color(GetSysColor(COLOR_HOTLIGHT)),
            }))
        }
    }
    pub fn apply(self, style: &mut egui::Style) {
        let v = &mut style.visuals;
        v.override_text_color = None;
        v.weak_text_color = Some(self.text);
        v.panel_fill = self.window;
        v.window_fill = self.window;
        v.extreme_bg_color = self.window;
        v.faint_bg_color = self.window;
        v.text_edit_bg_color = Some(self.window);
        v.code_bg_color = self.window;
        v.hyperlink_color = self.link;
        v.error_fg_color = self.text;
        v.warn_fg_color = self.text;
        v.selection.bg_fill = self.selected;
        v.selection.stroke = egui::Stroke::new(2.0, self.selected_text);
        v.window_stroke = egui::Stroke::new(2.0, self.text);
        v.window_shadow = egui::epaint::Shadow::NONE;
        v.popup_shadow = egui::epaint::Shadow::NONE;
        v.disabled_alpha = 1.0;
        for w in [
            &mut v.widgets.noninteractive,
            &mut v.widgets.inactive,
            &mut v.widgets.hovered,
            &mut v.widgets.active,
            &mut v.widgets.open,
        ] {
            w.bg_fill = self.button;
            w.weak_bg_fill = self.button;
            w.bg_stroke = egui::Stroke::new(1.0, self.button_text);
            w.fg_stroke = egui::Stroke::new(2.0, self.button_text);
        }
        v.widgets.noninteractive.bg_fill = self.window;
        v.widgets.noninteractive.weak_bg_fill = self.window;
        v.widgets.noninteractive.bg_stroke = egui::Stroke::new(1.0, self.text);
        v.widgets.noninteractive.fg_stroke = egui::Stroke::new(2.0, self.text);
        for w in [
            &mut v.widgets.hovered,
            &mut v.widgets.active,
            &mut v.widgets.open,
        ] {
            w.bg_fill = self.selected;
            w.weak_bg_fill = self.selected;
            w.fg_stroke = egui::Stroke::new(2.0, self.selected_text);
        }
    }
}
pub(super) fn apply(ctx: &egui::Context, bases: &[egui::Style; 2], palette: Option<Palette>) {
    for (theme, base) in [egui::Theme::Light, egui::Theme::Dark]
        .into_iter()
        .zip(bases)
    {
        let mut style = base.clone();
        if let Some(p) = palette {
            p.apply(&mut style);
        }
        ctx.set_style_of(theme, style);
    }
}
struct Data {
    ctx: egui::Context,
    changed: AtomicBool,
    destroyed: AtomicBool,
    inbox: Option<mpsc::SyncSender<Vec<PathBuf>>>,
}
/// Callback registration and destruction are restricted to the owning UI thread.
pub(super) struct Watch {
    window: HWND,
    data: Option<Box<Data>>,
    _thread: PhantomData<Rc<()>>,
    endpoint: Option<vartacopy_shell_windows::instance::Endpoint>,
}
impl Watch {
    pub fn attach(window: HWND, ctx: egui::Context) -> io::Result<Self> {
        let data = Box::new(Data {
            ctx,
            changed: AtomicBool::new(true),
            destroyed: AtomicBool::new(false),
            inbox: None,
        });
        let ptr = (&*data as *const Data) as usize;
        // SAFETY: Called on the HWND's UI thread. Box allocation remains stable until callback removal.
        if unsafe { SetWindowSubclass(window, Some(changed), ptr, ptr) } == 0 {
            return Err(io::Error::other(
                "cannot observe system accessibility settings",
            ));
        }
        Ok(Self {
            window,
            data: Some(data),
            _thread: PhantomData,
            endpoint: None,
        })
    }
    pub fn shell(&mut self, marker: &[u16]) -> io::Result<mpsc::Receiver<Vec<PathBuf>>> {
        let (tx, rx) = mpsc::sync_channel(32);
        let data = self
            .data
            .as_mut()
            .ok_or_else(|| io::Error::other("window callback is unavailable"))?;
        data.inbox = Some(tx);
        // SAFETY: Watch is bound to the calling UI thread and removed before the HWND is destroyed.
        self.endpoint = Some(unsafe {
            vartacopy_shell_windows::instance::Endpoint::attach(self.window, marker)
        }?);
        Ok(rx)
    }
    pub fn take_changed(&self) -> bool {
        self.data
            .as_ref()
            .is_some_and(|d| d.changed.swap(false, Ordering::AcqRel))
    }
}
impl Drop for Watch {
    fn drop(&mut self) {
        self.endpoint.take();
        if let Some(data) = self.data.take() {
            let ptr = (&*data as *const Data) as usize;
            if !data.destroyed.load(Ordering::Acquire)
                // SAFETY: Watch is !Send/!Sync and is dropped on the HWND's thread before freeing callback data.
                && unsafe { RemoveWindowSubclass(self.window, Some(changed), ptr) } == 0
            {
                // A live callback must never receive a freed pointer, even if Win32 cannot remove it.
                eprintln!(
                    "VartaCopy: native settings callback could not be removed; retaining callback data"
                );
                Box::leak(data);
            }
        }
    }
}
unsafe extern "system" fn changed(
    window: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
    id: usize,
    data: usize,
) -> LRESULT {
    // SAFETY: Registration owns this allocation until removal or WM_NCDESTROY on this same UI thread.
    let data = unsafe { &*(data as *const Data) };
    if message == WM_COPYDATA {
        let accepted = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            use vartacopy_shell_windows::{COPYDATA_MAGIC, MAX_REQUEST_BYTES, decode};
            use windows_sys::Win32::System::DataExchange::COPYDATASTRUCT;
            if lparam == 0 {
                return false;
            }
            // SAFETY: WM_COPYDATA supplies a Windows-marshaled COPYDATASTRUCT for the duration of this call.
            let request = unsafe { &*(lparam as *const COPYDATASTRUCT) };
            if request.dwData != COPYDATA_MAGIC
                || request.cbData as usize > MAX_REQUEST_BYTES
                || request.lpData.is_null()
            {
                return false;
            }
            // SAFETY: The OS copies exactly cbData bytes and keeps that buffer valid during this message.
            let bytes = unsafe {
                std::slice::from_raw_parts(request.lpData as *const u8, request.cbData as usize)
            };
            match decode(bytes) {
                Ok(paths) => {
                    if let Some(inbox) = &data.inbox {
                        if inbox.try_send(paths).is_ok() {
                            data.ctx.request_repaint();
                            true
                        } else {
                            false
                        }
                    } else {
                        false
                    }
                }
                Err(error) => {
                    eprintln!("VartaCopy: rejected shell request: {error}");
                    false
                }
            }
        }))
        .unwrap_or_else(|_| {
            eprintln!("VartaCopy: shell request handler panicked");
            false
        });
        return isize::from(accepted);
    }
    if matches!(
        message,
        WM_SETTINGCHANGE | WM_SYSCOLORCHANGE | WM_THEMECHANGED
    ) {
        data.changed.store(true, Ordering::Release);
        // Rust must never unwind through a native window procedure.
        if std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| data.ctx.request_repaint()))
            .is_err()
        {
            eprintln!("VartaCopy: could not wake UI after system accessibility settings changed");
        }
    }
    if message == WM_NCDESTROY {
        data.destroyed.store(true, Ordering::Release);
        // SAFETY: Remove exactly this callback from the window currently being destroyed.
        if unsafe { RemoveWindowSubclass(window, Some(changed), id) } == 0 {
            eprintln!("VartaCopy: settings callback removal during window destruction failed");
        }
    }
    // SAFETY: Preserve the original processing and every other subclass, including native destruction.
    unsafe { DefSubclassProc(window, message, wparam, lparam) }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn palette_keeps_system_color_pairs_and_restores_both_themes() {
        assert_eq!(color(0x00332211), egui::Color32::from_rgb(0x11, 0x22, 0x33));
        let ctx = egui::Context::default();
        let bases = [
            (*ctx.style_of(egui::Theme::Light)).clone(),
            (*ctx.style_of(egui::Theme::Dark)).clone(),
        ];
        let p = Palette {
            window: egui::Color32::BLACK,
            text: egui::Color32::WHITE,
            button: egui::Color32::from_rgb(40, 20, 60),
            button_text: egui::Color32::YELLOW,
            selected: egui::Color32::from_rgb(0, 255, 255),
            selected_text: egui::Color32::BLACK,
            link: egui::Color32::GREEN,
        };
        apply(&ctx, &bases, Some(p));
        for theme in [egui::Theme::Light, egui::Theme::Dark] {
            let s = ctx.style_of(theme);
            let v = &s.visuals;
            assert_eq!(v.panel_fill, p.window);
            assert_eq!(v.text_color(), p.text);
            assert_eq!(v.weak_text_color(), p.text);
            assert_eq!(v.widgets.inactive.bg_fill, p.button);
            assert_eq!(v.widgets.inactive.fg_stroke.color, p.button_text);
            assert_eq!(v.widgets.active.bg_fill, p.selected);
            assert_eq!(v.widgets.active.fg_stroke.color, p.selected_text);
            assert_eq!(v.selection.bg_fill, p.selected);
            assert_eq!(v.selection.stroke.color, p.selected_text);
            assert_eq!(v.disabled_alpha, 1.0);
        }
        apply(&ctx, &bases, None);
        for (theme, base) in [egui::Theme::Light, egui::Theme::Dark]
            .into_iter()
            .zip(&bases)
        {
            assert_eq!(
                ctx.style_of(theme).visuals.panel_fill,
                base.visuals.panel_fill
            );
        }
    }
    #[test]
    fn native_notifications_wake_once_and_drop_removes_callback() {
        let class: Vec<u16> = "STATIC\0".encode_utf16().collect();
        // SAFETY: Predefined system class, hidden test HWND on this thread, no borrowed creation data.
        let window = unsafe {
            CreateWindowExW(
                0,
                class.as_ptr(),
                std::ptr::null(),
                0,
                0,
                0,
                100,
                100,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null(),
            )
        };
        assert!(!window.is_null());
        let watch = Watch::attach(window, egui::Context::default()).unwrap();
        assert!(watch.take_changed());
        assert!(!watch.take_changed());
        // SAFETY: Synchronous messages target only our test HWND on the same thread; no pointer payloads.
        unsafe {
            SendMessageW(window, WM_SYSCOLORCHANGE, 0, 0);
        }
        assert!(watch.take_changed());
        assert!(!watch.take_changed());
        drop(watch);
        // SAFETY: Destroy only the test HWND after its callback has been removed.
        assert_ne!(unsafe { DestroyWindow(window) }, 0);
        assert!(Palette::read().is_ok());
    }
    #[test]
    fn secondary_launch_delivers_native_paths_to_one_bounded_inbox() {
        use vartacopy_shell_windows::instance::Instance;
        let root = tempfile::tempdir().unwrap();
        let journal = root.path().join("ipc.sqlite");
        let primary = Instance::enter(&journal, &[]).unwrap().unwrap();
        let class: Vec<u16> = "STATIC\0".encode_utf16().collect();
        // SAFETY: Hidden predefined class, owned test HWND on this thread, no borrowed creation data.
        let window = unsafe {
            CreateWindowExW(
                0,
                class.as_ptr(),
                std::ptr::null(),
                0,
                0,
                0,
                20,
                20,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null(),
            )
        };
        assert!(!window.is_null());
        let mut watch = Watch::attach(window, egui::Context::default()).unwrap();
        let inbox = watch.shell(primary.marker()).unwrap();
        let selected = vec![
            root.path().join("дані з пробілом"),
            root.path().join("second"),
        ];
        use std::os::windows::process::CommandExt;
        let mut client = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--ignored",
                "--exact",
                "app::system_style::tests::shell_client_probe",
            ])
            .env("VARTACOPY_TEST_JOURNAL", &journal)
            .env("VARTACOPY_TEST_SOURCE", &selected[0])
            .env("VARTACOPY_TEST_SECOND", &selected[1])
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .creation_flags(0x08000000)
            .spawn()
            .unwrap();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(8);
        while client.try_wait().unwrap().is_none() {
            let mut msg = MSG::default();
            // SAFETY: Pump only the hidden test window, including synchronous WM_COPYDATA sent by the client thread.
            unsafe {
                while PeekMessageW(&mut msg, window, 0, 0, PM_REMOVE) != 0 {
                    TranslateMessage(&msg);
                    DispatchMessageW(&msg);
                }
            }
            assert!(std::time::Instant::now() < deadline, "IPC client timed out");
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
        let result = client.wait_with_output().unwrap();
        assert!(
            result.status.success(),
            "child failed: {}",
            String::from_utf8_lossy(&result.stderr)
        );
        assert_eq!(inbox.try_recv().unwrap(), selected);
        assert!(inbox.try_recv().is_err());
        drop(watch);
        // SAFETY: Destroy our hidden test window after removing its endpoint and callback.
        assert_ne!(unsafe { DestroyWindow(window) }, 0);
        drop(primary);
    }
    #[test]
    #[ignore = "subprocess probe, executed by secondary_launch test"]
    fn shell_client_probe() {
        use vartacopy_shell_windows::instance::Instance;
        let journal =
            PathBuf::from(std::env::var_os("VARTACOPY_TEST_JOURNAL").expect("probe journal"));
        let paths = vec![
            PathBuf::from(std::env::var_os("VARTACOPY_TEST_SOURCE").expect("probe source")),
            PathBuf::from(std::env::var_os("VARTACOPY_TEST_SECOND").expect("probe second source")),
        ];
        assert!(Instance::enter(&journal, &paths).unwrap().is_none());
    }
}

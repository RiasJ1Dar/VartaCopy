//! A small modal chooser with native dialog navigation and native accessible controls.
//! No process-wide theme, hooks, transfer execution, or changes to the host's DPI mode.
use super::choice::Choice;
use std::{
    ffi::c_void,
    mem::size_of,
    panic::{AssertUnwindSafe, catch_unwind},
};
use vartacopy_shell_windows::transfer::{TransferOperation, TransferRequest};
use windows::{
    Win32::{
        Foundation::{COLORREF, E_FAIL, HINSTANCE, HWND, LPARAM, RECT, WPARAM},
        Graphics::{
            Dwm::{DWMWA_USE_IMMERSIVE_DARK_MODE, DwmSetWindowAttribute},
            Gdi::*,
        },
        System::{LibraryLoader::GetModuleHandleW, Threading::GetCurrentProcessId},
        UI::{
            Accessibility::{HCF_HIGHCONTRASTON, HIGHCONTRASTW},
            Controls::{DRAWITEMSTRUCT, ODS_FOCUS, ODS_SELECTED},
            Input::KeyboardAndMouse::{GetFocus, SetFocus},
            WindowsAndMessaging::*,
        },
    },
    core::{BOOL, Result},
};

const EXPLORER: u16 = 100;
const VARTA: u16 = 101;
const CANCEL: u16 = 2;
const TITLE: i32 = 200;
const SOURCE: i32 = 202;
const DESTINATION: i32 = 204;
const WIDTH: i16 = 280;
const HEIGHT: i16 = 228;
// SDK macros absent from windows metadata: DWLP_USER = sizeof(LRESULT) +
// sizeof(DLGPROC), unlike the obsolete 32-bit DWL_USER offset.
const DWLP_USER: WINDOW_LONG_PTR_INDEX = WINDOW_LONG_PTR_INDEX((2 * size_of::<isize>()) as i32);
const SS_LEFT: u32 = 0;

const fn rgb(r: u32, g: u32, b: u32) -> COLORREF {
    COLORREF(r | (g << 8) | (b << 16))
}
#[derive(Clone, Copy)]
struct Palette {
    background: COLORREF,
    surface: COLORREF,
    pressed: COLORREF,
    text: COLORREF,
    muted: COLORREF,
    accent: COLORREF,
    border: COLORREF,
}
impl Palette {
    fn dark() -> Self {
        Self {
            background: rgb(24, 26, 31),
            surface: rgb(36, 40, 48),
            pressed: rgb(48, 57, 70),
            text: rgb(242, 244, 248),
            muted: rgb(181, 190, 203),
            accent: rgb(115, 191, 255),
            border: rgb(115, 129, 148),
        }
    }
    fn system() -> Self {
        // SAFETY: All indexes are documented, read-only system color queries.
        unsafe {
            Self {
                background: COLORREF(GetSysColor(COLOR_WINDOW)),
                surface: COLORREF(GetSysColor(COLOR_BTNFACE)),
                pressed: COLORREF(GetSysColor(COLOR_HIGHLIGHT)),
                text: COLORREF(GetSysColor(COLOR_WINDOWTEXT)),
                muted: COLORREF(GetSysColor(COLOR_WINDOWTEXT)),
                accent: COLORREF(GetSysColor(COLOR_HOTLIGHT)),
                border: COLORREF(GetSysColor(COLOR_WINDOWTEXT)),
            }
        }
    }
}
pub fn high_contrast() -> Result<bool> {
    let mut settings = HIGHCONTRASTW {
        cbSize: size_of::<HIGHCONTRASTW>() as u32,
        ..Default::default()
    };
    // SAFETY: The documented query writes exactly the sized output structure.
    unsafe {
        SystemParametersInfoW(
            SPI_GETHIGHCONTRAST,
            settings.cbSize,
            Some((&mut settings as *mut HIGHCONTRASTW).cast()),
            SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS(0),
        )?;
    }
    Ok(settings.dwFlags.contains(HCF_HIGHCONTRASTON))
}

struct Brush(HBRUSH);
impl Brush {
    fn new(color: COLORREF) -> Result<Self> {
        // SAFETY: Create an owned, solid GDI brush; checked for allocation failure.
        let brush = unsafe { CreateSolidBrush(color) };
        if brush.is_invalid() {
            return Err(E_FAIL.into());
        }
        Ok(Self(brush))
    }
}
impl Drop for Brush {
    fn drop(&mut self) {
        // SAFETY: No brush remains selected into a DC; each object is uniquely owned.
        if !unsafe { DeleteObject(self.0.into()) }.as_bool() {
            eprintln!("VartaCopy chooser brush release failed");
        }
    }
}
struct Font(HFONT);
impl Drop for Font {
    fn drop(&mut self) {
        // SAFETY: Dialog controls have been destroyed before these owned fonts are dropped.
        if !unsafe { DeleteObject(self.0.into()) }.as_bool() {
            eprintln!("VartaCopy chooser font release failed");
        }
    }
}
struct State {
    palette: Palette,
    background: Brush,
    surface: Brush,
    heading: Option<Font>,
    button_title: Option<Font>,
    failure: Option<windows::core::Error>,
}
impl State {
    fn new() -> Result<Self> {
        let palette = Palette::dark();
        Ok(Self {
            palette,
            background: Brush::new(palette.background)?,
            surface: Brush::new(palette.surface)?,
            heading: None,
            button_title: None,
            failure: None,
        })
    }
}

// DLGTEMPLATE and every DLGITEMTEMPLATE begin on a DWORD boundary. Store DWORDs,
// rather than casting Vec<u8> (whose allocation need not have the required alignment).
struct Template(Vec<u16>);
impl Template {
    fn word(&mut self, value: u16) {
        self.0.push(value);
    }
    fn dword(&mut self, value: u32) {
        self.word(value as u16);
        self.word((value >> 16) as u16);
    }
    fn text(&mut self, value: &str) {
        self.0.extend(value.encode_utf16().chain(Some(0)));
    }
    fn item(&mut self, id: u16, class: u16, style: u32, rect: [i16; 4], text: &str) {
        if !self.0.len().is_multiple_of(2) {
            self.word(0);
        }
        self.dword(style | WS_CHILD.0 | WS_VISIBLE.0);
        self.dword(0);
        for value in rect {
            self.word(value as u16);
        }
        self.word(id);
        self.word(0xffff);
        self.word(class);
        self.text(text);
        self.word(0);
    }
}
fn template(request: &TransferRequest) -> Vec<u32> {
    let mut t = Template(Vec::new());
    t.dword(
        WS_POPUP.0
            | WS_CAPTION.0
            | WS_SYSMENU.0
            | DS_SETFONT as u32
            | DS_MODALFRAME as u32
            | DS_CENTER as u32,
    );
    t.dword(0);
    t.word(9);
    for value in [0, 0, WIDTH, HEIGHT] {
        t.word(value as u16);
    }
    t.word(0);
    t.word(0);
    t.text("VartaCopy");
    t.word(9);
    t.text("Segoe UI");
    t.item(
        TITLE as u16,
        0x82,
        SS_LEFT,
        [16, 12, 248, 18],
        match request.operation {
            TransferOperation::Copy => "Копіювати файли",
            TransferOperation::Move => "Перемістити файли",
        },
    );
    t.item(
        201,
        0x82,
        SS_LEFT,
        [16, 35, 248, 10],
        &format!("Джерела · {}", request.sources.len()),
    );
    let paths = request
        .sources
        .iter()
        .map(|p| p.display().to_string())
        .collect::<Vec<_>>()
        .join("\r\n");
    let edit_style =
        ES_MULTILINE as u32 | ES_READONLY as u32 | ES_AUTOVSCROLL as u32 | WS_TABSTOP.0;
    t.item(SOURCE as u16, 0x81, edit_style, [16, 47, 248, 18], &paths);
    t.item(203, 0x82, SS_LEFT, [16, 71, 248, 10], "Призначення");
    t.item(
        DESTINATION as u16,
        0x81,
        edit_style,
        [16, 83, 248, 14],
        &request.destination.display().to_string(),
    );
    t.item(
        EXPLORER,
        0x80,
        BS_OWNERDRAW as u32 | WS_TABSTOP.0,
        [16, 107, 248, 30],
        "Explorer\nСтандартна передача Windows",
    );
    t.item(
        VARTA,
        0x80,
        BS_OWNERDRAW as u32 | WS_TABSTOP.0,
        [16, 145, 248, 30],
        "VartaCopy\nПеревірка файлів і відновлення",
    );
    t.item(
        205,
        0x82,
        SS_LEFT,
        [16, 181, 248, 14],
        "VartaCopy зупиниться, якщо файл уже існує.",
    );
    t.item(
        CANCEL,
        0x80,
        BS_OWNERDRAW as u32 | WS_TABSTOP.0,
        [184, 201, 80, 20],
        "Скасувати",
    );
    if !t.0.len().is_multiple_of(2) {
        t.word(0);
    }
    t.0.chunks_exact(2)
        .map(|words| words[0] as u32 | ((words[1] as u32) << 16))
        .collect()
}

pub fn show(request: &TransferRequest) -> Result<Choice> {
    let template = template(request);
    let mut state = State::new()?;
    // SAFETY: We only parent to a foreground window belonging to this process.
    let owner = unsafe {
        let hwnd = GetForegroundWindow();
        let mut pid = 0;
        GetWindowThreadProcessId(hwnd, Some(&mut pid));
        (pid == GetCurrentProcessId()).then_some(hwnd)
    };
    // SAFETY: The DWORD-aligned template and stack state outlive the synchronous modal
    // loop. Each callback is on this calling apartment; no foreign COM reference moves.
    let result = unsafe {
        DialogBoxIndirectParamW(
            Some(HINSTANCE(GetModuleHandleW(None)?.0)),
            template.as_ptr().cast(),
            owner,
            Some(procedure),
            LPARAM((&mut state as *mut State) as isize),
        )
    };
    if let Some(error) = state.failure {
        return Err(error);
    }
    match result {
        100 => Ok(Choice::Explorer),
        101 => Ok(Choice::VartaCopy),
        2 => Ok(Choice::Cancel),
        -1 => Err(windows::core::Error::from_thread()),
        _ => Err(E_FAIL.into()),
    }
}

unsafe extern "system" fn procedure(hwnd: HWND, message: u32, wp: WPARAM, lp: LPARAM) -> isize {
    // SAFETY: DialogBox owns this invocation. Panics never unwind into Explorer.
    match catch_unwind(AssertUnwindSafe(|| unsafe {
        dispatch(hwnd, message, wp, lp)
    })) {
        Ok(Ok(result)) => result,
        Ok(Err(error)) => {
            eprintln!("VartaCopy chooser failed: {error}");
            // SAFETY: Pointer was installed during WM_INITDIALOG and cleared at teardown.
            unsafe {
                let pointer = GetWindowLongPtrW(hwnd, DWLP_USER) as *mut State;
                if let Some(state) = pointer.as_mut() {
                    state.failure = Some(error);
                }
                if let Err(error) = EndDialog(hwnd, CANCEL as isize) {
                    eprintln!("VartaCopy chooser close failed: {error}");
                }
            }
            1
        }
        Err(_) => {
            eprintln!("VartaCopy chooser callback panicked");
            // SAFETY: Only cancel the current dialog, with no transfer execution.
            unsafe {
                if let Err(error) = EndDialog(hwnd, CANCEL as isize) {
                    eprintln!("VartaCopy chooser panic close failed: {error}");
                }
            }
            1
        }
    }
}

unsafe fn dispatch(hwnd: HWND, message: u32, wp: WPARAM, lp: LPARAM) -> Result<isize> {
    // SAFETY: Pointer originates in show(), remains live through DialogBox return,
    // and is cleared before the dialog is destroyed. No reference spans a nested call.
    unsafe {
        if message == WM_INITDIALOG {
            SetWindowLongPtrW(hwnd, DWLP_USER, lp.0);
            initialize(hwnd, lp.0 as *mut State)?;
            return Ok(0); // We explicitly focus Cancel; Windows must not replace it.
        }
        let pointer = GetWindowLongPtrW(hwnd, DWLP_USER) as *mut State;
        if pointer.is_null() {
            return Ok(0);
        }
        match message {
            WM_COMMAND => {
                let id = (wp.0 & 0xffff) as u16;
                // Only native button activation; Enter elsewhere must not start a transfer.
                if (wp.0 >> 16) == BN_CLICKED as usize && matches!(id, EXPLORER | VARTA | CANCEL) {
                    EndDialog(hwnd, id as isize)?;
                    return Ok(1);
                }
            }
            WM_CLOSE => {
                EndDialog(hwnd, CANCEL as isize)?;
                return Ok(1);
            }
            WM_CTLCOLORDLG => return Ok((*pointer).background.0.0 as isize),
            WM_CTLCOLORSTATIC | WM_CTLCOLOREDIT => {
                let dc = HDC(wp.0 as *mut c_void);
                let control = HWND(lp.0 as *mut c_void);
                let edit = matches!(GetDlgCtrlID(control), SOURCE | DESTINATION);
                let state = &*pointer;
                SetBkColor(
                    dc,
                    if edit {
                        state.palette.surface
                    } else {
                        state.palette.background
                    },
                );
                SetTextColor(
                    dc,
                    if edit || GetDlgCtrlID(control) == TITLE {
                        state.palette.text
                    } else {
                        state.palette.muted
                    },
                );
                return Ok(if edit {
                    state.surface.0.0
                } else {
                    state.background.0.0
                } as isize);
            }
            WM_DRAWITEM => {
                if lp.0 != 0 {
                    draw_button(&*(lp.0 as *const DRAWITEMSTRUCT), &*pointer)?;
                    return Ok(1);
                }
            }
            WM_SETTINGCHANGE | WM_SYSCOLORCHANGE => {
                let palette = if high_contrast()? {
                    Palette::system()
                } else {
                    Palette::dark()
                };
                let background = Brush::new(palette.background)?;
                let surface = Brush::new(palette.surface)?;
                (*pointer).palette = palette;
                (*pointer).background = background;
                (*pointer).surface = surface;
                if !InvalidateRect(Some(hwnd), None, true).as_bool() {
                    return Err(windows::core::Error::from_thread());
                }
            }
            WM_NCDESTROY => {
                SetWindowLongPtrW(hwnd, DWLP_USER, 0);
            }
            _ => {}
        }
        Ok(0)
    }
}

unsafe fn initialize(hwnd: HWND, state: *mut State) -> Result<()> {
    // SAFETY: All handles belong to this live dialog. Fonts are retained in State
    // until after child windows are destroyed. No process-level theme API is used.
    unsafe {
        let dark = BOOL(1);
        if let Err(error) = DwmSetWindowAttribute(
            hwnd,
            DWMWA_USE_IMMERSIVE_DARK_MODE,
            (&dark as *const BOOL).cast(),
            size_of::<BOOL>() as u32,
        ) {
            // Older Windows may lack this cosmetic attribute; client colors still work.
            eprintln!("VartaCopy dark title bar unavailable: {error}");
        }
        let font = HFONT(SendMessageW(hwnd, WM_GETFONT, None, None).0 as *mut c_void);
        let mut description = LOGFONTW::default();
        if GetObjectW(
            font.into(),
            size_of::<LOGFONTW>() as i32,
            Some((&mut description as *mut LOGFONTW).cast()),
        ) == 0
        {
            return Err(E_FAIL.into());
        }
        description.lfWeight = FW_SEMIBOLD.0 as i32;
        let mut button_description = description;
        button_description.lfHeight = button_description.lfHeight * 5 / 4;
        let button_title = CreateFontIndirectW(&button_description);
        if button_title.is_invalid() {
            return Err(E_FAIL.into());
        }
        (*state).button_title = Some(Font(button_title));
        description.lfHeight = description.lfHeight * 3 / 2;
        let heading = CreateFontIndirectW(&description);
        if heading.is_invalid() {
            return Err(E_FAIL.into());
        }
        (*state).heading = Some(Font(heading));
        SendMessageW(
            GetDlgItem(Some(hwnd), TITLE)?,
            WM_SETFONT,
            Some(WPARAM(heading.0 as usize)),
            Some(LPARAM(1)),
        );
        let cancel = GetDlgItem(Some(hwnd), CANCEL as i32)?;
        // SetFocus returns the PREVIOUS window. NULL is valid when no window had
        // focus, although the generated binding reports Err(S_OK). Check the actual
        // postcondition instead of rejecting a successfully focused new dialog.
        let previous = SetFocus(Some(cancel));
        if GetFocus() != cancel {
            return Err(previous.err().unwrap_or_else(|| E_FAIL.into()));
        }
        // Explicit default Cancel also makes Enter on a path field harmless.
        SendMessageW(hwnd, DM_SETDEFID, Some(WPARAM(CANCEL as usize)), None);
        Ok(())
    }
}

unsafe fn draw_button(item: &DRAWITEMSTRUCT, state: &State) -> Result<()> {
    // SAFETY: WM_DRAWITEM supplies a live DC and item rectangle; SaveDC/RestoreDC
    // ensure no selected GDI object or text state escapes back into the host.
    unsafe {
        let saved = SaveDC(item.hDC);
        if saved == 0 {
            return Err(E_FAIL.into());
        }
        let result = paint_button(item, state);
        if !RestoreDC(item.hDC, saved).as_bool() {
            return Err(E_FAIL.into());
        }
        result
    }
}
unsafe fn paint_button(item: &DRAWITEMSTRUCT, state: &State) -> Result<()> {
    // SAFETY: Caller saves/restores the DC. Brushes are never selected, only used
    // directly by FillRect/FrameRect. Text buffers remain live during DrawTextW.
    unsafe {
        let p = state.palette;
        let pressed = item.itemState.0 & ODS_SELECTED.0 != 0;
        let focus = item.itemState.0 & ODS_FOCUS.0 != 0;
        let cancel = item.CtlID == CANCEL as u32;
        let background = Brush::new(if pressed { p.pressed } else { p.surface })?;
        let border = Brush::new(if focus { p.accent } else { p.border })?;
        FillRect(item.hDC, &item.rcItem, background.0);
        FrameRect(item.hDC, &item.rcItem, border.0);
        SetBkMode(item.hDC, TRANSPARENT);
        let mut bounds = item.rcItem;
        let scale = |value: i32| {
            value * (item.rcItem.bottom - item.rcItem.top) / if cancel { 44 } else { 80 }
        };
        let inset = scale(16).max(8);
        bounds.left += inset;
        bounds.right -= inset;
        SetTextColor(
            item.hDC,
            if item.CtlID == VARTA as u32 {
                p.accent
            } else {
                p.text
            },
        );
        if cancel {
            let mut text = "Скасувати".encode_utf16().collect::<Vec<_>>();
            DrawTextW(
                item.hDC,
                &mut text,
                &mut bounds,
                DT_CENTER | DT_VCENTER | DT_SINGLELINE | DT_NOPREFIX,
            );
        } else {
            let (title, subtitle) = if item.CtlID == VARTA as u32 {
                ("VartaCopy", "Перевірка файлів і відновлення")
            } else {
                ("Explorer", "Стандартна передача Windows")
            };
            bounds.top += scale(10);
            let split = bounds.top + scale(27);
            let mut title_rect = RECT {
                bottom: split,
                ..bounds
            };
            let mut title = title.encode_utf16().collect::<Vec<_>>();
            if let Some(font) = &state.button_title {
                SelectObject(item.hDC, font.0.into());
            }
            DrawTextW(
                item.hDC,
                &mut title,
                &mut title_rect,
                DT_SINGLELINE | DT_NOPREFIX,
            );
            let font = SendMessageW(item.hwndItem, WM_GETFONT, None, None);
            SelectObject(item.hDC, HGDIOBJ(font.0 as *mut c_void));
            SetTextColor(item.hDC, p.muted);
            bounds.top = split + scale(3);
            let mut subtitle = subtitle.encode_utf16().collect::<Vec<_>>();
            DrawTextW(
                item.hDC,
                &mut subtitle,
                &mut bounds,
                DT_SINGLELINE | DT_NOPREFIX,
            );
        }
        if focus {
            let mut rect = item.rcItem;
            rect.left += 3;
            rect.top += 3;
            rect.right -= 3;
            rect.bottom -= 3;
            if !DrawFocusRect(item.hDC, &rect).as_bool() {
                return Err(E_FAIL.into());
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn dark_palette_preserves_text_and_focus_contrast() {
        let luminance = |color: COLORREF| {
            let component = |v: u32| {
                let s = v as f64 / 255.0;
                if s <= 0.04045 {
                    s / 12.92
                } else {
                    ((s + 0.055) / 1.055).powf(2.4)
                }
            };
            0.2126 * component(color.0 & 255)
                + 0.7152 * component((color.0 >> 8) & 255)
                + 0.0722 * component((color.0 >> 16) & 255)
        };
        let ratio = |a, b| {
            let a = luminance(a);
            let b = luminance(b);
            (a.max(b) + 0.05) / (a.min(b) + 0.05)
        };
        let p = Palette::dark();
        for background in [p.background, p.surface, p.pressed] {
            for foreground in [p.text, p.muted, p.accent] {
                assert!(ratio(foreground, background) >= 4.5);
            }
        }
        assert!(ratio(p.border, p.surface) >= 3.0);
    }
}

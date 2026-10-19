use vartacopy_shell_windows::transfer::{TransferOperation, TransferRequest};
use windows::{
    Win32::{
        Foundation::{E_FAIL, HWND},
        System::Threading::GetCurrentProcessId,
        UI::{
            Controls::{
                TASKDIALOG_BUTTON, TASKDIALOGCONFIG, TDF_ALLOW_DIALOG_CANCELLATION,
                TDF_SIZE_TO_CONTENT, TDF_USE_COMMAND_LINKS,
            },
            WindowsAndMessaging::{
                GetForegroundWindow, GetWindowThreadProcessId, MB_ICONERROR, MB_OK, MessageBoxW,
            },
        },
    },
    core::{PCWSTR, Result, w},
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Choice {
    Explorer,
    VartaCopy,
    Cancel,
}
pub fn show(request: &TransferRequest) -> Result<Choice> {
    let mut lines = vec![format!("Джерел: {}", request.sources.len())];
    for source in request.sources.iter().take(3) {
        lines.push(source.display().to_string());
    }
    if request.sources.len() > 3 {
        lines.push(format!("…і ще {}", request.sources.len() - 3));
    }
    lines.push(format!("\nПризначення:\n{}", request.destination.display()));
    let content = lines
        .join("\n")
        .chars()
        .take(4096)
        .collect::<String>()
        .encode_utf16()
        .chain(Some(0))
        .collect::<Vec<_>>();
    let buttons = [
        TASKDIALOG_BUTTON {
            nButtonID: 100,
            pszButtonText: w!("Explorer\nСтандартна передача Windows"),
        },
        TASKDIALOG_BUTTON {
            nButtonID: 101,
            pszButtonText: w!("VartaCopy\nПеревірка BLAKE3 та журнал. Наявна назва — помилка."),
        },
        TASKDIALOG_BUTTON {
            nButtonID: 2,
            pszButtonText: w!("Скасувати"),
        },
    ];
    // SAFETY: The returned window is only used as an owner after verifying its PID.
    let owner = unsafe {
        let hwnd = GetForegroundWindow();
        let mut pid = 0;
        GetWindowThreadProcessId(hwnd, Some(&mut pid));
        if pid == GetCurrentProcessId() {
            hwnd
        } else {
            HWND::default()
        }
    };
    let config = TASKDIALOGCONFIG {
        cbSize: std::mem::size_of::<TASKDIALOGCONFIG>() as u32,
        hwndParent: owner,
        dwFlags: TDF_ALLOW_DIALOG_CANCELLATION | TDF_USE_COMMAND_LINKS | TDF_SIZE_TO_CONTENT,
        pszWindowTitle: w!("VartaCopy — вибір обробника"),
        pszMainInstruction: match request.operation {
            TransferOperation::Copy => w!("Копіювати файли"),
            TransferOperation::Move => w!("Перемістити файли"),
        },
        pszContent: PCWSTR(content.as_ptr()),
        cButtons: buttons.len() as u32,
        pButtons: buttons.as_ptr(),
        nDefaultButton: 2,
        ..Default::default()
    };
    let mut selected = 2;
    // SAFETY: Every string and button remains live for the entire modal call.
    unsafe {
        super::controls::Controls::load()?.show(&config, &mut selected)?;
    }
    match selected {
        100 => Ok(Choice::Explorer),
        101 => Ok(Choice::VartaCopy),
        2 => Ok(Choice::Cancel),
        _ => Err(E_FAIL.into()),
    }
}
pub fn error(message: &str) {
    let text = message.encode_utf16().chain(Some(0)).collect::<Vec<_>>();
    // SAFETY: These NUL-terminated strings remain live throughout this modal call.
    unsafe {
        MessageBoxW(
            None,
            PCWSTR(text.as_ptr()),
            w!("VartaCopy — стан передавання запиту"),
            MB_OK | MB_ICONERROR,
        );
    }
}

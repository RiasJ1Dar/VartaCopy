# Explorer command DLL

This crate implements the source-only command from `specs/explorer-v2.json`
(CR-SHELL-004). `build.rs` reads the shared `explorer-identity-v1.json` so the
CLSID, title, and GUI filename match the package's identity.

Build the x64 COM server with:

```text
cargo build -p vartacopy-explorer-command --release --target x86_64-pc-windows-msvc
cargo test -p vartacopy-explorer-command
cargo clippy -p vartacopy-explorer-command --all-targets -- -D warnings
```

`vartacopy_explorer.dll` exports `DllGetClassObject` and `DllCanUnloadNow`.
The class factory rejects aggregation, supplies `IExplorerCommand`/`IUnknown`,
and counts both active objects and balanced server locks. HRESULT callbacks
catch Rust panics. Reference counting has no fallible or unwinding operations.

`GetState(FALSE)` checks the selection count and returns `E_PENDING` for a
single item. Deferred `GetState(TRUE)` and `Invoke` accept only an ordinary
local filesystem folder. They reject roots, virtual items, files, UNC, mixed
selection, offline paths, and reparse points in the selected path or ancestors.
Only path metadata is queried; directory contents are never enumerated.
Metadata checks are a scope guard, not an atomic filesystem identity guarantee.

`Invoke` validates the current selection again. It resolves this DLL's image
through `GetModuleHandleExW(FROM_ADDRESS)` and launches the sibling
`vartacopy-gui.exe` with two separate arguments: `--source` and the native UTF-16
path. No shell is involved and no transfer starts in this DLL. Launch failures
return an HRESULT. The DLL does not wait for the GUI or for copy completion.

`GetTitle` returns a `CoTaskMemAlloc` string. `GetCanonicalName` returns the
command CLSID; `GetFlags` returns `ECF_DEFAULT`. `GetIcon`, `GetToolTip`, and
`EnumSubCommands` initialize their output to null and return `E_NOTIMPL`.
There is no native icon resource in this slice.

Tests use native Shell items and synthetic local files, directories and an NTFS
junction. They do not register a COM class/package, modify certificate trust,
or launch the GUI. Package activation and visual Windows 11 menu acceptance
remain separate checks.

Public API sources:

- [IExplorerCommand](https://learn.microsoft.com/en-us/windows/win32/api/shobjidl_core/nn-shobjidl_core-iexplorercommand)
- [GetState](https://learn.microsoft.com/en-us/windows/win32/api/shobjidl_core/nf-shobjidl_core-iexplorercommand-getstate)
- [GetDisplayName allocation ownership](https://learn.microsoft.com/en-us/windows/win32/api/shobjidl_core/nf-shobjidl_core-ishellitem-getdisplayname)
- [Module lookup by address](https://learn.microsoft.com/en-us/windows/win32/api/libloaderapi/nf-libloaderapi-getmodulehandleexw)
- [Modern packaged Explorer commands](https://learn.microsoft.com/en-us/windows/apps/desktop/modernize/integrate-packaged-app-with-file-explorer)
- [Mount-point fixture layout](https://learn.microsoft.com/en-us/windows-hardware/drivers/ddi/ntifs/ns-ntifs-_reparse_data_buffer)

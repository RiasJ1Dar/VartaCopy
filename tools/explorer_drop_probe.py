"""Explicit scoped registration of the Explorer transfer handler or passive probe.

Defaults to the current user. Machine scope needs elevation and is only for an
authorized diagnostic test. Never edits other extensions, startup or processes.
"""
from __future__ import annotations

import argparse
import ctypes
import json
import os
from pathlib import Path
import winreg

CLSID = "{328DB05E-95E2-4427-9E48-D9F11F57E96C}"
BASE = "Software\\Classes\\CLSID\\" + CLSID
HANDLERS = ["Software\\Classes\\" + cls + "\\shellex\\DragDropHandlers\\" + CLSID
            for cls in ("Directory", "Folder", "*", "Drive", "AllFilesystemObjects")]
HANDLERS.append("Software\\Classes\\Directory\\Background\\shellex\\ContextMenuHandlers\\" + CLSID)
APPROVED = r"Software\Microsoft\Windows\CurrentVersion\Shell Extensions\Approved"
APPROVAL_LABEL = "VartaCopy passive Explorer probe"
VIEW = winreg.KEY_WOW64_64KEY
HIVE = winreg.HKEY_CURRENT_USER


def select_handler(kind: str) -> None:
    global CLSID, BASE, HANDLERS, APPROVAL_LABEL
    CLSID = ("{91DD2708-74C6-4498-87B6-09420A94F66D}" if kind == "transfer"
             else "{328DB05E-95E2-4427-9E48-D9F11F57E96C}")
    APPROVAL_LABEL = ("VartaCopy Explorer transfer" if kind == "transfer"
                      else "VartaCopy passive Explorer probe")
    BASE = "Software\\Classes\\CLSID\\" + CLSID
    HANDLERS = ["Software\\Classes\\" + cls + "\\shellex\\DragDropHandlers\\" + CLSID
                for cls in ("Directory", "Folder", "*", "Drive", "AllFilesystemObjects")]
    if kind == "probe":
        HANDLERS.append("Software\\Classes\\Directory\\Background\\shellex\\ContextMenuHandlers\\" + CLSID)


def native_units(path: Path) -> list[int]:
    if not path.is_absolute() or str(path).startswith("\\\\") or ".." in path.parts:
        raise ValueError("Configuration paths must be absolute local drive paths")
    raw = str(path).encode("utf-16-le", errors="surrogatepass")
    units = [int.from_bytes(raw[i:i + 2], "little") for i in range(0, len(raw), 2)]
    if not units or len(units) > 32767 or 0 in units:
        raise ValueError("Invalid native configuration path")
    return units


def configuration(gui: Path, journal: Path | None) -> bytes:
    if gui.name.lower() != "vartacopy-gui.exe" or not gui.is_file():
        raise ValueError("--gui must name an existing vartacopy-gui.exe")
    payload = {"version": 1, "enabled": True, "gui": native_units(gui),
               "journal": native_units(journal) if journal else None}
    result = json.dumps(payload, separators=(",", ":")).encode("utf-8")
    if len(result) > 512 * 1024:
        raise ValueError("Shell configuration is oversized")
    return result


def values(key: str) -> dict[str, str] | None:
    try:
        with winreg.OpenKey(HIVE, key, 0, winreg.KEY_READ | VIEW) as handle:
            result = {}
            index = 0
            while True:
                try:
                    name, value, kind = winreg.EnumValue(handle, index)
                except OSError as error:
                    if error.winerror == 259:
                        break
                    raise
                if kind != winreg.REG_SZ:
                    raise ValueError(f"Unexpected value type in probe key: {key}")
                result[name] = value
                index += 1
            return result
    except FileNotFoundError:
        return None


def expected(dll: Path) -> dict[str, dict[str, str]]:
    return {BASE: {"": APPROVAL_LABEL},
            BASE + r"\InprocServer32": {"": str(dll), "ThreadingModel": "Apartment"},
            BASE + r"\shellex": {},
            BASE + r"\shellex\MayChangeDefaultMenu": {},
            **{key: {"": CLSID} for key in HANDLERS}}


def approval() -> str | None:
    try:
        with winreg.OpenKey(HIVE, APPROVED, 0, winreg.KEY_READ | VIEW) as key:
            value, kind = winreg.QueryValueEx(key, CLSID)
            if kind != winreg.REG_SZ:
                raise ValueError("Unexpected probe approval value type")
            return value
    except FileNotFoundError:
        return None


def remove_approval() -> None:
    with winreg.OpenKey(HIVE, APPROVED, 0, winreg.KEY_SET_VALUE | VIEW) as key:
        winreg.DeleteValue(key, CLSID)


def snapshot(keys: dict[str, dict[str, str]]) -> dict:
    return {key: values(key) for key in keys}


def notify() -> None:
    # SHCNE_ASSOCCHANGED, SHCNF_IDLIST; void API, no user input or process restart.
    ctypes.windll.shell32.SHChangeNotify(0x08000000, 0, None, None)


def main() -> int:
    global HIVE
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("action", choices=("status", "install", "uninstall"))
    parser.add_argument("--scope", choices=("user", "machine"), default="user")
    parser.add_argument("--handler", choices=("probe", "transfer"), default="probe")
    parser.add_argument("--dll", required=True, type=Path)
    parser.add_argument("--gui", type=Path, help="Transfer handler only: absolute path to vartacopy-gui.exe")
    parser.add_argument("--journal", type=Path, help="Optional separate acceptance-test journal")
    parser.add_argument("--record", required=True, type=Path)
    args = parser.parse_args()
    select_handler(args.handler)
    HIVE = winreg.HKEY_CURRENT_USER if args.scope == "user" else winreg.HKEY_LOCAL_MACHINE
    dll = args.dll.resolve(strict=True)
    if dll.suffix.lower() != ".dll":
        parser.error("--dll must name the compiled probe DLL")
    if not args.record.is_absolute() or args.record.exists():
        parser.error("--record must be a new absolute file")
    if args.handler == "transfer" and args.gui is None and args.action != "status":
        parser.error("Transfer install/uninstall requires --gui to verify configuration ownership")
    config_path = Path(os.environ["LOCALAPPDATA"]) / "VartaCopy" / "explorer-transfer.json"
    if not config_path.is_absolute():
        parser.error("LOCALAPPDATA must be absolute")
    config_bytes = configuration(args.gui, args.journal) if args.gui else None
    config_created = False
    keys = expected(dll)
    before = snapshot(keys)
    approval_before = approval()
    changes = []
    approval_created = False
    report = {"scope": args.scope, "handler": args.handler, "action": args.action, "dll": str(dll), "before": before, "created": changes, "approval_before":approval_before}
    try:
        if args.action == "install":
            if any(value is not None for value in before.values()) or approval_before is not None:
                raise ValueError("Probe registration already exists; inspect it before changing anything")
            try:
                if args.handler == "transfer":
                    config_path.parent.mkdir(parents=True, exist_ok=True)
                    try:
                        with config_path.open("xb") as file:
                            config_created = True
                            file.write(config_bytes)
                            file.flush()
                            os.fsync(file.fileno())
                    except FileExistsError:
                        if config_path.read_bytes() != config_bytes:
                            raise ValueError("Existing transfer configuration differs; refusing to replace it")
                for key, content in keys.items():
                    with winreg.CreateKeyEx(HIVE, key, 0, winreg.KEY_WRITE | VIEW) as handle:
                        changes.append(key)
                        for name, value in content.items():
                            winreg.SetValueEx(handle, name, 0, winreg.REG_SZ, value)
                with winreg.CreateKeyEx(HIVE, APPROVED, 0, winreg.KEY_SET_VALUE | VIEW) as key:
                    winreg.SetValueEx(key, CLSID, 0, winreg.REG_SZ, APPROVAL_LABEL)
                    approval_created = True
                if snapshot(keys) != keys or approval() != APPROVAL_LABEL:
                    raise RuntimeError("Shell registration readback differs from requested values")
            except Exception:
                if approval_created:
                    remove_approval()
                for key in reversed(changes):
                    winreg.DeleteKeyEx(HIVE, key, VIEW)
                if config_created:
                    if config_path.read_bytes() != config_bytes:
                        raise RuntimeError("New configuration changed during rollback; retained for inspection")
                    config_path.unlink()
                raise
            notify()
        elif args.action == "uninstall":
            if before != keys or approval_before != APPROVAL_LABEL:
                raise ValueError("Probe registration differs from this DLL; refusing to delete changed keys")
            if args.handler == "transfer" and config_path.read_bytes() != config_bytes:
                raise ValueError("Transfer configuration differs; refusing to remove changed settings")
            remove_approval()
            for key in reversed(keys):
                winreg.DeleteKeyEx(HIVE, key, VIEW)
            if any(value is not None for value in snapshot(keys).values()):
                raise RuntimeError("Probe registration remains after removal")
            if args.handler == "transfer":
                config_path.unlink()
            notify()
        report["after"] = snapshot(keys)
        report["approval_after"] = approval()
        report["config_created"] = config_created
        report["ok"] = True
    except Exception as error:
        report["error"] = str(error)
        report["after"] = snapshot(keys)
        raise
    finally:
        args.record.write_text(json.dumps(report, ensure_ascii=False, indent=2), encoding="utf-8")
    print(json.dumps({"action": args.action, "ok": True, "record": str(args.record)}))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())

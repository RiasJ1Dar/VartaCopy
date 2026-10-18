"""Explicit per-user registration of the passive Explorer callback probe.

This never changes TeraCopy, the modern VartaCopy package, machine settings,
startup entries or Explorer processes. Run only during an authorized UI test.
"""
from __future__ import annotations

import argparse
import ctypes
import json
from pathlib import Path
import winreg

CLSID = "{328DB05E-95E2-4427-9E48-D9F11F57E96C}"
BASE = "Software\\Classes\\CLSID\\" + CLSID
HANDLERS = ["Software\\Classes\\" + cls + r"\shellex\DragDropHandlers\00VartaCopyProbe"
            for cls in ("Directory", "Folder", "*")]
VIEW = winreg.KEY_WOW64_64KEY


def values(key: str) -> dict[str, str] | None:
    try:
        with winreg.OpenKey(winreg.HKEY_CURRENT_USER, key, 0, winreg.KEY_READ | VIEW) as handle:
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
    return {BASE: {"": "VartaCopy passive Explorer probe"},
            BASE + r"\InprocServer32": {"": str(dll), "ThreadingModel": "Apartment"},
            **{key: {"": CLSID} for key in HANDLERS}}


def snapshot(keys: dict[str, dict[str, str]]) -> dict:
    return {key: values(key) for key in keys}


def notify() -> None:
    # SHCNE_ASSOCCHANGED, SHCNF_IDLIST; void API, no user input or process restart.
    ctypes.windll.shell32.SHChangeNotify(0x08000000, 0, None, None)


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("action", choices=("status", "install", "uninstall"))
    parser.add_argument("--dll", required=True, type=Path)
    parser.add_argument("--record", required=True, type=Path)
    args = parser.parse_args()
    dll = args.dll.resolve(strict=True)
    if dll.suffix.lower() != ".dll":
        parser.error("--dll must name the compiled probe DLL")
    if not args.record.is_absolute() or args.record.exists():
        parser.error("--record must be a new absolute file")
    keys = expected(dll)
    before = snapshot(keys)
    changes = []
    report = {"action": args.action, "dll": str(dll), "before": before, "created": changes}
    try:
        if args.action == "install":
            if any(value is not None for value in before.values()):
                raise ValueError("Probe registration already exists; inspect it before changing anything")
            try:
                for key, content in keys.items():
                    with winreg.CreateKeyEx(winreg.HKEY_CURRENT_USER, key, 0, winreg.KEY_WRITE | VIEW) as handle:
                        changes.append(key)
                        for name, value in content.items():
                            winreg.SetValueEx(handle, name, 0, winreg.REG_SZ, value)
            except Exception:
                for key in reversed(changes):
                    winreg.DeleteKeyEx(winreg.HKEY_CURRENT_USER, key, VIEW)
                raise
            if snapshot(keys) != keys:
                raise RuntimeError("Probe registration readback differs from requested values")
            notify()
        elif args.action == "uninstall":
            if before != keys:
                raise ValueError("Probe registration differs from this DLL; refusing to delete changed keys")
            for key in reversed(keys):
                winreg.DeleteKeyEx(winreg.HKEY_CURRENT_USER, key, VIEW)
            if any(value is not None for value in snapshot(keys).values()):
                raise RuntimeError("Probe registration remains after removal")
            notify()
        report["after"] = snapshot(keys)
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

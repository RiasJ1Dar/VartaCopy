"""Check registered single-item and mixed-selection verbs without opening UI or invoking them."""
from __future__ import annotations
import argparse
import json
from pathlib import Path
import shutil
import subprocess


def check(gui: Path, folder: Path) -> dict:
    gui, folder = gui.resolve(strict=True), folder.resolve(strict=True)
    sample = next(p for p in folder.iterdir() if p.is_file())
    cli = gui.with_name("vartacopy.exe")
    status = subprocess.run([str(cli), "explorer", "status", "--gui", str(gui)],
                            check=True, capture_output=True, text=True, encoding="utf-8")
    if "installed=true" not in status.stdout:
        raise ValueError("This GUI does not own the Explorer registration")
    powershell = shutil.which("pwsh") or shutil.which("powershell")
    if not powershell:
        raise ValueError("PowerShell is required for the Windows Shell API probe")
    script = r'''
$ErrorActionPreference='Stop'
[Console]::InputEncoding=[System.Text.UTF8Encoding]::new($false)
[Console]::OutputEncoding=[System.Text.UTF8Encoding]::new($false)
$request=[Console]::In.ReadToEnd() | ConvertFrom-Json
$shell=New-Object -ComObject Shell.Application
$folder=$shell.Namespace($request.folder)
if ($null -eq $folder) { throw 'Cannot bind fixture folder' }
$item=$folder.ParseName($request.sample)
$single=@($item.Verbs() | ForEach-Object { $_.Name })
$items=$folder.Items()
$mixed=@($items.Verbs() | ForEach-Object { $_.Name })
@{single=$single; mixed=$mixed; count=$items.Count} | ConvertTo-Json -Compress
'''
    result = subprocess.run([powershell, "-NoProfile", "-NonInteractive", "-Command", script],
                            input=json.dumps({"folder": str(folder), "sample": sample.name}),
                            text=True, encoding="utf-8", capture_output=True, check=True, timeout=30)
    data = json.loads(result.stdout.lstrip("\ufeff"))
    label = "Копіювати через VartaCopy…"
    if label not in data["single"] or label not in data["mixed"]:
        raise ValueError("VartaCopy verb missing from single or mixed Windows Shell selection")
    return {"status": "passed", "selection_count": data["count"],
            "single_file_verb": True, "mixed_selection_verb": True, "invoked": False}


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--gui", required=True, type=Path)
    parser.add_argument("--folder", required=True, type=Path)
    args = parser.parse_args()
    print(json.dumps(check(args.gui, args.folder)))

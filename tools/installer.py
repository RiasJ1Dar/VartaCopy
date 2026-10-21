"""Compile Inno Setup from a verified portable archive, without replacing outputs."""
from __future__ import annotations

import argparse
import hashlib
import json
from pathlib import Path, PurePosixPath
import re
import subprocess
import sys
import zipfile

try:
    from tools import package
except ModuleNotFoundError:
    import package

ROOT = Path(__file__).resolve().parents[1]
REQUIRED = {"vartacopy.exe", "vartacopy-gui.exe", "vartacopy_transfer.dll",
            "README.txt", "EXPLORER.txt", "LICENSE.txt", "THIRD-PARTY.json", "vartacopy.ico"}


def sha256(path: Path) -> str:
    with path.open("rb") as stream:
        return hashlib.file_digest(stream, "sha256").hexdigest()


def stage(archive: Path, output: Path, allow_dirty: bool = False) -> dict:
    package.verify(archive)
    with zipfile.ZipFile(archive) as bundle:
        manifest_name = next(n for n in bundle.namelist() if n.count("/") == 1 and n.endswith("/manifest.json"))
        manifest = json.loads(bundle.read(manifest_name))
        if "explorer-transfer-native" not in manifest.get("integrations", []):
            raise ValueError("Installer requires the native integration package; legacy alpha archives are unsupported")
        version = manifest.get("version", "")
        revision = manifest.get("revision", "")
        if not re.fullmatch(r"\d+\.\d+\.\d+(?:-[a-zA-Z0-9.-]+)?", version):
            raise ValueError("Invalid package version")
        if not re.fullmatch(r"[0-9a-f]{40}", revision):
            raise ValueError("Expected the complete Git revision")
        if not isinstance(manifest.get("source_dirty"), bool) or (manifest["source_dirty"] and not allow_dirty):
            raise ValueError("Installer requires committed sources; --allow-dirty is for local checks only")
        files = {entry["path"]: entry for entry in manifest["files"]}
        if not REQUIRED.issubset(files):
            raise ValueError("Installer payload is incomplete")
        # Extract only the product payload; never execute or ship development helpers.
        selected = [name for name in files if name in REQUIRED or name.startswith("licenses/")]
        seen = set()
        for name in selected:
            parts = PurePosixPath(name).parts
            if any(not re.fullmatch(r"[\w .()+@,-]+", p, re.ASCII) or p.endswith((".", " "))
                   or p.split(".")[0].upper() in {"CON", "PRN", "AUX", "NUL", *[f"COM{i}" for i in range(10)], *[f"LPT{i}" for i in range(10)]}
                   for p in parts):
                raise ValueError(f"Unsupported Windows payload name: {name}")
            if name.casefold() in seen:
                raise ValueError("Case-insensitive payload collision")
            seen.add(name.casefold())
        module_hash = files["vartacopy_transfer.dll"]["sha256"]
        shell_directory = f"shell/{version}-{module_hash}"
        output.mkdir(parents=True, exist_ok=False)
        payload = output / "payload"
        payload.mkdir()
        prefix = manifest_name.rsplit("/", 1)[0] + "/"
        for name in selected:
            destination = payload / (shell_directory + "/vartacopy_transfer.dll" if name == "vartacopy_transfer.dll" else name)
            destination.parent.mkdir(parents=True, exist_ok=True)
            with destination.open("xb") as stream:
                stream.write(bundle.read(prefix + name))
        locator = {"version": 1, "dll": shell_directory + "/vartacopy_transfer.dll"}
        (payload / "integration-install.json").write_text(json.dumps(locator) + "\n", encoding="utf-8")
    return {"version": version, "revision": revision, "source_dirty": manifest["source_dirty"],
            "portable_sha256": sha256(archive), "payload": str(payload),
            "shell_directory": shell_directory, "shell_sha256": module_hash}


def build(archive: Path, output: Path, compiler: Path, allow_dirty: bool = False) -> dict:
    compiler = compiler.resolve(strict=True)
    compiler_version = subprocess.run([str(compiler), "--version"], check=True, capture_output=True,
                                      text=True, encoding="utf-8").stdout.strip()
    if not re.search(r"\b7\.[1-9]\d*\.\d+\b", compiler_version):
        raise ValueError("Inno Setup 7.1 or later in the 7.x series is required")
    record = stage(archive.resolve(strict=True), output.resolve(), allow_dirty)
    script = ROOT / "packaging" / "installer.iss"
    subprocess.run([str(compiler), f'/DPayloadDir={record["payload"]}',
                    f'/DAppVersion={record["version"]}',
                    '/DShellDirectory=' + record["shell_directory"].replace("/", "\\"),
                    f'/DShellHash={record["shell_sha256"]}', f'/DBuildOutput={output.resolve()}', str(script)],
                   cwd=ROOT, check=True)
    installer = output / f'VartaCopy-{record["version"]}-windows-x64-setup.exe'
    with installer.open("rb") as stream:
        if stream.read(2) != b"MZ":
            raise ValueError("Compiler did not produce a Windows executable")
    record.update({"status": "compiled", "installer": str(installer.resolve()), "sha256": sha256(installer),
                   "compiler": compiler_version, "compiler_sha256": sha256(compiler),
                   "script_sha256": sha256(script),
                   "files": [{"path": p.relative_to(Path(record["payload"])).as_posix(), "sha256": sha256(p)}
                             for p in sorted(Path(record["payload"]).rglob("*")) if p.is_file()]})
    with installer.with_suffix(".exe.sha256").open("x", encoding="ascii") as stream:
        stream.write(f'{record["sha256"]}  {installer.name}\n')
    with (output / "build-record.json").open("x", encoding="utf-8") as stream:
        json.dump(record, stream, ensure_ascii=False, indent=2)
        stream.write("\n")
    return record


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--portable", required=True, type=Path)
    parser.add_argument("--output", required=True, type=Path)
    parser.add_argument("--iscc", required=True, type=Path)
    parser.add_argument("--allow-dirty", action="store_true")
    args = parser.parse_args()
    record = build(args.portable, args.output, args.iscc, args.allow_dirty)
    print(json.dumps({key: value for key, value in record.items() if key != "files"}, ensure_ascii=True))


if __name__ == "__main__":
    try:
        main()
    except (OSError, ValueError, KeyError, subprocess.CalledProcessError, zipfile.BadZipFile) as failure:
        print(json.dumps({"status": "failed", "error": str(failure)}), file=sys.stderr)
        sys.exit(1)

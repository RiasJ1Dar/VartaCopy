"""Build and verify a local Windows portable archive. Never replaces an output."""
from __future__ import annotations
import argparse
from datetime import datetime, timezone
import hashlib
import json
from pathlib import Path, PurePosixPath
import shutil
import struct
import subprocess
import sys
import zipfile

ROOT = Path(__file__).resolve().parents[1]
TARGET = "x86_64-pc-windows-msvc"


def command(*args: str) -> str:
    result = subprocess.run(args, cwd=ROOT, check=True, text=True, encoding="utf-8", capture_output=True)
    return result.stdout.strip()


def digest(stream) -> str:
    return hashlib.file_digest(stream, "sha256").hexdigest()


def verify(archive: Path) -> dict:
    with zipfile.ZipFile(archive) as bundle:
        names = bundle.namelist()
        if len(set(names)) != len(names):
            raise ValueError("Duplicate ZIP entry")
        for name in names:
            path = PurePosixPath(name)
            if path.is_absolute() or ".." in path.parts or "\\" in name or ":" in name:
                raise ValueError(f"Unsafe ZIP entry: {name}")
        manifests = [n for n in names if n.count("/") == 1 and n.endswith("/manifest.json")]
        if len(manifests) != 1:
            raise ValueError("Expected one package manifest")
        manifest_name = manifests[0]
        manifest = json.loads(bundle.read(manifest_name))
        if manifest.get("format") != "vartacopy-portable" or manifest.get("target") != TARGET:
            raise ValueError("Unrecognized portable package format/target")
        files = manifest["files"]
        required = {"vartacopy.exe", "vartacopy-gui.exe", "README.txt", "THIRD-PARTY.json"}
        paths = [file["path"] for file in files]
        if len(paths) != len(set(paths)) or not required.issubset(paths):
            raise ValueError("Duplicate manifest path or missing required file")
        if len(names) > 5000 or sum(item.file_size for item in bundle.infolist()) > 1024**3:
            raise ValueError("Package exceeds inspection limits")
        prefix = manifest_name.rsplit("/", 1)[0] + "/"
        expected = {manifest_name}
        for file in manifest["files"]:
            name = prefix + file["path"]
            expected.add(name)
            with bundle.open(name) as stream:
                actual = digest(stream)
            if actual != file["sha256"] or bundle.getinfo(name).file_size != file["bytes"]:
                raise ValueError(f"Checksum mismatch: {name}")
        if set(names) != expected:
            raise ValueError("Archive contents differ from manifest")
        corrupt = bundle.testzip()
        if corrupt is not None:
            raise ValueError(f"ZIP CRC failure: {corrupt}")
    return {"status": "verified", "archive": str(archive), "files": len(expected), "revision": manifest["revision"], "source_dirty": manifest["source_dirty"]}


def pe_imports(path: Path) -> list[str]:
    data = path.read_bytes()
    if len(data) > 128 * 1024**2 or data[:2] != b"MZ":
        raise ValueError("Not a supported PE image")
    def u16(offset):
        return struct.unpack_from("<H", data, offset)[0]
    def u32(offset):
        return struct.unpack_from("<I", data, offset)[0]
    pe = u32(0x3C)
    if data[pe:pe+4] != b"PE" + bytes(2):
        raise ValueError("PE signature missing")
    coff = pe + 4
    optional = coff + 20
    if u16(optional) != 0x20B:
        raise ValueError("Expected a PE32+ Windows x64 executable")
    sections = optional + u16(coff + 16)
    def offset(rva):
        for number in range(u16(coff + 2)):
            entry = sections + number * 40
            va, raw_size, raw = u32(entry+12), u32(entry+16), u32(entry+20)
            if va <= rva < va + raw_size and raw + rva - va < len(data):
                return raw + rva - va
        raise ValueError("Import RVA outside file sections")
    table = offset(u32(optional + 120))
    imports = []
    for number in range(512):
        entry = table + number * 20
        if data[entry:entry+20] == bytes(20):
            return sorted(set(imports), key=str.casefold)
        name = offset(u32(entry + 12))
        end = data.find(bytes(1), name, name + 260)
        if end < 0:
            raise ValueError("Unterminated DLL import")
        imports.append(data[name:end].decode("ascii"))
    raise ValueError("Excessive PE import descriptors")


def portable_imports(path: Path) -> list[str]:
    imports = pe_imports(path)
    if any(name.lower().startswith(("vcruntime", "msvcp", "ucrtbased", "api-ms-win-crt")) for name in imports):
        raise ValueError(f"External CRT dependency in {path.name}; build with +crt-static")
    return imports


def dependencies(folder: Path) -> list[dict]:
    metadata = json.loads(command("cargo", "metadata", "--locked", "--format-version", "1", "--filter-platform", TARGET))
    packages = {p["id"]: p for p in metadata["packages"]}
    nodes = {n["id"]: n for n in metadata["resolve"]["nodes"]}
    pending = list(metadata["workspace_members"])
    selected = set()
    while pending:
        package_id = pending.pop()
        if package_id in selected:
            continue
        selected.add(package_id)
        for dependency in nodes[package_id]["deps"]:
            if any(kind["kind"] != "dev" for kind in dependency["dep_kinds"]):
                pending.append(dependency["pkg"])
    notices = []
    for package_id in sorted(selected):
        package = packages[package_id]
        if package_id in metadata["workspace_members"]:
            continue
        root = Path(package["manifest_path"]).parent
        # Bundled fonts and native libraries may carry additional notices below
        # the crate root. Preserve their relative paths, including repeated names.
        candidates = {p for p in root.rglob("*") if p.is_file()
                      and (any(word in p.name.upper() for word in ("LICENSE", "LICENCE", "NOTICE", "COPYRIGHT", "COPYING"))
                           or p.name.upper().startswith(("OFL", "UFL"))
                           or ("fonts" in p.relative_to(root).parts and p.suffix.lower() == ".txt"))
                      and p.suffix.lower() not in {".rs", ".c", ".h", ".py"}}
        if package.get("license_file"):
            path = Path(package["license_file"])
            candidates.add(path if path.is_absolute() else root / path)
        files = []
        label = package["name"] + "-" + package["version"]
        for path in sorted(candidates):
            if not path.is_file():
                raise ValueError(f"Declared license file absent for {label}: {path.name}")
            within_crate = path.relative_to(root) if path.is_relative_to(root) else Path(path.name)
            relative = Path("licenses") / label / within_crate
            target = folder / relative
            target.parent.mkdir(parents=True, exist_ok=True)
            shutil.copyfile(path, target)
            files.append(relative.as_posix())
        if not package.get("license") and not files:
            raise ValueError(f"Dependency has no declared license: {label}")
        notices.append({"name": package["name"], "version": package["version"], "license": package.get("license"), "repository": package.get("repository"), "notice_files": files})
    (folder / "THIRD-PARTY.json").write_text(json.dumps(notices, ensure_ascii=False, indent=2) + "\n", encoding="utf-8")
    return notices


def build() -> dict:
    if sys.platform != "win32":
        raise ValueError("Build this package on Windows with the MSVC Rust toolchain")
    # Explicit target and binary names prevent a case-insensitive CLI/GUI collision.
    subprocess.run(["cargo", "build", "--locked", "--release", "--target", TARGET, "--bin", "vartacopy", "--bin", "vartacopy-gui"], cwd=ROOT, check=True)
    metadata = json.loads(command("cargo", "metadata", "--no-deps", "--format-version", "1"))
    package = next(p for p in metadata["packages"] if p["name"] == "vartacopy-desktop")
    version = package["version"]
    revision = command("git", "rev-parse", "HEAD")
    dirty = bool(command("git", "status", "--porcelain"))
    stamp = datetime.now(timezone.utc).strftime("%Y%m%dT%H%M%SZ")
    label = f"VartaCopy-{version}-windows-x64-{revision[:7]}-{stamp}"
    output = ROOT / "artifacts" / "portable" / label
    output.mkdir(parents=True, exist_ok=False)
    binary_root = ROOT / "target" / TARGET / "release"
    imports = {}
    for name in ("vartacopy.exe", "vartacopy-gui.exe"):
        imports[name] = portable_imports(binary_root / name)
        shutil.copyfile(binary_root / name, output / name)
    notices = dependencies(output)
    (output / "README.txt").write_text(f"""VartaCopy {version} — Windows x64, попередня версія

Розпакуйте архів у звичайний каталог і відкрийте vartacopy-gui.exe.
Rust, Python і права адміністратора для запуску не потрібні.

1. Натисніть «Нова операція…» або Ctrl+N. Оберіть джерело й наявне призначення.
2. «Копіювати» запускає чергу; «До черги» лише додає завдання до неї.
3. «Пауза» зберігає прогрес; «Відновити» доступне для вибраної операції в історії.
4. Список показує стан і розмір елементів. Подвійне натискання відкриває подробиці.
5. Меню «Ще…» містить експорт JSON, порядок черги та подробиці операції.
6. «Перевірити» повторно перевіряє готову копію. «Меню» → «Вигляд і правила…»
   перемикає системну, світлу й темну тему.

Журнал: %LOCALAPPDATA%\\VartaCopy\\jobs.sqlite3, на локальному фіксованому диску.
Інший журнал: vartacopy-gui.exe --journal D:\\VartaCopy\\jobs.sqlite3
Батьківський каталог власного журналу має існувати.
Після аварії відкрийте історію та відновіть перервану операцію.
Програма перевіряє раніше записані блоки; це може тривати певний час.
«Зупинити» залишає власні проміжні дані для можливого відновлення.
Джерело не видаляється. Існуючі файли не перезаписуються; каталоги не об’єднуються.

CLI, приклади:
  vartacopy.exe copy "D:\\Джерело" "D:\\Копії" --json
  vartacopy.exe copy "D:\\Джерело" "D:\\Копії" --enqueue --on-conflict keep-both
  vartacopy.exe queue --run --json
  vartacopy.exe jobs --json
  vartacopy.exe show 1 --json
  vartacopy.exe resume 1 --json
  vartacopy.exe export 1 "D:\\Звіти\\job-1.json"
  vartacopy.exe --help

Перевіряються всі підтримані файлові потоки даних (BLAKE3 readback).
Пропуски дають частковий результат. Reparse points за замовчуванням блокуються.
Hardlinks копіюються як незалежні файли. ADS каталогів блокують операцію.
ACL та власник успадковуються від призначення; повна тотожність метаданих
не заявляється. Основні перевірки виконано на локальній NTFS, Windows x64.
Move, overwrite, повні ACL/owner/audit, Explorer integration, filters та
повна матриця USB/SMB/DPI ще розробляються. Це не реліз1.0.

manifest.json містить SHA-256 кожного файла пакета та revision вихідного коду.
THIRD-PARTY.json містить інвентар залежностей; тексти з їхніх пакетів — у licenses/.
""", encoding="utf-8")
    files = []
    for path in sorted(output.rglob("*")):
        if path.is_file():
            with path.open("rb") as stream:
                sha = digest(stream)
            files.append({"path": path.relative_to(output).as_posix(), "bytes": path.stat().st_size, "sha256": sha})
    manifest = {"format": "vartacopy-portable", "version": version, "target": TARGET, "revision": revision, "source_dirty": dirty, "rustc": command("rustc", "--version"), "dependencies": len(notices), "imports": imports, "files": files}
    (output / "manifest.json").write_text(json.dumps(manifest, ensure_ascii=False, indent=2) + "\n", encoding="utf-8")
    archive = output.parent / (label + ".zip")
    with zipfile.ZipFile(archive, "x", compression=zipfile.ZIP_DEFLATED, compresslevel=9) as bundle:
        for path in sorted(output.rglob("*")):
            if path.is_file():
                bundle.write(path, (Path(label) / path.relative_to(output)).as_posix())
    return verify(archive)


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--inspect", type=Path, help="Verify a previously built archive without extracting it")
    args = parser.parse_args()
    result = verify(args.inspect) if args.inspect else build()
    print(json.dumps(result, ensure_ascii=True))


if __name__ == "__main__":
    try:
        main()
    except (OSError, ValueError, KeyError, subprocess.CalledProcessError, zipfile.BadZipFile, struct.error) as failure:
        print(json.dumps({"status": "failed", "error": str(failure)}, ensure_ascii=True), file=sys.stderr)
        sys.exit(1)

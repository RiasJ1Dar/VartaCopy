"""Build a VartaCopy sparse identity package. Deployment is always explicit.

Python stdlib only. SDK tools are invoked with argument arrays, never a shell.
The fixed PowerShell bridge receives data through a JSON environment variable.
"""
from __future__ import annotations

import argparse
import base64
import hashlib
import json
import os
from pathlib import Path
import re
import shutil
import stat
import struct
import subprocess
import sys
import xml.etree.ElementTree as ET
import zipfile
import zlib

ROOT = Path(__file__).resolve().parents[1]
CONTRACT_HASH = "1c76768f206e22527cdf98922f4bf52c4f1855fe116f190cb2cb00c18ece6a6e"
SDK = Path("C:/Program Files (x86)/Windows Kits/10/bin/10.0.26100.0/x64")
NS = {
    "f": "http://schemas.microsoft.com/appx/manifest/foundation/windows10",
    "uap": "http://schemas.microsoft.com/appx/manifest/uap/windows10",
    "uap10": "http://schemas.microsoft.com/appx/manifest/uap/windows10/10",
    "rescap": "http://schemas.microsoft.com/appx/manifest/foundation/windows10/restrictedcapabilities",
    "com": "http://schemas.microsoft.com/appx/manifest/com/windows10",
    "desktop4": "http://schemas.microsoft.com/appx/manifest/desktop/windows10/4",
    "desktop5": "http://schemas.microsoft.com/appx/manifest/desktop/windows10/5",
}
ASM = "urn:schemas-microsoft-com:asm.v1"
MSIX = "urn:schemas-microsoft-com:msix.v1"
RECORD = "build.json"


def sha256(path: Path) -> str:
    return hashlib.sha256(path.read_bytes()).hexdigest()


def identity() -> dict:
    if sha256(ROOT / "specs/explorer-v2.json") != CONTRACT_HASH:
        raise ValueError("Explorer behavior contract hash differs from approved input")
    result = json.loads((ROOT / "specs/explorer-identity-v1.json").read_text(encoding="utf-8"))
    expected = {
        "schema_version": 1, "behavior_sha256": CONTRACT_HASH,
        "package_name": "VartaCopy.Explorer", "publisher": "CN=VartaCopy Development",
        "application_id": "VartaCopy", "architecture": "x64",
        "command_clsid": "c282efce-1e66-41b6-a895-b43bcb86c1fb",
        "command_verb": "VartaCopyAddSource", "item_type": "Directory",
        "dll_filename": "vartacopy_explorer.dll", "gui_filename": "vartacopy-gui.exe",
    }
    for key, value in expected.items():
        if result.get(key) != value:
            raise ValueError(f"Unapproved identity field: {key}")
    if not re.fullmatch(r"\d+\.\d+\.\d+\.\d+", result["package_version"]):
        raise ValueError("Invalid package version")
    return result


def run(argv: list[str], *, env=None) -> str:
    completed = subprocess.run([str(a) for a in argv], check=False, capture_output=True,
                               text=True, encoding="utf-8", errors="replace", env=env,
                               shell=False, timeout=180)
    if completed.returncode:
        raise RuntimeError(f"{Path(argv[0]).name} failed ({completed.returncode}): "
                           f"{completed.stdout.strip()} {completed.stderr.strip()}")
    return completed.stdout.strip()


def sdk_tool(name: str, sdk: Path = SDK) -> str:
    target = (sdk / name).resolve(strict=True)
    if not target.is_file():
        raise ValueError(f"SDK tool is not a file: {target}")
    return str(target)


def local_directory(path: Path, *, must_exist=True) -> Path:
    target = path.resolve(strict=must_exist)
    if str(target).startswith("\\\\"):
        raise ValueError("External payload must be on a local filesystem")
    if (must_exist or target.exists()) and not target.is_dir():
        raise ValueError("Expected an external directory")
    # Junctions/symlinks could change the payload or ownership meaning later.
    for part in [path.absolute(), *path.absolute().parents]:
        try:
            metadata = part.lstat()
        except FileNotFoundError:
            continue
        if (stat.S_ISLNK(metadata.st_mode) or
            getattr(metadata, "st_file_attributes", 0) & stat.FILE_ATTRIBUTE_REPARSE_POINT):
            raise ValueError(f"Reparse path is not supported: {part}")
    return target


def require_x64_pe(path: Path, *, dll: bool) -> None:
    data = path.read_bytes()
    if len(data) < 256 or data[:2] != b"MZ":
        raise ValueError(f"Not a PE image: {path.name}")
    offset = struct.unpack_from("<I", data, 0x3C)[0]
    if offset + 26 > len(data) or data[offset:offset + 4] != b"PE\0\0":
        raise ValueError(f"Invalid PE image: {path.name}")
    machine, = struct.unpack_from("<H", data, offset + 4)
    flags, = struct.unpack_from("<H", data, offset + 22)
    if machine != 0x8664 or bool(flags & 0x2000) != dll:
        raise ValueError(f"Expected x64 {'DLL' if dll else 'EXE'}: {path.name}")


def manifest_xml(i: dict) -> bytes:
    for prefix, uri in NS.items():
        ET.register_namespace("" if prefix == "f" else prefix, uri)
    def tag(prefix, name):
        return f"{{{NS[prefix]}}}{name}"
    def add(parent, prefix, name, attrs=None, value=None):
        child = ET.SubElement(parent, tag(prefix, name), attrs or {})
        child.text = value
        return child
    package = ET.Element(tag("f", "Package"), {
        "IgnorableNamespaces": "uap uap10 rescap com desktop4 desktop5"})
    add(package, "f", "Identity", {"Name": i["package_name"], "Publisher": i["publisher"],
        "Version": i["package_version"], "ProcessorArchitecture": i["architecture"]})
    props = add(package, "f", "Properties")
    add(props, "f", "DisplayName", value="VartaCopy")
    add(props, "f", "PublisherDisplayName", value=i["publisher_display_name"])
    add(props, "f", "Logo", value="Assets\\StoreLogo.png")
    add(props, "uap10", "AllowExternalContent", value="true")
    resources = add(package, "f", "Resources")
    add(resources, "f", "Resource", {"Language": "uk-UA"})
    deps = add(package, "f", "Dependencies")
    add(deps, "f", "TargetDeviceFamily", {"Name": "Windows.Desktop",
        "MinVersion": i["minimum_windows_version"], "MaxVersionTested": "10.0.26100.0"})
    apps = add(package, "f", "Applications")
    app = add(apps, "f", "Application", {"Id": i["application_id"],
        "Executable": i["gui_filename"], tag("uap10", "RuntimeBehavior"): "win32App",
        tag("uap10", "TrustLevel"): "mediumIL"})
    add(app, "uap", "VisualElements", {"AppListEntry": "none", "DisplayName": "VartaCopy",
        "Description": "VartaCopy Explorer source command", "BackgroundColor": "transparent",
        "Square150x150Logo": "Assets\\Square150x150Logo.png",
        "Square44x44Logo": "Assets\\Square44x44Logo.png"})
    extensions = add(app, "f", "Extensions")
    com = add(extensions, "com", "Extension", {"Category": "windows.comServer"})
    server = add(add(com, "com", "ComServer"), "com", "SurrogateServer", {"DisplayName": "VartaCopy"})
    add(server, "com", "Class", {"Id": i["command_clsid"], "Path": i["dll_filename"],
        "ThreadingModel": "STA"})
    shell = add(extensions, "desktop4", "Extension", {"Category": "windows.fileExplorerContextMenus"})
    menus = add(shell, "desktop4", "FileExplorerContextMenus")
    item = add(menus, "desktop5", "ItemType", {"Type": "Directory"})
    add(item, "desktop5", "Verb", {"Id": i["command_verb"], "Clsid": i["command_clsid"]})
    capabilities = add(package, "f", "Capabilities")
    for name in ["runFullTrust", "unvirtualizedResources"]:
        add(capabilities, "rescap", "Capability", {"Name": name})
    ET.indent(package)
    return ET.tostring(package, encoding="utf-8", xml_declaration=True)


def app_manifest(i: dict, existing: bytes | None = None) -> bytes:
    root = ET.fromstring(existing) if existing else ET.Element(f"{{{ASM}}}assembly", {"manifestVersion": "1.0"})
    if root.tag != f"{{{ASM}}}assembly":
        raise ValueError("Invalid existing application manifest")
    # Refuse conflicting identity instead of silently overwriting unrelated identity.
    for old in root.findall(f"{{{MSIX}}}msix"):
        if old.attrib != {"publisher": i["publisher"], "packageName": i["package_name"],
                          "applicationId": i["application_id"]}:
            raise ValueError("GUI already has a different MSIX identity")
        root.remove(old)
    if root.find(f"{{{ASM}}}assemblyIdentity") is None:
        ET.SubElement(root, f"{{{ASM}}}assemblyIdentity", {"version": "0.1.0.0", "name": "VartaCopy"})
    ET.SubElement(root, f"{{{MSIX}}}msix", {"publisher": i["publisher"],
        "packageName": i["package_name"], "applicationId": i["application_id"]})
    av3 = "urn:schemas-microsoft-com:asm.v3"
    for level in root.findall(f".//{{{av3}}}requestedExecutionLevel"):
        if level.get("level") != "asInvoker" or level.get("uiAccess", "false").lower() != "false":
            raise ValueError("Explorer GUI manifest must request asInvoker without uiAccess")
    if root.find(f"{{{av3}}}trustInfo") is None:
        trust = ET.SubElement(root, f"{{{av3}}}trustInfo")
        security = ET.SubElement(trust, f"{{{av3}}}security")
        levels = ET.SubElement(security, f"{{{av3}}}requestedPrivileges")
        ET.SubElement(levels, f"{{{av3}}}requestedExecutionLevel", {"level": "asInvoker", "uiAccess": "false"})
    compat = "urn:schemas-microsoft-com:compatibility.v1"
    if root.find(f"{{{compat}}}compatibility") is None:
        compat_app = ET.SubElement(ET.SubElement(root, f"{{{compat}}}compatibility"), f"{{{compat}}}application")
        ET.SubElement(compat_app, f"{{{compat}}}supportedOS", {"Id": "{8e0f7a12-bfb3-4fe8-b9a5-48fd50a15a9a}"})
    settings = root.find(f"{{{av3}}}application/{{{av3}}}windowsSettings")
    if settings is None:
        application = root.find(f"{{{av3}}}application")
        if application is None:
            application = ET.SubElement(root, f"{{{av3}}}application")
        settings = ET.SubElement(application, f"{{{av3}}}windowsSettings")
    for uri, name, value in [("http://schemas.microsoft.com/SMI/2005/WindowsSettings", "dpiAware", "true/pm"),
                             ("http://schemas.microsoft.com/SMI/2016/WindowsSettings", "dpiAwareness", "PerMonitorV2")]:
        if settings.find(f"{{{uri}}}{name}") is None:
            ET.SubElement(settings, f"{{{uri}}}{name}").text = value
    ET.indent(root)
    return ET.tostring(root, encoding="utf-8", xml_declaration=True)


def logo_png(size: int) -> bytes:
    """Original geometric V: two offset sheets and a mint transfer arrow."""
    rows = bytearray()
    for y in range(size):
        rows.append(0)
        for x in range(size):
            u, v = x / size, y / size
            color = (17, 29, 45, 255)
            if .18 < u < .64 and .18 < v < .67:
                color = (83, 109, 137, 255)
            if .34 < u < .8 and .33 < v < .82:
                color = (233, 241, 246, 255)
            if (.44 < u < .7 and .52 < v < .61) or (.64 < u < .78 and abs(v - .565) < (.78 - u)):
                color = (14, 155, 123, 255)
            rows.extend(color)
    def chunk(kind, data):
        return struct.pack(">I", len(data)) + kind + data + struct.pack(">I", zlib.crc32(kind + data))
    return b"\x89PNG\r\n\x1a\n" + chunk(b"IHDR", struct.pack(">IIBBBBB", size, size, 8, 6, 0, 0, 0)) + chunk(b"IDAT", zlib.compress(rows)) + chunk(b"IEND", b"")


def embed_manifest(gui: Path, output: Path, i: dict, sdk: Path) -> None:
    mt = sdk_tool("mt.exe", sdk)
    extracted = output / "original-gui.manifest"
    try:
        run([mt, "-nologo", f"-inputresource:{gui};#1", f"-out:{extracted}"])
    except RuntimeError as error:
        # Only an absent resource is expected; malformed existing resources remain errors.
        if "c101008c" not in str(error) or not any(s in str(error) for s in
            ["did not contain a resource section", "resource type cannot be found", "resource name cannot be found"]):
            raise
        original = None
    else:
        original = extracted.read_bytes()
    sidecar = gui.with_suffix(gui.suffix + ".manifest")
    sidecar.write_bytes(app_manifest(i, original))
    run([mt, "-nologo", "-manifest", str(sidecar), f"-outputresource:{gui};#1"])
    verified = output / "embedded-gui.manifest"
    run([mt, "-nologo", f"-inputresource:{gui};#1", f"-out:{verified}"])
    check_app_identity(verified.read_bytes(), i)


def check_app_identity(xml: bytes, i: dict) -> None:
    root = ET.fromstring(xml)
    entries = root.findall(f"{{{MSIX}}}msix")
    expected = {"publisher": i["publisher"], "packageName": i["package_name"], "applicationId": i["application_id"]}
    if len(entries) != 1 or entries[0].attrib != expected:
        raise ValueError("Embedded GUI identity does not match package")


def check_package(package: Path, i: dict) -> None:
    with zipfile.ZipFile(package) as archive:
        names = archive.namelist()
        if len(names) != len(set(names)) or len(names) > 20:
            raise ValueError("Unexpected package entries")
        allowed = {"AppxManifest.xml", "AppxBlockMap.xml", "[Content_Types].xml", "AppxSignature.p7x",
                   "Assets/StoreLogo.png", "Assets/Square150x150Logo.png", "Assets/Square44x44Logo.png"}
        if not set(names).issubset(allowed):
            raise ValueError("Package has unexpected content")
        if sum(x.file_size for x in archive.infolist()) > 1024 * 1024:
            raise ValueError("Identity package exceeds inspection limit")
        actual = ET.fromstring(archive.read("AppxManifest.xml"))
        expected = ET.fromstring(manifest_xml(i))
        def canonical(node):
            return (node.tag, sorted(node.attrib.items()), (node.text or "").strip(),
                    tuple(canonical(child) for child in node))
        if canonical(actual) != canonical(expected):
            raise ValueError("Package manifest differs from approved identity and Directory command")


def check_signature(package: Path, sdk: Path) -> None:
    # /pa verifies Authenticode trust policy. No certificate is created/imported.
    run([sdk_tool("signtool.exe", sdk), "verify", "/pa", "/all", str(package)])


def build(gui: Path, dll: Path, output: Path, sdk: Path = SDK, thumbprint: str | None = None) -> dict:
    i = identity()
    gui, dll = gui.resolve(strict=True), dll.resolve(strict=True)
    require_x64_pe(gui, dll=False)
    require_x64_pe(dll, dll=True)
    source_gui_hash, source_dll_hash = sha256(gui), sha256(dll)
    if thumbprint is not None and not re.fullmatch(r"[0-9a-fA-F]{40}", thumbprint):
        raise ValueError("Certificate thumbprint must have exactly 40 hexadecimal characters")
    sdk_tool("mt.exe", sdk)
    sdk_tool("makeappx.exe", sdk)
    output = local_directory(output, must_exist=False)
    output.mkdir(parents=True, exist_ok=False)
    external, content = output / "external", output / "identity"
    external.mkdir()
    content.mkdir()
    shutil.copyfile(gui, external / i["gui_filename"])
    shutil.copyfile(dll, external / i["dll_filename"])
    if (sha256(external / i["gui_filename"]) != source_gui_hash or
        sha256(external / i["dll_filename"]) != source_dll_hash or
        sha256(gui) != source_gui_hash or sha256(dll) != source_dll_hash):
        raise ValueError("Input binary changed during staging; build a fresh output after compilation finishes")
    embed_manifest(external / i["gui_filename"], output, i, sdk)
    for folder in [content / "Assets", external / "Assets"]:
        folder.mkdir()
        for name, size in [("StoreLogo.png", 50), ("Square150x150Logo.png", 150), ("Square44x44Logo.png", 44)]:
            (folder / name).write_bytes(logo_png(size))
    (content / "AppxManifest.xml").write_bytes(manifest_xml(i))
    package = output / "VartaCopy.Explorer.msix"
    # Sparse manifests reference external binaries; Microsoft requires /nv for pack.
    run([sdk_tool("makeappx.exe", sdk), "pack", "/d", str(content), "/p", str(package), "/nv"])
    check_package(package, i)
    signing = "unsigned-not-installable-until-signed"
    if thumbprint:
        run([sdk_tool("signtool.exe", sdk), "sign", "/fd", "SHA256", "/s", "My", "/sha1", thumbprint, str(package)])
        check_signature(package, sdk)
        signing = "signed-trust-verified-on-build-host"
    files = {p.relative_to(output).as_posix(): sha256(p)
             for p in external.rglob("*") if p.is_file()}
    files[package.name] = sha256(package)
    record = {"format": "vartacopy-modern-explorer-v1", "identity": i, "files": files,
              "signing": signing, "shell_activation": "not-tested", "source_gui_sha256": source_gui_hash,
              "source_dll_sha256": source_dll_hash}
    (output / RECORD).write_text(json.dumps(record, indent=2, ensure_ascii=False) + "\n", encoding="utf-8")
    return {"status": "built", "output": str(output), "signing": signing, "shell_activation": "not-tested"}


def validate_output(output: Path, sdk: Path = SDK, *, trusted=False) -> tuple[Path, Path, dict]:
    output = local_directory(output)
    i = identity()
    record = json.loads((output / RECORD).read_text(encoding="utf-8"))
    if not isinstance(record, dict) or record.get("format") != "vartacopy-modern-explorer-v1" or record.get("identity") != i:
        raise ValueError("Build record identity differs from approved identity")
    external = local_directory(output / "external")
    package = output / "VartaCopy.Explorer.msix"
    expected_files = {"VartaCopy.Explorer.msix", "external/" + i["gui_filename"],
        "external/" + i["dll_filename"], "external/" + i["gui_filename"] + ".manifest",
        "external/Assets/StoreLogo.png", "external/Assets/Square150x150Logo.png", "external/Assets/Square44x44Logo.png"}
    if not isinstance(record.get("files"), dict) or set(record["files"]) != expected_files:
        raise ValueError("Build record file set differs from expected payload")
    for relative, expected in record["files"].items():
        file = output / relative
        local_directory(file.parent)
        if file.is_symlink() or sha256(file) != expected:
            raise ValueError(f"Build content changed: {relative}")
    actual_files = {p.relative_to(output).as_posix() for p in external.rglob("*") if p.is_file()}
    if actual_files != expected_files - {package.name}:
        raise ValueError("External directory has unexpected files")
    check_package(package, i)
    check_app_identity((external / (i["gui_filename"] + ".manifest")).read_bytes(), i)
    require_x64_pe(external / i["gui_filename"], dll=False)
    require_x64_pe(external / i["dll_filename"], dll=True)
    if trusted:
        check_signature(package, sdk)
    return package, external, record


# Read-only discovery uses PackageManager for the current SID only. No wildcard
# removal, -AllUsers, provisioning, registry writes or trust-store mutation.
POWERSHELL_BRIDGE = r'''
$ErrorActionPreference = 'Stop'
$ProgressPreference = 'SilentlyContinue'
[Console]::OutputEncoding = [System.Text.UTF8Encoding]::new($false)
try {
$request = $env:VARTACOPY_PACKAGE_REQUEST | ConvertFrom-Json
$sid = [System.Security.Principal.WindowsIdentity]::GetCurrent().User.Value
$pm = [Windows.Management.Deployment.PackageManager, Windows.Management.Deployment, ContentType=WindowsRuntime]::new()
$packages = @($pm.FindPackagesForUser($sid) | Where-Object { $_.Id.Name -eq $request.name })
$items = @($packages | ForEach-Object {
    [ordered]@{ name=$_.Id.Name; publisher=$_.Id.Publisher; architecture=$_.Id.Architecture.ToString().ToLowerInvariant();
        full_name=$_.Id.FullName; external=$_.EffectiveExternalPath; current_user_sid=$sid;
        machine_external=$_.MachineExternalPath; status_ok=$_.Status.VerifyIsOK() }
})
if ($request.action -eq 'query') {
    [ordered]@{ current_user_sid=$sid; packages=$items } | ConvertTo-Json -Depth 6 -Compress
    exit 0
}
if ($request.action -eq 'install') {
    if ($packages.Count -ne 0) { throw 'Identity already registered; remove the exact owned package first' }
    Add-AppxPackage -Path $request.package -ExternalLocation $request.external -ErrorAction Stop
} elseif ($request.action -eq 'remove') {
    $owned = @($items | Where-Object { $_.full_name -ceq $request.full_name -and
        $_.name -ceq $request.name -and $_.publisher -ceq $request.publisher -and
        $_.architecture -ceq 'x64' -and $_.current_user_sid -ceq $request.sid -and
        [string]::IsNullOrEmpty($_.machine_external) -and
        [string]::Equals($_.external, $request.external, [StringComparison]::OrdinalIgnoreCase) })
    if ($owned.Count -ne 1) { throw 'Package ownership or external directory changed; refusing removal' }
    Remove-AppxPackage -Package $owned[0].full_name -ErrorAction Stop
} else { throw 'Unsupported deployment action' }
[ordered]@{ action=$request.action; completed=$true } | ConvertTo-Json -Compress
} catch {
    [Console]::Error.WriteLine($_.Exception.Message)
    exit 1
}
'''


def bridge(action: str, **data) -> dict:
    request = {"action": action, "name": identity()["package_name"], **data}
    env = os.environ.copy()
    env["VARTACOPY_PACKAGE_REQUEST"] = json.dumps(request, ensure_ascii=True)
    encoded = base64.b64encode(POWERSHELL_BRIDGE.encode("utf-16-le")).decode("ascii")
    executable = str(Path(os.environ.get("SystemRoot", "C:/Windows")) / "System32/WindowsPowerShell/v1.0/powershell.exe")
    return json.loads(run([executable, "-NoLogo", "-NoProfile", "-NonInteractive", "-WindowStyle", "Hidden",
                           "-EncodedCommand", encoded], env=env))


def owned_package(snapshot: dict, external: Path, i: dict) -> dict | None:
    entries = snapshot["packages"]
    if not isinstance(entries, list):
        raise ValueError("Invalid package discovery result")
    matching = []
    for entry in entries:
        if entry.get("name") != i["package_name"]:
            raise ValueError("Discovery returned a different package")
        if (entry.get("publisher") != i["publisher"] or entry.get("architecture") != "x64" or
            entry.get("current_user_sid") != snapshot["current_user_sid"] or entry.get("machine_external") or
            not entry.get("external") or Path(entry["external"]).resolve() != external.resolve()):
            raise ValueError("Identity exists but publisher, owner, architecture or external location differs")
        # FullName comes from PackageManager; still constrain the token passed to removal.
        if not re.fullmatch(r"VartaCopy\.Explorer_\d+\.\d+\.\d+\.\d+_x64__[^_\\/\s]+", entry.get("full_name", "")):
            raise ValueError("Unexpected package full name")
        matching.append(entry)
    if len(matching) > 1:
        raise ValueError("Ambiguous package ownership")
    return matching[0] if matching else None


def classic_status() -> dict:
    # Classic registration belongs to separate CLI. A modern package is not proof
    # of legacy registry registration, or of actual Shell activation.
    return {"status": "not-inspected", "mechanism": "separate-classic-registry-verb",
            "note": "Use the existing classic Explorer integration status command."}


def status(output: Path, sdk: Path = SDK) -> dict:
    # Discovery must survive quarantined binaries, missing records and missing
    # MSIX files. Artifact health is independent from registration ownership.
    output = local_directory(output, must_exist=False)
    external = local_directory(output / "external", must_exist=False)
    package = output / "VartaCopy.Explorer.msix"
    snapshot = bridge("query")
    current = owned_package(snapshot, external, identity())
    record = None
    try:
        _, _, record = validate_output(output, sdk)
    except (ValueError, OSError, RuntimeError, zipfile.BadZipFile, ET.ParseError, KeyError) as error:
        payload = {"status": "damaged-or-missing", "detail": str(error)}
    else:
        payload = {"status": "intact"}
    if not package.is_file():
        signing = {"status": "unknown", "trusted": None, "detail": "Package file is missing"}
    else:
        try:
            check_signature(package, sdk)
        except RuntimeError as error:
            signing = {"status": "verification-failed", "trusted": False, "detail": str(error)}
        except (OSError, ValueError, subprocess.TimeoutExpired) as error:
            signing = {"status": "error", "trusted": None, "detail": str(error)}
        else:
            signing = {"status": "trusted", "trusted": True}
    return {"classic": classic_status(), "modern": {
        "registration": "registered-current-user" if current else "not-registered",
        "package": str(package), "external": str(external), "signature": signing,
        "payload": payload, "build_signing": record.get("signing") if record else None,
        "package_status_ok": current.get("status_ok") if current else None,
        "shell_activation": "not-tested"}}


def install(output: Path, sdk: Path = SDK) -> dict:
    package, external, _ = validate_output(output, sdk, trusted=True)
    if bridge("query")["packages"]:
        raise ValueError("Identity already registered; no implicit replacement or takeover")
    bridge("install", package=str(package), external=str(external))
    current = owned_package(bridge("query"), external, identity())
    if not current or not current.get("status_ok"):
        raise RuntimeError("Registration postcondition did not pass")
    return {"status": "registered-current-user", "full_name": current["full_name"],
            "shell_activation": "not-tested", "classic": classic_status()}


def remove(output: Path, sdk: Path = SDK) -> dict:
    # Recover an owned registration even if AV quarantine or disk damage removed
    # its binaries, MSIX or build record. Identity/owner/location checks remain.
    output = local_directory(output, must_exist=False)
    external = local_directory(output / "external", must_exist=False)
    snapshot = bridge("query")
    current = owned_package(snapshot, external, identity())
    if current is None:
        return {"status": "already-not-registered", "files_removed": False}
    bridge("remove", full_name=current["full_name"], publisher=identity()["publisher"],
           external=str(external), sid=snapshot["current_user_sid"])
    if bridge("query")["packages"]:
        raise RuntimeError("Removal postcondition did not pass")
    return {"status": "unregistered-current-user", "files_removed": False, "classic": classic_status()}


def main(argv=None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("action", choices=["build", "status", "install", "remove"])
    parser.add_argument("--output", type=Path, required=True, help="New build directory; never overwritten")
    parser.add_argument("--gui", type=Path)
    parser.add_argument("--dll", type=Path)
    parser.add_argument("--sdk", type=Path, default=SDK)
    parser.add_argument("--sign-thumbprint", help="Existing CurrentUser/My certificate, explicit SHA1 selector")
    args = parser.parse_args(argv)
    try:
        if args.action == "build":
            if args.gui is None or args.dll is None:
                parser.error("build requires --gui and --dll")
            result = build(args.gui, args.dll, args.output, args.sdk, args.sign_thumbprint)
        else:
            if args.gui or args.dll or args.sign_thumbprint:
                parser.error("--gui, --dll and --sign-thumbprint are build-only")
            result = {"status": status, "install": install, "remove": remove}[args.action](args.output, args.sdk)
        print(json.dumps(result, indent=2, ensure_ascii=False))
        return 0
    except (ValueError, OSError, RuntimeError, zipfile.BadZipFile, ET.ParseError, KeyError, subprocess.TimeoutExpired) as error:
        print(json.dumps({"status": "failed", "detail": str(error)}, ensure_ascii=False), file=sys.stderr)
        return 1


if __name__ == "__main__":
    raise SystemExit(main())

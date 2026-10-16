"""Safety and generation checks. These do not claim installed Shell activation."""
from __future__ import annotations

import base64
import json
import os
from pathlib import Path
import struct
import tempfile
import unittest
from unittest.mock import patch
import xml.etree.ElementTree as ET
import zipfile

import modern_explorer as m


def synthetic_pe(path: Path, dll=False):
    data = bytearray(512)
    data[:2] = b"MZ"
    struct.pack_into("<I", data, 0x3C, 128)
    data[128:132] = b"PE\0\0"
    struct.pack_into("<H", data, 132, 0x8664)
    struct.pack_into("<H", data, 150, 0x2022 if dll else 0x22)
    path.write_bytes(data)


class PackageTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory(prefix="vartacopy-package-test-")
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name).resolve()
        self.i = m.identity()

    def fixture(self):
        out = self.root / "build"
        external = out / "external"
        assets = external / "Assets"
        assets.mkdir(parents=True)
        synthetic_pe(external / self.i["gui_filename"])
        synthetic_pe(external / self.i["dll_filename"], dll=True)
        (external / (self.i["gui_filename"] + ".manifest")).write_bytes(m.app_manifest(self.i))
        for name, size in [("StoreLogo.png", 50), ("Square150x150Logo.png", 150), ("Square44x44Logo.png", 44)]:
            (assets / name).write_bytes(m.logo_png(size))
        package = out / "VartaCopy.Explorer.msix"
        with zipfile.ZipFile(package, "w") as archive:
            archive.writestr("AppxManifest.xml", m.manifest_xml(self.i))
            for asset in assets.iterdir():
                archive.writestr("Assets/" + asset.name, asset.read_bytes())
        files = {p.relative_to(out).as_posix(): m.sha256(p) for p in external.rglob("*") if p.is_file()}
        files[package.name] = m.sha256(package)
        (out / m.RECORD).write_text(json.dumps({"format": "vartacopy-modern-explorer-v1",
            "identity": self.i, "files": files, "signing": "unsigned-not-installable-until-signed"}), encoding="utf-8")
        return out, package, external

    def snapshot(self, external):
        return {"current_user_sid": "S-1-5-21-test", "packages": [{
            "name": "VartaCopy.Explorer", "publisher": self.i["publisher"], "architecture": "x64",
            "full_name": "VartaCopy.Explorer_0.1.0.0_x64__abcdefghijklm", "external": str(external),
            "machine_external": "", "current_user_sid": "S-1-5-21-test", "status_ok": True}]}

    def test_manifest_directory_only_and_identity(self):
        tree = ET.fromstring(m.manifest_xml(self.i))
        self.assertEqual(tree.find("f:Identity", m.NS).attrib, {
            "Name": self.i["package_name"], "Publisher": self.i["publisher"],
            "Version": self.i["package_version"], "ProcessorArchitecture": "x64"})
        self.assertEqual(tree.find("f:Properties/uap10:AllowExternalContent", m.NS).text, "true")
        self.assertEqual([x.attrib for x in tree.findall(".//desktop5:ItemType", m.NS)], [{"Type": "Directory"}])
        self.assertEqual([x.attrib for x in tree.findall(".//desktop5:Verb", m.NS)],
            [{"Id": self.i["command_verb"], "Clsid": self.i["command_clsid"]}])
        self.assertEqual(tree.find(".//com:Class", m.NS).attrib,
            {"Id": self.i["command_clsid"], "Path": self.i["dll_filename"], "ThreadingModel": "STA"})
        self.assertEqual(tree.find(".//f:Application", m.NS).get("Id"), self.i["application_id"])

    def test_app_manifest_preserves_existing_settings(self):
        original = m.app_manifest(self.i)
        root = ET.fromstring(original)
        root.find(".//{urn:schemas-microsoft-com:asm.v3}requestedExecutionLevel").set("level", "asInvoker")
        ET.SubElement(root, "{urn:schemas-microsoft-com:asm.v1}description").text = "Existing metadata"
        merged = m.app_manifest(self.i, ET.tostring(root))
        m.check_app_identity(merged, self.i)
        self.assertIn(b"Existing metadata", merged)
        self.assertIn(b"PerMonitorV2", merged)
        self.assertIn(b"8e0f7a12-bfb3-4fe8-b9a5-48fd50a15a9a", merged)

    def test_app_conflicting_identity_fails(self):
        original = m.app_manifest(self.i).replace(b'packageName="VartaCopy.Explorer"', b'packageName="Other.App"')
        with self.assertRaisesRegex(ValueError, "different MSIX"):
            m.app_manifest(self.i, original)

    def test_gui_elevation_not_silently_preserved(self):
        original = m.app_manifest(self.i).replace(b'level="asInvoker"', b'level="requireAdministrator"')
        with self.assertRaisesRegex(ValueError, "asInvoker"):
            m.app_manifest(self.i, original)

    def test_full_unsigned_build_preserves_input_and_uses_exact_argv(self):
        gui, dll = self.root / "own gui & literal.exe", self.root / "own cmd.dll"
        synthetic_pe(gui)
        synthetic_pe(dll, dll=True)
        before = m.sha256(gui)
        output = self.root / "new build & literal"
        calls = []
        def run(argv, **kwargs):
            calls.append(argv)
            if argv[0] == "mt.exe":
                source = next((a for a in argv if a.startswith("-inputresource:")), None)
                destination = next((a[5:] for a in argv if a.startswith("-out:")), None)
                if source and not (output / "embedded-gui.manifest").exists() and len(calls) == 1:
                    raise RuntimeError("c101008c: image file did not contain a resource section")
                if destination:
                    Path(destination).write_bytes(m.app_manifest(self.i))
            if argv[0] == "makeappx.exe":
                package = Path(argv[argv.index("/p") + 1])
                content = Path(argv[argv.index("/d") + 1])
                with zipfile.ZipFile(package, "w") as archive:
                    for file in content.rglob("*"):
                        if file.is_file():
                            archive.write(file, file.relative_to(content).as_posix())
            return ""
        with patch.object(m, "sdk_tool", side_effect=lambda name, sdk: name), patch.object(m, "run", side_effect=run):
            result = m.build(gui, dll, output)
        self.assertEqual(result["signing"], "unsigned-not-installable-until-signed")
        self.assertEqual(m.sha256(gui), before)
        self.assertEqual(m.validate_output(output)[1], output / "external")
        pack = [c for c in calls if c[0] == "makeappx.exe"][0]
        self.assertEqual(pack, ["makeappx.exe", "pack", "/d", str(output / "identity"), "/p", str(output / "VartaCopy.Explorer.msix"), "/nv"])
        self.assertFalse(any(c[0] == "signtool.exe" for c in calls))

    def test_unsigned_artifact_valid_but_not_trusted(self):
        out, package, external = self.fixture()
        self.assertEqual(m.validate_output(out)[:2], (package, external))
        with patch.object(m, "check_signature", side_effect=RuntimeError("untrusted")), patch.object(m, "bridge") as bridge:
            with self.assertRaisesRegex(RuntimeError, "untrusted"):
                m.install(out)
            bridge.assert_not_called()

    def test_changed_payload_refused_before_deployment(self):
        out, _, external = self.fixture()
        (external / self.i["dll_filename"]).write_bytes(b"changed")
        with patch.object(m, "bridge") as bridge:
            with self.assertRaisesRegex(ValueError, "Build content changed"):
                m.install(out)
            bridge.assert_not_called()

    def test_unexpected_payload_refused(self):
        out, _, external = self.fixture()
        (external / "unexpected.dll").write_bytes(b"x")
        with self.assertRaisesRegex(ValueError, "unexpected files"):
            m.validate_output(out)

    def test_tampered_selection_manifest_refused(self):
        out, package, _ = self.fixture()
        with zipfile.ZipFile(package, "w") as archive:
            archive.writestr("AppxManifest.xml", m.manifest_xml(self.i).replace(b'Type="Directory"', b'Type="*"'))
        with self.assertRaisesRegex(ValueError, "Directory command"):
            m.check_package(package, self.i)

    def test_package_extra_file_and_path_refused(self):
        _, package, _ = self.fixture()
        with zipfile.ZipFile(package, "a") as archive:
            archive.writestr("../escape", b"x")
        with self.assertRaisesRegex(ValueError, "unexpected content"):
            m.check_package(package, self.i)

    def test_record_path_traversal_refused(self):
        out, _, _ = self.fixture()
        record = json.loads((out / m.RECORD).read_text(encoding="utf-8"))
        record["files"]["../elsewhere"] = "x"
        (out / m.RECORD).write_text(json.dumps(record), encoding="utf-8")
        with self.assertRaisesRegex(ValueError, "file set"):
            m.validate_output(out)

    def test_windows_junction_attributes_refused_without_is_junction_api(self):
        from types import SimpleNamespace
        directory = self.root / "junction"
        directory.mkdir()
        original_lstat = type(directory).lstat
        def synthetic_lstat(path):
            if path == directory:
                return SimpleNamespace(st_mode=0o040755, st_file_attributes=0x400)
            return original_lstat(path)
        with patch.object(type(directory), "lstat", synthetic_lstat):
            with self.assertRaisesRegex(ValueError, "Reparse path"):
                m.local_directory(directory)

    def test_pe_architecture_and_dll_type(self):
        pe = self.root / "synthetic.dll"
        synthetic_pe(pe, dll=True)
        m.require_x64_pe(pe, dll=True)
        with self.assertRaisesRegex(ValueError, "Expected x64 EXE"):
            m.require_x64_pe(pe, dll=False)
        data = bytearray(pe.read_bytes())
        struct.pack_into("<H", data, 132, 0x14c)
        pe.write_bytes(data)
        with self.assertRaisesRegex(ValueError, "Expected x64 DLL"):
            m.require_x64_pe(pe, dll=True)

    def test_exact_ownership_required(self):
        external = self.root / "external"
        snap = self.snapshot(external)
        self.assertIsNotNone(m.owned_package(snap, external, self.i))
        for field, value in [("publisher", "CN=Other"), ("current_user_sid", "S-1-5-21-other"),
                              ("architecture", "arm64"), ("machine_external", "C:/machine"),
                              ("external", str(self.root / "other")), ("full_name", "Other_0.1.0.0_x64__abc")]:
            changed = json.loads(json.dumps(snap))
            changed["packages"][0][field] = value
            with self.subTest(field=field), self.assertRaises(ValueError):
                m.owned_package(changed, external, self.i)

    def test_remove_exact_current_user_no_deletion(self):
        out, _, external = self.fixture()
        calls = []
        def bridge(action, **kwargs):
            calls.append((action, kwargs))
            if action == "query":
                return self.snapshot(external) if len(calls) == 1 else {"packages": [], "current_user_sid": "S-1-5-21-test"}
            return {"completed": True}
        with patch.object(m, "bridge", side_effect=bridge):
            result = m.remove(out)
        self.assertEqual(result["status"], "unregistered-current-user")
        self.assertTrue((external / self.i["gui_filename"]).exists())
        self.assertEqual(calls[1][1]["full_name"], "VartaCopy.Explorer_0.1.0.0_x64__abcdefghijklm")

    def test_remove_foreign_directory_never_mutates(self):
        out, _, _ = self.fixture()
        with patch.object(m, "bridge", return_value=self.snapshot(self.root / "foreign")) as bridge:
            with self.assertRaises(ValueError):
                m.remove(out)
            self.assertEqual(bridge.call_count, 1)

    def test_recovery_remove_missing_entire_payload_and_record(self):
        output = self.root / "quarantined-output"
        external = output / "external"
        responses = [self.snapshot(external), {"completed": True},
                     {"packages": [], "current_user_sid": "S-1-5-21-test"}]
        with patch.object(m, "bridge", side_effect=responses) as bridge, patch.object(m, "validate_output") as validate, patch.object(m, "check_signature") as signature:
            result = m.remove(output)
            self.assertEqual(result["status"], "unregistered-current-user")
            self.assertEqual(bridge.call_args_list[1].kwargs["external"], str(external))
            validate.assert_not_called()
            signature.assert_not_called()
        self.assertFalse(output.exists())

    def test_recovery_remove_corrupted_dll_and_missing_msix(self):
        output, package, external = self.fixture()
        (external / self.i["dll_filename"]).write_bytes(b"corrupt")
        package.unlink()
        (output / m.RECORD).unlink()
        with patch.object(m, "bridge", side_effect=[self.snapshot(external), {"completed": True},
            {"packages": [], "current_user_sid": "S-1-5-21-test"}]):
            self.assertEqual(m.remove(output)["status"], "unregistered-current-user")
        self.assertEqual((external / self.i["dll_filename"]).read_bytes(), b"corrupt")

    def test_recovery_remove_foreign_identity_or_owner_refused_without_payload(self):
        output = self.root / "missing-output"
        external = output / "external"
        for field, value in [("name", "Other.App"), ("publisher", "CN=Other"),
                             ("current_user_sid", "S-1-other"), ("machine_external", "C:/machine"),
                             ("external", str(self.root / "foreign")),
                             ("full_name", "Other.App_0.1.0.0_x64__abcdefghijklm")]:
            snapshot = self.snapshot(external)
            snapshot["packages"][0][field] = value
            with self.subTest(field=field), patch.object(m, "bridge", return_value=snapshot) as bridge:
                with self.assertRaises(ValueError):
                    m.remove(output)
                self.assertEqual(bridge.call_count, 1)

    def test_status_keeps_registration_when_payload_missing(self):
        output = self.root / "missing-output"
        external = output / "external"
        with patch.object(m, "bridge", return_value=self.snapshot(external)), patch.object(m, "check_signature") as signature:
            modern = m.status(output)["modern"]
            signature.assert_not_called()
        self.assertEqual(modern["registration"], "registered-current-user")
        self.assertEqual(modern["payload"]["status"], "damaged-or-missing")
        self.assertEqual(modern["signature"]["status"], "unknown")
        self.assertIsNone(modern["signature"]["trusted"])
        self.assertIsNone(modern["build_signing"])

    def test_status_keeps_registration_with_corrupt_dll_and_signature_failure(self):
        output, _, external = self.fixture()
        (external / self.i["dll_filename"]).write_bytes(b"corrupt")
        with patch.object(m, "bridge", return_value=self.snapshot(external)), patch.object(m, "check_signature", side_effect=RuntimeError("invalid signature")):
            modern = m.status(output)["modern"]
        self.assertEqual(modern["registration"], "registered-current-user")
        self.assertEqual(modern["payload"]["status"], "damaged-or-missing")
        self.assertIn("Build content changed", modern["payload"]["detail"])
        self.assertEqual(modern["signature"]["status"], "verification-failed")
        self.assertFalse(modern["signature"]["trusted"])

    def test_status_signature_tool_error_is_unknown_trust(self):
        output, _, external = self.fixture()
        with patch.object(m, "bridge", return_value=self.snapshot(external)), patch.object(m, "check_signature", side_effect=FileNotFoundError("SDK unavailable")):
            modern = m.status(output)["modern"]
        self.assertEqual(modern["payload"]["status"], "intact")
        self.assertEqual(modern["signature"]["status"], "error")
        self.assertIsNone(modern["signature"]["trusted"])

    def test_status_malformed_record_still_shows_registration(self):
        output, _, external = self.fixture()
        (output / m.RECORD).write_text('[]', encoding="utf-8")
        with patch.object(m, "bridge", return_value=self.snapshot(external)), patch.object(m, "check_signature", side_effect=RuntimeError("unsigned")):
            modern = m.status(output)["modern"]
        self.assertEqual(modern["registration"], "registered-current-user")
        self.assertEqual(modern["payload"]["status"], "damaged-or-missing")
        self.assertEqual(modern["signature"]["status"], "verification-failed")

    def test_install_no_implicit_replace(self):
        out, _, external = self.fixture()
        with patch.object(m, "check_signature"), patch.object(m, "bridge", return_value=self.snapshot(external)) as bridge:
            with self.assertRaisesRegex(ValueError, "no implicit replacement"):
                m.install(out)
            self.assertEqual(bridge.call_count, 1)

    def test_install_checks_registration_postcondition(self):
        out, _, _ = self.fixture()
        with patch.object(m, "check_signature"), patch.object(m, "bridge", side_effect=[
            {"packages": []}, {"completed": True}, {"packages": [], "current_user_sid": "S-1-5-21-test"}]):
            with self.assertRaisesRegex(RuntimeError, "postcondition"):
                m.install(out)

    def test_bridge_paths_are_data_not_code(self):
        hostile = 'C:/Space & Quote/"; Write-Output INJECTED; $x/кирилиця'
        captured = {}
        def run(argv, **kwargs):
            captured.update(argv=argv, **kwargs)
            return '{"packages": [], "current_user_sid": "S-1-test"}'
        with patch.object(m, "run", side_effect=run):
            m.bridge("install", package=hostile, external=hostile)
        args = captured["argv"]
        self.assertNotIn(hostile, " ".join(args))
        script = base64.b64decode(args[-1]).decode("utf-16-le")
        self.assertEqual(script, m.POWERSHELL_BRIDGE)
        data = json.loads(captured["env"]["VARTACOPY_PACKAGE_REQUEST"])
        self.assertEqual(data["external"], hostile)
        self.assertNotIn("INJECTED", script)
        for prohibited in ["Import-Certificate", "Set-ItemProperty", "Restart", "-AllUsers", "-SkipSignatureCheck", "DevelopmentMode"]:
            self.assertNotIn(prohibited, script)

    def test_run_uses_argv_and_no_shell(self):
        from types import SimpleNamespace
        argv = ["tool.exe", "C:/a & b/$(payload); 'literal'"]
        with patch.object(m.subprocess, "run", return_value=SimpleNamespace(returncode=0, stdout="ok", stderr="")) as process:
            self.assertEqual(m.run(argv), "ok")
            self.assertEqual(process.call_args.args[0], argv)
            self.assertFalse(process.call_args.kwargs["shell"])

    def test_build_no_clobber_and_bad_thumbprint(self):
        gui, dll = self.root / "gui.exe", self.root / "cmd.dll"
        synthetic_pe(gui)
        synthetic_pe(dll, dll=True)
        output = self.root / "existing"
        output.mkdir()
        sentinel = output / "keep.txt"
        sentinel.write_text("keep", encoding="utf-8")
        with patch.object(m, "sdk_tool", return_value="sdk.exe"), patch.object(m, "run") as run:
            with self.assertRaises(FileExistsError):
                m.build(gui, dll, output)
            run.assert_not_called()
        self.assertEqual(sentinel.read_text(encoding="utf-8"), "keep")
        with self.assertRaisesRegex(ValueError, "thumbprint"):
            m.build(gui, dll, self.root / "new", thumbprint='abc & bad')

    def test_unknown_mt_errors_not_swallowed(self):
        gui = self.root / "gui.exe"
        with patch.object(m, "sdk_tool", return_value="mt.exe"), patch.object(m, "run", side_effect=RuntimeError("access denied")):
            with self.assertRaisesRegex(RuntimeError, "access denied"):
                m.embed_manifest(gui, self.root, self.i, m.SDK)

    def test_changed_input_during_staging_refused(self):
        gui, dll = self.root / "gui.exe", self.root / "cmd.dll"
        synthetic_pe(gui)
        synthetic_pe(dll, dll=True)
        original_copy = m.shutil.copyfile
        def unstable_copy(source, destination):
            result = original_copy(source, destination)
            if source == gui:
                gui.write_bytes(gui.read_bytes() + b"parallel rebuild")
            return result
        with patch.object(m, "sdk_tool", return_value="sdk.exe"), patch.object(m.shutil, "copyfile", side_effect=unstable_copy), patch.object(m, "run") as run:
            with self.assertRaisesRegex(ValueError, "changed during staging"):
                m.build(gui, dll, self.root / "new-build")
            run.assert_not_called()

    def test_status_distinguishes_classic_modern_activation(self):
        out, _, _ = self.fixture()
        with patch.object(m, "bridge", return_value={"packages": [], "current_user_sid": "SID"}), patch.object(m, "check_signature", side_effect=RuntimeError("unsigned")):
            status = m.status(out)
        self.assertEqual(status["classic"]["status"], "not-inspected")
        self.assertEqual(status["modern"]["registration"], "not-registered")
        self.assertFalse(status["modern"]["signature"]["trusted"])
        self.assertEqual(status["modern"]["shell_activation"], "not-tested")


def com_dll_smoke(path: Path) -> dict:
    """Independently exercise the release DLL ABI without Shell registration/UI.

    Invoke is called only with null/empty/multiple selections or a synthetic
    single selection whose GetItemAt fails. No valid source reaches launch.
    """
    import ctypes as c
    import uuid
    HR, U32, PV = c.c_int32, c.c_uint32, c.c_void_p
    PVOID = c.POINTER(PV)
    class GUID(c.Structure):
        _fields_ = [("data1", c.c_uint32), ("data2", c.c_uint16),
                    ("data3", c.c_uint16), ("data4", c.c_ubyte * 8)]
    def guid(text):
        return GUID.from_buffer_copy(uuid.UUID(text).bytes_le)
    PG = c.POINTER(GUID)
    clsid = guid(m.identity()["command_clsid"])
    factory_iid = guid("00000001-0000-0000-c000-000000000046")
    unknown_iid = guid("00000000-0000-0000-c000-000000000046")
    command_iid = guid("a08ce4d0-fa25-44ab-b57c-c7b1c323e0b9")
    wrong = guid("01234567-89ab-cdef-0123-456789abcdef")
    checks = 0
    def equal(actual, expected):
        nonlocal checks
        if actual != expected:
            raise AssertionError(f"COM smoke mismatch: {actual!r} != {expected!r}")
        checks += 1
    def hr(actual, expected=0):
        equal(actual & 0xFFFFFFFF, expected)
    def method(obj, index, result, *args):
        vtable = c.cast(obj, c.POINTER(c.POINTER(PV))).contents
        return c.WINFUNCTYPE(result, PV, *args)(vtable[index])
    def release(obj):
        return method(obj, 2, U32)(obj)
    ole = c.WinDLL("ole32.dll")
    ole.CoInitializeEx.argtypes, ole.CoInitializeEx.restype = [PV, U32], HR
    ole.CoTaskMemFree.argtypes, ole.CoTaskMemFree.restype = [PV], None
    ole.CoUninitialize.argtypes, ole.CoUninitialize.restype = [], None
    hr(ole.CoInitializeEx(None, 2))
    dll = c.WinDLL(str(path.resolve(strict=True)))
    get = dll.DllGetClassObject
    get.argtypes, get.restype = [PG, PG, PVOID], HR
    unload = dll.DllCanUnloadNow
    unload.argtypes, unload.restype = [], HR
    factory, command = PV(), PV()
    locked = False
    try:
        for export in ["DllRegisterServer", "DllUnregisterServer"]:
            try:
                getattr(dll, export)
            except AttributeError:
                checks += 1
            else:
                raise AssertionError(f"Unexpected registry-registration export: {export}")
        hr(unload())
        raw = PV(1)
        hr(get(None, c.byref(factory_iid), c.byref(raw)), 0x80004003)
        equal(raw.value, None)
        hr(get(c.byref(wrong), c.byref(factory_iid), c.byref(raw)), 0x80040111)
        equal(raw.value, None)
        hr(get(c.byref(clsid), c.byref(command_iid), c.byref(raw)), 0x80004002)
        hr(get(c.byref(clsid), c.byref(factory_iid), None), 0x80004003)
        hr(get(c.byref(clsid), c.byref(factory_iid), c.byref(factory)))
        hr(unload(), 1)
        equal(method(factory, 1, U32)(factory), 2)
        equal(release(factory), 1)
        create = method(factory, 3, HR, PV, PG, PVOID)
        lock = method(factory, 4, HR, c.c_int32)
        hr(create(factory, factory, c.byref(command_iid), c.byref(raw)), 0x80040110)
        equal(raw.value, None)
        hr(create(factory, None, c.byref(wrong), c.byref(raw)), 0x80004002)
        equal(raw.value, None)
        hr(create(factory, None, None, c.byref(raw)), 0x80004003)
        hr(lock(factory, 0), 0x80070057)
        hr(lock(factory, 1))
        locked = True
        hr(create(factory, None, c.byref(command_iid), c.byref(command)))
        qi = method(command, 0, HR, PG, PVOID)
        hr(qi(command, c.byref(wrong), c.byref(raw)), 0x80004002)
        equal(raw.value, None)
        hr(qi(command, c.byref(unknown_iid), c.byref(raw)))
        equal(raw.value, command.value)
        equal(release(raw), 1)
        get_title = method(command, 3, HR, PV, PVOID)
        hr(get_title(command, None, None), 0x80004003)
        title = PV()
        hr(get_title(command, None, c.byref(title)))
        equal(c.wstring_at(title), m.identity()["command_title"])
        ole.CoTaskMemFree(title)
        for index in [4, 5]:
            value = PV(1)
            hr(method(command, index, HR, PV, PVOID)(command, None, c.byref(value)), 0x80004001)
            equal(value.value, None)
        canonical = GUID()
        hr(method(command, 6, HR, PG)(command, c.byref(canonical)))
        equal(bytes(canonical), bytes(clsid))
        state = U32(99)
        get_state = method(command, 7, HR, PV, c.c_int32, c.POINTER(U32))
        invoke = method(command, 8, HR, PV, PV)
        hr(get_state(command, None, 0, c.byref(state)))
        equal(state.value, 2)  # ECS_HIDDEN
        hr(invoke(command, None, None), 0x80070057)
        flags = U32(99)
        hr(method(command, 9, HR, c.POINTER(U32))(command, c.byref(flags)))
        equal(flags.value, 0)
        raw = PV(1)
        hr(method(command, 10, HR, PVOID)(command, c.byref(raw)), 0x80004001)
        equal(raw.value, None)

        # Minimal synthetic IShellItemArray; code may only ask GetCount and
        # GetItemAt. The latter always fails, so GUI launch is unreachable.
        count = U32(0)
        @c.WINFUNCTYPE(HR, PV, c.POINTER(U32))
        def get_count(this, out):
            out[0] = count.value
            return 0
        @c.WINFUNCTYPE(HR, PV, U32, PVOID)
        def get_item(this, index, out):
            out[0] = None
            return -2147467259  # E_FAIL
        vtable = (PV * 10)()
        vtable[7] = c.cast(get_count, PV)
        vtable[8] = c.cast(get_item, PV)
        array = c.pointer(c.cast(vtable, PV))
        for selection_count in [0, 2]:
            count.value = selection_count
            hr(get_state(command, array, 1, c.byref(state)))
            equal(state.value, 2)
            hr(invoke(command, array, None), 0x80070057)
        count.value = 1
        hr(get_state(command, array, 0, c.byref(state)), 0x8000000A)
        equal(state.value, 2)
        hr(get_state(command, array, 1, c.byref(state)), 0x80004005)
        equal(state.value, 2)
        hr(invoke(command, array, None), 0x80004005)

        equal(release(command), 0)
        command = PV()
        equal(release(factory), 0)
        factory = PV()
        hr(unload(), 1)  # server lock survives released factory
        hr(get(c.byref(clsid), c.byref(factory_iid), c.byref(factory)))
        hr(method(factory, 4, HR, c.c_int32)(factory, 0))
        locked = False
        equal(release(factory), 0)
        factory = PV()
        hr(unload())
    finally:
        if command.value:
            release(command)
        if locked:
            if not factory.value:
                hr(get(c.byref(clsid), c.byref(factory_iid), c.byref(factory)))
            hr(method(factory, 4, HR, c.c_int32)(factory, 0))
        if factory.value:
            release(factory)
        ole.CoUninitialize()
    return {"dll": str(path.resolve()), "sha256": m.sha256(path), "abi_checks": checks,
            "registration": "not-run", "gui_launch": "not-run", "shell_activation": "not-tested"}


if __name__ == "__main__":
    import argparse
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--dll-smoke", type=Path, help="Optional actual DLL ABI smoke; no registration or valid Invoke")
    args = parser.parse_args()
    suite = unittest.defaultTestLoader.loadTestsFromTestCase(PackageTests)
    result = unittest.TextTestRunner(verbosity=2).run(suite)
    if result.wasSuccessful():
        if args.dll_smoke:
            print(json.dumps(com_dll_smoke(args.dll_smoke), ensure_ascii=False))
        print("passed MODERN-PACKAGE")
    raise SystemExit(0 if result.wasSuccessful() else 1)

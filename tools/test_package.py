import hashlib
import json
from pathlib import Path
import tempfile
import unittest
import zipfile
from tools.package import TARGET, verify


class PackageVerification(unittest.TestCase):
    def archive(self, root, corrupt=False, extra=None, integration=False, omit=None, native=False):
        path = Path(root) / "fixture.zip"
        files = {"vartacopy.exe": b"cli", "vartacopy-gui.exe": b"gui", "README.txt": b"guide", "THIRD-PARTY.json": b"[]"}
        if integration:
            files.update({"vartacopy_transfer.dll": b"dll", "tools/explorer_drop_probe.py": b"helper", "EXPLORER.txt": b"instructions"})
        if native:
            files.update({"vartacopy_transfer.dll": b"dll", "EXPLORER.txt": b"instructions"})
        if omit:
            del files[omit]
        manifest = {"format": "vartacopy-portable", "target": TARGET, "revision": "test", "source_dirty": False,
                    "files": [{"path": name, "bytes": len(data), "sha256": hashlib.sha256(data).hexdigest()} for name, data in files.items()]}
        if integration:
            manifest["integrations"] = ["explorer-transfer"]
        if native:
            manifest["integrations"] = ["explorer-transfer-native"]
        if corrupt:
            files["vartacopy.exe"] = b"bad"
        with zipfile.ZipFile(path, "w") as bundle:
            for name, data in files.items():
                bundle.writestr("fixture/" + name, data)
            bundle.writestr("fixture/manifest.json", json.dumps(manifest))
            if extra:
                bundle.writestr(extra, b"unexpected")
        return path

    def test_valid_archive(self):
        with tempfile.TemporaryDirectory() as root:
            self.assertEqual(verify(self.archive(root))["status"], "verified")

    def test_corrupted_binary_is_rejected(self):
        with tempfile.TemporaryDirectory() as root:
            with self.assertRaisesRegex(ValueError, "Checksum mismatch"):
                verify(self.archive(root, corrupt=True))

    def test_transfer_package_requires_both_module_and_registration_instructions(self):
        for omit in ["vartacopy_transfer.dll", "tools/explorer_drop_probe.py", "EXPLORER.txt"]:
            with self.subTest(omit=omit), tempfile.TemporaryDirectory() as root:
                with self.assertRaisesRegex(ValueError, "missing required file"):
                    verify(self.archive(root, integration=True, omit=omit))

    def test_complete_transfer_package_is_verified(self):
        with tempfile.TemporaryDirectory() as root:
            self.assertEqual(verify(self.archive(root, integration=True))["status"], "verified")

    def test_native_integration_needs_no_python_helper(self):
        with tempfile.TemporaryDirectory() as root:
            self.assertEqual(verify(self.archive(root, native=True))["status"], "verified")
        for omit in ["vartacopy_transfer.dll", "EXPLORER.txt"]:
            with self.subTest(omit=omit), tempfile.TemporaryDirectory() as root:
                with self.assertRaisesRegex(ValueError, "missing required file"):
                    verify(self.archive(root, native=True, omit=omit))

    def test_extra_payload_and_traversal_are_rejected(self):
        for name in ["fixture/unlisted.exe", "../escape.exe", "fixture/evil" + chr(92) + "file"]:
            with self.subTest(name=name), tempfile.TemporaryDirectory() as root:
                with self.assertRaises(ValueError):
                    verify(self.archive(root, extra=name))


if __name__ == "__main__":
    unittest.main()

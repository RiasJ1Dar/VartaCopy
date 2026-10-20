import hashlib
import json
from pathlib import Path
import tempfile
import unittest
import zipfile

from tools.installer import REQUIRED, stage
from tools.package import TARGET


class InstallerPayload(unittest.TestCase):
    def archive(self, root, *, dirty=False, version="0.1.0-beta.1", extra=None, corrupt=False, legacy=False):
        files = {name: name.encode() for name in REQUIRED}
        files["licenses/dependency/LICENSE.txt"] = b"license"
        files["tools/developer.py"] = b"developer only"
        if extra:
            files[extra] = b"unexpected"
        manifest = {"format": "vartacopy-portable", "target": TARGET, "version": version,
                    "revision": "a" * 40, "source_dirty": dirty, "integrations": ["explorer-transfer-native"],
                    "files": [{"path": name, "bytes": len(data), "sha256": hashlib.sha256(data).hexdigest()}
                              for name, data in files.items()]}
        if corrupt:
            files["vartacopy.exe"] = b"corrupt"
        if legacy:
            manifest["integrations"] = []
        archive = root / "fixture.zip"
        with zipfile.ZipFile(archive, "x") as bundle:
            for name, data in files.items():
                bundle.writestr("fixture/" + name, data)
            bundle.writestr("fixture/manifest.json", json.dumps(manifest))
        return archive

    def test_immutable_module_locator_and_license_inventory(self):
        with tempfile.TemporaryDirectory() as folder:
            root = Path(folder)
            record = stage(self.archive(root), root / "output")
            payload = Path(record["payload"])
            locator = json.loads((payload / "integration-install.json").read_text())
            self.assertEqual(sha256 := hashlib.sha256(b"vartacopy_transfer.dll").hexdigest(), record["shell_sha256"])
            self.assertIn(sha256, locator["dll"])
            self.assertEqual((payload / locator["dll"]).read_bytes(), b"vartacopy_transfer.dll")
            self.assertFalse((payload / "vartacopy_transfer.dll").exists())
            self.assertFalse((payload / "tools").exists())
            self.assertEqual((payload / "licenses/dependency/LICENSE.txt").read_bytes(), b"license")
            with self.assertRaises(FileExistsError):
                stage(root / "fixture.zip", root / "output")

    def test_dirty_invalid_and_corrupt_packages_fail_before_output_creation(self):
        for options in [{"dirty": True}, {"version": '1.0\"\n[Code]'}, {"corrupt": True}, {"legacy": True},
                        {"extra": "licenses/NUL.txt"}, {"extra": "licenses/dependency/license.TXT"}]:
            with self.subTest(options=options), tempfile.TemporaryDirectory() as folder:
                root = Path(folder)
                with self.assertRaises(ValueError):
                    stage(self.archive(root, **options), root / "output")
                self.assertFalse((root / "output").exists())

    def test_dirty_build_requires_explicit_local_override(self):
        with tempfile.TemporaryDirectory() as folder:
            root = Path(folder)
            self.assertTrue(stage(self.archive(root, dirty=True), root / "output", True)["source_dirty"])


if __name__ == "__main__":
    unittest.main()

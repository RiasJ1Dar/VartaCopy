"""Installation contract: no background-menu interception; lossless bounded paths."""
import json
from pathlib import Path
import tempfile
import unittest

import explorer_drop_probe as installer


class TransferInstallation(unittest.TestCase):
    def tearDown(self):
        installer.select_handler("probe")

    def test_production_identity_is_separate_and_registers_only_drop_handlers(self):
        installer.select_handler("transfer")
        keys = installer.expected(Path(r"D:\VartaCopy\vartacopy_transfer.dll"))
        self.assertIn("91DD2708", installer.CLSID)
        self.assertEqual(len(installer.HANDLERS), 5)
        self.assertTrue(all("\\DragDropHandlers\\" in key for key in installer.HANDLERS))
        self.assertFalse(any("ContextMenuHandlers" in key for key in keys))
        self.assertEqual(keys[installer.BASE + r"\shellex\MayChangeDefaultMenu"], {})
        self.assertEqual(keys[installer.BASE + r"\InprocServer32"]["ThreadingModel"], "Apartment")

    def test_configuration_preserves_native_names_and_does_not_create_a_journal(self):
        with tempfile.TemporaryDirectory() as root:
            gui = Path(root) / "дані-😀" / "vartacopy-gui.exe"
            gui.parent.mkdir()
            gui.write_bytes(b"fixture")
            journal = gui.parent / "перевірка.sqlite"
            config = json.loads(installer.configuration(gui, journal))
            self.assertEqual(config["version"], 1)
            self.assertEqual(config["gui"], installer.native_units(gui))
            self.assertEqual(config["journal"], installer.native_units(journal))
            self.assertFalse(journal.exists())
            self.assertEqual(installer.native_units(Path("D:\\дані-😀"))[-2:], [0xD83D, 0xDE00])

    def test_configuration_rejects_wrong_executable_and_unsupported_paths(self):
        for path in [Path("relative"), Path(r"\\host\share\file"), Path(r"D:\a\..\b")]:
            with self.assertRaises(ValueError):
                installer.native_units(path)
        with tempfile.TemporaryDirectory() as root:
            wrong = Path(root) / "other.exe"
            wrong.write_bytes(b"fixture")
            with self.assertRaises(ValueError):
                installer.configuration(wrong, None)


if __name__ == "__main__":
    unittest.main()

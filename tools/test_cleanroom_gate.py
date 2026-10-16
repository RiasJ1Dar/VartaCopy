"""Boundary regressions using synthetic, independently authored observations."""
import copy
import hashlib
import io
import json
from pathlib import Path
import tempfile
import unittest
from unittest import mock

import cleanroom_gate as gate


class BoundaryTests(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory()
        self.addCleanup(self.temporary.cleanup)
        self.root = Path(self.temporary.name)
        (self.root / "reference.bin").write_bytes(b"synthetic reference, not vendor software")
        (self.root / "record.json").write_text('{"result":"copied"}', encoding="utf-8")
        self.case = {
            "id": "CR-COPY-001", "title": "Copy a file", "area": "copy", "status": "observed",
            "fixture": ["An independently generated file"], "actions": ["Copy to an empty folder"],
            "observed": ["Destination bytes equal source bytes"],
            "requirements": ["Copy source bytes without changing the source"],
            "observer": "black-session-A", "observed_at": "2026-10-16T10:00:00Z",
            "evidence": [self.artifact("record.json")],
            "review": {"decision": "approved", "reviewer": "boundary-session-B",
                       "reviewed_at": "2026-10-16T11:00:00Z"},
        }
        self.data = {
            "schema_version": 1,
            "reference": {"name": "Synthetic test double", "version": "1",
                          "download_url": "https://example.invalid/fixture",
                          "binary": self.artifact("reference.bin")},
            "environment": {"os_build": "test", "filesystem": "test", "edition": "test",
                            "settings": ["test defaults"]},
            "cases": [self.case],
        }
        self.manifest = self.root / "manifest.json"

    def artifact(self, name):
        return {"path": name, "sha256": gate.sha256_file(self.root / name)}

    def run_gate(self):
        self.manifest.write_text(json.dumps(self.data), encoding="utf-8")
        return gate.contract(self.manifest, self.root)

    def test_export_excludes_raw_paths_and_review_internals_and_does_not_change_evidence(self):
        before = (self.root / "record.json").read_bytes()
        value = self.run_gate()
        output = self.root / "white.json"
        gate.publish(value, output)
        decoded = json.loads(output.read_text(encoding="utf-8"))
        self.assertEqual(decoded["cases"][0]["requirements"], self.case["requirements"])
        self.assertEqual(decoded["manifest_sha256"], hashlib.sha256(self.manifest.read_bytes()).hexdigest())
        self.assertNotIn("record.json", output.read_text(encoding="utf-8"))
        self.assertNotIn("observer", decoded["cases"][0])
        self.assertEqual((self.root / "record.json").read_bytes(), before)

    def test_plans_and_self_review_never_pass(self):
        for status in ("planned", "documented", "assumed"):
            with self.subTest(status=status):
                self.case["status"] = status
                with self.assertRaisesRegex(gate.Rejected, "only observed"):
                    self.run_gate()
        self.case["status"] = "observed"
        self.case["review"]["decision"] = "pending"
        with self.assertRaisesRegex(gate.Rejected, "different reviewer"):
            self.run_gate()
        self.case["review"]["decision"] = "approved"
        self.case["review"]["reviewer"] = " " + self.case["observer"].upper() + " "
        with self.assertRaisesRegex(gate.Rejected, "different reviewer"):
            self.run_gate()

    def test_changed_binary_and_changed_observation_are_rejected(self):
        for name in ("reference.bin", "record.json"):
            original = (self.root / name).read_bytes()
            (self.root / name).write_bytes(b"changed")
            with self.assertRaisesRegex(gate.Rejected, "content changed"):
                self.run_gate()
            (self.root / name).write_bytes(original)

    def test_paths_cannot_escape_black_room(self):
        for path in ("../outside.json", "/outside.json", "C:/outside.json", "..\\outside.json"):
            with self.subTest(path=path):
                self.case["evidence"][0]["path"] = path
                with self.assertRaises(gate.Rejected):
                    self.run_gate()

    def test_unknown_fields_and_duplicate_cases_are_rejected(self):
        self.case["vendor_source"] = "must not cross boundary"
        with self.assertRaisesRegex(gate.Rejected, "expected exactly"):
            self.run_gate()
        del self.case["vendor_source"]
        self.data["cases"].append(copy.deepcopy(self.case))
        with self.assertRaisesRegex(gate.Rejected, "duplicate identifier"):
            self.run_gate()

    def test_publishing_never_overwrites_an_existing_contract(self):
        output = self.root / "white.json"
        output.write_bytes(b"earlier approved contract")
        with self.assertRaises(FileExistsError):
            gate.publish(self.run_gate(), output)
        self.assertEqual(output.read_bytes(), b"earlier approved contract")
        self.assertEqual(list(self.root.glob(".cleanroom-*")), [])

    def test_duplicate_json_keys_are_rejected(self):
        self.manifest.write_text('{"schema_version":1,"schema_version":1}', encoding="utf-8")
        with self.assertRaisesRegex(gate.Rejected, "duplicate JSON key"):
            gate.contract(self.manifest, self.root)

    def test_oversize_reference_and_evidence_are_rejected_without_opening(self):
        for name in ("reference.bin", "record.json"):
            with self.subTest(name=name):
                with (self.root / name).open("r+b") as stream:
                    stream.truncate(gate.ARTIFACT_BYTES + 1)
                original_open = Path.open

                def guarded_open(path, *args, **kwargs):
                    self.assertNotEqual(path, self.root / name, "oversize artifact was opened")
                    return original_open(path, *args, **kwargs)

                with mock.patch.object(Path, "open", guarded_open):
                    with self.assertRaisesRegex(gate.Rejected, "artifact exceeds"):
                        self.run_gate()
                (self.root / name).write_bytes(b"synthetic")
                if name == "reference.bin":
                    self.data["reference"]["binary"] = self.artifact(name)

    def test_repeated_evidence_and_reference_share_the_contract_budget(self):
        reference_size = (self.root / "reference.bin").stat().st_size
        evidence_size = (self.root / "record.json").stat().st_size
        self.case["evidence"] *= 3
        with mock.patch.object(gate, "CONTRACT_BYTES", reference_size + evidence_size * 2):
            with self.assertRaisesRegex(gate.Rejected, "contract artifacts exceed"):
                self.run_gate()

    def test_growing_stream_is_bounded_even_when_reported_size_fits(self):
        path = self.root / "record.json"
        for artifact_cap, contract_cap, message in (
            (64, 128, "artifact exceeds"), (128, 64, "contract artifacts exceed")
        ):
            with self.subTest(message=message):
                stream = io.BytesIO(b"x" * 1024)
                # getvalue is unavailable once the context manager closes the stream.
                reads = []
                original_read = stream.read

                def read(size):
                    block = original_read(size)
                    reads.append(len(block))
                    return block

                stream.read = read
                with (mock.patch.object(gate, "ARTIFACT_BYTES", artifact_cap),
                      mock.patch.object(gate, "CONTRACT_BYTES", contract_cap),
                      mock.patch.object(Path, "open", return_value=stream)):
                    with self.assertRaisesRegex(gate.Rejected, message):
                        gate.sha256_file(path)
                self.assertEqual(sum(reads), min(artifact_cap, contract_cap) + 1)

    def test_exact_artifact_and_aggregate_limits_are_accepted(self):
        reference_size = (self.root / "reference.bin").stat().st_size
        evidence_size = (self.root / "record.json").stat().st_size
        with (mock.patch.object(gate, "ARTIFACT_BYTES", reference_size),
              mock.patch.object(gate, "CONTRACT_BYTES", reference_size + evidence_size)):
            self.assertEqual(len(self.run_gate()["cases"]), 1)

    def test_manifest_size_is_bounded_before_json_parsing(self):
        self.manifest.write_bytes(b" " * (gate.MANIFEST_BYTES + 1))
        with mock.patch.object(gate.json, "loads", side_effect=AssertionError("parsed oversize JSON")):
            with self.assertRaisesRegex(gate.Rejected, "manifest exceeds 1 MiB"):
                gate.contract(self.manifest, self.root)


if __name__ == "__main__":
    unittest.main()

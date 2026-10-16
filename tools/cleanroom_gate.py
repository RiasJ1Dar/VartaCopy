"""Publish a reviewed behavioral contract without copying black-room artifacts.

This is a format/provenance gate, not an isolation or identity-authentication system.
Human review must check that prose describes behavior rather than implementation.
"""
import argparse
import hashlib
import json
import os
from pathlib import Path, PurePosixPath
import re
import tempfile


# Allow ordinary installer/runtime binaries (~15 MiB) and small observation records.
# Repeated artifacts count again; reference and evidence share one contract budget.
# A single sentinel byte detects a growing stream: at most cap + 1 byte is read
# per artifact and per contract, including on rejection.
MANIFEST_BYTES = 1024 * 1024
ARTIFACT_BYTES = 64 * 1024 * 1024
CONTRACT_BYTES = 128 * 1024 * 1024


class Rejected(ValueError):
    pass


def exact(value, fields, label):
    if not isinstance(value, dict) or set(value) != set(fields):
        raise Rejected(f"{label}: expected exactly {', '.join(fields)}")


def text(value, label):
    if not isinstance(value, str) or not value.strip() or len(value) > 4000:
        raise Rejected(f"{label}: expected 1..4000 characters")
    if any(ord(c) < 32 and c not in "\n\t" for c in value):
        raise Rejected(f"{label}: control character")
    return value


def lines(value, label):
    if not isinstance(value, list) or not 1 <= len(value) <= 32:
        raise Rejected(f"{label}: expected 1..32 items")
    return [text(item, label) for item in value]


class ReadBudget:
    def __init__(self):
        self.used = 0


def sha256_file(path, budget=None):
    """Hash bounded bytes; never trust a pre-read size check to bound a stream."""
    if budget is None:
        budget = ReadBudget()
    size = path.stat().st_size
    if size > ARTIFACT_BYTES:
        raise Rejected("artifact exceeds 64 MiB")
    if size > CONTRACT_BYTES - budget.used:
        raise Rejected("contract artifacts exceed 128 MiB")
    digest = hashlib.sha256()
    consumed = 0
    # Unbuffered I/O prevents hidden read-ahead beyond the remaining byte budget.
    with path.open("rb", buffering=0) as stream:
        while True:
            allowance = min(ARTIFACT_BYTES - consumed, CONTRACT_BYTES - budget.used)
            block = stream.read(min(1024 * 1024, allowance + 1))
            consumed += len(block)
            budget.used += len(block)
            if consumed > ARTIFACT_BYTES:
                raise Rejected("artifact exceeds 64 MiB while reading")
            if budget.used > CONTRACT_BYTES:
                raise Rejected("contract artifacts exceed 128 MiB while reading")
            if not block:
                break
            digest.update(block)
    return digest.hexdigest()


def artifact(root, value, budget):
    exact(value, ("path", "sha256"), "artifact")
    name = text(value["path"], "artifact.path")
    relative = PurePosixPath(name)
    if (relative.is_absolute() or ".." in relative.parts
            or any(c in name for c in "\\:")):
        raise Rejected("artifact.path: expected a relative portable path within black room")
    path = root.joinpath(*relative.parts).resolve(strict=True)
    if not path.is_relative_to(root) or not path.is_file():
        raise Rejected("artifact.path: escaped black room or not a regular file")
    expected = value["sha256"]
    if not isinstance(expected, str) or not re.fullmatch(r"[0-9a-f]{64}", expected):
        raise Rejected("artifact.sha256: invalid digest")
    if sha256_file(path, budget) != expected:
        raise Rejected(f"artifact.sha256: content changed: {name}")
    return expected


def contract(manifest, black_root):
    """Read a bounded manifest and return only the permitted white-room fields."""
    with Path(manifest).open("rb") as stream:
        raw = stream.read(MANIFEST_BYTES + 1)
    if len(raw) > MANIFEST_BYTES:
        raise Rejected("manifest exceeds 1 MiB")
    # Reject duplicate JSON keys instead of accepting the last, possibly misleading value.
    def pairs(items):
        result = {}
        for key, value in items:
            if key in result:
                raise Rejected(f"duplicate JSON key: {key}")
            result[key] = value
        return result

    data = json.loads(raw.decode("utf-8"), object_pairs_hook=pairs)
    exact(data, ("schema_version", "reference", "environment", "cases"), "manifest")
    if type(data["schema_version"]) is not int or data["schema_version"] != 1:
        raise Rejected("unsupported manifest schema_version")
    root = Path(black_root).resolve(strict=True)
    reference = data["reference"]
    exact(reference, ("name", "version", "download_url", "binary"), "reference")
    for key in ("name", "version", "download_url"):
        text(reference[key], f"reference.{key}")
    budget = ReadBudget()
    binary_digest = artifact(root, reference["binary"], budget)
    environment = data["environment"]
    exact(environment, ("os_build", "filesystem", "edition", "settings"), "environment")
    for key in ("os_build", "filesystem", "edition"):
        text(environment[key], f"environment.{key}")
    lines(environment["settings"], "environment.settings")
    cases = data["cases"]
    if not isinstance(cases, list) or not 1 <= len(cases) <= 64:
        raise Rejected("cases: expected 1..64 cases")
    published = []
    identifiers = set()
    for case in cases:
        exact(case, ("id", "title", "area", "status", "fixture", "actions", "observed",
                     "requirements", "observer", "observed_at", "evidence", "review"), "case")
        identifier = text(case["id"], "case.id")
        if not re.fullmatch(r"CR-[A-Z]+-[0-9]{3}", identifier) or identifier in identifiers:
            raise Rejected("case.id: invalid or duplicate identifier")
        identifiers.add(identifier)
        if case["status"] != "observed":
            raise Rejected(f"{identifier}: only observed cases may cross the boundary")
        for key in ("title", "area", "observer", "observed_at"):
            text(case[key], f"case.{key}")
        for key in ("fixture", "actions", "observed", "requirements"):
            lines(case[key], f"case.{key}")
        review = case["review"]
        exact(review, ("decision", "reviewer", "reviewed_at"), "review")
        reviewer = text(review["reviewer"], "review.reviewer")
        text(review["reviewed_at"], "review.reviewed_at")
        if (review["decision"] != "approved"
                or reviewer.strip().casefold() == case["observer"].strip().casefold()):
            raise Rejected(f"{identifier}: a different reviewer must approve the case")
        evidence = case["evidence"]
        if not isinstance(evidence, list) or not 1 <= len(evidence) <= 16:
            raise Rejected(f"{identifier}: expected 1..16 evidence artifacts")
        digests = [artifact(root, value, budget) for value in evidence]
        entry = {key: case[key] for key in (
            "id", "title", "area", "fixture", "actions", "observed", "requirements"
        )}
        entry["provenance"] = {"evidence_sha256": digests}
        published.append(entry)
    return {
        "schema_version": 1,
        "reference": {"name": reference["name"], "version": reference["version"],
                      "binary_sha256": binary_digest},
        "environment": environment,
        "cases": published,
        "manifest_sha256": hashlib.sha256(raw).hexdigest(),
    }


def publish(value, output):
    """Stage and flush, then publish without overwriting an existing contract."""
    output = Path(output)
    raw = (json.dumps(value, ensure_ascii=False, indent=2) + "\n").encode("utf-8")
    temporary = None
    try:
        with tempfile.NamedTemporaryFile(dir=output.parent, prefix=".cleanroom-", delete=False) as stream:
            temporary = Path(stream.name)
            stream.write(raw)
            stream.flush()
            os.fsync(stream.fileno())
        os.link(temporary, output)  # Exclusive publication; a preexisting target is preserved.
    finally:
        if temporary is not None:
            temporary.unlink()


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("manifest", type=Path)
    parser.add_argument("--black-root", type=Path, required=True)
    parser.add_argument("--output", type=Path, help="Omit to validate without publishing")
    args = parser.parse_args()
    try:
        value = contract(args.manifest, args.black_root)
        if args.output is not None:
            publish(value, args.output)
        print(json.dumps({"status": "published" if args.output else "validated",
                          "cases": len(value["cases"]),
                          "manifest_sha256": value["manifest_sha256"]}))
    except (OSError, ValueError) as error:
        parser.exit(1, f"cleanroom: {error}\n")


if __name__ == "__main__":
    main()

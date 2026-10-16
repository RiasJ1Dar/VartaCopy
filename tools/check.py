"""Run the supported workspace checks with one consistent Cargo feature graph."""
from __future__ import annotations

import argparse
from pathlib import Path
import subprocess
import sys

ROOT = Path(__file__).resolve().parents[1]
TARGET = "x86_64-pc-windows-msvc"


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--offline", action="store_true", help="Use cached Cargo dependencies only")
    args = parser.parse_args()
    cargo = ["--locked", "--target", TARGET]
    if args.offline:
        cargo.append("--offline")
    commands = [
        ["cargo", "fmt", "--all", "--check"],
        ["cargo", "test", "--workspace", "--all-features", *cargo],
        ["cargo", "clippy", "--workspace", "--all-targets", "--all-features", *cargo, "--", "-D", "warnings"],
        [sys.executable, "-m", "unittest", "discover", "-s", "tools", "-p", "test_*.py"],
    ]
    for command in commands:
        print("Running: " + " ".join(command), flush=True)
        try:
            result = subprocess.run(command, cwd=ROOT, shell=False, check=False)
        except OSError as error:
            print(f"Cannot start check: {error}", file=sys.stderr)
            return 1
        if result.returncode:
            print(f"Check failed with exit code {result.returncode}", file=sys.stderr)
            return result.returncode if 0 < result.returncode < 256 else 1
    print("Workspace checks passed", flush=True)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())

"""Offline, bidirectional archive compatibility check. Python is validation only.

Usage: python tools/check_rust_compat.py target/debug/opixiv[.exe]
"""
from __future__ import annotations

import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile

ROOT = Path(__file__).resolve().parents[1]


def run(*args: str | Path) -> dict:
    result = subprocess.run([str(arg) for arg in args], cwd=ROOT, text=True,
                            encoding="utf-8", capture_output=True, check=True,
                            env={**os.environ, "PYTHONUTF8": "1"})
    return json.loads(result.stdout)


def main() -> None:
    binary = Path(sys.argv[1]).resolve()
    legacy = [sys.executable, ROOT / "run.py"]
    fixture = ROOT / "examples/synthetic-fixture.json"
    with tempfile.TemporaryDirectory(prefix="opixiv-compat-") as temporary:
        work = Path(temporary)
        rust_archive, python_archive = work / "Rust保存", work / "Python保存"
        first = run(binary, "fetch", "user:9001", "art:1001", "--demo", "--all",
                    "--jobs", "2", "--out", rust_archive)
        assert first["counts"] == {"complete": 4}, first
        assert run(*legacy, "verify", rust_archive)["valid"] == 4
        resumed = run(*legacy, "fetch", "user:9001", "--fixture", fixture, "--all", "--out", rust_archive)
        assert resumed["already_complete"] == 4 and resumed["selected"] == 0, resumed
        assert run(binary, "pack", rust_archive, work / "rust-export.zip")["complete_works"] == 4

        run(*legacy, "fetch", "user:9001", "--fixture", fixture, "--all", "--jobs", "2", "--out", python_archive)
        assert run(binary, "verify", python_archive)["valid"] == 4
        resumed = run(binary, "fetch", "user:9001", "--demo", "--all", "--out", python_archive)
        assert resumed["already_complete"] == 4 and resumed["selected"] == 0, resumed
        assert run(*legacy, "pack", python_archive, work / "python-export.zip")["complete_works"] == 4

        # Compare source bytes and declared file sets; generated HTML/layout can differ.
        for name in ["art_1001", "art_1002", "art_1003", "novel_1004"]:
            rust_dir, python_dir = rust_archive / "works" / name, python_archive / "works" / name
            rust_files = {entry["path"] for entry in json.loads((rust_dir / "manifest.json").read_text(encoding="utf-8"))["files"]}
            python_files = {entry["path"] for entry in json.loads((python_dir / "manifest.json").read_text(encoding="utf-8"))["files"]}
            assert rust_files == python_files, (name, rust_files ^ python_files)
            for relative in rust_files - {"raw.json", "metadata.json", "pages.json", "frames.json", "index.html"}:
                assert (rust_dir / relative).read_bytes() == (python_dir / relative).read_bytes(), (name, relative)
            assert json.loads((rust_dir / "raw.json").read_text(encoding="utf-8")) == json.loads((python_dir / "raw.json").read_text(encoding="utf-8"))
        # A saved asset changed after completion must prevent export and be repaired.
        image = rust_archive / "works/art_1001/images/0000.png"
        image.write_bytes(b"simulated corruption")
        broken = subprocess.run([str(binary), "verify", str(rust_archive)], cwd=ROOT, capture_output=True, text=True, encoding="utf-8")
        assert broken.returncode == 1 and len(json.loads(broken.stdout)["problems"]) == 1
        repaired = run(binary, "fetch", "user:9001", "--demo", "--all", "--out", rust_archive)
        assert repaired["already_complete"] == 3 and repaired["counts"] == {"complete": 1}
        assert run(*legacy, "verify", rust_archive)["valid"] == 4
    print(json.dumps({"rust_to_python": "passed", "python_to_rust": "passed",
                      "original_bytes_and_file_sets": "matched", "corruption_repair": "passed"}, indent=2))


if __name__ == "__main__":
    main()

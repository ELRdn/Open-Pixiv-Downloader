"""Kill a real downloader process after a body checkpoint, then resume offline.

Usage: python tools/check_crash_resume.py target/release/opixiv[.exe] [--out result.json]
Uses generated 4000-work fixtures only. No Pixiv/network requests.
"""
from __future__ import annotations

import argparse
import json
from pathlib import Path
import platform
import subprocess
import tempfile
import time

from check_bulk import ROOT, make_fixture, write_fixture


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("binary", type=Path)
    parser.add_argument("--out", type=Path)
    args = parser.parse_args()
    binary = args.binary.resolve()
    with tempfile.TemporaryDirectory(prefix="opixiv-crash-") as temporary:
        directory = Path(temporary)
        fixture, archive = directory / "fixture.json", directory / "archive"
        write_fixture(fixture, make_fixture(4000, shared=True))
        common = ["--fixture", str(fixture), "--all", "--jobs", "4",
                  "--request-budget", "20000", "--out", str(archive)]
        with (directory / "stdout").open("wb") as stdout, (directory / "stderr").open("wb") as stderr:
            process = subprocess.Popen([str(binary), "fetch", "user:9001", *common],
                                       cwd=ROOT, stdout=stdout, stderr=stderr)
            try:
                deadline = time.monotonic() + 30
                while time.monotonic() < deadline:
                    if process.poll() is not None:
                        raise RuntimeError("Job exited before the kill checkpoint")
                    if (archive / "job.json").exists() and next((archive / "works").glob("novel_*/plain.txt"), None):
                        break
                    time.sleep(0.01)
                else:
                    raise RuntimeError("No saved body checkpoint within 30 seconds")
                assert (archive / ".run.lock").is_file()
            finally:
                if process.poll() is None:
                    process.kill()
                process.wait(timeout=10)
        assert (archive / ".run.lock").is_file(), "Hard kill should leave the versioned sentinel"
        resumed = subprocess.run([str(binary), "fetch", "--resume", *common],
                                 cwd=ROOT, capture_output=True, text=True, encoding="utf-8", timeout=300)
        if resumed.returncode:
            raise RuntimeError(f"Resume failed: {resumed.stderr[-1000:]}")
        report = json.loads(resumed.stdout)
        assert report["status"] == "complete" and report["full_complete_count"] == 4000
        assert not (archive / ".run.lock").exists()
        assert (archive / ".archive.lock").is_file()
        evidence = {
            "version": report["tool_version"], "platform": platform.system(),
            "scope": "Real process kill after a saved body; generated 4000-work fixture; no network",
            "hard_kill_exit_code": process.returncode, "resume_status": report["status"],
            "full_complete_count": report["full_complete_count"], "requests_on_resume": report["requests"],
            "stale_versioned_sentinel_recovered": True, "manual_lock_removal": False,
            "compatibility_sentinel_removed_on_exit": True, "persistent_os_guard_file_remains": True,
        }
    text = json.dumps(evidence, ensure_ascii=False, indent=2) + "\n"
    if args.out:
        args.out.parent.mkdir(parents=True, exist_ok=True)
        args.out.write_text(text, encoding="utf-8")
    print(text, end="")


if __name__ == "__main__":
    main()

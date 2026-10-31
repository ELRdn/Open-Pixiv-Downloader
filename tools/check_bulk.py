"""Offline end-to-end checks for bulk archiving and missing-only repair.

Usage: python tools/check_bulk.py target/release/opixiv[.exe] [--count 4000]

All API/media responses are generated locally through --fixture. This script
never starts --online or downloads Pixiv works. Timings measure this host's
fixture parsing, scheduling, hashing, SQLite, copying and disk synchronization;
they do not measure a real server, TLS, network rate limits, or Windows unless
run on Windows. Temporary archives are deleted after the checks.
"""
from __future__ import annotations

import argparse
import base64
from copy import deepcopy
import hashlib
import json
import os
from pathlib import Path
import platform
import subprocess
import tempfile
import time
from typing import Any

ROOT = Path(__file__).resolve().parents[1]
BASE = json.loads((ROOT / "examples/synthetic-fixture.json").read_text(encoding="utf-8"))
AUTHOR = "9001"
FIRST_ID = 200000
COVER_BYTES = base64.b64decode(next(value for key, value in BASE["assets"].items() if "_cover." in key))


class CheckFailure(RuntimeError):
    pass


def require(condition: bool, description: str) -> None:
    if not condition:
        raise CheckFailure(description)


def excerpt(report: dict[str, Any]) -> str:
    """A small error diagnostic; never print thousands of result rows."""
    keys = ("status", "reason", "stop_reason", "requests", "counts", "selected",
            "already_complete", "unattempted_selected", "body_ready_count",
            "full_complete_count", "valid")
    return json.dumps({key: report[key] for key in keys if key in report}, ensure_ascii=False)


def body_for(work_id: int) -> str:
    return f"合成小説 {work_id}\n" + "独立した本文の保存と再開を検証します。\n" * 100


def cover_url(work_id: int, shared: bool) -> str:
    key = "shared" if shared else str(work_id)
    return f"https://i.pximg.net/synthetic/open-pixiv-bulk/{key}_cover.png"


def make_fixture(count: int, *, shared: bool, missing: bool = False) -> dict[str, Any]:
    responses: dict[str, Any] = {}
    ids: dict[str, Any] = {}
    assets: dict[str, str] = {}
    encoded_cover = base64.b64encode(COVER_BYTES).decode("ascii")
    for work_id in range(FIRST_ID, FIRST_ID + count):
        item = deepcopy(BASE["responses"]["/ajax/novel/1004"])
        body = body_for(work_id)
        item.update({"id": str(work_id), "title": f"合成小説 {work_id}",
                     "content": body, "textCount": len(body),
                     "coverUrl": cover_url(work_id, shared),
                     "textEmbeddedImages": {}, "seriesNavData": None})
        responses[f"/ajax/novel/{work_id}"] = item
        ids[str(work_id)] = None
        if not missing:
            assets[item["coverUrl"]] = encoded_cover
    responses[f"/ajax/user/{AUTHOR}/profile/all"] = {"illusts": {}, "manga": {}, "novels": ids}
    return {"synthetic": True, "description": "Generated bulk acceptance fixture, no live media",
            "responses": responses, "assets": assets}


def write_fixture(path: Path, fixture: dict[str, Any]) -> None:
    path.write_text(json.dumps(fixture, ensure_ascii=False), encoding="utf-8")


def digest(data: bytes) -> str:
    return hashlib.sha256(data).hexdigest()


class Runner:
    def __init__(self, binary: Path, timeout: float) -> None:
        self.binary = binary
        self.timeout = timeout
        self.records: list[dict[str, Any]] = []

    def run(self, label: str, *arguments: object) -> dict[str, Any]:
        started = time.perf_counter()
        completed = subprocess.run(
            [str(self.binary), *(str(argument) for argument in arguments)],
            cwd=ROOT, capture_output=True, encoding="utf-8", errors="strict",
            timeout=self.timeout, env={**os.environ, "PYTHONUTF8": "1", "OPIXIV_NO_UPDATE_CHECK": "1"},
        )
        try:
            report = json.loads(completed.stdout)
        except (ValueError, TypeError) as error:
            raise CheckFailure(
                f"{label}: expected JSON, exit={completed.returncode}; "
                f"stderr={completed.stderr[-1500:]}"
            ) from error
        require(isinstance(report, dict), f"{label}: response is not an object")
        record: dict[str, Any] = {
            "case": label, "exit_code": completed.returncode,
            "wall_seconds": time.perf_counter() - started,
        }
        for key in ("tool_version", "status", "requests", "counts", "already_complete", "stop_reason",
                    "elapsed_seconds", "body_ready_count", "full_complete_count"):
            if key in report:
                record[key] = report[key]
        self.records.append(record)
        require(completed.returncode in (0, 1, 130),
                f"{label}: process failed unexpectedly: {excerpt(report)}")
        if report.get("status") in ("complete", "planned"):
            require(completed.returncode == 0,
                    f"{label}: successful JSON returned failure exit {completed.returncode}")
        return report

    def fetch(self, label: str, fixture: Path, archive: Path, *, budget: int = 20000,
              resume: bool = False) -> dict[str, Any]:
        arguments: list[object] = ["fetch"]
        if resume:
            arguments.append("--resume")
        else:
            arguments.append(f"user:{AUTHOR}")
        arguments.extend(["--fixture", fixture, "--all", "--jobs", "4",
                          "--request-budget", budget, "--out", archive])
        return self.run(label, *arguments)


def check_complete(report: dict[str, Any], label: str) -> None:
    require(report.get("status") == "complete", f"{label}: {excerpt(report)}")
    require(not report.get("collection", {}).get("problems"), f"{label}: collection problems")


def check_files(archive: Path, count: int) -> None:
    for work_id in range(FIRST_ID, FIRST_ID + count):
        work = archive / "works" / f"novel_{work_id}"
        expected = body_for(work_id).encode("utf-8")
        require((work / "source.txt").read_bytes() == expected, f"source changed: {work_id}")
        require((work / "plain.txt").read_bytes() == expected, f"plain text changed: {work_id}")
        aggregate = archive / "works" / "novel" / f"novel_{work_id}.txt"
        require(aggregate.read_bytes() == expected, f"aggregate changed: {work_id}")
        require((work / "cover.png").read_bytes() == COVER_BYTES, f"cover changed: {work_id}")
        manifest = json.loads((work / "manifest.json").read_text(encoding="utf-8"))
        require(manifest["status"] == "complete", f"incomplete manifest: {work_id}")
        for entry in manifest["files"]:
            data = (work / entry["path"]).read_bytes()
            require(len(data) == entry["bytes"] and digest(data) == entry["sha256"],
                    f"manifest bytes/hash mismatch: {work_id}/{entry['path']}")


def run_checks(runner: Runner, parent: Path, count: int) -> None:
    parent.mkdir(parents=True, exist_ok=True)
    shared_fixture = parent / "shared.json"
    shared_archive = parent / "共有表紙の保存"
    write_fixture(shared_fixture, make_fixture(count, shared=True))
    first = runner.fetch("shared_covers_fresh", shared_fixture, shared_archive)
    check_complete(first, "shared covers")
    require(first["requests"] == count + 2,
            f"shared covers must need author + {count} detail + one cover request: {excerpt(first)}")
    check_files(shared_archive, count)

    resumed = runner.fetch("shared_covers_resume_complete", shared_fixture, shared_archive, resume=True)
    check_complete(resumed, "completed resume")
    require(resumed["requests"] == 0, f"completed resume fetched remote data: {excerpt(resumed)}")

    # These are per-work original files, each independently copied from the cache.
    first_cover = shared_archive / "works" / f"novel_{FIRST_ID}" / "cover.png"
    second_cover = shared_archive / "works" / f"novel_{FIRST_ID + 1}" / "cover.png"
    first_cover.write_bytes(b"intentional corrupt original cover")
    require(second_cover.read_bytes() == COVER_BYTES, "per-work covers share mutable storage")
    broken = runner.run("corrupt_original_detected", "verify", shared_archive)
    require(bool(broken.get("problems")), "verification accepted corrupt original cover")
    repaired = runner.fetch("corrupt_cover_missing_only_repair", shared_fixture, shared_archive, resume=True)
    check_complete(repaired, "cover repair")
    require(repaired["requests"] == 1, f"cover repair repeated details: {excerpt(repaired)}")
    require(first_cover.read_bytes() == COVER_BYTES, "corrupt cover not repaired")

    flat = shared_archive / "works" / "novel" / f"novel_{FIRST_ID}.txt"
    plain = shared_archive / "works" / f"novel_{FIRST_ID}" / "plain.txt"
    flat.write_bytes(b"user-edited flat text")
    require(plain.read_bytes() == body_for(FIRST_ID).encode("utf-8"), "flat copy edit changed original")
    collision = runner.run("edited_flat_copy_preserved", "collect", shared_archive)
    require(collision.get("status") == "partial", f"edited flat copy not reported: {excerpt(collision)}")
    require(flat.read_bytes() == b"user-edited flat text", "edited flat copy overwritten")
    good = runner.run("originals_remain_valid_after_flat_edit", "verify", shared_archive)
    require(good.get("valid") == count and not good.get("problems"), "flat edit invalidated originals")
    flat.unlink()
    collected = runner.run("deleted_flat_copy_restored_offline", "collect", shared_archive)
    check_complete(collected, "flat restore")
    require(flat.read_bytes() == body_for(FIRST_ID).encode("utf-8"), "flat copy not restored")

    unique_fixture = parent / "unique.json"
    unique_archive = parent / "個別表紙の保存"
    write_fixture(unique_fixture, make_fixture(count, shared=False))
    unique = runner.fetch("unique_covers_fresh", unique_fixture, unique_archive)
    check_complete(unique, "unique covers")
    require(unique["requests"] == count * 2 + 1,
            f"unique cover URLs unexpectedly omitted or repeated: {excerpt(unique)}")
    check_files(unique_archive, count)

    fault_count = max(8, min(count, 16))
    fault_fixture = parent / "repair.json"
    fault_archive = parent / "欠損素材だけの修復"
    write_fixture(fault_fixture, make_fixture(fault_count, shared=True, missing=True))
    missing = runner.fetch("missing_shared_cover_body_preserved", fault_fixture, fault_archive)
    require(missing.get("status") == "partial", f"missing cover marked complete: {excerpt(missing)}")
    require(missing.get("body_ready_count") == fault_count,
            f"failed cover hid ready bodies: {excerpt(missing)}")
    require(missing.get("full_complete_count") == 0,
            f"failed cover counted as fully complete: {excerpt(missing)}")
    require(missing["requests"] == fault_count + 2,
            f"failed shared cover retried once per work: {excerpt(missing)}")
    for work_id in range(FIRST_ID, FIRST_ID + fault_count):
        require((fault_archive / "works" / "novel" / f"novel_{work_id}.txt").read_bytes()
                == body_for(work_id).encode("utf-8"), "body unavailable after cover failure")
    write_fixture(fault_fixture, make_fixture(fault_count, shared=True))
    repaired = runner.fetch("missing_shared_cover_repair_without_body_requests", fault_fixture,
                            fault_archive, resume=True)
    check_complete(repaired, "missing cover repair")
    require(repaired["requests"] == 1, f"missing cover repair fetched body/list: {excerpt(repaired)}")
    check_files(fault_archive, fault_count)

    exact_fixture = parent / "exact-fit.json"
    exact_archive = parent / "要求予算ぴったりで完走"
    exact_count = 8
    write_fixture(exact_fixture, make_fixture(exact_count, shared=True))
    exact = runner.fetch("exact_fit_budget_finishes_local_stages", exact_fixture, exact_archive,
                         budget=exact_count + 2)
    check_complete(exact, "exact-fit request budget")
    require(exact["requests"] == exact_count + 2,
            f"exact-fit budget counted cache hits: {excerpt(exact)}")
    require(exact.get("stop_reason") is None,
            f"complete job incorrectly marked budget-stopped: {excerpt(exact)}")
    check_files(exact_archive, exact_count)

    budget_fixture = parent / "budget.json"
    budget_archive = parent / "予算停止と再開"
    write_fixture(budget_fixture, make_fixture(fault_count, shared=True))
    paused = runner.fetch("budget_exhaustion_graceful_pause", budget_fixture, budget_archive, budget=4)
    require(paused.get("status") == "paused", f"budget exhaustion not paused: {excerpt(paused)}")
    require(paused["requests"] <= 4, f"parallel workers exceeded request budget: {excerpt(paused)}")
    require((paused.get("stop_reason") or {}).get("kind") == "request_budget",
            f"budget exhaustion reason lost: {excerpt(paused)}")
    resumed = runner.fetch("budget_resume_saved_selection", budget_fixture, budget_archive, resume=True)
    check_complete(resumed, "budget resume")
    check_files(budget_archive, fault_count)


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("binary", type=Path)
    parser.add_argument("--count", type=int, default=4000, help="novels in each large case (default: 4000)")
    parser.add_argument("--timeout", type=float, default=300, help="per-process timeout in seconds")
    parser.add_argument("--out", type=Path, help="write the compact result JSON here")
    arguments = parser.parse_args()
    if not 2 <= arguments.count <= 10000:
        parser.error("--count must be between 2 and 10000")
    binary = arguments.binary.resolve()
    if not binary.is_file():
        parser.error(f"binary not found: {binary}")
    runner = Runner(binary, arguments.timeout)
    started = time.perf_counter()
    output: dict[str, Any] = {
        "scope": "Synthetic offline fixture only; no Pixiv HTTP/TLS or network rate-limit benchmark",
        "platform": platform.system(), "novels_per_large_case": arguments.count,
        "body_utf8_bytes": len(body_for(FIRST_ID).encode("utf-8")), "checks": runner.records,
    }
    try:
        with tempfile.TemporaryDirectory(prefix="opixiv-bulk-") as temporary:
            run_checks(runner, Path(temporary) / "日本語 空白", arguments.count)
        output["status"] = "passed"
    except (CheckFailure, OSError, subprocess.TimeoutExpired) as error:
        output["status"] = "failed"
        output["error"] = str(error)
    output["wall_seconds"] = time.perf_counter() - started
    data = json.dumps(output, ensure_ascii=False, indent=2) + "\n"
    if arguments.out:
        arguments.out.parent.mkdir(parents=True, exist_ok=True)
        arguments.out.write_text(data, encoding="utf-8")
    print(data, end="")
    return 0 if output["status"] == "passed" else 1


if __name__ == "__main__":
    raise SystemExit(main())

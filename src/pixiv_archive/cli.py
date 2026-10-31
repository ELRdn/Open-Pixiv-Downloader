"""Command-line entry point. Online mode is explicit; demo mode has no network."""
from __future__ import annotations

import argparse
import json
import sys
from collections import Counter
from pathlib import Path

from . import __version__
from .archive import Archive, atomic_bytes, discover, json_bytes, now, parse_target, safe_path
from .jobs import download_many
from .net import ArchiveError, FixtureClient, HttpClient, RemoteStop


def parser() -> argparse.ArgumentParser:
    result = argparse.ArgumentParser(description="Local Pixiv archival research prototype (alpha)")
    result.add_argument("--version", action="version", version=__version__)
    commands = result.add_subparsers(dest="command", required=True)
    for name, help_text in (("plan", "List target IDs without downloading media"),
                            ("fetch", "Archive a bounded set of public/general-audience works")):
        command = commands.add_parser(name, help=help_text)
        command.add_argument("targets", nargs="*", help="user:ID, art:ID, novel:ID, series:ID or pixiv HTTPS URL")
        command.add_argument("--out", type=Path, default=Path("archive"))
        command.add_argument("--types", choices=("all", "art", "novel"), default="all")
        command.add_argument("--input", type=Path, help="UTF-8 file with one target per line")
        command.add_argument("--jobs", type=int, choices=range(1, 5), default=1, help="Concurrent works (1–4); request interval remains global")
        selection = command.add_mutually_exclusive_group()
        selection.add_argument("--all", action="store_true", help="Select all discovered unfinished works within the request budget")
        selection.add_argument("--limit", type=int, default=100, help="Maximum works per run (1–1000, default 100)")
        transport = command.add_mutually_exclusive_group(required=True)
        transport.add_argument("--fixture", type=Path, help="Synthetic offline response fixture")
        transport.add_argument("--online", action="store_true", help="Use unauthenticated pixiv HTTPS requests")
        command.add_argument("--interval", type=float, default=2.0, help="Minimum request interval, >=2 seconds")
        command.add_argument("--request-budget", type=int, default=500)
        command.add_argument("--max-file-mib", type=int, default=256)
        command.add_argument("--retry-unavailable", action="store_true", help="Recheck previously skipped or unavailable IDs")
    commands.add_parser("interactive", help="Open the guided terminal interface")
    verify = commands.add_parser("verify", help="Verify saved file checksums without network access")
    verify.add_argument("out", type=Path)
    pack = commands.add_parser("pack", help="Export complete works only to a ZIP64-compatible archive")
    pack.add_argument("out", type=Path)
    pack.add_argument("zip", type=Path)
    return result


def output(value):
    print(json.dumps(value, ensure_ascii=False, indent=2))


def main(argv: list[str] | None = None, *, reporter=None) -> int:
    argv = sys.argv[1:] if argv is None else argv
    if not argv or argv == ["interactive"]:
        if not argv and not sys.stdin.isatty():
            print("対話モードはターミナルで python run.py を実行してください。コマンド一覧: --help", file=sys.stderr)
            return 2
        from .interactive import run
        return run(main)
    args = parser().parse_args(argv)

    def show(value):
        if reporter:
            reporter("result", value)
        else:
            output(value)

    try:
        if args.command in ("verify", "pack"):
            if not (args.out / "state.sqlite3").is_file():
                raise ArchiveError("Archive database does not exist")
            with Archive(args.out) as archive:
                result = archive.verify_all() if args.command == "verify" else archive.pack(args.zip)
            show(result)
            return 1 if result.get("problems") else 0
        if not 1 <= args.limit <= 1000:
            raise ArchiveError("--limit must be between 1 and 1000")
        if not 1 <= args.request_budget <= 5000 or not 1 <= args.max_file_mib <= 1024:
            raise ArchiveError("Use request budget 1–5000 and file limit 1–1024 MiB")
        values = list(args.targets)
        if args.input:
            if args.input.stat().st_size > 2 * 1024 * 1024:
                raise ArchiveError("Input list exceeds 2 MiB")
            values.extend(line.strip() for line in args.input.read_text(encoding="utf-8-sig").splitlines()
                          if line.strip() and not line.lstrip().startswith("#"))
        if not values or len(values) > 1000:
            raise ArchiveError("Supply between 1 and 1000 targets as arguments or --input")
        targets = list(dict.fromkeys(parse_target(value) for value in values))
        kind, ident = targets[0]
        types = {"art", "novel"} if args.types == "all" else {args.types}
        client = FixtureClient(args.fixture) if args.fixture else HttpClient(
            args.interval, args.request_budget, args.max_file_mib * 1024 * 1024)
        report = {"tool_version": __version__, "started_at": now(),
                  "target": f"{kind}:{ident}" if len(targets) == 1 else "batch",
                  "mode": "synthetic_offline" if args.fixture else "anonymous_online_unvalidated",
                  "targets": [f"{k}:{i}" for k, i in targets],
                  "jobs": args.jobs, "limit": None if args.all else args.limit, "status": "running", "results": []}
        interrupted = False
        with Archive(args.out, client) as archive:
            try:
                if reporter:
                    reporter("discovering", {"targets": len(targets)})
                # Complete discovery before downloads, and deduplicate overlapping author/work URLs.
                refs = list(dict.fromkeys(ref for target_kind, target_id in targets
                    for ref in discover(client, target_kind, target_id, types)))
                # Fill the limit with unfinished works, so repeated bounded runs advance.
                complete = [ref for ref in refs if archive.is_complete(ref)]
                complete_set = set(complete)
                exclusions = set() if args.retry_unavailable else {
                    ref for ref in refs if archive.status(ref) in ("skipped", "unavailable")}
                remaining = [ref for ref in refs if ref not in complete_set and ref not in exclusions]
                # Newly discovered IDs advance even when old failures occupy the front of a list.
                remaining.sort(key=lambda ref: archive.status(ref) is not None)
                selected = remaining if args.all else remaining[:args.limit]
                report.update({"discovered": len(refs), "already_complete": len(complete),
                               "selected": len(selected), "deferred_by_limit": max(0, len(remaining) - len(selected)),
                               "previous_exclusions": len(exclusions),
                               "order": "unattempted first, then API series order or kind/ID descending"})
                if reporter:
                    reporter("selected", report)
                if args.command == "plan":
                    report["selected_targets"] = [f"{ref.kind}:{ref.id}" for ref in selected]
                    report["status"] = "planned"
                else:
                    for ref, status in download_many(archive, selected, args.jobs):
                        report["results"].append({"key": ref.key, "status": status})
                        if reporter:
                            reporter("progress", {"done": len(report["results"]), "total": len(selected),
                                                  "key": ref.key, "status": status})
                    counts = Counter(item["status"] for item in report["results"])
                    report["counts"] = dict(counts)
                    report["status"] = ("complete" if all(item["status"] in ("complete", "already_complete")
                        for item in report["results"]) and not report["deferred_by_limit"]
                        and not exclusions else "partial")
            except KeyboardInterrupt:
                report["status"] = "interrupted"
                report["reason"] = "Stopped by user; completed works remain saved"
                interrupted = True
            except RemoteStop as exc:
                report["status"], report["reason"] = "paused", str(exc)
            except ArchiveError as exc:
                report["status"], report["reason"] = "failed", str(exc)
            finally:
                report["counts"] = dict(Counter(item["status"] for item in report["results"]))
                report["unattempted_selected"] = max(0, report.get("selected", 0) - len(report["results"]))
                report["finished_at"] = now()
                report["requests"] = client.requests
                report["catalog_entries"] = len(archive.catalog())
                atomic_bytes(safe_path(archive.root, "last-run.json"), json_bytes(report))
                # Retain each run for audit, instead of overwriting the only record.
                name = report["started_at"].replace(":", "-")
                atomic_bytes(safe_path(archive.root, f"runs/{name}.json"), json_bytes(report))
        show(report)
        if interrupted:
            return 130
        return 0 if report["status"] in ("planned", "complete") else 1
    except (ArchiveError, ValueError, OSError) as exc:
        if reporter:
            reporter("error", str(exc))
        else:
            print(f"Error: {exc}", file=sys.stderr)
        return 2


if __name__ == "__main__":
    raise SystemExit(main())

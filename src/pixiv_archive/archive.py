"""Work-level archives with atomic writes, checksums and explicit partial results."""
from __future__ import annotations

import hashlib
import json
import os
import re
import sqlite3
import tempfile
import threading
import urllib.parse
import zipfile
from dataclasses import dataclass
from datetime import datetime, timezone
from pathlib import Path, PurePosixPath

from . import __version__
from .net import ArchiveError, RemoteStop, SchemaChanged, Unavailable
from .novel import extract_image_refs, plain_text, render_novel


def now() -> str:
    return datetime.now(timezone.utc).isoformat()


def require_id(value) -> str:
    value = str(value)
    if not re.fullmatch(r"[1-9][0-9]{0,19}", value):
        raise ArchiveError("An ID must be a positive decimal number of at most 20 digits")
    return value


@dataclass(frozen=True)
class WorkRef:
    kind: str
    id: str

    def __post_init__(self):
        if self.kind not in ("art", "novel"):
            raise ArchiveError("Unknown work kind")
        object.__setattr__(self, "id", require_id(self.id))

    @property
    def key(self) -> str:
        return f"{self.kind}_{self.id}"

    @property
    def url(self) -> str:
        return (f"https://www.pixiv.net/artworks/{self.id}" if self.kind == "art"
                else f"https://www.pixiv.net/novel/show.php?id={self.id}")


def parse_target(target: str) -> tuple[str, str]:
    match = re.fullmatch(r"(user|art|novel|series):([1-9][0-9]{0,19})", target)
    if match:
        return match[1], match[2]
    parsed = urllib.parse.urlsplit(target)
    if (parsed.scheme != "https" or parsed.hostname not in ("pixiv.net", "www.pixiv.net")
            or parsed.username or parsed.password or parsed.port not in (None, 443)):
        raise ArchiveError("Use user:ID, art:ID, novel:ID, series:ID or a canonical HTTPS pixiv URL")
    match = re.fullmatch(r"/(?:en/)?(users|artworks|novel/series)/([1-9][0-9]{0,19})/?", parsed.path)
    if match:
        return {"users": "user", "artworks": "art", "novel/series": "series"}[match[1]], match[2]
    if parsed.path in ("/novel/show.php", "/en/novel/show.php"):
        values = urllib.parse.parse_qs(parsed.query).get("id", [])
        if len(values) == 1:
            return "novel", require_id(values[0])
    raise ArchiveError("Unsupported pixiv target; use a work, author or novel-series URL")


def public_general(body: dict) -> bool:
    # Unknown age/access fields are not treated as permission.
    return (isinstance(body, dict) and body.get("xRestrict") in (0, "0")
            and body.get("restrict") in (None, 0, "0") and not body.get("isUnlisted", False))


def discover(client, kind: str, ident: str, types: set[str] | None = None) -> list[WorkRef]:
    ident = require_id(ident)
    types = types or {"art", "novel"}
    if kind in ("art", "novel"):
        return [WorkRef(kind, ident)] if kind in types else []
    if kind == "user":
        body = client.api(f"/ajax/user/{ident}/profile/all")
        if not isinstance(body, dict) or not all(key in body for key in ("illusts", "manga", "novels")):
            raise SchemaChanged("Author list schema changed; this is not an empty-author result")
        result = set()
        for category in ("illusts", "manga", "novels"):
            category_kind = "novel" if category == "novels" else "art"
            if category_kind not in types:
                continue
            group = body[category]
            # Some empty upstream ID maps are serialized as [] rather than {}.
            if group == []:
                continue
            if not isinstance(group, dict):
                raise SchemaChanged("Author ID map schema changed")
            result.update(WorkRef(category_kind, key) for key in group)
        return sorted(result, key=lambda ref: (ref.kind, -int(ref.id)))
    if kind == "series":
        if "novel" not in types:
            return []
        detail = client.api(f"/ajax/novel/series/{ident}")
        if not public_general(detail):
            raise Unavailable("Series is not confirmed public/general-audience")
        refs, seen, cursor = [], set(), 0
        for _ in range(100):
            page = client.api(f"/ajax/novel/series_content/{ident}", [
                ("limit", "30"), ("last_order", str(cursor)), ("order_by", "asc")])
            try:
                entries = page["page"]["seriesContents"]
            except (KeyError, TypeError) as exc:
                raise SchemaChanged("Novel series pagination schema changed") from exc
            if not isinstance(entries, list):
                raise SchemaChanged("Novel series entries are not a list")
            if not entries:
                return refs
            added = 0
            for item in entries:
                try:
                    ref = WorkRef("novel", item["id"])
                except (KeyError, TypeError) as exc:
                    raise SchemaChanged("Novel series entry has no ID") from exc
                if ref.id not in seen:
                    seen.add(ref.id)
                    refs.append(ref)
                    added += 1
            if not added:
                raise SchemaChanged("Novel series pagination made no progress")
            # Matches the observed upstream Web client; API pagination is not a public contract.
            cursor += len(entries)
            if len(entries) < 30:
                return refs
        raise RemoteStop("Series discovery exceeded its 100-page bound")
    raise ArchiveError("Unknown target kind")


def safe_path(root: Path, relative: str) -> Path:
    pure = PurePosixPath(relative)
    if (not relative or pure.is_absolute() or ".." in pure.parts or "\\" in relative
            or ":" in relative or any(ord(c) < 32 for c in relative)):
        raise ArchiveError("Unsafe archive path")
    path = root.joinpath(*pure.parts)
    for parent in (path, *path.parents):
        if parent == root:
            break
        if parent.is_symlink():
            raise ArchiveError("Symlinks are not accepted inside an archive")
    if not path.resolve().is_relative_to(root.resolve()):
        raise ArchiveError("Archive path escapes its root")
    return path


def atomic_bytes(path: Path, data: bytes):
    path.parent.mkdir(parents=True, exist_ok=True)
    descriptor, name = tempfile.mkstemp(prefix=".write-", dir=path.parent)
    try:
        with os.fdopen(descriptor, "wb") as output:
            output.write(data)
            output.flush()
            os.fsync(output.fileno())
        os.replace(name, path)
    finally:
        Path(name).unlink(missing_ok=True)


def json_bytes(data) -> bytes:
    return (json.dumps(data, ensure_ascii=False, indent=2) + "\n").encode("utf-8")


def fingerprint(path: Path) -> dict:
    digest = hashlib.sha256()
    length = 0
    with path.open("rb") as source:
        while chunk := source.read(128 * 1024):
            digest.update(chunk)
            length += len(chunk)
    return {"sha256": digest.hexdigest(), "bytes": length}


def extension(url: str) -> str:
    ext = Path(urllib.parse.urlsplit(url).path).suffix.lower()
    if ext not in (".jpg", ".jpeg", ".png", ".gif", ".webp", ".avif"):
        raise SchemaChanged("Unsupported image suffix; refusing to guess a media format")
    return ext


class Archive:
    def __init__(self, root: Path, client=None):
        root.mkdir(parents=True, exist_ok=True)
        self.root = root.resolve()
        self.client = client
        self._db_lock = threading.RLock()
        self.lock = safe_path(self.root, ".run.lock")
        try:
            fd = os.open(self.lock, os.O_CREAT | os.O_EXCL | os.O_WRONLY, 0o600)
        except FileExistsError as exc:
            raise ArchiveError("Archive is locked. Confirm no process is running before removing .run.lock") from exc
        try:
            with os.fdopen(fd, "w") as output:
                output.write(str(os.getpid()))
            self.db = sqlite3.connect(safe_path(self.root, "state.sqlite3"), check_same_thread=False)
            self.db.execute("""CREATE TABLE IF NOT EXISTS works (
                kind TEXT NOT NULL, id TEXT NOT NULL, status TEXT NOT NULL,
                updated_at TEXT NOT NULL, reason TEXT,
                PRIMARY KEY(kind,id))""")
            self.db.commit()
        except Exception:
            self.lock.unlink(missing_ok=True)
            raise

    def close(self):
        try:
            self.db.close()
        finally:
            self.lock.unlink(missing_ok=True)

    def __enter__(self):
        return self

    def __exit__(self, *args):
        self.close()

    def state(self, ref: WorkRef, status: str, reason: str | None = None):
        with self._db_lock:
            self.db.execute("INSERT INTO works VALUES (?,?,?,?,?) ON CONFLICT(kind,id) DO UPDATE SET "
                            "status=excluded.status,updated_at=excluded.updated_at,reason=excluded.reason",
                            (ref.kind, ref.id, status, now(), reason))
            self.db.commit()

    def _path(self, ref: WorkRef, relative: str) -> Path:
        return safe_path(self.root, f"works/{ref.key}/{relative}")

    def manifest(self, ref: WorkRef) -> dict | None:
        path = self._path(ref, "manifest.json")
        try:
            value = json.loads(path.read_text(encoding="utf-8"))
            return value if isinstance(value, dict) else None
        except (OSError, json.JSONDecodeError, UnicodeError):
            return None

    def check(self, ref: WorkRef) -> list[str]:
        manifest = self.manifest(ref)
        if not manifest or not isinstance(manifest.get("files"), list) or not manifest["files"]:
            return ["missing or invalid manifest"]
        problems = []
        for entry in manifest["files"]:
            try:
                path = self._path(ref, entry["path"])
                expected = {"sha256": entry["sha256"], "bytes": entry["bytes"]}
                if fingerprint(path) != expected:
                    problems.append(f"changed: {entry['path']}")
            except (OSError, ArchiveError, KeyError, TypeError):
                problems.append("missing or invalid manifest file entry")
        return problems

    def is_complete(self, ref: WorkRef) -> bool:
        with self._db_lock:
            row = self.db.execute("SELECT status FROM works WHERE kind=? AND id=?", (ref.kind, ref.id)).fetchone()
        manifest = self.manifest(ref)
        return bool(row and row[0] == "complete" and manifest and
                    manifest.get("status") == "complete" and not self.check(ref))

    def status(self, ref: WorkRef) -> str | None:
        with self._db_lock:
            row = self.db.execute("SELECT status FROM works WHERE kind=? AND id=?", (ref.kind, ref.id)).fetchone()
        return row[0] if row else None

    def save(self, ref: WorkRef) -> str:
        if self.is_complete(ref):
            return "already_complete"
        self.state(ref, "in_progress")
        try:
            body = self.client.api(f"/ajax/{'illust' if ref.kind == 'art' else 'novel'}/{ref.id}")
            if not public_general(body):
                self.state(ref, "skipped", "Not confirmed public/general-audience")
                return "skipped"
            if str(body.get("id", "")) != ref.id:
                raise SchemaChanged("Returned work ID does not match the requested ID")
            status = self._save_body(ref, body)
            self.state(ref, status)
            return status
        except RemoteStop as exc:
            self.state(ref, "paused", str(exc))
            raise
        except Unavailable as exc:
            self.state(ref, "unavailable", str(exc))
            return "unavailable"
        except (ArchiveError, OSError, ValueError, TypeError, KeyError, AttributeError) as exc:
            # Do not persist remote bodies or arbitrary URLs in the job's error log.
            reason = str(exc) if isinstance(exc, ArchiveError) else f"Invalid data or filesystem error ({type(exc).__name__})"
            self.state(ref, "failed", reason)
            return "failed"

    def _save_body(self, ref: WorkRef, body: dict) -> str:
        files, omissions = [], []
        previous = self.manifest(ref) or {}
        old_entries = previous.get("files")
        old_files = {entry["path"]: entry for entry in old_entries
                     if isinstance(entry, dict) and isinstance(entry.get("path"), str)} if isinstance(old_entries, list) else {}

        def write(relative: str, data: bytes):
            path = self._path(ref, relative)
            atomic_bytes(path, data)
            files.append({"path": relative, **fingerprint(path)})

        def asset(url: str, relative: str, *, optional: bool = False) -> bool:
            path = self._path(ref, relative)
            old = old_files.get(relative)
            if old and old.get("source_url") == url and path.exists():
                fp = fingerprint(path)
                if all(old.get(key) == value for key, value in fp.items()):
                    files.append({"path": relative, **fp, "source_url": url})
                    return True
            try:
                result = self.client.download(url, path)
                files.append({"path": relative, **result, "source_url": url})
                return True
            except RemoteStop:
                raise
            except (Unavailable, ArchiveError) as exc:
                if not optional:
                    raise
                omissions.append({"resource": relative, "reason": str(exc)})
                return False

        art_type = body.get("illustType")
        if ref.kind == "art" and art_type not in (0, 1, 2):
            raise SchemaChanged("Unknown artwork type")
        content_type = "novel" if ref.kind == "novel" else {0: "illustration", 1: "manga", 2: "ugoira"}[art_type]
        metadata = {
            "schema_version": 1, "id": ref.id, "kind": content_type,
            "title": body.get("title", ""), "author_id": body.get("userId"),
            "author_name": body.get("userName", ""), "source_url": ref.url,
            "created_at": body.get("createDate"), "updated_at": body.get("uploadDate"),
            "tags": body.get("tags"), "series": body.get("seriesNavData"),
            "archived_at": now(), "source_visibility": "public_general",
            "rights": {"copyright_holder": "original author", "license": "unknown",
                       "ai_training_permission": "unknown", "redistribution_permission": "unknown"},
        }
        write("raw.json", json_bytes(body))
        write("metadata.json", json_bytes(metadata))
        if ref.kind == "art" and art_type in (0, 1):
            pages = self.client.api(f"/ajax/illust/{ref.id}/pages")
            if not isinstance(pages, list) or len(pages) != body.get("pageCount") or not pages:
                raise SchemaChanged("Image-page count does not match artwork metadata")
            write("pages.json", json_bytes(pages))
            for index, page in enumerate(pages):
                url = page["urls"]["original"]
                asset(url, f"images/{index:04d}" + extension(url))
        elif ref.kind == "art":
            motion = self.client.api(f"/ajax/illust/{ref.id}/ugoira_meta")
            frames = motion.get("frames") if isinstance(motion, dict) else None
            if not isinstance(frames, list) or not frames:
                raise SchemaChanged("Missing ugoira frame timing")
            names = []
            for frame in frames:
                if not isinstance(frame, dict):
                    raise SchemaChanged("Ugoira frame is not an object")
                name, delay = frame.get("file"), frame.get("delay")
                if (not isinstance(name, str) or PurePosixPath(name).name != name or "\\" in name
                        or not isinstance(delay, (int, float)) or not 0 < delay <= 3600000):
                    raise SchemaChanged("Invalid ugoira frame metadata")
                names.append(name)
            if len(set(names)) != len(names):
                raise SchemaChanged("Duplicate ugoira frame names")
            asset(motion["originalSrc"], "frames.zip")
            try:
                with zipfile.ZipFile(self._path(ref, "frames.zip")) as zf:
                    members = zf.infolist()
                    if (len(members) != len(names) or set(zf.namelist()) != set(names)
                            or any(item.flag_bits & 1 for item in members)):
                        raise SchemaChanged("Ugoira ZIP members do not match frame metadata")
            except zipfile.BadZipFile as exc:
                raise SchemaChanged("Invalid ugoira ZIP") from exc
            # No extraction or transcoding: preserve original bytes and exact timing.
            write("frames.json", json_bytes(motion))
        else:
            content = body.get("content")
            if not isinstance(content, str):
                raise SchemaChanged("Novel body is missing")
            if not content and body.get("textCount", 0):
                raise SchemaChanged("Novel body is empty despite a nonzero declared text count")
            write("source.txt", content.encode("utf-8"))
            write("plain.txt", plain_text(content).encode("utf-8"))
            image_map = {}
            cover = body.get("coverUrl")
            if cover:
                try:
                    asset(cover, "cover" + extension(cover), optional=True)
                except ArchiveError as exc:
                    if isinstance(exc, RemoteStop):
                        raise
                    omissions.append({"resource": "cover", "reason": str(exc)})
            embedded = body.get("textEmbeddedImages") or {}
            if not isinstance(embedded, dict):
                raise SchemaChanged("Uploaded novel image map changed")
            for key in extract_image_refs(content):
                tag, image_id = key.split(":", 1)
                if tag == "uploadedimage":
                    item = embedded.get(image_id)
                    if not isinstance(item, dict) or str(item.get("sl", "0")) in ("4", "6"):
                        omissions.append({"resource": key, "reason": "No eligible embedded-image metadata"})
                        continue
                    urls = item.get("urls")
                    if not isinstance(urls, dict):
                        raise SchemaChanged("Uploaded-image URL map changed")
                    url = urls.get("original")
                else:
                    # Only the novel's visible/approved insertion relation grants access here.
                    # Never resolve an unapproved reference via a standalone artwork request.
                    try:
                        response = self.client.api(f"/ajax/novel/{ref.id}/insert_illusts", [("id[]", image_id)])
                    except Unavailable:
                        omissions.append({"resource": key, "reason": "Insertion relation unavailable"})
                        continue
                    item = response.get(image_id) if isinstance(response, dict) else None
                    illust = item.get("illust") if isinstance(item, dict) else None
                    if not item or item.get("visible") is not True or not public_general(illust):
                        omissions.append({"resource": key, "reason": "No visible/general-audience insertion relation"})
                        continue
                    urls = illust.get("images")
                    if not isinstance(urls, dict):
                        raise SchemaChanged("Inserted-image URL map changed")
                    url = urls.get("original")
                if not isinstance(url, str):
                    omissions.append({"resource": key, "reason": "No original image URL"})
                    continue
                relative = f"images/{tag}_{image_id}" + extension(url)
                if asset(url, relative, optional=True):
                    image_map[key] = relative
            write("index.html", render_novel(content, title=str(metadata["title"]),
                  author=str(metadata["author_name"]), image_map=image_map).encode("utf-8"))
        status = "partial" if omissions else "complete"
        manifest = {"schema_version": 1, "tool_version": __version__, "key": ref.key,
                    "source_url": ref.url, "archived_at": now(), "status": status,
                    "completeness_scope": "supported work payload; excludes comments, likes and account data",
                    "files": files, "omissions": omissions}
        atomic_bytes(self._path(ref, "manifest.json"), json_bytes(manifest))
        if self.check(ref):
            raise ArchiveError("Post-write checksum verification failed")
        return status

    def catalog(self):
        rows = self.db.execute("SELECT kind,id,status,updated_at,reason FROM works ORDER BY kind,id").fetchall()
        data = []
        for kind, ident, status, updated, reason in rows:
            ref = WorkRef(kind, ident)
            row = {"key": ref.key, "source_url": ref.url, "status": status,
                   "state_updated_at": updated, "reason": reason}
            if status in ("complete", "partial"):
                try:
                    row["metadata"] = json.loads(self._path(ref, "metadata.json").read_text(encoding="utf-8"))
                except (OSError, json.JSONDecodeError):
                    row["metadata_error"] = True
            data.append(row)
        atomic_bytes(safe_path(self.root, "catalog.jsonl"),
                     "".join(json.dumps(row, ensure_ascii=False) + "\n" for row in data).encode("utf-8"))
        return data

    def verify_all(self) -> dict:
        result = {"checked": 0, "valid": 0, "problems": []}
        for kind, ident, status in self.db.execute("SELECT kind,id,status FROM works"):
            if status not in ("complete", "partial"):
                continue
            ref = WorkRef(kind, ident)
            result["checked"] += 1
            errors = self.check(ref)
            if errors:
                result["problems"].append({"key": ref.key, "errors": errors})
            else:
                result["valid"] += 1
        return result

    def pack(self, destination: Path):
        if destination.is_symlink():
            raise ArchiveError("ZIP destination must not be a symlink")
        destination = destination.resolve()
        if destination.is_relative_to(self.root):
            raise ArchiveError("Place exported ZIP outside the archive directory")
        checked = self.verify_all()
        if checked["problems"]:
            raise ArchiveError("Archive integrity check failed; export cancelled")
        rows = self.db.execute("SELECT kind,id FROM works WHERE status='complete' ORDER BY kind,id").fetchall()
        if not rows:
            raise ArchiveError("There are no complete works to export")
        destination.parent.mkdir(parents=True, exist_ok=True)
        fd, temporary_name = tempfile.mkstemp(prefix=".export-", suffix=".part", dir=destination.parent)
        os.close(fd)
        temporary = Path(temporary_name)
        included = []
        try:
            with zipfile.ZipFile(temporary, "w", compression=zipfile.ZIP_DEFLATED, allowZip64=True) as out:
                for kind, ident in rows:
                    ref = WorkRef(kind, ident)
                    manifest = self.manifest(ref)
                    for entry in manifest["files"]:
                        name = f"works/{ref.key}/{entry['path']}"
                        out.write(safe_path(self.root, name), name)
                    out.write(self._path(ref, "manifest.json"), f"works/{ref.key}/manifest.json")
                    included.append(ref.key)
                out.writestr("export.json", json_bytes({"tool_version": __version__, "created_at": now(),
                             "included_complete_works": included,
                             "excluded": "partial, paused, failed, unavailable and skipped works"}))
            os.replace(temporary, destination)
        finally:
            temporary.unlink(missing_ok=True)
        return {"zip": str(destination), "complete_works": len(rows)}

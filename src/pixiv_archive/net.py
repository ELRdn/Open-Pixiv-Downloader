"""Anonymous, bounded HTTP transport. No login, challenge solving or proxy rotation."""
from __future__ import annotations

import base64
import hashlib
import http.client
import json
import math
import os
import tempfile
import time
import urllib.error
import urllib.parse
import urllib.request
from pathlib import Path

from . import __version__


class ArchiveError(Exception):
    """An expected, user-readable archive failure."""


class RemoteStop(ArchiveError):
    """Stop the entire job; retrying other works could increase remote load."""


class Unavailable(ArchiveError):
    """One work or resource cannot be read."""


class SchemaChanged(ArchiveError):
    """Upstream contract is missing or differs from the observed schema."""


def validate_url(url: str, *, asset: bool = False) -> str:
    try:
        parsed = urllib.parse.urlsplit(url)
        host = (parsed.hostname or "").lower()
        port = parsed.port
    except ValueError as exc:
        raise ArchiveError("Invalid remote URL") from exc
    allowed = (host == "pximg.net" or host.endswith(".pximg.net")) if asset else host == "www.pixiv.net"
    if (parsed.scheme != "https" or not allowed or port not in (None, 443)
            or parsed.username is not None or parsed.password is not None
            or any(ord(c) < 32 for c in url) or "\\" in url):
        raise ArchiveError("Remote URL is outside the allowed HTTPS origin")
    return url


def api_url(path: str, query: list[tuple[str, str]] | None = None) -> str:
    if not path.startswith("/ajax/") or "?" in path or "#" in path or ".." in path:
        raise ArchiveError("Invalid API path")
    return validate_url("https://www.pixiv.net" + path + (
        "?" + urllib.parse.urlencode(query) if query else ""))


class _NoRedirect(urllib.request.HTTPRedirectHandler):
    def redirect_request(self, req, fp, code, msg, headers, newurl):
        # Fail closed rather than silently moving a request to a login/challenge origin.
        raise RemoteStop("HTTP redirect received; inspect the site in your browser before resuming")


class HttpClient:
    def __init__(self, interval: float = 2.0, request_budget: int = 500,
                 max_file_bytes: int = 256 * 1024 * 1024):
        if not math.isfinite(interval) or interval < 2.0 or request_budget < 1 or max_file_bytes < 1:
            raise ValueError("Use interval >= 2 seconds and positive budgets")
        self.interval = interval
        self.request_budget = request_budget
        self.max_file_bytes = max_file_bytes
        self.requests = 0
        self._last_start = 0.0
        self._opener = urllib.request.build_opener(_NoRedirect())

    def _open(self, url: str, asset: bool = False):
        validate_url(url, asset=asset)
        for attempt in range(3):
            if self.requests >= self.request_budget:
                raise RemoteStop("Request budget reached; completed works remain saved")
            time.sleep(max(0.0, self.interval - (time.monotonic() - self._last_start)))
            self._last_start = time.monotonic()
            self.requests += 1
            request = urllib.request.Request(url, headers={
                "User-Agent": f"PixivArchiveLab/{__version__} (local archival research)",
                "Accept": "*/*" if asset else "application/json",
                "Accept-Encoding": "identity",
                "Referer": "https://www.pixiv.net/",
            })
            try:
                return self._opener.open(request, timeout=30)
            except urllib.error.HTTPError as exc:
                code = exc.code
                retry_after = exc.headers.get("Retry-After", "unspecified")
                exc.close()
                if code in (401, 403, 429):
                    # Store only a bounded header, never a remote HTML body or credentials.
                    safe_retry = "".join(c for c in retry_after if c.isprintable())[:80]
                    raise RemoteStop(f"HTTP {code}; stopped. Retry-After: {safe_retry}") from exc
                if code in (404, 410):
                    raise Unavailable(f"HTTP {code}: resource unavailable") from exc
                if code in (502, 503, 504) and attempt < 2:
                    # A large Retry-After is respected by stopping, not shortening it.
                    try:
                        delay = max(float(retry_after), 2.0 ** (attempt + 1))
                    except ValueError:
                        if retry_after != "unspecified":
                            raise RemoteStop("Server requested a dated Retry-After; resume later") from exc
                        delay = 2.0 ** (attempt + 1)
                    if not math.isfinite(delay):
                        raise RemoteStop("Invalid Retry-After; stopped") from exc
                    if delay > 30:
                        raise RemoteStop("Server requested a long Retry-After; resume later") from exc
                    time.sleep(delay)
                    continue
                raise ArchiveError(f"HTTP {code}: request failed") from exc
            except (urllib.error.URLError, TimeoutError, OSError) as exc:
                if attempt == 2:
                    raise ArchiveError("Network request failed after bounded retries") from exc
                time.sleep(2.0 ** (attempt + 1))
        raise ArchiveError("Request failed")

    def api(self, path: str, query: list[tuple[str, str]] | None = None):
        try:
            with self._open(api_url(path, query)) as response:
                data = response.read(16 * 1024 * 1024 + 1)
        except (http.client.HTTPException, urllib.error.URLError, OSError) as exc:
            raise ArchiveError("API transfer interrupted") from exc
        if len(data) > 16 * 1024 * 1024:
            raise SchemaChanged("API response exceeds the 16 MiB limit")
        try:
            payload = json.loads(data)
        except (UnicodeError, json.JSONDecodeError) as exc:
            raise RemoteStop("Expected JSON; received an HTML challenge or incompatible response") from exc
        if not isinstance(payload, dict) or "error" not in payload or "body" not in payload:
            raise SchemaChanged("API envelope changed")
        if payload["error"] is not False:
            raise Unavailable("API reports an unavailable or inaccessible resource")
        return payload["body"]

    def download(self, url: str, path: Path) -> dict:
        validate_url(url, asset=True)
        digest = hashlib.sha256()
        total = 0
        path.parent.mkdir(parents=True, exist_ok=True)
        fd, name = tempfile.mkstemp(prefix=".media-", suffix=".part", dir=path.parent)
        temp = Path(name)
        try:
            with os.fdopen(fd, "wb") as output, self._open(url, asset=True) as response:
                content_type = response.headers.get("Content-Type", "").lower()
                if "text/html" in content_type or "application/json" in content_type:
                    raise RemoteStop("Expected media; received a challenge or error document")
                expected = response.headers.get("Content-Length")
                if expected is not None:
                    try:
                        expected = int(expected)
                    except ValueError as exc:
                        raise SchemaChanged("Invalid Content-Length") from exc
                    if expected < 1 or expected > self.max_file_bytes:
                        raise ArchiveError("Resource size exceeds the configured limit or is empty")
                while chunk := response.read(128 * 1024):
                    if total == 0 and chunk.lstrip().lower().startswith((b"<!doctype html", b"<html")):
                        raise RemoteStop("Expected media; received HTML")
                    total += len(chunk)
                    if total > self.max_file_bytes:
                        raise ArchiveError("Resource exceeds the configured size limit")
                    digest.update(chunk)
                    output.write(chunk)
                if total == 0 or (expected is not None and total != expected):
                    raise ArchiveError("Incomplete media transfer")
                output.flush()
                os.fsync(output.fileno())
            os.replace(temp, path)
            return {"sha256": digest.hexdigest(), "bytes": total}
        except (OSError, urllib.error.URLError, http.client.HTTPException) as exc:
            raise ArchiveError("Media transfer interrupted; partial file was discarded") from exc
        finally:
            temp.unlink(missing_ok=True)


class FixtureClient:
    """Explicit offline transport. Its responses contain synthetic works only."""
    def __init__(self, fixture: Path):
        self.data = json.loads(fixture.read_text(encoding="utf-8"))
        self.requests = 0

    def api(self, path: str, query: list[tuple[str, str]] | None = None):
        key = api_url(path, query).removeprefix("https://www.pixiv.net")
        self.requests += 1
        if key not in self.data["responses"]:
            raise Unavailable(f"Synthetic fixture has no response for {path}")
        # Match HttpClient's unwrapped body contract; no external request is ever made.
        return self.data["responses"][key]

    def download(self, url: str, path: Path) -> dict:
        validate_url(url, asset=True)
        self.requests += 1
        if url not in self.data["assets"]:
            raise Unavailable("Synthetic fixture has no asset")
        data = base64.b64decode(self.data["assets"][url], validate=True)
        path.parent.mkdir(parents=True, exist_ok=True)
        fd, name = tempfile.mkstemp(prefix=".media-", suffix=".part", dir=path.parent)
        temp = Path(name)
        try:
            with os.fdopen(fd, "wb") as output:
                output.write(data)
            os.replace(temp, path)
        finally:
            temp.unlink(missing_ok=True)
        return {"sha256": hashlib.sha256(data).hexdigest(), "bytes": len(data)}

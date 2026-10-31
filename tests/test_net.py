import http.client
import io
import json
import math
import tempfile
import unittest
import urllib.error
from pathlib import Path
from unittest.mock import Mock, patch

from pixiv_archive.net import ArchiveError, HttpClient, RemoteStop, SchemaChanged, Unavailable, validate_url


class Response(io.BytesIO):
    def __init__(self, content=b"", **headers):
        super().__init__(content)
        self.headers = headers


class Interrupted(Response):
    def read(self, *args):
        raise http.client.IncompleteRead(b"fragment", 12)


class NetTests(unittest.TestCase):
    def client(self, responses):
        client = HttpClient()
        client._opener = Mock()
        client._opener.open.side_effect = responses
        return client

    def test_origins_are_bounded(self):
        for url in ("http://www.pixiv.net/ajax/novel/1", "https://www.pixiv.net.evil.test/a",
                    "https://www.pixiv.net@evil.test/a", "https://www.pixiv.net:444/a",
                    "https://127.0.0.1/a", "https://www.pixiv.net\\@evil.test/a"):
            with self.assertRaises(ArchiveError):
                validate_url(url)
        self.assertEqual(validate_url("https://i.pximg.net/test.png", asset=True), "https://i.pximg.net/test.png")
        with self.assertRaises(ArchiveError):
            validate_url("https://i.pximg.net.evil.test/x.png", asset=True)

    def test_interval_must_be_finite_and_slow_enough(self):
        for interval in (float("nan"), float("inf"), -1, 0.0, 1.99):
            with self.assertRaises(ValueError):
                HttpClient(interval=interval)

    @patch("pixiv_archive.net.time.sleep")
    def test_restriction_codes_stop_without_retry(self, sleep):
        for code in (401, 403, 429):
            error = urllib.error.HTTPError("https://www.pixiv.net/a", code, "restricted", {"Retry-After": "120"}, None)
            client = self.client([error])
            with self.assertRaises(RemoteStop):
                client.api("/ajax/novel/1")
            self.assertEqual(client.requests, 1)

    @patch("pixiv_archive.net.time.sleep")
    def test_404_is_unavailable(self, sleep):
        error = urllib.error.HTTPError("https://www.pixiv.net/a", 404, "missing", {}, None)
        with self.assertRaises(Unavailable):
            self.client([error]).api("/ajax/novel/1")

    @patch("pixiv_archive.net.time.sleep")
    def test_retry_is_bounded(self, sleep):
        errors = [urllib.error.HTTPError("https://www.pixiv.net/a", 503, "busy", {}, None) for _ in range(3)]
        client = self.client(errors)
        with self.assertRaises(ArchiveError):
            client.api("/ajax/novel/1")
        self.assertEqual(client.requests, 3)

    @patch("pixiv_archive.net.time.sleep")
    def test_invalid_retry_after_stops(self, sleep):
        for value in ("nan", "inf", "3600", "Wed, 01 Jan 2030 10:00:00 GMT"):
            error = urllib.error.HTTPError("https://www.pixiv.net/a", 503, "busy", {"Retry-After": value}, None)
            client = self.client([error])
            with self.assertRaises(RemoteStop):
                client.api("/ajax/novel/1")
            self.assertEqual(client.requests, 1)

    @patch("pixiv_archive.net.time.sleep")
    def test_budget_is_enforced_before_request(self, sleep):
        client = self.client([Response(b'{"error":false,"body":{}}')])
        client.request_budget = 1
        client.api("/ajax/novel/1")
        with self.assertRaises(RemoteStop):
            client.api("/ajax/novel/2")
        self.assertEqual(client.requests, 1)

    @patch("pixiv_archive.net.time.sleep")
    def test_non_json_and_wrong_schema_are_not_empty_success(self, sleep):
        with self.assertRaises(RemoteStop):
            self.client([Response(b"<html>challenge</html>")]).api("/ajax/novel/1")
        with self.assertRaises(SchemaChanged):
            self.client([Response(b"{}")]).api("/ajax/novel/1")

    @patch("pixiv_archive.net.time.sleep")
    def test_incomplete_read_is_normalized_and_temp_removed(self, sleep):
        with self.assertRaises(ArchiveError):
            self.client([Interrupted()]).api("/ajax/novel/1")
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "image.png"
            with self.assertRaises(ArchiveError):
                self.client([Interrupted()]).download("https://i.pximg.net/x.png", path)
            self.assertEqual(list(Path(directory).iterdir()), [])

    @patch("pixiv_archive.net.time.sleep")
    def test_predicted_part_symlink_does_not_overwrite_other_file(self, sleep):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            victim = root / "keep.txt"
            victim.write_bytes(b"KEEP")
            path = root / "image.png"
            (root / "image.png.part").symlink_to(victim)
            data = b"\x89PNG\r\n\x1a\nsynthetic"
            self.client([Response(data)]).download("https://i.pximg.net/x.png", path)
            self.assertEqual(victim.read_bytes(), b"KEEP")
            self.assertEqual(path.read_bytes(), data)

    @patch("pixiv_archive.net.time.sleep")
    def test_bad_length_does_not_replace_existing_file(self, sleep):
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "image.png"
            path.write_bytes(b"old")
            response = Response(b"short", **{"Content-Length": "100"})
            with self.assertRaises(ArchiveError):
                self.client([response]).download("https://i.pximg.net/x.png", path)
            self.assertEqual(path.read_bytes(), b"old")


if __name__ == "__main__":
    unittest.main()

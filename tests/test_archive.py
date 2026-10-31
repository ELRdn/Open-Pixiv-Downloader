"""Integration tests against synthetic, offline responses and media only."""

import base64
from contextlib import redirect_stdout
import hashlib
from io import StringIO
import json
from pathlib import Path
import tempfile
import unittest
from unittest.mock import patch
from urllib.parse import urlencode
import zipfile

from pixiv_archive import cli
from pixiv_archive.archive import Archive, WorkRef, discover
from pixiv_archive.net import FixtureClient, RemoteStop, SchemaChanged


FIXTURE = Path(__file__).resolve().parents[1] / "examples" / "synthetic-fixture.json"


class RecordingClient(FixtureClient):
    def __init__(self):
        super().__init__(FIXTURE)
        self.api_calls = []
        self.download_calls = []
        self.stop_on = None

    def api(self, path, query=None):
        self.api_calls.append(path + ("?" + urlencode(query) if query else ""))
        if path == self.stop_on:
            self.requests += 1
            raise RemoteStop("HTTP 429; stopped. Retry-After: 120")
        return super().api(path, query)

    def download(self, url, path):
        self.download_calls.append(url)
        return super().download(url, path)


class ArchiveIntegrationTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.base = Path(self.temp.name)
        self.out = self.base / "archive"
        self.client = RecordingClient()

    def test_wrong_work_identity_and_null_author_map_fail_closed(self):
        self.client.data["responses"]["/ajax/user/9001/profile/all"]["novels"] = None
        with self.assertRaises(SchemaChanged):
            discover(self.client, "user", "9001")
        self.client.data["responses"]["/ajax/illust/1001"]["id"] = "9999"
        with Archive(self.out, self.client) as archive:
            ref = WorkRef("art", "1001")
            self.assertEqual(archive.save(ref), "failed")
            self.assertEqual(self.client.download_calls, [])
            self.assertFalse(archive.is_complete(ref))

    def test_four_formats_series_raw_text_and_independent_checksums(self):
        refs = discover(self.client, "user", "9001")
        self.assertEqual(set(refs), {
            WorkRef("art", "1001"), WorkRef("art", "1002"),
            WorkRef("art", "1003"), WorkRef("novel", "1004"),
        })
        self.assertEqual(discover(self.client, "series", "8001"), [WorkRef("novel", "1004")])
        with Archive(self.out, self.client) as archive:
            for ref in refs:
                self.assertEqual(archive.save(ref), "complete")
                manifest = archive.manifest(ref)
                for entry in manifest["files"]:
                    data = (self.out / "works" / ref.key / entry["path"]).read_bytes()
                    self.assertEqual(len(data), entry["bytes"])
                    self.assertEqual(hashlib.sha256(data).hexdigest(), entry["sha256"])
            self.assertEqual(archive.verify_all(), {"checked": 4, "valid": 4, "problems": []})
            self.assertEqual(len(list((self.out / "works/art_1002/images").glob("*.png"))), 2)
            with zipfile.ZipFile(self.out / "works/art_1003/frames.zip") as frames:
                self.assertEqual(frames.namelist(), ["000000.png", "000001.png"])
                self.assertIsNone(frames.testzip())
            motion = json.loads((self.out / "works/art_1003/frames.json").read_text())
            self.assertEqual([frame["delay"] for frame in motion["frames"]], [80, 125])
            novel_dir = self.out / "works/novel_1004"
            source = self.client.data["responses"]["/ajax/novel/1004"]["content"]
            self.assertEqual((novel_dir / "source.txt").read_bytes(), source.encode("utf-8"))
            html = (novel_dir / "index.html").read_text(encoding="utf-8")
            self.assertIn("<ruby>出発", html)
            self.assertIn('src="images/uploadedimage_7001.png"', html)
            self.assertIn('src="images/pixivimage_1002-2.png"', html)
            self.assertNotIn("<script>", html)

    def test_resume_uses_no_requests_and_repairs_only_corrupted_media(self):
        ref = WorkRef("art", "1002")
        with Archive(self.out, self.client) as archive:
            self.assertEqual(archive.save(ref), "complete")
            requests = self.client.requests
            self.client.download_calls.clear()
            self.client.api_calls.clear()
            self.assertEqual(archive.save(ref), "already_complete")
            self.assertEqual(self.client.requests, requests)
            self.assertEqual(self.client.api_calls, [])
            self.assertEqual(self.client.download_calls, [])
            broken = self.out / "works/art_1002/images/0001.png"
            broken.write_bytes(b"interrupted or corrupted file")
            self.assertTrue(archive.check(ref))
            self.assertEqual(archive.save(ref), "complete")
            expected_url = self.client.data["responses"]["/ajax/illust/1002/pages"][1]["urls"]["original"]
            self.assertEqual(self.client.download_calls, [expected_url])
            self.assertEqual(broken.read_bytes(), base64.b64decode(self.client.data["assets"][expected_url]))
            self.assertEqual(archive.check(ref), [])

    def test_pack_excludes_partial_works_and_untracked_part_files(self):
        self.client.data["responses"]["/ajax/novel/1004"]["textEmbeddedImages"] = {}
        with Archive(self.out, self.client) as archive:
            self.assertEqual(archive.save(WorkRef("art", "1001")), "complete")
            self.assertEqual(archive.save(WorkRef("novel", "1004")), "partial")
            (self.out / "works/art_1001/images/incomplete.png.part").write_bytes(b"unfinished")
            (self.out / "works/art_1001/untracked.txt").write_text("not in manifest")
            destination = self.base / "export.zip"
            self.assertEqual(archive.pack(destination)["complete_works"], 1)
        with zipfile.ZipFile(destination) as packed:
            names = packed.namelist()
            self.assertIn("works/art_1001/manifest.json", names)
            self.assertFalse(any("novel_1004" in name or name.endswith(".part") or "untracked" in name for name in names))
            self.assertFalse(any(name.endswith("state.sqlite3") for name in names))
            exported = json.loads(packed.read("export.json"))
            self.assertEqual(exported["included_complete_works"], ["art_1001"])

    def test_http_429_stops_entire_cli_job_and_records_paused_state(self):
        self.client.stop_on = "/ajax/illust/1003"  # First work in author discovery order.
        stdout = StringIO()
        with patch.object(cli, "FixtureClient", return_value=self.client), redirect_stdout(stdout):
            result = cli.main(["fetch", "user:9001", "--fixture", str(FIXTURE), "--out", str(self.out)])
        self.assertEqual(result, 1)
        report = json.loads(stdout.getvalue())
        self.assertEqual(report["status"], "paused")
        self.assertIn("429", report["reason"])
        self.assertEqual(self.client.api_calls, ["/ajax/user/9001/profile/all", "/ajax/illust/1003"])
        self.assertEqual(self.client.download_calls, [])
        self.assertFalse((self.out / ".run.lock").exists())
        self.assertEqual(json.loads((self.out / "last-run.json").read_text())["status"], "paused")
        with Archive(self.out) as archive:
            self.assertEqual([(row["key"], row["status"]) for row in archive.catalog()], [("art_1003", "paused")])

    def test_unapproved_insertion_is_not_resolved_via_artwork_endpoint(self):
        key = "/ajax/novel/1004/insert_illusts?" + urlencode([("id[]", "1002-2")])
        self.client.data["responses"][key]["1002-2"]["visible"] = False
        with Archive(self.out, self.client) as archive:
            ref = WorkRef("novel", "1004")
            self.assertEqual(archive.save(ref), "partial")
            manifest = archive.manifest(ref)
            self.assertTrue(any(item["resource"] == "pixivimage:1002-2" for item in manifest["omissions"]))
        self.assertFalse(any(path.startswith("/ajax/illust/") for path in self.client.api_calls))
        denied_url = self.client.data["responses"][key]["1002-2"]["illust"]["images"]["original"]
        self.assertNotIn(denied_url, self.client.download_calls)
        html = (self.out / "works/novel_1004/index.html").read_text(encoding="utf-8")
        self.assertIn('href="https://www.pixiv.net/artworks/1002"', html)
        self.assertNotIn('src="images/pixivimage_1002-2.png"', html)

    def test_missing_schema_fields_are_not_empty_or_complete_success(self):
        del self.client.data["responses"]["/ajax/user/9001/profile/all"]["novels"]
        with self.assertRaises(SchemaChanged):
            discover(self.client, "user", "9001")
        for index, (ref, path, missing) in enumerate([
            (WorkRef("novel", "1004"), "/ajax/novel/1004", "content"),
            (WorkRef("art", "1001"), "/ajax/illust/1001", "pageCount"),
        ]):
            with self.subTest(missing=missing):
                client = RecordingClient()
                del client.data["responses"][path][missing]
                with Archive(self.base / str(index), client) as archive:
                    self.assertEqual(archive.save(ref), "failed")
                    self.assertFalse(archive.is_complete(ref))
                    self.assertIsNone(archive.manifest(ref))
                    self.assertEqual(archive.catalog()[0]["status"], "failed")

    def test_series_duplicate_page_stops_when_pagination_makes_no_progress(self):
        entries = [{"id": str(3000 + index), "order": index + 1} for index in range(30)]
        for cursor in (0, 30):
            key = "/ajax/novel/series_content/8001?" + urlencode([
                ("limit", "30"), ("last_order", str(cursor)), ("order_by", "asc"),
            ])
            self.client.data["responses"][key] = {"page": {"seriesContents": entries}}
        with self.assertRaisesRegex(SchemaChanged, "no progress"):
            discover(self.client, "series", "8001")
        calls = [path for path in self.client.api_calls if path.startswith("/ajax/novel/series_content/")]
        self.assertEqual(len(calls), 2)


if __name__ == "__main__":
    unittest.main()

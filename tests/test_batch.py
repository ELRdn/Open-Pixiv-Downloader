import io
import json
import tempfile
import threading
import unittest
import urllib.error
from concurrent.futures import ThreadPoolExecutor
from contextlib import redirect_stdout
from pathlib import Path
from unittest.mock import Mock, patch

from pixiv_archive import cli
from pixiv_archive.archive import Archive, WorkRef
from pixiv_archive.jobs import download_many
from pixiv_archive.net import FixtureClient, HttpClient, RemoteStop


FIXTURE = Path(__file__).resolve().parents[1] / 'examples/synthetic-fixture.json'


class BatchTests(unittest.TestCase):
    def run_cli(self, args):
        stdout = io.StringIO()
        with redirect_stdout(stdout):
            code = cli.main(args)
        return code, json.loads(stdout.getvalue())

    def test_file_and_author_overlap_deduplicate_and_resume(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            listing = root / 'urls.txt'
            listing.write_text('\ufeff# selected targets\nhttps://www.pixiv.net/users/9001\n'
                               'https://www.pixiv.net/artworks/1002#1\nnovel:1004\n', encoding='utf-8')
            args = ['fetch', 'art:1001', '--input', str(listing), '--fixture', str(FIXTURE),
                    '--out', str(root / 'out'), '--all', '--jobs', '2']
            code, report = self.run_cli(args)
            self.assertEqual(code, 0)
            self.assertEqual(report['discovered'], 4)
            self.assertEqual(report['counts'], {'complete': 4})
            self.assertEqual(len({r['key'] for r in report['results']}), 4)
            self.assertEqual(report['unattempted_selected'], 0)
            code, resumed = self.run_cli(args)
            self.assertEqual(code, 0)
            self.assertEqual(resumed['already_complete'], 4)
            self.assertEqual(resumed['requests'], 1)

    def test_limit_is_global_across_urls_and_all_removes_work_limit(self):
        with tempfile.TemporaryDirectory() as tmp:
            args = ['fetch', 'user:9001', 'art:1001', '--fixture', str(FIXTURE), '--out', tmp]
            code, report = self.run_cli(args + ['--limit', '1'])
            self.assertEqual(code, 1)
            self.assertEqual(report['selected'], 1)
            self.assertEqual(report['deferred_by_limit'], 3)
            code, report = self.run_cli(args + ['--all', '--jobs', '4'])
            self.assertEqual(code, 0)
            self.assertEqual(report['counts'], {'complete': 3})
            self.assertEqual(report['already_complete'], 1)

    def test_real_archive_workers_overlap_and_sqlite_stays_consistent(self):
        barrier = threading.Barrier(2)

        class Overlapping(FixtureClient):
            def download(self, url, destination):
                barrier.wait(timeout=5)
                return super().download(url, destination)

        with tempfile.TemporaryDirectory() as tmp:
            client = Overlapping(FIXTURE)
            # One image and one original ugoira ZIP each reach the barrier.
            refs = [WorkRef('art', '1001'), WorkRef('art', '1003')]
            with Archive(Path(tmp), client) as archive:
                results = list(download_many(archive, refs, 2))
                self.assertEqual({s for _, s in results}, {'complete'})
                self.assertEqual(archive.verify_all(), {'checked': 2, 'valid': 2, 'problems': []})

    def test_shared_http_rate_and_budget_with_parallel_callers(self):
        client = HttpClient(interval=2, request_budget=3)
        client._opener = Mock()
        client._opener.open.side_effect = [io.BytesIO() for _ in range(3)]
        clock = [0.0]
        starts = []

        def sleep(delay):
            clock[0] += delay

        def opened(*args, **kwargs):
            starts.append(clock[0])
            return io.BytesIO()

        client._opener.open.side_effect = opened
        with patch('pixiv_archive.net.time.monotonic', side_effect=lambda: clock[0]), \
             patch('pixiv_archive.net.time.sleep', side_effect=sleep):
            def request(_):
                try:
                    with client._open('https://i.pximg.net/test.png', asset=True):
                        return 'opened'
                except RemoteStop:
                    return 'stopped'
            with ThreadPoolExecutor(max_workers=4) as pool:
                outcomes = list(pool.map(request, range(6)))
        self.assertEqual(outcomes.count('opened'), 3)
        self.assertEqual(client.requests, 3)
        self.assertTrue(all(b - a >= 2 for a, b in zip(starts, starts[1:])))

    def test_http_denial_latches_across_parallel_callers(self):
        client = HttpClient()
        client._opener = Mock()
        client._opener.open.side_effect = urllib.error.HTTPError(
            'https://www.pixiv.net/a', 429, 'limited', {}, None)
        with patch('pixiv_archive.net.time.sleep'):
            def request(_):
                with self.assertRaises(RemoteStop):
                    client.api('/ajax/illust/1001')
            with ThreadPoolExecutor(max_workers=4) as pool:
                list(pool.map(request, range(8)))
        self.assertEqual(client._opener.open.call_count, 1)
        self.assertEqual(client.requests, 1)

    def test_scheduler_does_not_start_queued_works_after_stop(self):
        class Stopped:
            client = Mock()
            def __init__(self):
                self.calls = []
                self.barrier = threading.Barrier(2)
            def save(self, ref):
                self.calls.append(ref)
                self.barrier.wait(timeout=5)
                raise RemoteStop('429 synthetic stop')
        archive = Stopped()
        with self.assertRaises(RemoteStop):
            list(download_many(archive, list(range(10)), 2))
        self.assertEqual(set(archive.calls), {0, 1})
        archive.client.cancel.assert_called()

    def test_cancel_during_rate_wait_prevents_request(self):
        client = HttpClient()
        client._opener = Mock()
        with patch('pixiv_archive.net.time.sleep', side_effect=lambda delay: client.cancel()):
            with self.assertRaises(RemoteStop):
                client.api('/ajax/illust/1001')
        self.assertEqual(client.requests, 0)
        client._opener.open.assert_not_called()

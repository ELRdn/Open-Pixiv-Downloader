import io
import json
import os
import tempfile
import unittest
import zipfile
from contextlib import redirect_stdout, redirect_stderr
from pathlib import Path
from unittest.mock import Mock, patch

from pixiv_archive import cli, interactive, terminal


class InteractiveTests(unittest.TestCase):
    def session(self, answers, execute):
        output = io.StringIO()
        with patch('builtins.input', side_effect=answers), redirect_stdout(output):
            code = interactive.run(execute)
        return code, output.getvalue()

    def test_demo_verify_pack_workflow_uses_real_engine_and_reuses_folder(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            folder, package = root / '日本語 archive', root / 'export.zip'
            code, output = self.session([
                '/demo', str(folder), '2', '',
                '/verify', '', '', '/pack', '', str(package), '', '/quit',
            ], cli.main)
            self.assertEqual(code, 0)
            self.assertIn('Welcome to Open Pixiv Downloader', output)
            self.assertIn('今回保存 4作品', output)
            self.assertIn('4作品中 4作品', output)
            self.assertIn('ZIPを保存しました', output)
            self.assertNotIn('"tool_version":', output)
            with zipfile.ZipFile(package) as archive:
                self.assertIsNone(archive.testzip())
                self.assertEqual(len(json.loads(archive.read('export.json'))['included_complete_works']), 4)

    def test_direct_paste_validates_and_deduplicates_before_start(self):
        execute = Mock(return_value=0)
        code, output = self.session([
            'https://www.pixiv.net/artworks/1001',
            'https://evil.invalid/a', 'art:1001 https://www.pixiv.net/users/9001', '',
            '', '9', '4', '', '/quit',
        ], execute)
        self.assertEqual(code, 0)
        args = execute.call_args.args[0]
        self.assertEqual(args[:3], ['fetch', 'https://www.pixiv.net/artworks/1001', 'https://www.pixiv.net/users/9001'])
        self.assertIn('--online', args)
        self.assertIn('--all', args)
        self.assertEqual(args[args.index('--jobs') + 1], '4')
        self.assertIn('入力を確認してください', output)
        self.assertIn('1〜4の数字', output)
        execute.assert_called_once()

    def test_back_from_preview_makes_no_network_or_archive_call(self):
        execute = Mock()
        self.session(['/download', 'art:1001', '', '', '', 'b', '/quit'], execute)
        execute.assert_not_called()

    def test_invalid_menu_and_help_do_not_execute(self):
        execute = Mock()
        code, output = self.session(['not-a-command', '/help', '/quit'], execute)
        self.assertEqual(code, 0)
        self.assertIn('/download', output)
        execute.assert_not_called()

    def test_input_end_and_interrupt_exit_cleanly(self):
        execute = Mock()
        self.assertEqual(self.session([EOFError()], execute)[0], 0)
        self.assertEqual(self.session([KeyboardInterrupt()], execute)[0], 130)
        execute.assert_not_called()

    def test_no_args_terminal_launches_but_pipe_does_not_hang(self):
        with patch('sys.stdin.isatty', return_value=True), patch('builtins.input', return_value='/quit'), redirect_stdout(io.StringIO()):
            self.assertEqual(cli.main([]), 0)
        with patch('sys.stdin.isatty', return_value=False), patch('builtins.input') as prompt, redirect_stderr(io.StringIO()):
            self.assertEqual(cli.main([]), 2)
            prompt.assert_not_called()

    def test_narrow_terminal_and_no_color_have_bounded_plain_banner(self):
        output = io.StringIO()
        with patch('pixiv_archive.terminal.shutil.get_terminal_size', return_value=os.terminal_size((40, 24))), \
             patch.dict('os.environ', {'NO_COLOR': '1'}), redirect_stdout(output):
            terminal.welcome()
        text = output.getvalue()
        self.assertNotIn('\x1b', text)
        self.assertTrue(all(terminal.width(line) <= 40 for line in text.splitlines()))

    def test_legacy_script_still_returns_json(self):
        fixture = Path(__file__).resolve().parents[1] / 'examples/synthetic-fixture.json'
        with tempfile.TemporaryDirectory() as tmp, redirect_stdout(io.StringIO()) as output:
            self.assertEqual(cli.main(['plan', 'user:9001', '--fixture', str(fixture), '--out', tmp]), 0)
            self.assertEqual(json.loads(output.getvalue())['discovered'], 4)

    def test_online_default_does_not_reuse_demo_archive(self):
        execute = Mock(return_value=0)
        self.session(['/demo', '', '', '', 'art:1001', '', '', '', '', '/quit'], execute)
        first, second = [call.args[0] for call in execute.call_args_list]
        self.assertEqual(first[first.index('--out') + 1], 'demo-archive')
        self.assertEqual(second[second.index('--out') + 1], 'archive')

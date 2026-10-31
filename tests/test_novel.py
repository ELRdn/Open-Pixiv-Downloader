"""Synthetic fixtures only; no downloaded creative works or credentials."""

from html.parser import HTMLParser
import unittest

from pixiv_archive.novel import extract_image_refs, plain_text, render_novel


class _Tags(HTMLParser):
    def __init__(self):
        super().__init__()
        self.elements = []

    def handle_starttag(self, tag, attrs):
        self.elements.append((tag, dict(attrs)))


class NovelTests(unittest.TestCase):
    def parsed(self, output):
        parser = _Tags()
        parser.feed(output)
        return parser.elements

    def test_basic_markup_and_nested_ruby_heading(self):
        source = "[chapter:第一章 [[rb:海>うみ]]]\n[b:出発]と[i:帰還]\n[jump:2][newpage]二頁"
        output = render_novel(source, "試作", "合成作家")
        self.assertIn('<h2 id="chapter-1">第一章 <ruby>海', output)
        self.assertIn("<rt>うみ</rt>", output)
        self.assertIn("<strong>出発</strong>", output)
        self.assertIn("<em>帰還</em>", output)
        self.assertIn('href="#page-2"', output)
        self.assertIn('id="page-2"', output)
        self.assertIn("合成作家", output)

    def test_plain_text_is_derivative_and_keeps_unknown_and_images(self):
        source = "[chapter:章][[rb:漢字>かんじ]][b:太字][newpage][unknown:x][uploadedimage:12]"
        self.assertEqual(plain_text(source), "\n\n章\n\n漢字太字\n\n[unknown:x][uploadedimage:12]")

    def test_image_keys_order_and_deduplication(self):
        source = "[uploadedimage:12][pixivimage:34-2][uploadedimage:12][pixivimage:34]"
        self.assertEqual(extract_image_refs(source), ["uploadedimage:12", "pixivimage:34-2", "pixivimage:34"])

    def test_images_only_use_relative_raster_files(self):
        output = render_novel(
            "[uploadedimage:12][pixivimage:34-2][pixivimage:56]",
            image_map={"uploadedimage:12": "images/挿絵 12.jpg", "pixivimage:34-2": "images/34_p1.png"},
        )
        tags = self.parsed(output)
        images = [attributes for tag, attributes in tags if tag == "img"]
        self.assertEqual(len(images), 2)
        self.assertEqual(images[0]["src"], "images/%E6%8C%BF%E7%B5%B5%2012.jpg")
        self.assertIn('href="https://www.pixiv.net/artworks/56"', output)

    def test_untrusted_markup_is_escaped_everywhere(self):
        output = render_novel(
            '<script>alert(1)</script>\n[chapter:<img src=x onerror=x>]'
            '[[rb:<img src=x> > <script>x</script>]]',
            title='</title><script>alert(2)</script>',
            author='<iframe src="https://bad.example">',
        )
        tags = self.parsed(output)
        self.assertFalse(any(tag in {"script", "iframe", "img"} for tag, _ in tags))
        self.assertIn("&lt;script&gt;", output)
        self.assertFalse(any(any(key.startswith("on") for key in attrs) for _, attrs in tags))

    def test_link_schemes_and_attributes(self):
        for target in ["javascript:alert(1)", "data:text/html,hello", "file:///tmp/x", "//evil.example/x", "https:\\evil.example", "https://u:p@evil.example", "https://example.org:bad"]:
            with self.subTest(target=target):
                output = render_novel("[[jumpuri:参考>" + target + "]]")
                self.assertFalse(any(tag == "a" for tag, _ in self.parsed(output)))
        output = render_novel('[[jumpuri:引用>https://example.org/?x="y"&a=1]]')
        anchors = [a for tag, a in self.parsed(output) if tag == "a"]
        self.assertEqual(len(anchors), 1)
        self.assertEqual(anchors[0]["href"], 'https://example.org/?x="y"&a=1')
        self.assertEqual(set(anchors[0]), {"href", "rel"})

    def test_traversal_remote_and_active_image_paths_rejected(self):
        for path in ["../secret.jpg", "/tmp/x.jpg", "images/../../x.jpg", "images\\x.jpg", "https://x.example/a.jpg", "//x.example/a.jpg", "images/%2e%2e/x.jpg", "images/%252e%252e/x.jpg", "images/x.svg", "images/x.html", "images/x.jpg?bad=1", "images/x.jpg#fragment", "C:/x.jpg", "images//x.jpg", "images/./x.jpg", "images/x\x00.jpg"]:
            with self.subTest(path=path):
                output = render_novel("[uploadedimage:12]", image_map={"uploadedimage:12": path})
                self.assertFalse(any(tag == "img" for tag, _ in self.parsed(output)))

    def test_invalid_page_jump_preserved(self):
        output = render_novel("[jump:2][jump:0][jump:-1]")
        self.assertNotIn('href="#page-', output)
        for raw in ["[jump:2]", "[jump:0]", "[jump:-1]"]:
            self.assertIn(raw, output)

    def test_huge_page_number_does_not_require_integer_conversion(self):
        source = "[jump:" + "9" * 5000 + "]"
        self.assertIn(source, render_novel(source))

    def test_emphasis_marker_cannot_inject_css(self):
        output = render_novel('[[emphasismark:大事>﹅]][[emphasismark:記号>"]]')
        spans = [a for tag, a in self.parsed(output) if tag == "span"]
        self.assertEqual(len(spans), 2)
        self.assertIn("\\00fe45", spans[0]["style"])
        self.assertIn("\\000022", spans[1]["style"])
        self.assertEqual(set(spans[1]), {"style"})

    def test_unknown_unclosed_and_deep_markup_do_not_crash(self):
        output = render_novel("[mystery:raw][[rb:unfinished[" + "[b:" * 100 + "word" + "]" * 100)
        self.assertIn("[mystery:raw]", output)
        self.assertIn("word", output)
        self.assertLess(output.count("<strong>"), 20)
        self.assertIn("[b:", output)

    def test_unknown_balanced_markup_kept_literal(self):
        source = "[unknown:[b:literal]][[rb:no separator]]"
        self.assertIn(source, render_novel(source))
        self.assertEqual(plain_text(source), source)
        self.assertNotIn("<strong>", render_novel(source))

    def test_no_external_assets_or_scripts_in_document(self):
        output = render_novel("本文")
        tags = self.parsed(output)
        self.assertFalse(any(tag in ("script", "link", "iframe", "object") for tag, _ in tags))
        policies = [attrs["content"] for tag, attrs in tags if tag == "meta" and attrs.get("http-equiv") == "Content-Security-Policy"]
        self.assertEqual(len(policies), 1)
        self.assertIn("default-src 'none'", policies[0])

    def test_empty_document(self):
        output = render_novel("")
        self.assertIn("<title>無題</title>", output)
        self.assertIn('id="page-1"', output)
        self.assertEqual(plain_text(""), "")
        self.assertEqual(extract_image_refs(""), [])


if __name__ == "__main__":
    unittest.main()

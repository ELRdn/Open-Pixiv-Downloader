#!/usr/bin/env python3
"""Regenerate the deterministic, entirely synthetic offline demonstration.

No network requests, credentials, third-party works, or non-standard packages
are used. The i.pximg.net URLs are fixture lookup keys, not live sample assets.

Run: python3 examples/make_fixture.py
"""

from __future__ import annotations

import argparse
import base64
from io import BytesIO
import json
from pathlib import Path
import struct
import urllib.parse
import zipfile
import zlib


def png(color: tuple[int, int, int], width: int = 24, height: int = 16) -> bytes:
    """Make a tiny valid RGB PNG with a contrasting checkerboard pattern."""
    def chunk(kind: bytes, data: bytes) -> bytes:
        return struct.pack(">I", len(data)) + kind + data + struct.pack(">I", zlib.crc32(kind + data) & 0xFFFFFFFF)

    rows = []
    for y in range(height):
        row = bytearray([0])  # PNG scanline filter: None.
        for x in range(width):
            shaded = (x // 4 + y // 4) % 2
            row.extend(max(0, channel - 45 * shaded) for channel in color)
        rows.append(bytes(row))
    return (
        b"\x89PNG\r\n\x1a\n"
        + chunk(b"IHDR", struct.pack(">IIBBBBB", width, height, 8, 2, 0, 0, 0))
        + chunk(b"IDAT", zlib.compress(b"".join(rows), level=9))
        + chunk(b"IEND", b"")
    )


def frame_zip(frames: list[bytes]) -> bytes:
    buffer = BytesIO()
    with zipfile.ZipFile(buffer, "w") as archive:
        for index, frame in enumerate(frames):
            info = zipfile.ZipInfo(f"{index:06d}.png", date_time=(2000, 1, 1, 0, 0, 0))
            info.compress_type = zipfile.ZIP_DEFLATED
            info.external_attr = 0o100644 << 16
            archive.writestr(info, frame)
    return buffer.getvalue()


def query_key(path: str, pairs: list[tuple[str, str]]) -> str:
    return path + "?" + urllib.parse.urlencode(pairs)


def make_fixture() -> dict:
    base = "https://i.pximg.net/synthetic/pixiv-archive-lab/"
    urls = {
        "illust": base + "1001_p0.png",
        "manga_0": base + "1002_p0.png",
        "manga_1": base + "1002_p1.png",
        "ugoira": base + "1003_ugoira.zip",
        "cover": base + "1004_cover.png",
        "uploaded": base + "7001.png",
    }
    assets = {
        urls["illust"]: png((100, 160, 240)),
        urls["manga_0"]: png((200, 130, 160)),
        urls["manga_1"]: png((140, 200, 150)),
        urls["ugoira"]: frame_zip([png((220, 180, 80)), png((180, 120, 230))]),
        urls["cover"]: png((160, 160, 220)),
        urls["uploaded"]: png((120, 200, 210)),
    }
    common = {
        "userId": "9001",
        "userName": "合成サンプル作者",
        "xRestrict": 0,
        "restrict": 0,
        "isUnlisted": False,
        "createDate": "2026-01-01T00:00:00+09:00",
        "uploadDate": "2026-01-02T00:00:00+09:00",
        "tags": {"tags": [{"tag": "synthetic-fixture"}, {"tag": "架空の作品"}]},
    }
    source = (
        "[chapter:合成テスト・第一章 [[rb:出発>しゅっぱつ]]]\n"
        "これはオフライン検証のためだけに書いた、架空の短い物語です。\n"
        "[b:あおい]は机の上に[i:小さな地図]を置きました。\n"
        "[[emphasismark:必ず帰ろう>﹅]]と、二人で約束します。\n"
        "[uploadedimage:7001]\n"
        "[jump:2]\n"
        "[newpage][chapter:合成テスト・第二章]\n"
        "二ページ目には、承認済みという設定の合成漫画を挿入します。\n"
        "[pixivimage:1002-2]\n"
        "[[jumpuri:関連作品>https://www.pixiv.net/artworks/1002]]\n"
        "文字としての <script>alert('synthetic')</script> も安全に表示します。\n"
        "[unknown:この未対応記法も原文には残ります]\n"
    )
    responses = {
        "/ajax/user/9001/profile/all": {
            "illusts": {"1001": None, "1003": None},
            "manga": {"1002": None},
            "novels": {"1004": None},
        },
        "/ajax/illust/1001": {**common, "id": "1001", "title": "合成イラスト", "illustType": 0, "pageCount": 1},
        "/ajax/illust/1001/pages": [{"urls": {"original": urls["illust"]}, "width": 24, "height": 16}],
        "/ajax/illust/1002": {**common, "id": "1002", "title": "合成漫画・二ページ", "illustType": 1, "pageCount": 2},
        "/ajax/illust/1002/pages": [
            {"urls": {"original": urls["manga_0"]}, "width": 24, "height": 16},
            {"urls": {"original": urls["manga_1"]}, "width": 24, "height": 16},
        ],
        "/ajax/illust/1003": {**common, "id": "1003", "title": "合成うごイラ", "illustType": 2, "pageCount": 1},
        "/ajax/illust/1003/ugoira_meta": {
            "src": urls["ugoira"],
            "originalSrc": urls["ugoira"],
            "mime_type": "image/png",
            "frames": [{"file": "000000.png", "delay": 80}, {"file": "000001.png", "delay": 125}],
        },
        "/ajax/novel/1004": {
            **common, "id": "1004", "title": "合成小説・二人の地図", "content": source,
            "coverUrl": urls["cover"],
            "seriesNavData": {"seriesId": "8001", "title": "合成シリーズ", "order": 1, "prev": None, "next": None},
            "textEmbeddedImages": {"7001": {"id": "7001", "sl": 2, "urls": {"original": urls["uploaded"]}}},
        },
        query_key("/ajax/novel/1004/insert_illusts", [("id[]", "1002-2")]): {
            "1002-2": {"visible": True, "illust": {
                "id": "1002", "xRestrict": 0, "restrict": 0,
                "images": {"original": urls["manga_1"]},
            }},
        },
        "/ajax/novel/series/8001": {
            "id": "8001", "title": "合成シリーズ", "userId": "9001", "xRestrict": 0, "restrict": 0,
        },
        query_key("/ajax/novel/series_content/8001", [("limit", "30"), ("last_order", "0"), ("order_by", "asc")]): {
            "page": {"seriesContents": [{"id": "1004", "order": 1, "title": "合成小説・二人の地図"}]},
        },
    }
    return {
        "synthetic": True,
        "description": "Entirely generated offline fixture; no real creative works, credentials, or live media URLs.",
        "targets": ["user:9001", "art:1001", "art:1002", "art:1003", "novel:1004", "series:8001"],
        "responses": responses,
        "assets": {url: base64.b64encode(data).decode("ascii") for url, data in assets.items()},
    }


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--out", type=Path, default=Path(__file__).with_name("synthetic-fixture.json"))
    args = parser.parse_args()
    data = json.dumps(make_fixture(), ensure_ascii=False, indent=2) + "\n"
    args.out.parent.mkdir(parents=True, exist_ok=True)
    args.out.write_text(data, encoding="utf-8")
    print(f"Generated synthetic fixture: {args.out}")


if __name__ == "__main__":
    main()

"""Conservative, offline-safe rendering of documented pixiv novel markup.

This is an independent implementation of the public syntax, not a port of an
upstream parser. The original source text must also be retained by the caller.

Syntax reference: https://www.pixiv.help/hc/ja/articles/235584168
Reference checked: 2026-10-29. Supported additions include b, i, emphasismark.

``image_map`` keys are ``uploadedimage:123`` or ``pixivimage:456-2`` and values
are relative raster-image paths (for example ``images/123.jpg``). The renderer
never performs network requests. Unmapped artwork references become ordinary
pixiv links; unmapped uploaded-image references remain visible placeholders.
"""

from __future__ import annotations

from dataclasses import dataclass
from html import escape
from pathlib import PurePosixPath
import re
from typing import Mapping
from urllib.parse import quote, unquote, urlsplit

__all__ = ["render_novel", "plain_text", "extract_image_refs"]

_MAX_NESTING = 16
_INTEGER = re.compile(r"[1-9][0-9]*\Z")
_ARTWORK = re.compile(r"([1-9][0-9]*)(?:-([1-9][0-9]*))?\Z")
_SINGLE = ("chapter", "jump", "pixivimage", "uploadedimage", "b", "i")
_DOUBLE = ("rb", "jumpuri", "emphasismark")
_RASTER_SUFFIXES = frozenset({".jpg", ".jpeg", ".png", ".gif", ".webp", ".avif", ".bmp"})


@dataclass(frozen=True)
class _Token:
    kind: str
    raw: str
    value: str = ""
    extra: str = ""


def _tokens(text: str) -> list[_Token]:
    """Tokenize in linear time; balanced brackets allow ruby in chapter titles.

    Unknown or malformed tags are kept as literal text. Computing closing
    brackets once avoids quadratic rescanning of long, unclosed inputs.
    """
    if not isinstance(text, str):
        raise TypeError("novel text must be a string")
    closings: dict[int, int] = {}
    stack: list[int] = []
    for position, character in enumerate(text):
        if character == "[":
            stack.append(position)
        elif character == "]" and stack:
            closings[stack.pop()] = position

    result: list[_Token] = []
    pending: list[str] = []
    position = 0
    while position < len(text):
        token: _Token | None = None
        end = closings.get(position)
        if text.startswith("[newpage]", position):
            token = _Token("newpage", "[newpage]")
            end = position + len(token.raw) - 1
        elif text[position] == "[" and end is not None:
            raw = text[position : end + 1]
            for kind in _DOUBLE:
                prefix = "[[" + kind + ":"
                if raw.startswith(prefix) and raw.endswith("]]"):
                    body = raw[len(prefix) : -2]
                    if ">" in body:
                        value, extra = body.split(">", 1)
                        value, extra = value.strip(), extra.strip()
                        if value and extra:
                            token = _Token(kind, raw, value, extra)
                    break
            if token is None:
                for kind in _SINGLE:
                    prefix = "[" + kind + ":"
                    if raw.startswith(prefix):
                        value = raw[len(prefix) : -1].strip()
                        valid = True
                        if kind in ("jump", "uploadedimage"):
                            valid = bool(_INTEGER.fullmatch(value))
                        elif kind == "pixivimage":
                            valid = bool(_ARTWORK.fullmatch(value))
                        if valid:
                            token = _Token(kind, raw, value)
                        break
        if token is None:
            # Consume balanced unknown/malformed groups atomically. This both
            # retains their exact spelling and prevents quadratic rescanning of
            # deeply nested unknown brackets or treating their contents as tags.
            if text[position] == "[" and end is not None:
                pending.append(text[position : end + 1])
                position = end + 1
            else:
                pending.append(text[position])
                position += 1
            continue
        if pending:
            result.append(_Token("text", "".join(pending)))
            pending.clear()
        result.append(token)
        position = end + 1  # type: ignore[operator]
    if pending:
        result.append(_Token("text", "".join(pending)))
    return result


def _http_link(value: str) -> str | None:
    """Allow only explicit HTTP(S) links; links are never fetched automatically."""
    if not value or any(character.isspace() or ord(character) < 32 for character in value):
        return None
    if "\\" in value or "\x7f" in value:
        return None
    try:
        url = urlsplit(value)
        if url.scheme.lower() not in ("http", "https") or not url.hostname:
            return None
        if url.username is not None or url.password is not None:
            return None
        # Accessing port also validates malformed ports before producing HTML.
        _ = url.port
    except ValueError:
        return None
    return value


def _local_image(value: object) -> str | None:
    """Return a quoted relative raster path, rejecting traversal and URL input."""
    if not isinstance(value, str) or not value:
        return None
    candidate = value
    for _ in range(6):
        if (
            candidate.startswith("/")
            or any(c in candidate for c in ("\\", ":", "?", "#"))
            or any(ord(c) < 32 or ord(c) == 127 for c in candidate)
            or any(component in ("", ".", "..") for component in candidate.split("/"))
        ):
            return None
        decoded = unquote(candidate)
        if decoded == candidate:
            break
        candidate = decoded
    else:
        return None
    if PurePosixPath(candidate).suffix.lower() not in _RASTER_SUFFIXES:
        return None
    return quote(value, safe="/-._~")


def _inline_token(token: _Token, depth: int) -> str:
    if depth >= _MAX_NESTING:
        return escape(token.raw)
    if token.kind == "text":
        return escape(token.raw)
    if token.kind == "rb":
        return (
            "<ruby>" + _inline(token.value, depth + 1)
            + "<rp>（</rp><rt>" + escape(token.extra) + "</rt><rp>）</rp></ruby>"
        )
    if token.kind in ("b", "i"):
        tag = "strong" if token.kind == "b" else "em"
        return "<" + tag + ">" + _inline(token.value, depth + 1) + "</" + tag + ">"
    if token.kind == "jumpuri":
        url = _http_link(token.extra)
        if url is None:
            return '<span class="unresolved">' + escape(token.raw) + "</span>"
        # Labels remain plain text to prevent nested anchors or unexpected markup.
        return '<a href="' + escape(url, quote=True) + '" rel="noreferrer noopener">' + escape(token.value) + "</a>"
    if token.kind == "emphasismark":
        if len(token.extra) != 1 or ord(token.extra) < 32 or ord(token.extra) == 127:
            return escape(token.raw)
        # CSS receives only a generated hexadecimal codepoint, never source CSS.
        mark = "\\" + format(ord(token.extra), "06x")
        style = 'text-emphasis:"' + mark + '";-webkit-text-emphasis:"' + mark + '"'
        return '<span style="' + escape(style, quote=True) + '">' + _inline(token.value, depth + 1) + "</span>"
    return escape(token.raw)


def _inline(text: str, depth: int = 0) -> str:
    if depth >= _MAX_NESTING:
        return escape(text)
    return "".join(_inline_token(token, depth) for token in _tokens(text))


def extract_image_refs(text: str) -> list[str]:
    """Return unique top-level image keys in appearance order.

    A reference such as ``[pixivimage:123-2]`` returns ``pixivimage:123-2``.
    Image tags inside an inline tag or heading are not rendered as images and
    are consequently not requested by this function.
    """
    seen: set[str] = set()
    result: list[str] = []
    for token in _tokens(text):
        if token.kind not in ("pixivimage", "uploadedimage"):
            continue
        key = token.kind + ":" + token.value
        if key not in seen:
            seen.add(key)
            result.append(key)
    return result


def _plain(text: str, depth: int = 0) -> str:
    if depth >= _MAX_NESTING:
        return text
    result: list[str] = []
    for token in _tokens(text):
        if token.kind in ("rb", "b", "i", "emphasismark"):
            result.append(_plain(token.value, depth + 1))
        elif token.kind == "chapter":
            result.append("\n\n" + _plain(token.value, depth + 1) + "\n\n")
        elif token.kind == "newpage":
            result.append("\n\n")
        elif token.kind == "jumpuri":
            result.append(token.value + " (" + token.extra + ")")
        elif token.kind == "jump":
            result.append("[" + token.value + "ページへ]")
        else:
            result.append(token.raw)
    return "".join(result)


def plain_text(text: str) -> str:
    """Make a readable text derivative, retaining unresolved/image references.

    Ruby readings are omitted while their base text is retained. This output
    is a derivative, not a lossless replacement for the original pixiv text.
    """
    return _plain(text)


def _image(token: _Token, image_map: Mapping[str, str]) -> str:
    key = token.kind + ":" + token.value
    source = _local_image(image_map.get(key))
    if source is not None:
        return (
            '<figure><img src="' + escape(source, quote=True)
            + '" alt="' + escape(token.raw, quote=True) + '" loading="lazy">'
            + "<figcaption>" + escape(token.raw) + "</figcaption></figure>"
        )
    if token.kind == "pixivimage":
        artwork_id = token.value.split("-", 1)[0]
        return (
            '<p class="unresolved"><a href="https://www.pixiv.net/artworks/'
            + artwork_id + '" rel="noreferrer noopener">'
            + escape(token.raw) + "（挿絵未保存・作品ページ）</a></p>"
        )
    return '<p class="unresolved">' + escape(token.raw) + "（挿絵未保存）</p>"


_STYLE = """
:root{color-scheme:light dark;font-family:serif;line-height:1.95}
body{max-width:48rem;margin:0 auto;padding:2rem 1.2rem 5rem;background:#faf9f6;color:#262421}
header{border-bottom:1px solid #cfcac1;margin-bottom:2rem;padding-bottom:1rem}
h1{font-size:1.8rem;line-height:1.5;overflow-wrap:anywhere}
.author{color:#68635c}.text{white-space:pre-wrap;overflow-wrap:anywhere}
h2{font-size:1.35rem;line-height:1.7;margin:2rem 0 1rem}
a{color:#356a9d;text-underline-offset:.18em}ruby{ruby-position:over}rt{font-size:.55em}
figure{margin:2rem 0;white-space:normal}img{max-width:100%;height:auto}
figcaption,.unresolved{font-size:.85rem;color:#68635c;white-space:normal}
.page+.page{border-top:1px solid #cfcac1;padding-top:2rem;margin-top:3rem}
@media(prefers-color-scheme:dark){body{background:#20201f;color:#eeece6}a{color:#9ac8f3}
.author,figcaption,.unresolved{color:#c5bfb4}}
@media print{body{max-width:none;color:#000;background:#fff}.page+.page{break-before:page;border:0}
a{color:inherit;text-decoration:none}figure{break-inside:avoid}}
"""


def render_novel(
    text: str,
    title: str = "",
    author: str = "",
    image_map: Mapping[str, str] | None = None,
) -> str:
    """Produce a complete HTML document without scripts or remote assets.

    Existing HTTP(S) links are clickable. Page jumps target numbered sections.
    Unsupported or malformed markup stays visible and HTML-escaped. Inline
    nesting is capped to protect against pathological input.
    """
    tokens = _tokens(text)
    image_map = image_map or {}
    total_pages = 1 + sum(token.kind == "newpage" for token in tokens)
    page_number = 1
    chapter_number = 0
    body = ['<section class="page" id="page-1" aria-label="1ページ"><div class="text">']
    for token in tokens:
        if token.kind == "newpage":
            page_number += 1
            body.append(
                '</div></section><section class="page" id="page-' + str(page_number)
                + '" aria-label="' + str(page_number) + 'ページ"><div class="text">'
            )
        elif token.kind == "chapter":
            chapter_number += 1
            body.append('<h2 id="chapter-' + str(chapter_number) + '">' + _inline(token.value) + "</h2>")
        elif token.kind == "jump":
            # String comparison avoids converting attacker-supplied huge integers.
            target = token.value
            in_range = len(target) < len(str(total_pages)) or (
                len(target) == len(str(total_pages)) and target <= str(total_pages)
            )
            if in_range:
                body.append('<a href="#page-' + target + '">' + target + "ページへ</a>")
            else:
                body.append('<span class="unresolved">' + escape(token.raw) + "</span>")
        elif token.kind in ("pixivimage", "uploadedimage"):
            body.append(_image(token, image_map))
        else:
            body.append(_inline_token(token, 0))
    body.append("</div></section>")
    document_title = title or "無題"
    return (
        '<!doctype html>\n<html lang="ja"><head><meta charset="utf-8">'
        '<meta name="viewport" content="width=device-width, initial-scale=1">'
        '<meta name="referrer" content="no-referrer">'
        '<meta http-equiv="Content-Security-Policy" content="default-src \'none\'; '
        "img-src 'self' file:; style-src 'unsafe-inline'; base-uri 'none'; form-action 'none'\">"
        + "<title>" + escape(document_title) + "</title><style>" + _STYLE + "</style></head><body>"
        + "<header><h1>" + escape(document_title) + "</h1>"
        + ('<p class="author">' + escape(author) + "</p>" if author else "")
        + "</header><main>" + "".join(body) + "</main></body></html>\n"
    )

"""Small responsive welcome screen; no mandatory terminal libraries or ANSI."""
import os
import shutil
import sys
import unicodedata

from . import __version__


def width(text):
    return sum(2 if unicodedata.east_asian_width(char) in ("W", "F") else 1 for char in text)


def clip(text, limit):
    if width(text) <= limit:
        return text
    result = ""
    for char in text:
        if width(result + char) > limit - 3:
            break
        result += char
    return result + "..."


# Original pixel lettering and archive illustration; no external art or font dependency.
_GLYPHS = {
    "O": ("01110", "11011", "11011", "11011", "11011", "01110"),
    "P": ("11110", "11011", "11011", "11110", "11000", "11000"),
    "E": ("11111", "11000", "11110", "11000", "11000", "11111"),
    "N": ("11001", "11101", "11111", "11011", "11001", "11001"),
    "I": ("11111", "01110", "01110", "01110", "01110", "11111"),
    "X": ("11011", "11011", "01110", "01110", "11011", "11011"),
    "V": ("11011", "11011", "11011", "11011", "01110", "00100"),
}


def wordmark(unicode=True):
    ink = "█" if unicode else "#"
    rows = []
    for row in range(6):
        rows.append(" ".join("   " if char == " " else
            "".join(ink if bit == "1" else " " for bit in _GLYPHS[char][row])
            for char in "OPEN PIXIV").rstrip())
    return rows


def artwork(unicode=True):
    if not unicode:
        return ["    .--------------------.", "    |       *            |",
                r"    |    /\       /\    |", r"    |___/  \_____/  \___|",
                "    '--------------------'", "              |", r"           \  |  /",
                r"            \ | /", r"             \|/", "        +-----------+",
                "        |  ARCHIVE  |", "        +-----------+"]
    pixels = set()
    def line(x0, y0, x1, y1):
        steps = max(abs(x1 - x0), abs(y1 - y0), 1)
        for n in range(steps + 1):
            pixels.add((round(x0 + (x1 - x0) * n / steps), round(y0 + (y1 - y0) * n / steps)))
    for a, b, c, d in ((4, 3, 55, 3), (55, 3, 55, 33), (55, 33, 4, 33), (4, 33, 4, 3),
                       (5, 31, 20, 11), (20, 11, 35, 31), (26, 31, 40, 17), (40, 17, 54, 31),
                       (18, 55, 18, 62), (18, 62, 42, 62), (42, 62, 42, 55)):
        line(a, b, c, d)
    for x in range(27, 33):
        line(x, 39, x, 52)
    for offset in range(4):
        line(21, 47 + offset, 30, 56 + offset)
        line(30, 56 + offset, 39, 47 + offset)
    for x in range(42, 49):
        for y in range(9, 16):
            if (x - 45) ** 2 + (y - 12) ** 2 <= 10:
                pixels.add((x, y))
    dots = ((0, 0, 1), (0, 1, 2), (0, 2, 4), (1, 0, 8),
            (1, 1, 16), (1, 2, 32), (0, 3, 64), (1, 3, 128))
    return ["".join(chr(0x2800 + sum(bit for dx, dy, bit in dots
                if (x * 2 + dx, y * 4 + dy) in pixels)) for x in range(30)) for y in range(16)]


def welcome():
    from pathlib import Path
    columns = max(20, min(120, shutil.get_terminal_size(fallback=(80, 24)).columns))
    encoding = sys.stdout.encoding or "utf-8"
    try:
        "╭─│╰╮╯█⣿".encode(encoding)
        unicode = True
    except UnicodeEncodeError:
        unicode = False
    color = (sys.stdout.isatty() and "NO_COLOR" not in os.environ
             and os.environ.get("TERM") != "dumb"
             and (os.name != "nt" or "WT_SESSION" in os.environ or "ANSICON" in os.environ))
    def paint(text, row=0, muted=False):
        if not color:
            return text
        palette = ((108, 232, 245), (82, 211, 246), (67, 187, 245),
                   (61, 161, 241), (70, 140, 233), (96, 124, 222))
        red, green, blue = (140, 157, 180) if muted else palette[min(row, 5)]
        return f"\033[38;2;{red};{green};{blue}m{text}\033[0m"
    print()
    logo = wordmark(unicode)
    logo_width = max(width(row) for row in logo)
    if logo_width * 2 <= columns:
        logo = ["".join(char * 2 for char in row) for row in logo]
    if max(width(row) for row in logo) <= columns:
        for row, text in enumerate(logo):
            print(paint(text, row))
        print(paint("D O W N L O A D E R", 5))
    else:
        print(paint(clip("OPEN PIXIV DOWNLOADER", columns)))
    print()
    right = ["DOWNLOAD", "/download  URLをまとめて保存", "/demo      同梱データで試す（通信なし）", "",
             "LIBRARY", "/verify    保存済み作品を検査", "/pack      完成した作品をZIPへ", "",
             "SESSION", "保存先（既定） archive", "並行保存       1〜4作品 / 既定2",
             "研究用alpha・実サイト検証中", "", "/help  操作一覧    /quit  終了"]
    left = artwork(unicode) + ["", "LOCAL FIRST / YOUR ARCHIVE", "".join(c for c in str(Path.cwd()) if c.isprintable())]
    if columns >= 92:
        rows = []
        for n in range(max(len(left), len(right))):
            a = clip(left[n] if n < len(left) else "", 30)
            b = right[n] if n < len(right) else ""
            rows.append(a + " " * (34 - width(a)) + b)
    else:
        rows = ["LOCAL FIRST / YOUR ARCHIVE", *right]
    if columns >= 40:
        top, horizontal, side, bottom, endtop, endbottom = ("╭", "─", "│", "╰", "╮", "╯") if unicode else ("+", "-", "|", "+", "+", "+")
        title = clip(f" Open Pixiv Downloader v{__version__} ", columns - 4)
        before = (columns - 2 - width(title)) // 2
        print(paint(top + horizontal * before + title + horizontal * (columns - 2 - before - width(title)) + endtop))
        for row in rows:
            text = clip(row, columns - 4)
            print(paint(side) + " " + paint(text) + " " * (columns - 3 - width(text)) + paint(side))
        print(paint(bottom + horizontal * (columns - 2) + endbottom))
    else:
        for row in rows:
            print(clip(row, columns))
    print()
    print(paint(clip("Welcome to Open Pixiv Downloader!", columns)))
    print(clip("URLを貼り付けて開始。操作一覧は /help", columns))
    print()


def help_text():
    print("  /download  URLをまとめて保存       /demo    通信なしで試す")
    print("  /verify    保存済み作品を検査       /pack    ZIPにまとめる")
    print("  /help      操作一覧                 /quit    終了")
    print("\nURLをそのまま貼って開始できます。数字1〜4、0での操作にも対応しています。")

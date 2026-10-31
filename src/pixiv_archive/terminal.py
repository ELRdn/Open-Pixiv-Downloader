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


def welcome():
    columns = max(20, min(96, shutil.get_terminal_size(fallback=(80, 24)).columns))
    color = (sys.stdout.isatty() and "NO_COLOR" not in os.environ
             and os.environ.get("TERM") != "dumb"
             and (os.name != "nt" or "WT_SESSION" in os.environ or "ANSICON" in os.environ))
    def accent(text):
        return f"\033[96m{text}\033[0m" if color else text

    print()
    if columns >= 60:
        for line in ("  ___  ____  ____", " / _ \\|  _ \\|  _ \\",
                     "| | | | |_) | | | |", "| |_| |  __/| |_| |", " \\___/|_|   |____/"):
            print(accent(line))
        print()
    print(accent(clip("Welcome to Open Pixiv Downloader", columns)))
    print(clip("ローカルに保存して、あとから確かめる。", columns))
    print()
    lines = [f"Open Pixiv Downloader  v{__version__}",
             "保存先（既定）  archive", "並行保存        1〜4作品 / 既定2",
             "状態            研究用alpha・実サイト検証中"]
    if columns >= 40:
        try:
            "╭─│╰╮╯".encode(sys.stdout.encoding or "utf-8")
            top, horizontal, side, bottom, endtop, endbottom = "╭", "─", "│", "╰", "╮", "╯"
        except UnicodeEncodeError:
            top, horizontal, side, bottom, endtop, endbottom = "+", "-", "|", "+", "+", "+"
        print(accent(top + horizontal * (columns - 2) + endtop))
        for line in lines:
            text = clip(line, columns - 4)
            print(accent(side) + " " + text + " " * (columns - 3 - width(text)) + accent(side))
        print(accent(bottom + horizontal * (columns - 2) + endbottom))
    else:
        for line in lines:
            print(clip(line, columns))
    print()


def help_text():
    print("  /download  URLをまとめて保存       /demo    通信なしで試す")
    print("  /verify    保存済み作品を検査       /pack    ZIPにまとめる")
    print("  /help      操作一覧                 /quit    終了")
    print("\nURLをそのまま貼って開始できます。数字1〜4、0での操作にも対応しています。")

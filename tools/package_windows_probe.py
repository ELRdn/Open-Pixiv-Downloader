"""Build the diagnostic extension ZIP from its checked-in source, deterministically."""
from pathlib import Path
from zipfile import ZIP_DEFLATED, ZipFile, ZipInfo


def main():
    tools = Path(__file__).resolve().parent
    source = tools / "windows-probe"
    target = tools / "open-pixiv-metadata-probe-windows.zip"
    names = (
        "manifest.json", "background.js", "collector.mjs", "report.html", "report.css",
        "report.js", "README_JA.md", "LICENSE", "collector.test.mjs",
    )
    with ZipFile(target, "w") as archive:
        for name in names:
            info = ZipInfo(f"open-pixiv-metadata-probe/{name}", (1980, 1, 1, 0, 0, 0))
            info.create_system = 3
            info.external_attr = 0o100644 << 16
            info.compress_type = ZIP_DEFLATED
            archive.writestr(info, (source / name).read_bytes())
    print(f"Built {target.name}")


if __name__ == "__main__":
    main()

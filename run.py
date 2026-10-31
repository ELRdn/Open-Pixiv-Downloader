"""Run from an unpacked source tree without installation."""
from pathlib import Path
import sys

sys.path.insert(0, str(Path(__file__).resolve().parent / "src"))
from pixiv_archive.cli import main

raise SystemExit(main())

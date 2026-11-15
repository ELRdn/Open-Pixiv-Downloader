# Development

[English README](../README.md) · [日本語README](../README.ja.md) · [All docs](README.md)

The current app is Rust + Ratatui. Python is used for legacy compatibility checks, not by the Rust app at runtime. The landing page is a separate TypeScript + Vite project.

## Run locally

From the repository root, with Rust stable 1.89+:

```sh
cargo run --release --locked -- --no-update-check
```

Use **F2** for the built-in offline demo. To check the CLI without touching a real archive, choose a separate output directory:

```sh
cargo run --locked -- plan user:9001 --demo --all --out demo-archive
```

## Checks

```sh
cargo fmt --check
cargo clippy --locked --all-targets -- -D warnings
cargo test --locked
cargo build --locked
python tools/check_rust_compat.py target/debug/opixiv
python tools/check_bulk.py target/debug/opixiv --count 4000
```

On Windows, use `target/debug/opixiv.exe` in the last two commands. The Python checks need Python 3.11+.

Additional targeted checks:

```sh
python run_tests.py
node --test tools/windows-probe/collector.test.mjs
```

[GitHub Actions](https://github.com/ELRdn/Open-Pixiv-Downloader/actions/workflows/rust.yml) runs Windows/Linux builds, Rust/Python tests, staged self-update, archive compatibility, and synthetic bulk save/resume. Linux also checks crash recovery. CI and synthetic fixtures do not establish live Pixiv behavior or physical-device acceptance. Older numeric test totals remain in their dated evidence files.

## Landing page

Use Node.js 22.18+ (24 LTS recommended). From `website/`:

```sh
npm ci
npm test
npm run build
npm run dev
```

See the [website guide](../website/README.md) for static-host settings and recording provenance. `website/dist/` is the deployable output. Do not publish local recording tools or demo archives.

## Repository map

| Location | Purpose |
| --- | --- |
| `rust/`, `Cargo.toml`, `Cargo.lock` | Current CLI, TUI, and archive core |
| `website/` | Bilingual landing page and actual TUI recordings |
| `src/pixiv_archive/`, `run.py`, `tests/` | Legacy Python implementation and compatibility baseline |
| `examples/` | Synthetic fixtures; no real works or credentials |
| `tools/` | Compatibility, bulk-save, crash-recovery, and update checks |
| [Windows diagnostic extension](../tools/windows-probe/README_JA.md) | Metadata diagnostics; does not collect image files or novel text |
| `docs/evidence/` | Dated validation records |

For archive semantics, see [usage](USAGE_JA.md), [bulk operations](BULK_DOWNLOAD_JA.md), and [collection rules](COLLECTION_JA.md). Preserve provenance and license notices in [NOTICE](../NOTICE.md).

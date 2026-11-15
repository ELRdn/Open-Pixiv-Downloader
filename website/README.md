# OpenPixiv landing page

[English README](../README.md) · [日本語README](../README.ja.md) · [Development](../docs/DEVELOPMENT.md)

A static, bilingual TypeScript + Vite site: actual TUI recording → install → workflow demos → comparison → open-source facts → FAQ. Its colors follow the app's dark background, cyan, and blue.

**Live:** [日本語](https://openpixiv.pages.dev/) · [English](https://openpixiv.pages.dev/?lang=en)

## Preview locally

Use **Node.js 22.18+**; Node.js 24 LTS is recommended. The test command uses Node's built-in TypeScript support.

```sh
cd website
npm ci
npm run dev
```

Open the localhost URL Vite prints. For a production preview:

```sh
npm test
npm run build
npm run preview
```

`npm run build` checks TypeScript and creates `dist/`. The preview normally opens at `http://127.0.0.1:4173/`.

## Static hosting

| Host | Project root | Build command | Output |
| --- | --- | --- | --- |
| Cloudflare Pages | `website` | `npm run build` | `dist` |
| GitHub Pages | Build from `website` after `npm ci` | `npm run build` | Upload `website/dist` as the Pages artifact |

The build uses `--base=./` so assets work under a repository subpath. Set the host's Node version to 24.

### Production deployment

Published on 2026-11-15 at **https://openpixiv.pages.dev**, using Cloudflare Pages [Direct Upload](https://developers.cloudflare.com/pages/get-started/direct-upload/). The Pages project is `openpixiv`, with production branch `main`; `wrangler.jsonc` records the target and build-output directory.

After authenticating with the intended Cloudflare account, run from `website/`:

```sh
npm test
npm run build
npm exec --yes --package=wrangler@4.162.0 -- wrangler pages deploy dist --project-name openpixiv --branch main
```

For API-token authentication, supply `CLOUDFLARE_ACCOUNT_ID` and `CLOUDFLARE_API_TOKEN` through the environment. Keep their values out of source control. The token needs Pages write access to the target account.

This is an existing Pages project. Wrangler 4.162.0 delegates creation of new Pages projects by agents to Workers automatically, so new-project setup needs separate verification of the target product. The command above updates the verified Pages project directly. GitHub pushes run CI; they do not automatically deploy this Direct Upload project.

## Language and playback

- JP / EN switches page copy, all four GIFs, their static PNGs, full-size links, and original recording links. `?lang=en` opens English directly.
- Recordings autoplay and loop without playback bars. One external pause button stops or resumes all four, and switching language preserves the paused state.
- Reduced-motion preferences and hidden tabs use static PNGs.
- Tap a recording to open the full-size GIF. On mobile, the complete terminal is scaled to fit.
- The mobile header menu links to features, installation, FAQ, and GitHub. It closes after choosing a link or pressing Escape.
- Language changes hide the previous recording until its replacement is decoded. Copy-success feedback and its screen-reader announcement follow the selected language.
- Install tabs provide a copyable Cargo command and the published Windows x64 download. The guide link follows the selected README language.
- Playback needs no video player or WASM. The site uses Google Fonts with system fallbacks.

## Recording provenance

All eight recordings capture the **actual Windows TUI of opixiv 0.3.0-alpha.1**, separately in Japanese and English, through ConPTY at 120 columns × 36 rows. The update check was disabled. F2 enables the offline fixture, and capture begins with a real redraw of that screen; startup waiting time is omitted.

| Scene | What it shows |
| --- | --- |
| `hero` | List a creator's works, then resume an existing archive |
| `batch` | List four synthetic works from `user:9001` |
| `resume` | Resume the four saved works with F11; the TUI shows zero fetch requests |
| `outputs` | Inspect the image and novel collection paths with F4 |

In `public/recordings/`, names without a suffix are Japanese; `-en` names are English. Each scene has its original `.cast`, a looping `.gif`, and a `.png` extracted from the GIF. README files reuse these assets without duplicating them.

The archive was prepared from the repository's synthetic fixture using the existing Python CLI, then checked and collected with the Rust CLI. The recordings demonstrate an existing synthetic archive. They do not show new Pixiv media downloads or establish performance or live-site compatibility. Python is not used by the website.

<details>
<summary>Version, digest, and media tools</summary>

Recorded on 2026-11-15 with the installed `opixiv 0.3.0-alpha.1`. Binary SHA-256:

```text
ddad2b8067cb3a8f4ccc2144f54af70b446da445a35a2a0fb9a9d24ef52f3b11
```

GIFs were rendered with official [asciinema/agg](https://github.com/asciinema/agg) v1.9.0 at font size 16 and line height 1.1. The agg archive digest was checked against GitHub's API. PNGs were extracted with FFmpeg. The images are 1073 × 651; the terminal content was not recreated or translated after recording.

Local capture tools and fixture archives stay in the ignored `recording-work/` directory. They are not needed to build or run the site.

</details>

## Validation and maintenance

The media test checks all eight casts for version, language, dimensions, chronological events, terminal color output, and absence of credential patterns; it also checks GIF/PNG signatures and infinite GIF looping.

Browser checks cover Japanese/English at 1280px and 390px widths, overflow, install tabs, copy feedback, autoplay/pause, paused-language switching, reduced motion, and direct English access. Physical phones and screen-reader acceptance have not been tested.

Production checks on 2026-11-15 confirmed HTTP 200, matching SHA-256 hashes for all 28 deployed files, Japanese/English media switching, copy feedback, download/guide links, direct English access, and no horizontal overflow at 390px. The browser console reported no warnings or errors.

Public facts were checked on **2026-11-15**: GitHub Star 1, MIT, and the v0.3.0-alpha.1 prerelease with Windows x64 / Linux x64 binaries and SHA256SUMS. Before publishing a new version, update the facts and download links together with the recordings. Do not infer current release availability from an older snapshot.

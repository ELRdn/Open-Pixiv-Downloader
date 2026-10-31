# 既存OSSの比較

2026年10月30日時点のコミットを固定したソース比較。実サイトでの互換性を保証するものではない。

| 候補 | 固定コミット | 上流本体の宣言 | 確認した長所 | 今回の要件で追加検証する点 |
|---|---|---|---|---|
| Powerful Pixiv Downloader | `2e137d0b45f4bc897f0792994dfa7176884922ce` | GPL-3.0-or-later | ブラウザ拡張、4形式、シリーズ・参照挿絵の取得経路 | ローカル素材の検査、欠損判定、版保持 |
| gallery-dl | `175d21fd8f7e52963911c02916d0634461cddb24` | GPL-2.0-only | 画像・うごイラ・小説・シリーズ、SQLite履歴、Windows配布案内 | 調査した経路のrefresh-token要件、小説表示、作品単位の完全性・版保持 |
| PixivUtil2 | `46bb910cd60c213c9199d939859670a044450239` | BSD-2-Clause | Windows案内、Cookie認証、小説・シリーズのモデル | 記法の再現、素材同梱、安全なHTML生成、版保持 |
| PixivPy | `4f2e9ea7fff6247d9f5bfe5a862e92c5dfe3b6dd` | Unlicense | 作品・うごイラ・小説・シリーズのAPIラッパー | 保存・欠損・履歴・UIは別責務。通常閲覧条件との一致も要実証 |

ライセンス欄は固定版のpackage／setup宣言とLICENSEに基づく上流本体の表示であり、全依存物の監査済みという意味ではない。参照先のソースツリーはこのリポジトリへ同梱していない。

## 保存品質への示唆

- 取得履歴のキーが存在するだけでは、保存した全素材の完全性や旧版を証明できない。
- 本文TXTの取得と、ルビ・挿絵・シリーズを保ったオフライン閲覧は別の受入条件。
- 作品参照挿絵を独立作品として取り直す方式と、小説側で表示可能な関係を確認する方式を区別する。
- 確認したPowerful Pixiv Downloaderの経路には、作者一覧取得の例外で空配列を返す処理、429で上限なく再試行する処理がある。新しい保存結果の契約に無条件に採用しない。
- Windows配布案内の存在と、こちらのWindows実機での動作確認を分ける。

PixivPyの固定版は、元のlockfileに基づく依存環境でHTTP・JSON等のオフラインテスト11件が合格した。小説の完全保存・実サイト・Windowsを確認した結果ではない。

α版2点の現物比較を追加した結果、Archive Labの保存コアを基盤に選んだ。[採用理由と不足](ALPHA_REVIEW_JA.md)を参照。

## 一次ソース

- [Powerful Pixiv Downloaderの宣言](https://github.com/xuejianxianzun/PixivBatchDownloader/blob/2e137d0b45f4bc897f0792994dfa7176884922ce/package.json)、[API](https://github.com/xuejianxianzun/PixivBatchDownloader/blob/2e137d0b45f4bc897f0792994dfa7176884922ce/src/ts/API.ts)
- [gallery-dlの取得処理](https://github.com/mikf/gallery-dl/blob/175d21fd8f7e52963911c02916d0634461cddb24/gallery_dl/extractor/pixiv.py)、[履歴](https://github.com/mikf/gallery-dl/blob/175d21fd8f7e52963911c02916d0634461cddb24/gallery_dl/archive.py)、[宣言](https://github.com/mikf/gallery-dl/blob/175d21fd8f7e52963911c02916d0634461cddb24/setup.py)
- [PixivUtil2の小説モデル](https://github.com/Nandaka/PixivUtil2/blob/46bb910cd60c213c9199d939859670a044450239/model/PixivNovel.py)、[処理](https://github.com/Nandaka/PixivUtil2/blob/46bb910cd60c213c9199d939859670a044450239/handler/PixivNovelHandler.py)、[HTML](https://github.com/Nandaka/PixivUtil2/blob/46bb910cd60c213c9199d939859670a044450239/novel_template.html)
- [PixivPyの小説API](https://github.com/upbit/pixivpy/blob/4f2e9ea7fff6247d9f5bfe5a862e92c5dfe3b6dd/pixivpy3/aapi.py)、[宣言](https://github.com/upbit/pixivpy/blob/4f2e9ea7fff6247d9f5bfe5a862e92c5dfe3b6dd/pyproject.toml)
- [調査したGit参照の記録](evidence/upstream-refs.json)

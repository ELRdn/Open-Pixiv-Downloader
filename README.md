# Open Pixiv Downloader

イラスト・漫画・うごイラ・小説を、欠損や破損を確認しながらローカルへ保存するための開発中プロジェクトです。Windowsを優先し、小説の構造、再開、更新検出、旧版保持を重視します。

**現在は研究用α版です。実サイトでの4形式の取得、Windows実機、更新検出・旧版保持は未検証または未実装です。** Pixivの公式製品ではありません。

## このリポジトリの内容

- `src/pixiv_archive/`：**Pixiv Archive Lab 0.1.0a1** を基盤に、一括入力と並列保存を追加したPython製CLI。パッケージ名とCLI名は元のものです。
- `examples/`、`tests/`：実作品を含まない合成データ、既存33件と追加7件のテスト。
- [Windows診断拡張](tools/windows-probe/README_JA.md)：指定4作品のメタデータを通常の閲覧セッションで確認するEdge／Chrome用補助ツール。
- [計画](docs/PLAN_JA.md)、[OSS比較](docs/COMPARISON_JA.md)、[α版の採用判断](docs/ALPHA_REVIEW_JA.md)、[現在の検証状況](docs/STATUS_JA.md)。

## オフラインで試す

Python 3.11以上で、リポジトリのルートから実行します。実行時の追加パッケージは不要です。Windowsでは環境に応じて `python` を `py -3` に置き換えてください。

```sh
python run.py plan user:9001 --fixture examples/synthetic-fixture.json --out demo-archive
python run.py fetch user:9001 --fixture examples/synthetic-fixture.json --out demo-archive
python run.py verify demo-archive
python run.py pack demo-archive demo-export.zip
```

同梱の架空の4作品を使用し、実サイトへは接続しません。小説の出力は `demo-archive/works/novel_1004/index.html` です。再度 `fetch` すると、整合性を確認できた保存済み作品をスキップします。

```sh
python run_tests.py
node --test tools/windows-probe/collector.test.mjs
```

Node.jsは診断拡張の開発用テストにだけ必要です。Pythonコアのテストは40件、診断処理のテストは8件です。Linuxでの実行結果であり、Windows対応の証明ではありません。

## URLをまとめて指定する

引数に複数の作品・作者・シリーズURLを指定するか、UTF-8のテキストファイルに1行1URLで記載します。両方を併用でき、重複する作品は1回だけ処理します。空行と `#` で始まるコメント行は無視します。

```sh
# 同梱データで作者・単体作品を混ぜ、2作品を並列保存する
python run.py fetch user:9001 art:1001 --fixture examples/synthetic-fixture.json --all --jobs 2 --out demo-archive

# 実サイト用の書式例：urls.txtに作品・作者のURLを記載する
python run.py plan --input urls.txt --online --all --out archive
python run.py fetch --input urls.txt --online --all --jobs 2 --out archive
```

作者URLは `https://www.pixiv.net/users/作者ID` の形式です。列挙できたイラスト・漫画・うごイラ・小説を対象にします。`--types novel` または `--types art` で絞り込めます。

`--jobs` は1〜4（既定1）。媒体の転送を重ねても、リクエスト開始間隔と要求予算は全ワーカーで共有します。401・403・429等で停止を共有し、後続を投入しません。`--all` は作品数の選択上限を外しますが、要求予算は維持し、残りは未完了として記録します。同じコマンドを再実行すると検査済みの作品をスキップして続きを進めます。

実サイトの経路は匿名取得のみで、通常セッションとの連携は未完成です。`--all` は、閲覧条件で見えない作品や削除済み作品まで取得できる意味ではありません。

[CLI・GUI・Rustの開発方針](docs/PRODUCT_DIRECTION_JA.md)も参照してください。

## Windowsでのメタデータ診断

[診断拡張ZIP](tools/open-pixiv-metadata-probe-windows.zip)を展開し、[同梱手順](tools/windows-probe/README_JA.md)に沿ってEdge／Chromeへ読み込みます。Cookieやパスワード、小説本文をレポートへ出力しません。画像・ZIP本体は取得しません。

対象は診断用に指定されたイラスト149564947、漫画130117333、小説29476682、うごイラ151427872です。403・429などでは全体を停止し、停止したレポートも保存できます。実作品データや診断レポートをGitへコミットしないでください。

## 既知の不足

| 項目 | 現状 |
|---|---|
| ページ不足、素材欠損、保存後の画像破損 | 合成データで検出・修復を確認 |
| うごイラZIP内部の破損 | CRC不一致を完了として扱うケースあり。改修対象 |
| 更新検出・旧版保持 | 未実装。通常の再開とは別に追加予定 |
| 本人の通常セッションとの連携 | Pythonコアは匿名のみ。実証・分離設計が必要 |
| 実サイト互換性 | クラウドからの作品APIはHTTP 403。未検証 |
| Windows実機・診断拡張のインストール | 未検証 |

## 利用条件とライセンス

[公式ガイドライン](https://policies.pixiv.net/)の「アプリケーション、各種サービス等への使用について」には、クローラー等を用いた作品収集の禁止が明記されています。作者の許諾とサービス側の利用条件は別に確認し、自動取得方式の適合を確認済みとは扱っていません。

コードは[MIT](LICENSE)。取り込んだα版と追加した診断ツールの来歴は[NOTICE](NOTICE.md)を参照してください。コードのライセンスは、Pixiv上の作品の利用許可やサービスへのアクセス許可を付与するものではありません。

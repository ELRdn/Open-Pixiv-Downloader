# Open Pixiv Downloader

**Rust + Ratatui** のローカル保存ツールです。大きな立体文字のロゴから起動し、作品・作者・小説シリーズのURLをまとめて入力できます。Windowsを優先して開発しています。

![保存結果と保存先を開くボタン](docs/images/ratatui-output-paths.png)

*合成4作品の保存後。実際のLinux端末出力を120桁×36行で取得し、レンダリングした表示例です。Windows実機のスクリーンショットではありません。*

研究用α版です。実サイトでの4形式の取得、本人セッションとの連携、Windows実機の操作は未検証です。更新検出・旧版保持は未実装です。Pixivの公式製品ではありません。

## 起動

[Rustの現行stable版](https://www.rust-lang.org/tools/install)を用意し、PowerShellを開きます。Windowsでソースからビルドする場合は、Rustの案内に従いVisual Studio Build ToolsのC++ビルド環境も導入してください。

```powershell
cargo install --git https://github.com/ELRdn/Open-Pixiv-Downloader.git --branch main --locked --force
opixiv
```

更新も同じインストールコマンド1行で行えます。作業フォルダーの移動や `git pull` は不要です。リポジトリから開発・試用する場合は、cloneしたフォルダー内で `cargo run --release --locked` を使えます。Rust版の実行にPythonは必要ありません。デモ用データも実行ファイルに組み込まれています。

以前Python版の `opixiv` をインストールしていた場合は `opixiv --version` を確認してください。新しいRust版は `opixiv 0.2.0-alpha.3` です。古いコマンドが優先される場合は、PowerShellで `& "$env:USERPROFILE\.cargo\bin\opixiv.exe"` と直接起動できます。

## 操作

URL欄へ1行に1URLを貼り付け、Tabで保存先・並行数へ移動します。**Ctrl+S → Enter** で確認して保存開始。初めて試すときは **F2 → Ctrl+S → Enter** で、架空の4作品を通信せず保存できます。

| キー | 操作 |
|---|---|
| Tab / Shift+Tab | URL・保存先・並行数・作品一覧を移動 |
| Enter | URL欄で改行、確認画面で保存開始 |
| F2 | デモ／匿名オンラインを切り替え。入力と保存先はモード別に保持 |
| F3 | 結果・欠損・停止理由の詳細。↑↓でスクロール |
| F5 | 対象一覧を確認。媒体はダウンロードしない |
| Ctrl+S | 保存内容の確認を開く |
| Alt+V | 選択した保存先の整合性を検査 |
| Alt+E | 完成済み作品を「保存先名-export.zip」へ出力 |
| ← / →、1〜4 | 並行数欄で同時保存数を設定 |
| ↑ / ↓、PgUp / PgDn | 一覧で作品を選択。Enterでそのフォルダーを開く |
| F6 / F7 | 保存先／選択した作品のフォルダーを開く。画面下のボタンもクリック可能 |
| F8 / F9 | 最新版を再確認／更新方法を開く。ロゴ下のボタンもクリック可能 |
| F1 | ヘルプ |
| Esc / Ctrl+C | 実行中は停止を要求し、処理終了を待つ。待機中は終了 |

一覧は `WORK`・`STATUS`・`OUTPUT / PATH` の3列です。実際に作成された作品フォルダーを表示し、長いパスは末尾を残して省略します。F3ではフルパスを確認できます。検査後も行を保持し、再開でスキップした保存済み作品も表示します。Windowsはエクスプローラー、macOSはFinder、Linuxは既定のファイルマネージャーを呼び出します。

16個のURLを貼った場合、既定では2作品を同時に保存し、終わった枠に次の作品を入れます。並行数は1〜4。作者URLは列挙できたイラスト・漫画・うごイラ・小説に展開し、単体URLとの重複を除きます。TUIは作品数の選択上限なしですが、共有の要求予算500件は維持します。

ロゴは `█` の面と `╔═╝` などの影を描き分けます。横幅・高さに応じてロゴと2カラムを縮小し、`NO_COLOR` で色を無効にできます。大きなロゴは、たとえば120桁×36行のWindows Terminalで表示できます。画面が狭い場合は短いロゴ・縦配置になります。

## ツールの更新通知

起動時にGitHubの最新版をバックグラウンドで確認します。その後は保存処理が動いていないときに1時間ごとに確認し、新しいバージョンがあればロゴの下へ **「アップデートが利用できます！」** と表示します。同じバージョンのコミットだけでは通知しません。

**F9 → Enter** でツールを終了し、PowerShellへ貼り付ける更新コマンドを表示します。保存処理の実行中はこの操作で終了せず、完了後に更新へ進めます。F8で手動再確認でき、再確認の間隔は最低1分です。

更新確認は固定のGitHub URLへ接続します。合成デモの保存処理とは別の通信です。完全にオフラインで起動する場合は `opixiv --no-update-check`、または環境変数 `OPIXIV_NO_UPDATE_CHECK=1` を使用してください。引数付きの保存・検査コマンドでは更新確認をしません。通信失敗時は確認できなかった旨を表示し、作品の保存操作は続けられます。[更新確認の仕様](docs/UPDATES_JA.md)も参照してください。

## 保存コア

- イラスト・漫画：元画像とページ順を保存し、ページ数を照合。
- うごイラ：元ZIPとフレーム順・表示時間を保存。全メンバーを読み、CRCエラーを検出。変換はしない。
- 小説：原文・読みやすいテキスト・オフラインHTMLを保存。章、改ページ、ルビ、太字・斜体、リンク、表紙、許可された挿入関係の画像を扱う。未知の記法は消さずに表示。
- 保存状態：SQLite、SHA-256付きmanifest、原子的なファイル確定、排他的な実行ロック。再開時は整合性を確認し、完了済み作品をスキップ、破損した素材を再取得。
- 不足・失敗・停止を完了と区別。ZIP出力は整合性検査後の完成済み作品だけを対象にする。

既存Python版のアーカイブと同じ保存形式を使用します。合成4形式について、両方向の検査・再開・ZIP出力と原本バイト列の一致を確認しています。生成するHTMLの装飾やJSONの空白は同一ではありません。更新検出と旧版保持は、再開とは別に今後追加します。

## コマンドで使う

引数付きコマンドはJSONを返します。標準入力・出力がパイプの場合、引数なしのTUIは開始しません。

```powershell
opixiv plan user:9001 --demo --all --out demo-archive
opixiv fetch user:9001 art:1001 --demo --all --jobs 2 --out demo-archive
opixiv verify demo-archive
opixiv pack demo-archive demo-export.zip

# UTF-8の一覧ファイル。空行、#コメント、BOMに対応
opixiv plan --input urls.txt --online --all --out archive
opixiv fetch --input urls.txt --online --all --jobs 2 --out archive
```

`--demo`、`--fixture examples/synthetic-fixture.json`、`--online` のいずれかを指定します。`--types art|novel|all`、`--limit`、`--retry-unavailable`、`--request-budget`、`--max-file-mib` も利用できます。

オンラインのリクエスト開始間隔は全ワーカーで共有し、既定2秒。401・403・429、リダイレクト、HTML応答では全体を停止します。自動リトライはせず、再実行で再開します。`--all` は作品数の選択上限を外す指定で、要求予算や閲覧条件を変更しません。現状は匿名取得のみ、公開・全年齢と確認できる作品だけを対象とします。

## 速度について

既定の「リクエスト開始間隔2秒」は、APIと画像取得の全ワーカーで共有するツール側の設定です。1枚絵を16作品、新規保存すると、詳細16回＋ページ情報16回＋画像16回の約48リクエストとなり、開始間隔だけで少なくとも約94秒かかります。作者URLの列挙や追加素材があればさらに増えます。並行数4でも、転送の重なりが増える一方で、この開始間隔は共有されます。

さらに転送・原子的な書き込み・ハッシュ照合・うごイラのCRC検査がかかります。利用者の回線・ストレージ別の内訳は未測定です。新しい実行結果には所要時間、送信間隔の待機時間、リクエスト数を表示し、F3と `last-run.json` でも確認できます。送信間隔の待機は他作品の転送と重なるため、所要時間から差し引いた値を転送時間とは扱いません。詳細は[速度の仕組み](docs/SPEED_JA.md)を参照してください。

## 開発と検証

```sh
cargo fmt --check
cargo clippy --locked --all-targets -- -D warnings
cargo test --locked
cargo build --locked
python tools/check_rust_compat.py target/debug/opixiv
```

Windowsの互換性確認では最後のパスを `target/debug/opixiv.exe` にしてください。Pythonは旧版との比較検証にのみ必要で、Rust版の実行時には呼び出しません。

Rustテスト34件、旧Pythonテスト49件、両方向のアーカイブ互換性検査をLinuxで実施しています。実ターミナルでもデモ保存・検査・ZIP出力・終了を確認済みです。GitHub ActionsにWindows／Linuxのビルド・テストと実行ファイルのartifact出力を追加しています。CIの成功とWindows実機での表示確認は区別します。

| 場所 | 内容 |
|---|---|
| `rust/`、`Cargo.toml`、`Cargo.lock` | 現行のRust製CLI・Ratatui・保存コア |
| `src/pixiv_archive/`、`run.py`、`tests/` | 比較検証用の旧Python版。`python run.py` で従来画面を起動 |
| `examples/` | 自作の合成fixture。実作品・認証情報なし |
| [Windows診断拡張](tools/windows-probe/README_JA.md) | 通常閲覧セッションでメタデータを確認する補助ツール |
| [移行記録](docs/RUST_MIGRATION_JA.md)・[検証状況](docs/STATUS_JA.md) | 移行の範囲と既知の不足 |
| [計画](docs/PLAN_JA.md)・[OSS比較](docs/COMPARISON_JA.md) | 目標と採用判断 |

旧Python版のテストは `python run_tests.py`、診断拡張のテストは `node --test tools/windows-probe/collector.test.mjs` です。診断拡張は画像本体や小説本文を収集するものではありません。

## 既知の不足と利用条件

本人セッションとの連携、4形式の実サイト互換性、Windows実機、全小説記法・媒体デコーダーによる照合は未検証です。クラウドからの作品APIはHTTP 403で、取得可能と判定していません。更新検出・旧版保持・GUIは未実装です。従来Python版にはうごイラZIPのCRC不一致を見逃すケースが残っています。

[公式ガイドライン](https://policies.pixiv.net/)の「アプリケーション、各種サービス等への使用について」には、クローラー等を用いた作品収集の禁止が明記されています。作者の許諾とサービス側の利用条件は別に確認し、自動取得方式の適合を確認済みとは扱っていません。

コードは[MIT](LICENSE)。来歴は[NOTICE](NOTICE.md)を参照してください。コードのライセンスは、作品の利用許可やサービスへのアクセス許可を付与しません。

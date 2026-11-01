# ツールの更新

alpha.6以降は、PowerShellで次を実行して更新する。

```powershell
opixiv update
```

最新版の番号を確認し、新しい版があれば、固定の公式mainから一時領域へビルドする。生成された実行ファイルの製品名・バージョンを検証した後、**今起動している実行ファイルそのもの**を置き換える。リポジトリのフォルダーへの移動やgit pullは不要。完了後は `opixiv` で起動する。

更新にはRust/CargoとGitが必要で、ビルドに数分かかる場合がある。現在の保存処理・別ウィンドウのopixivを終了してから実行する。アーカイブ・本文・画像を更新処理で書き換えることはない。

## 初回導入・alpha.5以前からの更新

古い版には `update` サブコマンドがないため、一度だけ従来のコマンドで更新する。

```powershell
cargo install --git https://github.com/ELRdn/Open-Pixiv-Downloader.git --branch main --locked --force
```

alpha.6以降になったら、以後は `opixiv update` を使用する。

## 確認・再インストール

```powershell
opixiv update --check
opixiv update --force
```

`--check` は最新版の確認だけで終了し、Rust/Cargoの起動、ビルド、本体の置換を行わない。`--force` は番号の事前確認を省略して、公式mainからビルド・再インストールする。同じ番号の修正や、確認先に接続できない場合にも使えるが、取得した実行ファイルが現在より古ければ置換しない。`--check` と `--force` は併用できない。

通常の更新で新しい版がなければ、ビルドせず終了する。新しい版を確認済みの場合は、それより古い生成物も拒否する。更新版の `--version` 実行には5秒・4KiBの上限を設ける。

## TUIの更新通知

TUI起動時と、保存が動いていない時間帯に1時間ごとに最新版を確認する。新しい版があればロゴ下へ「アップデートが利用できます！」を表示する。

- F8／確認ボタン：手動で再確認。直前の確認から最低1分空ける。
- F9／更新方法ボタン：現在・GitHubの版と更新方法を表示。
- 更新画面でEnter：保存中でなければTUIを終了し、コピー用の `opixiv update` を表示。
- Esc：画面を閉じる。

F9のEnter自体はインストールを実行しない。表示したコマンドをPowerShellで実行する。保存処理の実行中はEnterで終了せず、処理完了後に更新へ進める。

自動確認を止める場合は `opixiv --no-update-check` または環境変数 `OPIXIV_NO_UPDATE_CHECK=1`。F8や `update` は明示操作なので使用できる。plan/fetch/verify/collect/packには自動確認を加えない。

## 取得先・失敗時の動作

番号確認先は固定の `https://raw.githubusercontent.com/ELRdn/Open-Pixiv-Downloader/main/Cargo.toml`。製品名を照合し、SemVerの優先順位で比較する。接続3秒・全体5秒、応答64KiB上限、リダイレクトなし、TLS検証あり。OSの信頼済み証明書ストアを使用する。確認失敗を「最新版」とは扱わず、既知の更新通知は一時的な確認失敗で消さない。

ビルドはシェルを経由せず、固定のCargo引数で実行する。

```text
cargo install --git https://github.com/ELRdn/Open-Pixiv-Downloader.git --branch main --locked --force --bin opixiv --root <一時領域>
```

取得したTOMLやユーザーの保存先からコマンド文字列を作らない。一時領域のパスは一つの引数として渡し、空白・日本語・&等を含むパスにも対応する。

ビルドや生成物の検証が失敗した場合、本体は変更しない。同時更新はインストール先の `.opixiv-update.lock` により防ぐ。ロックファイルが終了後に残るのは正常で、削除は不要。

置換はWindows対応の `self-replace` を使用する。Windowsでは実行中の旧ファイルを別名へ移し、終了後に補助プロセスが片付ける。本体を移した後のエラーに備えて旧バイナリのコピーを取り、元パスがなくなった場合は復元を試みる。復元も失敗した場合はバックアップを残して場所を表示する。電源断等の瞬間によって一時ファイルが残る場合はある。

Cargoのインストール台帳は直接編集しない。実行中の版は `opixiv --version` を基準に確認する。更新できない権限のフォルダーや、OSがファイルの置換を拒否する場合はエラーを報告する。

## 検証

Rustの既存テストに、CLI構文、バージョン検証、固定引数とパスの分離、復元処理を追加した。ネイティブの模擬Cargoと使い捨ての実行ファイルを使った子プロセステストで、自己置換成功と、Cargo失敗・生成物なし・別製品・古い版・応答停止・過大出力の7ケースを確認する。保存データと失敗時の旧バイナリのハッシュも照合する。

```sh
python tools/check_update.py target/release/opixiv
```

Windowsでは `target/release/opixiv.exe`。Windows／LinuxのCIに追加したが、このクラウドでの実行確認はLinuxのみ。Windowsの実機・CI結果を確認済みとはしない。

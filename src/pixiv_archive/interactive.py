"""Dependency-free terminal workflow; calls the same engine as scripted commands."""
from pathlib import Path

from .archive import parse_target
from .net import ArchiveError
from .terminal import welcome, help_text


def ask(prompt, default=None):
    suffix = f" [{default}]" if default is not None else ""
    value = input(f"{prompt}{suffix} > ").strip()
    if value.lower() in ("q", "/quit", "/exit"):
        raise EOFError
    return value or default or ""


def folder(prompt, default):
    while True:
        value = Path(ask(prompt, default).strip('"')).expanduser()
        if value.exists() and not value.is_dir():
            print("そこにはファイルがあります。フォルダーを指定してください。")
        else:
            return str(value)


def collect_targets(initial=None):
    print("\n作品・作者・小説シリーズのURLを貼り付けてください。")
    print("複数行で貼り付けできます。最後に空行で確定します。qで終了。")
    targets, seen = [], set()
    while True:
        if initial is not None:
            line, initial = initial, None
        else:
            line = input("URL > ").strip()
        if line.lower() in ("q", "/quit", "/exit"):
            raise EOFError
        if not line:
            if targets:
                return targets
            print("URLを1つ以上入力してください。")
            continue
        for value in line.split():
            try:
                canonical = parse_target(value)
            except (ArchiveError, ValueError):
                print("入力を確認してください。Pixivの作品・作者・小説シリーズURLに対応しています。")
                continue
            if canonical not in seen:
                if len(targets) >= 1000:
                    print("1回に入力できる対象は1000件までです。")
                    continue
                targets.append(value)
                seen.add(canonical)
        print(f"  {len(targets)}件のURLを受付済み")


def parallelism(default="2"):
    while True:
        value = ask("同時に保存する作品数（1〜4）", default)
        if value in ("1", "2", "3", "4"):
            return value
        print("1〜4の数字を入力してください。")


def report(event, data):
    if event == "discovering":
        print("\n作品一覧を確認しています…")
    elif event == "selected":
        print(f"対象 {data['discovered']}作品 / 保存済み {data['already_complete']}作品 / 今回 {data['selected']}作品")
    elif event == "progress":
        labels = {"complete": "保存済み", "partial": "素材の欠損あり", "failed": "失敗",
                  "paused": "停止", "unavailable": "取得不可", "skipped": "対象外",
                  "already_complete": "保存済み"}
        print(f"  [{data['done']}/{data['total']}] {data['key']}  {labels.get(data['status'], data['status'])}")
    elif event == "error":
        print(f"\n処理できませんでした: {data}")
    elif event == "result":
        if "valid" in data:
            print(f"\n検査: {data['checked']}作品中 {data['valid']}作品で整合性が一致しました。")
            for problem in data.get("problems", []):
                print(f"  要確認: {problem['key']}")
        elif "zip" in data:
            print(f"\nZIPを保存しました: {data['zip']}（{data['complete_works']}作品）")
        else:
            labels = {"complete": "保存処理が完了しました", "partial": "一部が未完了です",
                      "paused": "処理を停止しました", "failed": "処理できませんでした",
                      "interrupted": "処理を中断しました"}
            print(f"\n{labels.get(data['status'], data['status'])}。")
            counts = data.get("counts", {})
            print(f"今回保存 {counts.get('complete', 0)}作品 / 保存済み {data.get('already_complete', 0)}作品")
            attention = sum(value for key, value in counts.items() if key not in ("complete", "already_complete"))
            pending = data.get("unattempted_selected", 0) + data.get("deferred_by_limit", 0)
            excluded = data.get("previous_exclusions", 0)
            print(f"要確認 {attention}作品 / 未着手 {pending}作品 / 前回の取得不可・対象外 {excluded}作品")
            if data.get("reason"):
                print(f"理由: {data['reason']}")
            print("保存済みの作品は次回も利用できます。詳細は保存先の last-run.json を確認してください。")


def run(execute):
    welcome()
    help_text()
    last_folder, last_jobs = "archive", "2"
    online_folder = "archive"
    try:
        while True:
            raw = ask("opixiv")
            initial = None
            if not raw:
                continue
            if raw in ("/help", "?"):
                help_text()
                continue
            aliases = {"/download": "1", "/fetch": "1", "/demo": "2", "/verify": "3",
                       "/pack": "4", "/quit": "0", "/exit": "0"}
            choice = aliases.get(raw, raw)
            if raw.startswith(("https://", "user:", "art:", "novel:", "series:")):
                choice, initial = "1", raw
            if choice == "0":
                print("終了しました。")
                return 0
            if choice not in ("1", "2", "3", "4"):
                print("URLまたは /help を入力してください。\n")
                continue
            if choice in ("1", "2"):
                demo = choice == "2"
                if demo:
                    fixture = Path(__file__).resolve().parents[2] / "examples/synthetic-fixture.json"
                    if not fixture.is_file():
                        print("同梱データが見つかりません。ソース一式を展開した場所で実行してください。\n")
                        continue
                    targets, transport = ["user:9001"], ["--fixture", str(fixture)]
                else:
                    print("実サイト取得は検証中です。閲覧条件によっては取得できません。")
                    targets, transport = collect_targets(initial), ["--online"]
                destination = folder("保存先", "demo-archive" if demo else online_folder)
                jobs = parallelism(last_jobs)
                last_folder, last_jobs = destination, jobs
                if not demo:
                    online_folder = destination
                args = ["fetch", *targets, *transport, "--all", "--jobs", jobs, "--out", destination]
                print(f"\n{'同梱の4作品' if demo else str(len(targets)) + '件のURL（作者URLは作品一覧へ展開）'}")
                print(f"保存先: {Path(destination).resolve()}\n同時保存: {jobs}作品\n")
            else:
                destination = folder("検査・出力する保存先", last_folder)
                args = ["verify", destination] if choice == "3" else ["pack", destination,
                    str(Path(ask("ZIPの保存先", "export.zip").strip('"')).expanduser())]
            while True:
                action = ask("Enterで開始 / bでメニューへ / qで終了")
                if action.lower() == "b":
                    break
                if not action:
                    execute(args, reporter=report)
                    break
                print("Enter、b、qのいずれかを入力してください。")
            print()
    except EOFError:
        print("\n終了しました。")
        return 0
    except KeyboardInterrupt:
        print("\n入力を中断しました。")
        return 130

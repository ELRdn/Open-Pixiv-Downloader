use crate::{
    archive::{safe_path, Archive},
    error::{invalid, Result},
    jobs::{self, Event, Options},
    target::Target,
    update,
};
use crossterm::{
    event::{
        self, DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture,
        Event as InputEvent, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton,
        MouseEvent, MouseEventKind,
    },
    execute,
};
use ratatui::{
    layout::{Alignment, Constraint, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, BorderType, Borders, Clear, Gauge, Paragraph, Row, Table, TableState, Wrap},
    Frame,
};
use serde_json::Value;
use std::{
    collections::{HashMap, HashSet, VecDeque},
    io,
    path::PathBuf,
    sync::{
        atomic::{AtomicBool, Ordering},
        mpsc, Arc,
    },
    thread,
    time::{Duration, Instant},
};
use unicode_width::UnicodeWidthStr;

const INK: Color = Color::Rgb(218, 232, 244);
const MUTED: Color = Color::Rgb(116, 143, 165);
const CYAN: Color = Color::Rgb(81, 225, 232);
const BLUE: Color = Color::Rgb(74, 117, 237);
const BG: Color = Color::Rgb(11, 17, 26);
const BALANCED_BUTTON: &str = "[ API・媒体を分離 ]";

fn glyph(c: char) -> [&'static str; 6] {
    match c {
        'O' => [
            " ██████╗ ",
            "██╔═══██╗",
            "██║   ██║",
            "██║   ██║",
            "╚██████╔╝",
            " ╚═════╝ ",
        ],
        'P' => [
            "██████╗ ",
            "██╔══██╗",
            "██████╔╝",
            "██╔═══╝ ",
            "██║     ",
            "╚═╝     ",
        ],
        'E' => [
            "███████╗",
            "██╔════╝",
            "█████╗  ",
            "██╔══╝  ",
            "███████╗",
            "╚══════╝",
        ],
        'N' => [
            "███╗   ██╗",
            "████╗  ██║",
            "██╔██╗ ██║",
            "██║╚██╗██║",
            "██║ ╚████║",
            "╚═╝  ╚═══╝",
        ],
        'I' => ["██╗", "██║", "██║", "██║", "██║", "╚═╝"],
        'X' => [
            "██╗  ██╗",
            "╚██╗██╔╝",
            " ╚███╔╝ ",
            " ██╔██╗ ",
            "██╔╝ ██╗",
            "╚═╝  ╚═╝",
        ],
        'V' => [
            "██╗   ██╗",
            "██║   ██║",
            "██║   ██║",
            "╚██╗ ██╔╝",
            " ╚████╔╝ ",
            "  ╚═══╝  ",
        ],
        _ => ["  "; 6],
    }
}
pub fn logo(word: &str) -> Vec<String> {
    (0..6)
        .map(|row| {
            word.chars()
                .map(|c| glyph(c)[row])
                .collect::<Vec<_>>()
                .join(" ")
        })
        .collect()
}

#[derive(Default, Clone)]
struct Editor {
    value: String,
    cursor: usize,
}
impl Editor {
    fn new(value: &str) -> Self {
        Self {
            value: value.into(),
            cursor: value.len(),
        }
    }
    fn insert(&mut self, text: &str, multiline: bool) {
        // Terminal controls are never echoed; paste remains data, never commands.
        let normalized = text.replace("\r\n", "\n").replace('\r', "\n");
        let clean: String = normalized
            .chars()
            .filter_map(|c| {
                if c == '\n' {
                    Some(if multiline { '\n' } else { ' ' })
                } else if c == '\t' {
                    Some(' ')
                } else if c.is_control() {
                    None
                } else {
                    Some(c)
                }
            })
            .collect();
        if self.value.len() + clean.len() > 2 * 1024 * 1024 {
            return;
        }
        self.value.insert_str(self.cursor, &clean);
        self.cursor += clean.len();
    }
    fn left(&mut self) {
        self.cursor = self.value[..self.cursor]
            .char_indices()
            .next_back()
            .map_or(0, |(i, _)| i);
    }
    fn right(&mut self) {
        if self.cursor < self.value.len() {
            self.cursor += self.value[self.cursor..].chars().next().unwrap().len_utf8();
        }
    }
    fn vertical(&mut self, down: bool) {
        let start = self.value[..self.cursor].rfind('\n').map_or(0, |i| i + 1);
        let column = UnicodeWidthStr::width(&self.value[start..self.cursor]);
        let range = if down {
            let Some(end) = self.value[self.cursor..]
                .find('\n')
                .map(|i| self.cursor + i + 1)
            else {
                return;
            };
            let next_end = self.value[end..]
                .find('\n')
                .map_or(self.value.len(), |i| end + i);
            end..next_end
        } else {
            if start == 0 {
                return;
            }
            let previous = self.value[..start - 1].rfind('\n').map_or(0, |i| i + 1);
            previous..start - 1
        };
        let line = &self.value[range.clone()];
        let mut width = 0;
        let mut offset = 0;
        for (i, c) in line.char_indices() {
            let w = UnicodeWidthStr::width(c.to_string().as_str());
            if width + w > column {
                break;
            }
            width += w;
            offset = i + c.len_utf8();
        }
        self.cursor = range.start + offset;
    }
    fn key(&mut self, key: KeyEvent, multiline: bool) {
        match key.code {
            KeyCode::Left => self.left(),
            KeyCode::Right => self.right(),
            KeyCode::Up if multiline => self.vertical(false),
            KeyCode::Down if multiline => self.vertical(true),
            KeyCode::Home => {
                self.cursor = self.value[..self.cursor].rfind('\n').map_or(0, |i| i + 1)
            }
            KeyCode::End => {
                self.cursor += self.value[self.cursor..]
                    .find('\n')
                    .unwrap_or(self.value.len() - self.cursor)
            }
            KeyCode::Backspace => {
                if self.cursor > 0 {
                    let end = self.cursor;
                    self.left();
                    self.value.drain(self.cursor..end);
                }
            }
            KeyCode::Delete => {
                if self.cursor < self.value.len() {
                    let start = self.cursor;
                    self.right();
                    self.value.drain(start..self.cursor);
                    self.cursor = start;
                }
            }
            KeyCode::Enter if multiline => self.insert("\n", true),
            KeyCode::Char(c)
                if !key
                    .modifiers
                    .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
            {
                self.insert(&c.to_string(), multiline)
            }
            _ => {}
        }
    }
    fn visible(&self, width: usize, height: usize) -> (String, u16, u16) {
        // Soft-wrap by terminal cells so Japanese paths and long URLs remain editable.
        let width = width.max(1);
        let height = height.max(1);
        let mut lines = vec![String::new()];
        let mut x = 0;
        let mut cx = 0;
        let mut cy = 0;
        for (i, c) in self.value.char_indices() {
            let w = UnicodeWidthStr::width(c.to_string().as_str());
            if c != '\n' && x + w > width {
                lines.push(String::new());
                x = 0;
            }
            if i == self.cursor {
                cx = x;
                cy = lines.len() - 1;
            }
            if c == '\n' {
                lines.push(String::new());
                x = 0;
            } else {
                lines.last_mut().unwrap().push(c);
                x += w;
            }
        }
        if self.cursor == self.value.len() {
            if x >= width {
                lines.push(String::new());
                x = 0;
            }
            cx = x;
            cy = lines.len() - 1;
        }
        let top = cy.saturating_sub(height - 1);
        (
            lines
                .iter()
                .skip(top)
                .take(height)
                .cloned()
                .collect::<Vec<_>>()
                .join("\n"),
            cx.min(width - 1) as u16,
            (cy - top) as u16,
        )
    }
}
#[derive(PartialEq)]
enum Modal {
    None,
    Help,
    Confirm,
    Result,
    Update,
    Collection,
    Settings,
}
struct Worker {
    receiver: mpsc::Receiver<Event>,
    handle: thread::JoinHandle<()>,
    stop: Arc<AtomicBool>,
}
struct WorkRow {
    key: String,
    status: String,
    output: Option<PathBuf>,
}
impl WorkRow {
    fn new(key: String, status: String, root: Option<&std::path::Path>) -> Self {
        let output = root.and_then(|root| {
            let (kind, id) = key.split_once('_')?;
            if !matches!(kind, "art" | "novel") {
                return None;
            }
            Target::new(kind, id).ok()?;
            let path = safe_path(root, &format!("works/{key}")).ok()?;
            path.is_dir().then_some(path)
        });
        Self {
            key,
            status,
            output,
        }
    }
}
struct App {
    urls: Editor,
    out: Editor,
    other_urls: Editor,
    other_out: Editor,
    jobs: usize,
    request_budget: usize,
    active_request_budget: usize,
    balanced: bool,
    settings_budget: Editor,
    settings_balanced: bool,
    settings_focus: usize,
    settings_error: Option<String>,
    confirm_resume: bool,
    dirty: bool,
    focus: usize,
    demo: bool,
    modal: Modal,
    modal_scroll: u16,
    worker: Option<Worker>,
    rows: Vec<WorkRow>,
    row_index: HashMap<String, usize>,
    body_keys: HashSet<String>,
    body_ready: usize,
    complete: usize,
    requests: usize,
    metrics: Value,
    run_out: Option<PathBuf>,
    selected: usize,
    logs: VecDeque<String>,
    done: usize,
    total: usize,
    status: String,
    last: Option<Value>,
    update_status: update::Status,
    update_receiver: Option<mpsc::Receiver<std::result::Result<semver::Version, String>>>,
    last_update_check: Option<Instant>,
    automatic_updates: bool,
    show_update_command: bool,
}
impl Default for App {
    fn default() -> Self {
        Self {
            urls: Editor::default(),
            out: Editor::new("archive"),
            other_urls: Editor::new("user:9001"),
            other_out: Editor::new("demo-archive"),
            jobs: 2,
            request_budget: 20_000,
            active_request_budget: 20_000,
            balanced: false,
            settings_budget: Editor::new("20000"),
            settings_balanced: false,
            settings_focus: 0,
            settings_error: None,
            confirm_resume: false,
            dirty: true,
            focus: 0,
            demo: false,
            modal: Modal::None,
            modal_scroll: 0,
            worker: None,
            rows: vec![],
            row_index: HashMap::new(),
            body_keys: HashSet::new(),
            body_ready: 0,
            complete: 0,
            requests: 0,
            metrics: Value::Null,
            run_out: None,
            selected: 0,
            logs: VecDeque::from([
                "Welcome to Open Pixiv Downloader!".into(),
                "作品・作者・小説シリーズのURLを貼り付けてください。".into(),
            ]),
            done: 0,
            total: 0,
            status: "READY".into(),
            last: None,
            update_status: update::Status::Unchecked,
            update_receiver: None,
            last_update_check: None,
            automatic_updates: false,
            show_update_command: false,
        }
    }
}
impl App {
    fn open_collection(&mut self, kind: &str) {
        if self.out.value.trim().is_empty() {
            self.log("保存先を入力してください。".into());
            return;
        }
        let root = std::path::Path::new(self.out.value.trim());
        let result = safe_path(root, &format!("works/{kind}"))
            .and_then(|p| crate::folder::open(&p).map(|()| p));
        match result {
            Ok(path) => self.log(format!("フォルダーを開きました: {}", path.display())),
            Err(e) => self.log(e.to_string()),
        }
    }
    fn collection_details(&self) -> String {
        if self.out.value.trim().is_empty() {
            return "保存先を入力してください。Escで戻れます。".into();
        }
        let root = std::env::current_dir()
            .unwrap_or_default()
            .join(self.out.value.trim());
        format!("{}\nI: 画像を開く / N: 小説を開く / Esc: 戻る\n↑↓: スクロール\n\n画像: {}\n小説: {}\n\n新しく保存した作品は自動で集約します。\n画像は作品ID＋ページ番号、小説は作品ID.txtで保存します。\nコピーなので、集約先の編集は作品別の元ファイルに影響しません。",if self.worker.is_some(){"保存処理中です。終了後にEnterで既存分を集約できます。"}else{"Enter: 既存の保存済み作品を集約・補完（通信なし）"},root.join("works/image").display(),root.join("works/novel").display())
    }
    fn open_settings(&mut self) {
        if self.worker.is_some() {
            self.log("保存中の設定は変更できません。終了後にF10で変更できます。".into());
            return;
        }
        self.settings_budget = Editor::new(&self.request_budget.to_string());
        self.settings_balanced = self.balanced;
        self.settings_focus = 0;
        self.settings_error = None;
        self.modal = Modal::Settings;
        self.dirty = true;
    }
    fn apply_settings(&mut self) {
        match self.settings_budget.value.parse::<usize>() {
            Ok(value) if (1..=100_000).contains(&value) => {
                self.request_budget = value;
                if self.worker.is_none() && self.last.is_none() {
                    self.active_request_budget = value;
                }
                self.balanced = self.settings_balanced;
                self.modal = Modal::None;
                self.log(format!(
                    "設定: {} / 要求予算 {}回",
                    profile_label(self.balanced),
                    self.request_budget
                ));
            }
            _ => self.settings_error = Some("要求予算は1〜100000の整数で入力してください。".into()),
        }
    }
    fn settings_key(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Esc => self.modal = Modal::None,
            KeyCode::Tab | KeyCode::BackTab => self.settings_focus = 1 - self.settings_focus,
            KeyCode::F(n @ 1..=3) => {
                self.settings_budget = Editor::new(["500", "5000", "20000"][(n - 1) as usize]);
                self.settings_focus = 1;
                self.settings_error = None;
            }
            KeyCode::Enter => self.apply_settings(),
            KeyCode::Left | KeyCode::Right | KeyCode::Char(' ') if self.settings_focus == 0 => {
                self.settings_balanced = !self.settings_balanced;
            }
            KeyCode::Char('a')
                if self.settings_focus == 1 && key.modifiers.contains(KeyModifiers::CONTROL) =>
            {
                self.settings_budget = Editor::default();
            }
            KeyCode::Char(c)
                if self.settings_focus == 1
                    && c.is_ascii_digit()
                    && !key
                        .modifiers
                        .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
            {
                if self.settings_budget.value.len() < 6 {
                    self.settings_budget.key(key, false);
                    self.settings_error = None;
                }
            }
            KeyCode::Backspace
            | KeyCode::Delete
            | KeyCode::Left
            | KeyCode::Right
            | KeyCode::Home
            | KeyCode::End
                if self.settings_focus == 1 =>
            {
                self.settings_budget.key(key, false);
                self.settings_error = None;
            }
            _ => {}
        }
    }
    fn check_update(&mut self) {
        if self.update_receiver.is_some() {
            return;
        }
        if self
            .last_update_check
            .is_some_and(|t| t.elapsed() < update::RETRY_INTERVAL)
        {
            self.log("更新の再確認は前回から1分後にできます。".into());
            return;
        }
        self.dirty = true;
        self.last_update_check = Some(Instant::now());
        if !matches!(self.update_status, update::Status::Available(_)) {
            self.update_status = update::Status::Checking;
        }
        self.update_receiver = Some(update::background());
    }
    fn poll_update(&mut self) {
        let message = self
            .update_receiver
            .as_ref()
            .and_then(|rx| match rx.try_recv() {
                Ok(v) => Some(v),
                Err(mpsc::TryRecvError::Empty) => None,
                Err(mpsc::TryRecvError::Disconnected) => Some(Err(
                    "更新確認の処理が終了しました。F8で再確認できます。".into(),
                )),
            });
        if let Some(result) = message {
            self.dirty = true;
            self.update_receiver = None;
            match result {
                Ok(version) => {
                    let next = update::Status::from_version(version);
                    if matches!(next, update::Status::Available(_)) && self.update_status != next {
                        self.log(format!("{} · F9で更新方法", next.message()));
                    }
                    self.update_status = next;
                }
                Err(error) => {
                    if !matches!(self.update_status, update::Status::Available(_)) {
                        self.update_status = update::Status::Failed(error);
                    }
                }
            }
        }
        if self.automatic_updates
            && self.worker.is_none()
            && self.update_receiver.is_none()
            && self
                .last_update_check
                .is_some_and(|t| t.elapsed() >= update::CHECK_INTERVAL)
        {
            self.check_update();
        }
    }
    fn update_details(&self) -> String {
        let latest = match &self.update_status {
            update::Status::Available(v) | update::Status::Current(v) => {
                format!("GitHubのバージョン: v{v}")
            }
            _ => self.update_status.message(),
        };
        let error = if let update::Status::Failed(s) = &self.update_status {
            format!("\n{s}\n")
        } else {
            String::new()
        };
        format!("{}\n\n現在: v{}\n{latest}\n{error}\n更新は、ツールを終了してPowerShellで opixiv update を実行します。\n保存済みの作品は引き続き利用できます。\nリポジトリへの移動やgit pullは不要です。\n\n{}\n\nEsc: この画面を閉じる",self.update_status.message(),env!("CARGO_PKG_VERSION"),if self.worker.is_some(){"保存処理が動いています。終了後にEnterで更新コマンドを表示できます。"}else{"Enter: ツールを終了し、コピー用の更新コマンドを表示"})
    }
    fn set_row(&mut self, key: String, status: String) {
        self.dirty = true;
        let is_complete = |status: &str| matches!(status, "complete" | "already_complete");
        if key.starts_with("novel_")
            && (status == "body_ready" || is_complete(&status))
            && self.body_keys.insert(key.clone())
        {
            self.body_ready += 1;
        }
        let row = WorkRow::new(key.clone(), status, self.run_out.as_deref());
        if let Some(&index) = self.row_index.get(&key) {
            self.complete = self
                .complete
                .saturating_sub(usize::from(is_complete(&self.rows[index].status)))
                + usize::from(is_complete(&row.status));
            self.rows[index] = row;
        } else {
            self.complete += usize::from(is_complete(&row.status));
            self.row_index.insert(key, self.rows.len());
            self.rows.push(row);
        }
    }
    fn clear_rows(&mut self) {
        self.rows.clear();
        self.row_index.clear();
        self.body_keys.clear();
        self.body_ready = 0;
        self.complete = 0;
    }
    fn progress_label(&self) -> String {
        format!(
            "本文 {} / 全素材完了 {} / 要求 {}/{}",
            self.body_ready, self.complete, self.requests, self.active_request_budget
        )
    }
    fn telemetry_label(&self) -> String {
        let transport = &self.metrics["transport"];
        if let (Some(api), Some(media)) = (
            transport["api"]["requests_per_second"].as_f64(),
            transport["media"]["requests_per_second"].as_f64(),
        ) {
            format!(
                "ACTIVITY · API {api:.2}/s 媒体 {media:.2}/s · 共有 {}",
                transport["shared_media"]["hits"].as_u64().unwrap_or(0)
            )
        } else {
            "ACTIVITY".into()
        }
    }
    fn folder(&self, selected: bool) -> Result<PathBuf> {
        if selected {
            self.rows
                .get(self.selected)
                .and_then(|r| r.output.clone())
                .ok_or_else(|| invalid("選択した作品の保存フォルダーはまだありません。"))
        } else {
            let value = self.out.value.trim();
            if value.is_empty() {
                return Err(invalid("保存先を入力してください。"));
            }
            Ok(PathBuf::from(value))
        }
    }
    fn open_folder(&mut self, selected: bool) {
        match self
            .folder(selected)
            .and_then(|p| crate::folder::open(&p).map(|()| p))
        {
            Ok(path) => self.log(format!("フォルダーを開きました: {}", path.display())),
            Err(e) => self.log(e.to_string()),
        }
    }
    fn summary(&self) -> String {
        let Some(v) = &self.last else {
            return "まだ結果はありません。".into();
        };
        let mut lines = vec![format!("結果: {}", self.status), "".into()];
        if v.get("selected").is_some() {
            lines.push(format!(
                "対象 {} / 保存済み {} / 未処理 {}",
                v["selected"], v["already_complete"], v["unattempted_selected"]
            ));
            lines.push(format!(
                "過去の除外 {} / 上限で保留 {}",
                v["previous_exclusions"], v["deferred_by_limit"]
            ));
        }
        if let Some(elapsed) = v["elapsed_seconds"].as_f64() {
            lines.push(format!(
                "所要 {elapsed:.1}秒 / 送信間隔の待機 {:.1}秒 / {}リクエスト",
                v["rate_wait_seconds"].as_f64().unwrap_or(0.0),
                v["requests"]
            ));
        }
        if v.get("body_ready_count").is_some() {
            lines.push(format!(
                "本文利用可能 {} / 全素材完了 {}",
                v["body_ready_count"], v["full_complete_count"]
            ));
        }
        if let Some(budget) = v["request_budget"].as_u64() {
            lines.push(format!("要求使用 {} / 上限 {budget}", v["requests"]));
        }
        if let Some(reason) = v.get("stop_reason").filter(|reason| !reason.is_null()) {
            lines.push(format!("停止理由: {}", stop_label(reason)));
            lines.push(
                "F11で最新の保存対象一覧から再開できます。F10で次回の予算を設定できます。".into(),
            );
        }
        if let Some(reason) = v["reason"].as_str() {
            lines.push(reason.into());
        }
        if let Some(transport) = v.get("transport_metrics") {
            lines.extend(transport_summary(transport));
        }
        if let Some(stages) = v["stage_seconds"].as_object() {
            for (name, value) in stages {
                if let Some(seconds) = value.as_f64() {
                    lines.push(format!("段階 {name}: {seconds:.2}秒（並行処理の合計）"));
                }
            }
        }
        if let Some(counts) = v["counts"].as_object() {
            for (status, count) in counts {
                lines.push(format!("{status}: {count}"));
            }
        }
        if v.get("valid").is_some() {
            lines.push(format!("検査: {} / {} 正常", v["valid"], v["checked"]));
        }
        if v.get("zip").is_some() {
            lines.push(format!(
                "出力: {} / {}作品",
                v["zip"].as_str().unwrap_or(""),
                v["complete_works"]
            ));
        }
        if let Some(results) = v["results"].as_array() {
            for item in results {
                lines.push(format!(
                    "\n{} · {}",
                    item["key"].as_str().unwrap_or(""),
                    item["status"].as_str().unwrap_or("")
                ));
                if let Some(reason) = item["reason"].as_str() {
                    lines.push(reason.into());
                }
                if let Some(omissions) = item["omissions"].as_array() {
                    for o in omissions {
                        lines.push(format!(
                            "未保存: {} — {}",
                            o["resource"].as_str().unwrap_or(""),
                            o["reason"].as_str().unwrap_or("")
                        ));
                    }
                }
            }
        }
        if let Some(problems) = v["problems"].as_array() {
            for p in problems {
                lines.push(format!("\n欠損: {}", p["key"].as_str().unwrap_or("")));
                if let Some(errors) = p["errors"].as_array() {
                    for e in errors {
                        lines.push(e.as_str().unwrap_or("").into());
                    }
                }
            }
        }
        if let Some(targets) = v["selected_targets"].as_array() {
            for t in targets {
                lines.push(t.as_str().unwrap_or("").into());
            }
        }
        for row in &self.rows {
            if let Some(path) = &row.output {
                lines.push(format!("\n{} の保存先: {}", row.key, path.display()));
            }
        }
        if let Some(c) = v.get("collection") {
            lines.push(format!(
                "\nファイル集約: コピー {} / 集約済み {} / 問題 {}",
                c["copied"],
                c["existing"],
                c["problems"].as_array().map_or(0, Vec::len)
            ));
            for field in ["image_directory", "novel_directory"] {
                if let Some(path) = c[field].as_str() {
                    lines.push(path.into());
                }
            }
            if let Some(problems) = c["problems"].as_array() {
                for p in problems {
                    lines.push(format!(
                        "{}: {}",
                        p["path"].as_str().unwrap_or(""),
                        p["reason"].as_str().unwrap_or("")
                    ));
                }
            }
        }
        clean_lines(&lines.join("\n"))
    }
    fn log(&mut self, message: String) {
        self.dirty = true;
        self.logs.push_back(clean(&message));
        while self.logs.len() > 100 {
            self.logs.pop_front();
        }
    }
    fn options(&self) -> Result<Options> {
        self.options_for(false)
    }
    fn options_for(&self, resume: bool) -> Result<Options> {
        let values: Vec<_> = self
            .urls
            .value
            .split_whitespace()
            .filter(|v| !v.is_empty())
            .map(str::to_owned)
            .collect();
        if !resume && (values.is_empty() || values.len() > 1000) {
            return Err(invalid("作品・作者URLを1〜1000件入力してください。"));
        }
        if !resume {
            for v in &values {
                Target::parse(v)?;
            }
        }
        if self.out.value.trim().is_empty() {
            return Err(invalid("保存先を入力してください。"));
        }
        Ok(Options {
            targets: if resume { vec![] } else { values },
            balanced: self.balanced,
            interval: if self.balanced { 1.0 } else { 2.0 },
            request_budget: self.request_budget,
            resume,
            out: PathBuf::from(self.out.value.trim()),
            jobs: self.jobs,
            all: true,
            demo: self.demo,
            online: !self.demo,
            ..Default::default()
        })
    }
    fn start(&mut self, operation: &str) {
        if self.worker.is_some() {
            return;
        }
        let opts = if matches!(operation, "fetch" | "plan" | "resume") {
            match self.options_for(operation == "resume") {
                Ok(o) => o,
                Err(e) => {
                    self.log(e.to_string());
                    return;
                }
            }
        } else {
            if self.out.value.trim().is_empty() {
                self.log("保存先を入力してください。".into());
                return;
            }
            Options {
                out: PathBuf::from(self.out.value.trim()),
                ..Default::default()
            }
        };
        let (tx, rx) = mpsc::channel();
        let stop = Arc::new(AtomicBool::new(false));
        let worker_stop = stop.clone();
        let op = operation.to_owned();
        if matches!(operation, "fetch" | "plan" | "resume") {
            self.clear_rows();
            self.requests = 0;
            self.active_request_budget = opts.request_budget;
            self.metrics = Value::Null;
            self.run_out = None;
            self.selected = 0;
        }
        self.last = None;
        self.done = 0;
        self.total = 0;
        self.status = operation.to_uppercase();
        self.log(format!(
            "{} / {}",
            self.status,
            if self.demo {
                "DEMO · 通信なし"
            } else {
                "LOCAL ARCHIVE"
            }
        ));
        let handle = thread::spawn(move || {
            let result = match op.as_str() {
                "fetch" | "plan" | "resume" => jobs::run(opts, op == "plan", worker_stop, |e| {
                    let _ = tx.send(e);
                }),
                "verify" => Archive::open(&opts.out, true).and_then(|a| a.verify()),
                "collect" => Archive::open(&opts.out, true)
                    .and_then(|a| a.collect(&|| worker_stop.load(Ordering::SeqCst))),
                "pack" => {
                    let mut dest = opts.out.as_os_str().to_owned();
                    dest.push("-export.zip");
                    Archive::open(&opts.out, true).and_then(|a| a.pack(&PathBuf::from(dest)))
                }
                _ => Err(invalid("Unknown operation")),
            };
            let _ = tx.send(match result {
                Ok(v) => Event::Finished(v),
                Err(e) => Event::Failed(e.to_string()),
            });
        });
        self.worker = Some(Worker {
            receiver: rx,
            handle,
            stop,
        });
    }
    fn poll(&mut self) {
        self.poll_update();
        let mut events: Vec<_> = self
            .worker
            .as_ref()
            .map(|w| w.receiver.try_iter().collect())
            .unwrap_or_default();
        let worker_done = self.worker.as_ref().is_some_and(|w| w.handle.is_finished());
        if worker_done {
            if let Some(w) = &self.worker {
                events.extend(w.receiver.try_iter());
            }
        }
        let mut finished = false;
        for e in events {
            self.dirty = true;
            match e {
                Event::ArchiveOpened(root) => self.run_out = Some(root),
                Event::Existing { key, status } => self.set_row(key, status),
                Event::Message(s) => self.log(s),
                Event::Selected {
                    total,
                    already,
                    deferred,
                    excluded,
                } => {
                    self.total = total;
                    self.log(format!("対象 {total} / 保存済み {already} / 保留 {deferred} / 過去の除外 {excluded}"));
                }
                Event::Started(key) => self.set_row(key, "saving".into()),
                Event::BodyReady { key } => self.set_row(key, "body_ready".into()),
                Event::Phase { key, status } => self.set_row(key, status),
                Event::Metrics(value) => {
                    if let Some(requests) = value["requests"].as_u64() {
                        self.requests = requests as usize;
                    }
                    if let Some(budget) = value["request_budget"].as_u64() {
                        self.active_request_budget = budget as usize;
                    }
                    if let Some(body_ready) = value["body_ready"].as_u64() {
                        self.body_ready = body_ready as usize;
                    }
                    if let Some(complete) = value["complete"].as_u64() {
                        self.complete = complete as usize;
                    }
                    self.metrics = value;
                }
                Event::Progress {
                    key,
                    status,
                    done,
                    total,
                } => {
                    self.done = done;
                    self.total = total;
                    self.set_row(key.clone(), status.clone());
                    self.log(format!("{key} · {status}"));
                }
                Event::Finished(value) => {
                    if let Some(requests) = value["requests"].as_u64() {
                        self.requests = requests as usize;
                    }
                    if let Some(budget) = value["request_budget"].as_u64() {
                        self.active_request_budget = budget as usize;
                    }
                    if let Some(transport) = value.get("transport_metrics") {
                        self.metrics = serde_json::json!({"transport":transport});
                    }
                    if let Some(body_ready) = value["body_ready_count"].as_u64() {
                        self.body_ready = body_ready as usize;
                    }
                    if let Some(complete) = value["full_complete_count"].as_u64() {
                        self.complete = complete as usize;
                    }
                    if let Some(reason) =
                        value.get("stop_reason").filter(|reason| !reason.is_null())
                    {
                        self.log(format!(
                            "停止理由: {} · F11で最新の保存対象から再開",
                            stop_label(reason)
                        ));
                    }
                    self.status = value["status"]
                        .as_str()
                        .unwrap_or(
                            if value["problems"].as_array().is_some_and(|v| !v.is_empty()) {
                                "problems"
                            } else {
                                "complete"
                            },
                        )
                        .to_uppercase();
                    if let Some(reason) = value["reason"].as_str() {
                        self.log(reason.into());
                    }
                    self.log(format!("結果: {}", self.status));
                    if let Some(targets) = value["selected_targets"].as_array() {
                        for t in targets {
                            if let Some(target) = t.as_str().and_then(|s| Target::parse(s).ok()) {
                                self.set_row(target.key(), "planned".into());
                            }
                        }
                    }
                    if value.get("valid").is_some() {
                        self.log(format!(
                            "検査: {} / {} 正常",
                            value["valid"], value["checked"]
                        ));
                    }
                    if let Some(problems) = value["problems"].as_array() {
                        for p in problems {
                            self.log(format!("欠損: {}", p));
                        }
                    }
                    if value.get("complete_works").is_some() {
                        self.log(format!(
                            "ZIP: {} / {}作品",
                            value["zip"], value["complete_works"]
                        ));
                    }
                    if value.get("unattempted_selected").is_some() {
                        self.log(format!(
                            "未処理: {} / 除外: {} / 上限保留: {}",
                            value["unattempted_selected"],
                            value["previous_exclusions"],
                            value["deferred_by_limit"]
                        ));
                    }
                    if let Some(elapsed) = value["elapsed_seconds"].as_f64() {
                        self.log(format!(
                            "所要 {elapsed:.1}秒 / 間隔待ち {:.1}秒 / {}回",
                            value["rate_wait_seconds"].as_f64().unwrap_or(0.0),
                            value["requests"]
                        ));
                    }
                    if let Some(c) = value.get("collection") {
                        self.log(format!(
                            "集約: コピー {} / 集約済み {} / 問題 {} · F4で開く",
                            c["copied"],
                            c["existing"],
                            c["problems"].as_array().map_or(0, Vec::len)
                        ));
                    }
                    self.last = Some(value);
                    finished = true;
                }
                Event::Failed(s) => {
                    self.status = "FAILED".into();
                    self.log(if s.starts_with("Saved selection is incomplete") {
                        "最新の対象一覧の取得が未完了です。元のURLを確認して再実行してください。以前の別の一覧は再開しません。".into()
                    } else { s });
                    finished = true;
                }
            }
        }
        if finished {
            if let Some(w) = self.worker.take() {
                let _ = w.handle.join();
            }
        } else if worker_done {
            self.status = "FAILED".into();
            self.log("ワーカーが予期せず終了しました。".into());
            if let Some(w) = self.worker.take() {
                let _ = w.handle.join();
            }
        }
    }
    fn cancel(&mut self) {
        if let Some(w) = &self.worker {
            w.stop.store(true, Ordering::SeqCst);
            self.status = "STOPPING".into();
            self.log("停止中… 保存済みデータを保持して処理の終了を待ちます。".into());
        }
    }
    fn key(&mut self, key: KeyEvent) -> bool {
        if key.kind == KeyEventKind::Release {
            return false;
        }
        self.dirty = true;
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
            if self.worker.is_some() {
                self.cancel();
                return false;
            }
            return true;
        }
        if self.modal == Modal::Settings {
            self.settings_key(key);
            return false;
        }
        if self.modal != Modal::None {
            match key.code {
                KeyCode::Esc => self.modal = Modal::None,
                KeyCode::Down | KeyCode::PageDown
                    if matches!(self.modal, Modal::Result | Modal::Collection) =>
                {
                    self.modal_scroll = self
                        .modal_scroll
                        .saturating_add(if key.code == KeyCode::Down { 1 } else { 5 })
                }
                KeyCode::Up | KeyCode::PageUp
                    if matches!(self.modal, Modal::Result | Modal::Collection) =>
                {
                    self.modal_scroll = self
                        .modal_scroll
                        .saturating_sub(if key.code == KeyCode::Up { 1 } else { 5 })
                }
                KeyCode::Enter if self.modal == Modal::Confirm => {
                    self.modal = Modal::None;
                    self.start(if self.confirm_resume {
                        "resume"
                    } else {
                        "fetch"
                    });
                }
                KeyCode::Enter if self.modal == Modal::Collection && self.worker.is_none() => {
                    self.modal = Modal::None;
                    self.start("collect");
                }
                KeyCode::Char('i' | 'I') if self.modal == Modal::Collection => {
                    self.modal = Modal::None;
                    self.open_collection("image")
                }
                KeyCode::Char('n' | 'N') if self.modal == Modal::Collection => {
                    self.modal = Modal::None;
                    self.open_collection("novel")
                }
                KeyCode::Enter if self.modal == Modal::Update && self.worker.is_none() => {
                    self.show_update_command = true;
                    return true;
                }
                _ => {}
            }
            return false;
        }
        if key.code == KeyCode::F(10) {
            self.open_settings();
            return false;
        }
        if (key.code == KeyCode::F(11)
            || (key.code == KeyCode::Char('r') && key.modifiers.contains(KeyModifiers::CONTROL)))
            && self.worker.is_none()
        {
            match self.options_for(true) {
                Ok(_) => {
                    self.confirm_resume = true;
                    self.modal = Modal::Confirm;
                }
                Err(e) => self.log(e.to_string()),
            }
            return false;
        }
        if key.code == KeyCode::F(1) {
            self.modal = Modal::Help;
            return false;
        }
        if key.code == KeyCode::F(3) {
            self.modal = Modal::Result;
            self.modal_scroll = 0;
            return false;
        }
        if key.code == KeyCode::F(4) {
            self.modal = Modal::Collection;
            self.modal_scroll = 0;
            return false;
        }
        if key.modifiers.contains(KeyModifiers::ALT) {
            match key.code {
                KeyCode::Char('i') => {
                    self.open_collection("image");
                    return false;
                }
                KeyCode::Char('n') => {
                    self.open_collection("novel");
                    return false;
                }
                _ => {}
            }
        }
        if key.code == KeyCode::F(8) {
            self.check_update();
            return false;
        }
        if key.code == KeyCode::F(9) {
            self.modal = Modal::Update;
            return false;
        }
        if key.code == KeyCode::F(6) {
            self.open_folder(false);
            return false;
        }
        if key.code == KeyCode::F(7) {
            self.open_folder(true);
            return false;
        }
        if key.code == KeyCode::Esc {
            if self.worker.is_some() {
                self.cancel();
                return false;
            }
            return true;
        }
        if key.code == KeyCode::PageDown {
            self.selected = self
                .selected
                .saturating_add(5)
                .min(self.rows.len().saturating_sub(1));
            return false;
        }
        if key.code == KeyCode::PageUp {
            self.selected = self.selected.saturating_sub(5);
            return false;
        }
        if self.focus == 3 {
            match key.code {
                KeyCode::Up => {
                    self.selected = self.selected.saturating_sub(1);
                    return false;
                }
                KeyCode::Down => {
                    self.selected = (self.selected + 1).min(self.rows.len().saturating_sub(1));
                    return false;
                }
                KeyCode::Enter => {
                    self.open_folder(true);
                    return false;
                }
                _ => {}
            }
        }
        if key.code == KeyCode::Tab {
            self.focus = (self.focus + 1) % 4;
            return false;
        }
        if key.code == KeyCode::BackTab {
            self.focus = (self.focus + 3) % 4;
            return false;
        }
        if self.worker.is_some() {
            return false;
        }
        match key.code {
            KeyCode::F(2) => {
                self.demo = !self.demo;
                std::mem::swap(&mut self.out, &mut self.other_out);
                std::mem::swap(&mut self.urls, &mut self.other_urls);
                self.log(
                    if self.demo {
                        "デモ: 合成4作品・通信なし"
                    } else {
                        "オンライン: 匿名取得・実サイト未検証"
                    }
                    .into(),
                );
            }
            KeyCode::F(5) => self.start("plan"),
            KeyCode::Char('s') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                match self.options() {
                    Ok(_) => {
                        self.confirm_resume = false;
                        self.modal = Modal::Confirm;
                    }
                    Err(e) => self.log(e.to_string()),
                }
            }
            KeyCode::Char('v') if key.modifiers.contains(KeyModifiers::ALT) => self.start("verify"),
            KeyCode::Char('e') if key.modifiers.contains(KeyModifiers::ALT) => self.start("pack"),
            KeyCode::Char('l') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.logs.clear();
                self.clear_rows();
            }
            _ => match self.focus {
                0 => self.urls.key(key, true),
                1 => self.out.key(key, false),
                2 => match key.code {
                    KeyCode::Left | KeyCode::Char('-') => {
                        self.jobs = self.jobs.saturating_sub(1).max(1)
                    }
                    KeyCode::Right | KeyCode::Char('+') => self.jobs = (self.jobs + 1).min(4),
                    KeyCode::Char(c) if ('1'..='4').contains(&c) => {
                        self.jobs = c.to_digit(10).unwrap() as usize
                    }
                    _ => {}
                },
                _ => {}
            },
        }
        false
    }
    fn mouse(&mut self, event: MouseEvent, area: Rect) {
        self.dirty = true;
        if self.modal == Modal::Settings {
            if event.kind == MouseEventKind::Down(MouseButton::Left) {
                if let Some(view) = View::new(area) {
                    let popup = popup_rect(view.body, &self.modal);
                    let point = ratatui::layout::Position::new(event.column, event.row);
                    let (legacy, balanced, budget, presets, apply) = settings_controls(popup);
                    if legacy.contains(point) {
                        self.settings_balanced = false;
                        self.settings_focus = 0;
                    }
                    if balanced.contains(point) {
                        self.settings_balanced = true;
                        self.settings_focus = 0;
                    }
                    if budget.contains(point) {
                        self.settings_focus = 1;
                    }
                    for (i, rect) in presets.iter().enumerate() {
                        if rect.contains(point) {
                            self.settings_budget = Editor::new(["500", "5000", "20000"][i]);
                            self.settings_focus = 1;
                            self.settings_error = None;
                        }
                    }
                    if apply.contains(point) {
                        self.apply_settings();
                    }
                }
            }
            return;
        }
        if self.modal != Modal::None {
            return;
        }
        let Some(view) = View::new(area) else {
            return;
        };
        let point = ratatui::layout::Position::new(event.column, event.row);
        if event.kind == MouseEventKind::Down(MouseButton::Left) {
            if let Some((_, check, details)) = update_controls(view.header) {
                if check.contains(point) {
                    self.check_update();
                    return;
                }
                if details.contains(point) {
                    self.modal = Modal::Update;
                    return;
                }
            }
            if let Some(settings) = settings_button(view.footer) {
                if settings.contains(point) {
                    self.open_settings();
                    return;
                }
            }
            if let Some(resume) = resume_button(view.footer) {
                if resume.contains(point) && self.worker.is_none() {
                    let _ = self.key(KeyEvent::new(KeyCode::F(11), KeyModifiers::NONE));
                    return;
                }
            }
            let (root, work) = buttons(view.footer);
            if root.contains(point) {
                self.open_folder(false);
                return;
            }
            if work.contains(point) {
                self.open_folder(true);
                return;
            }
            if let Some((image, novel)) = collection_buttons(view.footer) {
                if image.contains(point) {
                    self.open_collection("image");
                    return;
                }
                if novel.contains(point) {
                    self.open_collection("novel");
                    return;
                }
            }
            let rows = view.queue.inner(ratatui::layout::Margin::new(1, 1));
            if rows.contains(point) && event.row > rows.y {
                let visible = rows.height.saturating_sub(1) as usize;
                let offset = self.table_offset(visible);
                let selected = offset + (event.row - rows.y - 1) as usize;
                if selected < self.rows.len() {
                    self.selected = selected;
                    self.focus = 3;
                }
            }
        } else if view.queue.contains(point) {
            match event.kind {
                MouseEventKind::ScrollDown => {
                    self.selected = (self.selected + 1).min(self.rows.len().saturating_sub(1))
                }
                MouseEventKind::ScrollUp => self.selected = self.selected.saturating_sub(1),
                _ => {}
            }
        }
    }
    fn table_offset(&self, visible: usize) -> usize {
        self.selected.saturating_sub(visible.saturating_sub(1))
    }
}
fn row_status(value: &str) -> &str {
    match value {
        "body_ready" => "本文保存済み",
        "assets_pending" => "素材待ち",
        "verifying" => "検証中",
        "complete" => "完了",
        "already_complete" => "保存済み",
        "partial" => "欠損あり",
        "paused" => "一時停止",
        _ => value,
    }
}
fn clean(value: &str) -> String {
    value
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect()
}
fn clean_lines(value: &str) -> String {
    value
        .chars()
        .map(|c| if c.is_control() && c != '\n' { ' ' } else { c })
        .collect()
}
fn panel(title: &str, focus: bool) -> Block<'_> {
    Block::default()
        .title(format!(" {title} "))
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(if focus { CYAN } else { Color::Rgb(43, 63, 82) }))
}
fn editor(
    frame: &mut Frame,
    area: Rect,
    edit: &Editor,
    title: &str,
    focus: bool,
    placeholder: &str,
) {
    let block = panel(title, focus);
    let inner = block.inner(area);
    frame.render_widget(block, area);
    if inner.width == 0 || inner.height == 0 {
        return;
    }
    if edit.value.is_empty() && !focus {
        frame.render_widget(
            Paragraph::new(placeholder).style(Style::default().fg(MUTED)),
            inner,
        );
    } else {
        let (text, x, y) = edit.visible(inner.width as usize, inner.height as usize);
        frame.render_widget(Paragraph::new(text).style(Style::default().fg(INK)), inner);
        if focus {
            frame.set_cursor_position((inner.x + x, inner.y + y));
        }
    }
}
struct View {
    body: Rect,
    header: Rect,
    fields: Rect,
    queue: Rect,
    activity: Rect,
    footer: Rect,
    large: bool,
}
impl View {
    fn new(area: Rect) -> Option<Self> {
        let margin = if area.width >= 60 { 2 } else { 0 };
        let body = area.inner(ratatui::layout::Margin::new(margin, 1));
        if body.height < 10 || body.width < 32 {
            return None;
        }
        let large = body.height >= 23 && body.width >= 48;
        let chunks = Layout::vertical([
            Constraint::Length(if large { 8 } else { 2 }),
            Constraint::Min(7),
            Constraint::Length(3),
        ])
        .split(body);
        let main = if body.width >= 96 {
            Layout::horizontal([Constraint::Percentage(40), Constraint::Percentage(60)])
                .split(chunks[1])
        } else {
            Layout::vertical([Constraint::Percentage(58), Constraint::Percentage(42)])
                .split(chunks[1])
        };
        let queue = Layout::vertical([
            Constraint::Min(3),
            Constraint::Length(if main[1].height >= 14 { 5 } else { 3 }),
        ])
        .split(main[1]);
        Some(Self {
            body,
            header: chunks[0],
            fields: main[0],
            queue: queue[0],
            activity: queue[1],
            footer: chunks[2],
            large,
        })
    }
}
fn button_labels(footer: Rect) -> (&'static str, &'static str) {
    if footer.width >= 70 {
        ("[ F6 保存先を開く ]", "[ F7 選択作品を開く ]")
    } else {
        ("[F6 保存先]", "[F7 選択作品]")
    }
}
fn buttons(footer: Rect) -> (Rect, Rect) {
    let (root, work) = button_labels(footer);
    let y = footer.y.saturating_add(1);
    let root_width = UnicodeWidthStr::width(root) as u16;
    let work_width = UnicodeWidthStr::width(work) as u16;
    (
        Rect::new(footer.x, y, root_width, 1),
        Rect::new(footer.x + root_width + 1, y, work_width, 1),
    )
}
fn collection_buttons(footer: Rect) -> Option<(Rect, Rect)> {
    let (_, work) = buttons(footer);
    let x = work.right() + 1;
    let image_width = UnicodeWidthStr::width("[画像 Alt+I]") as u16;
    let novel_width = UnicodeWidthStr::width("[小説 Alt+N]") as u16;
    if x + image_width + 1 + novel_width > footer.right() {
        return None;
    }
    Some((
        Rect::new(x, work.y, image_width, 1),
        Rect::new(x + image_width + 1, work.y, novel_width, 1),
    ))
}
fn settings_button(footer: Rect) -> Option<Rect> {
    let (_, work) = buttons(footer);
    let x = collection_buttons(footer).map_or(work.right() + 1, |(_, novel)| novel.right() + 1);
    let width = UnicodeWidthStr::width("[F10 設定]") as u16;
    (x + width <= footer.right()).then_some(Rect::new(x, work.y, width, 1))
}
fn resume_button(footer: Rect) -> Option<Rect> {
    let settings = settings_button(footer)?;
    let width = UnicodeWidthStr::width("[再開 Ctrl+R]") as u16;
    let x = settings.right() + 1;
    (x + width <= footer.right()).then_some(Rect::new(x, settings.y, width, 1))
}
fn profile_label(balanced: bool) -> &'static str {
    if balanced {
        "分離: 全体1秒/API2秒/媒体1秒"
    } else {
        "互換: 全通信2秒間隔"
    }
}
fn stop_label(value: &Value) -> String {
    match value["kind"].as_str() {
        Some("user_cancel") => "ユーザー操作で停止".into(),
        Some("request_budget") => "要求予算に到達・進行中の保存を保持".into(),
        Some("http_status") => format!("HTTP {}により停止", value["status"]),
        Some("unexpected_content") => "期待と異なる応答で停止".into(),
        Some("worker_error") => "ワーカー異常で停止".into(),
        Some("disk_error") => "保存領域の問題で停止".into(),
        _ => clean(&value.to_string()),
    }
}
fn transport_summary(value: &Value) -> Vec<String> {
    let mut lines = Vec::new();
    for (key, label) in [("api", "API"), ("media", "媒体")] {
        let item = &value[key];
        if let Some(requests) = item["requests"].as_u64() {
            lines.push(format!(
                "{label}: {requests}要求 / {:.2}要求毎秒 / {:.2}MiB",
                item["requests_per_second"].as_f64().unwrap_or(0.0),
                item["bytes"].as_u64().unwrap_or(0) as f64 / (1024.0 * 1024.0)
            ));
        }
        if let Some(p95) = item["header_p95_seconds"].as_f64() {
            lines.push(format!(
                "{label} ヘッダー待ちp95 {p95:.2}秒 / 転送p95 {:.2}秒",
                item["transfer_p95_seconds"].as_f64().unwrap_or(0.0)
            ));
        }
    }
    if let Some(hits) = value["shared_media"]["hits"].as_u64() {
        lines.push(format!(
            "媒体の共有: 再利用 {hits} / 新規取得 {} / キャッシュ {:.2}MiB",
            value["shared_media"]["misses"].as_u64().unwrap_or(0),
            value["shared_media"]["cache_bytes"].as_u64().unwrap_or(0) as f64 / (1024.0 * 1024.0)
        ));
    }
    if let Some(retries) = value["retries"].as_u64() {
        lines.push(format!("追加試行: {retries}回"));
    }
    lines
}
fn popup_rect(body: Rect, modal: &Modal) -> Rect {
    let width = body
        .width
        .min(if *modal == Modal::Update { 88 } else { 72 });
    let height = body.height.min(match modal {
        Modal::Help => 24,
        Modal::Result => 24,
        Modal::Update => 19,
        Modal::Collection => 20,
        Modal::Settings | Modal::Confirm => 16,
        Modal::None => 12,
    });
    Rect::new(
        body.x + (body.width - width) / 2,
        body.y + (body.height - height) / 2,
        width,
        height,
    )
}
fn settings_controls(popup: Rect) -> (Rect, Rect, Rect, [Rect; 3], Rect) {
    let inner = popup.inner(ratatui::layout::Margin::new(1, 1));
    let control = |x, y, width, height| {
        Rect::new(popup.x + x, popup.y + y, width, height).intersection(inner)
    };
    (
        control(2, 3, 12, 1),
        control(17, 3, UnicodeWidthStr::width(BALANCED_BUTTON) as u16, 1),
        control(2, 5, popup.width.saturating_sub(4), 3),
        [
            control(2, 9, 10, 1),
            control(13, 9, 11, 1),
            control(25, 9, 12, 1),
        ],
        control(2, 13, 14, 1),
    )
}
fn draw_settings(frame: &mut Frame, app: &App, popup: Rect) {
    frame.render_widget(Clear, popup);
    frame.render_widget(panel("通信・要求予算 · 次回の保存から適用", true), popup);
    let inner = popup.inner(ratatui::layout::Margin::new(1, 1));
    let (legacy, balanced, budget, presets, apply) = settings_controls(popup);
    let row = |offset| {
        Rect::new(
            popup.x + 2,
            popup.y + offset,
            popup.width.saturating_sub(4),
            1,
        )
        .intersection(inner)
    };
    frame.render_widget(
        Paragraph::new("通信プロファイル  ← / → で切替").style(Style::default().fg(MUTED)),
        row(1),
    );
    for (rect, text, selected) in [
        (legacy, "[ 互換 2秒 ]", !app.settings_balanced),
        (balanced, BALANCED_BUTTON, app.settings_balanced),
    ] {
        frame.render_widget(
            Paragraph::new(text).style(
                Style::default()
                    .fg(if selected { CYAN } else { MUTED })
                    .add_modifier(if selected {
                        Modifier::BOLD
                    } else {
                        Modifier::empty()
                    }),
            ),
            rect,
        );
    }
    frame.render_widget(
        Paragraph::new(profile_label(app.settings_balanced)).style(Style::default().fg(MUTED)),
        row(4),
    );
    editor(
        frame,
        budget,
        &app.settings_budget,
        "要求予算 · 1〜100000回",
        app.settings_focus == 1,
        "",
    );
    for (rect, label) in presets
        .into_iter()
        .zip(["[F1 500]", "[F2 5000]", "[F3 20000]"])
    {
        frame.render_widget(Paragraph::new(label).style(Style::default().fg(CYAN)), rect);
    }
    frame.render_widget(
        Paragraph::new("Tab: 移動 / Ctrl+A: 数値消去 / Esc: 取消")
            .style(Style::default().fg(MUTED)),
        row(11),
    );
    if let Some(error) = &app.settings_error {
        frame.render_widget(
            Paragraph::new(error.as_str()).style(Style::default().fg(Color::Yellow)),
            row(12),
        );
    }
    frame.render_widget(
        Paragraph::new("[Enter 適用]").style(Style::default().fg(CYAN)),
        apply,
    );
}
fn update_controls(header: Rect) -> Option<(Rect, Rect, Rect)> {
    if header.height == 0 {
        return None;
    }
    let row = Rect::new(header.x, header.bottom() - 1, header.width, 1);
    let controls = Layout::horizontal([
        Constraint::Min(1),
        Constraint::Length(9),
        Constraint::Length(13),
    ])
    .split(row);
    Some((controls[0], controls[1], controls[2]))
}
fn path_label(path: Option<&std::path::Path>, width: usize) -> String {
    let Some(path) = path else {
        return "—".into();
    };
    let text = clean(&path.to_string_lossy());
    if UnicodeWidthStr::width(text.as_str()) <= width {
        return text;
    }
    let mut reversed = Vec::new();
    let mut length = 1;
    for c in text.chars().rev() {
        let w = UnicodeWidthStr::width(c.to_string().as_str());
        if length + w > width {
            break;
        }
        length += w;
        reversed.push(c);
    }
    format!("…{}", reversed.into_iter().rev().collect::<String>())
}
fn draw(frame: &mut Frame, app: &App, no_color: bool) {
    draw_inner(frame, app, no_color);
    if no_color {
        for cell in &mut frame.buffer_mut().content {
            cell.set_fg(Color::Reset);
            cell.set_bg(Color::Reset);
        }
    }
}
fn draw_inner(frame: &mut Frame, app: &App, no_color: bool) {
    let area = frame.area();
    frame.render_widget(
        Block::default().style(
            Style::default()
                .bg(if no_color { Color::Reset } else { BG })
                .fg(if no_color { Color::Reset } else { INK }),
        ),
        area,
    );
    let Some(view) = View::new(area) else {
        frame.render_widget(Paragraph::new("OPEN PIXIV DOWNLOADER\n\n端末を32桁×12行以上に広げてください。\nEsc / Ctrl+C: 終了").wrap(Wrap{trim:false}),area);
        return;
    };
    let body = view.body;
    if view.large {
        let word = if UnicodeWidthStr::width(logo("OPEN PIXIV")[0].as_str()) <= body.width as usize
        {
            "OPEN PIXIV"
        } else {
            "PIXIV"
        };
        let mut lines: Vec<Line> = logo(word)
            .into_iter()
            .enumerate()
            .map(|(i, s)| {
                let spans = s
                    .chars()
                    .map(|c| {
                        Span::styled(
                            c.to_string(),
                            Style::default().fg(if no_color {
                                Color::Reset
                            } else if c == '█' {
                                Color::Rgb(90 - i as u8 * 6, 226 - i as u8 * 14, 240)
                            } else {
                                Color::Rgb(42, 78, 130)
                            }),
                        )
                    })
                    .collect::<Vec<_>>();
                Line::from(spans).alignment(Alignment::Center)
            })
            .collect();
        lines.push(
            Line::from("D O W N L O A D E R")
                .style(Style::default().fg(if no_color { Color::Reset } else { MUTED }))
                .alignment(Alignment::Center),
        );
        frame.render_widget(Paragraph::new(lines), view.header);
    } else {
        frame.render_widget(
            Paragraph::new("OPEN PIXIV DOWNLOADER · Rust + Ratatui")
                .style(Style::default().fg(CYAN).add_modifier(Modifier::BOLD)),
            view.header,
        );
    }
    if let Some((notice, check, details)) = update_controls(view.header) {
        let available = matches!(app.update_status, update::Status::Available(_));
        let text = if notice.width >= 48 {
            app.update_status.message()
        } else {
            match app.update_status {
                update::Status::Available(_) => "更新あり！".into(),
                update::Status::Checking => "確認中…".into(),
                update::Status::Current(_) => "更新なし".into(),
                update::Status::Failed(_) => "確認不可".into(),
                update::Status::Unchecked => "未確認".into(),
            }
        };
        frame.render_widget(
            Paragraph::new(text).style(
                Style::default()
                    .fg(if available { Color::Yellow } else { MUTED })
                    .add_modifier(if available {
                        Modifier::BOLD
                    } else {
                        Modifier::empty()
                    }),
            ),
            notice,
        );
        frame.render_widget(
            Paragraph::new("[F8 確認]").style(Style::default().fg(MUTED)),
            check,
        );
        frame.render_widget(
            Paragraph::new("[F9 更新方法]").style(Style::default().fg(if available {
                CYAN
            } else {
                MUTED
            })),
            details,
        );
    }
    let fields = Layout::vertical([
        Constraint::Min(3),
        Constraint::Length(if view.fields.height >= 12 { 6 } else { 3 }),
    ])
    .split(view.fields);
    editor(
        frame,
        fields[0],
        &app.urls,
        "01  URL INPUT · 1行に1URL",
        app.focus == 0 && app.worker.is_none() && app.modal == Modal::None,
        "https://www.pixiv.net/artworks/…",
    );
    if fields[1].height >= 6 {
        let opts =
            Layout::vertical([Constraint::Length(3), Constraint::Length(3)]).split(fields[1]);
        editor(
            frame,
            opts[0],
            &app.out,
            "02  保存先",
            app.focus == 1 && app.worker.is_none() && app.modal == Modal::None,
            "",
        );
        frame.render_widget(
            Paragraph::new(format!(
                "  {} · {}作品 · F10 設定",
                if app.demo {
                    "DEMO · OFFLINE"
                } else {
                    "ONLINE · ANONYMOUS"
                },
                app.jobs
            ))
            .block(panel("03  MODE / WORKERS · F10設定", app.focus == 2)),
            opts[1],
        );
    } else {
        let opts = Layout::horizontal([Constraint::Percentage(65), Constraint::Percentage(35)])
            .split(fields[1]);
        editor(
            frame,
            opts[0],
            &app.out,
            "保存先",
            app.focus == 1 && app.worker.is_none() && app.modal == Modal::None,
            "",
        );
        frame.render_widget(
            Paragraph::new(format!(
                "{} / {}",
                if app.demo { "DEMO" } else { "ONLINE" },
                app.jobs
            ))
            .block(panel("並行数 ← →", app.focus == 2)),
            opts[1],
        );
    }
    if app.rows.is_empty() {
        frame.render_widget(
            Paragraph::new(vec![
                Line::from(""),
                Line::from("  Welcome to Open Pixiv Downloader!"),
                Line::from("  F2: デモの4作品で試す"),
                Line::from("  URL入力 → Ctrl+S: 保存を確認"),
                Line::from("  F5: 対象一覧 / Alt+V: 検査"),
                Line::from("  Alt+E: ZIP出力 / F1: ヘルプ"),
                Line::from("  F6: 保存先 / F7: 選択作品を開く"),
            ])
            .block(panel("QUEUE / LIBRARY", false)),
            view.queue,
        );
    } else {
        let path_width = view.queue.width.saturating_sub(6) as usize * 44 / 100;
        let visible = view.queue.height.saturating_sub(3) as usize;
        let offset = app.table_offset(visible);
        let rows = app.rows.iter().skip(offset).take(visible).map(|row| {
            Row::new([
                clean(&row.key),
                row_status(&row.status).to_owned(),
                path_label(row.output.as_deref(), path_width),
            ])
            .style(Style::default().fg(match row.status.as_str() {
                "complete" | "already_complete" => CYAN,
                "saving" => BLUE,
                _ => Color::Yellow,
            }))
        });
        let mut state =
            TableState::default().with_selected(Some(app.selected.saturating_sub(offset)));
        frame.render_stateful_widget(
            Table::new(
                rows,
                [
                    Constraint::Percentage(30),
                    Constraint::Percentage(26),
                    Constraint::Percentage(44),
                ],
            )
            .header(Row::new(["WORK", "STATUS", "OUTPUT / PATH"]).style(Style::default().fg(MUTED)))
            .highlight_symbol("› ")
            .row_highlight_style(
                Style::default()
                    .bg(Color::Rgb(27, 44, 62))
                    .add_modifier(Modifier::BOLD),
            )
            .block(panel("QUEUE · ↑↓ 選択 / Enter 開く", app.focus == 3)),
            view.queue,
            &mut state,
        );
    }
    let available = view.activity.height.saturating_sub(2) as usize;
    let logs: Vec<_> = app
        .logs
        .iter()
        .skip(app.logs.len().saturating_sub(available))
        .map(|s| Line::from(s.as_str()))
        .collect();
    frame.render_widget(
        Paragraph::new(logs)
            .style(Style::default().fg(MUTED))
            .block(panel(&app.telemetry_label(), false)),
        view.activity,
    );
    let foot = Layout::vertical([
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Length(1),
    ])
    .split(view.footer);
    let ratio = if app.total > 0 {
        (app.done as f64 / app.total as f64).min(1.0)
    } else {
        0.0
    };
    let (root, work) = buttons(view.footer);
    let (root_label, work_label) = button_labels(view.footer);
    frame.render_widget(
        Paragraph::new(root_label).style(Style::default().fg(CYAN).add_modifier(Modifier::BOLD)),
        root,
    );
    let can_open = app
        .rows
        .get(app.selected)
        .is_some_and(|r| r.output.is_some());
    frame.render_widget(
        Paragraph::new(work_label).style(Style::default().fg(if can_open { CYAN } else { MUTED })),
        work,
    );
    if let Some((image, novel)) = collection_buttons(view.footer) {
        frame.render_widget(
            Paragraph::new("[画像 Alt+I]").style(Style::default().fg(CYAN)),
            image,
        );
        frame.render_widget(
            Paragraph::new("[小説 Alt+N]").style(Style::default().fg(CYAN)),
            novel,
        );
    }
    if let Some(settings) = settings_button(view.footer) {
        frame.render_widget(
            Paragraph::new("[F10 設定]").style(Style::default().fg(CYAN)),
            settings,
        );
    }
    if let Some(resume) = resume_button(view.footer) {
        frame.render_widget(
            Paragraph::new("[再開 Ctrl+R]").style(Style::default().fg(CYAN)),
            resume,
        );
    }
    frame.render_widget(
        Gauge::default()
            .ratio(ratio)
            .gauge_style(
                Style::default()
                    .fg(if no_color { Color::White } else { BLUE })
                    .bg(if no_color { Color::Reset } else { BG }),
            )
            .label(format!(
                "{} {}/{} · {}",
                app.status,
                app.done,
                app.total,
                app.progress_label()
            )),
        foot[0],
    );
    frame.render_widget(
        Paragraph::new("Ctrl+S 保存   F10 設定   F11 再開   F4 集約   F1 ヘルプ   Esc 終了")
            .style(Style::default().fg(MUTED)),
        foot[2],
    );
    if app.modal != Modal::None {
        let popup = popup_rect(body, &app.modal);
        if app.modal == Modal::Settings {
            draw_settings(frame, app, popup);
            return;
        }
        frame.render_widget(Clear, popup);
        let text = if app.modal == Modal::Help {
            "URLを複数貼り付けて、Ctrl+S → Enterで保存します。\n作者URLは列挙できた全作品を対象にします。\n\nTab / Shift+Tab: 入力欄・作品一覧の移動\nF2: デモ ↔ 匿名オンライン（入力・保存先を切替）\nF3: 結果と欠損の詳細 / ↑↓でスクロール\nF4: 画像・小説の集約 / Alt+I・N: 開く\nF5: 対象一覧を確認（媒体は保存しません）\nF6: 保存先を開く / F7: 選択作品を開く\nF8: 更新確認 / F9: 更新方法\nF10: 通信と要求予算 / F11・Ctrl+R: 保存対象から再開\nF5→F11: 最新の確認対象を再列挙せず保存（以前の保存があっても可）\n一覧取得が未完了なら再開せず、元のURLでもう一度確認\nCtrl+S: 保存確認 / Enter: 開始\nAlt+V: 保存先を検査 / Alt+E: 保存先名-export.zipへ出力\n並行数欄: ← → または1〜4\n一覧で↑↓: 選択 / Enter: フォルダーを開く\nPgUp / PgDn: 5作品ずつ移動\nEsc / Ctrl+C: 実行中は停止、待機中は終了\n\n実サイト・Windows実機は未検証のα版です。\nEnter: URL改行 / Esc: この画面を閉じる".to_owned()
        } else if app.modal == Modal::Collection {
            app.collection_details()
        } else if app.modal == Modal::Update {
            app.update_details()
        } else if app.modal == Modal::Result {
            app.summary()
        } else {
            format!("{}\n\n対象: {}\n保存先: {}\n並行保存: {}作品\n通信: {}\n要求予算: {}回（F10で変更）\n\n{}\n\nEnter: 保存開始    Esc: 入力へ戻る",if app.demo{"合成データを保存します · 通信なし"}else{"Pixivへ匿名で接続します"},if app.confirm_resume { "最新の保存対象一覧から再開（job / plan）".into() }else{format!("入力{}件（作者URLは作品一覧へ展開）",app.urls.value.split_whitespace().count())},clean(&app.out.value),app.jobs,profile_label(app.balanced),app.request_budget,if app.demo{"デモ保存は実作品の取得を検証するものではありません。"}else{"拒否応答では新規要求を停止します。予算到達時は進行中の正常な保存を確定します。"})
        };
        frame.render_widget(
            Paragraph::new(text)
                .wrap(Wrap { trim: false })
                .scroll((
                    if matches!(app.modal, Modal::Result | Modal::Collection) {
                        app.modal_scroll
                    } else {
                        0
                    },
                    0,
                ))
                .style(Style::default().fg(INK).bg(BG))
                .block(panel(
                    if app.modal == Modal::Help {
                        "HELP"
                    } else if app.modal == Modal::Collection {
                        "画像・小説の集約"
                    } else if app.modal == Modal::Update {
                        "アップデート"
                    } else if app.modal == Modal::Result {
                        "結果 / 欠損 · ↑↓ / PgUp PgDn · Escで閉じる"
                    } else {
                        "保存内容の確認"
                    },
                    true,
                )),
            popup,
        );
    }
}
pub fn run(automatic_updates: bool) -> Result<()> {
    let no_color = std::env::var_os("NO_COLOR").is_some();
    let mut terminal = ratatui::init();
    // Ratatui's panic hook restores terminal state; the ordinary path also disables paste.
    let result = (|| -> Result<bool> {
        let _input_modes = InputModes;
        execute!(io::stdout(), EnableBracketedPaste, EnableMouseCapture)?;
        let mut app = App::default();
        app.automatic_updates = automatic_updates;
        if automatic_updates {
            app.check_update();
        }
        loop {
            app.poll();
            if app.dirty {
                terminal.draw(|f| draw(f, &app, no_color))?;
                app.dirty = false;
            }
            if event::poll(Duration::from_millis(100))? {
                match event::read()? {
                    InputEvent::Key(key) => {
                        if app.key(key) {
                            break;
                        }
                    }
                    InputEvent::Mouse(mouse) => {
                        let size = terminal.size()?;
                        app.mouse(mouse, Rect::new(0, 0, size.width, size.height));
                    }
                    InputEvent::Paste(text) if app.worker.is_none() && app.modal == Modal::None => {
                        app.dirty = true;
                        match app.focus {
                            0 => app.urls.insert(&text, true),
                            1 => app.out.insert(&text, false),
                            _ => {}
                        }
                    }
                    InputEvent::Resize(_, _) => app.dirty = true,
                    _ => {}
                }
            }
        }
        if let Some(w) = app.worker.take() {
            w.stop.store(true, Ordering::SeqCst);
            let _ = w.handle.join();
        }
        Ok(app.show_update_command)
    })();
    ratatui::restore();
    if result? {
        update::print_instructions();
    }
    Ok(())
}
struct InputModes;
impl Drop for InputModes {
    fn drop(&mut self) {
        let _ = execute!(io::stdout(), DisableBracketedPaste, DisableMouseCapture);
    }
}
impl Drop for App {
    fn drop(&mut self) {
        if let Some(w) = self.worker.take() {
            w.stop.store(true, Ordering::SeqCst);
            let _ = w.handle.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::{backend::TestBackend, Terminal};
    fn screen_text(terminal: &Terminal<TestBackend>) -> String {
        let buffer = terminal.backend().buffer();
        let width = buffer.area.width as usize;
        let mut output = String::new();
        for row in buffer.content.chunks(width) {
            let mut x = 0;
            while x < row.len() {
                let symbol = row[x].symbol();
                output.push_str(symbol);
                x += UnicodeWidthStr::width(symbol).max(1);
            }
            output.push('\n');
        }
        output
    }
    #[test]
    fn settings_validate_budget_and_apply_only_on_enter() {
        let mut app = App::default();
        assert_eq!(app.request_budget, 20_000);
        assert!(!app.balanced);
        app.key(KeyEvent::new(KeyCode::F(10), KeyModifiers::NONE));
        assert!(app.modal == Modal::Settings);
        app.key(KeyEvent::new(KeyCode::Right, KeyModifiers::NONE));
        assert!(!app.balanced);
        app.key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
        app.key(KeyEvent::new(KeyCode::Char('a'), KeyModifiers::CONTROL));
        app.key(KeyEvent::new(KeyCode::Char('0'), KeyModifiers::NONE));
        app.key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert!(app.modal == Modal::Settings);
        assert!(app.settings_error.is_some());
        assert_eq!(app.request_budget, 20_000);
        app.settings_budget = Editor::new("100001");
        app.apply_settings();
        assert!(app.modal == Modal::Settings);
        app.key(KeyEvent::new(KeyCode::F(2), KeyModifiers::NONE));
        app.key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert!(app.modal == Modal::None);
        assert_eq!(app.request_budget, 5000);
        assert_eq!(app.active_request_budget, 5000);
        assert!(app.balanced);
        app.urls = Editor::new("art:1");
        let options = app.options().unwrap();
        assert_eq!(options.request_budget, 5000);
        assert_eq!(options.interval, 1.0);
        assert!(options.balanced);
        app.open_settings();
        app.settings_budget = Editor::new("9");
        app.key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        assert_eq!(app.request_budget, 5000);
    }
    #[test]
    fn settings_mouse_and_small_screen_keep_controls_available() {
        for (width, height) in [(40, 18), (80, 24), (120, 36)] {
            let mut app = App::default();
            app.open_settings();
            let area = Rect::new(0, 0, width, height);
            let popup = popup_rect(View::new(area).unwrap().body, &app.modal);
            let (_, balanced, _, presets, apply) = settings_controls(popup);
            for rect in [balanced, presets[0], apply] {
                app.mouse(
                    MouseEvent {
                        kind: MouseEventKind::Down(MouseButton::Left),
                        column: rect.x,
                        row: rect.y,
                        modifiers: KeyModifiers::NONE,
                    },
                    area,
                );
            }
            assert_eq!(app.request_budget, 500);
            assert!(app.balanced);
            app.open_settings();
            let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
            terminal.draw(|frame| draw(frame, &app, false)).unwrap();
            let text = screen_text(&terminal);
            assert!(text.contains("要求予算"));
            assert!(text.contains("[ API・媒体を分離 ]"));
        }
    }
    #[test]
    fn resume_does_not_require_urls_and_confirmation_keeps_settings_visible() {
        let mut app = App::default();
        app.balanced = true;
        app.request_budget = 20_000;
        assert!(app.options().is_err());
        let options = app.options_for(true).unwrap();
        assert!(options.resume);
        assert!(options.targets.is_empty());
        assert_eq!(options.request_budget, 20_000);
        app.key(KeyEvent::new(KeyCode::F(11), KeyModifiers::NONE));
        assert!(app.modal == Modal::Confirm);
        assert!(app.confirm_resume);
        let mut terminal = Terminal::new(TestBackend::new(120, 36)).unwrap();
        terminal.draw(|frame| draw(frame, &app, false)).unwrap();
        let text = screen_text(&terminal);
        assert!(text.contains("対象一覧から再開"));
        assert!(text.contains("20000"));
        assert!(text.contains("全体1秒"));
    }
    #[test]
    fn body_ready_is_not_full_completion_and_thousands_of_rows_keep_selection() {
        let mut app = App::default();
        for id in 1..=4332 {
            app.set_row(format!("novel_{id}"), "assets_pending".into());
        }
        app.set_row("novel_4328".into(), "body_ready".into());
        app.set_row("novel_4328".into(), "body_ready".into());
        assert_eq!(app.body_ready, 1);
        assert_eq!(app.complete, 0);
        app.set_row("novel_1".into(), "complete".into());
        app.set_row("novel_1".into(), "already_complete".into());
        assert_eq!(app.body_ready, 2);
        assert_eq!(app.complete, 1);
        assert_eq!(app.rows.len(), 4332);
        assert_eq!(app.row_index.len(), 4332);
        app.selected = 4327;
        app.focus = 3;
        let area = Rect::new(0, 0, 120, 36);
        let mut terminal = Terminal::new(TestBackend::new(120, 36)).unwrap();
        terminal.draw(|frame| draw(frame, &app, false)).unwrap();
        let text = screen_text(&terminal);
        assert!(text.contains("novel_4328"));
        assert!(text.contains("本文保存済み"));
        let queue = View::new(area)
            .unwrap()
            .queue
            .inner(ratatui::layout::Margin::new(1, 1));
        let offset = app.table_offset(queue.height.saturating_sub(1) as usize);
        app.mouse(
            MouseEvent {
                kind: MouseEventKind::Down(MouseButton::Left),
                column: queue.x,
                row: queue.y + 1,
                modifiers: KeyModifiers::NONE,
            },
            area,
        );
        assert_eq!(app.selected, offset);
        app.clear_rows();
        assert!(app.row_index.is_empty());
        assert_eq!(app.complete, 0);
        assert_eq!(app.body_ready, 0);
    }
    #[test]
    fn planned_author_list_keeps_all_four_thousand_targets() {
        let mut app = App::default();
        let (sender, receiver) = mpsc::channel();
        app.worker = Some(Worker {
            receiver,
            stop: Arc::default(),
            handle: thread::spawn(move || {
                sender.send(Event::Finished(serde_json::json!({"status":"planned","selected_targets":(1..=4332).map(|id|format!("novel:{id}")).collect::<Vec<_>>()}))).unwrap();
            }),
        });
        finish(&mut app);
        assert_eq!(app.rows.len(), 4332);
        assert_eq!(app.row_index.len(), 4332);
        assert!(app.rows.iter().all(|row| row.status == "planned"));
        assert_eq!(app.complete, 0);
        assert_eq!(app.body_ready, 0);
    }
    #[test]
    fn collection_dialog_backfills_missing_text_offline_and_keeps_work_rows() {
        let temp = tempfile::tempdir().unwrap();
        let out = temp.path().join("集約テスト");
        let mut app = App::default();
        app.demo = true;
        app.urls = Editor::new("user:9001");
        app.out = Editor::new(out.to_str().unwrap());
        app.start("fetch");
        finish(&mut app);
        assert_eq!(app.rows.len(), 4);
        let novel = out.join("works/novel/novel_1004.txt");
        assert!(novel.is_file());
        std::fs::remove_file(&novel).unwrap();
        app.key(KeyEvent::new(KeyCode::F(4), KeyModifiers::NONE));
        assert!(app.modal == Modal::Collection);
        assert!(app
            .collection_details()
            .contains(&out.join("works/novel").display().to_string()));
        for (w, h) in [(40, 18), (80, 24), (120, 36)] {
            let mut t = Terminal::new(TestBackend::new(w, h)).unwrap();
            t.draw(|f| draw(f, &app, false)).unwrap();
        }
        app.key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        finish(&mut app);
        assert!(novel.is_file());
        assert_eq!(app.last.as_ref().unwrap()["collection"]["copied"], 1);
        assert_eq!(app.last.as_ref().unwrap()["collection"]["existing"], 5);
        assert_eq!(app.rows.len(), 4);
        assert!(app.summary().contains("ファイル集約"));
    }
    #[test]
    fn asynchronous_update_notification_is_visible_without_blocking_input() {
        let mut app = App::default();
        let (sender, receiver) = mpsc::channel();
        app.update_receiver = Some(receiver);
        app.update_status = update::Status::Checking;
        app.last_update_check = Some(Instant::now());
        app.poll();
        app.key(KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE));
        assert_eq!(app.urls.value, "x");
        sender.send(Ok(semver::Version::new(99, 0, 0))).unwrap();
        app.poll();
        assert!(matches!(app.update_status, update::Status::Available(_)));
        let mut terminal = Terminal::new(TestBackend::new(120, 36)).unwrap();
        terminal.draw(|f| draw(f, &app, false)).unwrap();
        // A terminal cell after a wide glyph is padding, not an extra character.
        let buffer = terminal.backend().buffer();
        let mut text = String::new();
        for row in buffer.content.chunks(120) {
            let mut x = 0;
            while x < row.len() {
                let symbol = row[x].symbol();
                text.push_str(symbol);
                x += UnicodeWidthStr::width(symbol).max(1);
            }
            text.push('\n');
        }
        assert!(text.contains("アップデートが利用できます！"));
        assert!(text.contains("F9 更新方法"));
        app.key(KeyEvent::new(KeyCode::F(8), KeyModifiers::NONE));
        assert!(app.update_receiver.is_none());
        assert!(matches!(app.update_status, update::Status::Available(_)));
        let (sender, receiver) = mpsc::channel();
        app.update_receiver = Some(receiver);
        sender.send(Err("offline".into())).unwrap();
        app.poll();
        assert!(matches!(app.update_status, update::Status::Available(_)));
    }
    #[test]
    fn update_dialog_does_not_exit_or_cancel_active_downloads() {
        let mut app = App::default();
        let (_sender, receiver) = mpsc::channel();
        let stop = Arc::new(AtomicBool::new(false));
        app.worker = Some(Worker {
            receiver,
            handle: thread::spawn(|| {}),
            stop: stop.clone(),
        });
        app.key(KeyEvent::new(KeyCode::F(9), KeyModifiers::NONE));
        assert!(app.modal == Modal::Update);
        assert!(!app.key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)));
        assert!(!stop.load(Ordering::SeqCst));
        assert!(!app.show_update_command);
        app.worker.take().unwrap().handle.join().unwrap();
        assert!(app.key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)));
        assert!(app.show_update_command);
    }
    #[test]
    fn update_failure_and_dialog_render_at_small_widths_and_support_clicks() {
        for (w, h) in [(40, 18), (80, 24), (120, 36)] {
            let mut app = App::default();
            let (sender, receiver) = mpsc::channel();
            app.update_receiver = Some(receiver);
            sender.send(Err("接続できません".into())).unwrap();
            app.poll();
            assert!(matches!(app.update_status, update::Status::Failed(_)));
            let area = Rect::new(0, 0, w, h);
            let view = View::new(area).unwrap();
            if let Some((_, _, details)) = update_controls(view.header) {
                app.mouse(
                    MouseEvent {
                        kind: MouseEventKind::Down(MouseButton::Left),
                        column: details.x,
                        row: details.y,
                        modifiers: KeyModifiers::NONE,
                    },
                    area,
                );
                assert!(app.modal == Modal::Update);
            }
            let mut terminal = Terminal::new(TestBackend::new(w, h)).unwrap();
            terminal.draw(|f| draw(f, &app, true)).unwrap();
            assert!(app.update_details().contains("接続できません"));
        }
    }
    #[test]
    fn logo_has_depth_and_fits_wide_terminal() {
        let rows = logo("OPEN PIXIV");
        assert_eq!(rows.len(), 6);
        assert!(rows.iter().any(|s| s.contains('╚')));
        assert!(rows
            .iter()
            .all(|s| UnicodeWidthStr::width(s.as_str()) <= 100));
    }
    #[test]
    fn unicode_edit_and_paste_are_data() {
        let mut e = Editor::new("日本語");
        e.left();
        e.key(KeyEvent::new(KeyCode::Backspace, KeyModifiers::NONE), false);
        assert_eq!(e.value, "日語");
        e.insert("\u{1b}[31m\r\nhttps://www.pixiv.net/artworks/1", true);
        assert!(!e.value.contains('\u{1b}'));
        let (_, x, y) = e.visible(12, 3);
        assert!(x < 12 && y < 3);
    }
    #[test]
    fn responsive_draws_and_confirmation() {
        for (w, h) in [(24, 8), (40, 18), (80, 24), (120, 36), (160, 45)] {
            let mut app = App::default();
            let mut t = Terminal::new(TestBackend::new(w, h)).unwrap();
            t.draw(|f| draw(f, &app, false)).unwrap();
            app.key(KeyEvent::new(KeyCode::F(2), KeyModifiers::NONE));
            app.key(KeyEvent::new(KeyCode::Char('s'), KeyModifiers::CONTROL));
            assert!(app.modal == Modal::Confirm);
            let mut plain = Terminal::new(TestBackend::new(w, h)).unwrap();
            plain.draw(|f| draw(f, &app, true)).unwrap();
            assert!(plain
                .backend()
                .buffer()
                .content
                .iter()
                .all(|c| c.fg == Color::Reset && c.bg == Color::Reset));
            app.key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
            assert!(app.modal == Modal::None);
            assert!(app.worker.is_none());
            app.last = Some(
                serde_json::json!({"status":"partial","results":[{"key":"novel_1004","status":"partial","omissions":[{"resource":"cover.png","reason":"Resource unavailable"}]}]}),
            );
            app.key(KeyEvent::new(KeyCode::F(3), KeyModifiers::NONE));
            assert!(app.summary().contains("未保存: cover.png"));
            t.draw(|f| draw(f, &app, false)).unwrap();
        }
    }
    #[test]
    fn pasted_sixteen_urls_validate_and_keep_job_bound() {
        let mut app = App::default();
        app.urls.insert(
            &(1..=16)
                .map(|i| format!("https://www.pixiv.net/artworks/{i}"))
                .collect::<Vec<_>>()
                .join("\n"),
            true,
        );
        let o = app.options().unwrap();
        assert_eq!(o.targets.len(), 16);
        assert_eq!(o.jobs, 2);
        app.focus = 2;
        for _ in 0..10 {
            app.key(KeyEvent::new(KeyCode::Right, KeyModifiers::NONE));
        }
        assert_eq!(app.jobs, 4);
    }
    #[test]
    fn demo_switch_keeps_online_urls_and_destination() {
        let mut app = App::default();
        app.urls = Editor::new("art:123\nnovel:456");
        app.out = Editor::new("D:/My archive");
        app.key(KeyEvent::new(KeyCode::F(2), KeyModifiers::NONE));
        assert_eq!(app.urls.value, "user:9001");
        app.key(KeyEvent::new(KeyCode::F(2), KeyModifiers::NONE));
        assert_eq!(app.urls.value, "art:123\nnovel:456");
        assert_eq!(app.out.value, "D:/My archive");
    }
    fn finish(app: &mut App) {
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while app.worker.is_some() {
            app.poll();
            assert!(
                std::time::Instant::now() < deadline,
                "TUI worker did not finish"
            );
            thread::sleep(Duration::from_millis(5));
        }
    }
    #[test]
    fn saved_and_resumed_rows_keep_actual_paths_after_settings_change_and_verify() {
        let temp = tempfile::tempdir().unwrap();
        let out = temp.path().join("保存先 空白");
        let mut app = App::default();
        app.demo = true;
        app.urls = Editor::new("user:9001");
        app.out = Editor::new(out.to_str().unwrap());
        app.start("fetch");
        finish(&mut app);
        assert_eq!(app.rows.len(), 4);
        assert!(app
            .rows
            .iter()
            .all(|r| r.status == "complete" && r.output.as_ref().is_some_and(|p| p.is_dir())));
        let actual = app.folder(true).unwrap();
        app.out = Editor::new("different-folder");
        assert_eq!(app.folder(true).unwrap(), actual);
        app.out = Editor::new(out.to_str().unwrap());
        app.start("verify");
        finish(&mut app);
        assert_eq!(app.rows.len(), 4);
        app.start("fetch");
        finish(&mut app);
        assert_eq!(app.rows.len(), 4);
        assert!(app
            .rows
            .iter()
            .all(|r| r.status == "already_complete" && r.output.is_some()));
        assert_eq!(app.last.as_ref().unwrap()["rate_wait_seconds"], 0.0);
        let mut terminal = Terminal::new(TestBackend::new(120, 36)).unwrap();
        terminal.draw(|f| draw(f, &app, false)).unwrap();
        let text = terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|c| c.symbol())
            .collect::<String>();
        assert!(text.contains("OUTPUT / PATH"));
        assert!(app.summary().contains("送信間隔の待機"));
    }
    #[test]
    fn mouse_row_selection_and_folder_buttons_use_same_layout_as_render() {
        let temp = tempfile::tempdir().unwrap();
        let mut app = App::default();
        app.out = Editor::new(temp.path().join("missing").to_str().unwrap());
        for i in 1..=12 {
            app.set_row(format!("art_{i}"), "planned".into());
        }
        let area = Rect::new(0, 0, 120, 36);
        let view = View::new(area).unwrap();
        let row = view.queue.inner(ratatui::layout::Margin::new(1, 1));
        app.mouse(
            MouseEvent {
                kind: MouseEventKind::Down(MouseButton::Left),
                column: row.x,
                row: row.y + 2,
                modifiers: KeyModifiers::NONE,
            },
            area,
        );
        assert_eq!(app.selected, 1);
        assert_eq!(app.focus, 3);
        app.key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
        assert_eq!(app.selected, 2);
        let (root, work) = buttons(view.footer);
        app.mouse(
            MouseEvent {
                kind: MouseEventKind::Down(MouseButton::Left),
                column: root.x,
                row: root.y,
                modifiers: KeyModifiers::NONE,
            },
            area,
        );
        assert!(app.logs.back().unwrap().contains("まだありません"));
        app.mouse(
            MouseEvent {
                kind: MouseEventKind::Down(MouseButton::Left),
                column: work.x,
                row: work.y,
                modifiers: KeyModifiers::NONE,
            },
            area,
        );
        assert!(app.logs.back().unwrap().contains("選択した作品"));
        assert!(
            WorkRow::new("art_../escape".into(), "complete".into(), Some(temp.path()))
                .output
                .is_none()
        );
    }
}

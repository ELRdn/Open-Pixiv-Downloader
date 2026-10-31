use crate::{
    archive::{safe_path, Archive},
    error::{invalid, Result},
    jobs::{self, Event, Options},
    target::Target,
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
    collections::VecDeque,
    io,
    path::PathBuf,
    sync::{
        atomic::{AtomicBool, Ordering},
        mpsc, Arc,
    },
    thread,
    time::Duration,
};
use unicode_width::UnicodeWidthStr;

const INK: Color = Color::Rgb(218, 232, 244);
const MUTED: Color = Color::Rgb(116, 143, 165);
const CYAN: Color = Color::Rgb(81, 225, 232);
const BLUE: Color = Color::Rgb(74, 117, 237);
const BG: Color = Color::Rgb(11, 17, 26);

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
    focus: usize,
    demo: bool,
    modal: Modal,
    modal_scroll: u16,
    worker: Option<Worker>,
    rows: Vec<WorkRow>,
    run_out: Option<PathBuf>,
    selected: usize,
    logs: VecDeque<String>,
    done: usize,
    total: usize,
    status: String,
    last: Option<Value>,
}
impl Default for App {
    fn default() -> Self {
        Self {
            urls: Editor::default(),
            out: Editor::new("archive"),
            other_urls: Editor::new("user:9001"),
            other_out: Editor::new("demo-archive"),
            jobs: 2,
            focus: 0,
            demo: false,
            modal: Modal::None,
            modal_scroll: 0,
            worker: None,
            rows: vec![],
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
        }
    }
}
impl App {
    fn set_row(&mut self, key: String, status: String) {
        let row = WorkRow::new(key, status, self.run_out.as_deref());
        if let Some(old) = self.rows.iter_mut().find(|r| r.key == row.key) {
            *old = row;
        } else {
            self.rows.push(row);
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
        if let Some(reason) = v["reason"].as_str() {
            lines.push(reason.into());
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
        clean_lines(&lines.join("\n"))
    }
    fn log(&mut self, message: String) {
        self.logs.push_back(clean(&message));
        while self.logs.len() > 100 {
            self.logs.pop_front();
        }
    }
    fn options(&self) -> Result<Options> {
        let values: Vec<_> = self
            .urls
            .value
            .split_whitespace()
            .filter(|v| !v.is_empty())
            .map(str::to_owned)
            .collect();
        if values.is_empty() || values.len() > 1000 {
            return Err(invalid("作品・作者URLを1〜1000件入力してください。"));
        }
        for v in &values {
            Target::parse(v)?;
        }
        if self.out.value.trim().is_empty() {
            return Err(invalid("保存先を入力してください。"));
        }
        Ok(Options {
            targets: values,
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
        let opts = if matches!(operation, "fetch" | "plan") {
            match self.options() {
                Ok(o) => o,
                Err(e) => {
                    self.log(e.to_string());
                    return;
                }
            }
        } else {
            Options {
                out: PathBuf::from(self.out.value.trim()),
                ..Default::default()
            }
        };
        let (tx, rx) = mpsc::channel();
        let stop = Arc::new(AtomicBool::new(false));
        let worker_stop = stop.clone();
        let op = operation.to_owned();
        if matches!(operation, "fetch" | "plan") {
            self.rows.clear();
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
                "fetch" | "plan" => jobs::run(opts, op == "plan", worker_stop, |e| {
                    let _ = tx.send(e);
                }),
                "verify" => Archive::open(&opts.out, true).and_then(|a| a.verify()),
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
                        for t in targets.iter().take(1000) {
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
                    self.last = Some(value);
                    finished = true;
                }
                Event::Failed(s) => {
                    self.status = "FAILED".into();
                    self.log(s);
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
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
            if self.worker.is_some() {
                self.cancel();
                return false;
            }
            return true;
        }
        if self.modal != Modal::None {
            match key.code {
                KeyCode::Esc => self.modal = Modal::None,
                KeyCode::Down | KeyCode::PageDown if self.modal == Modal::Result => {
                    self.modal_scroll = self
                        .modal_scroll
                        .saturating_add(if key.code == KeyCode::Down { 1 } else { 5 })
                }
                KeyCode::Up | KeyCode::PageUp if self.modal == Modal::Result => {
                    self.modal_scroll = self
                        .modal_scroll
                        .saturating_sub(if key.code == KeyCode::Up { 1 } else { 5 })
                }
                KeyCode::Enter if self.modal == Modal::Confirm => {
                    self.modal = Modal::None;
                    self.start("fetch");
                }
                _ => {}
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
                    Ok(_) => self.modal = Modal::Confirm,
                    Err(e) => self.log(e.to_string()),
                }
            }
            KeyCode::Char('v') if key.modifiers.contains(KeyModifiers::ALT) => self.start("verify"),
            KeyCode::Char('e') if key.modifiers.contains(KeyModifiers::ALT) => self.start("pack"),
            KeyCode::Char('l') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.logs.clear();
                self.rows.clear();
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
        if self.modal != Modal::None {
            return;
        }
        let Some(view) = View::new(area) else {
            return;
        };
        let point = ratatui::layout::Position::new(event.column, event.row);
        if event.kind == MouseEventKind::Down(MouseButton::Left) {
            let (root, work) = buttons(view.footer);
            if root.contains(point) {
                self.open_folder(false);
                return;
            }
            if work.contains(point) {
                self.open_folder(true);
                return;
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
                "  {}  ·  並行保存 {}作品  [← / →]",
                if app.demo {
                    "DEMO · OFFLINE"
                } else {
                    "ONLINE · ANONYMOUS"
                },
                app.jobs
            ))
            .block(panel("03  MODE / WORKERS", app.focus == 2)),
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
        let rows = app.rows.iter().map(|row| {
            Row::new([
                clean(&row.key),
                row.status.clone(),
                path_label(row.output.as_deref(), path_width),
            ])
            .style(Style::default().fg(match row.status.as_str() {
                "complete" | "already_complete" => CYAN,
                "saving" => BLUE,
                _ => Color::Yellow,
            }))
        });
        let visible = view.queue.height.saturating_sub(3) as usize;
        let mut state = TableState::default()
            .with_offset(app.table_offset(visible))
            .with_selected(Some(app.selected));
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
            .block(panel("ACTIVITY", false)),
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
    frame.render_widget(
        Gauge::default()
            .ratio(ratio)
            .gauge_style(
                Style::default()
                    .fg(if no_color { Color::White } else { BLUE })
                    .bg(if no_color { Color::Reset } else { BG }),
            )
            .label(format!(
                "{} · {} / {} · v{}",
                app.status,
                app.done,
                app.total,
                env!("CARGO_PKG_VERSION")
            )),
        foot[0],
    );
    frame.render_widget(
        Paragraph::new(
            "Tab 設定移動   F2 デモ   F3 結果   Ctrl+S 保存   F1 ヘルプ   Esc 停止/終了",
        )
        .style(Style::default().fg(MUTED)),
        foot[2],
    );
    if app.modal != Modal::None {
        let width = body.width.min(72);
        let height = body.height.min(match app.modal {
            Modal::Help => 20,
            Modal::Result => 24,
            _ => 12,
        });
        let popup = Rect::new(
            body.x + (body.width - width) / 2,
            body.y + (body.height - height) / 2,
            width,
            height,
        );
        frame.render_widget(Clear, popup);
        let text = if app.modal == Modal::Help {
            "URLを複数貼り付けて、Ctrl+S → Enterで保存します。\n作者URLは列挙できた全作品を対象にします。\n\nTab / Shift+Tab: 入力欄・作品一覧の移動\nF2: デモ ↔ 匿名オンライン（入力・保存先を切替）\nF3: 結果と欠損の詳細 / ↑↓でスクロール\nF5: 対象一覧を確認（媒体は保存しません）\nF6: 保存先を開く / F7: 選択作品を開く\nCtrl+S: 保存確認 / Enter: 開始\nAlt+V: 保存先を検査 / Alt+E: 保存先名-export.zipへ出力\n並行数欄: ← → または1〜4\n一覧で↑↓: 選択 / Enter: フォルダーを開く\nPgUp / PgDn: 5作品ずつ移動\nEsc / Ctrl+C: 実行中は停止、待機中は終了\n\n実サイト・Windows実機は未検証のα版です。\nEnter: URL改行 / Esc: この画面を閉じる".to_owned()
        } else if app.modal == Modal::Result {
            app.summary()
        } else {
            format!("{}\n\n入力: {}件（作者URLは作品一覧へ展開）\n保存先: {}\n並行保存: {}作品 / 対象作品数の選択上限なし\n\n{}\n\nEnter: 保存開始    Esc: 入力へ戻る",if app.demo{"合成データを保存します · 通信なし"}else{"Pixivへ匿名で接続します"},app.urls.value.split_whitespace().count(),clean(&app.out.value),app.jobs,if app.demo{"デモ保存は実作品の取得を検証するものではありません。"}else{"共有間隔2秒・要求予算500。拒否応答では全体停止。"})
        };
        frame.render_widget(
            Paragraph::new(text)
                .wrap(Wrap { trim: false })
                .scroll((
                    if app.modal == Modal::Result {
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
pub fn run() -> Result<()> {
    let no_color = std::env::var_os("NO_COLOR").is_some();
    let mut terminal = ratatui::init();
    // Ratatui's panic hook restores terminal state; the ordinary path also disables paste.
    let result = (|| -> Result<()> {
        let _input_modes = InputModes;
        execute!(io::stdout(), EnableBracketedPaste, EnableMouseCapture)?;
        let mut app = App::default();
        loop {
            app.poll();
            terminal.draw(|f| draw(f, &app, no_color))?;
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
                        match app.focus {
                            0 => app.urls.insert(&text, true),
                            1 => app.out.insert(&text, false),
                            _ => {}
                        }
                    }
                    _ => {}
                }
            }
        }
        if let Some(w) = app.worker.take() {
            w.stop.store(true, Ordering::SeqCst);
            let _ = w.handle.join();
        }
        Ok(())
    })();
    ratatui::restore();
    result
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

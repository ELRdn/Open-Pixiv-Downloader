use crate::{
    archive::Archive,
    error::{invalid, Result},
    jobs::{self, Event, Options},
    target::Target,
};
use crossterm::{
    event::{
        self, DisableBracketedPaste, EnableBracketedPaste, Event as InputEvent, KeyCode, KeyEvent,
        KeyEventKind, KeyModifiers,
    },
    execute,
};
use ratatui::{
    layout::{Alignment, Constraint, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, BorderType, Borders, Clear, Gauge, Paragraph, Row, Table, Wrap},
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
    rows: Vec<(String, String)>,
    logs: VecDeque<String>,
    done: usize,
    total: usize,
    status: String,
    last: Option<Value>,
    scroll: usize,
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
            logs: VecDeque::from([
                "Welcome to Open Pixiv Downloader!".into(),
                "作品・作者・小説シリーズのURLを貼り付けてください。".into(),
            ]),
            done: 0,
            total: 0,
            status: "READY".into(),
            last: None,
            scroll: 0,
        }
    }
}
impl App {
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
        self.rows.clear();
        self.last = None;
        self.done = 0;
        self.total = 0;
        self.scroll = 0;
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
                Event::Started(key) => self.rows.push((key, "saving".into())),
                Event::Progress {
                    key,
                    status,
                    done,
                    total,
                } => {
                    self.done = done;
                    self.total = total;
                    if let Some(row) = self.rows.iter_mut().find(|r| r.0 == key) {
                        row.1 = status.clone();
                    }
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
                            self.rows
                                .push((t.as_str().unwrap_or("").into(), "planned".into()));
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
        if key.code == KeyCode::Esc {
            if self.worker.is_some() {
                self.cancel();
                return false;
            }
            return true;
        }
        if key.code == KeyCode::PageDown {
            self.scroll = self
                .scroll
                .saturating_add(5)
                .min(self.rows.len().saturating_sub(1));
            return false;
        }
        if key.code == KeyCode::PageUp {
            self.scroll = self.scroll.saturating_sub(5);
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
            KeyCode::Tab => self.focus = (self.focus + 1) % 3,
            KeyCode::BackTab => self.focus = (self.focus + 2) % 3,
            KeyCode::Char('l') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.logs.clear();
                self.rows.clear();
            }
            _ => match self.focus {
                0 => self.urls.key(key, true),
                1 => self.out.key(key, false),
                _ => match key.code {
                    KeyCode::Left | KeyCode::Char('-') => {
                        self.jobs = self.jobs.saturating_sub(1).max(1)
                    }
                    KeyCode::Right | KeyCode::Char('+') => self.jobs = (self.jobs + 1).min(4),
                    KeyCode::Char(c) if ('1'..='4').contains(&c) => {
                        self.jobs = c.to_digit(10).unwrap() as usize
                    }
                    _ => {}
                },
            },
        }
        false
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
    let margin = if area.width >= 60 { 2 } else { 0 };
    let body = area.inner(ratatui::layout::Margin::new(margin, 1));
    if body.height < 10 || body.width < 32 {
        frame.render_widget(Paragraph::new("OPEN PIXIV DOWNLOADER\n\n端末を32桁×12行以上に広げてください。\nEsc / Ctrl+C: 終了").wrap(Wrap{trim:false}),body);
        return;
    }
    let large = body.height >= 23 && body.width >= 48;
    let chunks = Layout::vertical([
        Constraint::Length(if large { 8 } else { 2 }),
        Constraint::Min(7),
        Constraint::Length(2),
    ])
    .split(body);
    if large {
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
        frame.render_widget(Paragraph::new(lines), chunks[0]);
    } else {
        frame.render_widget(
            Paragraph::new("OPEN PIXIV DOWNLOADER · Rust + Ratatui")
                .style(Style::default().fg(CYAN).add_modifier(Modifier::BOLD)),
            chunks[0],
        );
    }
    let main = if body.width >= 96 {
        Layout::horizontal([Constraint::Percentage(57), Constraint::Percentage(43)])
            .split(chunks[1])
    } else {
        Layout::vertical([Constraint::Percentage(58), Constraint::Percentage(42)]).split(chunks[1])
    };
    let fields = Layout::vertical([
        Constraint::Min(3),
        Constraint::Length(if main[0].height >= 12 { 6 } else { 3 }),
    ])
    .split(main[0]);
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
    let queue = Layout::vertical([
        Constraint::Min(3),
        Constraint::Length(if main[1].height >= 14 { 5 } else { 3 }),
    ])
    .split(main[1]);
    if app.rows.is_empty() {
        frame.render_widget(
            Paragraph::new(vec![
                Line::from(""),
                Line::from("  Welcome to Open Pixiv Downloader!"),
                Line::from("  F2: デモの4作品で試す"),
                Line::from("  URL入力 → Ctrl+S: 保存を確認"),
                Line::from("  F5: 対象一覧 / Alt+V: 検査"),
                Line::from("  Alt+E: ZIP出力 / F1: ヘルプ"),
            ])
            .block(panel("QUEUE / LIBRARY", false)),
            queue[0],
        );
    } else {
        let rows = app.rows.iter().skip(app.scroll).map(|(key, status)| {
            Row::new([clean(key), status.clone()]).style(Style::default().fg(
                match status.as_str() {
                    "complete" | "already_complete" => CYAN,
                    "saving" => BLUE,
                    _ => Color::Yellow,
                },
            ))
        });
        frame.render_widget(
            Table::new(
                rows,
                [Constraint::Percentage(54), Constraint::Percentage(46)],
            )
            .header(Row::new(["WORK", "STATUS"]).style(Style::default().fg(MUTED)))
            .block(panel("QUEUE · PgUp / PgDn", false)),
            queue[0],
        );
    }
    let available = queue[1].height.saturating_sub(2) as usize;
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
        queue[1],
    );
    let foot = Layout::vertical([Constraint::Length(1), Constraint::Length(1)]).split(chunks[2]);
    let ratio = if app.total > 0 {
        (app.done as f64 / app.total as f64).min(1.0)
    } else {
        0.0
    };
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
        foot[1],
    );
    if app.modal != Modal::None {
        let width = body.width.min(72);
        let height = body.height.min(match app.modal {
            Modal::Help => 18,
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
            "URLを複数貼り付けて、Ctrl+S → Enterで保存します。\n作者URLは列挙できた全作品を対象にします。\n\nTab / Shift+Tab: 入力欄の移動\nF2: デモ ↔ 匿名オンライン（入力・保存先を切替）\nF3: 結果と欠損の詳細 / ↑↓でスクロール\nF5: 対象一覧を確認（媒体は保存しません）\nCtrl+S: 保存確認 / Enter: 開始\nAlt+V: 保存先を検査 / Alt+E: 保存先名-export.zipへ出力\n並行数欄: ← → または1〜4\nPgUp / PgDn: 作品一覧のスクロール\nEsc / Ctrl+C: 実行中は停止、待機中は終了\n\n実サイト・Windows実機は未検証のα版です。\nEnter: URL改行 / Esc: この画面を閉じる".to_owned()
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
        execute!(io::stdout(), EnableBracketedPaste)?;
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
    let _ = execute!(io::stdout(), DisableBracketedPaste);
    ratatui::restore();
    result
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
}

use super::{clean, panel, Editor, Language, CYAN, INK, MUTED};
use crate::{
    conversion::{self, Format, Library, Plan, Report},
    error::{invalid, Result},
};
use crossterm::event::{
    KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};
use ratatui::{
    layout::{Position, Rect},
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{Clear, Paragraph, Wrap},
    Frame,
};
use std::{
    collections::BTreeSet,
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, AtomicUsize, Ordering},
        Arc,
    },
    thread::{self, JoinHandle},
};
use unicode_width::UnicodeWidthStr;

#[derive(Default)]
struct Paths {
    source: Editor,
    destination: Editor,
}

enum Work {
    Preview(Plan),
    Finished(Report),
}

struct Task {
    handle: JoinHandle<Result<Work>>,
    stop: Arc<AtomicBool>,
    done: Arc<AtomicUsize>,
    total: Option<usize>,
    preparing: bool,
}

enum State {
    Form,
    Review(Plan),
    Busy(Task),
    Finished(Report),
}

#[derive(Clone, Copy, PartialEq)]
enum Field {
    Action,
    SourceMode,
    Library,
    Source,
    Range,
    Format,
    Destination,
}

enum SavedFiles {
    Loading {
        handle: JoinHandle<Result<Library>>,
        stop: Arc<AtomicBool>,
    },
    Ready(Arc<Library>),
    Failed(String),
}

pub(super) struct Conversion {
    archive_root: PathBuf,
    library: SavedFiles,
    from_library: bool,
    selected: BTreeSet<usize>,
    library_cursor: usize,
    range_anchor: Option<(usize, BTreeSet<usize>)>,
    export: Paths,
    restore: Paths,
    restoring: bool,
    range: Editor,
    format: usize,
    field: Field,
    state: State,
    error: Option<String>,
    scroll: u16,
}

impl Conversion {
    pub(super) fn new(archive_root: PathBuf, source: PathBuf) -> Self {
        let library = Self::load_library(&archive_root);
        Self {
            archive_root,
            library,
            from_library: true,
            selected: BTreeSet::new(),
            library_cursor: 0,
            range_anchor: None,
            export: Paths {
                source: Editor::new(&source.to_string_lossy()),
                destination: Editor::default(),
            },
            restore: Paths::default(),
            restoring: false,
            range: Editor::default(),
            format: 0,
            field: Field::Action,
            state: State::Form,
            error: None,
            scroll: 0,
        }
    }

    fn load_library(root: &Path) -> SavedFiles {
        let root = root.to_owned();
        let stop = Arc::new(AtomicBool::new(false));
        let worker_stop = stop.clone();
        let handle =
            thread::spawn(move || Library::read(&root, &|| worker_stop.load(Ordering::Relaxed)));
        SavedFiles::Loading { handle, stop }
    }

    pub(super) fn uses_library(&self) -> bool {
        !self.restoring && self.from_library
    }

    pub(super) fn library_focused(&self) -> bool {
        self.uses_library() && self.field == Field::Library && matches!(self.state, State::Form)
    }

    fn fields(&self) -> &[Field] {
        use Field::*;
        if self.restoring {
            &[Action, Source, Destination]
        } else if self.from_library {
            &[Action, SourceMode, Library, Format, Destination]
        } else {
            &[Action, SourceMode, Source, Range, Format, Destination]
        }
    }

    fn paths(&self) -> &Paths {
        if self.restoring {
            &self.restore
        } else {
            &self.export
        }
    }

    fn editor(&mut self) -> Option<&mut Editor> {
        let paths = if self.restoring {
            &mut self.restore
        } else {
            &mut self.export
        };
        match self.field {
            Field::Source => Some(&mut paths.source),
            Field::Destination => Some(&mut paths.destination),
            Field::Range => Some(&mut self.range),
            _ => None,
        }
    }

    pub(super) fn paste(&mut self, text: &str) {
        if matches!(self.state, State::Form) {
            self.error = None;
            if let Some(editor) = self.editor() {
                editor.insert(text.trim().trim_matches('"'), false);
            }
        }
    }

    fn start_task(
        &mut self,
        preparing: bool,
        total: Option<usize>,
        operation: impl FnOnce(&AtomicBool, &AtomicUsize) -> Result<Work> + Send + 'static,
    ) {
        let stop = Arc::new(AtomicBool::new(false));
        let done = Arc::new(AtomicUsize::new(0));
        let task_stop = stop.clone();
        let task_done = done.clone();
        self.scroll = 0;
        self.state = State::Busy(Task {
            handle: thread::spawn(move || operation(&task_stop, &task_done)),
            stop,
            done,
            total,
            preparing,
        });
    }

    fn preview(&mut self, language: Language) {
        let paths = self.paths();
        let destination = optional_path(&paths.destination.value);
        let format = Format::ALL[self.format];
        if self.uses_library() {
            let SavedFiles::Ready(library) = &self.library else {
                self.error = Some(
                    language
                        .text(
                            "Saved TXT is unavailable. Try Folder / TXT.",
                            "保存済みTXTを読み込めません。フォルダー / TXTを選べます。",
                        )
                        .into(),
                );
                return;
            };
            if self.selected.is_empty() {
                self.error = Some(
                    language
                        .text(
                            "Select TXT in the library first.",
                            "一覧から変換するTXTを選んでください。",
                        )
                        .into(),
                );
                self.field = Field::Library;
                return;
            }
            let library = library.clone();
            let selected: Vec<_> = self.selected.iter().copied().collect();
            self.error = None;
            self.start_task(true, None, move |_, _| {
                library
                    .exporting(&selected, destination.as_deref(), format)
                    .map(Work::Preview)
            });
            return;
        }
        let source = paths.source.value.trim();
        if source.is_empty() {
            self.field = Field::Source;
            self.error = Some(
                language
                    .text("Enter a source path.", "入力元のパスを指定してください。")
                    .into(),
            );
            return;
        }
        let source = PathBuf::from(source);
        let restoring = self.restoring;
        let range = self.range.value.trim();
        let range = if restoring || range.is_empty() || range.eq_ignore_ascii_case("all") {
            None
        } else {
            match range.parse() {
                Ok(range) => Some(range),
                Err(_) => {
                    self.field = Field::Range;
                    self.error = Some(
                        language
                            .text(
                                "Use a number or range like 2-10.",
                                "番号または 2-10 のような範囲を入力してください。",
                            )
                            .into(),
                    );
                    return;
                }
            }
        };
        self.error = None;
        self.start_task(true, None, move |stop, _| {
            let plan = if restoring {
                Plan::restoring(&source, destination.as_deref())?
            } else {
                Plan::exporting(&source, destination.as_deref(), format, range, &|| {
                    stop.load(Ordering::Relaxed)
                })?
            };
            Ok(Work::Preview(plan))
        });
    }

    fn execute(&mut self) {
        let State::Review(plan) = std::mem::replace(&mut self.state, State::Form) else {
            return;
        };
        let total = plan.file_count();
        self.start_task(false, total, move |stop, done| {
            plan.execute(&|| stop.load(Ordering::Relaxed), |n| {
                done.store(n, Ordering::Relaxed)
            })
            .map(Work::Finished)
        });
    }

    pub(super) fn poll(&mut self, language: Language) -> bool {
        let library_updated =
            matches!(&self.library, SavedFiles::Loading { handle, .. } if handle.is_finished());
        if library_updated {
            let SavedFiles::Loading { handle, .. } =
                std::mem::replace(&mut self.library, SavedFiles::Failed(String::new()))
            else {
                unreachable!()
            };
            self.library = match handle.join() {
                Ok(Ok(library)) => SavedFiles::Ready(Arc::new(library)),
                Ok(Err(error)) => SavedFiles::Failed(error.to_string()),
                Err(_) => SavedFiles::Failed(
                    language
                        .text("Could not load saved TXT.", "保存済みTXTを読み込めません。")
                        .into(),
                ),
            };
        }
        let State::Busy(task) = &self.state else {
            return library_updated;
        };
        if !task.handle.is_finished() {
            return true;
        }
        let State::Busy(task) = std::mem::replace(&mut self.state, State::Form) else {
            unreachable!()
        };
        let cancelled = task.stop.load(Ordering::Relaxed);
        let result = task
            .handle
            .join()
            .unwrap_or_else(|_| Err(invalid("TXT conversion worker stopped unexpectedly")));
        match result {
            Ok(Work::Finished(report)) => self.state = State::Finished(report),
            Ok(Work::Preview(plan)) if !cancelled => self.state = State::Review(plan),
            _ if cancelled => {
                self.error = Some(language.text("Cancelled.", "中止しました。").into())
            }
            Err(error) => self.error = Some(error.to_string()),
            _ => {}
        }
        true
    }

    pub(super) fn key(&mut self, key: KeyEvent, language: Language) -> bool {
        let control = key.modifiers.contains(KeyModifiers::CONTROL);
        let alt = key.modifiers.contains(KeyModifiers::ALT);
        let back = key.code == KeyCode::Esc || (control && key.code == KeyCode::Char('c'));
        let action = key.code == KeyCode::Enter || (control && key.code == KeyCode::Char('s'));
        let toggle = alt && key.code == KeyCode::Char('c');
        match &self.state {
            State::Busy(task) => {
                if back {
                    task.stop.store(true, Ordering::Relaxed);
                }
            }
            State::Review(_) | State::Finished(_) => {
                if toggle {
                    return true;
                }
                match key.code {
                    KeyCode::Down | KeyCode::PageDown => {
                        self.scroll = self.scroll.saturating_add(1)
                    }
                    KeyCode::Up | KeyCode::PageUp => self.scroll = self.scroll.saturating_sub(1),
                    _ if back => self.state = State::Form,
                    _ if action && key.kind == KeyEventKind::Press => {
                        if matches!(self.state, State::Review(_)) {
                            self.execute();
                        } else {
                            self.state = State::Form;
                        }
                    }
                    _ => {}
                }
            }
            State::Form => {
                if back || toggle {
                    return true;
                }
                if action && key.kind == KeyEventKind::Press {
                    self.preview(language);
                } else if self.library_focused() && self.library_key(key) {
                } else if matches!(
                    key.code,
                    KeyCode::Tab | KeyCode::BackTab | KeyCode::Up | KeyCode::Down
                ) {
                    let fields = self.fields();
                    let position = fields
                        .iter()
                        .position(|field| *field == self.field)
                        .unwrap_or(0);
                    let step = if matches!(key.code, KeyCode::BackTab | KeyCode::Up) {
                        fields.len() - 1
                    } else {
                        1
                    };
                    self.field = fields[(position + step) % fields.len()];
                } else if matches!(
                    key.code,
                    KeyCode::Left | KeyCode::Right | KeyCode::Char(' ')
                ) && !control
                    && !alt
                {
                    match self.field {
                        Field::Action => self.restoring = !self.restoring,
                        Field::SourceMode => self.toggle_source(),
                        Field::Format => self.cycle_format(key.code == KeyCode::Left),
                        _ => self.edit(key),
                    }
                } else if !alt {
                    self.edit(key);
                }
            }
        }
        false
    }

    fn toggle_source(&mut self) {
        self.from_library = !self.from_library;
        self.error = None;
    }

    fn library_len(&self) -> usize {
        match &self.library {
            SavedFiles::Ready(library) => library.len(),
            _ => 0,
        }
    }

    fn library_key(&mut self, key: KeyEvent) -> bool {
        if key.code == KeyCode::F(5) {
            if !matches!(self.library, SavedFiles::Loading { .. }) {
                self.library = Self::load_library(&self.archive_root);
                self.selected.clear();
                self.library_cursor = 0;
                self.range_anchor = None;
                self.error = None;
            }
            return true;
        }
        let len = self.library_len();
        let cursor = self.library_cursor;
        let extend = key.modifiers.contains(KeyModifiers::SHIFT);
        match key.code {
            KeyCode::Up => self.move_library(cursor.saturating_sub(1), extend),
            KeyCode::Down => self.move_library(cursor + 1, extend),
            KeyCode::PageUp => self.move_library(cursor.saturating_sub(10), extend),
            KeyCode::PageDown => self.move_library(cursor + 10, extend),
            KeyCode::Home => self.move_library(0, extend),
            KeyCode::End => self.move_library(len.saturating_sub(1), extend),
            KeyCode::Char(' ') if len > 0 && key.kind == KeyEventKind::Press => {
                self.toggle_selected(cursor)
            }
            KeyCode::Char('a') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.selected = (0..len).collect();
                self.range_anchor = None;
                self.error = None;
            }
            KeyCode::Char(' ') => {}
            _ => return false,
        }
        true
    }

    fn move_library(&mut self, index: usize, extend: bool) {
        let len = self.library_len();
        if len == 0 {
            return;
        }
        let index = index.min(len - 1);
        if extend {
            let (anchor, base) = self
                .range_anchor
                .get_or_insert_with(|| (self.library_cursor, self.selected.clone()));
            self.selected.clone_from(base);
            self.selected
                .extend((*anchor).min(index)..=(*anchor).max(index));
            self.error = None;
        } else {
            self.range_anchor = None;
        }
        self.library_cursor = index;
    }

    fn toggle_selected(&mut self, index: usize) {
        if !self.selected.remove(&index) {
            self.selected.insert(index);
        }
        self.range_anchor = None;
        self.error = None;
    }

    pub(super) fn library_mouse(&mut self, event: MouseEvent, area: Rect, compact: bool) {
        if !matches!(self.state, State::Form) {
            return;
        }
        let rows = library_rows(area);
        let point = Position::new(event.column, event.row);
        match event.kind {
            MouseEventKind::Down(MouseButton::Left)
                if compact
                    && inner(area).contains(point)
                    && event.row == inner(area).bottom() - 1 =>
            {
                self.field = Field::Format;
            }
            MouseEventKind::Down(MouseButton::Left) if rows.contains(point) => {
                let index = self.library_offset(rows.height) + (event.row - rows.y) as usize;
                if index < self.library_len() {
                    self.field = Field::Library;
                    let extend = event.modifiers.contains(KeyModifiers::SHIFT);
                    self.move_library(index, extend);
                    if !extend {
                        self.toggle_selected(index);
                    }
                }
            }
            MouseEventKind::ScrollDown => self.move_library(self.library_cursor + 1, false),
            MouseEventKind::ScrollUp => {
                self.move_library(self.library_cursor.saturating_sub(1), false)
            }
            _ => {}
        }
    }

    fn library_offset(&self, visible: u16) -> usize {
        self.library_cursor
            .saturating_sub(usize::from(visible.saturating_sub(1)))
    }

    pub(super) fn draw_library(&self, frame: &mut Frame, area: Rect, language: Language) {
        frame.render_widget(Clear, area);
        frame.render_widget(
            panel(
                language.text("QUEUE / LIBRARY · Saved TXT", "キュー / 一覧 · 保存済みTXT"),
                self.library_focused(),
            ),
            area,
        );
        let rows = library_rows(area);
        let inside = inner(area);
        let ready = match &self.library {
            SavedFiles::Ready(library) if library.len() > 0 => Some(library),
            _ => None,
        };
        if let Some(library) = ready {
            let header = Rect::new(inside.x, inside.y, inside.width, inside.height.min(1));
            frame.render_widget(
                Paragraph::new(language.text("SEL  TXT FILE", "選択 TXTファイル"))
                    .style(Style::default().fg(MUTED)),
                header,
            );
            let offset = self.library_offset(rows.height);
            for (line, name) in library
                .names()
                .skip(offset)
                .take(rows.height as usize)
                .enumerate()
            {
                let index = offset + line;
                let checked = self.selected.contains(&index);
                let field = Rect::new(rows.x, rows.y + line as u16, rows.width, 1);
                let style = if self.library_focused() && index == self.library_cursor {
                    Style::default().bg(super::BLUE).fg(INK)
                } else {
                    Style::default().fg(INK)
                };
                let entries = Line::from(vec![
                    Span::styled(
                        if checked { "[x]  " } else { "[ ]  " },
                        if checked {
                            style.add_modifier(Modifier::BOLD)
                        } else {
                            style
                        },
                    ),
                    Span::raw(clean(name)),
                ]);
                frame.render_widget(Paragraph::new(entries).style(style), field);
            }
        } else {
            let message = match &self.library {
                SavedFiles::Loading { .. } => language
                    .text("Loading saved TXT…", "保存済みTXTを読み込んでいます…")
                    .into(),
                SavedFiles::Ready(_) => language
                    .text(
                        "No saved TXT. You can also use Folder / TXT.",
                        "保存済みTXTがありません。フォルダー / TXTも使えます。",
                    )
                    .into(),
                SavedFiles::Failed(error) => tr_format!(
                    language,
                    "Saved TXT unavailable: {}",
                    "保存済みTXTを読み込めません: {}",
                    clean(error)
                ),
            };
            let message_area = Rect {
                height: inside.height.saturating_sub(1),
                ..inside
            };
            frame.render_widget(
                Paragraph::new(message)
                    .wrap(Wrap { trim: false })
                    .style(Style::default().fg(MUTED)),
                message_area,
            );
        }
        let footer = Rect::new(
            inside.x,
            inside.bottom().saturating_sub(1),
            inside.width,
            inside.height.min(1),
        );
        let text = if inside.width < 48 {
            tr_format!(
                language,
                "{} selected · Space · Tab",
                "{}件選択 · Space · Tab",
                self.selected.len()
            )
        } else {
            tr_format!(
                language,
                "{} selected · Space Toggle · Shift+↑↓ Range · Ctrl+A All · F5 Refresh",
                "{}件選択 · Space 切替 · Shift+↑↓ 範囲 · Ctrl+A 全選択 · F5 更新",
                self.selected.len()
            )
        };
        frame.render_widget(
            Paragraph::new(text).style(Style::default().fg(CYAN)),
            footer,
        );
    }

    fn cycle_format(&mut self, previous: bool) {
        self.format =
            (self.format + if previous { Format::ALL.len() - 1 } else { 1 }) % Format::ALL.len();
    }

    fn edit(&mut self, key: KeyEvent) {
        if let Some(editor) = self.editor() {
            editor.key(key, false);
            self.error = None;
        }
    }

    pub(super) fn mouse(&mut self, event: MouseEvent, area: Rect, language: Language) {
        let inner = inner(area);
        let point = Position::new(event.column, event.row);
        if !inner.contains(point) {
            return;
        }
        match event.kind {
            MouseEventKind::Down(MouseButton::Left) if event.row == inner.bottom() - 1 => {
                self.key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE), language);
            }
            MouseEventKind::Down(MouseButton::Left) if matches!(self.state, State::Form) => {
                if let Some(field) = self.fields().get((event.row - inner.y) as usize).copied() {
                    self.field = field;
                    match field {
                        Field::Action => self.restoring = !self.restoring,
                        Field::SourceMode => self.toggle_source(),
                        Field::Format => self.cycle_format(false),
                        _ => {}
                    }
                }
            }
            MouseEventKind::ScrollUp => self.scroll = self.scroll.saturating_sub(1),
            MouseEventKind::ScrollDown => self.scroll = self.scroll.saturating_add(1),
            _ => {}
        }
    }

    pub(super) fn draw(&self, frame: &mut Frame, area: Rect, language: Language) {
        frame.render_widget(Clear, area);
        frame.render_widget(
            panel(
                language.text(" TXT tools · Offline ", " TXT変換 · オフライン "),
                true,
            ),
            area,
        );
        let inner = inner(area);
        let content = Rect {
            height: inner.height.saturating_sub(1),
            ..inner
        };
        let action = match &self.state {
            State::Form => {
                self.draw_form(frame, content, language);
                language.text("[Enter Preview]  Esc Back", "[Enter 確認]  Esc 戻る")
            }
            State::Review(plan) => {
                let mut lines = Vec::new();
                if let Some((first, last, all)) = plan.range {
                    lines.push(tr_format!(
                        language,
                        "Convert {} TXT · #{}–{} / {}",
                        "{}件を変換 · #{}–{} / {}",
                        last - first + 1,
                        first,
                        last,
                        all
                    ));
                } else if let Some(count) = plan.file_count() {
                    lines.push(tr_format!(
                        language,
                        "Convert {count} selected TXT",
                        "選んだTXT {count}件を変換"
                    ));
                } else {
                    lines.push(
                        language
                            .text(
                                "Restore TXT into a new folder",
                                "新しいフォルダーへTXTを復元",
                            )
                            .into(),
                    );
                }
                lines.push(tr_format!(
                    language,
                    "Format: {} · {} bytes",
                    "形式: {} · {} バイト",
                    plan.format.extension(),
                    plan.input_bytes
                ));
                if let Some((first, last)) = plan.edges() {
                    lines.push(tr_format!(language, "First: {}", "先頭: {}", clean(first)));
                    lines.push(tr_format!(language, "Last:  {}", "末尾: {}", clean(last)));
                }
                lines.push(tr_format!(
                    language,
                    "Source: {}",
                    "入力元: {}",
                    shown_path(&plan.source)
                ));
                lines.push(tr_format!(
                    language,
                    "Output: {}",
                    "出力先: {}",
                    shown_path(&plan.destination)
                ));
                if plan.range.is_none() {
                    lines.extend(
                        plan.names()
                            .take(12)
                            .map(|name| format!("  {}", clean(name))),
                    );
                    if plan.file_count().is_some_and(|count| count > 12) {
                        lines.push(
                            language
                                .text("  … more selected files", "  … ほか選択済みファイル")
                                .into(),
                        );
                    }
                }
                draw_details(frame, content, lines, self.scroll);
                language.text("[Enter Run]  Esc Edit  ↑↓", "[Enter 実行]  Esc 修正  ↑↓")
            }
            State::Busy(task) => {
                let count = task.done.load(Ordering::Relaxed);
                let text = if task.stop.load(Ordering::Relaxed) {
                    language.text("Stopping…", "中止しています…").into()
                } else if task.preparing {
                    language
                        .text("Preparing preview…", "対象を確認しています…")
                        .into()
                } else if let Some(total) = task.total {
                    tr_format!(
                        language,
                        "Converting… {count}/{total}",
                        "変換中… {count}/{total}"
                    )
                } else {
                    tr_format!(language, "Restoring… {count} TXT", "復元中… TXT {count}件")
                };
                draw_details(frame, content, vec![text], 0);
                language.text("Esc Cancel", "Esc 中止")
            }
            State::Finished(report) => {
                let lines = vec![
                    tr_format!(
                        language,
                        "Completed · {} TXT",
                        "完了 · TXT {}件",
                        report.files
                    ),
                    tr_format!(
                        language,
                        "{} → {} bytes",
                        "{} → {} バイト",
                        report.input_bytes,
                        report.output_bytes
                    ),
                    tr_format!(
                        language,
                        "Output: {}",
                        "出力先: {}",
                        shown_path(&report.destination)
                    ),
                ];
                draw_details(frame, content, lines, self.scroll);
                language.text(
                    "[Enter New task]  Alt+C Download",
                    "[Enter 次の処理]  Alt+C ダウンロード",
                )
            }
        };
        let row = Rect::new(
            inner.x,
            inner.bottom().saturating_sub(1),
            inner.width,
            inner.height.min(1),
        );
        frame.render_widget(
            Paragraph::new(action).style(Style::default().fg(CYAN).add_modifier(Modifier::BOLD)),
            row,
        );
    }

    fn draw_form(&self, frame: &mut Frame, area: Rect, language: Language) {
        let paths = self.paths();
        let format = Format::ALL[self.format];
        let source = if self.uses_library() {
            &self.archive_root
        } else {
            Path::new(paths.source.value.trim())
        };
        let detected = if self.restoring {
            Format::detect(source).ok()
        } else {
            Some(format)
        };
        let placeholder = detected
            .and_then(|format| conversion::default_destination(source, format, self.restoring).ok())
            .map_or_else(
                || language.text("Automatic", "自動").into(),
                |path| shown_path(&path),
            );
        let selected = tr_format!(
            language,
            "{} / {} TXT selected",
            "TXT {} / {} 件を選択",
            self.selected.len(),
            self.library_len()
        );
        for (i, field) in self.fields().iter().enumerate() {
            if i >= area.height as usize {
                break;
            }
            let (label, editor, value) = match field {
                Field::Action => (
                    language.text("Action", "操作"),
                    None,
                    if self.restoring {
                        language.text("‹ Restore TXT ›", "‹ TXTへ復元 ›")
                    } else {
                        language.text("‹ Convert TXT ›", "‹ TXTを変換 ›")
                    },
                ),
                Field::SourceMode => (
                    language.text("Source", "入力方法"),
                    None,
                    if self.from_library {
                        language.text("‹ Saved library ›", "‹ 保存済み一覧 ›")
                    } else {
                        language.text("‹ Folder / TXT ›", "‹ フォルダー / TXT ›")
                    },
                ),
                Field::Library => (language.text("Target", "対象"), None, selected.as_str()),
                Field::Source => (
                    language.text("Source", "入力元"),
                    Some(&paths.source),
                    if self.restoring {
                        ".7z / .tar.zst / .parquet"
                    } else {
                        language.text("TXT file or folder", "TXTファイルかフォルダー")
                    },
                ),
                Field::Range => (
                    language.text("Range", "範囲"),
                    Some(&self.range),
                    language.text("All · or e.g. 2-10", "すべて · 例 2-10"),
                ),
                Field::Format => (
                    language.text("Format", "形式"),
                    None,
                    match format {
                        Format::SevenZip => {
                            language.text("‹ 7z · High compression ›", "‹ 7z · 高圧縮 ›")
                        }
                        Format::TarZst => language.text("‹ tar.zst · Fast ›", "‹ tar.zst · 高速 ›"),
                        Format::Parquet => {
                            language.text("‹ Parquet · Dataset ›", "‹ Parquet · データセット ›")
                        }
                    },
                ),
                Field::Destination => (
                    language.text("Output", "出力先"),
                    Some(&paths.destination),
                    placeholder.as_str(),
                ),
            };
            let focused = self.field == *field;
            let y = area.y + i as u16;
            let value_x = (area.x + 9).min(area.right());
            let label_area = Rect::new(area.x, y, value_x - area.x, 1);
            let value_area = Rect::new(value_x, y, area.right() - value_x, 1);
            frame.render_widget(
                Paragraph::new(label).style(Style::default().fg(if focused {
                    CYAN
                } else {
                    MUTED
                })),
                label_area,
            );
            let display = editor
                .filter(|editor| !editor.value.is_empty())
                .map(|editor| {
                    if focused {
                        let (text, x, _) = editor.visible(value_area.width as usize, 1);
                        (text, x)
                    } else {
                        (editor.value.clone(), 0)
                    }
                });
            let (text, cursor) = display
                .as_ref()
                .map_or((value, 0), |(text, cursor)| (text.as_str(), *cursor));
            frame.render_widget(
                Paragraph::new(text).style(Style::default().fg(
                    if display.is_none() && editor.is_some() {
                        MUTED
                    } else {
                        INK
                    },
                )),
                value_area,
            );
            if focused && editor.is_some() && value_area.width > 0 {
                frame.set_cursor_position((value_x + cursor, y));
            }
        }
        let count = self.fields().len() as u16;
        if count < area.height {
            let hints = self
                .error
                .as_deref()
                .unwrap_or(language.text("Tab Fields · ←/→ Options", "Tab 項目移動 · ←/→ 選択"));
            let rect = Rect::new(area.x, area.y + count, area.width, area.height - count);
            frame.render_widget(
                Paragraph::new(clean(hints))
                    .wrap(Wrap { trim: false })
                    .style(Style::default().fg(if self.error.is_some() { CYAN } else { MUTED })),
                rect,
            );
        }
    }
}

impl Drop for Conversion {
    fn drop(&mut self) {
        if let State::Busy(task) = std::mem::replace(&mut self.state, State::Form) {
            task.stop.store(true, Ordering::Relaxed);
            let _ = task.handle.join();
        }
        if let SavedFiles::Loading { handle, stop } =
            std::mem::replace(&mut self.library, SavedFiles::Failed(String::new()))
        {
            stop.store(true, Ordering::Relaxed);
            let _ = handle.join();
        }
    }
}

fn inner(area: Rect) -> Rect {
    area.inner(ratatui::layout::Margin::new(1, 1))
}

fn library_rows(area: Rect) -> Rect {
    let inner = inner(area);
    Rect::new(
        inner.x,
        (inner.y + 1).min(inner.bottom()),
        inner.width,
        inner.height.saturating_sub(2),
    )
}

fn optional_path(value: &str) -> Option<PathBuf> {
    let value = value.trim();
    (!value.is_empty()).then(|| PathBuf::from(value))
}

fn shown_path(path: &Path) -> String {
    let value = path.to_string_lossy();
    let shown = if let Some(path) = value.strip_prefix(r"\\?\UNC\") {
        format!(r"\\{path}")
    } else {
        value.strip_prefix(r"\\?\").unwrap_or(&value).to_owned()
    };
    clean(&shown)
}

fn draw_details(frame: &mut Frame, area: Rect, lines: Vec<String>, scroll: u16) {
    // Keep the action line visible even when long paths need several rows.
    let width = area.width.max(1) as usize;
    let rows: usize = lines
        .iter()
        .map(|line| UnicodeWidthStr::width(line.as_str()).div_ceil(width).max(1))
        .sum();
    let max_scroll = rows
        .saturating_sub(area.height as usize)
        .min(u16::MAX as usize) as u16;
    let text: Vec<Line> = lines
        .into_iter()
        .map(|line| Line::from(Span::raw(line)))
        .collect();
    frame.render_widget(
        Paragraph::new(text)
            .wrap(Wrap { trim: false })
            .scroll((scroll.min(max_scroll), 0)),
        area,
    );
}

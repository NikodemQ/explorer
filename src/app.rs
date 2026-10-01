//! Interaction state on top of the [`Tree`]: modes, key handling and commands.

use std::{
    collections::BTreeSet,
    fs,
    ops::RangeInclusive,
    path::{Path, PathBuf},
    sync::Arc,
};

use ratatui::crossterm::event::KeyCode;

use crate::{
    editor::{EditEvent, Editor},
    excmd::{self, Ex},
    fileops::{FileOps, Finished, Job, JobSpec, PasteFlow, Step},
    imageview::Painter,
    jumps::Jumps,
    keys::{Command, Fed, InputState, Key, Keymap, Operator},
    lineedit::LineEditor,
    marks::Marks,
    model::{Effect, Tree},
    motion,
    ops::{Choice, NoTrash, Op, Outcome, PasteItem, PasteMode, Trasher, describe, plan_paste},
    prompt::{Prompt, PromptEvent, PromptKind},
    save, search,
};

/// How the session ended. Only `Quit` should carry the shell to the last directory.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Exit {
    Quit,
    Abort,
}

/// What a key press asks the runtime to do besides redrawing.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Response {
    pub effect: Option<Effect>,
    pub exit: Option<Exit>,
}

enum Mode {
    Normal,
    /// Selecting a range of entries in the focused column, from `anchor` to the cursor.
    Visual {
        anchor: usize,
    },
    Prompt(Prompt),
    /// A paste is waiting for the answer to a name clash.
    Conflict {
        flow: PasteFlow,
        item: PasteItem,
    },
    /// A read-only list drawn over the tree, such as the key reference.
    Overlay {
        title: String,
        lines: Vec<String>,
        scroll: usize,
    },
    /// The file under the cursor is open in the built-in editor, which takes every key.
    Edit(Box<Editor>),
}

pub struct OverlayView<'a> {
    pub title: &'a str,
    pub lines: &'a [String],
    pub scroll: usize,
}

/// What a screen column shows, so a click can be mapped back to it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ColumnKind {
    Level(usize),
    /// The file preview or the editor.
    Content,
}

/// Where the last frame put things. Filled in by the renderer.
#[derive(Debug, Clone, Default)]
pub struct HitMap {
    pub columns: Vec<(ColumnKind, u16, u16)>,
    /// Screen row of the shared cursor row.
    pub center_row: u16,
    /// Screen rows the columns occupy, end exclusive.
    pub rows: std::ops::Range<u16>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MouseAction {
    Click,
    DoubleClick,
    ScrollUp,
    ScrollDown,
}

const WHEEL_STEP: isize = 3;
const DOUBLE_CLICK: std::time::Duration = std::time::Duration::from_millis(400);

/// Startup choices that are not key bindings.
pub struct Settings {
    pub show_hidden: bool,
    /// Draws pictures in the preview.
    pub painter: Painter,
    pub depth: crate::theme::Depth,
    pub marks: Marks,
    pub trash: Arc<dyn Trasher>,
}

impl Default for Settings {
    fn default() -> Settings {
        Settings {
            show_hidden: false,
            painter: Painter::blocks(),
            depth: crate::theme::Depth::TrueColor,
            marks: Marks::default(),
            trash: Arc::new(NoTrash),
        }
    }
}

pub struct PromptView<'a> {
    pub label: &'static str,
    pub text: &'a str,
    pub cursor_col: usize,
}

/// A count this large means "as far as possible", so repeating a step further is pointless.
const MAX_REPEAT: usize = 1000;

pub struct App {
    tree: Tree,
    keymap: Keymap,
    input: InputState,
    mode: Mode,
    last_search: Option<String>,
    last_find: Option<(char, bool)>,
    marks: Marks,
    jumps: Jumps,
    files: FileOps,
    /// Entry to put the cursor on once its directory has been listed again.
    pending_focus: Option<PathBuf>,
    /// Where the last jump started, for `''`.
    previous: Option<PathBuf>,
    painter: Painter,
    depth: crate::theme::Depth,
    hitmap: std::cell::RefCell<HitMap>,
    last_click: Option<(std::time::Instant, u16, u16)>,
    /// Rows available to the tree, for page-sized motions.
    viewport: u16,
    home: Option<PathBuf>,
    /// One-line notice shown in the footer until the next key press.
    pub message: Option<String>,
}

impl App {
    pub fn new(root: PathBuf, keymap: Keymap) -> App {
        App::with_settings(root, keymap, Settings::default())
    }

    pub fn with_settings(root: PathBuf, keymap: Keymap, settings: Settings) -> App {
        App {
            tree: Tree::new(root, settings.show_hidden),
            keymap,
            input: InputState::default(),
            mode: Mode::Normal,
            last_search: None,
            last_find: None,
            marks: settings.marks,
            jumps: Jumps::default(),
            files: FileOps::new(settings.trash),
            pending_focus: None,
            previous: None,
            painter: settings.painter,
            depth: settings.depth,
            hitmap: std::cell::RefCell::default(),
            last_click: None,
            viewport: 24,
            home: std::env::var_os("HOME").map(PathBuf::from),
            message: None,
        }
    }

    pub fn tree(&self) -> &Tree {
        &self.tree
    }

    pub fn tree_mut(&mut self) -> &mut Tree {
        &mut self.tree
    }

    pub fn keymap(&self) -> &Keymap {
        &self.keymap
    }

    pub fn set_viewport(&mut self, rows: u16) {
        self.viewport = rows;
        if let Mode::Edit(editor) = &mut self.mode {
            editor.set_rows(usize::from(rows));
        }
    }

    pub fn set_hitmap(&self, map: HitMap) {
        *self.hitmap.borrow_mut() = map;
    }

    /// Turns a raw button press into a click or a double click on the same cell.
    pub fn classify_click(&mut self, col: u16, row: u16, now: std::time::Instant) -> MouseAction {
        let double = self.last_click.is_some_and(|(at, c, r)| {
            c == col && r == row && now.duration_since(at) <= DOUBLE_CLICK
        });
        self.last_click = if double { None } else { Some((now, col, row)) };
        if double {
            MouseAction::DoubleClick
        } else {
            MouseAction::Click
        }
    }

    /// Handles the mouse at a screen cell. Prompts and questions ignore it so nothing happens by accident.
    pub fn mouse(&mut self, action: MouseAction, col: u16, row: u16) -> Response {
        self.message = None;
        let wheel = match action {
            MouseAction::ScrollUp => Some(-WHEEL_STEP),
            MouseAction::ScrollDown => Some(WHEEL_STEP),
            _ => None,
        };
        match &mut self.mode {
            Mode::Edit(editor) => {
                if let Some(delta) = wheel {
                    let key = if delta < 0 {
                        KeyCode::Up
                    } else {
                        KeyCode::Down
                    };
                    for _ in 0..WHEEL_STEP {
                        editor.press(Key::plain(key));
                    }
                }
                return Response::default();
            }
            Mode::Overlay { lines, scroll, .. } => {
                if let Some(delta) = wheel {
                    *scroll = scroll
                        .saturating_add_signed(delta)
                        .min(lines.len().saturating_sub(1));
                }
                return Response::default();
            }
            Mode::Prompt(_) | Mode::Conflict { .. } => return Response::default(),
            Mode::Normal | Mode::Visual { .. } => {}
        }
        if action == MouseAction::DoubleClick {
            // The first click already slid the list to put its entry on the cursor row, so open that.
            return self.run(Command::Enter, None, None);
        }
        let map = self.hitmap.borrow().clone();
        if !map.rows.contains(&row) {
            return Response::default();
        }
        let Some(&(kind, _, _)) = map
            .columns
            .iter()
            .find(|(_, x, w)| col >= *x && col < x + w)
        else {
            return Response::default();
        };
        match (kind, wheel) {
            (ColumnKind::Content, Some(delta)) => {
                self.tree.scroll_preview(delta, usize::from(self.viewport));
                Response::default()
            }
            (ColumnKind::Content, None) => Response::default(),
            (ColumnKind::Level(level), Some(delta)) => {
                self.mode = Mode::Normal;
                self.tree.focus_level(level);
                self.tree.move_by(delta);
                Response::default()
            }
            (ColumnKind::Level(level), None) => {
                let Some(entries) = self
                    .tree
                    .levels()
                    .get(level)
                    .map(|l| (l.cursor, l.entries.len()))
                else {
                    return Response::default();
                };
                let (cursor, len) = entries;
                let offset = i64::from(row) - i64::from(map.center_row);
                let Some(index) = usize::try_from(cursor as i64 + offset)
                    .ok()
                    .filter(|&i| i < len)
                else {
                    return Response::default();
                };
                self.mode = Mode::Normal;
                self.tree.focus_level(level);
                self.tree.set_cursor(index);
                Response::default()
            }
        }
    }

    pub fn depth(&self) -> crate::theme::Depth {
        self.depth
    }

    pub fn painter(&self) -> &Painter {
        &self.painter
    }

    pub fn painter_mut(&mut self) -> &mut Painter {
        &mut self.painter
    }

    pub fn editor(&self) -> Option<&Editor> {
        match &self.mode {
            Mode::Edit(editor) => Some(editor),
            _ => None,
        }
    }

    /// Whether the editor's syntax colors are behind its text.
    pub fn editor_needs_highlight(&self) -> bool {
        self.editor().is_some_and(Editor::highlight_stale)
    }

    pub fn refresh_editor_highlight(&mut self) {
        if let Mode::Edit(editor) = &mut self.mode {
            editor.refresh_highlight();
        }
    }

    /// The half-typed normal-mode command, such as `12g`.
    pub fn pending(&self) -> String {
        self.input.display()
    }

    pub fn prompt_view(&self) -> Option<PromptView<'_>> {
        match &self.mode {
            Mode::Prompt(p) => Some(PromptView {
                label: p.label(),
                text: p.editor.text(),
                cursor_col: p.editor.cursor_col(),
            }),
            _ => None,
        }
    }

    pub fn overlay(&self) -> Option<OverlayView<'_>> {
        match &self.mode {
            Mode::Overlay {
                title,
                lines,
                scroll,
            } => Some(OverlayView {
                title,
                lines,
                scroll: *scroll,
            }),
            _ => None,
        }
    }

    pub fn selection(&self) -> &BTreeSet<PathBuf> {
        self.files.selection()
    }

    pub fn is_visual(&self) -> bool {
        matches!(self.mode, Mode::Visual { .. })
    }

    /// Rows of the focused column covered by visual mode.
    pub fn visual_range(&self) -> Option<RangeInclusive<usize>> {
        let Mode::Visual { anchor } = self.mode else {
            return None;
        };
        let level = self.tree.focused();
        let last = level.entries.len().checked_sub(1)?;
        let (a, b) = (anchor.min(last), level.cursor.min(last));
        Some(a.min(b)..=a.max(b))
    }

    /// The running job's name and progress, if it can tell.
    pub fn running(&self) -> Option<(&str, Option<u8>)> {
        self.files.running()
    }

    /// The question a paste is waiting on.
    pub fn conflict_prompt(&self) -> Option<String> {
        let Mode::Conflict { item, .. } = &self.mode else {
            return None;
        };
        let name = item.to.file_name().unwrap_or_default().to_string_lossy();
        Some(format!(
            "{name} exists: [s]kip [o]verwrite [k]eep both, capitals for all, Esc cancels"
        ))
    }

    pub fn trasher(&self) -> Arc<dyn Trasher> {
        self.files.trasher()
    }

    pub fn take_jobs(&mut self) -> Vec<Job> {
        self.files.take_jobs()
    }

    pub fn job_progress(&mut self, id: u64, done: u64, total: u64) {
        self.files.progress(id, done, total);
    }

    /// A job is over. Its directories are listed again and its outcome becomes the message.
    pub fn finish_job(&mut self, id: u64, outcome: Outcome) {
        let Some(Finished {
            message,
            reload,
            focus_on,
        }) = self.files.finish(id, outcome)
        else {
            return;
        };
        for dir in reload {
            self.tree.reload(&dir);
        }
        self.message = Some(message);
        self.pending_focus = focus_on;
    }

    /// Lists a directory load that finished. Also surfaces anything the tree wants to say, like a missing bookmark target.
    pub fn finish_load(&mut self, dir: &Path, result: std::io::Result<Vec<crate::model::Entry>>) {
        self.tree.finish_load(dir, result);
        self.follow_pending_focus(dir);
        self.pull_notice();
    }

    fn follow_pending_focus(&mut self, listed: &Path) {
        let Some(target) = self.pending_focus.clone() else {
            return;
        };
        let has_it = self.tree.levels().iter().any(|l| {
            l.dir == listed
                && target
                    .file_name()
                    .is_some_and(|n| l.entries.iter().any(|e| e.name == n))
        });
        if target.parent() == Some(listed) && has_it {
            self.pending_focus = None;
            self.tree.reveal(&target);
        }
    }

    /// Finishes every pending listing, preview and job on this thread. For tests.
    #[cfg(test)]
    pub fn settle(&mut self) {
        loop {
            let mut progressed = false;
            for r in self.tree.take_requests() {
                progressed = true;
                let result = crate::fsread::read_dir(&r.dir, r.keep.as_deref(), r.hidden);
                self.finish_load(&r.dir, result);
            }
            let previews = self.tree.take_preview_requests();
            for r in previews
                .into_iter()
                .chain(self.tree.take_prefetch_requests())
            {
                progressed = true;
                let result = crate::preview::build(&r.path);
                self.tree.finish_preview(&r, result);
            }
            for job in self.files.take_jobs() {
                progressed = true;
                let trash = self.files.trasher();
                let ctx = crate::ops::Ctx {
                    trash: &*trash,
                    cancel: &job.cancel,
                    progress: &|_| {},
                };
                let outcome = crate::ops::run_job(&job.ops, &ctx, |_| {});
                self.finish_job(job.id, outcome);
            }
            if !progressed {
                break;
            }
        }
        self.painter.run_jobs_now();
        self.pull_notice();
    }

    fn pull_notice(&mut self) {
        if let Some(notice) = self.tree.take_notice() {
            self.message = Some(notice);
        }
    }

    pub fn press(&mut self, key: Key) -> Response {
        self.message = None;
        self.pending_focus = None;
        let response = match self.mode {
            Mode::Prompt(_) => self.press_prompt(key),
            Mode::Conflict { .. } => {
                self.press_conflict(key);
                Response::default()
            }
            Mode::Overlay { .. } => {
                self.press_overlay(key);
                Response::default()
            }
            Mode::Edit(_) => {
                self.press_edit(key);
                Response::default()
            }
            Mode::Normal | Mode::Visual { .. } => self.press_normal(key),
        };
        self.pull_notice();
        response
    }

    fn press_normal(&mut self, key: Key) -> Response {
        let escape = key.code == KeyCode::Esc && !key.ctrl && !key.alt;
        let interrupt = key.code == KeyCode::Char('c') && key.ctrl;
        if (escape || interrupt) && !self.input.is_pending() {
            if self.files.cancel() {
                self.message = Some("cancelling…".into());
                return Response::default();
            }
            if escape && self.is_visual() {
                self.mode = Mode::Normal;
                return Response::default();
            }
        }
        match self.input.feed(key, &self.keymap, self.is_visual()) {
            Fed::Run {
                command,
                count,
                arg,
            } => self.run(command, count, arg),
            Fed::Operate {
                operator,
                motion,
                count,
            } => self.operate(operator, motion, count),
            Fed::Pending | Fed::Unbound => Response::default(),
        }
    }

    fn run(&mut self, command: Command, count: Option<usize>, arg: Option<char>) -> Response {
        let keeps_visual = command.is_motion()
            || command.operator().is_some()
            || matches!(
                command,
                Command::Visual | Command::PreviewDown | Command::PreviewUp
            );
        if self.is_visual() && !keeps_visual {
            self.mode = Mode::Normal;
        }
        let before = self.tree.location();
        let response = self.execute(command, count, arg);
        let jumps = matches!(
            command,
            Command::First
                | Command::Last
                | Command::Enter
                | Command::Leave
                | Command::SearchNext
                | Command::SearchPrev
        );
        if jumps && self.tree.location() != before {
            self.remember_jump(before);
        }
        response
    }

    /// Moves the focused cursor with a motion. Nothing found leaves the cursor and says why.
    fn motion(&mut self, command: Command, count: Option<usize>, arg: Option<char>) {
        if let (Command::Find | Command::FindBack, Some(letter)) = (command, arg) {
            self.last_find = Some((letter, command == Command::Find));
        }
        let level = self.tree.focused();
        let ctx = motion::Ctx {
            entries: &level.entries,
            cursor: level.cursor,
            viewport: self.viewport,
            last_search: self.last_search.as_deref(),
            last_find: self.last_find,
        };
        match motion::target(command, count, arg, &ctx) {
            Ok(index) => self.tree.set_cursor(index),
            Err(message) => self.message = Some(message),
        }
    }

    fn remember_jump(&mut self, from: PathBuf) {
        self.jumps.record(from.clone());
        self.previous = Some(from);
    }

    fn execute(&mut self, command: Command, count: Option<usize>, arg: Option<char>) -> Response {
        let n = count.unwrap_or(1);
        let step = isize::try_from(n).unwrap_or(isize::MAX);
        match command {
            Command::Down
            | Command::Up
            | Command::First
            | Command::Last
            | Command::HalfPageDown
            | Command::HalfPageUp
            | Command::PageDown
            | Command::PageUp
            | Command::SearchNext
            | Command::SearchPrev
            | Command::Find
            | Command::FindBack
            | Command::FindRepeat
            | Command::FindRepeatBack => self.motion(command, count, arg),
            Command::PreviewDown => self.tree.scroll_preview(step, usize::from(self.viewport)),
            Command::PreviewUp => self.tree.scroll_preview(-step, usize::from(self.viewport)),
            Command::Enter => {
                for _ in 0..n.min(MAX_REPEAT) {
                    let before = self.tree.focus();
                    if let Some(Effect::Open(path)) = self.tree.enter() {
                        return Response {
                            effect: self.edit_here(path),
                            exit: None,
                        };
                    }
                    if self.tree.focus() == before {
                        break;
                    }
                }
            }
            Command::Leave => (0..n.min(MAX_REPEAT)).for_each(|_| self.tree.leave()),
            Command::Search => self.open_prompt(PromptKind::Search),
            Command::ExPrompt => self.open_prompt(PromptKind::Ex),
            Command::Help => self.open_help(),
            Command::OpenExternal => {
                let level = self.tree.focused();
                match level.selected() {
                    Some(entry) if entry.is_openable() => {
                        return Response {
                            effect: Some(Effect::Open(level.dir.join(&entry.name))),
                            exit: None,
                        };
                    }
                    Some(_) => self.message = Some("i opens files, l opens a folder".into()),
                    None => self.message = Some("nothing to open".into()),
                }
            }
            Command::Trash | Command::Yank | Command::Cut => {
                let operator = command.operator().expect("these are operators");
                let targets = match self.visual_range() {
                    Some(range) => self.paths_in(range),
                    None => self.default_targets(),
                };
                return self.apply_operator(operator, targets);
            }
            Command::Visual => self.toggle_visual(),
            Command::ToggleSelect => self.toggle_selected(),
            Command::Paste => self.paste(false),
            Command::PasteInto => self.paste(true),
            Command::Rename => self.start_rename(),
            Command::NewEntry => self.open_prompt(PromptKind::New),
            Command::Undo => self.report(FileOps::undo),
            Command::Redo => self.report(FileOps::redo),
            Command::SetMark => {
                if let Some(name) = arg {
                    let here = self.tree.location();
                    if let Err(message) = self.marks.set(name, here) {
                        self.message = Some(message);
                    }
                }
            }
            Command::JumpMark => {
                if let Some(name) = arg {
                    self.jump_to_mark(name);
                }
            }
            Command::JumpBack => self.walk_jumps(n, false),
            Command::JumpForward => self.walk_jumps(n, true),
            Command::ToggleHidden => self.tree.set_show_hidden(!self.tree.show_hidden()),
            Command::Quit | Command::Abort if self.files.running().is_some() => {
                let label = self
                    .files
                    .running()
                    .map_or("", |(label, _)| label)
                    .to_string();
                self.message = Some(format!("still {label}, Esc cancels it"));
            }
            Command::Quit => {
                return Response {
                    effect: None,
                    exit: Some(Exit::Quit),
                };
            }
            Command::Abort => {
                return Response {
                    effect: None,
                    exit: Some(Exit::Abort),
                };
            }
        }
        Response::default()
    }

    fn paths_in(&self, range: RangeInclusive<usize>) -> Vec<PathBuf> {
        let level = self.tree.focused();
        level.entries[range]
            .iter()
            .map(|e| level.dir.join(&e.name))
            .collect()
    }

    /// What an action without a motion applies to: the selection, or else the entry under the cursor.
    fn default_targets(&self) -> Vec<PathBuf> {
        if !self.files.selection().is_empty() {
            return self.files.selection().iter().cloned().collect();
        }
        self.tree
            .focused()
            .selected()
            .map(|_| vec![self.tree.location()])
            .unwrap_or_default()
    }

    fn toggle_visual(&mut self) {
        self.mode = match self.mode {
            Mode::Visual { .. } => Mode::Normal,
            _ => Mode::Visual {
                anchor: self.tree.focused().cursor,
            },
        };
    }

    fn toggle_selected(&mut self) {
        if self.tree.focused().selected().is_some() {
            self.files.toggle_selected(self.tree.location());
            self.tree.move_by(1);
        }
    }

    /// An operator with a motion covers the entries between the cursor and where the motion lands.
    /// A doubled operator covers the selection, or `count` entries from the cursor.
    fn operate(
        &mut self,
        operator: Operator,
        motion: Option<(Command, Option<char>)>,
        count: Option<usize>,
    ) -> Response {
        let level = self.tree.focused();
        let Some(last) = level.entries.len().checked_sub(1) else {
            self.message = Some("nothing here".into());
            return Response::default();
        };
        let cursor = level.cursor;
        let range = match motion {
            Some((command, arg)) => {
                if let (Command::Find | Command::FindBack, Some(letter)) = (command, arg) {
                    self.last_find = Some((letter, command == Command::Find));
                }
                let ctx = motion::Ctx {
                    entries: &level.entries,
                    cursor,
                    viewport: self.viewport,
                    last_search: self.last_search.as_deref(),
                    last_find: self.last_find,
                };
                match motion::target(command, count, arg, &ctx) {
                    Ok(target) => cursor.min(target)..=cursor.max(target),
                    Err(message) => {
                        self.message = Some(message);
                        return Response::default();
                    }
                }
            }
            None if !self.files.selection().is_empty() && count.is_none() => {
                return self.apply_operator(operator, self.default_targets());
            }
            None => cursor..=(cursor.saturating_add(count.unwrap_or(1) - 1)).min(last),
        };
        let targets = self.paths_in(range);
        self.apply_operator(operator, targets)
    }

    fn apply_operator(&mut self, operator: Operator, targets: Vec<PathBuf>) -> Response {
        if self.is_visual() {
            self.mode = Mode::Normal;
        }
        if targets.is_empty() {
            self.message = Some("nothing to act on".into());
            return Response::default();
        }
        self.files.clear_selection();
        let what = noun(&targets);
        match operator {
            Operator::Yank | Operator::Cut => {
                let cut = operator == Operator::Cut;
                let mode = if cut { PasteMode::Cut } else { PasteMode::Copy };
                self.files.set_register(targets.clone(), mode);
                let verb = if cut { "cut" } else { "yanked" };
                self.message = Some(format!("{verb} {what}"));
                Response {
                    effect: Some(Effect::Clipboard {
                        paths: targets,
                        cut,
                    }),
                    exit: None,
                }
            }
            Operator::Trash => {
                self.submit(JobSpec {
                    label: format!("deleting {what}"),
                    success: format!("moved {what} to the trash, u undoes it"),
                    ops: targets.into_iter().map(|path| Op::Trash { path }).collect(),
                    focus_on: None,
                    clears_register: false,
                });
                Response::default()
            }
        }
    }

    fn submit(&mut self, spec: JobSpec) {
        if let Err(message) = self.files.start(spec) {
            self.message = Some(message);
        }
    }

    fn report(&mut self, action: fn(&mut FileOps) -> Result<(), String>) {
        if let Err(message) = action(&mut self.files) {
            self.message = Some(message);
        }
    }

    fn paste(&mut self, into_hovered: bool) {
        let Some(register) = self.files.register() else {
            self.message = Some("nothing to paste".into());
            return;
        };
        let (sources, mode) = (register.paths.clone(), register.mode);
        let level = self.tree.focused();
        let hovered = level
            .selected()
            .filter(|e| e.is_dir())
            .map(|e| level.dir.join(&e.name));
        let dest = match (into_hovered, hovered) {
            (true, Some(dir)) => dir,
            _ => level.dir.clone(),
        };
        let items = plan_paste(&sources, &dest, mode, &exists);
        self.drive_paste(PasteFlow::new(items, mode), None);
    }

    fn drive_paste(&mut self, mut flow: PasteFlow, answer: Option<(Choice, bool)>) {
        let mode = flow.mode();
        let step = match answer {
            Some((choice, all)) => flow.choose(choice, all, &exists),
            None => flow.advance(&exists),
        };
        match step {
            Step::Ask(item) => self.mode = Mode::Conflict { flow, item },
            Step::Failed(message) => {
                self.mode = Mode::Normal;
                self.message = Some(message);
            }
            Step::Ready(ops) => {
                self.mode = Mode::Normal;
                let moving = mode == PasteMode::Cut;
                let focus_on = ops.iter().rev().find_map(|op| match op {
                    Op::Copy { to, .. } | Op::Move { to, .. } => Some(to.clone()),
                    _ => None,
                });
                let sources: Vec<PathBuf> = ops
                    .iter()
                    .filter_map(|op| match op {
                        Op::Copy { from, .. } | Op::Move { from, .. } => Some(from.clone()),
                        _ => None,
                    })
                    .collect();
                let pasted = sources.len();
                let what = noun(&sources);
                if pasted == 0 {
                    self.message = Some("nothing to paste here".into());
                    return;
                }
                let (label, done) = if moving {
                    ("moving", "moved")
                } else {
                    ("pasting", "pasted")
                };
                self.submit(JobSpec {
                    label: format!("{label} {what}"),
                    success: format!("{done} {what}"),
                    ops,
                    focus_on,
                    clears_register: moving,
                });
            }
        }
    }

    fn press_conflict(&mut self, key: Key) {
        let Mode::Conflict { flow, item } = std::mem::replace(&mut self.mode, Mode::Normal) else {
            return;
        };
        let answer = match key.typed_char() {
            Some('s') => Some((Choice::Skip, false)),
            Some('o') => Some((Choice::Overwrite, false)),
            Some('k') => Some((Choice::KeepBoth, false)),
            Some('S') => Some((Choice::Skip, true)),
            Some('O') => Some((Choice::Overwrite, true)),
            Some('K') => Some((Choice::KeepBoth, true)),
            _ => None,
        };
        match answer {
            Some(answer) => self.drive_paste(flow, Some(answer)),
            None if key.code == KeyCode::Esc || (key.ctrl && key.code == KeyCode::Char('c')) => {
                self.message = Some("paste cancelled".into());
            }
            None => self.mode = Mode::Conflict { flow, item },
        }
    }

    fn start_rename(&mut self) {
        let Some(entry) = self.tree.focused().selected() else {
            self.message = Some("nothing to rename".into());
            return;
        };
        self.mode = Mode::Prompt(Prompt {
            kind: PromptKind::Rename,
            editor: LineEditor::with_text(&entry.display_name()),
            origin: None,
            subject: Some(self.tree.location()),
        });
    }

    fn confirm_rename(&mut self, subject: Option<&Path>, text: &str) {
        let Some(from) = subject else { return };
        let name = match validate_name(text) {
            Ok(name) => name,
            Err(message) => return self.message = Some(message),
        };
        let to = from.with_file_name(name);
        if to == from {
            return;
        }
        if fs::symlink_metadata(&to).is_ok() {
            return self.message = Some(format!("{name} already exists"));
        }
        self.submit(JobSpec {
            label: format!("renaming to {name}"),
            success: format!("renamed to {name}"),
            ops: vec![Op::Move {
                from: from.to_path_buf(),
                to: to.clone(),
            }],
            focus_on: Some(to),
            clears_register: false,
        });
    }

    fn create_entry(&mut self, text: &str, is_dir: bool) {
        let name = match validate_name(text) {
            Ok(name) => name,
            Err(message) => return self.message = Some(message),
        };
        let path = self.tree.focused().dir.join(name);
        if fs::symlink_metadata(&path).is_ok() {
            return self.message = Some(format!("{name} already exists"));
        }
        let op = if is_dir {
            Op::Mkdir { path: path.clone() }
        } else {
            Op::Touch { path: path.clone() }
        };
        self.submit(JobSpec {
            label: describe(&op),
            success: format!("created {name}{}", if is_dir { "/" } else { "" }),
            ops: vec![op],
            focus_on: Some(path),
            clears_register: false,
        });
    }

    fn chmod(&mut self, mode: u32) {
        let targets = self.default_targets();
        if targets.is_empty() {
            return self.message = Some("nothing to change".into());
        }
        self.files.clear_selection();
        let what = noun(&targets);
        self.submit(JobSpec {
            label: format!("changing the mode of {what}"),
            success: format!("mode of {what} is now {mode:o}"),
            ops: targets
                .into_iter()
                .map(|path| Op::Chmod { path, mode })
                .collect(),
            focus_on: None,
            clears_register: false,
        });
    }

    /// Opens a file in the built-in editor. A file it cannot take, such as a binary or a huge one,
    /// goes to the external opener instead.
    fn edit_here(&mut self, path: PathBuf) -> Option<Effect> {
        let loaded = save::fingerprint(&path).and_then(|key| {
            if key.0 > crate::textbuf::MAX_BYTES as u64 {
                return Ok(Err(crate::textbuf::LoadError::TooLarge(key.0 as usize)));
            }
            Ok(Editor::open(path.clone(), &fs::read(&path)?, key))
        });
        match loaded {
            Ok(Ok(mut editor)) => {
                editor.set_rows(usize::from(self.viewport));
                editor.refresh_highlight();
                self.mode = Mode::Edit(Box::new(editor));
                None
            }
            Ok(Err(_)) => Some(Effect::Open(path)),
            Err(e) => {
                self.message = Some(format!("{}: {e}", path.display()));
                None
            }
        }
    }

    fn press_edit(&mut self, key: Key) {
        let Mode::Edit(editor) = &mut self.mode else {
            return;
        };
        match editor.press(key) {
            None => {}
            Some(EditEvent::Close) => self.stop_editing(),
            Some(EditEvent::Save { force, then_close }) => {
                if self.save_editor(force) && then_close {
                    self.stop_editing();
                }
            }
            Some(EditEvent::Reload) => self.reload_editor(),
        }
    }

    /// Writes the editor's text to its file. Refuses when something else changed the file since it was opened,
    /// unless `force` is set.
    fn save_editor(&mut self, force: bool) -> bool {
        let Mode::Edit(editor) = &mut self.mode else {
            return false;
        };
        let path = editor.path().to_path_buf();
        match save::fingerprint(&path) {
            Ok(now) if now != editor.disk_key() && !force => {
                editor.message = Some(
                    "the file changed on disk since you opened it: :w! overwrites, :e! reloads"
                        .into(),
                );
                return false;
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound && !force => {
                editor.message = Some("the file was deleted: :w! writes it again".into());
                return false;
            }
            _ => {}
        }
        let written =
            save::write_file(&path, &editor.bytes()).and_then(|()| save::fingerprint(&path));
        match written {
            Ok(key) => {
                editor.mark_saved(key);
                if let Some(dir) = path.parent() {
                    self.tree.reload(dir);
                }
                true
            }
            Err(e) => {
                editor.message = Some(format!("not written: {e}"));
                false
            }
        }
    }

    fn reload_editor(&mut self) {
        let Mode::Edit(editor) = &mut self.mode else {
            return;
        };
        let path = editor.path().to_path_buf();
        let result = save::fingerprint(&path).and_then(|key| Ok((fs::read(&path)?, key)));
        match result {
            Ok((bytes, key)) => match editor.reload(&bytes, key) {
                Ok(()) => editor.message = Some("reloaded from disk".into()),
                Err(reason) => editor.message = Some(format!("cannot reload: {reason}")),
            },
            Err(e) => editor.message = Some(format!("cannot reload: {e}")),
        }
        editor.refresh_highlight();
    }

    fn stop_editing(&mut self) {
        if let Mode::Edit(editor) = &self.mode
            && let Some(dir) = editor.path().parent()
        {
            self.tree.reload(dir);
        }
        self.mode = Mode::Normal;
    }

    fn open_help(&mut self) {
        let rows = self.keymap.describe();
        let key_width = rows
            .iter()
            .map(|(k, _)| unicode_width::UnicodeWidthStr::width(k.as_str()))
            .max()
            .unwrap_or(0);
        let lines = rows
            .iter()
            .map(|(keys, help)| {
                let pad =
                    " ".repeat(key_width - unicode_width::UnicodeWidthStr::width(keys.as_str()));
                format!("{keys}{pad}   {help}")
            })
            .collect();
        self.mode = Mode::Overlay {
            title: "keys".into(),
            lines,
            scroll: 0,
        };
    }

    fn open_marks(&mut self) {
        let mut lines: Vec<String> = self
            .marks
            .list()
            .into_iter()
            .map(|(name, path)| format!("{name}   {}", path.display()))
            .collect();
        if lines.is_empty() {
            lines.push("no marks yet. m{a-z} sets one, m{A-Z} saves a bookmark".into());
        }
        self.mode = Mode::Overlay {
            title: "marks".into(),
            lines,
            scroll: 0,
        };
    }

    fn jump_to_mark(&mut self, name: char) {
        let here = self.tree.location();
        let target = if name == '\'' || name == '`' {
            match self.previous.replace(here.clone()) {
                Some(previous) => previous,
                None => {
                    self.previous = None;
                    self.message = Some("no previous position".into());
                    return;
                }
            }
        } else {
            match self.marks.get(name) {
                Some(path) => path.to_path_buf(),
                None => {
                    self.message = Some(format!("mark {name} is not set"));
                    return;
                }
            }
        };
        if name != '\'' && name != '`' && target != here {
            self.remember_jump(here);
        }
        self.tree.reveal(&target);
    }

    fn walk_jumps(&mut self, times: usize, forward: bool) {
        let mut here = self.tree.location();
        let mut target = None;
        for _ in 0..times.min(MAX_REPEAT) {
            let next = if forward {
                self.jumps.forward()
            } else {
                self.jumps.back(&here)
            };
            match next {
                Some(path) => {
                    here = path.clone();
                    target = Some(path);
                }
                None => break,
            }
        }
        match target {
            Some(path) => self.tree.reveal(&path),
            None => {
                let edge = if forward { "newest" } else { "oldest" };
                self.message = Some(format!("already at the {edge} jump"));
            }
        }
    }

    fn open_prompt(&mut self, kind: PromptKind) {
        let origin = match kind {
            PromptKind::Search => self.tree.focused().selected().map(|e| e.name.clone()),
            PromptKind::Ex | PromptKind::Rename | PromptKind::New => None,
        };
        self.mode = Mode::Prompt(Prompt {
            kind,
            editor: LineEditor::default(),
            origin,
            subject: None,
        });
    }

    fn press_prompt(&mut self, key: Key) -> Response {
        let Mode::Prompt(mut prompt) = std::mem::replace(&mut self.mode, Mode::Normal) else {
            return Response::default();
        };
        match prompt.handle(key) {
            PromptEvent::Idle => self.mode = Mode::Prompt(prompt),
            PromptEvent::Edited => {
                if prompt.kind == PromptKind::Search {
                    self.incremental_search(&prompt);
                }
                self.mode = Mode::Prompt(prompt);
            }
            PromptEvent::Cancel => {
                if prompt.kind == PromptKind::Search {
                    self.tree.set_cursor(self.search_origin(&prompt));
                }
            }
            PromptEvent::Confirm => {
                return match prompt.kind {
                    PromptKind::Search => {
                        let origin = self
                            .tree
                            .focused()
                            .dir
                            .join(prompt.origin.as_deref().unwrap_or_default());
                        self.confirm_search(prompt.editor.text(), origin);
                        Response::default()
                    }
                    PromptKind::Ex => self.run_ex(prompt.editor.text()),
                    PromptKind::Rename => {
                        self.confirm_rename(prompt.subject.as_deref(), prompt.editor.text());
                        Response::default()
                    }
                    PromptKind::New => {
                        let text = prompt.editor.text();
                        match text.strip_suffix('/') {
                            Some(name) => self.create_entry(name, true),
                            None => self.create_entry(text, false),
                        }
                        Response::default()
                    }
                };
            }
        }
        Response::default()
    }

    /// Index of the entry the search started on, which incremental matching measures from.
    fn search_origin(&self, prompt: &Prompt) -> usize {
        let level = self.tree.focused();
        prompt
            .origin
            .as_ref()
            .and_then(|name| level.entries.iter().position(|e| &e.name == name))
            .unwrap_or(level.cursor)
    }

    fn incremental_search(&mut self, prompt: &Prompt) {
        let origin = self.search_origin(prompt);
        let entries = &self.tree.focused().entries;
        let target =
            search::find(entries, prompt.editor.text(), origin, true, true).unwrap_or(origin);
        self.tree.set_cursor(target);
    }

    fn confirm_search(&mut self, query: &str, origin: PathBuf) {
        if self.tree.location() != origin {
            self.remember_jump(origin);
        }
        if query.is_empty() {
            if self.last_search.is_some() {
                self.motion(Command::SearchNext, None, None);
            }
            return;
        }
        let level = self.tree.focused();
        if search::find(&level.entries, query, level.cursor, true, true).is_none() {
            self.message = Some(format!("pattern not found: {query}"));
        }
        self.last_search = Some(query.to_string());
    }

    fn run_ex(&mut self, line: &str) -> Response {
        let parsed = excmd::parse(line, self.tree.current_dir(), self.home.as_deref());
        let exit = |exit| Response {
            effect: None,
            exit: Some(exit),
        };
        match parsed {
            Ok(Ex::Quit) => return exit(Exit::Quit),
            Ok(Ex::Abort) => return exit(Exit::Abort),
            Ok(Ex::Help) => self.open_help(),
            Ok(Ex::Marks) => self.open_marks(),
            Ok(Ex::Images) => self.message = Some(self.painter.describe()),
            Ok(Ex::Undo) => self.report(FileOps::undo),
            Ok(Ex::Redo) => self.report(FileOps::redo),
            Ok(Ex::Mkdir(name)) => self.create_entry(&name, true),
            Ok(Ex::Touch(name)) => self.create_entry(&name, false),
            Ok(Ex::Chmod(mode)) => self.chmod(mode),
            Ok(Ex::SetHidden(setting)) => {
                let on = setting.unwrap_or(!self.tree.show_hidden());
                self.tree.set_show_hidden(on);
            }
            Ok(Ex::Cd(path)) => self.change_root(&path),
            Err(message) => self.message = Some(message),
        }
        Response::default()
    }

    fn change_root(&mut self, path: &Path) {
        match path.canonicalize() {
            Ok(dir) if dir.is_dir() => {
                let before = self.tree.location();
                self.tree = Tree::new(dir, self.tree.show_hidden());
                self.remember_jump(before);
            }
            Ok(_) => self.message = Some(format!("not a directory: {}", path.display())),
            Err(e) => self.message = Some(format!("{}: {e}", path.display())),
        }
    }

    fn press_overlay(&mut self, key: Key) {
        let Mode::Overlay { lines, scroll, .. } = &mut self.mode else {
            return;
        };
        let last = lines.len().saturating_sub(1);
        let typed = key.typed_char();
        match (key.code, typed) {
            (KeyCode::Down, _) | (_, Some('j')) => *scroll = (*scroll + 1).min(last),
            (KeyCode::Up, _) | (_, Some('k')) => *scroll = scroll.saturating_sub(1),
            (KeyCode::PageDown, _) => *scroll = (*scroll + 10).min(last),
            (KeyCode::PageUp, _) => *scroll = scroll.saturating_sub(10),
            (KeyCode::Esc | KeyCode::Enter, _) | (_, Some('q' | '?')) => self.mode = Mode::Normal,
            _ => {}
        }
    }
}

fn exists(path: &Path) -> bool {
    fs::symlink_metadata(path).is_ok()
}

fn noun(paths: &[PathBuf]) -> String {
    match paths {
        [one] => one
            .file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .into_owned(),
        many => format!("{} items", many.len()),
    }
}

/// A single path component, so a rename or new file cannot escape the directory.
fn validate_name(text: &str) -> Result<&str, String> {
    match text {
        _ if text.trim().is_empty() => Err("the name is empty".into()),
        "." | ".." => Err(format!("{text} is not a usable name")),
        _ if text.contains('/') => Err("names cannot contain /".into()),
        _ if text.contains('\0') => Err("names cannot contain NUL".into()),
        _ => Ok(text),
    }
}

#[cfg(test)]
mod tests {
    use crate::marks::Marks;
    use std::{collections::BTreeMap, fs};

    use super::*;

    fn fixture() -> crate::testdir::TestDir {
        let tmp = crate::testdir::tempdir();
        for d in ["docs", "src", "target"] {
            fs::create_dir(tmp.path().join(d)).unwrap();
        }
        for f in ["Cargo.toml", "README.md", "notes.txt", "tsconfig.json"] {
            fs::write(tmp.path().join(f), "").unwrap();
        }
        fs::write(tmp.path().join("src/main.rs"), "").unwrap();
        fs::create_dir(tmp.path().join("src/deep")).unwrap();
        fs::write(tmp.path().join("src/deep/leaf.txt"), "").unwrap();
        tmp
    }

    fn open(path: &Path) -> App {
        let mut app = App::new(path.to_path_buf(), Keymap::default());
        app.tree_mut().settle();
        app
    }

    /// Types vim-notation keys, letting listings finish after each one. Returns the last non-empty response.
    fn keys(app: &mut App, text: &str) -> Response {
        let mut last = Response::default();
        for key in Key::parse_seq(text).unwrap() {
            let response = app.press(key);
            app.settle();
            if response != Response::default() {
                last = response;
            }
        }
        last
    }

    fn selected(app: &App) -> String {
        app.tree().focused().selected().unwrap().display_name()
    }

    #[test]
    fn counts_repeat_motions_and_clamp() {
        let tmp = fixture();
        let mut app = open(tmp.path());
        keys(&mut app, "3j");
        assert_eq!(selected(&app), "Cargo.toml");
        keys(&mut app, "2k");
        assert_eq!(selected(&app), "src");
        keys(&mut app, "99j");
        assert_eq!(selected(&app), "tsconfig.json");
    }

    #[test]
    fn gg_and_capital_g_jump_to_the_ends_or_to_a_numbered_entry() {
        let tmp = fixture();
        let mut app = open(tmp.path());
        keys(&mut app, "G");
        assert_eq!(selected(&app), "tsconfig.json");
        keys(&mut app, "gg");
        assert_eq!(selected(&app), "docs");
        keys(&mut app, "4G");
        assert_eq!(selected(&app), "Cargo.toml");
        keys(&mut app, "2gg");
        assert_eq!(selected(&app), "src");
        keys(&mut app, "500G");
        assert_eq!(selected(&app), "tsconfig.json");
    }

    #[test]
    fn page_motions_scale_with_the_viewport_and_count() {
        let tmp = crate::testdir::tempdir();
        for i in 0..100 {
            fs::write(tmp.path().join(format!("f{i:03}")), "").unwrap();
        }
        let mut app = open(tmp.path());
        app.set_viewport(12);
        keys(&mut app, "<c-d>");
        assert_eq!(app.tree().focused().cursor, 6);
        keys(&mut app, "<c-f>");
        assert_eq!(app.tree().focused().cursor, 16);
        keys(&mut app, "2<c-d>");
        assert_eq!(app.tree().focused().cursor, 28);
        keys(&mut app, "<c-u><c-b>");
        assert_eq!(app.tree().focused().cursor, 12);
    }

    #[test]
    fn f_jumps_to_names_by_first_letter_and_semicolon_repeats() {
        let tmp = fixture();
        let mut app = open(tmp.path());
        keys(&mut app, "ft");
        assert_eq!(selected(&app), "target");
        keys(&mut app, ";");
        assert_eq!(selected(&app), "tsconfig.json");
        keys(&mut app, ";");
        assert_eq!(selected(&app), "target", "wraps");
        keys(&mut app, ",");
        assert_eq!(selected(&app), "tsconfig.json");
        keys(&mut app, "Fc");
        assert_eq!(selected(&app), "Cargo.toml");
        keys(&mut app, "fz");
        assert_eq!(app.message.as_deref(), Some("no name starts with z"));
    }

    #[test]
    fn a_letter_that_is_a_command_can_still_be_jumped_to() {
        let tmp = crate::testdir::tempdir();
        for f in ["alpha", "quokka", "zulu"] {
            fs::write(tmp.path().join(f), "").unwrap();
        }
        let mut app = open(tmp.path());
        keys(&mut app, "fq");
        assert_eq!(selected(&app), "quokka");
    }

    #[test]
    fn l_and_h_walk_levels_and_counts_repeat_them() {
        let tmp = fixture();
        let mut app = open(tmp.path());
        keys(&mut app, "j2l");
        assert_eq!(app.tree().focus(), 3, "src then deep");
        assert_eq!(app.tree().current_dir(), tmp.path().join("src/deep"));
        keys(&mut app, "2h");
        assert_eq!(app.tree().focus(), 1);
        assert_eq!(app.tree().current_dir(), tmp.path());
    }

    #[test]
    fn i_hands_a_file_to_the_external_editor_and_l_edits_it_here() {
        let tmp = fixture();
        let mut app = open(tmp.path());
        keys(&mut app, "3j");
        let response = keys(&mut app, "i");
        assert_eq!(
            response.effect,
            Some(Effect::Open(tmp.path().join("Cargo.toml")))
        );
        assert!(app.editor().is_none());
        let response = keys(&mut app, "l");
        assert_eq!(response.effect, None);
        assert!(app.editor().is_some());
    }

    #[test]
    fn i_on_a_folder_says_what_it_does() {
        let tmp = fixture();
        let mut app = open(tmp.path());
        assert_eq!(keys(&mut app, "i").effect, None);
        assert_eq!(
            app.message.as_deref(),
            Some("i opens files, l opens a folder")
        );
    }

    #[test]
    fn quit_abort_and_escape_semantics() {
        let tmp = fixture();
        let mut app = open(tmp.path());
        assert_eq!(keys(&mut app, "q").exit, Some(Exit::Quit));
        assert_eq!(keys(&mut app, "<c-c>").exit, Some(Exit::Abort));
        assert_eq!(
            keys(&mut app, "3<esc>").exit,
            None,
            "escape cancels the count"
        );
        assert_eq!(keys(&mut app, "<esc>").exit, Some(Exit::Quit));
    }

    #[test]
    fn pending_shows_the_half_typed_command() {
        let tmp = fixture();
        let mut app = open(tmp.path());
        keys(&mut app, "12g");
        assert_eq!(app.pending(), "12g");
        keys(&mut app, "g");
        assert_eq!(app.pending(), "");
        keys(&mut app, "f");
        assert_eq!(app.pending(), "f");
    }

    #[test]
    fn typing_a_search_moves_to_the_first_match_at_or_after_the_start() {
        let tmp = fixture();
        let mut app = open(tmp.path());
        keys(&mut app, "3j/t");
        assert_eq!(selected(&app), "Cargo.toml", "the current entry matches");
        keys(&mut app, "s");
        assert_eq!(selected(&app), "tsconfig.json");
        keys(&mut app, "<bs>");
        assert_eq!(
            selected(&app),
            "Cargo.toml",
            "backspace re-measures from the origin"
        );
    }

    #[test]
    fn matching_a_directory_shows_its_contents_in_the_preview() {
        let tmp = fixture();
        let mut app = open(tmp.path());
        keys(&mut app, "/src");
        let preview = &app.tree().levels()[2];
        assert_eq!(preview.dir, tmp.path().join("src"));
        assert!(
            preview
                .entries
                .iter()
                .any(|e| e.display_name() == "main.rs")
        );
    }

    #[test]
    fn escape_restores_the_cursor_and_enter_keeps_it() {
        let tmp = fixture();
        let mut app = open(tmp.path());
        keys(&mut app, "/notes<esc>");
        assert_eq!(selected(&app), "docs");
        assert!(app.prompt_view().is_none());
        keys(&mut app, "/notes<cr>");
        assert_eq!(selected(&app), "notes.txt");
        assert!(app.prompt_view().is_none());
    }

    #[test]
    fn backspace_on_an_empty_prompt_closes_it() {
        let tmp = fixture();
        let mut app = open(tmp.path());
        keys(&mut app, "/<bs>");
        assert!(app.prompt_view().is_none());
    }

    #[test]
    fn motion_keys_are_query_text_while_the_prompt_is_open() {
        let tmp = fixture();
        let mut app = open(tmp.path());
        keys(&mut app, "/jjq");
        assert_eq!(app.prompt_view().unwrap().text, "jjq");
        assert_eq!(app.tree().focus(), 1);
    }

    #[test]
    fn prompt_supports_cursor_editing() {
        let tmp = fixture();
        let mut app = open(tmp.path());
        keys(&mut app, "/rdme<left><left><left>ea");
        assert_eq!(app.prompt_view().unwrap().text, "readme");
        assert_eq!(app.prompt_view().unwrap().cursor_col, 3);
        assert_eq!(selected(&app), "README.md");
    }

    #[test]
    fn n_and_capital_n_walk_the_matches_and_wrap() {
        let tmp = fixture();
        let mut app = open(tmp.path());
        keys(&mut app, "/t<cr>");
        let mut seen = vec![selected(&app)];
        for _ in 0..4 {
            keys(&mut app, "n");
            seen.push(selected(&app));
        }
        assert_eq!(
            seen,
            [
                "target",
                "Cargo.toml",
                "notes.txt",
                "tsconfig.json",
                "target"
            ]
        );
        keys(&mut app, "N");
        assert_eq!(selected(&app), "tsconfig.json");
        keys(&mut app, "2n");
        assert_eq!(selected(&app), "Cargo.toml");
    }

    #[test]
    fn searching_uses_smartcase() {
        let tmp = fixture();
        let mut app = open(tmp.path());
        keys(&mut app, "/readme");
        assert_eq!(selected(&app), "README.md");
        keys(&mut app, "<esc>/CARGO");
        assert_eq!(
            selected(&app),
            "docs",
            "an uppercase query is case sensitive and finds nothing"
        );
    }

    #[test]
    fn an_empty_search_repeats_the_previous_one() {
        let tmp = fixture();
        let mut app = open(tmp.path());
        keys(&mut app, "/src<cr>gg/<cr>");
        assert_eq!(selected(&app), "src");
    }

    #[test]
    fn a_missing_match_leaves_the_cursor_and_says_so() {
        let tmp = fixture();
        let mut app = open(tmp.path());
        keys(&mut app, "2j/zzz");
        assert_eq!(selected(&app), "target");
        keys(&mut app, "<cr>");
        assert_eq!(app.message.as_deref(), Some("pattern not found: zzz"));
        keys(&mut app, "j");
        assert_eq!(app.message, None, "the notice clears on the next key");
        keys(&mut app, "n");
        assert_eq!(app.message.as_deref(), Some("pattern not found: zzz"));
    }

    #[test]
    fn n_without_a_previous_search_says_so() {
        let tmp = fixture();
        let mut app = open(tmp.path());
        keys(&mut app, "n");
        assert_eq!(app.message.as_deref(), Some("no previous search"));
    }

    #[test]
    fn colon_q_quits_and_colon_q_bang_aborts() {
        let tmp = fixture();
        let mut app = open(tmp.path());
        assert_eq!(keys(&mut app, ":q<cr>").exit, Some(Exit::Quit));
        assert_eq!(keys(&mut app, ":q!<cr>").exit, Some(Exit::Abort));
    }

    #[test]
    fn colon_cd_restarts_the_tree_in_another_directory() {
        let tmp = fixture();
        let mut app = open(tmp.path());
        keys(&mut app, ":cd src<cr>");
        assert_eq!(
            app.tree().current_dir(),
            tmp.path().join("src").canonicalize().unwrap()
        );
        assert_eq!(selected(&app), "deep");
        keys(&mut app, ":cd nowhere<cr>");
        assert!(
            app.message.as_deref().unwrap().contains("nowhere"),
            "{:?}",
            app.message
        );
        keys(&mut app, ":cd main.rs<cr>");
        assert!(
            app.message
                .as_deref()
                .unwrap()
                .starts_with("not a directory"),
            "{:?}",
            app.message
        );
    }

    #[test]
    fn unknown_commands_are_reported_and_close_the_prompt() {
        let tmp = fixture();
        let mut app = open(tmp.path());
        keys(&mut app, ":frobnicate<cr>");
        assert_eq!(app.message.as_deref(), Some("unknown command: frobnicate"));
        assert!(app.prompt_view().is_none());
    }

    #[test]
    fn help_opens_scrolls_stays_in_range_and_closes() {
        let tmp = fixture();
        let mut app = open(tmp.path());
        keys(&mut app, "?");
        assert_eq!(app.overlay().map(|o| o.scroll), Some(0));
        keys(&mut app, "jj");
        assert_eq!(app.overlay().map(|o| o.scroll), Some(2));
        keys(&mut app, &"j".repeat(500));
        assert_eq!(
            app.overlay().map(|o| o.scroll),
            Some(app.keymap().describe().len() - 1)
        );
        keys(&mut app, "k");
        assert_eq!(
            selected(&app),
            "docs",
            "keys do not move the tree behind the overlay"
        );
        keys(&mut app, "q");
        assert_eq!(app.overlay().map(|o| o.scroll), None);
        keys(&mut app, ":help<cr>");
        assert_eq!(app.overlay().map(|o| o.scroll), Some(0));
    }

    #[test]
    fn user_keymaps_change_what_keys_do() {
        let tmp = fixture();
        let keymap = Keymap::with_overrides(&BTreeMap::from([
            ("<space>".to_string(), "down".to_string()),
            ("q".to_string(), "none".to_string()),
        ]))
        .unwrap();
        let mut app = App::new(tmp.path().to_path_buf(), keymap);
        app.tree_mut().settle();
        keys(&mut app, "<space><space>");
        assert_eq!(selected(&app), "target");
        assert_eq!(keys(&mut app, "q").exit, None);
    }

    fn here(app: &App) -> PathBuf {
        app.tree().location()
    }

    #[test]
    fn a_mark_jumps_back_to_the_entry_from_anywhere() {
        let tmp = fixture();
        let mut app = open(tmp.path());
        keys(&mut app, "jllma");
        assert_eq!(here(&app), tmp.path().join("src/deep/leaf.txt"));
        keys(&mut app, "hhgg");
        assert_eq!(here(&app), tmp.path().join("docs"));
        keys(&mut app, "'a");
        assert_eq!(here(&app), tmp.path().join("src/deep/leaf.txt"));
        assert_eq!(app.tree().focus(), 3);
    }

    #[test]
    fn quote_quote_swaps_with_the_previous_position() {
        let tmp = fixture();
        let mut app = open(tmp.path());
        keys(&mut app, "G");
        assert_eq!(here(&app), tmp.path().join("tsconfig.json"));
        keys(&mut app, "''");
        assert_eq!(here(&app), tmp.path().join("docs"));
        keys(&mut app, "''");
        assert_eq!(here(&app), tmp.path().join("tsconfig.json"));
    }

    #[test]
    fn quote_quote_with_no_history_says_so() {
        let tmp = fixture();
        let mut app = open(tmp.path());
        keys(&mut app, "''");
        assert_eq!(app.message.as_deref(), Some("no previous position"));
        assert_eq!(here(&app), tmp.path().join("docs"));
    }

    #[test]
    fn unset_marks_and_invalid_names_are_reported() {
        let tmp = fixture();
        let mut app = open(tmp.path());
        keys(&mut app, "'z");
        assert_eq!(app.message.as_deref(), Some("mark z is not set"));
        keys(&mut app, "m1");
        assert_eq!(app.message.as_deref(), Some("marks are a-z and A-Z, not 1"));
    }

    #[test]
    fn a_mark_whose_target_was_deleted_reports_it() {
        let tmp = fixture();
        let mut app = open(tmp.path());
        keys(&mut app, "jllmahhgg");
        fs::remove_file(tmp.path().join("src/deep/leaf.txt")).unwrap();
        app.tree_mut().reload(&tmp.path().join("src/deep"));
        app.tree_mut().settle();
        keys(&mut app, "'a");
        let message = app.message.clone().unwrap_or_default();
        assert!(message.contains("no longer exists"), "{message:?}");
    }

    #[test]
    fn uppercase_marks_survive_a_restart_and_lowercase_ones_do_not() {
        let tmp = fixture();
        let state = crate::testdir::tempdir();
        let file = state.path().join("marks");
        let mut app = App::with_settings(
            tmp.path().to_path_buf(),
            Keymap::default(),
            Settings {
                marks: Marks::open(Some(file.clone())),
                ..Settings::default()
            },
        );
        app.tree_mut().settle();
        keys(&mut app, "jmSmq");
        let mut again = App::with_settings(
            tmp.path().to_path_buf(),
            Keymap::default(),
            Settings {
                marks: Marks::open(Some(file)),
                ..Settings::default()
            },
        );
        again.tree_mut().settle();
        keys(&mut again, "'S");
        assert_eq!(here(&again), tmp.path().join("src"));
        keys(&mut again, "'q");
        assert_eq!(again.message.as_deref(), Some("mark q is not set"));
    }

    #[test]
    fn a_mark_outside_the_current_root_re_roots_the_tree() {
        let tmp = fixture();
        let other = crate::testdir::tempdir();
        fs::write(other.path().join("far.txt"), "x").unwrap();
        let mut app = open(tmp.path());
        app.marks.set('F', other.path().join("far.txt")).unwrap();
        keys(&mut app, "'F");
        assert_eq!(here(&app), other.path().join("far.txt"));
    }

    #[test]
    fn colon_marks_lists_them_in_an_overlay() {
        let tmp = fixture();
        let mut app = open(tmp.path());
        keys(&mut app, ":marks<cr>");
        let view = app.overlay().unwrap();
        assert_eq!(view.title, "marks");
        assert!(view.lines[0].contains("no marks yet"));
        keys(&mut app, "q");
        keys(&mut app, "jma:marks<cr>");
        let view = app.overlay().unwrap();
        assert_eq!(
            view.lines,
            [format!("a   {}", tmp.path().join("src").display())]
        );
    }

    #[test]
    fn ctrl_o_and_tab_walk_the_places_you_jumped_from() {
        let tmp = fixture();
        let mut app = open(tmp.path());
        keys(&mut app, "jl");
        assert_eq!(here(&app), tmp.path().join("src/deep"));
        keys(&mut app, "<c-o>");
        assert_eq!(here(&app), tmp.path().join("src"));
        keys(&mut app, "<c-o>");
        assert_eq!(app.message.as_deref(), Some("already at the oldest jump"));
        keys(&mut app, "<tab>");
        assert_eq!(here(&app), tmp.path().join("src/deep"));
        keys(&mut app, "<tab>");
        assert_eq!(app.message.as_deref(), Some("already at the newest jump"));
    }

    #[test]
    fn plain_line_motions_do_not_pollute_the_jumplist() {
        let tmp = fixture();
        let mut app = open(tmp.path());
        keys(&mut app, "jjjkk<c-o>");
        assert_eq!(app.message.as_deref(), Some("already at the oldest jump"));
    }

    #[test]
    fn a_count_walks_several_jumps_at_once() {
        let tmp = fixture();
        let mut app = open(tmp.path());
        keys(&mut app, "GggG4G");
        keys(&mut app, "3<c-o>");
        assert_eq!(here(&app), tmp.path().join("docs"));
    }

    #[test]
    fn zh_and_set_hidden_toggle_dotfiles_and_keep_the_cursor() {
        let tmp = fixture();
        fs::write(tmp.path().join(".env"), "").unwrap();
        let mut app = open(tmp.path());
        keys(&mut app, "3j");
        assert_eq!(selected(&app), "Cargo.toml");
        keys(&mut app, "zh");
        let names: Vec<_> = app
            .tree()
            .focused()
            .entries
            .iter()
            .map(|e| e.display_name())
            .collect();
        assert!(names.contains(&".env".to_string()));
        assert_eq!(selected(&app), "Cargo.toml");
        keys(&mut app, "zh");
        assert!(!app.tree().show_hidden());
        keys(&mut app, ":set hidden<cr>");
        assert!(app.tree().show_hidden());
        keys(&mut app, ":set hidden!<cr>");
        assert!(!app.tree().show_hidden());
        keys(&mut app, ":set nohidden<cr>");
        assert!(!app.tree().show_hidden());
    }

    #[test]
    fn hidden_setting_survives_colon_cd() {
        let tmp = fixture();
        let mut app = App::with_settings(
            tmp.path().to_path_buf(),
            Keymap::default(),
            Settings {
                show_hidden: true,
                ..Settings::default()
            },
        );
        app.tree_mut().settle();
        keys(&mut app, ":cd src<cr>");
        assert!(app.tree().show_hidden());
    }

    #[test]
    fn colon_cd_is_a_jump_you_can_undo() {
        let tmp = fixture();
        let mut app = open(tmp.path());
        keys(&mut app, ":cd src<cr>");
        keys(&mut app, "<c-o>");
        assert_eq!(here(&app), tmp.path().join("docs"));
    }

    #[test]
    fn a_search_that_moves_the_cursor_is_a_jump() {
        let tmp = fixture();
        let mut app = open(tmp.path());
        keys(&mut app, "/notes<cr>");
        assert_eq!(selected(&app), "notes.txt");
        keys(&mut app, "<c-o>");
        assert_eq!(here(&app), tmp.path().join("docs"));
    }

    struct Files {
        root: crate::testdir::TestDir,
        trash: Arc<crate::ops::FakeTrash>,
        _trash_dir: crate::testdir::TestDir,
        app: App,
    }

    impl Files {
        fn p(&self, rel: &str) -> PathBuf {
            self.root.path().join(rel)
        }

        fn has(&self, rel: &str) -> bool {
            fs::symlink_metadata(self.p(rel)).is_ok()
        }

        fn read(&self, rel: &str) -> String {
            fs::read_to_string(self.p(rel)).unwrap()
        }

        fn keys(&mut self, text: &str) -> Response {
            keys(&mut self.app, text)
        }

        fn names(&self) -> Vec<String> {
            self.app
                .tree()
                .focused()
                .entries
                .iter()
                .map(|e| e.display_name())
                .collect()
        }
    }

    /// dir1/ dir2/ a.txt b.txt c.txt, with a fake trash so no real one is touched.
    fn files() -> Files {
        let root = crate::testdir::tempdir();
        for d in ["dir1", "dir2"] {
            fs::create_dir(root.path().join(d)).unwrap();
        }
        fs::write(root.path().join("dir1/inner.txt"), "inner").unwrap();
        for f in ["a.txt", "b.txt", "c.txt"] {
            fs::write(root.path().join(f), format!("content of {f}")).unwrap();
        }
        let trash_dir = crate::testdir::tempdir();
        let trash = Arc::new(crate::ops::FakeTrash::new(&trash_dir.path().join("t")));
        let mut app = App::with_settings(
            root.path().to_path_buf(),
            Keymap::default(),
            Settings {
                trash: trash.clone(),
                ..Settings::default()
            },
        );
        app.settle();
        Files {
            root,
            trash,
            _trash_dir: trash_dir,
            app,
        }
    }

    #[test]
    fn dd_trashes_the_entry_and_u_and_ctrl_r_undo_and_redo_it() {
        let mut f = files();
        f.keys("2jdd");
        assert!(!f.has("a.txt"));
        assert_eq!(f.trash.item_count(), 1);
        assert_eq!(
            f.app.message.as_deref(),
            Some("moved a.txt to the trash, u undoes it")
        );
        f.keys("u");
        assert_eq!(f.read("a.txt"), "content of a.txt");
        assert_eq!(f.app.message.as_deref(), Some("undid deleting a.txt"));
        f.keys("<c-r>");
        assert!(!f.has("a.txt"));
        f.keys("uu");
        assert_eq!(f.app.message.as_deref(), Some("nothing to undo"));
    }

    #[test]
    fn operators_with_motions_cover_the_entries_between_cursor_and_target() {
        let mut f = files();
        f.keys("2jd1j");
        assert!(!f.has("a.txt") && !f.has("b.txt") && f.has("c.txt"));
        f.keys("uD");
        let mut f = files();
        f.keys("3jdk");
        assert!(!f.has("a.txt") && !f.has("b.txt") && f.has("c.txt"));
        let mut f = files();
        f.keys("3jdG");
        assert!(!f.has("b.txt") && !f.has("c.txt") && f.has("a.txt"));
        let mut f = files();
        f.keys("2jdgg");
        assert!(!f.has("dir1") && !f.has("dir2") && !f.has("a.txt") && f.has("b.txt"));
        let mut f = files();
        f.keys("2j3dd");
        assert!(!f.has("a.txt") && !f.has("b.txt") && !f.has("c.txt"));
        assert!(f.has("dir1") && f.has("dir2"));
    }

    #[test]
    fn an_operator_can_reach_to_a_letter_jump() {
        let mut f = files();
        f.keys("2jdfc");
        assert!(!f.has("a.txt") && !f.has("b.txt") && !f.has("c.txt"));
        assert!(f.has("dir1") && f.has("dir2"));
    }

    #[test]
    fn a_failed_motion_deletes_nothing() {
        let mut f = files();
        f.keys("2jdfz");
        assert_eq!(f.app.message.as_deref(), Some("no name starts with z"));
        assert!(f.has("a.txt"));
        f.keys("d/");
        assert!(f.has("a.txt"), "a prompt is not a motion");
    }

    #[test]
    fn yank_and_paste_copy_into_the_directory_you_are_in() {
        let mut f = files();
        let response = f.keys("2jyy");
        assert_eq!(
            response.effect,
            Some(Effect::Clipboard {
                paths: vec![f.p("a.txt")],
                cut: false
            })
        );
        f.keys("gg");
        f.keys("l");
        f.keys("p");
        assert_eq!(f.read("dir1/a.txt"), "content of a.txt");
        assert!(f.has("a.txt"), "yank leaves the original");
        assert_eq!(
            f.app.tree().location(),
            f.p("dir1/a.txt"),
            "the cursor follows the new file"
        );
        f.keys("p");
        let prompt = f.app.conflict_prompt().expect("a second paste clashes");
        assert!(prompt.starts_with("a.txt exists"), "{prompt}");
        f.keys("k");
        assert_eq!(f.read("dir1/a (1).txt"), "content of a.txt");
        f.keys("p");
        f.keys("s");
        assert!(!f.has("dir1/a (2).txt"));
        assert_eq!(f.app.message.as_deref(), Some("nothing to paste here"));
    }

    #[test]
    fn cut_and_paste_move_and_empty_the_register() {
        let mut f = files();
        f.keys("3jxx");
        f.keys("ggjl");
        f.keys("p");
        assert_eq!(f.read("dir2/b.txt"), "content of b.txt");
        assert!(!f.has("b.txt"));
        f.keys("p");
        assert_eq!(f.app.message.as_deref(), Some("nothing to paste"));
        f.keys("u");
        assert!(f.has("b.txt") && !f.has("dir2/b.txt"));
    }

    #[test]
    fn capital_p_pastes_into_the_directory_under_the_cursor() {
        let mut f = files();
        f.keys("3jyygg");
        f.keys("jP");
        assert!(f.has("dir2/b.txt"));
        assert_eq!(
            f.app.tree().location(),
            f.p("dir2/b.txt"),
            "the cursor follows the pasted file"
        );
    }

    #[test]
    fn cutting_into_the_same_folder_does_nothing() {
        let mut f = files();
        f.keys("2jxxp");
        assert_eq!(f.app.message.as_deref(), Some("nothing to paste here"));
        assert!(f.has("a.txt"));
    }

    #[test]
    fn copying_into_the_same_folder_makes_a_numbered_copy() {
        let mut f = files();
        f.keys("2jyyp");
        assert_eq!(f.read("a (1).txt"), "content of a.txt");
        assert!(f.app.conflict_prompt().is_none());
    }

    #[test]
    fn overwrite_trashes_the_old_file_first_and_undo_brings_both_states_back() {
        let mut f = files();
        fs::write(f.p("dir2/a.txt"), "old version").unwrap();
        let dir2 = f.p("dir2");
        f.app.tree_mut().reload(&dir2);
        f.app.settle();
        f.keys("2jyyggj");
        f.keys("lp");
        assert!(f.app.conflict_prompt().is_some());
        f.keys("o");
        assert_eq!(f.read("dir2/a.txt"), "content of a.txt");
        assert_eq!(
            f.trash.item_count(),
            1,
            "the old file is in the trash, not gone"
        );
        f.keys("u");
        assert_eq!(f.read("dir2/a.txt"), "old version");
        assert_eq!(
            f.trash.item_count(),
            1,
            "the pasted copy waits in the trash"
        );
    }

    #[test]
    fn an_answer_in_capitals_applies_to_every_clash() {
        let mut f = files();
        for name in ["a.txt", "b.txt"] {
            fs::write(f.p("dir2").join(name), "old").unwrap();
        }
        let dir2 = f.p("dir2");
        f.app.tree_mut().reload(&dir2);
        f.app.settle();
        f.keys("2jv1jy");
        f.keys("ggjlp");
        f.keys("K");
        assert!(f.app.conflict_prompt().is_none(), "one answer settled both");
        assert_eq!(f.read("dir2/a (1).txt"), "content of a.txt");
        assert_eq!(f.read("dir2/b (1).txt"), "content of b.txt");
        assert_eq!(f.read("dir2/a.txt"), "old");
    }

    #[test]
    fn escape_cancels_a_paste_that_is_waiting_for_an_answer() {
        let mut f = files();
        fs::write(f.p("dir2/a.txt"), "old").unwrap();
        let dir2 = f.p("dir2");
        f.app.tree_mut().reload(&dir2);
        f.app.settle();
        f.keys("2jyyggjlp");
        f.keys("x");
        assert!(
            f.app.conflict_prompt().is_some(),
            "other keys do not answer"
        );
        f.keys("<esc>");
        assert!(f.app.conflict_prompt().is_none());
        assert_eq!(f.app.message.as_deref(), Some("paste cancelled"));
        assert_eq!(f.read("dir2/a.txt"), "old");
    }

    #[test]
    fn pasting_a_directory_into_itself_is_refused() {
        let mut f = files();
        f.keys("yyl");
        f.keys("p");
        assert!(
            f.app.message.as_deref().unwrap().contains("into itself"),
            "{:?}",
            f.app.message
        );
        assert!(!f.has("dir1/dir1"));
    }

    #[test]
    fn visual_mode_selects_a_range_and_operators_act_on_it() {
        let mut f = files();
        f.keys("2jv");
        assert!(f.app.is_visual());
        f.keys("j");
        assert_eq!(f.app.visual_range(), Some(2..=3));
        f.keys("k");
        f.keys("jj");
        assert_eq!(f.app.visual_range(), Some(2..=4));
        f.keys("d");
        assert!(!f.app.is_visual());
        assert!(!f.has("a.txt") && !f.has("b.txt") && !f.has("c.txt") && f.has("dir1"));
    }

    #[test]
    fn visual_yank_and_escape_and_leaving_the_level() {
        let mut f = files();
        f.keys("v");
        let response = f.keys("<esc>");
        assert!(!f.app.is_visual());
        assert_eq!(
            response.exit, None,
            "escape leaves visual instead of quitting"
        );
        f.keys("2jvjy");
        assert!(f.has("a.txt") && f.has("b.txt"));
        assert!(!f.app.is_visual());
        f.keys("ggv");
        f.keys("l");
        assert!(!f.app.is_visual(), "entering a directory ends visual mode");
        f.keys("v");
        f.keys("v");
        assert!(!f.app.is_visual(), "v toggles");
    }

    #[test]
    fn space_selects_entries_across_motions_and_operators_take_the_selection() {
        let mut f = files();
        f.keys("2j<space><space>");
        assert_eq!(f.app.selection().len(), 2);
        assert_eq!(f.app.tree().location(), f.p("c.txt"), "space moves down");
        f.keys("gg");
        f.keys("dd");
        assert!(!f.has("a.txt") && !f.has("b.txt") && f.has("c.txt") && f.has("dir1"));
        assert!(f.app.selection().is_empty(), "the selection is used up");
    }

    #[test]
    fn a_selection_can_be_yanked_and_pasted_elsewhere() {
        let mut f = files();
        f.keys("2j<space><space>yy");
        f.keys("ggjl");
        f.keys("p");
        assert!(f.has("dir2/a.txt") && f.has("dir2/b.txt"));
    }

    #[test]
    fn rename_prompt_starts_with_the_name_and_moves_the_cursor_to_the_result() {
        let mut f = files();
        f.keys("2jr");
        assert_eq!(f.app.prompt_view().unwrap().text, "a.txt");
        assert_eq!(f.app.prompt_view().unwrap().label, "rename: ");
        f.keys("<c-u>zebra.txt<cr>");
        assert!(!f.has("a.txt"));
        assert_eq!(f.read("zebra.txt"), "content of a.txt");
        assert_eq!(f.app.tree().location(), f.p("zebra.txt"));
        f.keys("u");
        assert!(f.has("a.txt") && !f.has("zebra.txt"));
    }

    #[test]
    fn cw_and_cc_also_rename() {
        let mut f = files();
        f.keys("2jcw<c-u>x.txt<cr>");
        assert!(f.has("x.txt"));
        f.keys("cc<c-u>y.txt<cr>");
        assert!(f.has("y.txt") && !f.has("x.txt"));
    }

    #[test]
    fn rename_refuses_clashes_paths_and_empty_names_and_leaves_files_alone() {
        let mut f = files();
        f.keys("2jr<c-u>b.txt<cr>");
        assert_eq!(f.app.message.as_deref(), Some("b.txt already exists"));
        assert_eq!(f.read("b.txt"), "content of b.txt");
        f.keys("r<c-u>sub/x<cr>");
        assert_eq!(f.app.message.as_deref(), Some("names cannot contain /"));
        f.keys("r<c-u>..<cr>");
        assert_eq!(f.app.message.as_deref(), Some(".. is not a usable name"));
        f.keys("r<c-u><cr>");
        assert_eq!(f.app.message.as_deref(), Some("the name is empty"));
        f.keys("r<cr>");
        assert!(f.has("a.txt"), "confirming the unchanged name does nothing");
        f.keys("r<esc>");
        assert!(f.app.prompt_view().is_none());
    }

    #[test]
    fn new_creates_files_and_folders_in_the_current_directory() {
        let mut f = files();
        f.keys("onotes.md<cr>");
        assert!(f.p("notes.md").is_file());
        assert_eq!(f.app.tree().location(), f.p("notes.md"));
        f.keys("oscratch/<cr>");
        assert!(f.p("scratch").is_dir());
        f.keys("onotes.md<cr>");
        assert_eq!(f.app.message.as_deref(), Some("notes.md already exists"));
        assert_eq!(
            f.read("notes.md"),
            "",
            "an existing file is never truncated"
        );
        f.keys("u");
        assert!(!f.has("scratch"));
    }

    #[test]
    fn ex_commands_create_change_modes_and_undo() {
        let mut f = files();
        f.keys(":mkdir made<cr>");
        f.keys(":touch note<cr>");
        assert!(f.p("made").is_dir() && f.p("note").is_file());
        f.keys("G");
        f.keys(":chmod 600<cr>");
        assert_eq!(
            std::os::unix::fs::MetadataExt::mode(&fs::metadata(f.p("note")).unwrap()) & 0o777,
            0o600
        );
        f.keys(":undo<cr>");
        assert_ne!(
            std::os::unix::fs::MetadataExt::mode(&fs::metadata(f.p("note")).unwrap()) & 0o777,
            0o600
        );
        f.keys(":redo<cr>");
        f.keys(":mkdir<cr>");
        assert_eq!(f.app.message.as_deref(), Some("mkdir needs a name"));
    }

    #[test]
    fn a_file_operation_that_fails_says_what_and_how_far_it_got() {
        let root = crate::testdir::tempdir();
        fs::write(root.path().join("a.txt"), "x").unwrap();
        let mut app = App::new(root.path().to_path_buf(), Keymap::default());
        app.settle();
        keys(&mut app, "dd");
        let message = app.message.clone().unwrap();
        assert!(message.contains("the trash is not available"), "{message}");
        assert!(message.ends_with("(0 of 1 done)"), "{message}");
        assert!(root.path().join("a.txt").exists());
    }

    #[test]
    fn only_one_job_runs_at_a_time_and_esc_cancels_it() {
        let mut f = files();
        f.app.press(Key::parse_seq("d").unwrap()[0]);
        f.app.press(Key::parse_seq("d").unwrap()[0]);
        assert!(f.app.running().is_some());
        f.app.press(Key::parse_seq("j").unwrap()[0]);
        f.app.press(Key::parse_seq("d").unwrap()[0]);
        f.app.press(Key::parse_seq("d").unwrap()[0]);
        assert!(
            f.app
                .message
                .as_deref()
                .unwrap()
                .starts_with("busy: deleting"),
            "{:?}",
            f.app.message
        );
        let quit = f.app.press(Key::parse_seq("q").unwrap()[0]);
        assert_eq!(quit.exit, None, "quitting waits for the job");
        assert!(
            f.app
                .message
                .as_deref()
                .unwrap()
                .starts_with("still deleting")
        );
        f.app.press(Key::parse_seq("<esc>").unwrap()[0]);
        assert_eq!(f.app.message.as_deref(), Some("cancelling…"));
        f.app.settle();
        assert!(f.app.running().is_none());
        assert!(
            f.app.message.as_deref().unwrap().starts_with("cancelled"),
            "{:?}",
            f.app.message
        );
        assert!(f.has("dir1"), "the cancelled job did nothing");
    }

    #[test]
    fn deleting_the_last_entries_leaves_a_valid_cursor() {
        let mut f = files();
        f.keys("G");
        f.keys("2dk");
        assert_eq!(f.names(), ["dir1", "dir2"]);
        assert!(f.app.tree().focused().selected().is_some());
        f.keys("gg2dd");
        assert!(f.names().is_empty());
        f.keys("dd");
        assert_eq!(f.app.message.as_deref(), Some("nothing here"));
    }

    #[test]
    fn nothing_selected_means_nothing_to_paste_rename_or_change() {
        let mut f = files();
        f.keys("p");
        assert_eq!(f.app.message.as_deref(), Some("nothing to paste"));
        f.keys("5dd");
        f.keys("r");
        assert_eq!(f.app.message.as_deref(), Some("nothing to rename"));
        f.keys(":chmod 644<cr>");
        assert_eq!(f.app.message.as_deref(), Some("nothing to change"));
    }

    fn editing(content: &str) -> (crate::testdir::TestDir, App) {
        let root = crate::testdir::tempdir();
        fs::write(root.path().join("notes.txt"), content).unwrap();
        let mut app = open(root.path());
        keys(&mut app, "l");
        (root, app)
    }

    fn on_disk(root: &crate::testdir::TestDir) -> String {
        fs::read_to_string(root.path().join("notes.txt")).unwrap()
    }

    #[test]
    fn i_opens_the_file_under_the_cursor_in_the_editor_and_keys_go_to_it() {
        let (root, mut app) = editing("hello\nworld\n");
        let editor = app.editor().expect("editing");
        assert_eq!(editor.path(), root.path().join("notes.txt"));
        keys(&mut app, "jdd");
        assert_eq!(
            app.tree().focused().cursor,
            0,
            "j moved the text cursor, not the tree"
        );
        assert!(app.editor().unwrap().dirty());
        assert_eq!(
            on_disk(&root),
            "hello\nworld\n",
            "nothing is written before :w"
        );
    }

    #[test]
    fn colon_w_writes_and_colon_q_returns_to_the_tree() {
        let (root, mut app) = editing("hello\n");
        keys(&mut app, "A there<esc>:w<cr>");
        assert_eq!(on_disk(&root), "hello there\n");
        assert!(!app.editor().unwrap().dirty());
        assert!(
            app.editor()
                .unwrap()
                .message
                .as_deref()
                .unwrap()
                .starts_with("written")
        );
        keys(&mut app, ":q<cr>");
        assert!(app.editor().is_none());
        keys(&mut app, "j");
        assert!(app.editor().is_none());
    }

    #[test]
    fn colon_wq_and_zz_save_and_close() {
        let (root, mut app) = editing("a\n");
        keys(&mut app, "ob<esc>:wq<cr>");
        assert_eq!(on_disk(&root), "a\nb\n");
        assert!(app.editor().is_none());
        keys(&mut app, "lddZZ");
        assert_eq!(on_disk(&root), "b\n");
        assert!(app.editor().is_none());
    }

    #[test]
    fn unsaved_changes_block_colon_q_until_colon_q_bang() {
        let (root, mut app) = editing("keep\n");
        keys(&mut app, "dd:q<cr>");
        assert!(app.editor().is_some());
        keys(&mut app, ":q!<cr>");
        assert!(app.editor().is_none());
        assert_eq!(on_disk(&root), "keep\n");
    }

    #[test]
    fn a_file_changed_elsewhere_is_not_overwritten_without_bang() {
        let (root, mut app) = editing("mine\n");
        keys(&mut app, "Ax<esc>");
        std::thread::sleep(std::time::Duration::from_millis(20));
        fs::write(root.path().join("notes.txt"), "someone else wrote this\n").unwrap();
        keys(&mut app, ":w<cr>");
        assert_eq!(on_disk(&root), "someone else wrote this\n");
        let message = app.editor().unwrap().message.clone().unwrap();
        assert!(message.contains("changed on disk"), "{message}");
        keys(&mut app, ":e!<cr>");
        assert!(!app.editor().unwrap().dirty());
        keys(&mut app, "Ay<esc>:w<cr>");
        assert_eq!(on_disk(&root), "someone else wrote thisy\n");
    }

    #[test]
    fn colon_w_bang_overwrites_a_file_changed_elsewhere() {
        let (root, mut app) = editing("mine\n");
        keys(&mut app, "Ax<esc>");
        fs::write(root.path().join("notes.txt"), "theirs, longer\n").unwrap();
        keys(&mut app, ":w!<cr>");
        assert_eq!(on_disk(&root), "minex\n");
    }

    #[test]
    fn a_deleted_file_is_only_written_again_with_bang() {
        let (root, mut app) = editing("text\n");
        fs::remove_file(root.path().join("notes.txt")).unwrap();
        keys(&mut app, ":w<cr>");
        assert!(
            app.editor()
                .unwrap()
                .message
                .as_deref()
                .unwrap()
                .contains("deleted")
        );
        assert!(!root.path().join("notes.txt").exists());
        keys(&mut app, ":w!<cr>");
        assert_eq!(on_disk(&root), "text\n");
    }

    #[test]
    fn a_failed_write_keeps_the_editor_open_with_the_reason() {
        let (root, mut app) = editing("text\n");
        let path = root.path().join("notes.txt");
        fs::set_permissions(&path, std::os::unix::fs::PermissionsExt::from_mode(0o444)).unwrap();
        fs::set_permissions(
            root.path(),
            std::os::unix::fs::PermissionsExt::from_mode(0o555),
        )
        .unwrap();
        keys(&mut app, "x:wq<cr>");
        fs::set_permissions(
            root.path(),
            std::os::unix::fs::PermissionsExt::from_mode(0o755),
        )
        .unwrap();
        if std::os::unix::fs::MetadataExt::uid(&fs::metadata(root.path()).unwrap()) == 0 {
            return;
        }
        assert!(app.editor().is_some(), "a failed :wq does not close");
        let message = app.editor().unwrap().message.clone().unwrap();
        assert!(message.starts_with("not written"), "{message}");
        assert_eq!(fs::read_to_string(&path).unwrap(), "text\n");
    }

    #[test]
    fn binaries_and_huge_files_go_to_the_external_opener_instead() {
        let root = crate::testdir::tempdir();
        fs::write(root.path().join("bin"), b"\x7fELF\0\0").unwrap();
        fs::write(
            root.path().join("big"),
            vec![b'a'; crate::textbuf::MAX_BYTES + 1],
        )
        .unwrap();
        let mut app = open(root.path());
        let response = keys(&mut app, "l");
        assert_eq!(response.effect, Some(Effect::Open(root.path().join("big"))));
        assert!(app.editor().is_none());
        let response = keys(&mut app, "jl");
        assert_eq!(response.effect, Some(Effect::Open(root.path().join("bin"))));
        assert!(app.editor().is_none());
    }

    #[test]
    fn the_preview_shows_the_saved_text_after_closing() {
        let (_root, mut app) = editing("before\n");
        keys(&mut app, "ccafter<esc>:wq<cr>");
        app.settle();
        let preview = app.tree().preview().expect("a preview of the file");
        let crate::model::PreviewState::Ready(content) = &preview.state else {
            panic!("preview not ready")
        };
        assert_eq!(content.lines[0].text(), "after");
    }

    #[test]
    fn the_editor_scrolls_with_the_terminal_height() {
        let body: String = (0..200).map(|i| format!("{i}\n")).collect();
        let (_root, mut app) = editing(&body);
        app.set_viewport(20);
        keys(&mut app, "G");
        assert_eq!(app.editor().unwrap().top(), 180);
    }

    /// Renders once so the app knows where its columns are, as the runtime does before any click.
    fn draw(app: &App) -> Vec<String> {
        let mut buf = ratatui::buffer::Buffer::empty(ratatui::layout::Rect::new(0, 0, 100, 13));
        crate::render::render(app, buf.area, &mut buf);
        (0..13)
            .map(|y| (0..100).map(|x| buf[(x, y)].symbol().to_string()).collect())
            .collect()
    }

    fn cell_of(lines: &[String], text: &str) -> (u16, u16) {
        let row = lines.iter().position(|l| l.contains(text)).unwrap();
        let byte = lines[row].find(text).unwrap();
        (lines[row][..byte].chars().count() as u16, row as u16)
    }

    #[test]
    fn a_click_selects_the_entry_under_the_pointer() {
        let tmp = fixture();
        let mut app = open(tmp.path());
        let lines = draw(&app);
        let (x, y) = cell_of(&lines, "notes.txt");
        app.mouse(MouseAction::Click, x, y);
        assert_eq!(selected(&app), "notes.txt");
        let lines = draw(&app);
        assert_eq!(
            cell_of(&lines, "notes.txt").1,
            6,
            "the list slid so the pick is on the cursor row"
        );
    }

    #[test]
    fn a_click_in_the_child_column_moves_the_focus_there() {
        let tmp = fixture();
        let mut app = open(tmp.path());
        keys(&mut app, "j");
        let lines = draw(&app);
        let (x, y) = cell_of(&lines, "main.rs");
        app.mouse(MouseAction::Click, x, y);
        assert_eq!(app.tree().focus(), 2);
        assert_eq!(selected(&app), "main.rs");
        let lines = draw(&app);
        let (x, y) = cell_of(&lines, "docs/");
        app.mouse(MouseAction::Click, x, y);
        assert_eq!(
            app.tree().focus(),
            1,
            "clicking a parent column goes back to it"
        );
        assert_eq!(selected(&app), "docs");
    }

    #[test]
    fn a_double_click_opens_like_l() {
        let tmp = fixture();
        let mut app = open(tmp.path());
        let lines = draw(&app);
        let (x, y) = cell_of(&lines, "src/");
        let now = std::time::Instant::now();
        assert_eq!(app.classify_click(x, y, now), MouseAction::Click);
        app.mouse(MouseAction::Click, x, y);
        assert_eq!(
            app.classify_click(x, y, now + std::time::Duration::from_millis(200)),
            MouseAction::DoubleClick
        );
        app.mouse(MouseAction::DoubleClick, x, y);
        app.settle();
        assert_eq!(app.tree().current_dir(), tmp.path().join("src"));
        let later = now + std::time::Duration::from_secs(2);
        assert_eq!(
            app.classify_click(x, y, later),
            MouseAction::Click,
            "slow clicks stay single"
        );
    }

    #[test]
    fn a_double_click_on_a_file_opens_the_editor() {
        let tmp = fixture();
        let mut app = open(tmp.path());
        let lines = draw(&app);
        let (x, y) = cell_of(&lines, "README.md");
        app.mouse(MouseAction::Click, x, y);
        app.mouse(MouseAction::DoubleClick, x, y);
        assert!(app.editor().is_some());
    }

    #[test]
    fn the_wheel_scrolls_the_column_under_the_pointer() {
        let tmp = fixture();
        let body: String = (0..100).map(|i| format!("{i}\n")).collect();
        fs::write(tmp.path().join("notes.txt"), body).unwrap();
        let mut app = open(tmp.path());
        let lines = draw(&app);
        let (x, y) = cell_of(&lines, "docs/");
        app.mouse(MouseAction::ScrollDown, x, y);
        assert_eq!(selected(&app), "Cargo.toml", "three entries down");
        app.mouse(MouseAction::ScrollUp, x, y);
        assert_eq!(selected(&app), "docs");
        keys(&mut app, "/notes<cr>");
        app.settle();
        let lines = draw(&app);
        let (px, py) = cell_of(&lines, "1 0");
        app.mouse(MouseAction::ScrollDown, px + 4, py);
        assert_eq!(app.tree().preview().unwrap().scroll, 3);
    }

    #[test]
    fn clicks_outside_any_entry_and_during_prompts_do_nothing() {
        let tmp = fixture();
        let mut app = open(tmp.path());
        draw(&app);
        app.mouse(MouseAction::Click, 2, 0);
        app.mouse(MouseAction::Click, 2, 12);
        assert_eq!(selected(&app), "docs");
        keys(&mut app, ":");
        let lines = draw(&app);
        let (x, y) = cell_of(&lines, "src/");
        app.mouse(MouseAction::Click, x, y);
        assert_eq!(selected(&app), "docs");
        assert!(app.prompt_view().is_some());
    }

    #[test]
    fn the_wheel_moves_the_editor_cursor() {
        let tmp = fixture();
        let body: String = (0..50).map(|i| format!("{i}\n")).collect();
        fs::write(tmp.path().join("notes.txt"), body).unwrap();
        let mut app = open(tmp.path());
        keys(&mut app, "/notes<cr>l");
        app.mouse(MouseAction::ScrollDown, 80, 5);
        assert_eq!(app.editor().unwrap().cursor().line, 3);
    }

    #[test]
    fn colon_images_says_how_pictures_are_drawn() {
        let tmp = fixture();
        let mut app = open(tmp.path());
        keys(&mut app, ":images<cr>");
        assert_eq!(
            app.message.as_deref(),
            Some("pictures: quadrant blocks; cell 10x20 px")
        );
    }
}

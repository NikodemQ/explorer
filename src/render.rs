use std::time::Duration;

use ratatui::{
    buffer::Buffer,
    layout::Rect,
    style::{Modifier, Style},
};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::{
    app::{App, ColumnKind, HitMap, OverlayView},
    imageview::Painter,
    layout::{self, Placed},
    model::{Entry, FilePreview, Kind, Level, Load, PreviewState},
    preview::{Content, Span},
    theme::{self, BG, DIM, FG},
};

const MIN_WIDTH: u16 = 16;
const MAX_WIDTH: u16 = 40;
const META_WIDTH: u16 = 6;
/// Blank cells between a picture and the info lines beside it.
const PANEL_GAP: u16 = 2;
const WARN: (u8, u8, u8) = (0xff, 0x9e, 0x64);
/// A listing faster than this never flashes a loading label.
const LOADING_LABEL_DELAY: Duration = Duration::from_millis(120);

#[derive(Clone, Copy, PartialEq, Eq)]
enum Role {
    /// The level holding the cursor.
    Focused,
    /// A level on the path to the focus; its selected entry is the one that was opened.
    Ancestor,
    /// The child of the directory under the cursor.
    Preview,
}

pub fn render(app: &App, area: Rect, buf: &mut Buffer) {
    fill(buf, area, Style::new().bg(theme::rgb(BG)));
    if area.height < 3 {
        return;
    }
    let tree = Rect {
        y: area.y + 1,
        height: area.height - 2,
        ..area
    };
    let center = tree.height / 2;

    let focus = app.tree().focus();
    let levels = app.tree().levels();
    let editor = app.editor();
    let preview = app.tree().preview().filter(|_| editor.is_none());
    let widths: Vec<u16> = levels.iter().map(natural_width).collect();
    let content = editor.is_some() || preview.is_some();
    // Always three columns: the parent, the focused level and what is under the cursor.
    let first = focus.saturating_sub(1);
    let mut placed = layout::place(&widths[first..], content, tree.width);
    for p in &mut placed {
        p.level += first;
    }
    app.set_hitmap(HitMap {
        columns: placed
            .iter()
            .map(|p| {
                let kind = if p.level < levels.len() {
                    ColumnKind::Level(p.level)
                } else {
                    ColumnKind::Content
                };
                (kind, tree.x + p.x, p.width)
            })
            .collect(),
        center_row: tree.y + center,
        rows: tree.y..tree.bottom(),
    });

    for p in &placed {
        if p.level >= levels.len() {
            match (editor, preview) {
                (Some(editor), _) => draw_editor(buf, tree, center, *p, editor),
                (None, Some(preview)) => {
                    draw_preview(buf, tree, center, *p, preview, app.painter());
                }
                (None, None) => {}
            }
            continue;
        }
        let role = match p.level.cmp(&focus) {
            std::cmp::Ordering::Less => Role::Ancestor,
            std::cmp::Ordering::Equal => Role::Focused,
            std::cmp::Ordering::Greater => Role::Preview,
        };
        let marks = Marks {
            selection: app.selection(),
            visual: (role == Role::Focused)
                .then(|| app.visual_range())
                .flatten(),
        };
        draw_column(buf, tree, center, *p, &levels[p.level], role, &marks);
    }
    for pair in placed.windows(2) {
        // A brace starts at the cursor row of the level on its left, which a listing still on its way does not have yet.
        if levels
            .get(pair[0].level)
            .is_some_and(|l| matches!(l.load, Load::Loading { .. }))
        {
            continue;
        }
        let (spec, color) = match (levels.get(pair[1].level), editor, preview) {
            (Some(child), _, _) => (
                layout::brace(tree.height, center, child.entries.len(), child.cursor),
                level_color(child),
            ),
            (None, Some(editor), _) => (
                layout::block_brace(tree.height, center, editor.line_count()),
                editor_color(editor),
            ),
            (None, None, Some(preview)) => (
                layout::block_brace(
                    tree.height,
                    center,
                    preview_lines(preview, app.painter(), pair[1].width, tree.height),
                ),
                preview_color(preview),
            ),
            (None, None, None) => continue,
        };
        let style = Style::new().fg(theme::rgb(color));
        let x = tree.x + pair[0].x + pair[0].width;
        for (row, cells) in layout::brace_glyphs(spec) {
            for (i, ch) in cells.into_iter().enumerate() {
                let cell_x = x + i as u16;
                if cell_x < tree.right() {
                    buf[(cell_x, tree.y + row)].set_char(ch).set_style(style);
                }
            }
        }
    }

    let header = match editor {
        Some(editor) => format!(
            "{}{}",
            tilde(editor.path()),
            if editor.dirty() { "  [+]" } else { "" }
        ),
        None => tilde(app.tree().current_dir()),
    };
    buf.set_stringn(
        area.x + 1,
        area.y,
        header,
        usize::from(area.width.saturating_sub(2)),
        Style::new().fg(theme::rgb(if editor.is_some() { FG } else { DIM })),
    );
    if app.tree().show_hidden() {
        let label = "dotfiles shown";
        let x = area
            .right()
            .saturating_sub(label.width() as u16 + 1)
            .max(area.x);
        buf.set_string(x, area.y, label, Style::new().fg(theme::rgb(DIM)));
    }
    draw_footer(buf, area, app);
    if let Some(overlay) = app.overlay() {
        draw_overlay(buf, tree, overlay);
    }
    theme::adapt(buf, app.depth());
}

fn draw_prompt_line(buf: &mut Buffer, area: Rect, label: &str, text: &str, cursor_col: usize) {
    let y = area.y + area.height - 1;
    let width = usize::from(area.width.saturating_sub(2));
    let line = format!("{label}{text}");
    buf.set_stringn(area.x + 1, y, &line, width, Style::new().fg(theme::rgb(FG)));
    let cursor_x = area.x + 1 + (label.width() + cursor_col) as u16;
    if cursor_x < area.right() {
        let cell = &mut buf[(cursor_x, y)];
        if cursor_col >= text.width() {
            cell.set_char(' ');
        }
        cell.set_style(Style::new().add_modifier(Modifier::REVERSED));
    }
}

fn draw_editor_footer(buf: &mut Buffer, area: Rect, editor: &crate::editor::Editor) {
    if let Some((label, text, cursor_col)) = editor.prompt_view() {
        return draw_prompt_line(buf, area, label, text, cursor_col);
    }
    let y = area.y + area.height - 1;
    let width = usize::from(area.width.saturating_sub(2));
    let position = format!("{}:{}", editor.cursor().line + 1, editor.cursor().col + 1);
    let pending = editor.pending();
    let right = format!("{pending}  {position}");
    let (text, color) = match (&editor.message, editor.is_insert()) {
        (Some(message), _) => (message.as_str(), WARN),
        (None, _) if editor.visual_label().is_some() => (
            if editor.visual_label() == Some("-- VISUAL LINE --") {
                "-- VISUAL LINE --   d delete  c change  y yank  J join  Esc leaves"
            } else {
                "-- VISUAL --   d delete  c change  y yank  o other end  Esc leaves"
            },
            FG,
        ),
        (None, true) => ("-- INSERT --   Esc back to normal", FG),
        (None, false) => (":w save   :q close   :wq both   i insert   u undo", DIM),
    };
    let left_width = width.saturating_sub(right.width() + 2);
    buf.set_stringn(
        area.x + 1,
        y,
        text,
        left_width,
        Style::new().fg(theme::rgb(color)),
    );
    let x = area
        .right()
        .saturating_sub(right.width() as u16 + 1)
        .max(area.x);
    buf.set_string(x, y, &right, Style::new().fg(theme::rgb(DIM)));
}

fn draw_footer(buf: &mut Buffer, area: Rect, app: &App) {
    if let Some(editor) = app.editor() {
        return draw_editor_footer(buf, area, editor);
    }
    let y = area.y + area.height - 1;
    let width = usize::from(area.width.saturating_sub(2));
    if let Some(prompt) = app.prompt_view() {
        let text = format!("{}{}", prompt.label, prompt.text);
        buf.set_stringn(area.x + 1, y, &text, width, Style::new().fg(theme::rgb(FG)));
        let cursor_x = area.x + 1 + (prompt.label.width() + prompt.cursor_col) as u16;
        if cursor_x < area.right() {
            let under = if prompt.cursor_col >= prompt.text.width() {
                " "
            } else {
                ""
            };
            let cell = &mut buf[(cursor_x, y)];
            if !under.is_empty() {
                cell.set_char(' ');
            }
            cell.set_style(Style::new().add_modifier(Modifier::REVERSED));
        }
        return;
    }
    let running = app.running().map(|(label, percent)| match percent {
        Some(p) => format!("{label}… {p}%  (Esc cancels)"),
        None => format!("{label}…  (Esc cancels)"),
    });
    let conflict = app.conflict_prompt();
    let (text, color) = match (&conflict, &app.message, &running) {
        (Some(question), _, _) => (question.as_str(), FG),
        (None, Some(message), _) => (message.as_str(), WARN),
        (None, None, Some(progress)) => (progress.as_str(), FG),
        (None, None, None) if app.is_visual() => {
            ("-- VISUAL --   d trash   y yank   x cut   Esc leaves", FG)
        }
        (None, None, None) => (
            "j/k move  l open or edit  h back  i external editor  / search  d y x p  u undo  ? help  q quit",
            DIM,
        ),
    };
    let pending = app.pending();
    let reserved = if pending.is_empty() {
        0
    } else {
        pending.width() + 3
    };
    buf.set_stringn(
        area.x + 1,
        y,
        text,
        width.saturating_sub(reserved),
        Style::new().fg(theme::rgb(color)),
    );
    if !pending.is_empty() {
        let x = area
            .right()
            .saturating_sub(pending.width() as u16 + 2)
            .max(area.x);
        buf.set_string(x, y, &pending, Style::new().fg(theme::rgb(FG)));
    }
}

/// A read-only list drawn in a box over the tree.
fn draw_overlay(buf: &mut Buffer, tree: Rect, overlay: OverlayView) {
    let content = overlay.lines;
    let hint = format!("{}   (j/k scroll, q close)", overlay.title);
    let inner_w = content
        .iter()
        .map(|l| l.width())
        .max()
        .unwrap_or(0)
        .max(hint.width())
        + 4;
    let width = (inner_w as u16 + 2).min(tree.width);
    let height = (content.len() as u16 + 4).min(tree.height);
    let area = Rect {
        x: tree.x + (tree.width - width) / 2,
        y: tree.y + (tree.height - height) / 2,
        width,
        height,
    };
    let border = Style::new().fg(theme::rgb(DIM)).bg(theme::rgb(BG));
    fill(buf, area, Style::new().bg(theme::rgb(BG)));
    let bottom = area.y + area.height - 1;
    for x in area.x..area.right() {
        buf[(x, area.y)].set_char('─').set_style(border);
        buf[(x, bottom)].set_char('─').set_style(border);
    }
    for y in area.y..=bottom {
        buf[(area.x, y)].set_char('│').set_style(border);
        buf[(area.right() - 1, y)].set_char('│').set_style(border);
    }
    for (x, y, ch) in [
        (area.x, area.y, '╭'),
        (area.right() - 1, area.y, '╮'),
        (area.x, bottom, '╰'),
        (area.right() - 1, bottom, '╯'),
    ] {
        buf[(x, y)].set_char(ch).set_style(border);
    }
    let text_w = usize::from(area.width.saturating_sub(4));
    buf.set_stringn(
        area.x + 2,
        area.y + 1,
        &hint,
        text_w,
        Style::new().fg(theme::rgb(DIM)),
    );
    let visible = usize::from(area.height.saturating_sub(4));
    let first = overlay.scroll.min(content.len().saturating_sub(visible));
    for (i, line) in content.iter().skip(first).take(visible).enumerate() {
        let y = area.y + 3 + i as u16;
        buf.set_stringn(area.x + 2, y, line, text_w, Style::new().fg(theme::rgb(FG)));
    }
}

fn fill(buf: &mut Buffer, area: Rect, style: Style) {
    let area = area.intersection(buf.area);
    for y in area.top()..area.bottom() {
        for x in area.left()..area.right() {
            buf[(x, y)].set_char(' ').set_style(style);
        }
    }
}

fn preview_color(preview: &FilePreview) -> (u8, u8, u8) {
    theme::level_color(preview.path.components().count())
}

fn preview_lines(preview: &FilePreview, painter: &Painter, width: u16, height: u16) -> usize {
    match &preview.state {
        PreviewState::Ready(content) => match picture_layout(content, painter, width, height) {
            Some(layout) => layout.rows,
            None => content.lines.len().max(1),
        },
        _ => 1,
    }
}

/// Where a preview's picture and its info lines sit in a column.
struct PictureLayout {
    picture: ratatui::layout::Size,
    /// Where the info lines start, relative to the picture's top left corner.
    info: (u16, u16),
    /// Rows the picture and its info take together.
    rows: usize,
}

/// Fits the picture into a column with its info lines beside it or under it, whichever leaves the
/// picture more cells, and beside it when both do the same.
fn picture_layout(
    content: &Content,
    painter: &Painter,
    width: u16,
    height: u16,
) -> Option<PictureLayout> {
    let image = content.image.as_ref()?;
    let lines = content.lines.len();
    let room = width.saturating_sub(1);
    let below = painter
        .fitted_size(
            image,
            ratatui::layout::Size::new(room, height.saturating_sub(lines as u16 + 1)),
        )
        .map(|picture| PictureLayout {
            picture,
            info: (0, picture.height + 1),
            rows: usize::from(picture.height) + 1 + lines,
        });
    let panel = content
        .lines
        .iter()
        .map(|l| l.text().width())
        .max()
        .unwrap_or(0) as u16
        + PANEL_GAP;
    let beside = (usize::from(height) >= lines)
        .then(|| {
            painter.fitted_size(
                image,
                ratatui::layout::Size::new(room.checked_sub(panel)?, height),
            )
        })
        .flatten()
        .map(|picture| PictureLayout {
            picture,
            info: (
                picture.width + PANEL_GAP,
                picture.height.saturating_sub(lines as u16) / 2,
            ),
            rows: usize::from(picture.height).max(lines),
        });
    let cells = |l: &PictureLayout| l.picture.width * l.picture.height;
    match (below, beside) {
        (Some(below), Some(beside)) if cells(&beside) >= cells(&below) => Some(beside),
        (below, beside) => below.or(beside),
    }
}

fn gutter_width(content: &Content) -> u16 {
    if !content.numbered {
        return 0;
    }
    content.lines.len().to_string().len().max(2) as u16 + 1
}

fn draw_preview(
    buf: &mut Buffer,
    tree: Rect,
    center: u16,
    p: Placed,
    preview: &FilePreview,
    painter: &Painter,
) {
    painter.note_room(p.width.saturating_sub(1), tree.height);
    let color = preview_color(preview);
    let x = tree.x + p.x;
    let note = |buf: &mut Buffer, text: &str| {
        let style = Style::new().fg(theme::rgb(theme::blend(color, BG, 0.6)));
        buf.set_stringn(
            x + 1,
            tree.y + center,
            text,
            usize::from(p.width.saturating_sub(1)),
            style,
        );
    };
    let content = match &preview.state {
        PreviewState::Loading { since } if since.elapsed() < LOADING_LABEL_DELAY => return,
        PreviewState::Loading { .. } => return note(buf, "(loading…)"),
        PreviewState::Failed(message) => return note(buf, &format!("({message})")),
        PreviewState::Ready(content) if content.lines.is_empty() => {
            return note(buf, "(empty file)");
        }
        PreviewState::Ready(content) => content,
    };
    if let (Some(image), Some(fit)) = (
        &content.image,
        picture_layout(content, painter, p.width, tree.height),
    ) {
        let (top, _) = layout::block_rows(tree.height, center, fit.rows);
        let area = Rect::new(x + 1, tree.y + top, fit.picture.width, fit.picture.height);
        painter.draw(&preview.path, image, area, buf);
        for (i, line) in content.lines.iter().enumerate() {
            let y = tree.y + top + fit.info.1 + i as u16;
            if y >= tree.bottom() {
                break;
            }
            let mut cx = x + 1 + fit.info.0;
            for span in &line.0 {
                let end = x + p.width;
                if cx >= end {
                    break;
                }
                let style = Style::new().fg(theme::rgb(span.color));
                (cx, _) = buf.set_stringn(cx, y, &span.text, usize::from(end - cx), style);
            }
        }
        return;
    }
    let total = content.lines.len();
    let (top, count) = layout::block_rows(tree.height, center, total);
    let scroll = preview.scroll.min(total.saturating_sub(usize::from(count)));
    let gutter = gutter_width(content).min(p.width.saturating_sub(8));
    let digits = usize::from(gutter.saturating_sub(1));
    for i in 0..usize::from(count) {
        let y = tree.y + top + i as u16;
        let line = &content.lines[scroll + i];
        if gutter > 0 {
            let number = format!("{:>digits$} ", scroll + i + 1);
            buf.set_stringn(
                x + 1,
                y,
                number,
                usize::from(gutter),
                Style::new().fg(theme::rgb(DIM)),
            );
        }
        let mut cx = x + 1 + gutter;
        let end = x + p.width;
        for Span {
            text,
            color,
            bold,
            italic,
        } in &line.0
        {
            if cx >= end {
                break;
            }
            let mut style = Style::new().fg(theme::rgb(*color));
            if *bold {
                style = style.add_modifier(Modifier::BOLD);
            }
            if *italic {
                style = style.add_modifier(Modifier::ITALIC);
            }
            (cx, _) = buf.set_stringn(cx, y, text, usize::from(end - cx), style);
        }
    }
}

fn editor_color(editor: &crate::editor::Editor) -> (u8, u8, u8) {
    theme::level_color(editor.path().components().count())
}

fn editor_gutter(editor: &crate::editor::Editor) -> u16 {
    editor.line_count().to_string().len().max(2) as u16 + 1
}

/// The file being edited, with line numbers, syntax colors and a block cursor. Long lines scroll
/// sideways together so the cursor stays in view.
fn draw_editor(
    buf: &mut Buffer,
    tree: Rect,
    center: u16,
    p: Placed,
    editor: &crate::editor::Editor,
) {
    let color = editor_color(editor);
    let total = editor.line_count();
    let (top_row, count) = layout::block_rows(tree.height, center, total);
    let gutter = editor_gutter(editor).min(p.width.saturating_sub(6));
    let text_x = tree.x + p.x + 1 + gutter;
    let text_w = usize::from(p.width.saturating_sub(1 + gutter)).max(1);
    let cursor = editor.cursor();
    let cursor_col = editor.cursor_display_col();
    let left = cursor_col.saturating_sub(text_w - 1);
    let digits = usize::from(gutter.saturating_sub(1));
    for i in 0..usize::from(count) {
        let index = editor.top() + i;
        if index >= total {
            break;
        }
        let y = tree.y + top_row + i as u16;
        let current = index == cursor.line;
        let number_style = Style::new().fg(theme::rgb(if current { color } else { DIM }));
        buf.set_stringn(
            tree.x + p.x + 1,
            y,
            format!("{:>digits$} ", index + 1),
            usize::from(gutter),
            number_style,
        );
        draw_code_line(buf, text_x, y, text_w, left, &editor.styled_line(index));
        if let Some((from, to)) = selected_cols(editor, index) {
            let tint = Style::new().bg(theme::rgb(theme::blend(color, BG, 0.35)));
            for col in from.max(left)..to {
                let x = text_x + (col - left) as u16;
                if col - left >= text_w || !buf.area.contains(ratatui::layout::Position::new(x, y))
                {
                    break;
                }
                buf[(x, y)].set_style(tint);
            }
        }
        if current {
            let x = text_x + (cursor_col - left) as u16;
            if x < tree.x + p.x + p.width && buf.area.contains(ratatui::layout::Position::new(x, y))
            {
                let cell = &mut buf[(x, y)];
                if cell.symbol().trim().is_empty() {
                    cell.set_char(' ');
                }
                let style = if editor.is_insert() {
                    Style::new().bg(theme::rgb(FG)).fg(theme::rgb(BG))
                } else {
                    Style::new().bg(theme::rgb(color)).fg(theme::rgb(BG))
                };
                cell.set_style(style);
            }
        }
    }
}

/// Display columns of line `index` covered by the editor's selection, end exclusive. A selected
/// empty line, or the line break of a line the selection continues past, shows as one cell.
fn selected_cols(editor: &crate::editor::Editor, index: usize) -> Option<(usize, usize)> {
    use crate::editor::display_col;
    let (a, b, lines) = editor.selection()?;
    if index < a.line || index > b.line {
        return None;
    }
    let text = editor.line_text(index);
    let width = display_col(text, usize::MAX);
    if lines {
        return Some((0, width.max(1)));
    }
    let from = if index == a.line {
        display_col(text, a.col)
    } else {
        0
    };
    let to = if index == b.line {
        display_col(text, b.col + 1).max(from + 1)
    } else {
        width + 1
    };
    Some((from, to))
}

/// Draws one line of code from display column `left`, expanding tabs and showing control characters as dots.
fn draw_code_line(
    buf: &mut Buffer,
    x: u16,
    y: u16,
    width: usize,
    left: usize,
    line: &crate::preview::Line,
) {
    use unicode_segmentation::UnicodeSegmentation;
    let mut col = 0usize;
    for Span {
        text,
        color,
        bold,
        italic,
    } in &line.0
    {
        let mut style = Style::new().fg(theme::rgb(*color));
        if *bold {
            style = style.add_modifier(Modifier::BOLD);
        }
        if *italic {
            style = style.add_modifier(Modifier::ITALIC);
        }
        for g in text.graphemes(true) {
            let (shown, w) = match g {
                "\t" => (" ", 4 - col % 4),
                g if g.chars().any(char::is_control) => ("·", 1),
                g => (g, g.width().max(1)),
            };
            for k in 0..w {
                let at = col + k;
                if at >= left && at - left < width {
                    let cell_x = x + (at - left) as u16;
                    if !buf.area.contains(ratatui::layout::Position::new(cell_x, y)) {
                        return;
                    }
                    let symbol = if g == "\t" || k > 0 { " " } else { shown };
                    if k == 0 || g == "\t" {
                        buf[(cell_x, y)].set_symbol(symbol).set_style(style);
                    }
                }
            }
            col += w;
            if col >= left + width {
                return;
            }
        }
    }
}

fn level_color(level: &Level) -> (u8, u8, u8) {
    theme::level_color(level.dir.components().count())
}

fn natural_width(level: &Level) -> u16 {
    let longest = level
        .entries
        .iter()
        .map(|e| display(e).width())
        .max()
        .unwrap_or("(empty)".len()) as u16;
    (longest + META_WIDTH + 3).clamp(MIN_WIDTH, MAX_WIDTH)
}

/// What the rows of a column need to know beyond the listing itself.
struct Marks<'a> {
    selection: &'a std::collections::BTreeSet<std::path::PathBuf>,
    /// Rows covered by visual mode. Only the focused column has any.
    visual: Option<std::ops::RangeInclusive<usize>>,
}

fn draw_column(
    buf: &mut Buffer,
    tree: Rect,
    center: u16,
    p: Placed,
    level: &Level,
    role: Role,
    marks: &Marks,
) {
    let color = level_color(level);
    let x = tree.x + p.x;

    if level.entries.is_empty() {
        let text = match level.load {
            Load::Failed(kind) => format!("({})", kind.to_string().to_lowercase()),
            Load::Loading { since } if since.elapsed() < LOADING_LABEL_DELAY => return,
            Load::Loading { .. } => "(loading…)".to_string(),
            Load::Ready => "(empty)".to_string(),
        };
        let style = Style::new().fg(theme::rgb(theme::blend(color, BG, 0.5)));
        buf.set_stringn(x + 1, tree.y + center, text, usize::from(p.width), style);
        return;
    }

    for i in layout::visible_range(level.entries.len(), level.cursor, center, tree.height) {
        let row = layout::row_y(center, level.cursor, i) as u16;
        let y = tree.y + row;
        let entry = &level.entries[i];
        let is_cursor = i == level.cursor;

        let base = if entry.is_dir() {
            color
        } else {
            theme::blend(color, FG, 0.55)
        };
        let mut style = Style::new().fg(theme::rgb(base));
        if is_cursor {
            let bg = match role {
                Role::Focused => color,
                Role::Ancestor => theme::blend(color, BG, 0.24),
                Role::Preview => theme::blend(color, BG, 0.12),
            };
            style = Style::new().bg(theme::rgb(bg)).fg(theme::rgb(base));
            if role == Role::Focused {
                style = style.fg(theme::rgb(BG)).add_modifier(Modifier::BOLD);
            }
            fill(
                buf,
                Rect {
                    x,
                    y,
                    width: p.width,
                    height: 1,
                },
                style,
            );
        } else if marks.visual.as_ref().is_some_and(|r| r.contains(&i)) {
            style = style.bg(theme::rgb(theme::blend(color, BG, 0.24)));
            fill(
                buf,
                Rect {
                    x,
                    y,
                    width: p.width,
                    height: 1,
                },
                style,
            );
        }
        if marks.selection.contains(&level.dir.join(&entry.name)) {
            let marker = if is_cursor && role == Role::Focused {
                style
            } else {
                style.fg(theme::rgb(color)).add_modifier(Modifier::BOLD)
            };
            buf.set_string(x, y, "●", marker);
        }

        let meta = meta(entry);
        let name_width = usize::from(p.width.saturating_sub(META_WIDTH + 3));
        buf.set_stringn(
            x + 1,
            y,
            fit(&display(entry), name_width),
            name_width,
            style,
        );
        let meta_style = if is_cursor && role == Role::Focused {
            style
        } else {
            style.fg(theme::rgb(DIM))
        };
        let meta_x = (x + p.width).saturating_sub(1 + meta.width() as u16).max(x);
        buf.set_string(meta_x, y, meta, meta_style);
    }
}

fn display(entry: &Entry) -> String {
    let name = entry.display_name();
    match &entry.kind {
        Kind::Dir | Kind::Symlink { to_dir: true, .. } => format!("{name}/"),
        Kind::Symlink { target, .. } => format!("{name} → {}", target.display()),
        _ => name,
    }
}

fn meta(entry: &Entry) -> String {
    match entry.kind {
        Kind::File => human_size(entry.size),
        _ => String::new(),
    }
}

fn human_size(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["", "K", "M", "G", "T"];
    let mut size = bytes as f64;
    let mut unit = 0;
    while size >= 1024.0 && unit < UNITS.len() - 1 {
        size /= 1024.0;
        unit += 1;
    }
    if unit == 0 || size >= 10.0 {
        format!("{:.0}{}", size, UNITS[unit])
    } else {
        format!("{:.1}{}", size, UNITS[unit])
    }
}

fn fit(text: &str, width: usize) -> String {
    if text.width() <= width {
        return text.to_string();
    }
    let mut out = String::new();
    let mut used = 0;
    for ch in text.chars() {
        let w = ch.width().unwrap_or(0);
        if used + w + 1 > width {
            break;
        }
        out.push(ch);
        used += w;
    }
    out.push('…');
    out
}

fn tilde(path: &std::path::Path) -> String {
    let shown = path.display().to_string();
    match std::env::var("HOME") {
        Ok(home) if !home.is_empty() => match shown.strip_prefix(&home) {
            Some(rest) if rest.is_empty() || rest.starts_with('/') => format!("~{rest}"),
            _ => shown,
        },
        _ => shown,
    }
}

#[cfg(test)]
mod tests {
    use std::fs;

    use ratatui::{Terminal, backend::TestBackend};

    use crate::keys::{Key, Keymap};

    use super::*;

    fn rows(app: &App, w: u16, h: u16) -> (Vec<String>, Buffer) {
        let mut terminal = Terminal::new(TestBackend::new(w, h)).unwrap();
        terminal
            .draw(|f| render(app, f.area(), f.buffer_mut()))
            .unwrap();
        // Pictures are encoded after the first frame asks for them, as the runtime's encoder thread would.
        app.painter().run_jobs_now();
        terminal
            .draw(|f| render(app, f.area(), f.buffer_mut()))
            .unwrap();
        let buf = terminal.backend().buffer().clone();
        let lines = (0..h)
            .map(|y| {
                (0..w)
                    .map(|x| buf[(x, y)].symbol().to_string())
                    .collect::<String>()
            })
            .collect();
        (lines, buf)
    }

    fn open(path: &std::path::Path) -> App {
        let mut app = App::new(path.to_path_buf(), Keymap::default());
        app.tree_mut().settle();
        app
    }

    fn keys(app: &mut App, text: &str) {
        for key in Key::parse_seq(text).unwrap() {
            app.press(key);
            app.settle();
        }
    }

    fn fixture() -> crate::testdir::TestDir {
        let tmp = crate::testdir::tempdir();
        let r = tmp.path();
        fs::create_dir_all(r.join("alpha/inner")).unwrap();
        fs::create_dir_all(r.join("beta")).unwrap();
        fs::write(r.join("alpha/one.txt"), "x").unwrap();
        fs::write(r.join("alpha/two.txt"), "xx").unwrap();
        fs::write(r.join("file.md"), "hello").unwrap();
        tmp
    }

    #[test]
    fn selected_entries_of_all_levels_share_one_row_joined_by_a_brace() {
        let tmp = fixture();
        let app = open(tmp.path());
        let (lines, _) = rows(&app, 100, 11);
        // tree area is rows 1..10, center row is 1 + 9 / 2 = 5
        let center = &lines[5];
        assert!(center.contains("alpha/"), "{center:?}");
        assert!(center.contains("inner/"), "{center:?}");
        // inner/ is the first entry of alpha, so the tip is a tee opening downward
        assert!(
            center.contains("─┬─"),
            "brace tip missing on the center row: {center:?}"
        );
        assert!(lines[6].contains('│'), "brace spine missing: {lines:#?}");
    }

    #[test]
    fn brace_tip_is_a_left_tee_when_the_child_cursor_is_in_the_middle() {
        let tmp = fixture();
        let mut app = open(tmp.path());
        keys(&mut app, "l");
        keys(&mut app, "j");
        let (lines, _) = rows(&app, 90, 11);
        assert!(lines[5].contains("─┤"), "{lines:#?}");
        assert!(
            lines[4].contains('╭') && lines[6].contains('╰'),
            "{lines:#?}"
        );
    }

    #[test]
    fn moving_down_slides_the_list_and_swaps_the_child() {
        let tmp = fixture();
        let mut app = open(tmp.path());
        keys(&mut app, "j");
        let (lines, _) = rows(&app, 100, 11);
        assert!(lines[5].contains("beta/"), "{:?}", lines[5]);
        assert!(lines[4].contains("alpha/"), "{:?}", lines[4]);
        assert!(lines.iter().all(|l| !l.contains("one.txt")), "{lines:#?}");
        assert!(lines[5].contains("(empty)"), "{:?}", lines[5]);
    }

    #[test]
    fn levels_are_colored_by_depth_and_cursor_row_is_filled() {
        let tmp = fixture();
        let app = open(tmp.path());
        let (lines, buf) = rows(&app, 100, 11);
        let x = lines[5].find("alpha/").unwrap();
        let x = lines[5][..x].chars().count() as u16;
        let depth = tmp.path().components().count();
        let expect = theme::rgb(theme::level_color(depth));
        assert_eq!(buf[(x, 5)].bg, expect);
        let child = lines[5].find("inner/").unwrap();
        let child_x = lines[5][..child].chars().count() as u16;
        let child_color = theme::rgb(theme::level_color(depth + 1));
        assert_ne!(child_color, expect);
        assert_eq!(buf[(child_x, 5)].fg, child_color);
    }

    #[test]
    fn tiny_terminals_do_not_panic() {
        let tmp = fixture();
        let app = open(tmp.path());
        for (w, h) in [(1, 1), (5, 2), (10, 3), (20, 4), (3, 40)] {
            rows(&app, w, h);
        }
    }

    #[test]
    fn a_deep_path_in_a_narrow_terminal_keeps_all_three_levels_on_screen() {
        let tmp = crate::testdir::tempdir();
        let deep = tmp
            .path()
            .join("aaaaaaaaaaaa/bbbbbbbbbbbb/cccccccccccc/dddddddddddd/eeeeeeeeeeee");
        fs::create_dir_all(deep.join("leaf")).unwrap();
        fs::write(deep.join("readme.txt"), "x").unwrap();
        let mut app = open(tmp.path());
        for _ in 0..5 {
            keys(&mut app, "l");
        }
        assert_eq!(app.tree().focus(), 6);
        let (lines, _) = rows(&app, 50, 9);
        let center = &lines[4];
        assert!(
            center.starts_with(" eeee"),
            "the parent is the leftmost column, shrunk rather than dropped: {center:?}"
        );
        assert!(
            center.contains("leaf/"),
            "the focus stays visible: {center:?}"
        );
        assert!(
            center.contains("(empty)"),
            "the preview of leaf/ stays visible: {center:?}"
        );
        assert!(
            !center.contains("dddd"),
            "levels above the parent are not shown: {center:?}"
        );
        let (wide, _) = rows(&app, 140, 9);
        assert!(
            wide[4].contains("eeeeeeeeeeee/"),
            "the parent is whole once it fits: {:?}",
            wide[4]
        );
        assert!(lines.iter().all(|l| l.chars().count() == 50));
    }

    #[test]
    fn the_first_column_is_flush_left() {
        let tmp = fixture();
        let app = open(tmp.path());
        let (lines, buf) = rows(&app, 100, 11);
        assert!(
            lines[5].starts_with(" root/"),
            "the parent of the start: {:?}",
            lines[5]
        );
        assert_ne!(
            buf[(0, 5)].bg,
            theme::rgb(BG),
            "cursor highlight starts at column 0"
        );
    }

    #[test]
    fn a_pending_listing_renders_blank_instead_of_flashing_a_label() {
        let tmp = fixture();
        let app = App::new(tmp.path().to_path_buf(), Keymap::default());
        let (lines, _) = rows(&app, 100, 11);
        assert!(lines[5].trim().is_empty(), "{:?}", lines[5]);
    }

    fn footer(lines: &[String]) -> &str {
        lines.last().unwrap().trim()
    }

    #[test]
    fn typing_a_search_shows_the_prompt_and_jumps_the_cursor_row_to_the_match() {
        let tmp = fixture();
        let mut app = open(tmp.path());
        keys(&mut app, "/file");
        let (lines, _) = rows(&app, 100, 11);
        assert_eq!(footer(&lines), "/file");
        assert!(
            lines[5].contains("file.md"),
            "match sits on the center row: {:?}",
            lines[5]
        );
    }

    #[test]
    fn a_failed_search_reports_in_the_footer_after_enter() {
        let tmp = fixture();
        let mut app = open(tmp.path());
        keys(&mut app, "/zzz<cr>");
        let (lines, _) = rows(&app, 100, 11);
        assert_eq!(footer(&lines), "pattern not found: zzz");
    }

    #[test]
    fn the_prompt_cursor_is_drawn_as_a_reversed_cell() {
        let tmp = fixture();
        let mut app = open(tmp.path());
        keys(&mut app, "/ab<left>");
        let (lines, buf) = rows(&app, 100, 11);
        assert_eq!(footer(&lines), "/ab");
        assert!(
            buf[(3, 10)].modifier.contains(Modifier::REVERSED),
            "cursor sits on b"
        );
        assert!(!buf[(2, 10)].modifier.contains(Modifier::REVERSED));
        keys(&mut app, "<end>");
        let (_, buf) = rows(&app, 100, 11);
        assert!(
            buf[(4, 10)].modifier.contains(Modifier::REVERSED),
            "cursor sits past the end"
        );
    }

    #[test]
    fn a_pending_count_shows_at_the_right_edge_of_the_footer() {
        let tmp = fixture();
        let mut app = open(tmp.path());
        keys(&mut app, "12g");
        let (lines, _) = rows(&app, 100, 11);
        assert!(lines[10].trim_end().ends_with("12g"), "{:?}", lines[10]);
    }

    #[test]
    fn the_help_overlay_lists_the_live_bindings_and_follows_rebinding() {
        let tmp = fixture();
        let mut app = open(tmp.path());
        keys(&mut app, "?");
        let (lines, _) = rows(&app, 80, 30);
        let text = lines.join("\n");
        assert!(text.contains("move down"), "{text}");
        assert!(text.contains("gg"), "{text}");
        assert!(text.contains("╭") && text.contains("╯"), "{text}");
        keys(&mut app, "q");
        let (lines, _) = rows(&app, 80, 30);
        assert!(!lines.join("\n").contains("move down"));
    }

    #[test]
    fn the_help_overlay_fits_a_small_terminal() {
        let tmp = fixture();
        let mut app = open(tmp.path());
        keys(&mut app, "?");
        for (w, h) in [(20, 6), (40, 9), (5, 3), (120, 60)] {
            rows(&app, w, h);
        }
    }

    fn numbered_file(dir: &std::path::Path, name: &str, lines: usize) {
        let body: String = (1..=lines).map(|i| format!("row {i}\n")).collect();
        fs::write(dir.join(name), body).unwrap();
    }

    #[test]
    fn a_file_under_the_cursor_is_previewed_beside_a_brace_with_line_numbers() {
        let tmp = crate::testdir::tempdir();
        fs::write(tmp.path().join("hello.txt"), "alpha\nbeta\ngamma\n").unwrap();
        let app = open(tmp.path());
        let (lines, _) = rows(&app, 80, 11);
        assert!(lines[5].contains("hello.txt"), "{lines:#?}");
        assert!(
            lines[4].contains("1 alpha") || lines.iter().any(|l| l.contains(" 1 alpha")),
            "{lines:#?}"
        );
        assert!(lines.iter().any(|l| l.contains(" 3 gamma")), "{lines:#?}");
        let tip = lines
            .iter()
            .position(|l| l.contains("─┤"))
            .expect("brace tip");
        assert!(
            lines[tip].contains("hello.txt") || lines[tip].contains("2 beta"),
            "tip is on the center row: {lines:#?}"
        );
        assert_eq!(tip, 5);
    }

    #[test]
    fn a_long_file_fills_the_height_and_its_brace_stays_open_at_the_bottom() {
        let tmp = crate::testdir::tempdir();
        numbered_file(tmp.path(), "long.txt", 200);
        let app = open(tmp.path());
        let (lines, _) = rows(&app, 80, 15);
        let tree_rows = &lines[1..14];
        assert!(tree_rows[0].contains("row 1"), "{lines:#?}");
        assert!(tree_rows[12].contains("row 13"), "{lines:#?}");
        let spine: Vec<_> = tree_rows
            .iter()
            .map(|l| l.chars().any(|c| "╭│┤┬".contains(c)))
            .collect();
        assert!(
            spine.iter().all(|&b| b),
            "the brace covers every row: {lines:#?}"
        );
        assert!(
            !tree_rows[12].contains('╰'),
            "open end where content continues: {lines:#?}"
        );
    }

    #[test]
    fn capital_j_and_k_scroll_the_preview() {
        let tmp = crate::testdir::tempdir();
        numbered_file(tmp.path(), "long.txt", 200);
        let mut app = open(tmp.path());
        app.set_viewport(13);
        keys(&mut app, "10J");
        let (lines, _) = rows(&app, 80, 15);
        assert!(lines[1].contains("row 11"), "{lines:#?}");
        keys(&mut app, "3K");
        let (lines, _) = rows(&app, 80, 15);
        assert!(lines[1].contains("row 8"), "{lines:#?}");
        keys(&mut app, "500J");
        let (lines, _) = rows(&app, 80, 15);
        assert!(
            lines[13].contains("row 200"),
            "the last page stays full: {lines:#?}"
        );
        assert!(lines[1].contains("row 188"), "{lines:#?}");
    }

    #[test]
    fn preview_shrinks_to_the_screen_instead_of_hiding_the_directory_columns() {
        let tmp = fixture();
        let wide = "x".repeat(300);
        fs::write(tmp.path().join("alpha/wide.txt"), &wide).unwrap();
        let mut app = open(tmp.path());
        keys(&mut app, "l");
        keys(&mut app, "G");
        let (lines, _) = rows(&app, 100, 11);
        assert!(lines[5].contains("wide.txt"), "{:?}", lines[5]);
        assert!(
            lines[5].contains("alpha/"),
            "parent column is kept: {:?}",
            lines[5]
        );
        assert!(lines.iter().all(|l| l.chars().count() == 100));
    }

    #[test]
    fn syntax_colors_reach_the_screen() {
        let tmp = crate::testdir::tempdir();
        fs::write(tmp.path().join("a.rs"), "fn main() { let x = 1; }\n").unwrap();
        let app = open(tmp.path());
        let (lines, buf) = rows(&app, 80, 11);
        let row = lines.iter().position(|l| l.contains("fn main")).unwrap();
        let col = lines[row].find("fn main").unwrap();
        let fg_fn = buf[(col as u16, row as u16)].fg;
        let fg_main = buf[(col as u16 + 3, row as u16)].fg;
        assert_ne!(fg_fn, fg_main, "keyword and identifier differ");
    }

    #[test]
    fn a_failed_preview_says_why() {
        let tmp = crate::testdir::tempdir();
        fs::write(tmp.path().join("bad.zip"), b"not a zip").unwrap();
        let app = open(tmp.path());
        let (lines, _) = rows(&app, 80, 11);
        assert!(
            lines[5].contains("bad.zip") && lines[5].contains('('),
            "{lines:#?}"
        );
    }

    fn plain_files() -> crate::testdir::TestDir {
        let tmp = crate::testdir::tempdir();
        for f in ["a.txt", "b.txt", "c.txt", "d.txt"] {
            fs::write(tmp.path().join(f), f).unwrap();
        }
        tmp
    }

    fn row_of(lines: &[String], text: &str) -> usize {
        lines.iter().position(|l| l.contains(text)).unwrap()
    }

    #[test]
    fn selected_entries_carry_a_marker_and_others_do_not() {
        let tmp = plain_files();
        let mut app = open(tmp.path());
        keys(&mut app, "<space><space>");
        let (lines, _) = rows(&app, 80, 15);
        let before = |name: &str| {
            let line = &lines[row_of(&lines, name)];
            line[..line.find(name).unwrap()].chars().last().unwrap()
        };
        assert_eq!(before("a.txt"), '●', "{lines:#?}");
        assert_eq!(before("b.txt"), '●', "{lines:#?}");
        assert_eq!(before("c.txt"), ' ', "{lines:#?}");
    }

    #[test]
    fn visual_mode_tints_the_covered_rows_and_says_so_in_the_footer() {
        let tmp = plain_files();
        let mut app = open(tmp.path());
        keys(&mut app, "vj");
        let (lines, buf) = rows(&app, 80, 15);
        let (a, b, c) = (
            row_of(&lines, "a.txt") as u16,
            row_of(&lines, "b.txt") as u16,
            row_of(&lines, "c.txt") as u16,
        );
        let x = lines[a as usize][..lines[a as usize].find("a.txt").unwrap()]
            .chars()
            .count() as u16
            + 2;
        assert_ne!(buf[(x, a)].bg, theme::rgb(BG), "anchor row is tinted");
        assert_ne!(buf[(x, b)].bg, theme::rgb(BG), "cursor row is filled");
        assert_ne!(
            buf[(x, a)].bg,
            buf[(x, b)].bg,
            "the cursor row is stronger than the range"
        );
        assert_eq!(
            buf[(x, c)].bg,
            theme::rgb(BG),
            "rows outside the range are untouched"
        );
        assert!(
            footer(&lines).starts_with("-- VISUAL --"),
            "{:?}",
            footer(&lines)
        );
    }

    #[test]
    fn a_waiting_paste_asks_its_question_in_the_footer() {
        let tmp = plain_files();
        fs::create_dir(tmp.path().join("dir")).unwrap();
        fs::write(tmp.path().join("dir/a.txt"), "old").unwrap();
        let mut app = open(tmp.path());
        keys(&mut app, "jyygglp");
        let (lines, _) = rows(&app, 100, 15);
        assert!(
            footer(&lines).starts_with("a.txt exists: [s]kip [o]verwrite [k]eep both"),
            "{:?}",
            footer(&lines)
        );
    }

    #[test]
    fn a_running_job_shows_in_the_footer_with_its_cancel_hint() {
        let tmp = plain_files();
        let mut app = open(tmp.path());
        for key in Key::parse_seq("dd").unwrap() {
            app.press(key);
        }
        let (lines, _) = rows(&app, 100, 15);
        assert_eq!(footer(&lines), "deleting a.txt…  (Esc cancels)");
        let id = app.take_jobs()[0].id;
        app.job_progress(id, 30, 120);
        let (lines, _) = rows(&app, 100, 15);
        assert_eq!(footer(&lines), "deleting a.txt… 25%  (Esc cancels)");
    }

    #[test]
    fn the_rename_prompt_shows_its_label_and_puts_the_cursor_after_the_name() {
        let tmp = plain_files();
        let mut app = open(tmp.path());
        keys(&mut app, "r");
        let (lines, buf) = rows(&app, 80, 15);
        assert_eq!(footer(&lines), "rename: a.txt");
        assert!(
            buf[(1 + 8 + 5, 14)].modifier.contains(Modifier::REVERSED),
            "cursor is past the end of the name"
        );
        keys(&mut app, "<esc>o");
        let (lines, _) = rows(&app, 80, 15);
        assert!(
            footer(&lines).starts_with("new (end with / for a folder):"),
            "{:?}",
            footer(&lines)
        );
    }

    fn edit(content: &str) -> (crate::testdir::TestDir, App) {
        let tmp = crate::testdir::tempdir();
        fs::write(tmp.path().join("code.rs"), content).unwrap();
        let mut app = open(tmp.path());
        keys(&mut app, "l");
        (tmp, app)
    }

    #[test]
    fn the_editor_replaces_the_preview_with_numbered_lines_and_a_cursor() {
        let (_tmp, mut app) = edit("fn main() {\n    println!(\"hi\");\n}\n");
        keys(&mut app, "jw");
        let (lines, buf) = rows(&app, 80, 11);
        let row = row_of(&lines, "println");
        assert!(lines[row].contains(" 2     println!"), "{lines:#?}");
        let col = lines[row].find("println").unwrap();
        let x = lines[row][..col].chars().count() as u16;
        assert_ne!(
            buf[(x, row as u16)].bg,
            theme::rgb(BG),
            "the block cursor sits on p"
        );
        assert_eq!(buf[(x + 1, row as u16)].bg, theme::rgb(BG));
        assert!(
            lines[0].contains("code.rs"),
            "header names the file: {:?}",
            lines[0]
        );
        assert!(footer(&lines).contains(":w save"), "{:?}", footer(&lines));
        assert!(
            footer(&lines).ends_with("2:5"),
            "position is shown: {:?}",
            footer(&lines)
        );
    }

    #[test]
    fn edits_mark_the_header_and_insert_mode_shows_in_the_footer() {
        let (_tmp, mut app) = edit("x\n");
        keys(&mut app, "A");
        let (lines, _) = rows(&app, 80, 11);
        assert!(
            footer(&lines).starts_with("-- INSERT --"),
            "{:?}",
            footer(&lines)
        );
        keys(&mut app, "yz");
        let (lines, _) = rows(&app, 80, 11);
        assert!(lines[0].trim_end().ends_with("[+]"), "{:?}", lines[0]);
        assert!(lines.iter().any(|l| l.contains("1 xyz")), "{lines:#?}");
    }

    #[test]
    fn the_editor_prompt_takes_the_footer() {
        let (_tmp, mut app) = edit("x\n");
        keys(&mut app, ":wq");
        let (lines, _) = rows(&app, 80, 11);
        assert_eq!(footer(&lines), ":wq");
    }

    #[test]
    fn tabs_expand_to_tab_stops_and_long_lines_scroll_sideways() {
        let (_tmp, app) = edit("\tindented\n");
        let (lines, _) = rows(&app, 80, 11);
        assert!(
            lines.iter().any(|l| l.contains(" 1     indented")),
            "{lines:#?}"
        );
        let long = format!("start{}end\n", "-".repeat(300));
        let (_tmp, mut app) = edit(&long);
        keys(&mut app, "$");
        let (lines, _) = rows(&app, 80, 11);
        let row = row_of(&lines, "end");
        assert!(
            !lines[row].contains("start"),
            "scrolled to the end of the line: {:?}",
            lines[row]
        );
        keys(&mut app, "0");
        let (lines, _) = rows(&app, 80, 11);
        assert!(lines.iter().any(|l| l.contains("start")));
    }

    #[test]
    fn the_editor_fits_tiny_terminals() {
        let (_tmp, mut app) = edit("some text\nmore\n");
        keys(&mut app, "A long insert<esc>");
        for (w, h) in [(1, 1), (5, 3), (12, 4), (30, 6), (200, 60)] {
            rows(&app, w, h);
        }
    }

    #[test]
    fn the_tree_keeps_to_its_share_of_the_width_and_the_preview_fills_the_rest() {
        let tmp = crate::testdir::tempdir();
        let deep = tmp
            .path()
            .join("aaaaaaaaaaaaaaaaaa/bbbbbbbbbbbbbbbbbb/cccccccccccccccccc");
        fs::create_dir_all(&deep).unwrap();
        fs::write(deep.join("file.txt"), "x".repeat(200)).unwrap();
        let mut app = open(tmp.path());
        keys(&mut app, "lll");
        let (lines, _) = rows(&app, 120, 9);
        let center = &lines[4];
        assert!(center.contains("file.txt"), "{center:?}");
        let tip = center.find("───").unwrap();
        assert!(
            center[..tip].chars().count() <= 60 + 3,
            "folders stay in the left half: {center:?}"
        );
        assert!(
            center.trim_end().chars().count() >= 115,
            "the preview reaches the right edge: {center:?}"
        );
    }

    #[test]
    fn an_editor_selection_is_tinted_and_named_in_the_footer() {
        let (_tmp, mut app) = edit("alpha beta\ngamma\n");
        keys(&mut app, "wvl");
        let (lines, buf) = rows(&app, 80, 11);
        let row = row_of(&lines, "alpha beta");
        let b = lines[row].find("beta").unwrap();
        let x = lines[row][..b].chars().count() as u16;
        let y = row as u16;
        assert_ne!(buf[(x, y)].bg, theme::rgb(BG), "b is selected");
        assert_ne!(buf[(x + 1, y)].bg, theme::rgb(BG), "e is under the cursor");
        assert_eq!(buf[(x + 2, y)].bg, theme::rgb(BG), "t is outside");
        assert_eq!(buf[(x - 2, y)].bg, theme::rgb(BG), "alpha is outside");
        assert!(
            footer(&lines).starts_with("-- VISUAL --"),
            "{:?}",
            footer(&lines)
        );
        keys(&mut app, "<esc>Vj");
        let (lines, buf) = rows(&app, 80, 11);
        let row = row_of(&lines, "gamma") as u16;
        let g = lines[row as usize].find("gamma").unwrap();
        let gx = lines[row as usize][..g].chars().count() as u16;
        assert_ne!(
            buf[(gx + 3, row)].bg,
            theme::rgb(BG),
            "whole lines are tinted"
        );
        assert!(
            footer(&lines).starts_with("-- VISUAL LINE --"),
            "{:?}",
            footer(&lines)
        );
    }

    fn write_png(path: &std::path::Path, w: u32, h: u32) {
        let img = image::RgbImage::from_fn(w, h, |x, _| {
            if x < w / 2 {
                image::Rgb([230, 20, 20])
            } else {
                image::Rgb([20, 20, 230])
            }
        });
        img.save(path).unwrap();
    }

    #[test]
    fn a_picture_is_drawn_in_the_preview_with_its_card_under_it() {
        let tmp = crate::testdir::tempdir();
        write_png(&tmp.path().join("photo.png"), 200, 100);
        let app = open(tmp.path());
        // Too narrow for the card to fit beside the picture.
        let (lines, buf) = rows(&app, 80, 30);
        let card = row_of(&lines, "image/png");
        assert!(lines[card].contains("200×100"), "{lines:#?}");
        let tip = row_of(&lines, "─┤");
        let brace_x = lines[tip].chars().position(|c| c == '┤').unwrap() as u16;
        let reds_and_blues: Vec<_> = (brace_x + 3..80)
            .map(|x| buf[(x, (card - 2) as u16)].bg)
            .filter(|c| *c != theme::rgb(BG) && *c != ratatui::style::Color::Reset)
            .collect();
        assert!(
            reds_and_blues.len() > 10,
            "half blocks fill the row above the card: {lines:#?}"
        );
        assert_ne!(
            reds_and_blues.first(),
            reds_and_blues.last(),
            "red on the left, blue on the right"
        );
    }

    #[test]
    fn a_tall_picture_in_a_wide_column_has_its_card_beside_it() {
        let tmp = crate::testdir::tempdir();
        write_png(&tmp.path().join("photo.png"), 100, 300);
        let app = open(tmp.path());
        let (lines, buf) = rows(&app, 140, 30);
        let card = row_of(&lines, "image/png");
        let tip = row_of(&lines, "─┤");
        let brace_x = lines[tip].chars().position(|c| c == '┤').unwrap() as u16;
        let card_x = lines[card][..lines[card].find("type").unwrap()]
            .chars()
            .count() as u16;
        let picture: Vec<_> = (brace_x + 3..card_x)
            .map(|x| buf[(x, card as u16)].fg)
            .filter(|c| *c != theme::rgb(BG) && *c != ratatui::style::Color::Reset)
            .collect();
        assert!(
            picture.len() > 3,
            "the picture fills the row to the left of the card: {lines:#?}"
        );
        let histogram = row_of(&lines, "█");
        assert!(
            lines[histogram].find('█').unwrap() > lines[histogram].find('▀').unwrap_or(0),
            "the histogram sits beside the picture too: {lines:#?}"
        );
    }

    #[test]
    fn with_images_off_a_picture_shows_only_its_card() {
        let tmp = crate::testdir::tempdir();
        write_png(&tmp.path().join("photo.png"), 20, 10);
        let mut app = App::with_settings(
            tmp.path().to_path_buf(),
            Keymap::default(),
            crate::app::Settings {
                painter: crate::imageview::Painter::off(),
                ..Default::default()
            },
        );
        app.settle();
        let (lines, _) = rows(&app, 100, 15);
        assert_eq!(
            lines[row_of(&lines, "photo.png")],
            lines[7],
            "the card sits on the cursor row"
        );
        assert!(lines.iter().any(|l| l.contains("image/png")));
        assert!(
            !lines.iter().any(|l| l.contains('▀') || l.contains('▄')),
            "{lines:#?}"
        );
    }

    #[test]
    fn without_colour_the_cursor_row_is_reverse_video() {
        let tmp = fixture();
        let mut app = App::with_settings(
            tmp.path().to_path_buf(),
            Keymap::default(),
            crate::app::Settings {
                depth: theme::Depth::None,
                ..Default::default()
            },
        );
        app.settle();
        let (lines, buf) = rows(&app, 100, 11);
        let x = lines[5].find("alpha/").unwrap() as u16;
        assert!(buf[(x, 5)].modifier.contains(Modifier::REVERSED));
        assert_eq!(buf[(x, 5)].fg, ratatui::style::Color::Reset);
        assert!(!buf[(x, 4)].modifier.contains(Modifier::REVERSED));
    }

    #[test]
    fn in_256_colours_every_cell_uses_the_palette() {
        let tmp = fixture();
        let mut app = App::with_settings(
            tmp.path().to_path_buf(),
            Keymap::default(),
            crate::app::Settings {
                depth: theme::Depth::Ansi256,
                ..Default::default()
            },
        );
        app.settle();
        let (_, buf) = rows(&app, 100, 11);
        assert!(
            buf.content
                .iter()
                .all(|c| !matches!(c.fg, ratatui::style::Color::Rgb(..))
                    && !matches!(c.bg, ratatui::style::Color::Rgb(..)))
        );
    }

    #[test]
    fn entries_far_from_the_cursor_keep_their_full_colour() {
        let tmp = crate::testdir::tempdir();
        for i in 0..8 {
            fs::write(tmp.path().join(format!("f{i}.txt")), "x").unwrap();
        }
        let app = open(tmp.path());
        let (lines, buf) = rows(&app, 100, 21);
        let near = row_of(&lines, "f1.txt") as u16;
        let far = row_of(&lines, "f7.txt") as u16;
        assert_eq!(buf[(1, near)].fg, buf[(1, far)].fg);
    }
}

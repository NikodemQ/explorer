//! Pure geometry. The cursor row of every level sits on one shared center row,
//! so the current path reads as a single flat line.

use std::ops::Range;

/// Cells between two columns, holding the brace.
pub const GUTTER: u16 = 3;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Placed {
    pub level: usize,
    pub x: u16,
    pub width: u16,
}

/// Places every column flush left. When they do not all fit `viewport` they share it equally,
/// so none of them is ever dropped.
pub fn place_columns(widths: &[u16], viewport: u16) -> Vec<Placed> {
    if widths.is_empty() {
        return Vec::new();
    }
    let n = widths.len() as u16;
    let span = widths.iter().map(|&w| u32::from(w)).sum::<u32>() + u32::from(GUTTER * (n - 1));
    let fits = span <= u32::from(viewport);
    let cap = (viewport.saturating_sub(GUTTER * (n - 1)) / n).max(1);
    let mut x = 0;
    widths
        .iter()
        .enumerate()
        .map(|(level, &w)| {
            let width = if fits { w } else { w.min(cap) };
            let placed = Placed { level, x, width };
            x += width + GUTTER;
            placed
        })
        .collect()
}

/// The narrowest a file preview or editor may get before the folder columns shrink for it.
pub const CONTENT_MIN: u16 = 24;

/// Places all the directory columns at their natural widths and, when there is a file preview or
/// editor, gives it everything to the right of them. Its entry is last, at index `dirs.len()`.
pub fn place(dirs: &[u16], content: bool, viewport: u16) -> Vec<Placed> {
    let budget = if content {
        viewport.saturating_sub(GUTTER + CONTENT_MIN)
    } else {
        viewport
    };
    let mut placed = place_columns(dirs, budget.max(1));
    if content {
        let x = placed.last().map_or(0, |p| p.x + p.width + GUTTER);
        placed.push(Placed {
            level: dirs.len(),
            x,
            width: viewport.saturating_sub(x).max(1),
        });
    }
    placed
}

/// Screen row (relative to the tree area, may be off-screen) of entry `i`.
pub fn row_y(center: u16, cursor: usize, i: usize) -> i32 {
    i32::from(center) + i as i32 - cursor as i32
}

/// Entries whose rows fall inside a tree area of `height` rows.
pub fn visible_range(len: usize, cursor: usize, center: u16, height: u16) -> Range<usize> {
    let first = cursor.saturating_sub(usize::from(center));
    let end = (cursor + usize::from(height.saturating_sub(center))).min(len);
    first.min(end)..end
}

/// A `{` brace: its tip touches the parent's cursor row (the center row) and its
/// ends run to the first and last visible entry of the child level.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Brace {
    pub top: u16,
    pub bottom: u16,
    pub tip: u16,
    pub clipped_top: bool,
    pub clipped_bottom: bool,
}

pub fn brace(height: u16, center: u16, len: usize, cursor: usize) -> Brace {
    let len = len.max(1);
    let top = row_y(center, cursor, 0);
    let bottom = row_y(center, cursor, len - 1);
    let last_row = i32::from(height) - 1;
    Brace {
        top: top.clamp(0, last_row) as u16,
        bottom: bottom.clamp(0, last_row) as u16,
        tip: center,
        clipped_top: top < 0,
        clipped_bottom: bottom > last_row,
    }
}

/// Rows `(top, count)` for a block of `lines` lines that has no cursor, such as a file preview.
/// Short blocks are centered on the center row, and tall ones fill the whole height.
pub fn block_rows(height: u16, center: u16, lines: usize) -> (u16, u16) {
    let count = lines.clamp(1, usize::from(height.max(1))) as u16;
    let top = center
        .saturating_sub(count / 2)
        .min(height.saturating_sub(count));
    (top, count)
}

/// Brace around a block: the tip stays on the center row, which the block always covers.
pub fn block_brace(height: u16, center: u16, lines: usize) -> Brace {
    let (top, count) = block_rows(height, center, lines);
    Brace {
        top,
        bottom: top + count - 1,
        tip: center.clamp(top, top + count - 1),
        clipped_top: false,
        clipped_bottom: lines > usize::from(height),
    }
}

/// Glyphs for each brace row, as three cells: tip arm, spine, arm toward the child.
pub fn brace_glyphs(b: Brace) -> Vec<(u16, [char; 3])> {
    (b.top..=b.bottom)
        .map(|row| {
            let at_top = row == b.top;
            let at_bottom = row == b.bottom;
            let cells = if row == b.tip {
                let spine = match (at_top && !b.clipped_top, at_bottom && !b.clipped_bottom) {
                    (true, true) => '─',
                    (true, false) => '┬',
                    (false, true) => '┴',
                    (false, false) => '┤',
                };
                ['─', spine, if spine == '┤' { ' ' } else { '─' }]
            } else if at_top && !b.clipped_top {
                [' ', '╭', '─']
            } else if at_bottom && !b.clipped_bottom {
                [' ', '╰', '─']
            } else {
                [' ', '│', ' ']
            };
            (row, cells)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn columns_are_flush_left_when_they_fit() {
        let placed = place_columns(&[10, 20], 60);
        assert_eq!(
            placed,
            [
                Placed {
                    level: 0,
                    x: 0,
                    width: 10
                },
                Placed {
                    level: 1,
                    x: 13,
                    width: 20
                },
            ]
        );
    }

    #[test]
    fn columns_that_do_not_fit_shrink_instead_of_being_dropped() {
        let placed = place_columns(&[30, 30, 30], 40);
        assert_eq!(
            placed.iter().map(|p| p.level).collect::<Vec<_>>(),
            [0, 1, 2]
        );
        let end = placed.last().map(|p| p.x + p.width).unwrap();
        assert!(
            end <= 40,
            "columns must stay inside the viewport, ended at {end}"
        );
    }

    #[test]
    fn visible_range_keeps_cursor_on_center_row() {
        // 100 entries, cursor 50, center row 5, height 11: entries 45..56
        assert_eq!(visible_range(100, 50, 5, 11), 45..56);
        // near the top, rows above the first entry stay empty
        assert_eq!(visible_range(100, 2, 5, 11), 0..8);
        // near the bottom
        assert_eq!(visible_range(10, 9, 5, 11), 4..10);
        assert_eq!(visible_range(0, 0, 5, 11), 0..0);
    }

    fn column(glyphs: &[(u16, [char; 3])], index: usize) -> String {
        glyphs.iter().map(|(_, c)| c[index]).collect()
    }

    #[test]
    fn brace_spans_the_whole_child_with_the_tip_on_the_center_row() {
        // 5 entries, cursor on the middle one: symmetrical brace around row 5
        let b = brace(11, 5, 5, 2);
        assert_eq!((b.top, b.bottom, b.tip), (3, 7, 5));
        let g = brace_glyphs(b);
        assert_eq!(column(&g, 1), "╭│┤│╰");
        assert_eq!(column(&g, 0), "  ─  ");
        assert_eq!(column(&g, 2), "─   ─");
    }

    #[test]
    fn brace_tip_on_first_or_last_entry_uses_tee_corners() {
        let g = brace_glyphs(brace(11, 5, 3, 0));
        assert_eq!(column(&g, 1), "┬│╰");
        let g = brace_glyphs(brace(11, 5, 3, 2));
        assert_eq!(column(&g, 1), "╭│┴");
    }

    #[test]
    fn single_entry_or_empty_child_gets_a_straight_line() {
        for len in [0, 1] {
            let g = brace_glyphs(brace(11, 5, len, 0));
            assert_eq!(g, [(5, ['─', '─', '─'])]);
        }
    }

    #[test]
    fn brace_ends_open_where_the_child_is_clipped_by_the_screen() {
        let b = brace(11, 5, 100, 50);
        assert_eq!((b.top, b.bottom), (0, 10));
        assert!(b.clipped_top && b.clipped_bottom);
        let g = brace_glyphs(b);
        assert_eq!(column(&g, 1), "│││││┤│││││");
    }

    #[test]
    fn all_three_folder_columns_stay_whole_past_half_the_screen() {
        let placed = place(&[30, 30, 30], true, 130);
        let levels: Vec<_> = placed.iter().map(|p| p.level).collect();
        assert_eq!(levels, [0, 1, 2, 3], "the parent is kept as well");
        assert!(placed[..3].iter().all(|p| p.width == 30), "{placed:?}");
        let content = placed.last().unwrap();
        assert_eq!(content.x, 3 * (30 + GUTTER));
        assert_eq!(content.x + content.width, 130);
    }

    #[test]
    fn without_content_the_folder_columns_keep_their_natural_widths() {
        let placed = place(&[30, 30, 30], false, 100);
        assert_eq!(placed.len(), 3);
        assert!(placed.iter().all(|p| p.width == 30), "{placed:?}");
    }

    #[test]
    fn wide_folder_columns_still_leave_the_content_its_minimum() {
        let placed = place(&[40, 40, 40], true, 100);
        assert_eq!(placed.len(), 4, "no folder column is dropped");
        let content = placed.last().unwrap();
        assert!(content.width >= CONTENT_MIN, "{placed:?}");
        assert_eq!(content.x + content.width, 100);
    }

    #[test]
    fn short_columns_leave_the_content_the_rest() {
        let placed = place(&[10], true, 100);
        assert_eq!(placed[1].x, 10 + GUTTER);
        assert_eq!(placed[1].width, 100 - 10 - GUTTER);
    }

    #[test]
    fn short_blocks_center_on_the_cursor_row_and_tall_ones_fill_the_height() {
        assert_eq!(block_rows(21, 10, 5), (8, 5));
        assert_eq!(block_rows(21, 10, 1), (10, 1));
        assert_eq!(block_rows(21, 10, 100), (0, 21));
        assert_eq!(
            block_rows(21, 2, 9),
            (0, 9),
            "near the top the block is pushed down to fit"
        );
        assert_eq!(block_rows(21, 19, 9), (12, 9));
        assert_eq!(block_rows(0, 0, 3), (0, 1));
    }

    #[test]
    fn block_brace_covers_the_block_with_the_tip_on_the_center_row() {
        let b = block_brace(21, 10, 5);
        assert_eq!((b.top, b.bottom, b.tip), (8, 12, 10));
        assert!(!b.clipped_bottom);
        let tall = block_brace(21, 10, 300);
        assert_eq!((tall.top, tall.bottom, tall.tip), (0, 20, 10));
        assert!(tall.clipped_bottom && !tall.clipped_top);
        let g = brace_glyphs(b);
        assert_eq!(g.iter().map(|(_, c)| c[1]).collect::<String>(), "╭│┤│╰");
    }
}

//! Transcript view: word-wrapped, scrollable, selectable text.
//!
//! The app rebuilds the transcript as a list of styled blocks for every frame; a line
//! break separates consecutive blocks. The view wraps the blocks to the area's width,
//! keeps the scroll position, and maps screen cells back to source text, so a
//! selection copies the original text rather than the wrapped rows.
//!
//! Blocks are identified by `Key`, not by their place in the list: a block inserted
//! mid-turn (a verdict, a call's first output) moves every later block down, and the
//! scroll position, the selection, and the wrap cache must stay with their text.

use std::borrow::Cow;
use std::collections::HashMap;

use ratatui::buffer::{Buffer, CellWidth};
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use unicode_segmentation::UnicodeSegmentation;

const TAB_WIDTH: usize = 8;

/// Rows of context kept when paging.
const PAGE_OVERLAP: usize = 2;

/// Identifies a block across frames. Keys must be unique within a frame; the `u8`
/// numbers a block among the parts of the same thing.
#[derive(Clone, PartialEq, Eq, Hash)]
pub enum Key {
    /// The system message.
    System(u8),
    /// The message at this index in the session (for a reply still streaming, the
    /// index it will be saved at).
    Message(usize, u8),
    /// The tool call with this id.
    Call(String, u8),
    /// Display-only description of the pending approval for this call.
    ApprovalDescription(String),
    /// The note at this index.
    Note(usize, u8),
}

pub struct Block<'a> {
    pub key: Key,
    pub text: Cow<'a, str>,
    pub style: Style,
}

impl<'a> Block<'a> {
    /// A block without its trailing newlines, so it ends on its last line of text.
    pub fn new(key: Key, text: impl Into<Cow<'a, str>>, style: Style) -> Self {
        let text = match text.into() {
            Cow::Borrowed(text) => Cow::Borrowed(text.trim_end_matches('\n')),
            Cow::Owned(mut text) => {
                text.truncate(text.trim_end_matches('\n').len());
                Cow::Owned(text)
            }
        };
        Block { key, text, style }
    }
}

/// A position in the source text as of the last render: a block index and a byte
/// offset into its text. Only valid until the next render.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
struct Pos {
    block: usize,
    byte: usize,
}

/// A position kept across renders: a block key and a byte offset into its text.
/// The offset is clamped to the text when resolved, since the text may have changed.
#[derive(Clone)]
struct Mark {
    key: Key,
    byte: usize,
}

/// One screen row: a byte range of its block's text. A block's rows cover its text
/// contiguously apart from the `\n`s between source lines, so a byte range copied
/// from the text is exactly what was shown.
#[derive(Clone, Copy)]
struct Row {
    start: usize,
    end: usize,
}

/// A block's rows at one width, with the text they were computed from.
#[derive(Default)]
struct Wrapped {
    text: String,
    width: usize,
    rows: Vec<Row>,
}

impl Wrapped {
    fn update(&mut self, text: &str, width: usize) {
        if self.width == width && self.text == text {
            return;
        }
        let from = if self.width == width && text.starts_with(self.text.as_str()) {
            // Appended text (a streaming reply, running output) only changes the last
            // row: `wrap_line` ends a row only when a later grapheme doesn't fit, and
            // a row starts afresh at column 0, so wrapping can resume from its start.
            // (Appended text can join the last grapheme, but that is in the last row.)
            self.rows.pop().map_or(0, |row| row.start)
        } else {
            self.rows.clear();
            0
        };
        let mut start = from;
        loop {
            let end = text[start..].find('\n').map_or(text.len(), |i| start + i);
            wrap_line(text, start, end, width, &mut self.rows);
            if end == text.len() {
                break;
            }
            start = end + 1;
        }
        self.text.clear();
        self.text.push_str(text);
        self.width = width;
    }
}

/// One selection endpoint: the source range of the screen cell it's on, so the
/// selection can include that cell whichever direction it extends.
#[derive(Clone)]
struct Cell {
    left: Mark,
    right: Mark,
}

struct Selection {
    anchor: Cell,
    head: Cell,
    /// A press without a drag is a click: it selects nothing.
    dragged: bool,
}

#[derive(Default)]
pub struct View {
    /// Wrapped rows per block, by block index, from the last render.
    wrapped: Vec<Wrapped>,
    /// Each block's key, by block index, from the last render.
    keys: Vec<Key>,
    /// Block index by key, from the last render.
    index: HashMap<Key, usize>,
    /// Index of each block's first row, then the total row count.
    first_row: Vec<usize>,
    area: Rect,
    /// First visible row.
    top: usize,
    /// While scrolled up, the source position of the first visible row, which stays
    /// in place as output arrives or the width changes. `None` follows the bottom.
    anchor: Option<Mark>,
    selection: Option<Selection>,
}

impl View {
    pub fn render(&mut self, blocks: &[Block], area: Rect, buf: &mut Buffer) {
        self.area = area;
        let width = area.width as usize;
        // Each block takes its wrapped rows from last frame by key, wherever it now is.
        let mut previous: HashMap<Key, Wrapped> =
            self.keys.drain(..).zip(self.wrapped.drain(..)).collect();
        self.index.clear();
        self.first_row.clear();
        let mut total = 0;
        for (i, block) in blocks.iter().enumerate() {
            let mut wrapped = previous.remove(&block.key).unwrap_or_default();
            wrapped.update(&block.text, width);
            self.first_row.push(total);
            total += wrapped.rows.len();
            self.wrapped.push(wrapped);
            self.keys.push(block.key.clone());
            self.index.insert(block.key.clone(), i);
        }
        self.first_row.push(total);

        let max_top = self.max_top();
        // An anchor whose block is gone falls back to following the bottom.
        let anchor = self.anchor.as_ref().and_then(|mark| self.resolve(mark));
        self.top = anchor.map_or(max_top, |pos| self.row_of(pos).min(max_top));
        if self.top == max_top {
            self.anchor = None;
        }

        let selected = self.selected();
        if selected.is_none() && self.selection.as_ref().is_some_and(|s| s.dragged) {
            // An end's block is gone; the selection can't be shown or copied.
            self.selection = None;
        }
        let end = (self.top + area.height as usize).min(total);
        for (y, index) in (self.top..end).enumerate() {
            let (block, row) = self.row(index);
            let text = &self.wrapped[block].text;
            let y = area.y + y as u16;
            buf.set_stringn(
                area.x,
                y,
                display(&text[row.start..row.end]),
                width,
                blocks[block].style,
            );
            if let Some((start, end)) = selected {
                let lo = start.max(Pos {
                    block,
                    byte: row.start,
                });
                let hi = end.min(Pos {
                    block,
                    byte: row.end,
                });
                if lo < hi {
                    let from = text_width(&text[row.start..lo.byte]).min(width);
                    let to = text_width(&text[row.start..hi.byte]).min(width);
                    buf.set_style(
                        Rect::new(area.x + from as u16, y, (to - from) as u16, 1),
                        Style::new().add_modifier(Modifier::REVERSED),
                    );
                }
            }
        }
    }

    /// Scroll by `delta` rows (negative is up). Reaching the bottom resumes following.
    pub fn scroll(&mut self, delta: isize) {
        let max_top = self.max_top();
        let top = self.top.saturating_add_signed(delta).min(max_top);
        self.top = top;
        self.anchor = (top < max_top).then(|| {
            let (block, row) = self.row(top);
            self.mark(Pos {
                block,
                byte: row.start,
            })
        });
    }

    /// Scroll by a screen, less a little overlap. `direction` is -1 (up) or 1 (down).
    pub fn page(&mut self, direction: isize) {
        let rows = (self.area.height as usize)
            .saturating_sub(PAGE_OVERLAP)
            .max(1);
        self.scroll(direction * rows as isize);
    }

    pub fn scroll_to_bottom(&mut self) {
        self.anchor = None;
    }

    pub fn scrolled_up(&self) -> bool {
        self.anchor.is_some()
    }

    /// Mouse button pressed: start a selection there, or clear it outside the view.
    pub fn press(&mut self, x: u16, y: u16) {
        self.selection = self
            .contains(x, y)
            .then(|| self.cell_at(x, y))
            .flatten()
            .map(|cell| Selection {
                anchor: cell.clone(),
                head: cell,
                dragged: false,
            });
    }

    /// Extend the selection. Dragging onto the top row or below the view scrolls one
    /// row per event.
    pub fn drag(&mut self, x: u16, y: u16) {
        if self.selection.is_none() || self.area.is_empty() {
            return;
        }
        let area = self.area;
        let y = if y <= area.y {
            self.scroll(-1);
            area.y
        } else if y >= area.bottom() {
            self.scroll(1);
            area.bottom() - 1
        } else {
            y
        };
        let head = self.cell_at(x.min(area.right() - 1), y);
        if let (Some(selection), Some(head)) = (self.selection.as_mut(), head) {
            selection.head = head;
            selection.dragged = true;
        }
    }

    /// Mouse button released: the selected text, if a drag selected any. The
    /// selection stays highlighted until the next click or key press.
    pub fn release(&mut self) -> Option<String> {
        let (start, end) = self.selected()?;
        (start < end).then(|| self.text_between(start, end))
    }

    pub fn clear_selection(&mut self) {
        self.selection = None;
    }

    fn total(&self) -> usize {
        self.first_row.last().copied().unwrap_or(0)
    }

    fn max_top(&self) -> usize {
        self.total().saturating_sub(self.area.height as usize)
    }

    fn contains(&self, x: u16, y: u16) -> bool {
        self.area.contains(ratatui::layout::Position { x, y })
    }

    fn mark(&self, pos: Pos) -> Mark {
        Mark {
            key: self.keys[pos.block].clone(),
            byte: pos.byte,
        }
    }

    /// Where a mark is in the last render, with its offset clamped to its block's
    /// current text and moved back to a character boundary. `None` if the block is
    /// gone.
    fn resolve(&self, mark: &Mark) -> Option<Pos> {
        let block = *self.index.get(&mark.key)?;
        let byte = self.wrapped[block].text.floor_char_boundary(mark.byte);
        Some(Pos { block, byte })
    }

    /// The dragged selection's source range, inclusive of the cells at both ends.
    /// `None` without a drag, or if an end's block is gone.
    fn selected(&self) -> Option<(Pos, Pos)> {
        let selection = self.selection.as_ref().filter(|s| s.dragged)?;
        let anchor_left = self.resolve(&selection.anchor.left)?;
        let head_left = self.resolve(&selection.head.left)?;
        Some(if head_left >= anchor_left {
            (anchor_left, self.resolve(&selection.head.right)?)
        } else {
            (head_left, self.resolve(&selection.anchor.right)?)
        })
    }

    /// The block and row at a row index, which must be below `total`.
    fn row(&self, index: usize) -> (usize, Row) {
        let blocks = self.first_row.len() - 1;
        let block = self.first_row[..blocks].partition_point(|&first| first <= index) - 1;
        (
            block,
            self.wrapped[block].rows[index - self.first_row[block]],
        )
    }

    /// The row index showing `pos`, or the total if its block no longer exists.
    fn row_of(&self, pos: Pos) -> usize {
        let Some(wrapped) = self.wrapped.get(pos.block) else {
            return self.total();
        };
        let row = wrapped.rows.partition_point(|r| r.start <= pos.byte);
        self.first_row[pos.block] + row.saturating_sub(1)
    }

    /// The source range of the cell at a screen position inside the view. Past the
    /// end of a row, both ends are the row's end; below the last row, the end of the
    /// last block. `None` when there are no blocks.
    fn cell_at(&self, x: u16, y: u16) -> Option<Cell> {
        let point = |pos: Pos| {
            Some(Cell {
                left: self.mark(pos),
                right: self.mark(pos),
            })
        };
        let index = self.top + (y - self.area.y) as usize;
        if index >= self.total() {
            let block = self.wrapped.len().checked_sub(1)?;
            let byte = self.wrapped[block].text.len();
            return point(Pos { block, byte });
        }
        let (block, row) = self.row(index);
        let text = &self.wrapped[block].text[row.start..row.end];
        let x = (x - self.area.x) as usize;
        let mut col = 0;
        for (i, g) in text.grapheme_indices(true) {
            let width = grapheme_width(g, col);
            if x < col + width {
                let byte = row.start + i;
                return Some(Cell {
                    left: self.mark(Pos { block, byte }),
                    right: self.mark(Pos {
                        block,
                        byte: byte + g.len(),
                    }),
                });
            }
            col += width;
        }
        point(Pos {
            block,
            byte: row.end,
        })
    }

    /// Source text from `start` to `end`, with a line break between blocks.
    fn text_between(&self, start: Pos, end: Pos) -> String {
        let text = |block: usize| self.wrapped.get(block).map_or("", |w| w.text.as_str());
        if start.block == end.block {
            return text(start.block)[start.byte..end.byte].to_string();
        }
        let mut out = text(start.block)[start.byte..].to_string();
        for block in start.block + 1..end.block {
            out.push('\n');
            out.push_str(text(block));
        }
        out.push('\n');
        out.push_str(&text(end.block)[..end.byte]);
        out
    }
}

/// Word-wrap the source line `text[start..end]` into rows at most `width` columns
/// wide. Rows break after whitespace when possible; a word longer than the width is
/// broken at the width. Whitespace may run past the edge (it's clipped when drawn),
/// so a row never starts with the space that ended the previous one.
fn wrap_line(text: &str, start: usize, end: usize, width: usize, rows: &mut Vec<Row>) {
    let width = width.max(1);
    let mut row_start = start;
    let mut col = 0;
    // Just after the last whitespace in the current row: where to break before a word.
    let mut after_space: Option<usize> = None;
    for (i, g) in text[start..end].grapheme_indices(true) {
        let i = start + i;
        if !is_space(g) && i > row_start && col + grapheme_width(g, col) > width {
            match after_space {
                Some(at) if at > row_start => {
                    rows.push(Row {
                        start: row_start,
                        end: at,
                    });
                    row_start = at;
                    // The start of this word moves down with it.
                    col = text_width(&text[at..i]);
                }
                _ => {
                    rows.push(Row {
                        start: row_start,
                        end: i,
                    });
                    row_start = i;
                    col = 0;
                }
            }
            after_space = None;
            // The moved part of the word may still leave no room for `g`.
            if i > row_start && col + grapheme_width(g, col) > width {
                rows.push(Row {
                    start: row_start,
                    end: i,
                });
                row_start = i;
                col = 0;
            }
        }
        col += grapheme_width(g, col);
        if is_space(g) {
            after_space = Some(i + g.len());
        }
    }
    rows.push(Row {
        start: row_start,
        end,
    });
}

/// Display columns of grapheme `g` at column `col` of its row, exactly as ratatui
/// draws it (`CellWidth`), so wrapping, highlighting, and hit-testing line up with
/// the screen. Tab stops are measured from the row start. Graphemes containing
/// other control characters are never drawn (ratatui skips them too), so model
/// output can't emit terminal escape sequences.
fn grapheme_width(g: &str, col: usize) -> usize {
    if g == "\t" {
        TAB_WIDTH - col % TAB_WIDTH
    } else if g.contains(char::is_control) {
        0
    } else {
        g.cell_width() as usize
    }
}

fn is_space(g: &str) -> bool {
    g.chars().all(char::is_whitespace)
}

fn text_width(text: &str) -> usize {
    text.graphemes(true)
        .fold(0, |col, g| col + grapheme_width(g, col))
}

/// A row as drawn: tabs expanded to spaces, other control characters removed.
fn display(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut col = 0;
    for g in text.graphemes(true) {
        let width = grapheme_width(g, col);
        if g == "\t" {
            out.extend(std::iter::repeat_n(' ', width));
        } else if !g.contains(char::is_control) {
            out.push_str(g);
        }
        col += width;
    }
    out
}

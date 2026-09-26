//! Terminal layer: the transcript is written as raw text into the terminal's normal
//! scrollback, and only a small input region at the bottom of the screen is managed.
//!
//! No alternate screen and no mouse capture: the terminal owns scrolling, selection,
//! copy/paste, and reflow of transcript text on resize. Transcript lines are never
//! hard-wrapped by us, so the terminal can reflow them.
//!
//! The hardware cursor is kept hidden and parked at the top-left cell of the input
//! region. Terminals carry the cursor along when they reflow on resize, so after a
//! resize its position tells us where the region now starts.

use std::io::{self, Stdout, Write};

use crossterm::{
    cursor::{self, Hide, MoveTo, Show},
    event::{DisableBracketedPaste, EnableBracketedPaste},
    queue,
    style::{ContentStyle, Print, PrintStyledContent},
    terminal::{self, BeginSynchronizedUpdate, Clear, ClearType, EndSynchronizedUpdate},
};
use ratatui::backend::CrosstermBackend;
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Widget};
use ratatui_textarea::{CursorMove, TextArea};

/// Prompt marker at the start of the input box.
const PROMPT: &str = "> ";

pub struct Tui {
    out: CrosstermBackend<Stdout>,
    /// Screen row where the input region starts. Everything above it is transcript.
    region_top: u16,
    /// Unterminated last transcript line (a reply still streaming). It is reprinted
    /// whole from `pending_top` on every append instead of continued in place, so the
    /// terminal soft-wraps it as one line and reflows it on resize.
    pending: String,
    pending_style: ContentStyle,
    /// Screen row where `pending` starts. Only meaningful while `pending` is non-empty.
    pending_top: u16,
    /// A synchronized update is open. Transcript output and the region redraw erase and
    /// rewrite rows across several flushes; the terminal holds the display until
    /// `draw` ends the frame, so none of the intermediate states are visible.
    in_frame: bool,
}

impl Tui {
    pub fn enter() -> io::Result<Self> {
        install_panic_hook();
        terminal::enable_raw_mode()?;
        // Construct before any other fallible call so `Drop` restores the terminal if
        // setup fails partway.
        let mut tui = Tui {
            out: CrosstermBackend::new(io::stdout()),
            region_top: 0,
            pending: String::new(),
            pending_style: ContentStyle::new(),
            pending_top: 0,
            in_frame: false,
        };
        queue!(tui.out, EnableBracketedPaste, Hide)?;
        tui.out.flush()?;
        let (col, row) = cursor::position()?;
        tui.region_top = row;
        // Start the region on a fresh line if the shell left the cursor mid-line.
        if col != 0 {
            queue!(tui.out, Print("\r\n"))?;
            tui.out.flush()?;
            tui.region_top = cursor::position()?.1;
        }
        Ok(tui)
    }

    /// Print complete lines to the transcript, after ending any pending line.
    pub fn print(&mut self, text: &str, style: ContentStyle) -> io::Result<()> {
        self.end_line()?;
        self.append(&format!("{text}\n"), style)
    }

    /// Print many complete, styled blocks of text in one write with a single cursor
    /// query, after ending any pending line. Used for replaying a session: printing
    /// block by block costs a cursor round-trip each, which on a long session runs past
    /// the terminal's synchronized-update limit and visibly scrolls.
    pub fn print_batch(&mut self, blocks: &[(String, ContentStyle)]) -> io::Result<()> {
        self.end_line()?;
        self.begin_frame()?;
        queue!(
            self.out,
            MoveTo(0, self.region_top),
            Clear(ClearType::FromCursorDown)
        )?;
        for (text, style) in blocks {
            for line in text.split('\n') {
                self.queue_styled(line, *style)?;
                queue!(self.out, Print("\r\n"))?;
            }
        }
        self.out.flush()?;
        self.region_top = cursor::position()?.1;
        Ok(())
    }

    /// Terminate the pending line, if any.
    pub fn end_line(&mut self) -> io::Result<()> {
        if self.pending.is_empty() {
            return Ok(());
        }
        self.append("\n", self.pending_style)
    }

    /// Append text to the transcript above the input region. Each `\n` ends a line;
    /// long lines are left for the terminal to soft-wrap. Text after the last `\n`
    /// stays pending and is continued by the next append. `style` applies to the whole
    /// pending line.
    pub fn append(&mut self, text: &str, style: ContentStyle) -> io::Result<()> {
        self.begin_frame()?;
        let start = if self.pending.is_empty() {
            self.region_top
        } else {
            self.pending_top
        };
        queue!(self.out, MoveTo(0, start), Clear(ClearType::FromCursorDown))?;

        let mut pending = std::mem::take(&mut self.pending);
        pending.push_str(text);
        let (complete, rest) = match pending.rfind('\n') {
            Some(i) => (Some(&pending[..i]), &pending[i + 1..]),
            None => (None, pending.as_str()),
        };
        for line in complete.into_iter().flat_map(|c| c.split('\n')) {
            self.queue_styled(line, style)?;
            queue!(self.out, Print("\r\n"))?;
        }
        self.out.flush()?;
        // Ask the terminal where text ended rather than computing it: only the terminal
        // knows how it wrapped wide characters at the right edge.
        self.region_top = cursor::position()?.1;

        if !rest.is_empty() {
            let (width, height) = terminal::size()?;
            self.pending_top = self.region_top;
            self.queue_styled(rest, style)?;
            self.out.flush()?;
            self.region_top = cursor::position()?.1 + 1;
            // If printing `rest` ran past the bottom row, the terminal scrolled and the
            // line now starts higher than where printing began.
            let scrolled = (self.pending_top + rows_for(rest, width)).saturating_sub(height);
            self.pending_top = self.pending_top.saturating_sub(scrolled);
            self.pending = rest.to_string();
            self.pending_style = style;

            // A pending line must stay on screen to be reprinted. If one grows past half
            // the screen, commit it with a hard break; the rest of that logical line
            // continues as a new line and won't reflow as one on resize.
            if self.region_top - self.pending_top > height / 2 {
                queue!(self.out, Print("\r\n"))?;
                self.out.flush()?;
                self.region_top = cursor::position()?.1;
                self.pending.clear();
            }
        }
        Ok(())
    }

    /// Styled text resets its own style afterwards, so styles never bleed into the
    /// line break or the input region.
    fn queue_styled(&mut self, text: &str, style: ContentStyle) -> io::Result<()> {
        queue!(self.out, PrintStyledContent(style.apply(sanitize(text))))
    }

    /// Redraw the input region: a top rule, the textarea, and a status line. Ends the
    /// frame, displaying any transcript output since the last draw at once.
    pub fn draw(&mut self, textarea: &TextArea, status: Line) -> io::Result<()> {
        self.begin_frame()?;
        let (width, height) = terminal::size()?;
        if width == 0 || height == 0 {
            return self.end_frame();
        }
        // Text sits right of the `> ` prompt; wrapped and ^J lines align under it.
        let text_x = PROMPT.len() as u16;
        let text_width = width.saturating_sub(text_x).max(1);
        let max_input_rows = (height / 2).saturating_sub(2).max(1);
        let input_rows = wrapped_rows(textarea, text_width).clamp(1, max_input_rows);
        // Top rule + input rows + status line, never taller than the screen.
        let region_height = (input_rows + 2).min(height);

        // Scroll the screen up if the region doesn't fit below the transcript. Line
        // feeds at the bottom row push the transcript into scrollback.
        let overflow = (self.region_top + region_height).saturating_sub(height);
        if overflow > 0 {
            queue!(self.out, MoveTo(0, height - 1))?;
            for _ in 0..overflow {
                queue!(self.out, Print("\n"))?;
            }
            self.region_top -= overflow;
            self.pending_top = self.pending_top.saturating_sub(overflow);
        }

        let mut buf = Buffer::empty(Rect::new(0, 0, width, region_height));
        Block::default()
            .borders(Borders::TOP)
            .border_style(dim())
            .render(Rect::new(0, 0, width, 1), &mut buf);
        let text_rows = region_height.saturating_sub(2);
        if text_rows > 0 {
            // ratatui's `White` is bright white (SGR 97); `Gray` is the normal one.
            buf.set_string(0, 1, PROMPT, Style::default().white());
            textarea.render(Rect::new(text_x, 1, text_width, text_rows), &mut buf);
        }
        buf.set_line(0, region_height - 1, &status, width);

        for y in 0..region_height {
            self.write_row(&buf, y, width)?;
        }
        // Clear leftovers below a region that shrank. Skipped when the region reaches
        // the bottom row: an off-screen `MoveTo` clamps to the last row, and the clear
        // would erase the status line.
        let below = self.region_top + region_height;
        if below < height {
            queue!(self.out, MoveTo(0, below), Clear(ClearType::FromCursorDown))?;
        }
        queue!(self.out, MoveTo(0, self.region_top))?;
        self.end_frame()
    }

    fn begin_frame(&mut self) -> io::Result<()> {
        if !self.in_frame {
            queue!(self.out, BeginSynchronizedUpdate)?;
            self.in_frame = true;
        }
        Ok(())
    }

    fn end_frame(&mut self) -> io::Result<()> {
        if self.in_frame {
            queue!(self.out, EndSynchronizedUpdate)?;
            self.in_frame = false;
        }
        self.out.flush()
    }

    /// Re-locate the input region after the terminal has reflowed its contents. The
    /// parked cursor moved with the region's first line, so its row is the new top.
    /// The caller must `draw` afterwards; drawing clears any reflowed region remnants.
    pub fn handle_resize(&mut self) -> io::Result<()> {
        self.region_top = cursor::position()?.1;
        if !self.pending.is_empty() {
            // The pending line was reflowed too and ends just above the region.
            let (width, _) = terminal::size()?;
            self.pending_top = self
                .region_top
                .saturating_sub(rows_for(&self.pending, width));
        }
        Ok(())
    }

    /// Write one buffer row, trimming trailing blank cells so that a narrowed terminal
    /// only reflows real content, then clear the rest of the screen line.
    fn write_row(&mut self, buf: &Buffer, y: u16, width: u16) -> io::Result<()> {
        let row = self.region_top + y;
        let cells: Vec<_> = (0..width).map(|x| &buf[(x, y)]).collect();
        let end = cells
            .iter()
            .rposition(|c| {
                c.symbol() != " " || c.bg != ratatui::style::Color::Reset || !c.modifier.is_empty()
            })
            .map_or(0, |i| i + 1);

        // Skip the placeholder cells that follow a wide character; drawing them would
        // overwrite its right half.
        let mut skip = 0;
        let mut content = Vec::with_capacity(end);
        for (x, cell) in cells[..end].iter().enumerate() {
            if skip > 0 {
                skip -= 1;
                continue;
            }
            skip = Span::raw(cell.symbol()).width().saturating_sub(1);
            content.push((x as u16, row, *cell));
        }

        queue!(self.out, MoveTo(0, row))?;
        // Fully qualified: importing `Backend` makes `flush` ambiguous with `io::Write`.
        ratatui::backend::Backend::draw(&mut self.out, content.into_iter())?;
        queue!(self.out, Clear(ClearType::UntilNewLine))
    }

    /// Remove the input region and restore the terminal, leaving the transcript in
    /// scrollback and the cursor where the shell prompt should resume.
    pub fn exit(&mut self) -> io::Result<()> {
        queue!(
            self.out,
            MoveTo(0, self.region_top),
            Clear(ClearType::FromCursorDown)
        )?;
        restore(&mut self.out)
    }
}

impl Drop for Tui {
    fn drop(&mut self) {
        let _ = restore(&mut self.out);
    }
}

fn restore(out: &mut impl Write) -> io::Result<()> {
    // Ending a synchronized update that isn't open is harmless; this covers exits
    // (including panics) that happen mid-frame.
    queue!(out, EndSynchronizedUpdate, Show, DisableBracketedPaste)?;
    out.flush()?;
    terminal::disable_raw_mode()
}

/// Restore the terminal before the default panic message so it is readable.
fn install_panic_hook() {
    let default = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let _ = restore(&mut io::stdout());
        default(info);
    }));
}

/// Number of screen rows the textarea's content occupies at `width`, using the
/// textarea's own wrapping. It exposes no row count, so render a scratch copy to
/// build its screen map and read the screen row of the end of the text.
fn wrapped_rows(textarea: &TextArea, width: u16) -> u16 {
    let mut probe = textarea.clone();
    probe.move_cursor(CursorMove::Bottom);
    probe.move_cursor(CursorMove::End);
    // The screen map covers all text regardless of the rendered height.
    let area = Rect::new(0, 0, width, 1);
    let mut scratch = Buffer::empty(area);
    (&probe).render(area, &mut scratch);
    (probe.screen_cursor().row + 1) as u16
}

/// Screen rows `text` occupies as one soft-wrapped line at `width`, estimated from
/// display width. A wide character wrapped early at the right edge can make this one
/// row short.
fn rows_for(text: &str, width: u16) -> u16 {
    let cols = Span::raw(sanitize(text)).width().max(1);
    cols.div_ceil(width.max(1) as usize) as u16
}

/// Strip control characters (other than tab) so model output can't emit terminal
/// escape sequences or move the cursor.
fn sanitize(line: &str) -> String {
    line.chars()
        .filter(|&c| c == '\t' || !c.is_control())
        .collect()
}

/// Style for the input region's chrome.
pub fn dim() -> Style {
    Style::default().add_modifier(Modifier::DIM)
}

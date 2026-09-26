//! Terminal layer: a full-screen UI on the alternate screen. The app draws the whole
//! screen each frame: the transcript view (`transcript::View`) above an input region
//! of a top rule, the `> ` prompt and input box, and a status line.
//!
//! The mouse is captured, so the app handles wheel scrolling, selection, and copy
//! (OSC 52). Leaving the alternate screen on exit restores the terminal as it was.

use std::io::{self, Stdout, Write};

use crossterm::{
    Command,
    clipboard::CopyToClipboard,
    cursor::{Hide, Show},
    event::{DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste},
    queue,
    terminal::{
        self, BeginSynchronizedUpdate, EndSynchronizedUpdate, EnterAlternateScreen,
        LeaveAlternateScreen,
    },
};
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::Line;
use ratatui::widgets::{Block as Border, Borders, Widget};
use ratatui_textarea::{CursorMove, TextArea};

use crate::transcript::{Block, View};

/// Prompt marker at the start of the input box.
const PROMPT: &str = "> ";

/// Mouse reporting for what the app uses: presses, releases, and the wheel (1000),
/// motion only while a button is held, for dragging (1002), in SGR encoding (1006).
/// Unlike crossterm's `EnableMouseCapture`, it leaves out 1003, which reports every
/// movement and would redraw the screen on each. `DisableMouseCapture` turns it off.
struct EnableMouseButtons;

impl Command for EnableMouseButtons {
    fn write_ansi(&self, f: &mut impl std::fmt::Write) -> std::fmt::Result {
        f.write_str("\x1b[?1000h\x1b[?1002h\x1b[?1006h")
    }
}

pub struct Tui {
    terminal: Terminal<CrosstermBackend<Stdout>>,
}

impl Tui {
    pub fn enter() -> io::Result<Self> {
        install_panic_hook();
        terminal::enable_raw_mode()?;
        let terminal = match Terminal::new(CrosstermBackend::new(io::stdout())) {
            Ok(terminal) => terminal,
            Err(error) => {
                let _ = restore(&mut io::stdout());
                return Err(error);
            }
        };
        // Construct before the remaining fallible calls so `Drop` restores the
        // terminal if setup fails partway.
        let mut tui = Tui { terminal };
        let out = tui.terminal.backend_mut();
        queue!(
            out,
            EnterAlternateScreen,
            EnableMouseButtons,
            EnableBracketedPaste,
            Hide
        )?;
        out.flush()?;
        // Start from a blank screen; ratatui's buffers assume it.
        tui.terminal.clear()?;
        Ok(tui)
    }

    /// Draw a frame as one synchronized update. Only cells that changed since the
    /// last frame are written; a resize clears and redraws everything.
    pub fn draw(
        &mut self,
        blocks: &[Block],
        view: &mut View,
        textarea: &TextArea,
        status: Line,
    ) -> io::Result<()> {
        queue!(self.terminal.backend_mut(), BeginSynchronizedUpdate)?;
        let drawn = self
            .terminal
            .draw(|frame| {
                let area = frame.area();
                render(frame.buffer_mut(), area, blocks, view, textarea, status);
            })
            .map(|_| ());
        let out = self.terminal.backend_mut();
        queue!(out, EndSynchronizedUpdate)?;
        out.flush()?;
        drawn
    }

    /// Put `text` on the system clipboard (OSC 52). Terminals without OSC 52 support
    /// ignore it.
    pub fn copy(&mut self, text: &str) -> io::Result<()> {
        let out = self.terminal.backend_mut();
        queue!(out, CopyToClipboard::to_clipboard_from(text))?;
        out.flush()
    }

    pub fn exit(&mut self) -> io::Result<()> {
        restore(self.terminal.backend_mut())
    }
}

impl Drop for Tui {
    fn drop(&mut self) {
        let _ = restore(&mut io::stdout());
    }
}

/// Lay out the screen: the transcript fills everything above the input region.
fn render(
    buf: &mut Buffer,
    area: Rect,
    blocks: &[Block],
    view: &mut View,
    textarea: &TextArea,
    status: Line,
) {
    let (width, height) = (area.width, area.height);
    if width == 0 || height == 0 {
        return;
    }
    // Text sits right of the `> ` prompt; wrapped and ^J lines align under it.
    let text_x = PROMPT.len() as u16;
    let text_width = width.saturating_sub(text_x).max(1);
    let max_input_rows = (height / 2).saturating_sub(2).max(1);
    let input_rows = wrapped_rows(textarea, text_width).clamp(1, max_input_rows);
    // Top rule + input rows + status line, never taller than the screen.
    let region_height = (input_rows + 2).min(height);
    let region_top = height - region_height;

    view.render(blocks, Rect::new(0, 0, width, region_top), buf);

    Border::default()
        .borders(Borders::TOP)
        .border_style(dim())
        .render(Rect::new(0, region_top, width, 1), buf);
    let text_rows = region_height.saturating_sub(2);
    if text_rows > 0 {
        // ratatui's `White` is bright white (SGR 97); `Gray` is the normal one.
        buf.set_string(0, region_top + 1, PROMPT, Style::default().white());
        textarea.render(
            Rect::new(text_x, region_top + 1, text_width, text_rows),
            buf,
        );
    }
    buf.set_line(0, height - 1, &status, width);
}

fn restore(out: &mut impl Write) -> io::Result<()> {
    // Ending a synchronized update that isn't open, or leaving an alternate screen
    // that wasn't entered, is harmless; this covers exits (including panics) that
    // happen mid-frame or mid-setup.
    queue!(
        out,
        EndSynchronizedUpdate,
        DisableMouseCapture,
        DisableBracketedPaste,
        LeaveAlternateScreen,
        Show
    )?;
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

/// Style for the input region's chrome.
pub fn dim() -> Style {
    Style::default().add_modifier(Modifier::DIM)
}

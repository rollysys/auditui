// Custom terminal driver with ClearToEnd-per-row diff.
//
// Derived from ratatui 0.28's `Terminal` (MIT) via openai/codex's
// `codex-rs/tui/src/custom_terminal.rs` (Apache-2.0) with adaptations for
// auditui's fullscreen-only use and rust-version=1.77 (no let-chains).
//
// Why we ship our own Terminal instead of using `ratatui::Terminal`:
//
// Ratatui's default `Buffer::diff` compares cells 1:1. When content shrinks
// between frames, cells that go from glyph → default-space get explicit
// Put(" ") writes. That works on well-behaved terminals but breaks under
// two common edge cases:
//
//   1. East-Asian Ambiguous-Width glyphs. `unicode-width::width()` returns
//      1 for `·`, `→`, `▌`, etc., but CJK-locale terminals render them at
//      2 cells. The physical cursor drifts one cell right per glyph, so
//      subsequent writes land at shifted positions and the cells
//      ratatui thinks it cleared stay with stale content.
//   2. Multi-width character boundary bugs in ratatui 0.28 (see upstream
//      PR #1764, still open). Wide-char neighbors can emit an extra space
//      that the default diff doesn't compensate for.
//
// The trick openai/codex applies: per-row, scan for the last non-blank
// cell, then emit a single ANSI `ESC[K` (Clear-Until-Newline) past that
// column instead of a sequence of Put(" "). `ESC[K` is terminal-native
// and clears in *physical* cell space, so it doesn't care about cursor
// drift. It also tracks multi-width displacement via `invalidated` to
// correctly repaint cells a wide char's neighbours occupy.
//
// ---
//
// The MIT License (MIT)
// Copyright (c) 2016-2022 Florian Dehau
// Copyright (c) 2023-2025 The Ratatui Developers
//
// Permission is hereby granted, free of charge, to any person obtaining a copy
// of this software and associated documentation files (the "Software"), to deal
// in the Software without restriction, including without limitation the rights
// to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
// copies of the Software, and to permit persons to whom the Software is
// furnished to do so, subject to the following conditions:
//
// The above copyright notice and this permission notice shall be included in all
// copies or substantial portions of the Software.
//
// THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
// IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
// FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
// AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
// LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
// OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE
// SOFTWARE.

use std::io;
use std::io::Write;

use crossterm::cursor::MoveTo;
use crossterm::queue;
use crossterm::style::Colors;
use crossterm::style::Print;
use crossterm::style::SetAttribute;
use crossterm::style::SetBackgroundColor;
use crossterm::style::SetColors;
use crossterm::style::SetForegroundColor;
use crossterm::terminal::Clear;
use ratatui::backend::Backend;
use ratatui::backend::ClearType;
use ratatui::buffer::Buffer;
use ratatui::layout::Position;
use ratatui::layout::Rect;
use ratatui::layout::Size;
use ratatui::style::Color;
use ratatui::style::Modifier;
use ratatui::widgets::StatefulWidget;
use ratatui::widgets::Widget;
use unicode_width::UnicodeWidthStr;

/// Returns the display width of a cell symbol, ignoring OSC escape sequences.
fn display_width(s: &str) -> usize {
    if !s.contains('\x1B') {
        return s.width();
    }
    let mut visible = String::with_capacity(s.len());
    let mut chars = s.chars();
    while let Some(ch) = chars.next() {
        if ch == '\x1B' && chars.clone().next() == Some(']') {
            chars.next();
            for c in chars.by_ref() {
                if c == '\x07' {
                    break;
                }
            }
            continue;
        }
        visible.push(ch);
    }
    visible.width()
}

pub struct Frame<'a> {
    pub(crate) cursor_position: Option<Position>,
    pub(crate) viewport_area: Rect,
    pub(crate) buffer: &'a mut Buffer,
}

impl Frame<'_> {
    pub const fn area(&self) -> Rect {
        self.viewport_area
    }

    /// Shim matching `ratatui::Frame::render_widget` so existing
    /// call-sites in `tui.rs` keep working unchanged.
    pub fn render_widget<W: Widget>(&mut self, widget: W, area: Rect) {
        widget.render(area, self.buffer);
    }

    /// Shim matching `ratatui::Frame::render_stateful_widget`.
    pub fn render_stateful_widget<W: StatefulWidget>(
        &mut self,
        widget: W,
        area: Rect,
        state: &mut W::State,
    ) {
        widget.render(area, self.buffer, state);
    }

    #[allow(dead_code)]
    pub fn set_cursor_position<P: Into<Position>>(&mut self, position: P) {
        self.cursor_position = Some(position.into());
    }

    #[allow(dead_code)]
    pub fn buffer_mut(&mut self) -> &mut Buffer {
        self.buffer
    }
}

pub struct Terminal<B>
where
    B: Backend + Write,
{
    backend: B,
    buffers: [Buffer; 2],
    current: usize,
    hidden_cursor: bool,
    viewport_area: Rect,
    last_known_screen_size: Size,
}

impl<B> Drop for Terminal<B>
where
    B: Backend + Write,
{
    fn drop(&mut self) {
        if self.hidden_cursor {
            if let Err(err) = self.show_cursor() {
                eprintln!("Failed to show the cursor: {err}");
            }
        }
    }
}

impl<B> Terminal<B>
where
    B: Backend + Write,
{
    pub fn new(mut backend: B) -> io::Result<Self> {
        let screen_size = backend.size()?;
        // Initialize with a non-zero viewport so the first draw has somewhere
        // to render. `autoresize` will correct this from the actual terminal
        // size on every draw call.
        let initial_area = Rect::new(0, 0, screen_size.width, screen_size.height);
        Ok(Self {
            backend,
            buffers: [Buffer::empty(initial_area), Buffer::empty(initial_area)],
            current: 0,
            hidden_cursor: false,
            viewport_area: initial_area,
            last_known_screen_size: screen_size,
        })
    }

    fn current_buffer_mut(&mut self) -> &mut Buffer {
        &mut self.buffers[self.current]
    }

    fn previous_buffer(&self) -> &Buffer {
        &self.buffers[1 - self.current]
    }

    fn current_buffer(&self) -> &Buffer {
        &self.buffers[self.current]
    }

    #[allow(dead_code)]
    fn previous_buffer_mut(&mut self) -> &mut Buffer {
        &mut self.buffers[1 - self.current]
    }

    fn get_frame(&mut self) -> Frame<'_> {
        Frame {
            cursor_position: None,
            viewport_area: self.viewport_area,
            buffer: self.current_buffer_mut(),
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        let updates = diff_buffers(self.previous_buffer(), self.current_buffer());
        draw(&mut self.backend, updates.into_iter())
    }

    fn resize(&mut self, screen_size: Size) -> io::Result<()> {
        self.last_known_screen_size = screen_size;
        let area = Rect::new(0, 0, screen_size.width, screen_size.height);
        self.viewport_area = area;
        self.buffers[0].resize(area);
        self.buffers[1].resize(area);
        self.backend.clear()?;
        Ok(())
    }

    fn autoresize(&mut self) -> io::Result<()> {
        let screen_size = self.backend.size()?;
        if screen_size != self.last_known_screen_size {
            self.resize(screen_size)?;
        }
        Ok(())
    }

    pub fn draw<F>(&mut self, render_callback: F) -> io::Result<()>
    where
        F: FnOnce(&mut Frame),
    {
        self.autoresize()?;

        let mut frame = self.get_frame();
        render_callback(&mut frame);
        let cursor_position = frame.cursor_position;

        self.flush()?;

        match cursor_position {
            None => self.hide_cursor()?,
            Some(position) => {
                self.show_cursor()?;
                self.set_cursor_position(position)?;
            }
        }

        self.swap_buffers();
        Backend::flush(&mut self.backend)?;
        Ok(())
    }

    pub fn hide_cursor(&mut self) -> io::Result<()> {
        self.backend.hide_cursor()?;
        self.hidden_cursor = true;
        Ok(())
    }

    pub fn show_cursor(&mut self) -> io::Result<()> {
        self.backend.show_cursor()?;
        self.hidden_cursor = false;
        Ok(())
    }

    pub fn set_cursor_position<P: Into<Position>>(&mut self, position: P) -> io::Result<()> {
        let position = position.into();
        self.backend.set_cursor_position(position)?;
        Ok(())
    }

    /// Clear the visible viewport AND invalidate the previous buffer so the
    /// next draw repaints every cell. The per-row ClearToEnd in `diff_buffers`
    /// makes this rarely necessary, but keep it exposed for emergency use.
    #[allow(dead_code)]
    pub fn clear(&mut self) -> io::Result<()> {
        self.backend.clear_region(ClearType::All)?;
        self.previous_buffer_mut().reset();
        Ok(())
    }

    fn swap_buffers(&mut self) {
        self.buffers[1 - self.current].reset();
        self.current = 1 - self.current;
    }
}

#[derive(Debug)]
enum DrawCommand {
    Put { x: u16, y: u16, cell: ratatui::buffer::Cell },
    ClearToEnd { x: u16, y: u16, bg: Color },
}

fn diff_buffers(a: &Buffer, b: &Buffer) -> Vec<DrawCommand> {
    let previous_buffer = &a.content;
    let next_buffer = &b.content;

    let mut updates = vec![];
    let mut last_nonblank_columns = vec![0u16; a.area.height as usize];
    for y in 0..a.area.height {
        let row_start = y as usize * a.area.width as usize;
        let row_end = row_start + a.area.width as usize;
        let row = &next_buffer[row_start..row_end];
        let bg = row.last().map(|cell| cell.bg).unwrap_or(Color::Reset);

        // Scan row → last column that still carries content (non-space glyph,
        // non-trailing bg, or any modifier). Past that point, one ClearToEnd
        // wipes every cell to end-of-line regardless of physical cursor drift.
        let mut last_nonblank_column = 0usize;
        let mut column = 0usize;
        while column < row.len() {
            let cell = &row[column];
            let width = display_width(cell.symbol());
            if cell.symbol() != " " || cell.bg != bg || cell.modifier != Modifier::empty() {
                last_nonblank_column = column + (width.saturating_sub(1));
            }
            column += width.max(1);
        }

        if last_nonblank_column + 1 < row.len() {
            let (x, y) = a.pos_of(row_start + last_nonblank_column + 1);
            updates.push(DrawCommand::ClearToEnd { x, y, bg });
        }

        last_nonblank_columns[y as usize] = last_nonblank_column as u16;
    }

    // Track cells invalidated by a multi-width char replacing (or being
    // replaced by) another. ratatui's default diff doesn't know those
    // neighbour cells changed display-wise — we emit explicit Puts for them.
    let mut invalidated: usize = 0;
    let mut to_skip: usize = 0;
    for (i, (current, previous)) in next_buffer.iter().zip(previous_buffer.iter()).enumerate() {
        if !current.skip && (current != previous || invalidated > 0) && to_skip == 0 {
            let (x, y) = a.pos_of(i);
            let row = i / a.area.width as usize;
            if x <= last_nonblank_columns[row] {
                updates.push(DrawCommand::Put {
                    x,
                    y,
                    cell: next_buffer[i].clone(),
                });
            }
        }

        to_skip = display_width(current.symbol()).saturating_sub(1);

        let affected_width = std::cmp::max(
            display_width(current.symbol()),
            display_width(previous.symbol()),
        );
        invalidated = std::cmp::max(affected_width, invalidated).saturating_sub(1);
    }
    updates
}

fn draw<I>(writer: &mut impl Write, commands: I) -> io::Result<()>
where
    I: Iterator<Item = DrawCommand>,
{
    let mut fg = Color::Reset;
    let mut bg = Color::Reset;
    let mut modifier = Modifier::empty();
    let mut last_pos: Option<Position> = None;
    for command in commands {
        let (x, y) = match command {
            DrawCommand::Put { x, y, .. } => (x, y),
            DrawCommand::ClearToEnd { x, y, .. } => (x, y),
        };
        if !matches!(last_pos, Some(p) if x == p.x + 1 && y == p.y) {
            queue!(writer, MoveTo(x, y))?;
        }
        last_pos = Some(Position { x, y });
        match command {
            DrawCommand::Put { cell, .. } => {
                if cell.modifier != modifier {
                    let diff = ModifierDiff {
                        from: modifier,
                        to: cell.modifier,
                    };
                    diff.queue(writer)?;
                    modifier = cell.modifier;
                }
                if cell.fg != fg || cell.bg != bg {
                    queue!(writer, SetColors(Colors::new(cell.fg.into(), cell.bg.into())))?;
                    fg = cell.fg;
                    bg = cell.bg;
                }
                queue!(writer, Print(cell.symbol()))?;
            }
            DrawCommand::ClearToEnd { bg: clear_bg, .. } => {
                queue!(writer, SetAttribute(crossterm::style::Attribute::Reset))?;
                modifier = Modifier::empty();
                queue!(writer, SetBackgroundColor(clear_bg.into()))?;
                bg = clear_bg;
                queue!(writer, Clear(crossterm::terminal::ClearType::UntilNewLine))?;
            }
        }
    }

    queue!(
        writer,
        SetForegroundColor(crossterm::style::Color::Reset),
        SetBackgroundColor(crossterm::style::Color::Reset),
        SetAttribute(crossterm::style::Attribute::Reset),
    )?;

    Ok(())
}

struct ModifierDiff {
    pub from: Modifier,
    pub to: Modifier,
}

impl ModifierDiff {
    fn queue<W: io::Write>(self, w: &mut W) -> io::Result<()> {
        use crossterm::style::Attribute as CAttribute;
        let removed = self.from - self.to;
        if removed.contains(Modifier::REVERSED) {
            queue!(w, SetAttribute(CAttribute::NoReverse))?;
        }
        if removed.contains(Modifier::BOLD) {
            queue!(w, SetAttribute(CAttribute::NormalIntensity))?;
            if self.to.contains(Modifier::DIM) {
                queue!(w, SetAttribute(CAttribute::Dim))?;
            }
        }
        if removed.contains(Modifier::ITALIC) {
            queue!(w, SetAttribute(CAttribute::NoItalic))?;
        }
        if removed.contains(Modifier::UNDERLINED) {
            queue!(w, SetAttribute(CAttribute::NoUnderline))?;
        }
        if removed.contains(Modifier::DIM) {
            queue!(w, SetAttribute(CAttribute::NormalIntensity))?;
        }
        if removed.contains(Modifier::CROSSED_OUT) {
            queue!(w, SetAttribute(CAttribute::NotCrossedOut))?;
        }
        if removed.contains(Modifier::SLOW_BLINK) || removed.contains(Modifier::RAPID_BLINK) {
            queue!(w, SetAttribute(CAttribute::NoBlink))?;
        }

        let added = self.to - self.from;
        if added.contains(Modifier::REVERSED) {
            queue!(w, SetAttribute(CAttribute::Reverse))?;
        }
        if added.contains(Modifier::BOLD) {
            queue!(w, SetAttribute(CAttribute::Bold))?;
        }
        if added.contains(Modifier::ITALIC) {
            queue!(w, SetAttribute(CAttribute::Italic))?;
        }
        if added.contains(Modifier::UNDERLINED) {
            queue!(w, SetAttribute(CAttribute::Underlined))?;
        }
        if added.contains(Modifier::DIM) {
            queue!(w, SetAttribute(CAttribute::Dim))?;
        }
        if added.contains(Modifier::CROSSED_OUT) {
            queue!(w, SetAttribute(CAttribute::CrossedOut))?;
        }
        if added.contains(Modifier::SLOW_BLINK) {
            queue!(w, SetAttribute(CAttribute::SlowBlink))?;
        }
        if added.contains(Modifier::RAPID_BLINK) {
            queue!(w, SetAttribute(CAttribute::RapidBlink))?;
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::layout::Rect;
    use ratatui::style::Style;

    #[test]
    fn diff_buffers_emits_clear_to_end_when_row_shrinks() {
        // Previous frame had content in most of the row; new frame only
        // writes the first cell. We expect one ClearToEnd to wipe the rest
        // in one shot, not N Put(" ") commands.
        let area = Rect::new(0, 0, 10, 1);
        let mut previous = Buffer::empty(area);
        let mut next = Buffer::empty(area);
        previous.set_string(0, 0, "abcdefghij", Style::default());
        next.set_string(0, 0, "X", Style::default());

        let commands = diff_buffers(&previous, &next);

        let clear_to_end_count = commands
            .iter()
            .filter(|c| matches!(c, DrawCommand::ClearToEnd { .. }))
            .count();
        assert_eq!(clear_to_end_count, 1, "commands: {commands:?}");
    }

    #[test]
    fn diff_buffers_clear_to_end_starts_after_wide_char() {
        // Repro for the ratatui multi-width drift: previous had `中文`
        // (two wide chars), new has just `中`. The diff must emit a
        // ClearToEnd at col 2 (after the remaining wide char) so the
        // leftover `文` cells get wiped.
        let area = Rect::new(0, 0, 10, 1);
        let mut previous = Buffer::empty(area);
        let mut next = Buffer::empty(area);
        previous.set_string(0, 0, "中文", Style::default());
        next.set_string(0, 0, "中", Style::default());

        let commands = diff_buffers(&previous, &next);
        assert!(
            commands
                .iter()
                .any(|c| matches!(c, DrawCommand::ClearToEnd { x: 2, y: 0, .. })),
            "expected ClearToEnd at x=2; commands: {commands:?}"
        );
    }

    #[test]
    fn diff_buffers_no_clear_for_full_width_row() {
        // If the row is already full (non-space at the last col), there's
        // nothing past it to clear.
        let area = Rect::new(0, 0, 3, 1);
        let previous = Buffer::empty(area);
        let mut next = Buffer::empty(area);
        next.set_string(2, 0, "X", Style::default());

        let commands = diff_buffers(&previous, &next);

        let clear_count = commands
            .iter()
            .filter(|c| matches!(c, DrawCommand::ClearToEnd { .. }))
            .count();
        assert_eq!(clear_count, 0, "commands: {commands:?}");
    }
}

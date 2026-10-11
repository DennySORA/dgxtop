//! Ratatui backend that only encodes ANSI into a writer (the bounded frame
//! buffer). Unlike `CrosstermBackend` it never touches the terminal: the
//! size is set by the runner from resize events and the cursor position is
//! tracked, so a draw can never block on a cursor query or spawn `tput`.

use std::io::{self, Write};

use crossterm::cursor::{Hide, MoveTo, Show};
use crossterm::queue;
use crossterm::style::{
    Attribute, Color as CrosstermColor, Colors, Print, SetAttribute, SetBackgroundColor, SetColors,
    SetForegroundColor, SetUnderlineColor,
};
use crossterm::terminal::{Clear, ClearType as CrosstermClearType};
use ratatui::backend::{Backend, ClearType, IntoCrossterm, WindowSize};
use ratatui::buffer::Cell;
use ratatui::layout::{Position, Size};
use ratatui::style::{Color, Modifier};

/// ANSI-encoding backend over any writer.
#[derive(Debug)]
pub struct AnsiBackend<W: Write> {
    writer: W,
    size: Size,
    cursor: Position,
}

impl<W: Write> AnsiBackend<W> {
    pub fn new(writer: W, size: Size) -> Self {
        Self {
            writer,
            size,
            cursor: Position::ORIGIN,
        }
    }

    /// New terminal size (from a resize event); the next draw resizes.
    pub fn set_size(&mut self, size: Size) {
        self.size = size;
    }

    #[cfg(test)]
    pub fn writer_mut(&mut self) -> &mut W {
        &mut self.writer
    }
}

impl<W: Write> Backend for AnsiBackend<W> {
    type Error = io::Error;

    fn draw<'a, I>(&mut self, content: I) -> io::Result<()>
    where
        I: Iterator<Item = (u16, u16, &'a Cell)>,
    {
        let mut foreground = Color::Reset;
        let mut background = Color::Reset;
        let mut underline = Color::Reset;
        let mut modifier = Modifier::empty();
        let mut last: Option<Position> = None;
        for (x, y, cell) in content {
            if !matches!(last, Some(previous) if x == previous.x + 1 && y == previous.y) {
                queue!(self.writer, MoveTo(x, y))?;
            }
            last = Some(Position { x, y });
            if cell.modifier != modifier {
                queue_modifier_change(&mut self.writer, modifier, cell.modifier)?;
                modifier = cell.modifier;
            }
            if cell.fg != foreground || cell.bg != background {
                queue!(
                    self.writer,
                    SetColors(Colors::new(
                        cell.fg.into_crossterm(),
                        cell.bg.into_crossterm()
                    ))
                )?;
                foreground = cell.fg;
                background = cell.bg;
            }
            if cell.underline_color != underline {
                queue!(
                    self.writer,
                    SetUnderlineColor(cell.underline_color.into_crossterm())
                )?;
                underline = cell.underline_color;
            }
            queue!(self.writer, Print(cell.symbol()))?;
        }
        if let Some(position) = last {
            self.cursor = Position {
                x: position.x.saturating_add(1),
                y: position.y,
            };
        }
        queue!(
            self.writer,
            SetForegroundColor(CrosstermColor::Reset),
            SetBackgroundColor(CrosstermColor::Reset),
            SetUnderlineColor(CrosstermColor::Reset),
            SetAttribute(Attribute::Reset),
        )
    }

    fn append_lines(&mut self, lines: u16) -> io::Result<()> {
        for _ in 0..lines {
            queue!(self.writer, Print("\n"))?;
        }
        Ok(())
    }

    fn hide_cursor(&mut self) -> io::Result<()> {
        queue!(self.writer, Hide)
    }

    fn show_cursor(&mut self) -> io::Result<()> {
        queue!(self.writer, Show)
    }

    fn get_cursor_position(&mut self) -> io::Result<Position> {
        Ok(self.cursor)
    }

    fn set_cursor_position<P: Into<Position>>(&mut self, position: P) -> io::Result<()> {
        let position = position.into();
        queue!(self.writer, MoveTo(position.x, position.y))?;
        self.cursor = position;
        Ok(())
    }

    fn clear(&mut self) -> io::Result<()> {
        self.clear_region(ClearType::All)
    }

    fn clear_region(&mut self, clear_type: ClearType) -> io::Result<()> {
        let kind = match clear_type {
            ClearType::All => CrosstermClearType::All,
            ClearType::AfterCursor => CrosstermClearType::FromCursorDown,
            ClearType::BeforeCursor => CrosstermClearType::FromCursorUp,
            ClearType::CurrentLine => CrosstermClearType::CurrentLine,
            ClearType::UntilNewLine => CrosstermClearType::UntilNewLine,
        };
        queue!(self.writer, Clear(kind))
    }

    fn size(&self) -> io::Result<Size> {
        Ok(self.size)
    }

    fn window_size(&mut self) -> io::Result<WindowSize> {
        Ok(WindowSize {
            columns_rows: self.size,
            pixels: Size::default(),
        })
    }

    fn flush(&mut self) -> io::Result<()> {
        self.writer.flush()
    }
}

/// Emits only the attribute changes between two modifier sets (port of the
/// Ratatui crossterm backend's private diff).
fn queue_modifier_change<W: Write>(writer: &mut W, from: Modifier, to: Modifier) -> io::Result<()> {
    let removed = from - to;
    if removed.contains(Modifier::REVERSED) {
        queue!(writer, SetAttribute(Attribute::NoReverse))?;
    }
    let reset_intensity = removed.contains(Modifier::BOLD) || removed.contains(Modifier::DIM);
    if reset_intensity {
        queue!(writer, SetAttribute(Attribute::NormalIntensity))?;
        if to.contains(Modifier::DIM) {
            queue!(writer, SetAttribute(Attribute::Dim))?;
        }
        if to.contains(Modifier::BOLD) {
            queue!(writer, SetAttribute(Attribute::Bold))?;
        }
    }
    if removed.contains(Modifier::ITALIC) {
        queue!(writer, SetAttribute(Attribute::NoItalic))?;
    }
    if removed.contains(Modifier::UNDERLINED) {
        queue!(writer, SetAttribute(Attribute::NoUnderline))?;
    }
    if removed.contains(Modifier::CROSSED_OUT) {
        queue!(writer, SetAttribute(Attribute::NotCrossedOut))?;
    }
    if removed.contains(Modifier::HIDDEN) {
        queue!(writer, SetAttribute(Attribute::NoHidden))?;
    }
    if removed.contains(Modifier::SLOW_BLINK) || removed.contains(Modifier::RAPID_BLINK) {
        queue!(writer, SetAttribute(Attribute::NoBlink))?;
    }
    let added = to - from;
    if added.contains(Modifier::REVERSED) {
        queue!(writer, SetAttribute(Attribute::Reverse))?;
    }
    if added.contains(Modifier::BOLD) && !reset_intensity {
        queue!(writer, SetAttribute(Attribute::Bold))?;
    }
    if added.contains(Modifier::ITALIC) {
        queue!(writer, SetAttribute(Attribute::Italic))?;
    }
    if added.contains(Modifier::UNDERLINED) {
        queue!(writer, SetAttribute(Attribute::Underlined))?;
    }
    if added.contains(Modifier::DIM) && !reset_intensity {
        queue!(writer, SetAttribute(Attribute::Dim))?;
    }
    if added.contains(Modifier::CROSSED_OUT) {
        queue!(writer, SetAttribute(Attribute::CrossedOut))?;
    }
    if added.contains(Modifier::HIDDEN) {
        queue!(writer, SetAttribute(Attribute::Hidden))?;
    }
    if added.contains(Modifier::SLOW_BLINK) {
        queue!(writer, SetAttribute(Attribute::SlowBlink))?;
    }
    if added.contains(Modifier::RAPID_BLINK) {
        queue!(writer, SetAttribute(Attribute::RapidBlink))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::Terminal;
    use ratatui::style::Style;

    #[test]
    fn draws_encode_ansi_without_touching_the_terminal() {
        let backend = AnsiBackend::new(Vec::new(), Size::new(10, 2));
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| {
                frame.buffer_mut().set_string(
                    0,
                    0,
                    "hi",
                    Style::new().fg(Color::Red).add_modifier(Modifier::BOLD),
                );
            })
            .unwrap();
        let bytes = String::from_utf8(terminal.backend_mut().writer_mut().clone()).unwrap();
        assert!(bytes.contains("hi"), "{bytes:?}");
        assert!(bytes.contains("\u{1b}[1;1H"), "cursor move: {bytes:?}");
        assert!(bytes.contains("\u{1b}[1m"), "bold: {bytes:?}");
        assert!(bytes.contains("\u{1b}[?25l"), "cursor hidden: {bytes:?}");
    }

    #[test]
    fn terminal_clear_and_resize_work_without_terminal_queries() {
        let backend = AnsiBackend::new(Vec::new(), Size::new(20, 5));
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.clear().unwrap();
        terminal.backend_mut().set_size(Size::new(30, 6));
        terminal
            .draw(|frame| assert_eq!(frame.area().width, 30))
            .unwrap();
    }
}

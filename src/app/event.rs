use crossterm::event::{KeyEvent, MouseEvent};

/// Events flowing through the application.
#[derive(Debug)]
pub enum AppEvent {
    /// Periodic tick — triggers data collection and re-render.
    Tick,
    /// Keyboard input.
    Key(KeyEvent),
    /// Mouse input.
    Mouse(MouseEvent),
    /// Terminal resize.
    Resize(u16, u16),
}

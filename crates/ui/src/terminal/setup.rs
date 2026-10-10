//! Real terminal setup: raw mode, alternate screen and mouse capture via
//! crossterm, and their restoration on every exit path (including a panic
//! hook for the composition root).

use std::io::Write;

use crossterm::cursor::{Hide, Show};
use crossterm::event::{DisableMouseCapture, EnableMouseCapture};
use crossterm::queue;
use crossterm::style::{Attribute, ResetColor, SetAttribute};
use crossterm::terminal::{Clear, ClearType, EnterAlternateScreen, LeaveAlternateScreen};
use dgxtop_core::ports::{InputGate, SessionPort, TerminalTransport};

use super::clock::SystemClock;
use super::error::TuiError;
use super::input::GatedInput;
use super::runner::{TuiExit, TuiLoop, TuiOptions};

/// Fallback size when the terminal size cannot be read.
const FALLBACK_SIZE: (u16, u16) = (80, 24);

/// Bytes that switch to the TUI screen.
pub(crate) fn enter_sequence(mouse: bool) -> Vec<u8> {
    let mut bytes = Vec::new();
    // Writing into a Vec cannot fail.
    let _ = queue!(bytes, EnterAlternateScreen, Hide, Clear(ClearType::All));
    if mouse {
        let _ = queue!(bytes, EnableMouseCapture);
    }
    bytes
}

/// Bytes that undo [`enter_sequence`].
pub(crate) fn restore_sequence() -> Vec<u8> {
    let mut bytes = Vec::new();
    let _ = queue!(
        bytes,
        DisableMouseCapture,
        ResetColor,
        SetAttribute(Attribute::Reset),
        Show,
        LeaveAlternateScreen
    );
    bytes
}

/// Disables raw mode when dropped, on every exit path.
struct RawModeGuard;

impl RawModeGuard {
    fn enable() -> Result<Self, TuiError> {
        crossterm::terminal::enable_raw_mode().map_err(TuiError::Setup)?;
        Ok(Self)
    }
}

impl Drop for RawModeGuard {
    fn drop(&mut self) {
        let _ = crossterm::terminal::disable_raw_mode();
    }
}

/// Runs the interactive TUI on the calling (terminal main) thread.
///
/// Enters raw mode, the alternate screen and (optionally) mouse capture,
/// runs [`TuiLoop`] with crossterm input behind the injected hang-up-aware
/// `input_gate` and the injected non-blocking transport, and always
/// restores the terminal before returning. The caller should also install
/// [`install_panic_hook`].
pub fn run_tui(
    session: Box<dyn SessionPort>,
    transport: Box<dyn TerminalTransport>,
    input_gate: Box<dyn InputGate>,
    options: TuiOptions,
) -> Result<TuiExit, TuiError> {
    // The size is read once here (resize events carry later sizes) because
    // crossterm's fallback may spawn `tput`; never on the frame path.
    let size = crossterm::terminal::size().unwrap_or(FALLBACK_SIZE);
    let raw_mode = RawModeGuard::enable()?;
    let mut tui = TuiLoop::new(
        session,
        transport,
        Box::new(GatedInput::crossterm(input_gate)),
        Box::new(SystemClock::new()),
        options,
        size,
    )?;
    tui.enter_screen()?;
    let result = tui.run();
    drop(tui);
    drop(raw_mode);
    result
}

/// Best-effort terminal restore for panic hooks and fatal paths: leaves the
/// alternate screen, shows the cursor, disables mouse capture and raw mode.
/// If the terminal accepts no bytes the sequence may not become visible.
pub fn restore_terminal() {
    let _ = crossterm::terminal::disable_raw_mode();
    let mut stdout = std::io::stdout();
    let _ = stdout.write_all(&restore_sequence());
    let _ = stdout.flush();
}

/// Installs a panic hook that restores the terminal before the previous
/// hook prints. Panics caught by the viewer's module isolation are not
/// reported (they are drawn as a module-error box instead).
pub fn install_panic_hook() {
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        if crate::viewer::in_isolated_render() {
            return;
        }
        restore_terminal();
        previous(info);
    }));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn enter_and_restore_sequences_are_symmetric() {
        let enter = String::from_utf8(enter_sequence(true)).unwrap();
        assert!(
            enter.contains("\u{1b}[?1049h"),
            "alternate screen: {enter:?}"
        );
        assert!(enter.contains("\u{1b}[?25l"), "hide cursor: {enter:?}");
        assert!(enter.contains("\u{1b}[?1000h"), "mouse capture: {enter:?}");
        let without_mouse = String::from_utf8(enter_sequence(false)).unwrap();
        assert!(!without_mouse.contains("\u{1b}[?1000h"));
        let restore = String::from_utf8(restore_sequence()).unwrap();
        assert!(
            restore.contains("\u{1b}[?1049l"),
            "leave alternate screen: {restore:?}"
        );
        assert!(restore.contains("\u{1b}[?25h"), "show cursor: {restore:?}");
        assert!(
            restore.contains("\u{1b}[?1000l"),
            "mouse released: {restore:?}"
        );
        assert!(restore.len() < super::super::output::CONTROL_RESERVE);
    }
}

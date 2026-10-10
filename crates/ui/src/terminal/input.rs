//! InputBackend: where key, mouse and resize events come from. The real one
//! reads crossterm events behind a hang-up-aware gate; tests script events
//! against a fake clock.

use std::io;
use std::time::Duration;

use crossterm::event::Event;
use dgxtop_core::ports::{InputGate, InputReadiness};

use super::error::TuiError;

/// Source of terminal input events.
pub trait InputSource {
    /// Waits up to `timeout` for one event (`Ok(None)` on timeout).
    fn poll_event(&mut self, timeout: Duration) -> Result<Option<Event>, TuiError>;
}

/// Events the input library can deliver without waiting.
pub trait ReadyEvents {
    /// One already available event, if any; never blocks.
    fn next_ready(&mut self) -> io::Result<Option<Event>>;
}

/// crossterm's pending events: parsed key and mouse bytes, resize signals.
#[derive(Debug, Clone, Copy, Default)]
pub struct CrosstermEvents;

impl ReadyEvents for CrosstermEvents {
    fn next_ready(&mut self) -> io::Result<Option<Event>> {
        if crossterm::event::poll(Duration::ZERO)? {
            crossterm::event::read().map(Some)
        } else {
            Ok(None)
        }
    }
}

/// Terminal input that waits on an [`InputGate`] instead of crossterm's own
/// poll. crossterm retries end-of-file reads forever, so its reader is only
/// entered right after the gate saw the terminal alive, and a hang-up ends
/// the loop with [`TuiError::InputClosed`].
pub struct GatedInput {
    gate: Box<dyn InputGate>,
    events: Box<dyn ReadyEvents>,
}

impl GatedInput {
    pub fn new(gate: Box<dyn InputGate>, events: Box<dyn ReadyEvents>) -> Self {
        Self { gate, events }
    }

    /// crossterm events behind `gate`.
    pub fn crossterm(gate: Box<dyn InputGate>) -> Self {
        Self::new(gate, Box::new(CrosstermEvents))
    }

    fn readable(&mut self, timeout: Duration) -> Result<bool, TuiError> {
        match self.gate.wait(timeout).map_err(TuiError::Input)? {
            InputReadiness::Readable => Ok(true),
            InputReadiness::TimedOut => Ok(false),
            InputReadiness::HungUp => Err(TuiError::InputClosed),
        }
    }

    fn next_ready(&mut self) -> Result<Option<Event>, TuiError> {
        self.events.next_ready().map_err(TuiError::Input)
    }
}

impl InputSource for GatedInput {
    fn poll_event(&mut self, timeout: Duration) -> Result<Option<Event>, TuiError> {
        // Parsed events and resize signals do not make the terminal readable,
        // so they are taken before waiting (after the hang-up check).
        let readable_now = self.readable(Duration::ZERO)?;
        if let Some(event) = self.next_ready()? {
            return Ok(Some(event));
        }
        if timeout.is_zero() {
            return Ok(None);
        }
        if readable_now {
            // Bytes crossterm left unread do not wake its edge-triggered
            // poll, and the gate would report them at once: wait out the
            // timeout watching only for a hang-up.
            if self
                .gate
                .wait_for_hang_up(timeout)
                .map_err(TuiError::Input)?
            {
                return Err(TuiError::InputClosed);
            }
            return Ok(None);
        }
        if self.readable(timeout)? {
            return self.next_ready();
        }
        Ok(None)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    use std::collections::VecDeque;
    use std::sync::{Arc, Mutex};

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    enum Wait {
        Input(Duration),
        HangUp(Duration),
    }

    /// Scripted gate; records every wait.
    #[derive(Default)]
    struct ScriptedGate {
        readiness: VecDeque<io::Result<InputReadiness>>,
        hang_ups: VecDeque<bool>,
        waits: Arc<Mutex<Vec<Wait>>>,
    }

    impl InputGate for ScriptedGate {
        fn wait(&mut self, timeout: Duration) -> io::Result<InputReadiness> {
            self.waits.lock().unwrap().push(Wait::Input(timeout));
            self.readiness
                .pop_front()
                .unwrap_or(Ok(InputReadiness::TimedOut))
        }

        fn wait_for_hang_up(&mut self, timeout: Duration) -> io::Result<bool> {
            self.waits.lock().unwrap().push(Wait::HangUp(timeout));
            Ok(self.hang_ups.pop_front().unwrap_or(false))
        }
    }

    /// Scripted crossterm stand-in; counts how often it was entered.
    #[derive(Default)]
    struct ScriptedEvents {
        events: VecDeque<Option<Event>>,
        calls: Arc<Mutex<usize>>,
    }

    impl ReadyEvents for ScriptedEvents {
        fn next_ready(&mut self) -> io::Result<Option<Event>> {
            *self.calls.lock().unwrap() += 1;
            Ok(self.events.pop_front().flatten())
        }
    }

    struct Rig {
        input: GatedInput,
        waits: Arc<Mutex<Vec<Wait>>>,
        calls: Arc<Mutex<usize>>,
    }

    fn rig(
        readiness: Vec<io::Result<InputReadiness>>,
        hang_ups: Vec<bool>,
        events: Vec<Option<Event>>,
    ) -> Rig {
        let gate = ScriptedGate {
            readiness: readiness.into(),
            hang_ups: hang_ups.into(),
            ..ScriptedGate::default()
        };
        let source = ScriptedEvents {
            events: events.into(),
            ..ScriptedEvents::default()
        };
        let waits = Arc::clone(&gate.waits);
        let calls = Arc::clone(&source.calls);
        Rig {
            input: GatedInput::new(Box::new(gate), Box::new(source)),
            waits,
            calls,
        }
    }

    impl Rig {
        fn waits(&self) -> Vec<Wait> {
            self.waits.lock().unwrap().clone()
        }

        fn calls(&self) -> usize {
            *self.calls.lock().unwrap()
        }
    }

    fn key(code: char) -> Event {
        Event::Key(KeyEvent::new(KeyCode::Char(code), KeyModifiers::NONE))
    }

    const TIMEOUT: Duration = Duration::from_millis(50);

    #[test]
    fn hang_up_ends_input_without_entering_crossterm() {
        let mut rig = rig(vec![Ok(InputReadiness::HungUp)], vec![], vec![]);
        let result = rig.input.poll_event(TIMEOUT);
        assert!(matches!(result, Err(TuiError::InputClosed)), "{result:?}");
        assert_eq!(rig.calls(), 0);
    }

    #[test]
    fn readable_terminal_delivers_the_parsed_event() {
        let mut rig = rig(
            vec![Ok(InputReadiness::Readable)],
            vec![],
            vec![Some(key('q'))],
        );
        assert_eq!(rig.input.poll_event(TIMEOUT).unwrap(), Some(key('q')));
        assert_eq!(rig.waits(), vec![Wait::Input(Duration::ZERO)]);
    }

    #[test]
    fn hang_up_during_the_wait_ends_input() {
        let mut rig = rig(
            vec![Ok(InputReadiness::TimedOut), Ok(InputReadiness::HungUp)],
            vec![],
            vec![],
        );
        let result = rig.input.poll_event(TIMEOUT);
        assert!(matches!(result, Err(TuiError::InputClosed)), "{result:?}");
        // crossterm was only entered before the hang-up.
        assert_eq!(rig.calls(), 1);
    }

    #[test]
    fn unread_bytes_wait_for_hang_up_instead_of_spinning() {
        let mut rig = rig(vec![Ok(InputReadiness::Readable)], vec![false], vec![]);
        assert_eq!(rig.input.poll_event(TIMEOUT).unwrap(), None);
        assert_eq!(
            rig.waits(),
            vec![Wait::Input(Duration::ZERO), Wait::HangUp(TIMEOUT)]
        );
    }

    #[test]
    fn hang_up_while_bytes_are_unread_ends_input() {
        let mut rig = rig(vec![Ok(InputReadiness::Readable)], vec![true], vec![]);
        let result = rig.input.poll_event(TIMEOUT);
        assert!(matches!(result, Err(TuiError::InputClosed)), "{result:?}");
    }

    #[test]
    fn gate_failures_are_input_errors() {
        let mut rig = rig(vec![Err(io::Error::other("poll failed"))], vec![], vec![]);
        let result = rig.input.poll_event(TIMEOUT);
        assert!(matches!(result, Err(TuiError::Input(_))), "{result:?}");
        assert_eq!(rig.calls(), 0);
    }
}

use std::time::{Duration, Instant};

use crossbeam_channel::{Receiver, Sender};
use crossterm::event::{self, Event};

use super::event::AppEvent;

/// Spawns the event producer thread that generates Tick and input events.
/// Returns a receiver for AppEvents.
pub fn spawn_event_loop(tick_rate: Duration) -> (Sender<()>, Receiver<AppEvent>) {
    let (tx, rx) = crossbeam_channel::unbounded();
    let (stop_tx, stop_rx) = crossbeam_channel::bounded::<()>(1);

    std::thread::Builder::new()
        .name("dgxtop-events".to_owned())
        .spawn(move || {
            let mut last_tick = Instant::now();

            loop {
                // Check for stop signal
                if stop_rx.try_recv().is_ok() {
                    break;
                }

                let timeout = tick_rate
                    .checked_sub(last_tick.elapsed())
                    .unwrap_or(Duration::ZERO);

                if event::poll(timeout).unwrap_or(false) {
                    match event::read() {
                        Ok(Event::Key(key)) => {
                            if tx.send(AppEvent::Key(key)).is_err() {
                                break;
                            }
                        }
                        Ok(Event::Mouse(mouse)) => {
                            if tx.send(AppEvent::Mouse(mouse)).is_err() {
                                break;
                            }
                        }
                        Ok(Event::Resize(w, h)) => {
                            if tx.send(AppEvent::Resize(w, h)).is_err() {
                                break;
                            }
                        }
                        _ => {}
                    }
                }

                if last_tick.elapsed() >= tick_rate {
                    if tx.send(AppEvent::Tick).is_err() {
                        break;
                    }
                    last_tick = Instant::now();
                }
            }
        })
        .expect("failed to spawn event loop thread");

    (stop_tx, rx)
}

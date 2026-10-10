//! RenderScheduler: dirty-only frames. Nothing is drawn unless the active
//! projection revision, the viewport or the controller-visible state
//! changed, or the once-per-second age tick is due. Data-driven frames are
//! capped at `fps` (2 fps while the Manager reports pressure), input-driven
//! frames at 20 fps.

use std::time::Duration;

/// Hard cap for input-driven redraws (20 fps).
pub const INPUT_REDRAW_INTERVAL: Duration = Duration::from_millis(50);
/// Age text granularity.
pub const AGE_TICK: Duration = Duration::from_secs(1);
/// Data frame interval while the Manager is pressured/throttled (2 fps).
const PRESSURED_INTERVAL: Duration = Duration::from_millis(500);

/// What a frame would show.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RenderKey {
    pub frame_revision: Option<u64>,
    pub viewport: (u16, u16),
    pub ui_version: u64,
}

/// Scheduler decision.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decision {
    Draw,
    /// Not before this loop time.
    WaitUntil(Duration),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RenderScheduler {
    data_interval: Duration,
    last_draw_at: Option<Duration>,
    last_key: Option<RenderKey>,
    full_redraw: bool,
    not_before: Duration,
}

impl RenderScheduler {
    /// `fps` is clamped to the configuration range 1..=20.
    pub fn new(fps: u32) -> Self {
        let fps = u64::from(fps.clamp(1, 20));
        Self {
            data_interval: Duration::from_millis(1000 / fps),
            last_draw_at: None,
            last_key: None,
            full_redraw: false,
            not_before: Duration::ZERO,
        }
    }

    pub fn decide(&self, key: RenderKey, now: Duration, pressured: bool) -> Decision {
        if now < self.not_before {
            return Decision::WaitUntil(self.not_before);
        }
        let (Some(last_at), Some(last)) = (self.last_draw_at, self.last_key) else {
            return Decision::Draw;
        };
        let interactive =
            self.full_redraw || key.viewport != last.viewport || key.ui_version != last.ui_version;
        let interval = if interactive {
            INPUT_REDRAW_INTERVAL
        } else if key.frame_revision != last.frame_revision {
            if pressured {
                self.data_interval.max(PRESSURED_INTERVAL)
            } else {
                self.data_interval
            }
        } else {
            AGE_TICK
        };
        let due = last_at.saturating_add(interval);
        if now >= due {
            Decision::Draw
        } else {
            Decision::WaitUntil(due)
        }
    }

    pub fn drawn(&mut self, key: RenderKey, now: Duration) {
        self.last_key = Some(key);
        self.last_draw_at = Some(now);
        self.full_redraw = false;
    }

    /// The next frame must repaint everything (after an incomplete or slow
    /// flush, a failed draw or an overflow).
    pub fn request_full_redraw(&mut self) {
        self.full_redraw = true;
    }

    pub fn full_redraw_pending(&self) -> bool {
        self.full_redraw
    }

    /// Defers any draw until `until` (backoff after repeated failures).
    pub fn defer_until(&mut self, until: Duration) {
        self.not_before = until;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(revision: u64, ui: u64) -> RenderKey {
        RenderKey {
            frame_revision: Some(revision),
            viewport: (80, 24),
            ui_version: ui,
        }
    }

    fn ms(value: u64) -> Duration {
        Duration::from_millis(value)
    }

    #[test]
    fn data_frames_respect_fps_and_pressure() {
        let mut scheduler = RenderScheduler::new(4);
        scheduler.drawn(key(1, 0), ms(0));
        assert_eq!(
            scheduler.decide(key(2, 0), ms(100), false),
            Decision::WaitUntil(ms(250))
        );
        assert_eq!(scheduler.decide(key(2, 0), ms(250), false), Decision::Draw);
        assert_eq!(
            scheduler.decide(key(2, 0), ms(250), true),
            Decision::WaitUntil(ms(500))
        );
        let fast = RenderScheduler::new(100);
        assert_eq!(fast.data_interval, ms(50), "fps is capped at 20");
    }

    #[test]
    fn input_and_resize_redraw_quickly_but_capped_at_20_fps() {
        let mut scheduler = RenderScheduler::new(1);
        scheduler.drawn(key(1, 0), ms(0));
        assert_eq!(
            scheduler.decide(key(1, 1), ms(10), false),
            Decision::WaitUntil(ms(50))
        );
        assert_eq!(scheduler.decide(key(1, 1), ms(50), false), Decision::Draw);
        let resized = RenderKey {
            viewport: (100, 30),
            ..key(1, 0)
        };
        assert_eq!(scheduler.decide(resized, ms(60), false), Decision::Draw);
    }

    #[test]
    fn full_redraw_and_backoff() {
        let mut scheduler = RenderScheduler::new(4);
        scheduler.drawn(key(1, 0), ms(0));
        scheduler.request_full_redraw();
        assert!(scheduler.full_redraw_pending());
        assert_eq!(scheduler.decide(key(1, 0), ms(50), false), Decision::Draw);
        scheduler.defer_until(ms(2000));
        assert_eq!(
            scheduler.decide(key(1, 0), ms(60), false),
            Decision::WaitUntil(ms(2000))
        );
        scheduler.drawn(key(1, 0), ms(2000));
        assert!(!scheduler.full_redraw_pending());
    }
}

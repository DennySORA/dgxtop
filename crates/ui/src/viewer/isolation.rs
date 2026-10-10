//! Panic isolation for module renders. A panicking module is caught inside
//! unwinding (`catch_unwind`); its area then shows a module-error box while
//! the other modules keep rendering. Aborts and UB cannot be caught and are
//! not claimed to be.

use std::any::Any;
use std::cell::Cell;
use std::panic::{AssertUnwindSafe, catch_unwind};

use dgxtop_core::text::truncate_utf8;

/// Longest panic message kept for display.
const MAX_MESSAGE_BYTES: usize = 200;

thread_local! {
    static ISOLATION_DEPTH: Cell<u32> = const { Cell::new(0) };
}

/// True while this thread renders a module inside isolation; the panic hook
/// installed by [`crate::install_panic_hook`] stays quiet then, because the
/// panic is caught and drawn as a module error instead of ending the TUI.
pub fn in_isolated_render() -> bool {
    ISOLATION_DEPTH.with(|depth| depth.get() > 0)
}

struct DepthGuard;

impl DepthGuard {
    fn enter() -> Self {
        ISOLATION_DEPTH.with(|depth| depth.set(depth.get().saturating_add(1)));
        Self
    }
}

impl Drop for DepthGuard {
    fn drop(&mut self) {
        ISOLATION_DEPTH.with(|depth| depth.set(depth.get().saturating_sub(1)));
    }
}

/// Runs `render`, converting a panic into its (bounded) message.
pub fn render_isolated<T>(render: impl FnOnce() -> T) -> Result<T, String> {
    let _guard = DepthGuard::enter();
    // The frame buffer touched by a panicking module is cleared by the caller
    // before reuse, so observing it after unwinding is sound.
    catch_unwind(AssertUnwindSafe(render)).map_err(|payload| panic_message(payload.as_ref()))
}

fn panic_message(payload: &(dyn Any + Send)) -> String {
    let message = payload
        .downcast_ref::<&str>()
        .map(|text| (*text).to_owned())
        .or_else(|| payload.downcast_ref::<String>().cloned())
        .unwrap_or_else(|| "panic without message".to_owned());
    crate::format::clean(truncate_utf8(&message, MAX_MESSAGE_BYTES))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn panics_become_messages_and_depth_is_restored() {
        assert!(!in_isolated_render());
        let result: Result<(), String> = render_isolated(|| {
            assert!(in_isolated_render());
            panic!("boom {}", 42);
        });
        assert_eq!(result, Err("boom 42".to_owned()));
        assert!(!in_isolated_render());
        assert_eq!(render_isolated(|| 7), Ok(7));
    }
}

//! Pure formatting: units, ages, unicode-width-aware text fitting and the
//! evidence-preserving rendering of [`MetricCell`](dgxtop_core::view::MetricCell)
//! values and local wall-clock times. Nothing here performs I/O or reads a
//! clock.

mod action;
mod age;
mod cell;
mod clock;
mod text;
mod units;

pub use action::action_state_label;
pub(crate) use action::{
    COMPACT_CONFIRMATION_WIDTH, CONFIRM_DELETE_HINT, CONFIRM_HINT, confirmation_lines,
};
pub use age::{
    format_age, format_countdown_ms, format_interval_ms, format_span_ns, format_span_seconds,
};
pub use cell::{
    CONFLICT_SYMBOL, CellDetail, CellText, CellTone, cell_tone, describe_cell, kind_tag,
    quality_word, state_label_for_width, value_text,
};
pub use clock::{format_local_clock, format_local_date, format_local_datetime, format_local_short};
pub use text::{
    clean, counted, display_width, gpu_number, gpu_with_model, pad_left, pad_right,
    short_gpu_model, truncate_middle, truncate_to_width,
};
pub use units::{
    NOT_AVAILABLE, fixed, format_bytes, format_compact_count, format_duration_ms, format_energy,
    format_optional_bytes, format_optional_percent, format_percent, format_scalar, format_si_rate,
    format_value,
};

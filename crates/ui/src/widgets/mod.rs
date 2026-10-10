//! Small custom widgets: labeled gauge (with `n/a` when missing), gap-aware
//! block sparkline (level colors, dimmed peaks) for every chart, and a
//! segmented bar for shares of one whole. Bars stand three quarters of a
//! row high (D-52).

mod bar;
mod gauge;
mod sparkline;
mod stacked_bar;

pub use gauge::Gauge;
pub use sparkline::GapSparkline;
pub use stacked_bar::{Segment, StackedBar};

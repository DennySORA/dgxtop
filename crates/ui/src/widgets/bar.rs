//! The glyph of every bar (D-52): a block three quarters of a row high,
//! standing on the row's floor. Terminal rows have no spacing, so bars of
//! full blocks touch the chart or the bar in the next row and, in the same
//! color under load, read as one block. The quarter row above every bar,
//! and the quarter row every chart leaves above its tallest column, keep
//! them apart while a bar stays nearly a row high.

/// One cell of a bar, filled or track.
pub(crate) const BAR: &str = "▆";

//! Test rig shared by the fault-injection scenarios.
//!
//! * [`probes`]: serial lock, temp dirs, free-space double, storage gate,
//!   bounded polling and `/proc` leak counters.
//! * [`workers`]: real workers wired like the composition root.
//! * [`manager_rig`]: a real Manager on its own control thread.
//! * [`action_rig`]: Manager + real SQLite audit + recording action port.
//! * [`filesystem`]: failpoint filesystem plus hung fsync / unreadable file.
//! * [`hosts`]: replay fixtures split per backend, `dgxtop --adapter-host`
//!   child launches and an unreapable-child launcher.
//! * [`telemetry_rig`]: Manager + supervisor + collectors + state.

pub mod action_rig;
pub mod filesystem;
pub mod hosts;
pub mod manager_rig;
pub mod probes;
pub mod telemetry_rig;
pub mod workers;

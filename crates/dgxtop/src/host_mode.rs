//! Internal `--adapter-host <id>` mode: the process the supervisor launches
//! for one live (or replay) adapter. It speaks only the framed IPC protocol on
//! stdin/stdout after the nonce handshake; it never opens a terminal, never
//! reads arbitrary paths and never performs process actions.

use std::path::Path;
use std::process::ExitCode;
use std::sync::Arc;

use dgxtop_adapters::registry::{AdapterOptions, build_live_adapter};
use dgxtop_adapters::replay::ReplayAdapter;
use dgxtop_core::clock::TimeProvider;
use dgxtop_core::identity::AdapterId;
use dgxtop_core::naming;
use dgxtop_core::ports::TelemetryPort;
use dgxtop_runtime::clock::{BootClock, host_identity};
use dgxtop_runtime::host::{host_nonce_from_env, run_adapter_host};

use crate::cli::Cli;
use crate::modes::{EXIT_INVALID, fail};

/// Runs the adapter host until the parent sends `shutdown` or closes the pipe.
pub fn run(cli: &Cli) -> ExitCode {
    match prepare(cli) {
        Ok((adapter, generation, nonce, clock)) => {
            let exit = run_adapter_host(
                adapter,
                generation,
                nonce,
                clock,
                std::io::stdin().lock(),
                std::io::stdout().lock(),
            );
            ExitCode::from(u8::try_from(exit.exit_code()).unwrap_or(EXIT_INVALID))
        }
        Err(message) => fail(EXIT_INVALID, &message),
    }
}

type Prepared = (Box<dyn TelemetryPort>, u32, String, Arc<dyn TimeProvider>);

fn prepare(cli: &Cli) -> Result<Prepared, String> {
    let adapter_name = cli
        .adapter_host
        .as_deref()
        .ok_or("--adapter-host requires an adapter id")?;
    let generation = cli
        .host_generation
        .ok_or("adapter hosts are started by the supervisor (missing --host-generation)")?;
    // Without the supervisor's nonce this mode refuses to run at all.
    let nonce = host_nonce_from_env().map_err(|error| error.to_string())?;
    let clock: Arc<dyn TimeProvider> =
        Arc::new(BootClock::new().map_err(|error| error.to_string())?);
    let host_id = host_identity(Path::new("/")).map_err(|error| error.to_string())?;
    let adapter: Box<dyn TelemetryPort> = if naming::is_replay_adapter(adapter_name) {
        let fixture = cli
            .replay_fixture
            .as_deref()
            .ok_or("replay adapter hosts need --replay-fixture")?;
        let slice = cli.replay_slice().map_err(|error| error.to_string())?;
        let adapter_id = AdapterId::new(adapter_name).map_err(|error| error.to_string())?;
        Box::new(
            ReplayAdapter::from_path(fixture, adapter_id, Arc::clone(&clock), host_id, slice)
                .map_err(|error| error.to_string())?,
        )
    } else {
        let options = AdapterOptions {
            hwmon_reads_thermal_zones: cli.hwmon_reads_thermal_zones,
            ..AdapterOptions::default()
        };
        build_live_adapter(adapter_name, Arc::clone(&clock), host_id, options)
            .map_err(|error| error.to_string())?
    };
    Ok((adapter, generation, nonce, clock))
}

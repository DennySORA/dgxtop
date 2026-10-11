//! The adapter hosts of this run: one child process per live adapter, or
//! the replay adapter in process for `--replay`.

use std::sync::Arc;

use dgxtop_adapters::replay::ReplayAdapter;
use dgxtop_core::clock::TimeProvider;
use dgxtop_core::config::Config;
use dgxtop_core::identity::{AdapterId, HostId};
use dgxtop_core::naming;
use dgxtop_core::observation::AdapterManifest;
use dgxtop_core::ports::TelemetryPort;
use dgxtop_core::quality::Namespace;
use dgxtop_runtime::supervisor::{HostLaunch, HostSpec};

use super::StartError;
use crate::cli::Cli;

/// Adapter hosts of this run plus their manifests.
pub(super) fn host_specs(
    cli: &Cli,
    effective: &Config,
    namespace: Namespace,
    clock: &Arc<dyn TimeProvider>,
    host_id: &HostId,
) -> Result<(Vec<HostSpec>, Vec<AdapterManifest>), StartError> {
    if namespace == Namespace::Replay {
        return replay_host(cli, clock, host_id);
    }
    let program = std::env::current_exe()?;
    let limit = usize::try_from(effective.sampling.max_adapters).unwrap_or(8);
    let mut specs = Vec::new();
    let mut manifests = Vec::new();
    let selected: Vec<&str> = naming::adapters::LIVE
        .iter()
        .copied()
        .filter(|adapter| effective.sampling.gpu_enabled || *adapter != naming::adapters::NVML)
        .take(limit)
        .collect();
    for adapter in selected.iter().copied() {
        let Some(manifest) = dgxtop_adapters::registry::live_manifest(adapter) else {
            continue;
        };
        specs.push(HostSpec {
            adapter_id: AdapterId::new(adapter)?,
            launch: HostLaunch::ChildProcess {
                program: program.clone(),
                args: vec!["--adapter-host".to_owned(), adapter.to_owned()],
            },
        });
        manifests.push(manifest);
    }
    Ok((specs, manifests))
}

fn replay_host(
    cli: &Cli,
    clock: &Arc<dyn TimeProvider>,
    host_id: &HostId,
) -> Result<(Vec<HostSpec>, Vec<AdapterManifest>), StartError> {
    let fixture = cli
        .replay
        .clone()
        .ok_or_else(|| StartError::invalid("--replay requires a fixture"))?;
    let adapter_id = AdapterId::new(naming::adapters::REPLAY)?;
    // Validate the fixture up front so a bad file is reported before the UI starts.
    let probe = ReplayAdapter::from_path(
        &fixture,
        adapter_id.clone(),
        Arc::clone(clock),
        host_id.clone(),
        None,
    )
    .map_err(|error| StartError::invalid(format!("replay fixture: {error}")))?
    .with_in_process(true);
    let manifest = probe.manifest().clone();
    drop(probe);
    let factory_clock = Arc::clone(clock);
    let factory_host = host_id.clone();
    let factory_adapter = adapter_id.clone();
    let factory: Arc<dyn Fn() -> Box<dyn TelemetryPort> + Send + Sync> = Arc::new(move || {
        match ReplayAdapter::from_path(
            &fixture,
            factory_adapter.clone(),
            Arc::clone(&factory_clock),
            factory_host.clone(),
            None,
        ) {
            Ok(adapter) => Box::new(adapter.with_in_process(true)),
            Err(_) => Box::new(crate::replay_unavailable::UnavailableReplay::new(
                factory_adapter.clone(),
            )),
        }
    });
    Ok((
        vec![HostSpec {
            adapter_id,
            launch: HostLaunch::InProcess(factory),
        }],
        vec![manifest],
    ))
}

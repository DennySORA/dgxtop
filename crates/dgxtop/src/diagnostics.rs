//! `--diagnostics-json`: version, dependency, SQLite, capability and bounded
//! health summary. Capabilities come from a real catalog discovery through
//! the isolated adapter hosts (never from in-process driver calls); output
//! contains no secrets (host identity is already a hash, no command lines).

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;
use std::time::{Duration, Instant};

use crossbeam_channel::{Receiver, bounded};
use dgxtop_core::budget::{ByteLedger, Partition, QueueGate};
use dgxtop_core::clock::TimeProvider;
use dgxtop_core::diagnostics::Diagnostics;
use dgxtop_core::identity::AdapterId;
use dgxtop_core::naming;
use dgxtop_core::observation::{CatalogDelta, DiscoverRequest};
use dgxtop_core::ports::{AdapterJob, Dispatch, IngestionCompletion, NullLogger};
use dgxtop_core::view::{HostState, HostStatus};
use dgxtop_runtime::clock::{BootClock, host_identity, host_name, kernel_release};
use dgxtop_runtime::supervisor::{
    HostLaunch, HostSpec, Supervisor, SupervisorConfig, SupervisorHandle,
};
use serde::Serialize;
use uuid::Uuid;

use crate::cli::Cli;
use crate::modes::{EXIT_FAILURE, fail, sqlite_facts};
use crate::startup;
use crate::version::BuildInfo;

/// Upper bound for the whole discovery pass.
const DISCOVERY_BUDGET: Duration = Duration::from_secs(6);

#[derive(Debug, Serialize)]
struct DiagnosticsReport {
    version: String,
    commit: String,
    toolchain: String,
    target: String,
    profile: String,
    sqlite: String,
    sqlite_gate_ok: bool,
    host_id: String,
    host_name: Option<String>,
    kernel: Option<String>,
    config_file: String,
    state_dir: String,
    config_valid: bool,
    config_error: Option<String>,
    adapters: Vec<AdapterReport>,
    managed_budget_mib: u32,
}

#[derive(Debug, Serialize)]
struct AdapterReport {
    adapter_id: String,
    host_state: String,
    host_detail: Option<String>,
    availability: Option<String>,
    entities: usize,
    sources: usize,
    capabilities: BTreeMap<String, usize>,
    capability_issues: Vec<String>,
    error_code: Option<String>,
}

/// Runs the diagnostics pass and prints one JSON document.
pub fn run(cli: &Cli) -> ExitCode {
    match collect(cli) {
        Ok(report) => match serde_json::to_string_pretty(&report) {
            Ok(text) => {
                println!("{text}");
                ExitCode::SUCCESS
            }
            Err(error) => fail(EXIT_FAILURE, &error.to_string()),
        },
        Err(message) => fail(EXIT_FAILURE, &message),
    }
}

fn collect(cli: &Cli) -> Result<DiagnosticsReport, String> {
    let build = BuildInfo::current();
    let sqlite = sqlite_facts();
    let paths = startup::resolve_paths(cli).map_err(|error| error.to_string())?;
    let loaded = startup::load_configuration(cli, &paths, startup::real_filesystem());
    let (effective, config_error) = match &loaded {
        Ok((loaded, _)) => (loaded.effective.clone(), None),
        Err(error) => (
            dgxtop_core::config::Config::defaults().map_err(|defaults| defaults.to_string())?,
            Some(error.to_string()),
        ),
    };
    let clock: Arc<dyn TimeProvider> =
        Arc::new(BootClock::new().map_err(|error| error.to_string())?);
    let host_id = host_identity(std::path::Path::new("/")).map_err(|error| error.to_string())?;
    let adapters: Vec<&str> = naming::adapters::LIVE
        .iter()
        .copied()
        .filter(|adapter| effective.sampling.gpu_enabled || *adapter != naming::adapters::NVML)
        .collect();
    let reports = discover_adapters(&adapters, &effective, Arc::clone(&clock))?;
    Ok(DiagnosticsReport {
        version: build.version.to_owned(),
        commit: build.commit.to_owned(),
        toolchain: build.rustc.to_owned(),
        target: build.target.to_owned(),
        profile: build.profile.to_owned(),
        sqlite: sqlite.sqlite_version,
        sqlite_gate_ok: sqlite.sqlite_gate_ok,
        host_id: host_id.to_string(),
        host_name: host_name().ok(),
        kernel: kernel_release().ok(),
        config_file: paths.config_file.display().to_string(),
        state_dir: paths.state_dir.display().to_string(),
        config_valid: config_error.is_none(),
        config_error,
        adapters: reports,
        managed_budget_mib: effective.store.memory_limit_mib,
    })
}

fn discover_adapters(
    adapters: &[&str],
    config: &dgxtop_core::config::Config,
    clock: Arc<dyn TimeProvider>,
) -> Result<Vec<AdapterReport>, String> {
    let program = std::env::current_exe().map_err(|error| error.to_string())?;
    let ledger = ByteLedger::new(config.partition_limits());
    let ingress = QueueGate::new(
        "ingress",
        usize::try_from(config.queues.collection_batches).unwrap_or(32),
        u64::from(config.queues.ingress_bytes),
        ledger.clone(),
        Partition::Ingress,
    );
    let (sender, completions) = bounded(1024);
    let handle = Supervisor::start(
        SupervisorConfig::from_config(config),
        host_specs(adapters, &program)?,
        ledger,
        ingress,
        sender,
        Arc::clone(&clock),
        Arc::new(NullLogger),
        Diagnostics::new(),
    )
    .map_err(|error| error.to_string())?;
    let deadline = Instant::now() + DISCOVERY_BUDGET;
    let mut reports = Vec::new();
    for adapter in adapters {
        let status = wait_for_host(&handle, &completions, adapter, deadline);
        let catalog = match &status {
            Some(host) if host.state == HostState::Healthy => discover_pages(
                &handle,
                &completions,
                adapter,
                host.generation,
                clock.as_ref(),
                deadline,
            ),
            _ => Vec::new(),
        };
        reports.push(adapter_report(adapter, status.as_ref(), &catalog));
    }
    let shutdown = handle.shutdown(Duration::from_secs(3));
    if !shutdown.completed_within_deadline {
        eprintln!("dgxtop: warning: adapter hosts did not all stop within 3 s");
    }
    Ok(reports)
}

fn host_specs(adapters: &[&str], program: &std::path::Path) -> Result<Vec<HostSpec>, String> {
    adapters
        .iter()
        .map(|adapter| {
            Ok(HostSpec {
                adapter_id: AdapterId::new(*adapter).map_err(|error| error.to_string())?,
                launch: HostLaunch::ChildProcess {
                    program: PathBuf::from(program),
                    args: vec!["--adapter-host".to_owned(), (*adapter).to_owned()],
                },
            })
        })
        .collect()
}

fn wait_for_host(
    handle: &SupervisorHandle,
    completions: &Receiver<IngestionCompletion>,
    adapter: &str,
    deadline: Instant,
) -> Option<HostStatus> {
    loop {
        let status = handle
            .statuses()
            .into_iter()
            .find(|host| host.adapter_id == adapter);
        let settled = status
            .as_ref()
            .is_some_and(|host| host.state != HostState::Starting);
        if settled || Instant::now() >= deadline {
            return status;
        }
        let _ = completions.recv_timeout(Duration::from_millis(50));
    }
}

fn discover_pages(
    handle: &SupervisorHandle,
    completions: &Receiver<IngestionCompletion>,
    adapter: &str,
    generation: u32,
    clock: &dyn TimeProvider,
    deadline: Instant,
) -> Vec<CatalogDelta> {
    let mut pages = Vec::new();
    let mut page = 0;
    while Instant::now() < deadline {
        let Ok(adapter_id) = AdapterId::new(adapter) else {
            break;
        };
        let request = DiscoverRequest {
            request_id: Uuid::new_v4(),
            adapter_id,
            host_generation: generation,
            catalog_revision: 1,
            config_revision: 1,
            deadline_boot_ns: clock.now_boot_ns() + 2_000_000_000,
            previous_generation: 0,
            page,
        };
        let request_id = request.request_id;
        if handle.try_dispatch(AdapterJob::Discover(request)).is_err() {
            std::thread::sleep(Duration::from_millis(20));
            continue;
        }
        let Some(delta) = await_catalog(completions, request_id, deadline) else {
            break;
        };
        let next = delta.next_page.filter(|_| !delta.complete);
        pages.push(delta);
        match next {
            Some(next_page) => page = next_page,
            None => break,
        }
    }
    pages
}

fn await_catalog(
    completions: &Receiver<IngestionCompletion>,
    request_id: Uuid,
    deadline: Instant,
) -> Option<CatalogDelta> {
    while Instant::now() < deadline {
        if let Ok(IngestionCompletion::Catalog {
            request_id: id,
            result,
            ..
        }) = completions.recv_timeout(Duration::from_millis(100))
            && id == request_id
        {
            return result.ok();
        }
    }
    None
}

fn adapter_report(
    adapter: &str,
    status: Option<&HostStatus>,
    catalog: &[CatalogDelta],
) -> AdapterReport {
    let mut capabilities: BTreeMap<String, usize> = BTreeMap::new();
    let mut issues = Vec::new();
    for capability in catalog.iter().flat_map(|page| &page.capabilities) {
        let support = format!("{:?}", capability.support).to_lowercase();
        *capabilities.entry(support).or_insert(0) += 1;
        if let Some(reason) = &capability.reason
            && issues.len() < 32
        {
            issues.push(format!("{}: {reason}", capability.capability_id));
        }
    }
    AdapterReport {
        adapter_id: adapter.to_owned(),
        host_state: status
            .map_or("not started", |host| host.state.label())
            .to_owned(),
        host_detail: status.and_then(|host| host.detail.clone()),
        availability: catalog
            .iter()
            .find_map(|page| page.availability)
            .map(|availability| format!("{availability:?}").to_lowercase()),
        entities: catalog.iter().map(|page| page.entities.len()).sum(),
        sources: catalog.iter().map(|page| page.sources.len()).sum(),
        capabilities,
        capability_issues: issues,
        error_code: catalog
            .iter()
            .rev()
            .find_map(|page| page.error_code.clone()),
    }
}

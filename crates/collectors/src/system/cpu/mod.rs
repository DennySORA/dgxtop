//! CPU collector: utilization breakdown from `/proc/stat` tick counters,
//! frequency/load/task pass-through, arbitrated temperatures (package, core,
//! cluster and SoC are distinct keys; GB10's package from its clusters),
//! CPU package/platform power from energy counters, hwmon power channels
//! and the GB10 cluster formula, and the host's total power: the CPU
//! packages' and the GPUs' together (D-106).

mod power;
mod temperature;
mod total_power;
mod utilization;

use dgxtop_core::canonical::{CollectionBatch, CollectionPlan};
use dgxtop_core::identity::{AdapterId, CollectorId, IdentityError};
use dgxtop_core::metric::{Domain, MetricDescriptor, ids};
use dgxtop_core::naming;
use dgxtop_core::ports::Collector;

use crate::shared::context::{self, CollectorSpec, Route, Session};
use crate::shared::formulas::{self, Selections};

/// Canonical CPU/SoC metrics published straight through arbitration.
const PASS_THROUGH: [&str; 11] = [
    ids::CPU_FREQUENCY_CURRENT,
    ids::CPU_FREQUENCY_MAXIMUM,
    ids::CPU_TEMPERATURE_PACKAGE,
    ids::CPU_TEMPERATURE_CORE,
    ids::CPU_TEMPERATURE_CLUSTER,
    ids::SOC_TEMPERATURE,
    ids::CPU_LOAD_1M,
    ids::CPU_LOAD_5M,
    ids::CPU_LOAD_15M,
    ids::CPU_TASKS_RUNNING,
    ids::CPU_TASKS_TOTAL,
];

/// Metrics only this collector derives; adapters never provide them.
const DERIVED_ONLY: [&str; 6] = [
    ids::CPU_UTILIZATION_TOTAL,
    ids::CPU_UTILIZATION_USER,
    ids::CPU_UTILIZATION_SYSTEM,
    ids::CPU_UTILIZATION_IOWAIT,
    ids::CPU_UTILIZATION_STEAL,
    ids::HOST_POWER_TOTAL,
];

fn accepts(metric: &MetricDescriptor) -> bool {
    matches!(metric.domain, Domain::Cpu | Domain::Soc) && !DERIVED_ONLY.contains(&metric.id)
}

/// Energy counters are consumed once per reading; their derived power
/// candidates are retained instead (see `power`).
fn retains(_route: &Route<'_>, metric: &MetricDescriptor) -> bool {
    metric.id != ids::CPU_ENERGY_PACKAGE && metric.id != ids::CPU_ENERGY_PLATFORM
}

static SPEC: CollectorSpec = CollectorSpec {
    id: naming::collectors::CPU,
    accepts,
    retains,
    leaves: context::leaves_none,
};

/// Host CPU collector.
#[derive(Debug, Clone)]
pub struct CpuCollector {
    id: CollectorId,
}

impl CpuCollector {
    pub fn new() -> Result<Self, IdentityError> {
        Ok(Self {
            id: CollectorId::new(naming::collectors::CPU)?,
        })
    }
}

impl Collector for CpuCollector {
    fn id(&self) -> &CollectorId {
        &self.id
    }

    fn consumes(&self, adapter_id: &AdapterId, read_group: &str) -> bool {
        crate::shared::routes_to(adapter_id, read_group, SPEC.id)
    }

    fn committed_metrics(&self) -> &'static [&'static str] {
        &total_power::COMMITTED_METRICS
    }

    fn collect(&self, plan: &CollectionPlan) -> CollectionBatch {
        context::run(plan, &SPEC, collect_cpu)
    }
}

fn collect_cpu(session: &mut Session<'_>) {
    utilization::publish(session);
    power::publish(session);
    total_power::publish(session);
    let mut selections = Selections::new();
    formulas::publish_pending(
        session,
        |metric| PASS_THROUGH.contains(&metric),
        &mut selections,
    );
    temperature::publish(session, &mut selections);
}

#[cfg(test)]
mod tests;

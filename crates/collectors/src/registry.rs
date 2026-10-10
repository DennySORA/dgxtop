//! Compile-time registry of the six domain collectors (no dynamic loading).

use std::sync::Arc;

use dgxtop_core::naming::collectors;
use dgxtop_core::ports::Collector;

use crate::{
    CpuCollector, DiskCollector, GpuCollector, MemoryCollector, NetworkCollector, ProcessCollector,
};

/// The six collectors, in `naming::collectors::ALL` order.
pub fn default_collectors() -> Vec<Arc<dyn Collector>> {
    collectors::ALL
        .iter()
        .filter_map(|id| collector_for(id))
        .collect()
}

/// Collector registered under `id` (`cpu`, `gpu`, `memory`, `disk`,
/// `network`, `process`), `None` for unknown ids.
pub fn collector_for(id: &str) -> Option<Arc<dyn Collector>> {
    let collector: Arc<dyn Collector> = match id {
        collectors::CPU => Arc::new(CpuCollector::new().ok()?),
        collectors::GPU => Arc::new(GpuCollector::new().ok()?),
        collectors::MEMORY => Arc::new(MemoryCollector::new().ok()?),
        collectors::DISK => Arc::new(DiskCollector::new().ok()?),
        collectors::NETWORK => Arc::new(NetworkCollector::new().ok()?),
        collectors::PROCESS => Arc::new(ProcessCollector::new().ok()?),
        _ => return None,
    };
    Some(collector)
}

#[cfg(test)]
mod tests {
    use super::*;
    use dgxtop_core::identity::AdapterId;
    use dgxtop_core::naming::{READ_GROUPS, adapters, replay_group};

    fn adapter(name: &str) -> AdapterId {
        AdapterId::new(name).unwrap()
    }

    #[test]
    fn registry_holds_the_six_collectors_by_id() {
        let all = default_collectors();
        let ids: Vec<&str> = all
            .iter()
            .map(|collector| collector.id().as_str())
            .collect();
        assert_eq!(ids, collectors::ALL.to_vec());
        for id in collectors::ALL {
            assert_eq!(collector_for(id).unwrap().id().as_str(), id);
        }
        assert!(collector_for("ui").is_none());
    }

    #[test]
    fn consumption_follows_the_naming_routing_table() {
        for collector in default_collectors() {
            for spec in READ_GROUPS {
                let expected = spec.collectors.contains(&collector.id().as_str());
                assert_eq!(
                    collector.consumes(&adapter(spec.adapter), spec.group),
                    expected,
                    "{} {}/{}",
                    collector.id(),
                    spec.adapter,
                    spec.group
                );
                let mirrored = replay_group(spec.adapter, spec.group);
                assert_eq!(
                    collector.consumes(&adapter(adapters::REPLAY), &mirrored),
                    expected,
                    "replay mirror of {}/{}",
                    spec.adapter,
                    spec.group
                );
            }
        }
        let process = collector_for(collectors::PROCESS).unwrap();
        let gpu = collector_for(collectors::GPU).unwrap();
        assert!(process.consumes(&adapter(adapters::NVML), "process"));
        assert!(gpu.consumes(&adapter(adapters::NVML), "process"));
        assert!(process.consumes(&adapter(adapters::PROCFS), "process:3"));
        assert!(!process.consumes(&adapter(adapters::PROCFS), "cpu"));
    }
}

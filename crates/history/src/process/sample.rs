//! What a sample of the runs takes beside its process table (D-76, D-99),
//! derived here from the committed snapshot and the catalog the Manager
//! hands over unchanged: the labels of the GPUs the table's processes use,
//! the host's clock ticks per second, which place a run's start, and what
//! the host receives and sends over its interfaces other than the loopback.

use std::collections::{BTreeMap, BTreeSet};

use dgxtop_core::canonical::{CatalogSnapshot, ImmutableSnapshot, ProcessTable, SnapshotEntry};
use dgxtop_core::identity::EntityId;
use dgxtop_core::metric::{Scalar, ids};
use dgxtop_core::naming::entity;
use dgxtop_core::process_model::ProcessTableSample;

/// The interfaces' entity prefix.
const INTERFACE_PREFIX: &str = "netif:";
/// The loopback's name, whose traffic never leaves the host.
const LOOPBACK_NAME: &str = "lo";

/// The snapshot's and catalog's part of a sample.
#[derive(Debug, Clone, Default, PartialEq)]
pub(super) struct TableContext {
    pub(super) clk_tck: Option<u64>,
    pub(super) gpu_labels: BTreeMap<EntityId, String>,
    /// Received and sent, bytes per second; `None` where no interface
    /// reports.
    pub(super) host_network: [Option<f64>; 2],
}

impl TableContext {
    pub(super) fn of(sample: &ProcessTableSample) -> Self {
        Self {
            clk_tck: sample.catalog.clk_tck(),
            gpu_labels: gpu_labels(sample.table(), &sample.catalog),
            host_network: [ids::NET_RX_BPS, ids::NET_TX_BPS]
                .map(|metric| host_total(&sample.snapshot, &sample.catalog, metric)),
        }
    }
}

/// The label of each GPU a process of `table` uses.
fn gpu_labels(table: &ProcessTable, catalog: &CatalogSnapshot) -> BTreeMap<EntityId, String> {
    table
        .rows
        .iter()
        .flat_map(|row| row.gpu.iter().map(|link| &link.gpu_entity_id))
        .collect::<BTreeSet<_>>()
        .into_iter()
        .map(|gpu| (gpu.clone(), catalog.gpu_label(gpu)))
        .collect()
}

/// `metric` summed over the host's interfaces other than the loopback that
/// hold a current value; `None` when none does.
fn host_total(
    snapshot: &ImmutableSnapshot,
    catalog: &CatalogSnapshot,
    metric: &str,
) -> Option<f64> {
    let mut by_interface: BTreeMap<&EntityId, &SnapshotEntry> = BTreeMap::new();
    for domain in snapshot.domains.values() {
        for (key, entry) in &domain.samples {
            if key.metric_id.as_str() != metric
                || !key.entity_id.as_str().starts_with(INTERFACE_PREFIX)
            {
                continue;
            }
            // An interface's own scope first, as its panel reads it.
            let preferred = key.scope.as_str() == entity::SCOPE_INTERFACE;
            by_interface
                .entry(&key.entity_id)
                .and_modify(|kept| {
                    if preferred {
                        *kept = entry;
                    }
                })
                .or_insert(entry);
        }
    }
    by_interface
        .into_iter()
        .filter(|(interface, _)| catalog.interface_name(interface) != LOOPBACK_NAME)
        .filter_map(|(_, entry)| current_value(entry, snapshot.committed_boot_ns))
        .fold(None, |sum: Option<f64>, value| {
            Some(sum.unwrap_or(0.0) + value)
        })
}

/// The entry's value while it has one and has not gone stale by `now_ns`.
fn current_value(entry: &SnapshotEntry, now_ns: u64) -> Option<f64> {
    let sample = &entry.sample;
    let expired = entry
        .stale_after_boot_ns
        .is_some_and(|deadline| now_ns >= deadline);
    (sample.selection_state.has_value() && !expired)
        .then_some(sample.value)
        .flatten()
        .map(Scalar::to_f64_lossy)
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;
    use dgxtop_core::naming::attributes;
    use dgxtop_core::observation::EntityKind;
    use dgxtop_core::testing::{
        catalog_of, entity_record, gpu_process, metric_key, process_table, snapshot_entry,
        snapshot_of,
    };

    const NOW: u64 = 50_000;

    fn interface(id: &str, name: &str) -> dgxtop_core::observation::EntityRecord {
        entity_record(
            id,
            EntityKind::NetworkInterface,
            name,
            &[(attributes::IFNAME, name)],
        )
    }

    /// A host with two interfaces and the loopback, a GB10 with its NVML
    /// index and a GPU the catalog does not know, its clock at 100 ticks.
    fn sample(rows: Vec<dgxtop_core::canonical::ProcessRow>) -> ProcessTableSample {
        let catalog = catalog_of(vec![
            entity_record(
                entity::HOST,
                EntityKind::Host,
                "host",
                &[(attributes::CLK_TCK, "100")],
            ),
            entity_record(
                "gpu:GPU-aaaa1111",
                EntityKind::Gpu,
                "NVIDIA GB10",
                &[(attributes::NVML_INDEX, "0")],
            ),
            interface("netif:ns:1", "lo"),
            interface("netif:ns:2", "enp1s0"),
            interface("netif:ns:3", "wlan0"),
        ]);
        let rate = |id: &str, metric: &str, value: Option<f64>| {
            (
                "network",
                snapshot_entry(&metric_key(id, metric, "interface"), value, NOW),
            )
        };
        let mut snapshot = snapshot_of(
            2,
            vec![
                rate("netif:ns:1", ids::NET_RX_BPS, Some(9e9)),
                rate("netif:ns:2", ids::NET_RX_BPS, Some(1000.0)),
                rate("netif:ns:3", ids::NET_RX_BPS, Some(24.0)),
                rate("netif:ns:2", ids::NET_TX_BPS, Some(500.0)),
                rate("netif:ns:3", ids::NET_TX_BPS, None),
            ],
        );
        snapshot.committed_boot_ns = NOW;
        snapshot.process_table = Arc::new(process_table(2, NOW, rows));
        ProcessTableSample {
            snapshot: Arc::new(snapshot),
            catalog: Arc::new(catalog),
            process_period_ms: 1_000,
        }
    }

    /// The GPUs a process uses are labelled by NVML index, else by UUID;
    /// the clock ticks come from the host entity.
    #[test]
    fn labels_and_clock_ticks_come_from_the_catalog() {
        let context = TableContext::of(&sample(vec![
            gpu_process(10, "train", &[("gpu:GPU-aaaa1111", Some(1))]),
            gpu_process(11, "render", &[("gpu:GPU-cccc3333-x", None)]),
        ]));
        let labels: Vec<(&str, &str)> = context
            .gpu_labels
            .iter()
            .map(|(gpu, label)| (gpu.as_str(), label.as_str()))
            .collect();
        assert_eq!(
            labels,
            vec![
                ("gpu:GPU-aaaa1111", "GPU 0"),
                ("gpu:GPU-cccc3333-x", "GPU cccc3333")
            ]
        );
        assert_eq!(context.clk_tck, Some(100));
        let mut without = sample(Vec::new());
        Arc::make_mut(&mut without.catalog).entities.clear();
        let bare = TableContext::of(&without);
        assert_eq!(bare.clk_tck, None, "never defaulted");
        assert!(bare.gpu_labels.is_empty());
    }

    /// The host's traffic is the sum over its interfaces but the loopback;
    /// an interface without a value adds nothing, and none reporting is
    /// unknown, never zero.
    #[test]
    fn the_host_network_sums_the_interfaces_but_the_loopback() {
        let context = TableContext::of(&sample(Vec::new()));
        assert_eq!(context.host_network, [Some(1024.0), Some(500.0)]);
        let mut quiet = sample(Vec::new());
        Arc::make_mut(&mut quiet.snapshot).domains.clear();
        assert_eq!(TableContext::of(&quiet).host_network, [None, None]);
    }

    /// A rate past its stale deadline at the commit no longer counts.
    #[test]
    fn a_stale_rate_is_left_out() {
        let mut stale = sample(Vec::new());
        let snapshot = Arc::make_mut(&mut stale.snapshot);
        for domain in snapshot.domains.values_mut() {
            let domain = Arc::make_mut(domain);
            for entry in domain.samples.values_mut() {
                if entry.sample.key.entity_id.as_str() == "netif:ns:3" {
                    Arc::make_mut(entry).stale_after_boot_ns = Some(NOW);
                }
            }
        }
        assert_eq!(
            TableContext::of(&stale).host_network,
            [Some(1000.0), Some(500.0)]
        );
    }
}

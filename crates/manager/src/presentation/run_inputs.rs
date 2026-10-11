//! What a process history sample takes from a committed snapshot beside
//! the process table (D-76): the GPUs' labels, the clock ticks per second
//! that place a run's start, and the host's network rates.

use std::collections::BTreeMap;

use dgxtop_core::canonical::{CatalogSnapshot, ImmutableSnapshot};
use dgxtop_core::identity::EntityId;
use dgxtop_core::naming::{attributes, entity};
use dgxtop_core::view::InterfaceView;

use super::TrendBook;
use super::gpu_directory::GpuDirectory;
use super::network::ranked_interfaces;
use super::reader::MetricReader;

/// The snapshot's part of a process history sample.
#[derive(Debug, Clone, Default, PartialEq)]
pub(crate) struct RunInputs {
    pub(crate) gpu_labels: BTreeMap<EntityId, String>,
    pub(crate) clk_tck: Option<u64>,
    /// Received and sent, bytes per second, over the host's interfaces
    /// other than the loopback; `None` where none reports.
    pub(crate) host_network: [Option<f64>; 2],
}

pub(crate) fn run_inputs(
    snapshot: &ImmutableSnapshot,
    catalog: &CatalogSnapshot,
    now_boot_ns: u64,
) -> RunInputs {
    let trends = TrendBook::new(0);
    let reader = MetricReader::new(snapshot, catalog, &trends, now_boot_ns);
    let directory = GpuDirectory::new(catalog, reader.index());
    let gpu_labels = directory
        .gpus()
        .iter()
        .map(|gpu| (gpu.entity_id.clone(), gpu.label.clone()))
        .collect();
    let interfaces = ranked_interfaces(&reader);
    let total = |rate: fn(&InterfaceView) -> Option<f64>| {
        interfaces
            .iter()
            .filter_map(rate)
            .fold(None, |sum: Option<f64>, value| {
                Some(sum.unwrap_or(0.0) + value)
            })
    };
    RunInputs {
        gpu_labels,
        clk_tck: clk_tck(catalog),
        host_network: [
            total(|view| view.rx_bps.value_f64()),
            total(|view| view.tx_bps.value_f64()),
        ],
    }
}

/// `clk_tck` the catalog's host entity publishes; never defaulted.
fn clk_tck(catalog: &CatalogSnapshot) -> Option<u64> {
    let host = EntityId::new(entity::HOST).ok()?;
    catalog
        .entity(&host)?
        .attribute(attributes::CLK_TCK)?
        .trim()
        .parse::<u64>()
        .ok()
        .filter(|ticks| *ticks > 0)
}

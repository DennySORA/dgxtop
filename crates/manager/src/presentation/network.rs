//! Network module projection: interfaces by activity, loopback hidden, at
//! most `viewer.network_max_visible` rows (a layout limit, not a collection
//! cap); the busiest interface's rate trends feed the panel chart.

use dgxtop_core::canonical::CatalogSnapshot;
use dgxtop_core::identity::EntityId;
use dgxtop_core::metric::ids;
use dgxtop_core::naming::{attributes, entity};
use dgxtop_core::view::{InterfaceView, NetworkView, Trend};

use super::labels::label_text;
use super::ordering::{descending_present, sum_present};
use super::reader::MetricReader;

const INTERFACE_PREFIX: &str = "netif:";
const LOOPBACK_NAME: &str = "lo";
const INTERFACE_METRICS: [&str; 10] = [
    ids::NET_RX_BPS,
    ids::NET_TX_BPS,
    ids::NET_RX_PACKETS,
    ids::NET_TX_PACKETS,
    ids::NET_RX_ERRORS,
    ids::NET_TX_ERRORS,
    ids::NET_RX_DROPS,
    ids::NET_TX_DROPS,
    ids::NET_RX_BYTES,
    ids::NET_TX_BYTES,
];

pub(crate) fn build_network_view(
    reader: &MetricReader<'_>,
    max_visible: u32,
    trend_width: usize,
) -> NetworkView {
    let visible = usize::try_from(max_visible.max(1)).unwrap_or(usize::MAX);
    let interfaces: Vec<InterfaceView> = ranked_interfaces(reader)
        .into_iter()
        .take(visible)
        .collect();
    let trend = |metric: &str| {
        interfaces.first().map_or_else(Trend::default, |busiest| {
            reader.trend_preferring(
                metric,
                busiest.entity_id.as_str(),
                entity::SCOPE_INTERFACE,
                trend_width,
            )
        })
    };
    NetworkView {
        rx_trend: trend(ids::NET_RX_BPS),
        tx_trend: trend(ids::NET_TX_BPS),
        interfaces,
        max_visible,
    }
}

/// Interfaces other than the loopback, by activity (missing last), then
/// name; the first is the busiest.
pub(crate) fn ranked_interfaces(reader: &MetricReader<'_>) -> Vec<InterfaceView> {
    let mut ranked: Vec<(Option<f64>, InterfaceView)> = reader
        .index()
        .entities_reporting(&INTERFACE_METRICS, INTERFACE_PREFIX)
        .into_iter()
        .map(|entity_id| interface_view(reader, entity_id))
        .filter(|view| view.name != LOOPBACK_NAME)
        .map(|view| {
            let activity = sum_present([view.rx_bps.value_f64(), view.tx_bps.value_f64()]);
            (activity, view)
        })
        .collect();
    ranked.sort_by(|(left_rate, left), (right_rate, right)| {
        descending_present(*left_rate, *right_rate)
            .then_with(|| left.name.cmp(&right.name))
            .then_with(|| left.entity_id.cmp(&right.entity_id))
    });
    ranked.into_iter().map(|(_, view)| view).collect()
}

fn interface_view(reader: &MetricReader<'_>, entity_id: &EntityId) -> InterfaceView {
    let id = entity_id.as_str();
    let cell = |metric: &str| reader.cell_preferring(metric, id, entity::SCOPE_INTERFACE);
    let record = reader.catalog().entity(entity_id);
    InterfaceView {
        entity_id: entity_id.clone(),
        name: interface_name(reader.catalog(), entity_id),
        state: record
            .and_then(|record| record.attribute(attributes::OPERSTATE))
            .map(label_text),
        rx_bps: cell(ids::NET_RX_BPS),
        tx_bps: cell(ids::NET_TX_BPS),
        rx_packets: cell(ids::NET_RX_PACKETS),
        tx_packets: cell(ids::NET_TX_PACKETS),
        rx_errors: cell(ids::NET_RX_ERRORS),
        tx_errors: cell(ids::NET_TX_ERRORS),
        rx_drops: cell(ids::NET_RX_DROPS),
        tx_drops: cell(ids::NET_TX_DROPS),
    }
}

/// Interfaces are identified by namespace + ifindex; the name is display only.
fn interface_name(catalog: &CatalogSnapshot, entity_id: &EntityId) -> String {
    label_text(catalog.interface_name(entity_id))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::presentation::testkit::{Fixture, NOW, entity_record, sample};
    use dgxtop_core::observation::EntityKind;
    use dgxtop_core::testing::metric_key;

    const TREND_WIDTH: usize = 60;

    fn interface(fixture: &mut Fixture, id: &str, name: &str, rx: Option<f64>) {
        fixture.add_entity(entity_record(
            id,
            EntityKind::NetworkInterface,
            "",
            &[(attributes::IFNAME, name), (attributes::OPERSTATE, "up")],
        ));
        let row = sample("network.rx.bytes_per_second", id, "interface");
        fixture.add(
            "network",
            match rx {
                Some(value) => row.f64(value),
                None => row.unavailable().reason("WARMUP"),
            },
        );
    }

    #[test]
    fn interfaces_sort_by_activity_hide_loopback_and_respect_max_visible() {
        let mut fixture = Fixture::new();
        interface(&mut fixture, "netif:ns:1", "lo", Some(9e9));
        interface(&mut fixture, "netif:ns:2", "eth0", Some(10.0));
        interface(&mut fixture, "netif:ns:3", "ib0", Some(5e9));
        interface(&mut fixture, "netif:ns:4", "eth1", None);
        interface(&mut fixture, "netif:ns:5", "docker0", Some(0.0));
        let reader = MetricReader::new(&fixture.snapshot, &fixture.catalog, NOW)
            .with_trends(&fixture.trends);
        let view = build_network_view(&reader, 3, TREND_WIDTH);
        let names: Vec<&str> = view
            .interfaces
            .iter()
            .map(|row| row.name.as_str())
            .collect();
        assert_eq!(names, vec!["ib0", "eth0", "docker0"]);
        assert_eq!(view.max_visible, 3);
        assert_eq!(view.interfaces[0].state.as_deref(), Some("up"));
        let all = build_network_view(&reader, 20, TREND_WIDTH);
        assert_eq!(all.interfaces.len(), 4);
        assert_eq!(all.interfaces[3].name, "eth1");
        assert_eq!(all.interfaces[3].rx_bps.value, None);
    }

    #[test]
    fn the_busiest_interface_carries_its_receive_and_transmit_trends() {
        let mut fixture = Fixture::new();
        interface(&mut fixture, "netif:ns:1", "lo", Some(9e9));
        interface(&mut fixture, "netif:ns:2", "eth0", Some(10.0));
        interface(&mut fixture, "netif:ns:3", "ib0", Some(5e9));
        let rx = metric_key("netif:ns:3", "network.rx.bytes_per_second", "interface");
        let tx = metric_key("netif:ns:3", "network.tx.bytes_per_second", "interface");
        let loopback = metric_key("netif:ns:1", "network.rx.bytes_per_second", "interface");
        fixture.trends.push(&rx, Some(4e9));
        fixture.trends.push(&rx, Some(5e9));
        fixture.trends.push(&tx, Some(2.0));
        fixture.trends.push(&loopback, Some(9e9));
        let reader = MetricReader::new(&fixture.snapshot, &fixture.catalog, NOW)
            .with_trends(&fixture.trends);
        let view = build_network_view(&reader, 3, TREND_WIDTH);
        assert_eq!(view.interfaces[0].name, "ib0");
        assert_eq!(view.rx_trend.points, vec![Some(4e9), Some(5e9)]);
        assert_eq!(view.tx_trend.points, vec![Some(2.0)]);
    }
}

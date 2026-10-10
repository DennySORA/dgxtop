//! CPU module projection: host aggregate, logical cores, package and SoC
//! temperatures, power, load and tasks. Package, core and SoC temperatures
//! are different metrics and are never substituted for one another.

use std::collections::{BTreeMap, BTreeSet};

use dgxtop_core::canonical::{CatalogSnapshot, SnapshotEntry};
use dgxtop_core::identity::EntityId;
use dgxtop_core::metric::ids;
use dgxtop_core::naming::{attributes, entity};
use dgxtop_core::view::{CoreView, CpuView, LabeledCell, MetricCell};

use super::labels::{humanize_scope, label_text};
use super::reader::MetricReader;

/// Logical CPUs kept per projection (the largest reference hosts have 512).
pub(crate) const MAX_CORES: usize = 512;
/// Rows kept per labeled temperature/power list.
const MAX_LABELED_ROWS: usize = 64;
/// Logical CPU entity spelling (`naming::entity::cpu_core`).
const CORE_ENTITY_PREFIX: &str = "cpu.core.";
/// Physical core id of a logical CPU; proposed for `naming::attributes`.
const CORE_ID_ATTRIBUTE: &str = "core_id";

pub(crate) fn build_cpu_view(
    reader: &MetricReader<'_>,
    fallback_model: Option<&str>,
    trend_width: usize,
) -> CpuView {
    let aggregate = |metric: &str| reader.cell(metric, entity::CPU_AGGREGATE, entity::SCOPE_HOST);
    let host = |metric: &str| reader.cell(metric, entity::HOST, entity::SCOPE_HOST);
    CpuView {
        model: cpu_model(reader.catalog(), fallback_model),
        total: aggregate(ids::CPU_UTILIZATION_TOTAL),
        user: aggregate(ids::CPU_UTILIZATION_USER),
        system: aggregate(ids::CPU_UTILIZATION_SYSTEM),
        iowait: aggregate(ids::CPU_UTILIZATION_IOWAIT),
        steal: aggregate(ids::CPU_UTILIZATION_STEAL),
        cores: core_views(reader),
        frequency_max: frequency_max(reader),
        package_temperatures: labeled_rows(reader, ids::CPU_TEMPERATURE_PACKAGE),
        soc_temperatures: labeled_rows(reader, ids::SOC_TEMPERATURE),
        power: power_rows(reader),
        load: [
            host(ids::CPU_LOAD_1M),
            host(ids::CPU_LOAD_5M),
            host(ids::CPU_LOAD_15M),
        ],
        tasks_running: host(ids::CPU_TASKS_RUNNING),
        tasks_total: host(ids::CPU_TASKS_TOTAL),
        utilization_trend: reader.trend_preferring(
            ids::CPU_UTILIZATION_TOTAL,
            entity::CPU_AGGREGATE,
            entity::SCOPE_HOST,
            trend_width,
        ),
    }
}

/// Catalog host (or CPU aggregate) attribute first; the static host facts
/// only when discovery has not reported a model.
fn cpu_model(catalog: &CatalogSnapshot, fallback: Option<&str>) -> Option<String> {
    [entity::HOST, entity::CPU_AGGREGATE]
        .iter()
        .filter_map(|id| EntityId::new(*id).ok())
        .find_map(|id| {
            catalog
                .entity(&id)
                .and_then(|record| record.attribute(attributes::CPU_MODEL))
                .map(str::to_owned)
        })
        .or_else(|| fallback.map(str::to_owned))
        .map(|model| label_text(&model))
        .filter(|model| !model.is_empty())
}

fn parse_core_id(entity_id: &str) -> Option<u32> {
    entity_id.strip_prefix(CORE_ENTITY_PREFIX)?.parse().ok()
}

/// Logical CPUs from samples and live catalog entries, numerically ordered
/// (`cpu.core.2` before `cpu.core.10`), never by read order.
fn core_ids(reader: &MetricReader<'_>) -> BTreeSet<u32> {
    let from_samples = reader
        .index()
        .entities_reporting(
            &[ids::CPU_UTILIZATION_TOTAL, ids::CPU_FREQUENCY_CURRENT],
            CORE_ENTITY_PREFIX,
        )
        .into_iter()
        .filter_map(|entity_id| parse_core_id(entity_id.as_str()));
    let from_catalog = reader
        .catalog()
        .entities
        .values()
        .filter(|record| record.removed_boot_ns.is_none())
        .filter_map(|record| parse_core_id(record.entity_id.as_str()));
    from_samples.chain(from_catalog).collect()
}

fn core_views(reader: &MetricReader<'_>) -> Vec<CoreView> {
    core_ids(reader)
        .into_iter()
        .take(MAX_CORES)
        .filter_map(|core| {
            let entity_id = entity::cpu_core(core).ok()?;
            let scope = entity::scope_core(core).ok()?;
            Some(core_view(reader, core, entity_id.as_str(), scope.as_str()))
        })
        .collect()
}

fn core_view(reader: &MetricReader<'_>, core: u32, entity_id: &str, scope: &str) -> CoreView {
    CoreView {
        core_id: core.to_string(),
        utilization: reader.cell(ids::CPU_UTILIZATION_TOTAL, entity_id, scope),
        frequency: reader.cell(ids::CPU_FREQUENCY_CURRENT, entity_id, scope),
        temperature: core_temperature(reader, entity_id, scope),
    }
}

/// A logical CPU shows a core temperature only when it provably matches:
/// reported on the logical CPU itself, or mapped through the catalog
/// topology (`physical_package` + `core_id`) to a per-physical-core sensor.
fn core_temperature(reader: &MetricReader<'_>, entity_id: &str, scope: &str) -> MetricCell {
    let entry = reader
        .index()
        .find(ids::CPU_TEMPERATURE_CORE, entity_id, scope)
        .or_else(|| physical_core_temperature(reader, entity_id));
    reader.cell_or_missing(entry, ids::CPU_TEMPERATURE_CORE)
}

fn physical_core_temperature<'a>(
    reader: &MetricReader<'a>,
    logical_entity: &str,
) -> Option<&'a SnapshotEntry> {
    let record = reader
        .catalog()
        .entity(&EntityId::new(logical_entity).ok()?)?;
    let package: u32 = record
        .attribute(attributes::PHYSICAL_PACKAGE)?
        .trim()
        .parse()
        .ok()?;
    let core: u32 = record.attribute(CORE_ID_ATTRIBUTE)?.trim().parse().ok()?;
    let package_entity = entity::cpu_package(package).ok()?;
    let core_scope = entity::scope_physical_core(core).ok()?;
    reader.index().get(
        ids::CPU_TEMPERATURE_CORE,
        package_entity.as_str(),
        core_scope.as_str(),
    )
}

/// Host-level maximum when reported; otherwise the highest per-core maximum
/// (an existing sample, not a computed value), else the first core's state.
fn frequency_max(reader: &MetricReader<'_>) -> MetricCell {
    let index = reader.index();
    if let Some(entry) = index.get(
        ids::CPU_FREQUENCY_MAXIMUM,
        entity::CPU_AGGREGATE,
        entity::SCOPE_HOST,
    ) {
        return reader.cell_of(entry);
    }
    let entries = index.entries_of_metric(ids::CPU_FREQUENCY_MAXIMUM);
    let highest = entries
        .iter()
        .filter_map(|entry| MetricReader::current_value(entry).map(|value| (value, *entry)))
        .max_by(|(left, _), (right, _)| left.total_cmp(right))
        .map(|(_, entry)| entry);
    reader.cell_or_missing(
        highest.or_else(|| entries.first().copied()),
        ids::CPU_FREQUENCY_MAXIMUM,
    )
}

/// Package power first, then platform, then CPU cluster components.
fn power_rows(reader: &MetricReader<'_>) -> Vec<LabeledCell> {
    [
        ids::CPU_POWER_PACKAGE,
        ids::CPU_POWER_PLATFORM,
        ids::CPU_POWER_CLUSTER,
    ]
    .iter()
    .flat_map(|metric| labeled_rows(reader, metric))
    .take(MAX_LABELED_ROWS)
    .collect()
}

/// One labeled row per reported scope ("package 0", "SoC", "platform"),
/// ordered by scope kind then numeric id; duplicates name their entity.
fn labeled_rows(reader: &MetricReader<'_>, metric: &str) -> Vec<LabeledCell> {
    let mut entries = reader.index().entries_of_metric(metric);
    entries.sort_by_cached_key(|entry| scope_order(entry));
    let mut scope_counts: BTreeMap<&str, usize> = BTreeMap::new();
    for entry in &entries {
        *scope_counts
            .entry(entry.sample.key.scope.as_str())
            .or_default() += 1;
    }
    entries
        .iter()
        .take(MAX_LABELED_ROWS)
        .map(|entry| {
            let key = &entry.sample.key;
            let scope_label = humanize_scope(key.scope.as_str());
            let shared = scope_counts.get(key.scope.as_str()).copied().unwrap_or(0) > 1;
            let label = if shared {
                label_text(&format!("{scope_label} · {}", key.entity_id))
            } else {
                scope_label
            };
            LabeledCell {
                label,
                cell: reader.cell_of(entry),
            }
        })
        .collect()
}

fn scope_order(entry: &SnapshotEntry) -> (String, Option<u64>, String, String) {
    let scope = entry.sample.key.scope.as_str();
    let (kind, number) = match scope.rsplit_once(':') {
        Some((kind, number)) => (kind, number.parse::<u64>().ok()),
        None => (scope, None),
    };
    (
        kind.to_owned(),
        number,
        scope.to_owned(),
        entry.sample.key.entity_id.to_string(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::presentation::testkit::{Fixture, NOW, entity_record, sample};
    use dgxtop_core::observation::EntityKind;

    use dgxtop_core::view::CellState;

    fn build(fixture: &Fixture) -> CpuView {
        let reader = MetricReader::new(&fixture.snapshot, &fixture.catalog, &fixture.trends, NOW);
        build_cpu_view(&reader, fixture.facts.cpu_model.as_deref(), 60)
    }

    #[test]
    fn aggregate_load_tasks_and_true_zero() {
        let mut fixture = Fixture::new();
        fixture.add(
            "cpu",
            sample("cpu.utilization.total", "cpu", "host").f64(37.5),
        );
        fixture.add(
            "cpu",
            sample("cpu.utilization.steal", "cpu", "host").f64(0.0),
        );
        fixture.add("cpu", sample("cpu.load.1m", "host", "host").f64(1.25));
        fixture.add("cpu", sample("cpu.tasks.total", "host", "host").u64(812));
        let view = build(&fixture);
        assert_eq!(view.total.value_f64(), Some(37.5));
        assert_eq!(view.steal.value_f64(), Some(0.0));
        assert_eq!(view.steal.state, CellState::Ok);
        assert_eq!(view.load[0].value_f64(), Some(1.25));
        assert_eq!(view.load[1].state, CellState::Unavailable);
        assert_eq!(view.load[1].value, None);
        assert_eq!(view.tasks_total.value_f64(), Some(812.0));
        assert_eq!(view.iowait.state, CellState::Unavailable);
    }

    #[test]
    fn cores_are_sorted_numerically_with_matching_temperatures_only() {
        let mut fixture = Fixture::new();
        for core in [10_u32, 2, 0] {
            fixture.add(
                "cpu",
                sample(
                    "cpu.utilization.total",
                    &format!("cpu.core.{core}"),
                    &format!("core:{core}"),
                )
                .f64(f64::from(core)),
            );
        }
        fixture.add(
            "cpu",
            sample("cpu.frequency.current", "cpu.core.2", "core:2").f64(2400.0),
        );
        fixture.add(
            "cpu",
            sample("cpu.temperature.core", "cpu.core.2", "core:2").f64(55.0),
        );
        // A per-physical-core sensor without topology must not be guessed onto cpu.core.0.
        fixture.add(
            "cpu",
            sample("cpu.temperature.core", "cpu.package.0", "core:0").f64(70.0),
        );
        let view = build(&fixture);
        let ids: Vec<&str> = view
            .cores
            .iter()
            .map(|core| core.core_id.as_str())
            .collect();
        assert_eq!(ids, vec!["0", "2", "10"]);
        assert_eq!(view.cores[1].frequency.value_f64(), Some(2400.0));
        assert_eq!(view.cores[1].temperature.value_f64(), Some(55.0));
        assert_eq!(view.cores[0].temperature.state, CellState::Unavailable);
        assert_eq!(view.cores[0].temperature.value, None);
        assert_eq!(view.cores[2].frequency.state, CellState::Unavailable);
    }

    #[test]
    fn topology_maps_physical_core_temperature_to_logical_cpu() {
        let mut fixture = Fixture::new();
        fixture.add_entity(entity_record(
            "cpu.core.5",
            EntityKind::CpuCore,
            "cpu5",
            &[(attributes::PHYSICAL_PACKAGE, "1"), ("core_id", "3")],
        ));
        fixture.add(
            "cpu",
            sample("cpu.temperature.core", "cpu.package.1", "core:3").f64(61.0),
        );
        let view = build(&fixture);
        assert_eq!(view.cores.len(), 1);
        assert_eq!(view.cores[0].core_id, "5");
        assert_eq!(view.cores[0].temperature.value_f64(), Some(61.0));
        assert_eq!(view.cores[0].utilization.state, CellState::Unavailable);
    }
}

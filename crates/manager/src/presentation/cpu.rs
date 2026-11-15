//! CPU module projection: host aggregate, logical cores, the current and
//! maximum frequency of each kind of core, package, cluster and SoC
//! temperatures, power, load, tasks and the CPU's stall pressure (D-135). Package, core, cluster and SoC
//! temperatures are different metrics and are never substituted for one
//! another.

use std::collections::{BTreeMap, BTreeSet};

use dgxtop_core::canonical::{CatalogSnapshot, SnapshotEntry};
use dgxtop_core::identity::EntityId;
use dgxtop_core::metric::Scalar;
use dgxtop_core::metric::ids;
use dgxtop_core::naming::{attributes, entity};
use dgxtop_core::pressure::PressureResource;
use dgxtop_core::quality::SampleKind;
use dgxtop_core::view::{CellState, CoreView, CpuView, FrequencyGroup, LabeledCell, MetricCell};

use super::cell::missing_cell;
use super::labels::{humanize_scope, label_text};
use super::pressure::pressure_view;
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
    let cores = core_views(reader);
    let temperatures = temperature_rows(reader);
    CpuView {
        model: cpu_model(reader.catalog(), fallback_model),
        total: aggregate(ids::CPU_UTILIZATION_TOTAL),
        user: aggregate(ids::CPU_UTILIZATION_USER),
        system: aggregate(ids::CPU_UTILIZATION_SYSTEM),
        iowait: aggregate(ids::CPU_UTILIZATION_IOWAIT),
        steal: aggregate(ids::CPU_UTILIZATION_STEAL),
        frequencies: frequency_groups(reader, &cores),
        cores,
        package_temperatures: temperatures.rows,
        hottest_cluster: temperatures.hottest_cluster,
        soc_temperatures: labeled_rows(reader, ids::SOC_TEMPERATURE),
        package_power: labeled_rows(reader, ids::CPU_POWER_PACKAGE),
        power: power_rows(reader),
        load: [
            host(ids::CPU_LOAD_1M),
            host(ids::CPU_LOAD_5M),
            host(ids::CPU_LOAD_15M),
        ],
        tasks_running: host(ids::CPU_TASKS_RUNNING),
        tasks_total: host(ids::CPU_TASKS_TOTAL),
        pressure: pressure_view(
            reader,
            PressureResource::Cpu,
            entity::HOST,
            entity::SCOPE_HOST,
        ),
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
/// The host's logical CPUs the snapshot knows of.
pub(crate) fn logical_cpu_count(reader: &MetricReader<'_>) -> usize {
    core_ids(reader).len()
}

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

/// One group per kind of core: the logical CPUs sharing a maximum
/// frequency, by ascending maximum (hybrid ARM: efficiency then
/// performance cores; x86: usually one group). Cores without a maximum
/// form a last group, kept only when one of them reports a current
/// frequency; without cpufreq there is no group.
fn frequency_groups(reader: &MetricReader<'_>, cores: &[CoreView]) -> Vec<FrequencyGroup> {
    let mut kinds: BTreeMap<(bool, u64), (MetricCell, Vec<&CoreView>)> = BTreeMap::new();
    for core in cores {
        let maximum = core_frequency(reader, core, ids::CPU_FREQUENCY_MAXIMUM);
        let key = frequency_khz(&maximum).map_or((true, 0), |khz| (false, khz));
        kinds
            .entry(key)
            .or_insert_with(|| (maximum, Vec::new()))
            .1
            .push(core);
    }
    kinds
        .into_iter()
        .filter_map(|((unknown_maximum, _), (maximum, members))| {
            let current = mean_frequency(&members);
            (!unknown_maximum || current.value.is_some()).then(|| FrequencyGroup {
                maximum,
                limit: lowest_limit(reader, &members),
                current,
                cores: members.len(),
            })
        })
        .collect()
}

/// The lowest limit the cpufreq policy sets among `cores` (D-136): the cap
/// that binds their kind. Where every core reporting one agrees it is that
/// reading; where they differ it is derived and says of how many cores it
/// is the lowest. With none reporting, the first core's cell says why.
fn lowest_limit(reader: &MetricReader<'_>, cores: &[&CoreView]) -> MetricCell {
    let limits: Vec<MetricCell> = cores
        .iter()
        .map(|core| core_frequency(reader, core, ids::CPU_FREQUENCY_LIMIT))
        .collect();
    let reported: Vec<(u64, &MetricCell)> = limits
        .iter()
        .filter_map(|cell| Some((frequency_khz(cell)?, cell)))
        .collect();
    let Some((lowest_khz, lowest)) = reported.iter().min_by_key(|(khz, _)| *khz) else {
        return limits
            .into_iter()
            .next()
            .unwrap_or_else(|| missing_cell(ids::CPU_FREQUENCY_LIMIT));
    };
    if reported.iter().all(|(khz, _)| khz == lowest_khz) {
        return (*lowest).clone();
    }
    MetricCell {
        kind: SampleKind::Derived,
        source_label: Some(format!("lowest of {} cores", reported.len())),
        conflict: None,
        last_good: None,
        ..(*lowest).clone()
    }
}

/// A cpufreq figure of one logical CPU (its maximum or its limit).
fn core_frequency(reader: &MetricReader<'_>, core: &CoreView, metric: &str) -> MetricCell {
    let Some(cpu) = core.core_id.parse::<u32>().ok() else {
        return missing_cell(metric);
    };
    match (entity::cpu_core(cpu), entity::scope_core(cpu)) {
        (Ok(entity_id), Ok(scope)) => reader.cell(metric, entity_id.as_str(), scope.as_str()),
        _ => missing_cell(metric),
    }
}

/// A frequency in whole kHz (the unit cpufreq reports), so cores of one
/// kind group exactly and limits compare exactly; `None` without a value.
fn frequency_khz(frequency: &MetricCell) -> Option<u64> {
    let mhz = frequency
        .value_f64()
        .filter(|_| frequency.state.has_value())?;
    // Finite, non-negative and far below u64::MAX for any real clock.
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let khz = (mhz * 1000.0).round().max(0.0) as u64;
    Some(khz)
}

/// The mean current frequency of the cores that report one (D-67), a
/// derived value: the oldest and least certain of them set its age and
/// quality. An idle core reports no current frequency; with none reporting,
/// the first core's cell says why.
fn mean_frequency(cores: &[&CoreView]) -> MetricCell {
    let current: Vec<&MetricCell> = cores
        .iter()
        .map(|core| &core.frequency)
        .filter(|cell| matches!(cell.state, CellState::Ok | CellState::Degraded))
        .filter(|cell| cell.value_f64().is_some())
        .collect();
    let Some(first) = current.first() else {
        return cores.first().map_or_else(
            || missing_cell(ids::CPU_FREQUENCY_CURRENT),
            |core| core.frequency.clone(),
        );
    };
    let total: f64 = current.iter().filter_map(|cell| cell.value_f64()).sum();
    #[allow(clippy::cast_precision_loss)]
    let mean = total / current.len() as f64;
    MetricCell {
        value: Scalar::from_f64(mean).ok(),
        kind: SampleKind::Derived,
        quality: current
            .iter()
            .map(|cell| cell.quality)
            .max()
            .unwrap_or(first.quality),
        age_ms: current.iter().filter_map(|cell| cell.age_ms).max(),
        source_label: Some(match current.len() {
            1 => "1 core".to_owned(),
            count => format!("mean of {count} cores"),
        }),
        conflict: None,
        last_good: None,
        ..(*first).clone()
    }
}

/// The CPU's temperature rows and the cluster the first of them comes from.
struct CpuTemperatures {
    rows: Vec<LabeledCell>,
    /// [`CpuView::hottest_cluster`].
    hottest_cluster: Option<String>,
}

/// Package temperatures first, then the cluster sensors a package's may be
/// derived from (GB10, D-68). A package derived from its clusters names
/// the hottest of them as its source (D-137), so the row says which
/// cluster its temperature is.
fn temperature_rows(reader: &MetricReader<'_>) -> CpuTemperatures {
    let clusters = labeled_entries(reader, ids::CPU_TEMPERATURE_CLUSTER);
    let mut hottest_cluster = None;
    let mut rows: Vec<LabeledCell> = labeled_entries(reader, ids::CPU_TEMPERATURE_PACKAGE)
        .into_iter()
        .enumerate()
        .map(|(position, (package, mut row))| {
            if let Some((cluster, hottest)) = hottest_of(package, &row.cell, &clusters) {
                row.cell.source_label = hottest.cell.source_label.clone();
                if position == 0 {
                    // Its own package is the CPU line's: the scope names it.
                    hottest_cluster = Some(humanize_scope(cluster.sample.key.scope.as_str()));
                }
            }
            row
        })
        .collect();
    rows.extend(clusters.into_iter().map(|(_, row)| row));
    rows.truncate(MAX_LABELED_ROWS);
    CpuTemperatures {
        rows,
        hottest_cluster,
    }
}

/// The hottest cluster of `package`, whose temperature `cell` is, when the
/// package is derived from its clusters and has a value: the first in
/// display order among equals. A package a sensor of its own reports has
/// none.
fn hottest_of<'r, 'a>(
    package: &SnapshotEntry,
    cell: &MetricCell,
    clusters: &'r [(&'a SnapshotEntry, LabeledCell)],
) -> Option<(&'a SnapshotEntry, &'r LabeledCell)> {
    if cell.kind != SampleKind::Derived || cell.value_f64().is_none() {
        return None;
    }
    let mut hottest: Option<(f64, &'a SnapshotEntry, &'r LabeledCell)> = None;
    for (entry, row) in clusters {
        if entry.sample.key.entity_id != package.sample.key.entity_id {
            continue;
        }
        let Some(celsius) = row.cell.value_f64() else {
            continue;
        };
        if hottest.is_none_or(|(highest, _, _)| celsius > highest) {
            hottest = Some((celsius, *entry, row));
        }
    }
    hottest.map(|(_, entry, row)| (entry, row))
}

/// The platform's power, then the CPU cluster components a package's may
/// be derived from (GB10, D-10).
fn power_rows(reader: &MetricReader<'_>) -> Vec<LabeledCell> {
    [ids::CPU_POWER_PLATFORM, ids::CPU_POWER_CLUSTER]
        .iter()
        .flat_map(|metric| labeled_rows(reader, metric))
        .take(MAX_LABELED_ROWS)
        .collect()
}

/// One labeled row per reported scope ("package 0", "SoC", "platform"),
/// ordered by scope kind then numeric id; duplicates name their entity.
fn labeled_rows(reader: &MetricReader<'_>, metric: &str) -> Vec<LabeledCell> {
    labeled_entries(reader, metric)
        .into_iter()
        .map(|(_, row)| row)
        .collect()
}

/// The rows of [`labeled_rows`], each with the entry it is of.
fn labeled_entries<'a>(
    reader: &MetricReader<'a>,
    metric: &str,
) -> Vec<(&'a SnapshotEntry, LabeledCell)> {
    let mut entries: Vec<_> = reader.index().entries_of_metric(metric).collect();
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
            let row = LabeledCell {
                label,
                cell: reader.cell_of(entry),
            };
            (*entry, row)
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

    fn build(fixture: &Fixture) -> CpuView {
        let reader = MetricReader::new(&fixture.snapshot, &fixture.catalog, NOW)
            .with_trends(&fixture.trends);
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

    fn frequency(fixture: &mut Fixture, metric: &str, core: u32, mhz: Option<f64>) {
        let sample = sample(metric, &format!("cpu.core.{core}"), &format!("core:{core}"));
        fixture.add(
            "cpu",
            match mhz {
                Some(mhz) => sample.f64(mhz),
                None => sample.unavailable(),
            },
        );
    }

    /// `(current, maximum, cores)` per group.
    fn groups(view: &CpuView) -> Vec<(Option<f64>, Option<f64>, usize)> {
        view.frequencies
            .iter()
            .map(|group| {
                (
                    group.current.value_f64(),
                    group.maximum.value_f64(),
                    group.cores,
                )
            })
            .collect()
    }

    /// Each kind of core runs at the mean frequency of its cores reporting
    /// one now (D-67), a derived value; a core reporting none (idle) is
    /// left out. A hybrid CPU (GB10: ten 2.808 GHz and ten 3.9 GHz cores,
    /// interleaved) shows one group per kind, slowest first.
    #[test]
    fn hybrid_cores_group_by_their_maximum_frequency() {
        let mut fixture = Fixture::new();
        for core in 0..4_u32 {
            let performance = core % 2 == 1;
            let maximum = if performance { 3900.0 } else { 2808.0 };
            frequency(
                &mut fixture,
                ids::CPU_FREQUENCY_MAXIMUM,
                core,
                Some(maximum),
            );
            let current = match core {
                0 => Some(2100.0),
                1 => Some(3300.0),
                2 => Some(2300.0),
                _ => None,
            };
            frequency(&mut fixture, ids::CPU_FREQUENCY_CURRENT, core, current);
        }
        let view = build(&fixture);
        assert_eq!(
            groups(&view),
            vec![
                (Some(2200.0), Some(2808.0), 2),
                (Some(3300.0), Some(3900.0), 2)
            ]
        );
        let efficiency = &view.frequencies[0].current;
        assert_eq!(efficiency.kind, SampleKind::Derived);
        assert_eq!(efficiency.source_label.as_deref(), Some("mean of 2 cores"));
        assert_eq!(
            view.frequencies[1].current.source_label.as_deref(),
            Some("1 core")
        );
    }

    /// A kind's limit is what its cores' cpufreq policies allow (D-136):
    /// the reading they share, or the lowest where they differ, a derived
    /// value that says of how many cores it is the lowest. A driver that
    /// reports no limit leaves the group without one.
    #[test]
    fn a_kinds_limit_is_the_lowest_its_cores_report() {
        let mut fixture = Fixture::new();
        for core in 0..4_u32 {
            let maximum = if core % 2 == 1 { 3900.0 } else { 2808.0 };
            frequency(
                &mut fixture,
                ids::CPU_FREQUENCY_MAXIMUM,
                core,
                Some(maximum),
            );
            frequency(&mut fixture, ids::CPU_FREQUENCY_CURRENT, core, Some(2000.0));
        }
        let unreported = build(&fixture);
        assert_eq!(unreported.frequencies[0].limit.value, None);
        assert_eq!(
            unreported.frequencies[0].limit.state,
            CellState::Unavailable
        );
        for (core, limit) in [(0, 2808.0), (2, 2808.0), (1, 3200.0), (3, 3000.0)] {
            frequency(&mut fixture, ids::CPU_FREQUENCY_LIMIT, core, Some(limit));
        }
        let view = build(&fixture);
        let efficiency = &view.frequencies[0].limit;
        assert_eq!(efficiency.value_f64(), Some(2808.0));
        assert_eq!(efficiency.kind, SampleKind::Measured);
        let performance = &view.frequencies[1].limit;
        assert_eq!(performance.value_f64(), Some(3000.0));
        assert_eq!(performance.kind, SampleKind::Derived);
        assert_eq!(
            performance.source_label.as_deref(),
            Some("lowest of 2 cores")
        );
        assert_eq!(view.frequencies[1].maximum.value_f64(), Some(3900.0));
    }

    /// One kind of core (plain x86) is one group; an idle group keeps its
    /// maximum and its first core's cell says why it has no current value.
    #[test]
    fn one_kind_of_core_is_one_group_and_idle_keeps_the_maximum() {
        let mut fixture = Fixture::new();
        for core in 0..3_u32 {
            frequency(&mut fixture, ids::CPU_FREQUENCY_MAXIMUM, core, Some(4500.0));
            frequency(&mut fixture, ids::CPU_FREQUENCY_CURRENT, core, None);
        }
        let view = build(&fixture);
        assert_eq!(groups(&view), vec![(None, Some(4500.0), 3)]);
        assert_eq!(view.frequencies[0].current.state, CellState::Unavailable);
    }

    /// Without cpufreq there is no group; cores reporting a current
    /// frequency without a maximum form one group without a maximum.
    #[test]
    fn missing_cpufreq_has_no_group() {
        let mut fixture = Fixture::new();
        fixture.add(
            "cpu",
            sample("cpu.utilization.total", "cpu.core.0", "core:0").f64(5.0),
        );
        assert!(build(&fixture).frequencies.is_empty());
        assert!(build(&Fixture::new()).frequencies.is_empty());
        frequency(&mut fixture, ids::CPU_FREQUENCY_CURRENT, 0, Some(1800.0));
        let view = build(&fixture);
        assert_eq!(groups(&view), vec![(Some(1800.0), None, 1)]);
        assert_eq!(view.frequencies[0].maximum.value, None);
    }

    /// The package temperature leads, and the cluster sensors it may come
    /// from follow it (D-68).
    #[test]
    fn cluster_temperatures_follow_the_package() {
        let mut fixture = Fixture::new();
        for (scope, celsius) in [("cluster:1p", 87.6), ("cluster:0e", 74.8)] {
            fixture.add(
                "cpu",
                sample("cpu.temperature.cluster", "cpu.package.0", scope).f64(celsius),
            );
        }
        fixture.add(
            "cpu",
            sample("cpu.temperature.package", "cpu.package.0", "package:0").f64(87.6),
        );
        let view = build(&fixture);
        let rows: Vec<(&str, Option<f64>)> = view
            .package_temperatures
            .iter()
            .map(|row| (row.label.as_str(), row.cell.value_f64()))
            .collect();
        assert_eq!(
            rows,
            vec![
                ("package 0", Some(87.6)),
                ("cluster 0e", Some(74.8)),
                ("cluster 1p", Some(87.6)),
            ]
        );
    }

    /// A package whose temperature is derived from its clusters (GB10,
    /// D-68) names the hottest of them as its source (D-137), the first in
    /// display order among equals, and the view names that cluster for the
    /// CPU line. A package a sensor reports itself keeps that sensor, and
    /// another package's clusters are never taken for its own.
    #[test]
    fn a_derived_package_names_its_hottest_cluster() {
        let cluster = |fixture: &mut Fixture, package: u32, scope: &str, celsius: f64| {
            fixture.add(
                "cpu",
                sample(
                    "cpu.temperature.cluster",
                    &format!("cpu.package.{package}"),
                    scope,
                )
                .source(&format!("thermal:acpitz:{package}.{scope}"))
                .f64(celsius),
            );
        };
        let mut fixture = Fixture::new();
        for (scope, celsius) in [
            ("cluster:0e", 74.8),
            ("cluster:0p", 87.6),
            ("cluster:1e", 70.0),
            ("cluster:1p", 87.6),
        ] {
            cluster(&mut fixture, 0, scope, celsius);
        }
        cluster(&mut fixture, 1, "cluster:0p", 99.0);
        fixture.add(
            "cpu",
            sample("cpu.temperature.package", "cpu.package.0", "package:0")
                .f64(87.6)
                .derived(),
        );
        let view = build(&fixture);
        assert_eq!(view.hottest_cluster.as_deref(), Some("cluster 0p"));
        let package = &view.package_temperatures[0];
        assert_eq!(package.label, "package 0");
        let hottest = view
            .package_temperatures
            .iter()
            .find(|row| row.label.starts_with("cluster 0p") && row.cell.value_f64() == Some(87.6))
            .unwrap();
        assert!(hottest.cell.source_label.is_some());
        assert_eq!(package.cell.source_label, hottest.cell.source_label);
        // A sensor of the package itself is its own source.
        let mut measured = Fixture::new();
        cluster(&mut measured, 0, "cluster:0p", 87.6);
        measured.add(
            "cpu",
            sample("cpu.temperature.package", "cpu.package.0", "package:0").f64(87.6),
        );
        let view = build(&measured);
        assert_eq!(view.hottest_cluster, None);
        // A derived package without a value names none.
        let mut missing = Fixture::new();
        cluster(&mut missing, 0, "cluster:0p", 87.6);
        missing.add(
            "cpu",
            sample("cpu.temperature.package", "cpu.package.0", "package:0")
                .derived()
                .unavailable(),
        );
        assert_eq!(build(&missing).hottest_cluster, None);
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

//! Settings module projection: one field per configuration leaf with saved
//! vs effective values, field origin, restart-only marking and overload
//! notes, plus the module list, persistence status and each adapter
//! host's status (the data sources block, D-91).

use std::collections::BTreeMap;

use dgxtop_core::config::{BUILTIN_MODULES, Config, ValueOrigin};
use dgxtop_core::identity::ModuleId;
use dgxtop_core::naming::groups;
use dgxtop_core::observation::split_page;
use dgxtop_core::view::{HostStatus, IntervalView, OverloadState, SettingField, SettingsView};
use serde_json::Value;

use super::SettingsInputs;
use super::labels::{label_text, note_text};

/// Whole sections that only take effect after a restart.
const RESTART_ONLY_SECTIONS: [&str; 2] = ["queues", "runtime"];
/// Individual restart-only fields (`Config::restart_required_changes`).
const RESTART_ONLY_FIELDS: [&str; 8] = [
    "sampling.max_adapters",
    "store.memory_limit_mib",
    "store.max_series",
    "store.max_sources",
    "store.max_processes",
    "store.max_process_gpu_links",
    "store.archive_enabled",
    "store.process_history_enabled",
];
/// Document metadata shown as the view revision, not as editable fields.
const METADATA_PATHS: [&str; 2] = ["schema_version", "revision"];
const INTERVAL_FIELD: &str = "sampling.interval_ms";
const CAPACITY_INTERVAL_FIELD: &str = "sampling.capacity_interval_ms";
const SLOW_HEALTH_INTERVAL_FIELD: &str = "sampling.slow_health_interval_ms";
const ABSENT_TEXT: &str = "(absent)";
/// Adapter hosts listed in the data sources block.
const MAX_HOSTS: usize = 16;
const EMPTY_LIST_TEXT: &str = "none";

/// What the settings projection is built from besides the configuration.
pub(crate) struct SettingsRuntime<'a> {
    pub(crate) intervals: &'a [IntervalView],
    pub(crate) overload: OverloadState,
    pub(crate) enabled_modules: &'a [String],
    pub(crate) hosts: &'a [HostStatus],
}

pub(crate) fn build_settings_view(
    settings: &SettingsInputs<'_>,
    runtime: &SettingsRuntime<'_>,
) -> SettingsView {
    let SettingsRuntime {
        intervals,
        overload,
        enabled_modules,
        hosts,
    } = *runtime;
    let saved = leaf_texts(settings.saved);
    let notes = overload_notes(intervals, overload);
    let fields = leaf_texts(settings.effective)
        .into_iter()
        .map(|(path, effective)| SettingField {
            saved: saved
                .get(&path)
                .cloned()
                .unwrap_or_else(|| ABSENT_TEXT.to_owned()),
            effective,
            origin: origin_of(&path, settings.origins),
            restart_required: is_restart_only(&path),
            overload_note: notes.get(path.as_str()).cloned(),
            path,
        })
        .collect();
    SettingsView {
        revision: settings.effective.revision,
        pending: settings.pending.as_deref().map(note_text),
        last_result: settings.last_result.as_deref().map(note_text),
        fields,
        modules: module_flags(enabled_modules),
        config_status: settings.status.clone(),
        hosts: hosts.iter().take(MAX_HOSTS).map(bounded_host).collect(),
    }
}

/// A host status whose texts are bounded like every other label.
fn bounded_host(status: &HostStatus) -> HostStatus {
    HostStatus {
        adapter_id: label_text(&status.adapter_id),
        detail: status.detail.as_deref().map(note_text),
        ..status.clone()
    }
}

/// Dotted leaf path → display text (arrays are leaves).
fn leaf_texts(config: &Config) -> BTreeMap<String, String> {
    let mut leaves = BTreeMap::new();
    if let Ok(document) = serde_json::to_value(config) {
        flatten("", &document, &mut leaves);
    }
    for path in METADATA_PATHS {
        leaves.remove(path);
    }
    leaves
}

fn flatten(prefix: &str, value: &Value, leaves: &mut BTreeMap<String, String>) {
    match value {
        Value::Object(members) => {
            for (key, member) in members {
                let path = if prefix.is_empty() {
                    key.clone()
                } else {
                    format!("{prefix}.{key}")
                };
                flatten(&path, member, leaves);
            }
        }
        leaf => {
            leaves.insert(prefix.to_owned(), value_text(leaf));
        }
    }
}

fn value_text(value: &Value) -> String {
    let text = match value {
        Value::String(text) => text.clone(),
        Value::Array(items) if items.is_empty() => EMPTY_LIST_TEXT.to_owned(),
        Value::Array(items) if items.iter().all(Value::is_string) => items
            .iter()
            .filter_map(Value::as_str)
            .collect::<Vec<_>>()
            .join(", "),
        Value::Array(items) => format!("{} entries: {value}", items.len()),
        other => other.to_string(),
    };
    note_text(&text)
}

/// The most specific recorded origin of the path or one of its parents;
/// unrecorded fields come from the saved configuration.
fn origin_of(path: &str, origins: &BTreeMap<String, ValueOrigin>) -> ValueOrigin {
    let mut candidate = path;
    loop {
        if let Some(origin) = origins.get(candidate) {
            return *origin;
        }
        match candidate.rsplit_once('.') {
            Some((parent, _)) => candidate = parent,
            None => return ValueOrigin::Saved,
        }
    }
}

pub(crate) fn is_restart_only(path: &str) -> bool {
    RESTART_ONLY_FIELDS.contains(&path)
        || RESTART_ONLY_SECTIONS.iter().any(|section| {
            path.strip_prefix(section)
                .is_some_and(|rest| rest.starts_with('.'))
        })
}

/// Notes the slowest effective period that exceeds its configured period,
/// on the sampling field that configures that read group.
fn overload_notes(
    intervals: &[IntervalView],
    overload: OverloadState,
) -> BTreeMap<&'static str, String> {
    let mut slowest: BTreeMap<&'static str, &IntervalView> = BTreeMap::new();
    for interval in intervals
        .iter()
        .filter(|interval| interval.effective_ms > interval.configured_ms)
    {
        let slot = slowest.entry(interval_field(interval)).or_insert(interval);
        if interval.effective_ms > slot.effective_ms {
            *slot = interval;
        }
    }
    slowest
        .into_iter()
        .map(|(path, interval)| {
            let note = note_text(&format!(
                "effective {} ms > configured {} ms ({}; {}/{}: {})",
                interval.effective_ms,
                interval.configured_ms,
                overload.label(),
                interval.adapter_id,
                interval.read_group,
                interval.reason
            ));
            (path, note)
        })
        .collect()
}

/// Replay groups mirror live ones as `<adapter>.<group>`; pages are `group:N`.
fn interval_field(interval: &IntervalView) -> &'static str {
    let (group, _) = split_page(&interval.read_group);
    let live_group = group.rsplit_once('.').map_or(group, |(_, live)| live);
    match live_group {
        groups::CAPACITY => CAPACITY_INTERVAL_FIELD,
        groups::SLOW_HEALTH => SLOW_HEALTH_INTERVAL_FIELD,
        _ => INTERVAL_FIELD,
    }
}

fn module_flags(enabled: &[String]) -> Vec<(ModuleId, bool)> {
    BUILTIN_MODULES
        .iter()
        .filter_map(|name| {
            let module = ModuleId::new(*name).ok()?;
            Some((module, enabled.iter().any(|entry| entry == name)))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::presentation::testkit::{Fixture, host_status};
    use dgxtop_core::view::HostState;

    fn build(fixture: &Fixture) -> SettingsView {
        let context = fixture.context();
        build_settings_view(
            &context.settings,
            &SettingsRuntime {
                intervals: context.intervals,
                overload: context.overload,
                enabled_modules: context.enabled_modules,
                hosts: context.hosts,
            },
        )
    }

    fn field<'a>(view: &'a SettingsView, path: &str) -> &'a SettingField {
        view.fields
            .iter()
            .find(|field| field.path == path)
            .unwrap_or_else(|| panic!("missing field {path}"))
    }

    #[test]
    fn every_leaf_is_a_field_with_saved_and_effective_values() {
        let mut fixture = Fixture::new();
        fixture.effective.sampling.interval_ms = 500;
        fixture
            .origins
            .insert("sampling.interval_ms".into(), ValueOrigin::CliOverride);
        fixture
            .origins
            .insert("viewer".into(), ValueOrigin::Default);
        let view = build(&fixture);
        assert!(view.fields.len() > 60);
        assert!(view.fields.iter().all(|field| field.path != "revision"));
        let interval = field(&view, "sampling.interval_ms");
        assert_eq!(interval.saved, "1000");
        assert_eq!(interval.effective, "500");
        assert_eq!(interval.origin, ValueOrigin::CliOverride);
        assert!(!interval.restart_required);
        assert_eq!(field(&view, "viewer.theme").effective, "dark");
        assert_eq!(field(&view, "viewer.theme").origin, ValueOrigin::Default);
        assert_eq!(field(&view, "store.max_series").origin, ValueOrigin::Saved);
        assert_eq!(field(&view, "quality.excluded_sources").effective, "none");
        assert!(
            field(&view, "viewer.enabled_modules")
                .effective
                .starts_with("cpu, gpu, memory")
        );
        assert_eq!(view.revision, 1);
    }

    #[test]
    fn restart_only_fields_match_core_semantics() {
        let base = Config::defaults().unwrap();
        let view = build(&Fixture::new());
        for setting in &view.fields {
            let mut document = serde_json::to_value(&base).unwrap();
            let Some(slot) = document.pointer_mut(&format!("/{}", setting.path.replace('.', "/")))
            else {
                continue;
            };
            let changed = match slot {
                Value::Bool(flag) => Value::Bool(!*flag),
                Value::Number(number) if number.is_u64() => {
                    Value::from(number.as_u64().unwrap().wrapping_add(1))
                }
                _ => continue,
            };
            *slot = changed;
            let candidate: Config = serde_json::from_value(document).unwrap();
            let core_says_restart = !base.restart_required_changes(&candidate).is_empty();
            assert_eq!(
                setting.restart_required, core_says_restart,
                "{}",
                setting.path
            );
        }
        assert!(is_restart_only("queues.commands"));
        assert!(is_restart_only("runtime.max_projection_bytes"));
        assert!(!is_restart_only("queuesx.commands"));
        assert!(!is_restart_only("store.max_history_points"));
    }

    /// The data sources block lists each adapter host with its timing
    /// facts, bounded in count and text (D-91).
    #[test]
    fn settings_carry_bounded_host_status() {
        let mut fixture = Fixture::new();
        let mut nvml = host_status("nvml", HostState::CircuitOpen, Some(&"x".repeat(400)));
        nvml.last_success_boot_ns = Some(3_000_000_000);
        nvml.backoff_until_boot_ns = Some(9_000_000_000);
        fixture.hosts = vec![nvml];
        let view = build(&fixture);
        assert_eq!(view.hosts.len(), 1);
        let host = &view.hosts[0];
        assert_eq!(host.adapter_id, "nvml");
        assert_eq!(host.state, HostState::CircuitOpen);
        assert_eq!(host.last_success_boot_ns, Some(3_000_000_000));
        assert_eq!(host.backoff_until_boot_ns, Some(9_000_000_000));
        assert!(host.detail.as_ref().unwrap().len() <= 256);

        fixture.hosts = (0..40)
            .map(|index| host_status(&format!("a{index}"), HostState::Healthy, None))
            .collect();
        assert_eq!(build(&fixture).hosts.len(), MAX_HOSTS);
        fixture.hosts.clear();
        assert!(build(&fixture).hosts.is_empty());
    }
}

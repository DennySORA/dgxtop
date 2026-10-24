//! Contract suite: every serialized Rust DTO must validate against the JSON
//! Schemas in `contracts/`, every example in `contracts/examples/` must parse
//! into the Rust type and re-serialize to a schema-valid document, and the
//! closed enum sets of the schemas must equal the Rust enums (AT02, AT40).

// Test helpers outside #[test] functions may unwrap: a broken fixture must fail loudly.
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod structure;

use std::path::PathBuf;

use dgxtop_core::action::{ActionState, ConfirmationToken, Signal};
use dgxtop_core::canonical::{CanonicalSample, CanonicalValue};
use dgxtop_core::commands::{CommandBody, CommandEnvelope, CommandResult, SortKey, ViewKind};
use dgxtop_core::config::Config;
use dgxtop_core::errors::ErrorCode;
use dgxtop_core::identity::{
    BootId, EntityId, HostId, ModuleId, PidNamespace, ProcessKey, SessionId, SourceId,
};
use dgxtop_core::metric::{CanonicalUnit, Scalar};
use dgxtop_core::observation::{
    AdapterManifest, IpcRequest, IpcRequestKind, Observation, ObservationBatch,
};
use dgxtop_core::quality::{Freshness, Quality, SampleKind, SelectionState};
use serde_json::{Value, json};
use uuid::Uuid;

/// dgxtop's contracts: the JSON Schemas and, under `examples/`, documents
/// that follow them.
fn contracts_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../contracts")
}

fn load_json(relative: &str) -> Value {
    let path = contracts_dir().join(relative);
    let text = std::fs::read_to_string(&path)
        .unwrap_or_else(|error| panic!("{}: {error}", path.display()));
    serde_json::from_str(&text).unwrap_or_else(|error| panic!("{}: {error}", path.display()))
}

fn validator(schema_name: &str) -> jsonschema::Validator {
    jsonschema::options()
        .should_validate_formats(true)
        .build(&load_json(schema_name))
        .unwrap_or_else(|error| panic!("{schema_name} does not compile: {error}"))
}

#[track_caller]
fn assert_valid(schema_name: &str, instance: &Value) {
    let validator = validator(schema_name);
    let errors: Vec<String> = validator
        .iter_errors(instance)
        .map(|error| format!("{} at {}", error, error.instance_path()))
        .collect();
    assert!(
        errors.is_empty(),
        "{schema_name} rejected {instance}: {errors:#?}"
    );
}

#[track_caller]
fn assert_invalid(schema_name: &str, instance: &Value) {
    assert!(
        !validator(schema_name).is_valid(instance),
        "{schema_name} unexpectedly accepted {instance}"
    );
}

fn to_value<T: serde::Serialize>(value: &T) -> Value {
    serde_json::to_value(value).expect("serializable contract value")
}

fn process_key() -> ProcessKey {
    ProcessKey {
        host_id: HostId::new("demo-host").unwrap(),
        boot_id: BootId::new("demo-boot").unwrap(),
        pid_namespace: PidNamespace::new("pid:[4026531836]").unwrap(),
        pid: dgxtop_core::identity::Pid::new(4242).unwrap(),
        start_time_ticks: u64::MAX,
    }
}

fn token() -> ConfirmationToken {
    ConfirmationToken::new("ab".repeat(32)).unwrap()
}

/// Example file -> (schema, parse into Rust, re-serialize, validate again).
macro_rules! example_round_trip {
    ($test:ident, $example:literal, $schema:literal, $ty:ty) => {
        #[test]
        fn $test() {
            let original = load_json(concat!("examples/", $example));
            assert_valid($schema, &original);
            let parsed: $ty = serde_json::from_value(original.clone())
                .unwrap_or_else(|error| panic!("{} does not parse: {error}", $example));
            let reencoded = to_value(&parsed);
            assert_valid($schema, &reencoded);
        }
    };
}

example_round_trip!(
    example_observation_cpu_package,
    "observation.cpu-package.json",
    "observation.schema.json",
    Observation
);
example_round_trip!(
    example_observation_conflicting,
    "observation.conflicting-source.json",
    "observation.schema.json",
    Observation
);
example_round_trip!(
    example_observation_batch,
    "observation-batch.partial.json",
    "observation-batch.schema.json",
    ObservationBatch
);
example_round_trip!(
    example_canonical_conflict,
    "canonical.conflict.json",
    "canonical.schema.json",
    CanonicalSample
);
example_round_trip!(
    example_command_prepare,
    "command.prepare-signal.json",
    "command.schema.json",
    CommandEnvelope
);
example_round_trip!(
    example_command_result,
    "command-result.interval.json",
    "command-result.schema.json",
    CommandResult
);
/// D-88: the REF configuration, written by an earlier version, parses; dgxtop
/// writes it back without the retired keys and everything else as written.
#[test]
fn example_config_reference() {
    let original = load_json("examples/config.reference.json");
    assert_valid("config.schema.json", &original);
    let parsed = Config::from_json(&original.to_string()).unwrap();
    assert_eq!(
        parsed.retired_keys_present(),
        vec![
            "store.raw_candidate_seconds",
            "store.max_query_scan_records",
            "runtime.max_history_result_bytes",
            "viewer.redline_threshold_percent"
        ]
    );
    let reencoded = to_value(&parsed);
    assert_valid("config.schema.json", &reencoded);
    let mut expected = original.clone();
    dgxtop_core::config::remove_retired_keys(&mut expected);
    assert_eq!(reencoded, expected);
}
example_round_trip!(
    example_ipc_request,
    "ipc-request.read.json",
    "ipc-request.schema.json",
    IpcRequest
);
example_round_trip!(
    example_adapter_manifest,
    "adapter-manifest.nvml.json",
    "adapter-manifest.schema.json",
    AdapterManifest
);

/// D-80: the contracts accept the history's bar sizes and the Process
/// History switch, and hold the bar sizes to the sizes dgxtop has.
#[test]
fn contract_extensions_are_accepted() {
    let session = SessionId::new("session-1").unwrap();
    let with_bars = CommandEnvelope::new(
        session.clone(),
        CommandBody::UpdateViewSession {
            view: ViewKind::History,
            selected_entity_id: None,
            selected_process: None,
            filter: String::new(),
            sort: SortKey::GpuMemory,
            ascending: false,
            frame_revision: 3,
            overview_bar: Some(dgxtop_core::tiers::BarSize::OneHour),
            process_bar: Some(dgxtop_core::tiers::BarSize::Auto),
        },
    );
    let encoded = to_value(&with_bars);
    assert_eq!(encoded["body"]["overview_bar"], "1h");
    assert_valid("command.schema.json", &encoded);
    let mut wrong = encoded.clone();
    wrong["body"]["process_bar"] = json!("3h");
    assert_invalid("command.schema.json", &wrong);
    let mut config = load_json("examples/config.reference.json");
    config["store"]["process_history_enabled"] = json!(false);
    assert_valid("config.schema.json", &config);
    let parsed = Config::from_json(&config.to_string()).unwrap();
    assert_eq!(parsed.store.process_history_enabled, Some(false));
}

#[test]
fn display_module_example_matches_schema() {
    assert_valid(
        "display-module.schema.json",
        &load_json("examples/display-module.cpu.json"),
    );
}

#[test]
fn embedded_default_config_is_schema_valid() {
    assert_valid(
        "config.schema.json",
        &to_value(&Config::defaults().unwrap()),
    );
}

fn every_command_body() -> Vec<CommandBody> {
    let config = Config::defaults().unwrap();
    vec![
        CommandBody::SetSamplingInterval {
            interval_ms: 2000,
            expected_config_revision: 1,
        },
        CommandBody::ReplaceConfig {
            expected_config_revision: 1,
            candidate: Box::new(config),
        },
        CommandBody::UpdateViewSession {
            view: ViewKind::Processes,
            selected_entity_id: Some(EntityId::new("gpu:GPU-1").unwrap()),
            selected_process: Some(process_key()),
            filter: "python".into(),
            sort: SortKey::GpuMemory,
            ascending: false,
            frame_revision: u64::MAX,
            overview_bar: None,
            process_bar: None,
        },
        CommandBody::UpdateViewSession {
            view: ViewKind::History,
            selected_entity_id: None,
            selected_process: None,
            filter: "1h".into(),
            sort: SortKey::Pid,
            ascending: true,
            frame_revision: 0,
            overview_bar: None,
            process_bar: None,
        },
        CommandBody::PrepareSignal {
            target: process_key(),
            signal: Signal::Sigterm,
        },
        CommandBody::PrepareSignal {
            target: process_key(),
            signal: Signal::Sigkill,
        },
        CommandBody::ConfirmSignal {
            confirmation_token: token(),
        },
        CommandBody::CancelSignal {
            confirmation_token: token(),
        },
        CommandBody::DeleteRuns {
            runs: vec![process_key(); dgxtop_core::commands::MAX_DELETE_RUNS],
        },
        CommandBody::DeleteAllRuns {
            filter: "comfy".into(),
        },
        CommandBody::SetFolded {
            panels: dgxtop_core::view::FOLDABLE_PANELS
                .iter()
                .map(|panel| (*panel).to_owned())
                .collect(),
        },
        CommandBody::SetFolded { panels: Vec::new() },
        CommandBody::SetModuleEnabled {
            module_id: ModuleId::new("history").unwrap(),
            enabled: false,
            expected_config_revision: 3,
        },
        CommandBody::Shutdown {
            reason: "user".into(),
        },
    ]
}

#[test]
fn every_command_variant_is_schema_valid() {
    let session = SessionId::new("session-1").unwrap();
    for body in every_command_body() {
        let envelope = CommandEnvelope::new(session.clone(), body);
        envelope.validate_syntax().unwrap();
        assert_valid("command.schema.json", &to_value(&envelope));
    }
}

#[test]
fn command_results_are_schema_valid() {
    let request = Uuid::new_v4();
    let mut prepared =
        CommandResult::completed(request, "prepared").with_action_state(ActionState::Prepared);
    prepared.confirmation_token = Some(token());
    prepared.expires_boot_ns = Some(u64::MAX);
    let mut applied = CommandResult::completed(request, "applied");
    applied.config_revision = Some(2);
    applied.effective_interval_ms = Some(1000);
    let results = [
        CommandResult::accepted(request),
        CommandResult::in_progress(request, "sending").with_action_state(ActionState::Sending),
        CommandResult::rejected(request, ErrorCode::StaleView, "stale frame"),
        CommandResult::unknown(request, "audit outcome unknown")
            .with_action_state(ActionState::Unknown),
        CommandResult::completed(request, "sent").with_action_state(ActionState::SignalSent),
        CommandResult::completed(request, "gone").with_action_state(ActionState::ExitObserved),
        CommandResult::completed(request, "late").with_action_state(ActionState::NotSent),
        prepared,
        applied,
    ];
    for result in results {
        assert_valid("command-result.schema.json", &to_value(&result));
    }
}

#[test]
fn every_error_code_is_a_schema_result_code() {
    let schema = load_json("command-result.schema.json");
    let allowed: Vec<String> = schema["properties"]["code"]["enum"]
        .as_array()
        .unwrap()
        .iter()
        .map(|value| value.as_str().unwrap().to_owned())
        .collect();
    let codes = [
        ErrorCode::ValidationFailed,
        ErrorCode::Conflict,
        ErrorCode::Unsupported,
        ErrorCode::Unavailable,
        ErrorCode::PermissionDenied,
        ErrorCode::CapabilityDenied,
        ErrorCode::Busy,
        ErrorCode::TimedOut,
        ErrorCode::StaleView,
        ErrorCode::SourceConflict,
        ErrorCode::PinnedSourceUnavailable,
        ErrorCode::CapacityExceeded,
        ErrorCode::PersistFailed,
        ErrorCode::RequiresRestart,
        ErrorCode::ProcessExited,
        ErrorCode::ProcessIdentityChanged,
        ErrorCode::NamespaceMismatch,
        ErrorCode::ConfirmationExpired,
        ErrorCode::ConfirmationUsed,
        ErrorCode::UnknownOutcome,
        ErrorCode::InternalError,
        ErrorCode::QueryTooLarge,
        ErrorCode::Cancelled,
    ];
    let mut rust: Vec<String> = codes.iter().map(|code| code.as_str().to_owned()).collect();
    rust.push("Ok".to_owned());
    rust.sort();
    let mut schema_codes = allowed;
    schema_codes.sort();
    assert_eq!(
        rust, schema_codes,
        "ErrorCode and command-result code enum diverged"
    );
}

#[test]
fn canonical_units_match_schema_enum() {
    let schema = load_json("canonical.schema.json");
    let mut allowed: Vec<String> = schema["properties"]["unit"]["enum"]
        .as_array()
        .unwrap()
        .iter()
        .map(|value| value.as_str().unwrap().to_owned())
        .collect();
    allowed.sort();
    let units = [
        CanonicalUnit::DegC,
        CanonicalUnit::Watts,
        CanonicalUnit::Bytes,
        CanonicalUnit::BytesPerSecond,
        CanonicalUnit::Percent,
        CanonicalUnit::MHz,
        CanonicalUnit::Count,
        CanonicalUnit::CountPerSecond,
        CanonicalUnit::Milliseconds,
        CanonicalUnit::Joules,
    ];
    let mut rust: Vec<String> = units
        .iter()
        .map(|unit| to_value(unit).as_str().unwrap().to_owned())
        .collect();
    rust.sort();
    assert_eq!(rust, allowed);
}

#[test]
fn view_and_sort_enums_match_command_schema() {
    let schema = load_json("command.schema.json");
    let update = schema["properties"]["body"]["oneOf"]
        .as_array()
        .unwrap()
        .iter()
        .find(|variant| variant["properties"]["type"]["const"] == "UpdateViewSession")
        .unwrap()
        .clone();
    let mut views: Vec<String> = update["properties"]["view"]["enum"]
        .as_array()
        .unwrap()
        .iter()
        .map(|value| value.as_str().unwrap().to_owned())
        .collect();
    views.sort();
    let mut rust_views: Vec<String> = ViewKind::ALL
        .iter()
        .map(|view| to_value(view).as_str().unwrap().to_owned())
        .collect();
    rust_views.sort();
    assert_eq!(rust_views, views);
    let mut sorts: Vec<String> = update["properties"]["sort"]["enum"]
        .as_array()
        .unwrap()
        .iter()
        .map(|value| value.as_str().unwrap().to_owned())
        .collect();
    sorts.sort();
    let mut rust_sorts: Vec<String> = [
        SortKey::GpuMemory,
        SortKey::GpuUtil,
        SortKey::CpuPercent,
        SortKey::HostMemory,
        SortKey::Pid,
    ]
    .iter()
    .map(|sort| to_value(sort).as_str().unwrap().to_owned())
    .collect();
    rust_sorts.sort();
    assert_eq!(rust_sorts, sorts);
}

fn selected_value() -> CanonicalValue {
    CanonicalValue {
        key: serde_json::from_value(json!({
            "host_id":"demo-host","boot_id":"demo-boot","entity_id":"cpu.package.0",
            "metric_id":"cpu.temperature.package","scope":"package:0"
        }))
        .unwrap(),
        cycle_id: 42,
        source_id: Some(SourceId::new("hwmon:coretemp.0:temp1").unwrap()),
        source_generation: Some(1),
        unit: CanonicalUnit::DegC,
        value: Some(Scalar::from_f64(0.0).unwrap()),
        sample_kind: SampleKind::Measured,
        selection_state: SelectionState::Selected,
        quality: Quality::Verified,
        freshness: Freshness::Fresh,
        measurement_freshness: Freshness::Unknown,
        observed_boot_ns: None,
        read_finished_boot_ns: 42_000_000_000,
        policy_revision: 1,
        candidate_source_ids: vec![SourceId::new("thermal:x86_pkg_temp:zone2").unwrap()],
        reason_codes: vec![],
        eligible_for_history: true,
    }
}

#[test]
fn canonical_samples_follow_schema_conditionals() {
    let selected = selected_value();
    selected.validate().unwrap();
    assert_valid(
        "canonical.schema.json",
        &to_value(&CanonicalSample::commit(selected.clone(), 9, 43)),
    );

    let mut conflict = selected.clone();
    conflict.selection_state = SelectionState::Conflict;
    conflict.value = None;
    conflict.source_id = None;
    conflict.source_generation = None;
    conflict.eligible_for_history = false;
    conflict.validate().unwrap();
    assert_valid(
        "canonical.schema.json",
        &to_value(&CanonicalSample::commit(conflict, 9, 43)),
    );

    // A value carried by a conflict is rejected by both Rust and the schema.
    let mut broken = selected;
    broken.selection_state = SelectionState::Conflict;
    assert!(broken.validate().is_err());
    assert_invalid(
        "canonical.schema.json",
        &to_value(&CanonicalSample::commit(broken, 9, 43)),
    );
}

#[test]
fn ipc_requests_for_every_kind_are_schema_valid() {
    let mut request: IpcRequest =
        serde_json::from_value(load_json("examples/ipc-request.read.json")).unwrap();
    assert_valid("ipc-request.schema.json", &to_value(&request));
    for kind in [IpcRequestKind::Discover, IpcRequestKind::Shutdown] {
        request.kind = kind;
        request.source_slots.clear();
        request.validate().unwrap();
        assert_valid("ipc-request.schema.json", &to_value(&request));
    }
}

#[test]
fn u64_values_are_decimal_strings_up_to_max() {
    let mut observation: Observation =
        serde_json::from_value(load_json("examples/observation.cpu-package.json")).unwrap();
    observation.value = Some(Scalar::from_u64(u64::MAX));
    observation.cycle_id = u64::MAX;
    let encoded = to_value(&observation);
    assert_eq!(encoded["value"]["value"], "18446744073709551615");
    assert_valid("observation.schema.json", &encoded);
    let back: Observation = serde_json::from_value(encoded).unwrap();
    assert_eq!(back.value.unwrap().as_u64(), Some(u64::MAX));

    let mut overflow = load_json("examples/observation.cpu-package.json");
    overflow["cycle_id"] = json!("18446744073709551616");
    assert!(serde_json::from_value::<Observation>(overflow).is_err());
}

#[test]
fn unknown_fields_are_rejected_by_rust_and_schema() {
    let mut command = load_json("examples/command.prepare-signal.json");
    command["body"]["shell"] = json!("rm -rf /");
    assert_invalid("command.schema.json", &command);
    assert!(serde_json::from_value::<CommandEnvelope>(command).is_err());

    let mut config = load_json("examples/config.reference.json");
    config["runtime"]["turbo"] = json!(true);
    assert_invalid("config.schema.json", &config);
    assert!(Config::from_json(&config.to_string()).is_err());
}

#[test]
fn invalid_pids_are_rejected_by_rust_and_schema() {
    for pid in [0_i64, -1, i64::from(i32::MAX) + 1] {
        let mut command = load_json("examples/command.prepare-signal.json");
        command["body"]["target"]["pid"] = json!(pid);
        assert_invalid("command.schema.json", &command);
        assert!(serde_json::from_value::<CommandEnvelope>(command).is_err());
    }
}

#[test]
fn failed_reads_carry_null_values_in_both_layers() {
    let batch = load_json("examples/observation-batch.partial.json");
    let parsed: ObservationBatch = serde_json::from_value(batch).unwrap();
    let failed = &parsed.records[1];
    assert!(failed.value.is_none());
    let mut zero_filled = to_value(failed);
    zero_filled["value"] = json!({"type": "f64", "value": 0.0});
    assert_invalid("observation.schema.json", &zero_filled);
    assert!(
        serde_json::from_value::<Observation>(zero_filled)
            .unwrap()
            .validate()
            .is_err()
    );
}

#[test]
fn performance_report_schema_rejects_synthetic_shape_gaps() {
    // The schema itself must compile; a synthetic example is not measured evidence.
    let report = json!({"schema_version": 1, "kind": "synthetic_example"});
    assert_invalid("performance-report.schema.json", &report);
}

#[test]
fn every_schema_compiles() {
    for name in [
        "adapter-manifest.schema.json",
        "canonical.schema.json",
        "command-result.schema.json",
        "command.schema.json",
        "config.schema.json",
        "display-module.schema.json",
        "ipc-request.schema.json",
        "observation-batch.schema.json",
        "observation.schema.json",
        "performance-report.schema.json",
    ] {
        let _ = validator(name);
    }
}

//! Shared sysfs mechanics of the hwmon, thermal and powercap adapters:
//! bounded directory listing, optional attribute reads, stable device keys
//! and the static manifest of a live sysfs adapter.
//!
//! Lives under `hwmon` only because of module ownership; it is a candidate
//! for `common.rs`.

use std::io;

use dgxtop_core::identity::{AdapterId, EntityId};
use dgxtop_core::naming::{attributes, entity};
use dgxtop_core::observation::{
    AdapterManifest, AdapterOperation, Attribute, EntityKind, Isolation,
};
use dgxtop_core::quality::Namespace;
use dgxtop_core::text::{sanitize_display, truncate_utf8};

use super::table::EntityDraft;
use crate::common::SystemFiles;

/// Maximum directory entries considered per listing; hostile or broken trees
/// cannot make discovery unbounded.
pub(crate) const MAX_LISTED_ENTRIES: usize = 4096;
/// Maximum bytes of a device key embedded in source and entity ids.
pub(crate) const MAX_DEVICE_KEY_BYTES: usize = 160;
/// Maximum bytes kept from a driver-provided name or label.
pub(crate) const MAX_LABEL_BYTES: usize = 64;
/// Root of physical devices; device keys are expressed below it.
const SYS_DEVICES: &str = "/sys/devices/";

/// Static manifest of a live sysfs adapter (persistent host process).
pub(crate) fn live_manifest(adapter: &'static str, implementation: &str) -> AdapterManifest {
    AdapterManifest {
        api_version: 1,
        adapter_id: static_adapter_id(adapter),
        implementation: implementation.to_owned(),
        isolation: Isolation::PersistentProcess,
        operations: vec![
            AdapterOperation::Discover,
            AdapterOperation::Read,
            AdapterOperation::Shutdown,
        ],
        max_inflight: 1,
        runtime_library: None,
        minimum_sample_ms: 1000,
        source_limit: 2048,
        sample_namespace: Namespace::Live,
    }
}

/// Typed id of a compile-time adapter name (the literals are covered by tests).
fn static_adapter_id(adapter: &'static str) -> AdapterId {
    match AdapterId::new(adapter) {
        Ok(id) => id,
        Err(error) => panic!("built-in adapter id {adapter:?} is invalid: {error}"),
    }
}

/// Reads a small attribute and trims it; a missing file is `Ok(None)`.
pub(crate) fn read_optional(files: &dyn SystemFiles, path: &str) -> io::Result<Option<String>> {
    match files.read_to_string(path) {
        Ok(text) => Ok(Some(text.trim().to_owned())),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error),
    }
}

/// Reads a driver label/name: sanitized for display, `None` when absent or empty.
pub(crate) fn read_label(files: &dyn SystemFiles, path: &str) -> io::Result<Option<String>> {
    Ok(read_optional(files, path)?.and_then(|text| sanitized_label(&text)))
}

/// Sanitized, bounded label; `None` when nothing printable remains.
pub(crate) fn sanitized_label(text: &str) -> Option<String> {
    let cleaned = sanitize_display(text.trim(), MAX_LABEL_BYTES);
    let cleaned = cleaned.trim();
    (!cleaned.is_empty()).then(|| cleaned.to_owned())
}

/// Entry names of a directory, bounded to [`MAX_LISTED_ENTRIES`].
pub(crate) fn list_names(files: &dyn SystemFiles, path: &str) -> io::Result<Vec<String>> {
    let mut names: Vec<String> = files
        .read_dir(path)?
        .into_iter()
        .take(MAX_LISTED_ENTRIES)
        .map(|entry| entry.name)
        .collect();
    names.sort();
    Ok(names)
}

/// Canonical (symlink-free) path text as reported by the filesystem backend.
pub(crate) fn canonical(files: &dyn SystemFiles, path: &str) -> io::Result<String> {
    Ok(files.canonicalize(path)?.to_string_lossy().into_owned())
}

/// Canonical target of an optional link (`device`); `Ok(None)` when absent.
pub(crate) fn canonical_link(files: &dyn SystemFiles, path: &str) -> io::Result<Option<String>> {
    match canonical(files, path) {
        Ok(target) => Ok(Some(target)),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error),
    }
}

/// Index of a numbered sysfs entry (`hwmon3` with prefix `hwmon` -> 3).
pub(crate) fn numbered(name: &str, prefix: &str) -> Option<u32> {
    let digits = name.strip_prefix(prefix)?;
    if digits.is_empty() || !digits.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    digits.parse().ok()
}

/// Whether an I/O error means the entry vanished (as opposed to a failure).
pub(crate) fn is_missing(error: &io::Error) -> bool {
    error.kind() == io::ErrorKind::NotFound
}

/// Stable device key: `<name>` or `<name>@<device path below /sys/devices>`.
/// The enumeration index (`hwmonN`, `thermal_zoneN`) is never part of it.
pub(crate) fn device_key(name: &str, device_path: Option<&str>) -> String {
    let name = key_text(name);
    let key = match device_path {
        Some(path) => {
            let relative = path.strip_prefix(SYS_DEVICES).unwrap_or(path);
            format!("{name}@{}", key_text(relative))
        }
        None => name,
    };
    bounded_key(key)
}

/// Appends `#<index>` to keys shared by several devices. The index is only a
/// last-resort disambiguator for devices without a distinguishing device link.
pub(crate) fn disambiguate(keys: &mut [(String, u32)]) {
    let mut counts = std::collections::BTreeMap::<String, usize>::new();
    for (key, _) in keys.iter() {
        *counts.entry(key.clone()).or_insert(0) += 1;
    }
    for (key, index) in keys.iter_mut() {
        if counts.get(key.as_str()).copied().unwrap_or(0) > 1 {
            *key = bounded_key(format!("{key}#{index}"));
        }
    }
}

/// Restricts key text to a conservative id alphabet.
fn key_text(text: &str) -> String {
    let mapped: String = text
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() || "._-:@/+".contains(character) {
                character
            } else {
                '_'
            }
        })
        .collect();
    if mapped.is_empty() {
        "unnamed".to_owned()
    } else {
        mapped
    }
}

/// Keeps keys within [`MAX_DEVICE_KEY_BYTES`]; long keys keep a readable prefix
/// plus a stable hash of the full text so distinct devices stay distinct.
fn bounded_key(key: String) -> String {
    if key.len() <= MAX_DEVICE_KEY_BYTES {
        return key;
    }
    let digest = fnv1a(&key);
    let prefix = truncate_utf8(&key, MAX_DEVICE_KEY_BYTES - 17);
    format!("{prefix}~{digest:016x}")
}

/// Bounded entity attribute (key <= 64 bytes, value <= 256 bytes).
pub(crate) fn attribute(key: &str, value: &str) -> Attribute {
    Attribute {
        key: truncate_utf8(key, 64).to_owned(),
        value: truncate_utf8(value, 256).to_owned(),
    }
}

/// Record of an entity shared with other adapters (`cpu.package.N`, `soc`).
/// Such entities are never removed by a sysfs adapter. `host` belongs to the
/// procfs adapter and is not emitted here.
pub(crate) fn shared_entity_draft(entity_id: &EntityId) -> Option<EntityDraft> {
    let host = EntityId::new(entity::HOST).ok();
    if entity_id.as_str() == entity::SOC {
        return Some(EntityDraft {
            entity_id: entity_id.clone(),
            kind: EntityKind::Soc,
            parent_id: host,
            display_name: "SoC".to_owned(),
            attributes: Vec::new(),
            device_key: None,
            owned: false,
        });
    }
    let package = entity_id.as_str().strip_prefix("cpu.package.")?;
    package.parse::<u32>().ok()?;
    Some(EntityDraft {
        entity_id: entity_id.clone(),
        kind: EntityKind::CpuPackage,
        parent_id: host,
        display_name: format!("CPU package {package}"),
        attributes: vec![attribute(attributes::PHYSICAL_PACKAGE, package)],
        device_key: None,
        owned: false,
    })
}

/// FNV-1a 64-bit: stable across runs and platforms.
pub(crate) fn fnv1a(text: &str) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in text.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn device_keys_use_path_below_sys_devices() {
        assert_eq!(
            device_key("coretemp", Some("/sys/devices/platform/coretemp.0")),
            "coretemp@platform/coretemp.0"
        );
        assert_eq!(device_key("acpitz", None), "acpitz");
        assert_eq!(device_key("we ird\u{1b}", None), "we_ird_");
        assert_eq!(device_key("", None), "unnamed");
    }

    #[test]
    fn long_keys_are_bounded_and_distinct() {
        let first = device_key("x", Some(&format!("/sys/devices/{}a", "p/".repeat(120))));
        let second = device_key("x", Some(&format!("/sys/devices/{}b", "p/".repeat(120))));
        assert!(first.len() <= MAX_DEVICE_KEY_BYTES);
        assert_ne!(first, second);
    }

    #[test]
    fn collisions_get_index_suffix_only_when_shared() {
        let mut keys = vec![
            ("x86_pkg_temp".to_owned(), 1),
            ("x86_pkg_temp".to_owned(), 2),
            ("acpitz".to_owned(), 0),
        ];
        disambiguate(&mut keys);
        assert_eq!(keys[0].0, "x86_pkg_temp#1");
        assert_eq!(keys[1].0, "x86_pkg_temp#2");
        assert_eq!(keys[2].0, "acpitz");
    }

    #[test]
    fn numbered_entries_require_digits() {
        assert_eq!(numbered("hwmon12", "hwmon"), Some(12));
        assert_eq!(numbered("hwmon", "hwmon"), None);
        assert_eq!(numbered("hwmonx", "hwmon"), None);
        assert_eq!(numbered("thermal_zone3", "thermal_zone"), Some(3));
    }

    #[test]
    fn labels_are_sanitized_and_empty_is_none() {
        assert_eq!(
            sanitized_label(" Package id 0\n").as_deref(),
            Some("Package id 0")
        );
        assert_eq!(sanitized_label("  \u{1b}[31m ").as_deref(), None);
        assert_eq!(
            sanitized_label("evil\u{1b}]0;title\u{7}name").as_deref(),
            Some("evilname")
        );
    }
}

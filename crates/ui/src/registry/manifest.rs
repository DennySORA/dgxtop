//! `DisplayModuleManifest` (`contracts/display-module.schema.json`, v1).
//!
//! The UI crate has no serde dependency (the workspace lockfile pins its
//! dependency set), so the manifest is a typed struct whose `validate()`
//! enforces the schema constraints of the fields it keeps. The schema's
//! `topics` and `supports_compact` are left out, and of its slots only
//! `overview` and `detail` are kept: nothing read them or placed a module
//! in the others (D-91, a spec deviation).

use dgxtop_core::identity::ModuleId;
use thiserror::Error;

/// The only accepted display-module API version.
pub const DISPLAY_MODULE_API_VERSION: u32 = 1;

const MAX_TITLE_BYTES: usize = 80;
const MIN_COLUMNS_RANGE: (u16, u16) = (10, 300);
const MIN_ROWS_RANGE: (u16, u16) = (2, 100);
const MAX_PRIORITY: u8 = 100;

/// Layout position a module supports (schema `slots` enum).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Slot {
    Overview,
    Detail,
}

impl Slot {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Overview => "overview",
            Self::Detail => "detail",
        }
    }
}

/// Schema violations of a manifest.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum ManifestError {
    #[error("api_version must be {DISPLAY_MODULE_API_VERSION}, found {0}")]
    ApiVersion(u32),
    #[error("title must be 1..=80 bytes, found {0}")]
    TitleLength(usize),
    #[error("min_columns must be 10..=300, found {0}")]
    MinColumns(u16),
    #[error("min_rows must be 2..=100, found {0}")]
    MinRows(u16),
    #[error("priority must be 0..=100, found {0}")]
    Priority(u8),
    #[error("duplicate slot {0}")]
    DuplicateSlot(&'static str),
}

/// Static description of one display module.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DisplayModuleManifest {
    pub module_id: ModuleId,
    pub api_version: u32,
    /// Shown where the module's own panel cannot be, such as the error
    /// frame of a module that failed to render.
    pub title: String,
    pub min_columns: u16,
    pub min_rows: u16,
    pub priority: u8,
    pub slots: Vec<Slot>,
    pub enabled_by_default: bool,
}

impl DisplayModuleManifest {
    /// Enforces every kept schema constraint (`module_id` length is
    /// guaranteed by [`ModuleId`]).
    pub fn validate(&self) -> Result<(), ManifestError> {
        if self.api_version != DISPLAY_MODULE_API_VERSION {
            return Err(ManifestError::ApiVersion(self.api_version));
        }
        if self.title.is_empty() || self.title.len() > MAX_TITLE_BYTES {
            return Err(ManifestError::TitleLength(self.title.len()));
        }
        if !(MIN_COLUMNS_RANGE.0..=MIN_COLUMNS_RANGE.1).contains(&self.min_columns) {
            return Err(ManifestError::MinColumns(self.min_columns));
        }
        if !(MIN_ROWS_RANGE.0..=MIN_ROWS_RANGE.1).contains(&self.min_rows) {
            return Err(ManifestError::MinRows(self.min_rows));
        }
        if self.priority > MAX_PRIORITY {
            return Err(ManifestError::Priority(self.priority));
        }
        if let Some(slot) = first_duplicate(&self.slots) {
            return Err(ManifestError::DuplicateSlot(slot.as_str()));
        }
        Ok(())
    }

    pub fn has_slot(&self, slot: Slot) -> bool {
        self.slots.contains(&slot)
    }

    /// Compact JSON of the kept fields in schema field order.
    #[cfg(test)]
    fn to_json(&self) -> String {
        let slots: Vec<String> = self
            .slots
            .iter()
            .map(|slot| json_string(slot.as_str()))
            .collect();
        format!(
            "{{\"module_id\":{},\"api_version\":{},\"title\":{},\"min_columns\":{},\"min_rows\":{},\"priority\":{},\"slots\":[{}],\"enabled_by_default\":{}}}",
            json_string(self.module_id.as_str()),
            self.api_version,
            json_string(&self.title),
            self.min_columns,
            self.min_rows,
            self.priority,
            slots.join(","),
            self.enabled_by_default,
        )
    }
}

fn first_duplicate<T: Copy + PartialEq>(items: &[T]) -> Option<T> {
    items
        .iter()
        .enumerate()
        .find(|(index, item)| items.iter().take(*index).any(|earlier| earlier == *item))
        .map(|(_, item)| *item)
}

#[cfg(test)]
fn json_string(text: &str) -> String {
    let mut encoded = String::with_capacity(text.len() + 2);
    encoded.push('"');
    for character in text.chars() {
        match character {
            '"' => encoded.push_str("\\\""),
            '\\' => encoded.push_str("\\\\"),
            '\n' => encoded.push_str("\\n"),
            '\r' => encoded.push_str("\\r"),
            '\t' => encoded.push_str("\\t"),
            control if control.is_control() => {
                encoded.push_str(&format!("\\u{:04x}", u32::from(control)));
            }
            other => encoded.push(other),
        }
    }
    encoded.push('"');
    encoded
}

#[cfg(test)]
mod tests {
    use super::*;

    fn example_cpu_manifest() -> DisplayModuleManifest {
        DisplayModuleManifest {
            module_id: ModuleId::new("cpu").unwrap(),
            api_version: 1,
            title: "CPU 使用率與溫度".into(),
            min_columns: 36,
            min_rows: 8,
            priority: 80,
            slots: vec![Slot::Overview, Slot::Detail],
            enabled_by_default: true,
        }
    }

    /// Removes insignificant whitespace outside JSON strings.
    fn minify(json: &str) -> String {
        let mut output = String::new();
        let mut in_string = false;
        let mut escaped = false;
        for character in json.chars() {
            if in_string {
                output.push(character);
                if escaped {
                    escaped = false;
                } else if character == '\\' {
                    escaped = true;
                } else if character == '"' {
                    in_string = false;
                }
            } else if character == '"' {
                in_string = true;
                output.push(character);
            } else if !character.is_whitespace() {
                output.push(character);
            }
        }
        output
    }

    /// The kept fields match the spec's example exactly; only the dropped
    /// `topics` and `supports_compact` differ (D-91).
    #[test]
    fn to_json_matches_the_spec_example_without_the_dropped_fields() {
        let example = include_str!("../../../../spec/dgxtop-v2/examples/display-module.cpu.json");
        let dropped_topics = "\"topics\":[\"cpu\",\"sources\",\"health\"],";
        let dropped_compact = ",\"supports_compact\":true";
        let minified = minify(example);
        assert!(minified.contains(dropped_topics) && minified.contains(dropped_compact));
        let kept = minified
            .replace(dropped_topics, "")
            .replace(dropped_compact, "");
        let manifest = example_cpu_manifest();
        manifest.validate().unwrap();
        assert_eq!(manifest.to_json(), kept);
    }

    #[test]
    fn schema_constraints_are_enforced() {
        let mut manifest = example_cpu_manifest();
        manifest.api_version = 2;
        assert_eq!(manifest.validate(), Err(ManifestError::ApiVersion(2)));
        let mut manifest = example_cpu_manifest();
        manifest.title = String::new();
        assert_eq!(manifest.validate(), Err(ManifestError::TitleLength(0)));
        let mut manifest = example_cpu_manifest();
        manifest.title = "x".repeat(81);
        assert!(manifest.validate().is_err());
        let mut manifest = example_cpu_manifest();
        manifest.min_columns = 9;
        assert_eq!(manifest.validate(), Err(ManifestError::MinColumns(9)));
        let mut manifest = example_cpu_manifest();
        manifest.min_rows = 101;
        assert_eq!(manifest.validate(), Err(ManifestError::MinRows(101)));
        let mut manifest = example_cpu_manifest();
        manifest.priority = 101;
        assert_eq!(manifest.validate(), Err(ManifestError::Priority(101)));
        let mut manifest = example_cpu_manifest();
        manifest.slots = vec![Slot::Overview, Slot::Overview];
        assert_eq!(
            manifest.validate(),
            Err(ManifestError::DuplicateSlot("overview"))
        );
    }

    #[test]
    fn json_strings_are_escaped() {
        assert_eq!(json_string("a\"b\\c\u{1}"), "\"a\\\"b\\\\c\\u0001\"");
    }
}

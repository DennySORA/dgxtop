//! Safe reading and parsing of configuration documents.

use std::path::Path;

use dgxtop_core::config::{CONFIG_SCHEMA_VERSION, Config, ConfigError, ValidationIssue};
use dgxtop_core::digest::sha256_hex;
use dgxtop_core::errors::InfraError;
use dgxtop_core::ports::ConfigLoadError;
use dgxtop_core::text::sanitize_message;

use crate::fs::{FileSystemOps, LimitedRead, ReadLimitedError};

/// Largest configuration file accepted (1 MiB).
pub const MAX_CONFIG_FILE_BYTES: u64 = 1024 * 1024;

/// Why a file we would trust could not be read.
#[derive(Debug)]
pub(crate) enum TrustError {
    Unsafe(String),
    TooLarge { limit: u64 },
    Io(InfraError),
}

/// Reads a regular file that we own and nobody else can modify, without
/// following a final symlink. `Ok(None)` when it does not exist.
pub(crate) fn read_trusted(
    fs: &dyn FileSystemOps,
    path: &Path,
    max_bytes: u64,
) -> Result<Option<LimitedRead>, TrustError> {
    let read = match fs.read_limited(path, max_bytes) {
        Ok(read) => read,
        Err(ReadLimitedError::NotFound) => return Ok(None),
        Err(ReadLimitedError::Symlink) => {
            return Err(TrustError::Unsafe("is a symbolic link".to_owned()));
        }
        Err(ReadLimitedError::NotRegular(kind)) => {
            return Err(TrustError::Unsafe(format!(
                "is not a regular file ({kind:?})"
            )));
        }
        Err(ReadLimitedError::TooLarge { limit }) => return Err(TrustError::TooLarge { limit }),
        Err(ReadLimitedError::Io(error)) => {
            return Err(TrustError::Io(InfraError::from_io("config.read", &error)));
        }
    };
    let euid = fs.effective_uid();
    if read.facts.owner_uid != euid {
        return Err(TrustError::Unsafe(format!(
            "is owned by uid {}, expected uid {euid}",
            read.facts.owner_uid
        )));
    }
    if read.facts.is_group_or_world_writable() {
        return Err(TrustError::Unsafe(format!(
            "is group/world-writable (mode {:o})",
            read.facts.permissions
        )));
    }
    Ok(Some(read))
}

/// What the configuration path holds right now.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum CurrentDocument {
    Missing,
    Present {
        config: Box<Config>,
        sha256: String,
        len: u64,
    },
}

pub(crate) fn display(path: &Path) -> String {
    path.display().to_string()
}

fn document_issue(message: String) -> ConfigError {
    ConfigError::Invalid(vec![ValidationIssue {
        path: "$".to_owned(),
        message,
    }])
}

/// Reads and fully validates the document at `path`.
pub(crate) fn read_current(
    fs: &dyn FileSystemOps,
    path: &Path,
) -> Result<CurrentDocument, ConfigLoadError> {
    let label = display(path);
    let read = match read_trusted(fs, path, MAX_CONFIG_FILE_BYTES) {
        Ok(Some(read)) => read,
        Ok(None) => return Ok(CurrentDocument::Missing),
        Err(TrustError::Unsafe(reason)) => {
            return Err(ConfigLoadError::UnsafePath(format!("{label} {reason}")));
        }
        Err(TrustError::TooLarge { limit }) => {
            return Err(ConfigLoadError::Invalid {
                path: label,
                error: document_issue(format!("config file exceeds {limit} bytes")),
            });
        }
        Err(TrustError::Io(error)) => return Err(ConfigLoadError::Io(error)),
    };
    let config = parse_document(&label, &read.bytes)?;
    Ok(CurrentDocument::Present {
        config: Box::new(config),
        sha256: sha256_hex(&read.bytes),
        len: read.facts.len,
    })
}

/// Parses a v2 document. A newer `schema_version` is detected before the
/// strict parse so it is reported as such (and never rewritten) even when
/// its fields no longer match this version.
pub(crate) fn parse_document(label: &str, bytes: &[u8]) -> Result<Config, ConfigLoadError> {
    let value: serde_json::Value =
        serde_json::from_slice(bytes).map_err(|error| ConfigLoadError::Invalid {
            path: label.to_owned(),
            error: ConfigError::Parse {
                line: error.line(),
                column: error.column(),
                message: sanitize_message(&error.to_string()),
            },
        })?;
    if let Some(found) = value
        .get("schema_version")
        .and_then(serde_json::Value::as_u64)
        && found > u64::from(CONFIG_SCHEMA_VERSION)
    {
        return Err(ConfigLoadError::FutureSchema {
            path: label.to_owned(),
            found,
        });
    }
    let text = std::str::from_utf8(bytes).map_err(|error| ConfigLoadError::Invalid {
        path: label.to_owned(),
        error: document_issue(format!("config file is not UTF-8: {error}")),
    })?;
    Config::from_json(text).map_err(|error| ConfigLoadError::Invalid {
        path: label.to_owned(),
        error,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use dgxtop_core::config::REFERENCE_CONFIG_JSON;

    #[test]
    fn future_schema_is_detected_even_with_unknown_fields() {
        let error =
            parse_document("mem", br#"{"schema_version": 3, "brand_new": {}}"#).unwrap_err();
        assert_eq!(
            error,
            ConfigLoadError::FutureSchema {
                path: "mem".to_owned(),
                found: 3
            }
        );
    }

    #[test]
    fn syntax_errors_carry_line_and_column() {
        let error = parse_document("mem", b"{\n  \"schema_version\": 2,\n  oops\n}").unwrap_err();
        match error {
            ConfigLoadError::Invalid {
                error: ConfigError::Parse { line, column, .. },
                ..
            } => {
                assert_eq!(line, 3);
                assert!(column > 0);
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn nan_and_unknown_keys_are_rejected() {
        let nan = REFERENCE_CONFIG_JSON.replace(
            "\"temperature_disagreement_deg_c\": 8.0",
            "\"temperature_disagreement_deg_c\": NaN",
        );
        assert_ne!(nan, REFERENCE_CONFIG_JSON);
        assert!(matches!(
            parse_document("mem", nan.as_bytes()),
            Err(ConfigLoadError::Invalid {
                error: ConfigError::Parse { .. },
                ..
            })
        ));
        let unknown = REFERENCE_CONFIG_JSON.replacen(
            "\"revision\": 1,",
            "\"revision\": 1, \"surprise\": true,",
            1,
        );
        assert_ne!(unknown, REFERENCE_CONFIG_JSON);
        assert!(matches!(
            parse_document("mem", unknown.as_bytes()),
            Err(ConfigLoadError::Invalid {
                error: ConfigError::Parse { .. },
                ..
            })
        ));
    }

    #[test]
    fn old_schema_versions_are_validation_errors() {
        let old =
            REFERENCE_CONFIG_JSON.replacen("\"schema_version\": 2", "\"schema_version\": 1", 1);
        assert!(matches!(
            parse_document("mem", old.as_bytes()),
            Err(ConfigLoadError::Invalid {
                error: ConfigError::Invalid(_),
                ..
            })
        ));
    }
}

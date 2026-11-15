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
    let Some(read) = read_document_bytes(fs, path)? else {
        return Ok(CurrentDocument::Missing);
    };
    let config = parse_document(&display(path), &read.bytes)?;
    Ok(CurrentDocument::Present {
        config: Box::new(config),
        sha256: sha256_hex(&read.bytes),
        len: read.facts.len,
    })
}

/// The bytes of the document at `path` under the trust rules of
/// [`read_trusted`], its refusals told as a load tells them; `Ok(None)`
/// when there is no file.
fn read_document_bytes(
    fs: &dyn FileSystemOps,
    path: &Path,
) -> Result<Option<LimitedRead>, ConfigLoadError> {
    let label = display(path);
    read_trusted(fs, path, MAX_CONFIG_FILE_BYTES).map_err(|refusal| match refusal {
        TrustError::Unsafe(reason) => ConfigLoadError::UnsafePath(format!("{label} {reason}")),
        TrustError::TooLarge { limit } => ConfigLoadError::Invalid {
            path: label,
            error: document_issue(format!("config file exceeds {limit} bytes")),
        },
        TrustError::Io(error) => ConfigLoadError::Io(error),
    })
}

/// Copies the document at `saved` to the new private file `copy`, for a
/// run that keeps a state of its own (D-140). The document is read as a
/// load reads it: a regular file of ours that nobody else can write, never
/// through a symbolic link, within the size limit; a file a load would
/// refuse is refused here, an invalid document with its own path and
/// place. `saved` and its directory are only read. `Ok(false)` when there
/// is no saved document, and nothing is written.
pub fn copy_trusted_document(
    fs: &dyn FileSystemOps,
    saved: &Path,
    copy: &Path,
) -> Result<bool, ConfigLoadError> {
    let Some(read) = read_document_bytes(fs, saved)? else {
        return Ok(false);
    };
    parse_document(&display(saved), &read.bytes)?;
    fs.create_private_file(copy)
        .and_then(|mut file| fs.write_all(&mut file, &read.bytes))
        .map(|()| true)
        .map_err(|error| ConfigLoadError::Io(InfraError::from_io("config.copy", &error)))
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
    use std::os::unix::fs::PermissionsExt;
    use std::path::PathBuf;

    use super::*;
    use crate::fs::RealFileSystem;
    use dgxtop_core::config::REFERENCE_CONFIG_JSON;

    /// A throw-away directory of this test.
    struct Scratch(PathBuf);

    impl Scratch {
        fn new(label: &str) -> Self {
            let root = std::env::temp_dir().join(format!(
                "dgxtop-store-document-{label}-{}",
                std::process::id()
            ));
            let _ = std::fs::remove_dir_all(&root);
            std::fs::create_dir_all(&root).unwrap();
            Self(root)
        }

        /// A saved document of mode `mode` holding `bytes`.
        fn saved(&self, bytes: &[u8], mode: u32) -> PathBuf {
            let path = self.0.join("saved.json");
            std::fs::write(&path, bytes).unwrap();
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(mode)).unwrap();
            path
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// A saved document is copied byte for byte into a file only its owner
    /// reads and writes, and is itself left as it was; without one nothing
    /// is written.
    #[test]
    fn a_trusted_document_is_copied_into_a_private_file() {
        let scratch = Scratch::new("copy");
        let copy = scratch.0.join("copy.json");
        assert_eq!(
            copy_trusted_document(&RealFileSystem, &scratch.0.join("saved.json"), &copy),
            Ok(false)
        );
        assert!(!copy.exists(), "no saved document: no copy");
        let saved = scratch.saved(REFERENCE_CONFIG_JSON.as_bytes(), 0o644);
        assert_eq!(
            copy_trusted_document(&RealFileSystem, &saved, &copy),
            Ok(true)
        );
        assert_eq!(
            std::fs::read(&copy).unwrap(),
            REFERENCE_CONFIG_JSON.as_bytes()
        );
        let mode = |path: &Path| std::fs::metadata(path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&copy), 0o600);
        assert_eq!(mode(&saved), 0o644);
        assert_eq!(
            std::fs::read(&saved).unwrap(),
            REFERENCE_CONFIG_JSON.as_bytes()
        );
        // The copy is a new file: one in its place is never overwritten.
        assert!(matches!(
            copy_trusted_document(&RealFileSystem, &saved, &copy),
            Err(ConfigLoadError::Io(_))
        ));
    }

    /// What a load refuses is not copied: a document others can write and
    /// one behind a symbolic link.
    #[test]
    fn an_untrusted_document_is_refused_and_not_copied() {
        let scratch = Scratch::new("refuse");
        let copy = scratch.0.join("copy.json");
        let writable = scratch.saved(b"{}", 0o666);
        assert!(matches!(
            copy_trusted_document(&RealFileSystem, &writable, &copy),
            Err(ConfigLoadError::UnsafePath(reason)) if reason.contains("writable")
        ));
        let link = scratch.0.join("link.json");
        std::os::unix::fs::symlink(scratch.saved(b"{}", 0o600), &link).unwrap();
        assert!(matches!(
            copy_trusted_document(&RealFileSystem, &link, &copy),
            Err(ConfigLoadError::UnsafePath(reason)) if reason.contains("symbolic link")
        ));
        assert!(!copy.exists());
        // An invalid document is told with the saved file's own path.
        let broken = scratch.saved(b"{\n  oops\n}", 0o600);
        match copy_trusted_document(&RealFileSystem, &broken, &copy) {
            Err(ConfigLoadError::Invalid { path, .. }) => {
                assert_eq!(path, broken.display().to_string());
            }
            other => panic!("unexpected {other:?}"),
        }
        assert!(!copy.exists());
    }

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

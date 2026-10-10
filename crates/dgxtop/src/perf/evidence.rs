//! Evidence directory of one bench report: every raw file is written once,
//! fingerprinted (SHA-256) and listed with a path relative to the
//! directory, which is what `tools/evaluate_performance.py --evidence-dir`
//! verifies.

use std::path::{Component, Path, PathBuf};

use serde::Serialize;

use super::EvidenceFile;
use super::report::BenchMode;

/// Largest number of evidence files the report schema accepts.
pub const MAX_EVIDENCE_FILES: usize = 64;

/// Where a bench writes its report and evidence.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReportPaths {
    pub report: PathBuf,
    pub evidence_dir: PathBuf,
}

/// `target/perf/<bench>-<mode>.json` (or `out`), with the evidence beside it
/// in a directory of the same stem.
pub fn report_paths(
    target_dir: &Path,
    bench: &str,
    mode: BenchMode,
    out: Option<&Path>,
) -> ReportPaths {
    let report = out.map_or_else(
        || {
            target_dir
                .join("perf")
                .join(format!("{bench}-{}.json", mode.label()))
        },
        Path::to_path_buf,
    );
    let evidence_dir = report.with_extension("");
    let evidence_dir = if evidence_dir == report {
        report.with_extension("evidence")
    } else {
        evidence_dir
    };
    ReportPaths {
        report,
        evidence_dir,
    }
}

/// Writes evidence files and remembers their fingerprints.
#[derive(Debug)]
pub struct EvidenceWriter {
    root: PathBuf,
    files: Vec<EvidenceFile>,
}

impl EvidenceWriter {
    pub fn create(root: &Path) -> std::io::Result<Self> {
        std::fs::create_dir_all(root)?;
        Ok(Self {
            root: root.to_path_buf(),
            files: Vec::new(),
        })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Writes `bytes` at `relative` (a plain relative path without `..`).
    pub fn write_bytes(&mut self, relative: &str, bytes: &[u8]) -> std::io::Result<()> {
        let path = self.checked_path(relative)?;
        if self.files.len() >= MAX_EVIDENCE_FILES
            && !self.files.iter().any(|file| file.path == relative)
        {
            return Err(std::io::Error::other(format!(
                "more than {MAX_EVIDENCE_FILES} evidence files"
            )));
        }
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(&path, bytes)?;
        let sha256 = dgxtop_core::digest::sha256_hex(bytes);
        self.files.retain(|file| file.path != relative);
        self.files.push(EvidenceFile {
            path: relative.to_owned(),
            sha256,
        });
        Ok(())
    }

    /// Pretty JSON evidence file.
    pub fn write_json<T: Serialize>(&mut self, relative: &str, value: &T) -> std::io::Result<()> {
        let mut bytes = serde_json::to_vec_pretty(value).map_err(std::io::Error::other)?;
        bytes.push(b'\n');
        self.write_bytes(relative, &bytes)
    }

    pub fn files(&self) -> &[EvidenceFile] {
        &self.files
    }

    fn checked_path(&self, relative: &str) -> std::io::Result<PathBuf> {
        let candidate = Path::new(relative);
        let plain = !relative.is_empty()
            && candidate
                .components()
                .all(|component| matches!(component, Component::Normal(_)));
        if plain {
            Ok(self.root.join(candidate))
        } else {
            Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                format!("evidence path {relative:?} must be relative without '..'"),
            ))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!("dgxtop-evidence-{name}-{}", std::process::id()))
    }

    #[test]
    fn files_are_fingerprinted_and_confined() {
        let root = scratch("confined");
        let mut writer = EvidenceWriter::create(&root).unwrap();
        writer.write_bytes("run1/raw.txt", b"abc").unwrap();
        writer
            .write_json("summary.json", &serde_json::json!({"x": 1}))
            .unwrap();
        writer.write_bytes("run1/raw.txt", b"abcd").unwrap();
        assert_eq!(writer.files().len(), 2, "rewrites replace the entry");
        let raw = writer
            .files()
            .iter()
            .find(|file| file.path == "run1/raw.txt")
            .unwrap();
        assert_eq!(raw.sha256, dgxtop_core::digest::sha256_hex(b"abcd"));
        assert_eq!(std::fs::read(root.join("run1/raw.txt")).unwrap(), b"abcd");
        for bad in ["../escape", "/abs", "", "a/../b"] {
            assert!(writer.write_bytes(bad, b"x").is_err(), "{bad}");
        }
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn evidence_count_is_bounded_by_the_schema() {
        let root = scratch("bounded");
        let mut writer = EvidenceWriter::create(&root).unwrap();
        for index in 0..MAX_EVIDENCE_FILES {
            writer.write_bytes(&format!("f{index}"), b"x").unwrap();
        }
        assert!(writer.write_bytes("one-too-many", b"x").is_err());
        assert!(
            writer.write_bytes("f0", b"y").is_ok(),
            "rewriting stays allowed"
        );
        std::fs::remove_dir_all(&root).unwrap();
    }
}

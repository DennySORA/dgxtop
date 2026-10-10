//! The NVIDIA software stack beside the driver library (D-48), read from
//! files only: no command runs and nothing leaves the host. The kernel
//! module's flavor and compiler come from `/proc/driver/nvidia/version`;
//! the CUDA toolkits under `/usr/local` (and `/opt/cuda`) with their
//! `nvcc` from each toolkit's `version.json` (CUDA 11.1 and later) or
//! `version.txt`. `/usr/local/cuda` names the default toolkit.

use std::path::{Path, PathBuf};

/// Driver version file of the kernel module.
const KERNEL_VERSION_FILE: &str = "/proc/driver/nvidia/version";
/// Where toolkits are installed.
const TOOLKIT_PARENT: &str = "/usr/local";
const OPT_TOOLKIT: &str = "/opt/cuda";
/// The default toolkit's link.
const DEFAULT_TOOLKIT: &str = "/usr/local/cuda";
/// Bounds: version files are a few KiB, a host has a few toolkits.
const MAX_FILE_BYTES: u64 = 64 * 1024;
const MAX_ENTRIES: usize = 256;
pub const MAX_TOOLKITS: usize = 8;

/// Read access to the files the stack is described by.
pub trait StackFiles: Send {
    /// The file's text when it exists and is at most `limit` bytes.
    fn read_text(&self, path: &Path, limit: u64) -> Option<String>;
    /// Names of at most `limit` entries of the directory `path`.
    fn entries(&self, path: &Path, limit: usize) -> Vec<String>;
    fn is_file(&self, path: &Path) -> bool;
    fn is_dir(&self, path: &Path) -> bool;
    /// Target of the symbolic link `path`, as stored.
    fn link_target(&self, path: &Path) -> Option<PathBuf>;
}

/// The host's file system.
#[derive(Debug, Default, Clone, Copy)]
pub struct SystemFiles;

impl StackFiles for SystemFiles {
    fn read_text(&self, path: &Path, limit: u64) -> Option<String> {
        let metadata = std::fs::metadata(path).ok()?;
        if !metadata.is_file() || metadata.len() > limit {
            return None;
        }
        std::fs::read_to_string(path).ok()
    }

    fn entries(&self, path: &Path, limit: usize) -> Vec<String> {
        std::fs::read_dir(path)
            .map(|entries| {
                entries
                    .filter_map(Result::ok)
                    .take(limit)
                    .filter_map(|entry| entry.file_name().into_string().ok())
                    .collect()
            })
            .unwrap_or_default()
    }

    fn is_file(&self, path: &Path) -> bool {
        path.is_file()
    }

    fn is_dir(&self, path: &Path) -> bool {
        path.is_dir()
    }

    fn link_target(&self, path: &Path) -> Option<PathBuf> {
        std::fs::read_link(path).ok()
    }
}

/// No files: what an adapter on a fake backend sees.
#[derive(Debug, Default, Clone, Copy)]
pub struct NoFiles;

impl StackFiles for NoFiles {
    fn read_text(&self, _path: &Path, _limit: u64) -> Option<String> {
        None
    }

    fn entries(&self, _path: &Path, _limit: usize) -> Vec<String> {
        Vec::new()
    }

    fn is_file(&self, _path: &Path) -> bool {
        false
    }

    fn is_dir(&self, _path: &Path) -> bool {
        false
    }

    fn link_target(&self, _path: &Path) -> Option<PathBuf> {
        None
    }
}

/// The stack found; empty when nothing was.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct NvidiaStack {
    /// `open · gcc 14.2.0`: the kernel module's flavor and compiler.
    pub kernel_module: Option<String>,
    /// The default toolkit first, then the newest.
    pub toolkits: Vec<CudaToolkit>,
}

/// One installed CUDA toolkit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CudaToolkit {
    pub path: String,
    /// `13.4`
    pub version: String,
    /// `13.4.92`, when the toolkit has `bin/nvcc`.
    pub nvcc: Option<String>,
    /// `/usr/local/cuda` points here.
    pub default: bool,
}

/// Reads the stack through `files`.
pub fn read_stack(files: &dyn StackFiles) -> NvidiaStack {
    NvidiaStack {
        kernel_module: files
            .read_text(Path::new(KERNEL_VERSION_FILE), MAX_FILE_BYTES)
            .and_then(|text| kernel_module(&text)),
        toolkits: toolkits(files),
    }
}

/// `open · gcc 14.2.0` from the kernel module's version file.
fn kernel_module(text: &str) -> Option<String> {
    let banner = text
        .lines()
        .find(|line| line.starts_with("NVRM version:"))?;
    let flavor = if banner.contains("Open Kernel Module") {
        "open"
    } else if banner.contains("Kernel Module") {
        "proprietary"
    } else {
        return None;
    };
    let compiler = text
        .lines()
        .find_map(|line| line.split_once("gcc version "))
        .and_then(|(_, rest)| rest.split_whitespace().next())
        .map(|version| format!("gcc {version}"));
    Some(match compiler {
        Some(compiler) => format!("{flavor} · {compiler}"),
        None => flavor.to_owned(),
    })
}

/// Toolkit directories: `/usr/local/cuda*` and `/opt/cuda`, each once.
fn toolkits(files: &dyn StackFiles) -> Vec<CudaToolkit> {
    let parent = Path::new(TOOLKIT_PARENT);
    let default_target = files
        .link_target(Path::new(DEFAULT_TOOLKIT))
        .map(|target| normalized(&parent.join(target)))
        .or_else(|| {
            files
                .is_dir(Path::new(DEFAULT_TOOLKIT))
                .then(|| DEFAULT_TOOLKIT.to_owned())
        });
    let mut candidates: Vec<String> = files
        .entries(parent, MAX_ENTRIES)
        .into_iter()
        .filter(|name| name.starts_with("cuda"))
        .map(|name| normalized(&parent.join(name)))
        .filter(|path| path != DEFAULT_TOOLKIT || default_target.as_deref() == Some(path))
        .collect();
    candidates.push(OPT_TOOLKIT.to_owned());
    candidates.sort();
    candidates.dedup();
    let mut found: Vec<CudaToolkit> = candidates
        .iter()
        .filter_map(|path| toolkit(files, path, default_target.as_deref() == Some(path)))
        .collect();
    found.sort_by(|left, right| {
        right
            .default
            .cmp(&left.default)
            .then_with(|| version_key(&right.version).cmp(&version_key(&left.version)))
    });
    found.truncate(MAX_TOOLKITS);
    found
}

/// A toolkit at `path` when its version file names one.
fn toolkit(files: &dyn StackFiles, path: &str, default: bool) -> Option<CudaToolkit> {
    let root = Path::new(path);
    let (full, nvcc) = files
        .read_text(&root.join("version.json"), MAX_FILE_BYTES)
        .and_then(|text| versions_from_json(&text))
        .or_else(|| {
            files
                .read_text(&root.join("version.txt"), MAX_FILE_BYTES)
                .and_then(|text| version_from_text(&text))
                .map(|full| (full.clone(), Some(full)))
        })?;
    let has_nvcc = files.is_file(&root.join("bin").join("nvcc"));
    Some(CudaToolkit {
        path: path.to_owned(),
        version: major_minor(&full),
        nvcc: nvcc.filter(|_| has_nvcc),
        default,
    })
}

/// The toolkit's and nvcc's versions from `version.json`.
fn versions_from_json(text: &str) -> Option<(String, Option<String>)> {
    let json: serde_json::Value = serde_json::from_str(text).ok()?;
    let version = |component: &str| {
        json.get(component)?
            .get("version")?
            .as_str()
            .map(str::to_owned)
    };
    Some((version("cuda")?, version("cuda_nvcc")))
}

/// `11.0.228` from `CUDA Version 11.0.228`.
fn version_from_text(text: &str) -> Option<String> {
    text.lines()
        .find_map(|line| line.trim().strip_prefix("CUDA Version "))
        .map(|version| version.trim().to_owned())
        .filter(|version| !version.is_empty())
}

/// `13.4` of `13.4.20260911`.
fn major_minor(version: &str) -> String {
    version.split('.').take(2).collect::<Vec<_>>().join(".")
}

/// Numeric components, so `13.10` sorts after `13.4`.
fn version_key(version: &str) -> Vec<u64> {
    version
        .split('.')
        .map(|part| part.parse().unwrap_or(0))
        .collect()
}

/// `path` without `.` components or a trailing slash.
fn normalized(path: &Path) -> String {
    let mut parts: Vec<String> = Vec::new();
    for component in path.components() {
        match component {
            std::path::Component::RootDir => parts.clear(),
            std::path::Component::Normal(part) => parts.push(part.to_string_lossy().into_owned()),
            std::path::Component::ParentDir => {
                parts.pop();
            }
            std::path::Component::CurDir | std::path::Component::Prefix(_) => {}
        }
    }
    format!("/{}", parts.join("/"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    /// An in-memory file tree.
    #[derive(Default)]
    struct Tree {
        files: BTreeMap<String, String>,
        links: BTreeMap<String, String>,
    }

    impl Tree {
        fn file(mut self, path: &str, text: &str) -> Self {
            self.files.insert(path.to_owned(), text.to_owned());
            self
        }
    }

    impl StackFiles for Tree {
        fn read_text(&self, path: &Path, limit: u64) -> Option<String> {
            let text = self.files.get(path.to_str()?)?;
            (u64::try_from(text.len()).ok()? <= limit).then(|| text.clone())
        }

        fn entries(&self, path: &Path, limit: usize) -> Vec<String> {
            let prefix = format!("{}/", path.to_str().unwrap_or_default());
            let mut names: Vec<String> = self
                .files
                .keys()
                .chain(self.links.keys())
                .filter_map(|key| key.strip_prefix(&prefix))
                .map(|rest| rest.split('/').next().unwrap_or(rest).to_owned())
                .collect();
            names.sort();
            names.dedup();
            names.truncate(limit);
            names
        }

        fn is_file(&self, path: &Path) -> bool {
            path.to_str()
                .is_some_and(|path| self.files.contains_key(path))
        }

        fn is_dir(&self, path: &Path) -> bool {
            let prefix = format!("{}/", path.to_str().unwrap_or_default());
            self.files.keys().any(|key| key.starts_with(&prefix))
        }

        fn link_target(&self, path: &Path) -> Option<PathBuf> {
            self.links.get(path.to_str()?).map(PathBuf::from)
        }
    }

    const KERNEL: &str = "NVRM version: NVIDIA UNIX Open Kernel Module for x86_64  595.104.02  \
                          Release Build  (dvs-builder@U22-I3-H06-02-6)  Thu Sep 17 2026\n\
                          GCC version:  gcc version 14.2.0 (Debian 14.2.0-19) \n";

    #[test]
    fn the_kernel_module_names_its_flavor_and_compiler() {
        assert_eq!(kernel_module(KERNEL).as_deref(), Some("open · gcc 14.2.0"));
        let proprietary = "NVRM version: NVIDIA UNIX x86_64 Kernel Module  550.54.14  Release\n";
        assert_eq!(kernel_module(proprietary).as_deref(), Some("proprietary"));
        assert_eq!(kernel_module("no driver here"), None);
    }

    #[test]
    fn nothing_installed_is_an_empty_stack() {
        assert_eq!(read_stack(&NoFiles), NvidiaStack::default());
        let broken = Tree::default().file("/usr/local/cuda-13.4/version.json", "{not json");
        assert!(read_stack(&broken).toolkits.is_empty());
        assert_eq!(version_key("13.10"), vec![13, 10]);
        assert!(version_key("13.10") > version_key("13.4"));
        assert_eq!(
            normalized(Path::new("/usr/local/./cuda-13.4/")),
            "/usr/local/cuda-13.4"
        );
    }
}

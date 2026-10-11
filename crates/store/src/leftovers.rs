//! Files in the state directory that dgxtop no longer needs (D-85): the
//! backups older builds wrote before a migration, `-wal`/`-shm` journals
//! whose database is gone, and temporary files of an interrupted
//! short-term write. Nothing reads them, so they are found by name here and
//! deleted at startup by the composition, under the instance lock.
//!
//! Only names derived from [`StatePaths`] are matched, only in its state
//! directory and only while that directory is private; a symbolic link is
//! removed as a link, never followed, and directories or FIFOs are left
//! alone.

use std::ffi::OsString;
use std::io;
use std::path::{Path, PathBuf};

use crate::fs::{FileKind, FileSystemOps};
use crate::paths::{StatePaths, verify_private_directory};

/// The most names one look at the state directory reads.
const MAX_ENTRIES: usize = 4096;

/// Why a file is not needed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LeftoverKind {
    /// `<database>.pre-v<N>-<time>-<id>.bak`, written before a migration by
    /// builds older than D-85.
    MigrationBackup,
    /// `<database>-wal` or `-shm` without its database.
    OrphanJournal,
    /// `.<short-term file>.<id>` left by a write that never renamed it.
    InterruptedWrite,
}

impl LeftoverKind {
    pub const fn reason(self) -> &'static str {
        match self {
            Self::MigrationBackup => "a migration backup of an older build",
            Self::OrphanJournal => "a journal without its database",
            Self::InterruptedWrite => "the temporary file of an interrupted write",
        }
    }
}

/// One file to delete.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Leftover {
    pub path: PathBuf,
    pub kind: LeftoverKind,
}

/// The leftovers in the state directory, sorted by path.
pub fn find_leftovers(fs: &dyn FileSystemOps, paths: &StatePaths) -> io::Result<Vec<Leftover>> {
    verify_private_directory(fs, &paths.state_dir).map_err(io::Error::other)?;
    let names = fs.list_directory(&paths.state_dir, MAX_ENTRIES)?;
    let rules = NameRules::of(paths);
    let mut found: Vec<Leftover> = names
        .iter()
        .filter_map(|name| {
            let kind =
                rules.kind_of(name, |database| exists(fs, &paths.state_dir.join(database)))?;
            let path = paths.state_dir.join(name);
            removable(fs, &path).then_some(Leftover { path, kind })
        })
        .collect();
    found.sort_by(|left, right| left.path.cmp(&right.path));
    Ok(found)
}

/// Deletes one leftover (a link as a link); one already gone is no error.
pub fn remove_leftover(fs: &dyn FileSystemOps, leftover: &Leftover) -> io::Result<()> {
    match fs.remove_file(&leftover.path) {
        Err(error) if error.kind() != io::ErrorKind::NotFound => Err(error),
        _ => Ok(()),
    }
}

fn exists(fs: &dyn FileSystemOps, path: &Path) -> bool {
    !matches!(fs.symlink_metadata(path), Err(error) if error.kind() == io::ErrorKind::NotFound)
}

fn removable(fs: &dyn FileSystemOps, path: &Path) -> bool {
    fs.symlink_metadata(path)
        .is_ok_and(|facts| matches!(facts.kind, FileKind::Regular | FileKind::Symlink))
}

/// The file names the rules are made of.
struct NameRules {
    databases: Vec<String>,
    short_term: String,
}

impl NameRules {
    fn of(paths: &StatePaths) -> Self {
        let name = |path: &Path| {
            path.file_name()
                .map(|name| name.to_string_lossy().into_owned())
                .unwrap_or_default()
        };
        Self {
            databases: [
                &paths.audit_database,
                &paths.history_database,
                &paths.history_tiers_database,
                &paths.process_history_database,
            ]
            .into_iter()
            .map(|path| name(path))
            .collect(),
            short_term: name(&paths.process_recent_file),
        }
    }

    fn kind_of(
        &self,
        name: &OsString,
        database_exists: impl Fn(&str) -> bool,
    ) -> Option<LeftoverKind> {
        let name = name.to_str()?;
        if name.starts_with(&format!(".{}.", self.short_term)) {
            return Some(LeftoverKind::InterruptedWrite);
        }
        self.databases.iter().find_map(|database| {
            let rest = name.strip_prefix(database.as_str())?;
            if rest.starts_with(".pre-v") && rest.ends_with(".bak") {
                Some(LeftoverKind::MigrationBackup)
            } else if (rest == "-wal" || rest == "-shm") && !database_exists(database) {
                Some(LeftoverKind::OrphanJournal)
            } else {
                None
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::PermissionsExt;

    use dgxtop_core::quality::Namespace;

    use super::*;
    use crate::fs::RealFileSystem;
    use crate::paths::MapEnv;

    struct Scratch(PathBuf);

    impl Scratch {
        fn new(label: &str) -> Self {
            let dir = std::env::temp_dir().join(format!(
                "dgxtop-leftovers-{label}-{}-{}",
                std::process::id(),
                uuid::Uuid::new_v4().simple()
            ));
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).unwrap();
            Self(dir)
        }

        fn paths(&self) -> StatePaths {
            let env = MapEnv::new().with("HOME", "/home/acme");
            let mut paths =
                StatePaths::resolve(&env, None, Some(&self.0), Namespace::Live).unwrap();
            // The live namespace adds no subdirectory to an override.
            assert_eq!(paths.state_dir, self.0);
            paths.notes.clear();
            paths
        }

        fn touch(&self, name: &str) -> PathBuf {
            let path = self.0.join(name);
            std::fs::write(&path, b"x").unwrap();
            path
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::set_permissions(&self.0, std::fs::Permissions::from_mode(0o700));
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn kinds(found: &[Leftover]) -> Vec<(String, LeftoverKind)> {
        found
            .iter()
            .map(|leftover| {
                let name = leftover.path.file_name().unwrap().to_string_lossy();
                (name.into_owned(), leftover.kind)
            })
            .collect()
    }

    /// Backups, orphan journals and interrupted writes are found; the
    /// databases, their live journals, the short-term file, the log, the
    /// lock and files of others are not.
    #[test]
    fn only_files_dgxtop_no_longer_needs_are_found() {
        let scratch = Scratch::new("find");
        for name in [
            "audit.sqlite3",
            "audit.sqlite3-wal",
            "audit.sqlite3-shm",
            "history.sqlite3",
            "history.sqlite3-wal",
            "process_recent.json",
            "dgxtop.log",
            "instance.lock",
            "notes.txt",
            "history.sqlite3.bak",
            "process_history.sqlite3-journal",
            "audit.sqlite3.pre-v2-20260101T000000Z-1a2b3c4d.bak",
            "process_history.sqlite3.pre-v3-20260101T000000Z-5e6f7a8b.bak",
            "history_tiers.sqlite3-wal",
            "process_history.sqlite3-shm",
            ".process_recent.json.0123abcd",
        ] {
            scratch.touch(name);
        }
        let found = find_leftovers(&RealFileSystem, &scratch.paths()).unwrap();
        assert_eq!(
            kinds(&found),
            vec![
                (
                    ".process_recent.json.0123abcd".to_owned(),
                    LeftoverKind::InterruptedWrite
                ),
                (
                    "audit.sqlite3.pre-v2-20260101T000000Z-1a2b3c4d.bak".to_owned(),
                    LeftoverKind::MigrationBackup
                ),
                (
                    "history_tiers.sqlite3-wal".to_owned(),
                    LeftoverKind::OrphanJournal
                ),
                (
                    "process_history.sqlite3-shm".to_owned(),
                    LeftoverKind::OrphanJournal
                ),
                (
                    "process_history.sqlite3.pre-v3-20260101T000000Z-5e6f7a8b.bak".to_owned(),
                    LeftoverKind::MigrationBackup
                ),
            ]
        );
        for leftover in &found {
            remove_leftover(&RealFileSystem, leftover).unwrap();
            assert!(!leftover.path.exists());
            // Removing it again is no error.
            remove_leftover(&RealFileSystem, leftover).unwrap();
        }
        assert!(scratch.0.join("notes.txt").exists());
        assert!(scratch.0.join("audit.sqlite3-wal").exists());
    }

    /// A link is removed as a link, its target stays; a directory with a
    /// leftover's name is not touched.
    #[test]
    fn links_are_removed_as_links_and_directories_are_left() {
        let scratch = Scratch::new("links");
        let outside = Scratch::new("outside");
        let target = outside.touch("precious.bin");
        let link = scratch.0.join("history.sqlite3.pre-v2-x-y.bak");
        std::os::unix::fs::symlink(&target, &link).unwrap();
        std::fs::create_dir(scratch.0.join("audit.sqlite3-wal")).unwrap();
        let found = find_leftovers(&RealFileSystem, &scratch.paths()).unwrap();
        assert_eq!(found.len(), 1);
        remove_leftover(&RealFileSystem, &found[0]).unwrap();
        assert!(std::fs::symlink_metadata(&link).is_err());
        assert_eq!(std::fs::read(&target).unwrap(), b"x");
        assert!(scratch.0.join("audit.sqlite3-wal").is_dir());
    }

    /// Nothing is looked for in a state directory others may write.
    #[test]
    fn a_state_directory_that_is_not_private_is_refused() {
        let scratch = Scratch::new("open");
        scratch.touch("audit.sqlite3.pre-v2-x-y.bak");
        std::fs::set_permissions(&scratch.0, std::fs::Permissions::from_mode(0o777)).unwrap();
        assert!(find_leftovers(&RealFileSystem, &scratch.paths()).is_err());
        assert!(scratch.0.join("audit.sqlite3.pre-v2-x-y.bak").exists());
    }
}

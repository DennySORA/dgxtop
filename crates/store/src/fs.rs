//! Filesystem operations needed by the persistence layer.
//!
//! Every disk touch of a configuration commit goes through [`FileSystemOps`]
//! so that tests can inject `ENOSPC`, a failed rename or a failed directory
//! fsync *after* a successful rename. That is how the repository proves that a
//! commit which did not durably complete never reports success.
//!
//! [`RealFileSystem`] creates files with `O_CREAT|O_EXCL|O_NOFOLLOW|O_CLOEXEC`
//! and mode 0600, directories with mode 0700, and never follows a symlink in
//! the final path component when reading or creating.

mod failpoint;

use std::ffi::OsString;
use std::fmt;
use std::fs::{File, Metadata, OpenOptions, Permissions};
use std::io::{self, Read, Write};
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

use nix::errno::Errno;
use nix::fcntl::{Flock, FlockArg};
use thiserror::Error;

pub use failpoint::{Failpoint, FailpointFileSystem, Trigger};

/// Mode of every file the persistence layer creates.
pub const PRIVATE_FILE_MODE: u32 = 0o600;
/// Mode of every directory the persistence layer creates.
pub const PRIVATE_DIR_MODE: u32 = 0o700;
/// Permission bits that let another user modify a file or directory.
pub const GROUP_OR_WORLD_WRITABLE: u32 = 0o022;

/// What a path points to, observed without following a final symlink.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum FileKind {
    Regular,
    Directory,
    Symlink,
    /// FIFO, socket or device: never read or written by persistence.
    Other,
}

/// Ownership and permission facts used by the safety checks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FileFacts {
    pub kind: FileKind,
    pub owner_uid: u32,
    /// Permission bits only (`mode & 0o7777`).
    pub permissions: u32,
    pub len: u64,
}

impl FileFacts {
    /// Extracts the facts from metadata obtained by `lstat`/`fstat`.
    pub fn from_metadata(metadata: &Metadata) -> Self {
        let file_type = metadata.file_type();
        let kind = if file_type.is_symlink() {
            FileKind::Symlink
        } else if file_type.is_dir() {
            FileKind::Directory
        } else if file_type.is_file() {
            FileKind::Regular
        } else {
            FileKind::Other
        };
        Self {
            kind,
            owner_uid: metadata.uid(),
            permissions: metadata.mode() & 0o7777,
            len: metadata.len(),
        }
    }

    pub const fn is_group_or_world_writable(&self) -> bool {
        self.permissions & GROUP_OR_WORLD_WRITABLE != 0
    }
}

/// A file created exclusively by [`FileSystemOps::create_private_file`].
pub struct PrivateFile {
    file: File,
    path: PathBuf,
}

impl PrivateFile {
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Borrow of the underlying descriptor for operations outside the trait
    /// (for example letting SQLite write into a pre-created file).
    pub fn as_file(&self) -> &File {
        &self.file
    }
}

impl fmt::Debug for PrivateFile {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("PrivateFile")
            .field("path", &self.path)
            .finish_non_exhaustive()
    }
}

/// Bytes of a bounded read plus the `fstat` facts of the descriptor that was
/// read (so ownership checks and the read cannot race against each other).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LimitedRead {
    pub bytes: Vec<u8>,
    pub facts: FileFacts,
}

/// Why [`FileSystemOps::read_limited`] returned no bytes.
#[derive(Debug, Error)]
pub enum ReadLimitedError {
    #[error("file not found")]
    NotFound,
    #[error("refusing to follow a symbolic link")]
    Symlink,
    #[error("not a regular file ({0:?})")]
    NotRegular(FileKind),
    #[error("file exceeds the {limit} byte limit")]
    TooLarge { limit: u64 },
    #[error(transparent)]
    Io(io::Error),
}

/// Exclusive advisory lock on a directory; released when dropped.
#[derive(Debug)]
pub struct DirectoryLock {
    _lock: Flock<File>,
}

/// Filesystem operations required by the configuration repository, the path
/// checks and the SQLite pre-creation and recreation steps.
pub trait FileSystemOps: Send + Sync + fmt::Debug {
    /// Creates `path` with `O_CREAT|O_EXCL|O_NOFOLLOW|O_CLOEXEC`, mode 0600.
    fn create_private_file(&self, path: &Path) -> io::Result<PrivateFile>;
    fn write_all(&self, file: &mut PrivateFile, bytes: &[u8]) -> io::Result<()>;
    /// `fsync` of the file data and metadata.
    fn sync_file(&self, file: &PrivateFile) -> io::Result<()>;
    /// Atomic replace of `to` by `from` (same directory).
    fn rename(&self, from: &Path, to: &Path) -> io::Result<()>;
    /// Publishes `existing` under `new_path` only if `new_path` does not exist
    /// (hard link; fails with `AlreadyExists` otherwise).
    fn link_new(&self, existing: &Path, new_path: &Path) -> io::Result<()>;
    /// `fsync` of a directory so a rename inside it becomes durable.
    fn sync_directory(&self, directory: &Path) -> io::Result<()>;
    /// Reads at most `max_bytes` from a regular file without following a
    /// final symlink and without blocking on FIFOs.
    fn read_limited(&self, path: &Path, max_bytes: u64) -> Result<LimitedRead, ReadLimitedError>;
    /// `lstat`: facts about the path itself, never its symlink target.
    fn symlink_metadata(&self, path: &Path) -> io::Result<FileFacts>;
    /// At most `max_entries` names of `directory` (unordered).
    fn list_directory(&self, directory: &Path, max_entries: usize) -> io::Result<Vec<OsString>>;
    fn remove_file(&self, path: &Path) -> io::Result<()>;
    /// Creates one directory with mode exactly 0700 (independent of umask).
    fn create_private_dir(&self, path: &Path) -> io::Result<()>;
    /// Non-blocking exclusive `flock` on a directory; `Ok(None)` when another
    /// holder has it.
    fn try_lock_directory(&self, directory: &Path) -> io::Result<Option<DirectoryLock>>;
    /// Effective uid that must own every file and directory we trust.
    fn effective_uid(&self) -> u32;
}

/// The real Linux filesystem.
#[derive(Debug, Clone, Copy, Default)]
pub struct RealFileSystem;

impl RealFileSystem {
    pub const fn new() -> Self {
        Self
    }
}

fn open_directory_no_follow(directory: &Path) -> io::Result<File> {
    OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(directory)
}

fn classify_open_error(error: io::Error) -> ReadLimitedError {
    if error.raw_os_error() == Some(libc::ELOOP) {
        ReadLimitedError::Symlink
    } else if error.kind() == io::ErrorKind::NotFound {
        ReadLimitedError::NotFound
    } else {
        ReadLimitedError::Io(error)
    }
}

fn byte_count(bytes: &[u8]) -> u64 {
    u64::try_from(bytes.len()).unwrap_or(u64::MAX)
}

impl FileSystemOps for RealFileSystem {
    fn create_private_file(&self, path: &Path) -> io::Result<PrivateFile> {
        let file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(PRIVATE_FILE_MODE)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(path)?;
        // The umask can only remove bits; normalize to exactly 0600 so the
        // owner can always write and nobody else can read.
        file.set_permissions(Permissions::from_mode(PRIVATE_FILE_MODE))?;
        Ok(PrivateFile {
            file,
            path: path.to_path_buf(),
        })
    }

    fn write_all(&self, file: &mut PrivateFile, bytes: &[u8]) -> io::Result<()> {
        file.file.write_all(bytes)
    }

    fn sync_file(&self, file: &PrivateFile) -> io::Result<()> {
        file.file.sync_all()
    }

    fn rename(&self, from: &Path, to: &Path) -> io::Result<()> {
        std::fs::rename(from, to)
    }

    fn link_new(&self, existing: &Path, new_path: &Path) -> io::Result<()> {
        std::fs::hard_link(existing, new_path)
    }

    fn sync_directory(&self, directory: &Path) -> io::Result<()> {
        open_directory_no_follow(directory)?.sync_all()
    }

    fn read_limited(&self, path: &Path, max_bytes: u64) -> Result<LimitedRead, ReadLimitedError> {
        let file = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC)
            .open(path)
            .map_err(classify_open_error)?;
        let metadata = file.metadata().map_err(ReadLimitedError::Io)?;
        let facts = FileFacts::from_metadata(&metadata);
        if facts.kind != FileKind::Regular {
            return Err(ReadLimitedError::NotRegular(facts.kind));
        }
        if facts.len > max_bytes {
            return Err(ReadLimitedError::TooLarge { limit: max_bytes });
        }
        let mut bytes = Vec::with_capacity(usize::try_from(facts.len).unwrap_or(0));
        // Read one byte past the limit so a file that grew after fstat is
        // still rejected instead of silently truncated.
        file.take(max_bytes.saturating_add(1))
            .read_to_end(&mut bytes)
            .map_err(ReadLimitedError::Io)?;
        if byte_count(&bytes) > max_bytes {
            return Err(ReadLimitedError::TooLarge { limit: max_bytes });
        }
        Ok(LimitedRead { bytes, facts })
    }

    fn symlink_metadata(&self, path: &Path) -> io::Result<FileFacts> {
        std::fs::symlink_metadata(path).map(|metadata| FileFacts::from_metadata(&metadata))
    }

    fn list_directory(&self, directory: &Path, max_entries: usize) -> io::Result<Vec<OsString>> {
        std::fs::read_dir(directory)?
            .take(max_entries)
            .map(|entry| entry.map(|entry| entry.file_name()))
            .collect()
    }

    fn remove_file(&self, path: &Path) -> io::Result<()> {
        std::fs::remove_file(path)
    }

    fn create_private_dir(&self, path: &Path) -> io::Result<()> {
        std::fs::DirBuilder::new()
            .mode(PRIVATE_DIR_MODE)
            .create(path)?;
        open_directory_no_follow(path)?.set_permissions(Permissions::from_mode(PRIVATE_DIR_MODE))
    }

    fn try_lock_directory(&self, directory: &Path) -> io::Result<Option<DirectoryLock>> {
        let handle = open_directory_no_follow(directory)?;
        match Flock::lock(handle, FlockArg::LockExclusiveNonblock) {
            Ok(lock) => Ok(Some(DirectoryLock { _lock: lock })),
            Err((_, Errno::EWOULDBLOCK)) => Ok(None),
            Err((_, errno)) => Err(io::Error::from(errno)),
        }
    }

    fn effective_uid(&self) -> u32 {
        nix::unistd::geteuid().as_raw()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(label: &str) -> PathBuf {
        let path = std::env::temp_dir().join(format!(
            "dgxtop-fs-unit-{}-{label}-{}",
            std::process::id(),
            uuid::Uuid::new_v4().simple()
        ));
        std::fs::create_dir_all(&path).unwrap();
        path
    }

    #[test]
    fn private_file_is_exclusive_and_0600() {
        let dir = temp_dir("exclusive");
        let path = dir.join("file");
        let fs = RealFileSystem::new();
        let mut file = fs.create_private_file(&path).unwrap();
        fs.write_all(&mut file, b"hello").unwrap();
        fs.sync_file(&file).unwrap();
        let facts = fs.symlink_metadata(&path).unwrap();
        assert_eq!(facts.kind, FileKind::Regular);
        assert_eq!(facts.permissions, PRIVATE_FILE_MODE);
        assert_eq!(facts.owner_uid, fs.effective_uid());
        let second = fs.create_private_file(&path).unwrap_err();
        assert_eq!(second.kind(), io::ErrorKind::AlreadyExists);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn create_refuses_a_planted_symlink() {
        let dir = temp_dir("planted");
        let target = dir.join("target");
        std::fs::write(&target, b"victim").unwrap();
        let link = dir.join("link");
        std::os::unix::fs::symlink(&target, &link).unwrap();
        let error = RealFileSystem.create_private_file(&link).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::AlreadyExists);
        assert_eq!(std::fs::read(&target).unwrap(), b"victim");
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn read_limited_enforces_kind_symlink_and_size() {
        let dir = temp_dir("read");
        let fs = RealFileSystem;
        let file = dir.join("data");
        std::fs::write(&file, b"0123456789").unwrap();
        let read = fs.read_limited(&file, 10).unwrap();
        assert_eq!(read.bytes, b"0123456789");
        assert_eq!(read.facts.len, 10);
        assert!(matches!(
            fs.read_limited(&file, 9),
            Err(ReadLimitedError::TooLarge { limit: 9 })
        ));
        let link = dir.join("link");
        std::os::unix::fs::symlink(&file, &link).unwrap();
        assert!(matches!(
            fs.read_limited(&link, 100),
            Err(ReadLimitedError::Symlink)
        ));
        assert!(matches!(
            fs.read_limited(&dir, 100),
            Err(ReadLimitedError::NotRegular(FileKind::Directory))
        ));
        assert!(matches!(
            fs.read_limited(&dir.join("missing"), 100),
            Err(ReadLimitedError::NotFound)
        ));
        let fifo = dir.join("fifo");
        nix::unistd::mkfifo(&fifo, nix::sys::stat::Mode::from_bits_truncate(0o600)).unwrap();
        assert!(matches!(
            fs.read_limited(&fifo, 100),
            Err(ReadLimitedError::NotRegular(FileKind::Other))
        ));
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn private_dir_is_exactly_0700_and_not_recreated() {
        let dir = temp_dir("dir");
        let fs = RealFileSystem;
        let child = dir.join("child");
        fs.create_private_dir(&child).unwrap();
        let facts = fs.symlink_metadata(&child).unwrap();
        assert_eq!(facts.kind, FileKind::Directory);
        assert_eq!(facts.permissions, PRIVATE_DIR_MODE);
        assert_eq!(
            fs.create_private_dir(&child).unwrap_err().kind(),
            io::ErrorKind::AlreadyExists
        );
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn link_new_never_replaces_an_existing_file() {
        let dir = temp_dir("link");
        let fs = RealFileSystem;
        let staged = dir.join("staged");
        let target = dir.join("target");
        std::fs::write(&staged, b"new").unwrap();
        fs.link_new(&staged, &target).unwrap();
        assert_eq!(std::fs::read(&target).unwrap(), b"new");
        std::fs::write(&staged, b"newer").unwrap();
        let other = dir.join("other");
        std::fs::write(&other, b"other").unwrap();
        assert_eq!(
            fs.link_new(&other, &target).unwrap_err().kind(),
            io::ErrorKind::AlreadyExists
        );
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn directory_lock_is_exclusive_until_dropped() {
        let dir = temp_dir("lock");
        let fs = RealFileSystem;
        let first = fs.try_lock_directory(&dir).unwrap();
        assert!(first.is_some());
        assert!(fs.try_lock_directory(&dir).unwrap().is_none());
        drop(first);
        assert!(fs.try_lock_directory(&dir).unwrap().is_some());
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn list_directory_is_bounded() {
        let dir = temp_dir("list");
        for index in 0..5 {
            std::fs::write(dir.join(format!("f{index}")), b"x").unwrap();
        }
        assert_eq!(RealFileSystem.list_directory(&dir, 3).unwrap().len(), 3);
        assert_eq!(RealFileSystem.list_directory(&dir, 100).unwrap().len(), 5);
        std::fs::remove_dir_all(dir).unwrap();
    }
}

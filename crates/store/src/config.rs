//! Versioned atomic JSON configuration repository (`STORAGE.md` §7,
//! `contracts/CONFIG.md`, AT08, AT18, AT36).
//!
//! * `load`: a missing file yields the built-in defaults (nothing written);
//!   a symlinked, foreign-owned or group/world-writable file is refused; a
//!   newer `schema_version` is reported and never rewritten; parse errors
//!   carry line and column.
//! * `compare_and_swap`: under an exclusive directory lock the current file
//!   is re-read, both the candidate's and the on-disk revision must equal
//!   the expected one, the candidate is validated, and the document is
//!   written as `temp (0600) -> fsync -> rename -> directory fsync` with
//!   revision + 1. Failures before the rename leave the old file untouched;
//!   a directory fsync failure after the rename is `Uncertain` and carries
//!   the configuration reloaded from disk.
//! * `import_legacy`: dry-run mapping of a v1 file; `--apply` writes only a
//!   new v2 file and refuses to replace an existing one. The legacy file is
//!   only read.

mod document;
mod legacy;
mod repository;

pub use document::MAX_CONFIG_FILE_BYTES;
pub use legacy::LegacyImportError;
pub use repository::FileConfigRepository;

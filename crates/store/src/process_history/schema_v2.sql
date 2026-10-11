-- Version 2 of schema.sql, as released before D-77: kept so the tests can
-- build a version 2 database and check upgrade_from_2.sql reaches version 3.
-- dgxtop process history (D-76): GPU process runs and their buckets.
-- Not part of the sealed spec; versioned with the other partitions.
PRAGMA foreign_keys = ON;
BEGIN IMMEDIATE;

CREATE TABLE IF NOT EXISTS process_runs (
    run_id INTEGER PRIMARY KEY,
    host_id TEXT NOT NULL,
    boot_id TEXT NOT NULL,
    pid_namespace TEXT NOT NULL,
    pid INTEGER NOT NULL CHECK (pid > 0),
    start_time_ticks INTEGER NOT NULL CHECK (start_time_ticks >= 0),
    name TEXT NOT NULL,
    command TEXT NOT NULL,
    cwd TEXT NOT NULL,
    uid INTEGER CHECK (uid IS NULL OR uid >= 0),
    -- GPU labels, one per line.
    gpus TEXT NOT NULL,
    started_utc_ns INTEGER NOT NULL,
    last_seen_utc_ns INTEGER NOT NULL,
    ended_utc_ns INTEGER,
    -- NULL while it runs; else the cause code and its signal or exit code.
    end_cause TEXT CHECK (end_cause IS NULL OR end_cause IN (
        'dgxtop_sigterm', 'dgxtop_sigkill', 'signal', 'exit', 'oom',
        'not_visible', 'while_away', 'host_restarted')),
    end_code INTEGER CHECK (end_code IS NULL OR end_code BETWEEN 0 AND 255),
    CHECK ((end_cause IS NULL) = (ended_utc_ns IS NULL)),
    UNIQUE (host_id, boot_id, pid_namespace, pid, start_time_ticks)
) STRICT;

-- One closed bucket: tier 1 a minute, 2 ten minutes. `aggregates` packs,
-- per metric in RunMetric order, count (u32), mean (f32) and peak (f32),
-- little-endian: 96 bytes.
CREATE TABLE IF NOT EXISTS process_buckets (
    run_id INTEGER NOT NULL REFERENCES process_runs (run_id) ON DELETE CASCADE,
    tier INTEGER NOT NULL CHECK (tier IN (1, 2)),
    start_utc_ns INTEGER NOT NULL,
    aggregates BLOB NOT NULL CHECK (length(aggregates) = 96),
    PRIMARY KEY (run_id, tier, start_utc_ns)
) STRICT, WITHOUT ROWID;

CREATE INDEX IF NOT EXISTS idx_process_runs_seen ON process_runs (last_seen_utc_ns);
CREATE INDEX IF NOT EXISTS idx_process_buckets_time ON process_buckets (tier, start_utc_ns);

PRAGMA user_version = 2;
COMMIT;

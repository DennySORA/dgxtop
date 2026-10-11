-- dgxtop process history (D-76, D-77): GPU process runs and their buckets.
-- Not part of the sealed spec. A file of another format is recreated,
-- never upgraded (D-85).
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
    -- What the probe read once (RunDetails).
    parent_pid INTEGER CHECK (parent_pid IS NULL OR parent_pid > 0),
    parent_name TEXT NOT NULL DEFAULT '',
    executable TEXT NOT NULL DEFAULT '',
    cgroup TEXT NOT NULL DEFAULT '',
    nice INTEGER CHECK (nice IS NULL OR nice BETWEEN -20 AND 19),
    CHECK ((end_cause IS NULL) = (ended_utc_ns IS NULL)),
    UNIQUE (host_id, boot_id, pid_namespace, pid, start_time_ticks)
) STRICT;

-- One closed bucket of a size (Tier::code): 1 a minute, 2 ten minutes,
-- 3 to 12 five minutes, 30 minutes, 1, 2, 4, 6, 8, 12, 18 and 24 hours.
-- `aggregates` packs, per metric in RunMetric order, count (u32), mean (f32)
-- and peak (f32), little-endian: 12 bytes a metric, every metric.
CREATE TABLE IF NOT EXISTS process_buckets (
    run_id INTEGER NOT NULL REFERENCES process_runs (run_id) ON DELETE CASCADE,
    tier INTEGER NOT NULL CHECK (tier BETWEEN 1 AND 12),
    start_utc_ns INTEGER NOT NULL,
    aggregates BLOB NOT NULL CHECK (
        length(aggregates) BETWEEN 12 AND 1200 AND length(aggregates) % 12 = 0),
    PRIMARY KEY (run_id, tier, start_utc_ns)
) STRICT, WITHOUT ROWID;

CREATE INDEX IF NOT EXISTS idx_process_runs_seen ON process_runs (last_seen_utc_ns);
CREATE INDEX IF NOT EXISTS idx_process_buckets_time ON process_buckets (tier, start_utc_ns);

PRAGMA user_version = 3;
COMMIT;

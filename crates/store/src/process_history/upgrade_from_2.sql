-- dgxtop process history: version 2 to 3 (D-77). Run after an integrity
-- check and a backup; it leaves the tables schema.sql creates: the runs
-- gain what the probe reads once, the buckets more sizes and room for
-- more metrics (the bucket table is rebuilt, its CHECKs being fixed).
BEGIN IMMEDIATE;

ALTER TABLE process_runs ADD COLUMN parent_pid INTEGER
    CHECK (parent_pid IS NULL OR parent_pid > 0);
ALTER TABLE process_runs ADD COLUMN parent_name TEXT NOT NULL DEFAULT '';
ALTER TABLE process_runs ADD COLUMN executable TEXT NOT NULL DEFAULT '';
ALTER TABLE process_runs ADD COLUMN cgroup TEXT NOT NULL DEFAULT '';
ALTER TABLE process_runs ADD COLUMN nice INTEGER
    CHECK (nice IS NULL OR nice BETWEEN -20 AND 19);

CREATE TABLE process_buckets_v3 (
    run_id INTEGER NOT NULL REFERENCES process_runs (run_id) ON DELETE CASCADE,
    tier INTEGER NOT NULL CHECK (tier BETWEEN 1 AND 12),
    start_utc_ns INTEGER NOT NULL,
    aggregates BLOB NOT NULL CHECK (
        length(aggregates) BETWEEN 12 AND 1200 AND length(aggregates) % 12 = 0),
    PRIMARY KEY (run_id, tier, start_utc_ns)
) STRICT, WITHOUT ROWID;
INSERT INTO process_buckets_v3 (run_id, tier, start_utc_ns, aggregates)
    SELECT run_id, tier, start_utc_ns, aggregates FROM process_buckets;
DROP TABLE process_buckets;
ALTER TABLE process_buckets_v3 RENAME TO process_buckets;
CREATE INDEX IF NOT EXISTS idx_process_buckets_time ON process_buckets (tier, start_utc_ns);

PRAGMA user_version = 3;
COMMIT;

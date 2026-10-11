-- dgxtop Overview History tiers (D-82): each Overview series' closed
-- buckets of every bar size, the latest 300 of each (series, size). Not
-- part of the sealed spec; dgxtop's own version 1.
PRAGMA foreign_keys = ON;
BEGIN IMMEDIATE;

-- A series in every boot (its metric key without the boot).
CREATE TABLE IF NOT EXISTS tier_series (
    series_id INTEGER PRIMARY KEY,
    host_id TEXT NOT NULL,
    entity_id TEXT NOT NULL,
    metric_id TEXT NOT NULL,
    scope TEXT NOT NULL,
    -- The latest bucket start written. Nothing reads it since the history
    -- decides which series go (D-95); kept so that files of this format
    -- stay usable (a format change recreates the file, D-85).
    last_utc_ns INTEGER NOT NULL,
    UNIQUE (host_id, entity_id, metric_id, scope)
) STRICT;

-- One closed bucket of a size (Tier::code): 1 a minute, 2 ten minutes,
-- 3 to 12 five minutes, 30 minutes, 1, 2, 4, 6, 8, 12, 18 and 24 hours.
-- What makes merging exact: the time held, the time-weighted sum, the
-- extremes (NULL for a counter), the samples and a counter's exact total
-- (decimal; NULL when it was not exact).
CREATE TABLE IF NOT EXISTS tier_buckets (
    series_id INTEGER NOT NULL REFERENCES tier_series (series_id) ON DELETE CASCADE,
    tier INTEGER NOT NULL CHECK (tier BETWEEN 1 AND 12),
    start_utc_ns INTEGER NOT NULL,
    covered_ns INTEGER NOT NULL CHECK (covered_ns > 0),
    weighted_sum REAL NOT NULL,
    minimum REAL,
    maximum REAL,
    sample_count INTEGER NOT NULL CHECK (sample_count >= 0),
    total TEXT CHECK (total IS NULL OR length(total) BETWEEN 1 AND 20),
    PRIMARY KEY (series_id, tier, start_utc_ns)
) STRICT, WITHOUT ROWID;

CREATE INDEX IF NOT EXISTS idx_tier_series_last ON tier_series (last_utc_ns);

PRAGMA user_version = 1;
COMMIT;

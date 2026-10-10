-- 僅可選 history.sqlite3；採WAL/NORMAL與有界writer/reader；new DB使用本DDL。
PRAGMA foreign_keys = ON;
BEGIN IMMEDIATE;
CREATE TABLE IF NOT EXISTS history_segments (
 segment_id TEXT PRIMARY KEY,
 host_id TEXT NOT NULL,
 boot_id TEXT NOT NULL,
 entity_id TEXT NOT NULL,
 metric_id TEXT NOT NULL,
 scope TEXT NOT NULL,
 source_id TEXT NOT NULL,
 source_generation INTEGER NOT NULL CHECK(source_generation>=0),
 policy_revision INTEGER NOT NULL CHECK(policy_revision>0),
 unit TEXT NOT NULL,
 sample_kind TEXT NOT NULL CHECK(sample_kind IN ('measured','derived','estimated')),
 started_utc TEXT NOT NULL
);
CREATE TABLE IF NOT EXISTS history_buckets (
 segment_id TEXT NOT NULL REFERENCES history_segments(segment_id) ON DELETE CASCADE,
 start_boot_ns TEXT NOT NULL CHECK(length(start_boot_ns)=20 AND start_boot_ns NOT GLOB '*[^0-9]*' AND start_boot_ns<='18446744073709551615'),
 end_boot_ns TEXT NOT NULL CHECK(length(end_boot_ns)=20 AND end_boot_ns NOT GLOB '*[^0-9]*' AND end_boot_ns<='18446744073709551615'),
 start_utc TEXT NOT NULL,
 end_utc TEXT NOT NULL,
 covered_ns TEXT NOT NULL CHECK(length(covered_ns)=20 AND covered_ns NOT GLOB '*[^0-9]*' AND covered_ns<='00000000060000000000'),
 weighted_sum REAL,
 minimum REAL,
 maximum REAL,
 counter_delta_decimal TEXT CHECK(counter_delta_decimal IS NULL OR (length(counter_delta_decimal) BETWEEN 1 AND 39 AND counter_delta_decimal NOT GLOB '*[^0-9]*')),
 sample_count INTEGER NOT NULL CHECK(sample_count>=0),
 method TEXT NOT NULL CHECK(method IN ('time_weighted_step','counter_delta','estimated_integral','gap')),
 reason_code TEXT,
 PRIMARY KEY(segment_id,start_boot_ns),
 CHECK(start_boot_ns<end_boot_ns)
);
-- 跨桶完整counter interval獨立保存，不同時算進兩個bucket total。
CREATE TABLE IF NOT EXISTS counter_bridges (
 segment_id TEXT NOT NULL REFERENCES history_segments(segment_id) ON DELETE CASCADE,
 start_boot_ns TEXT NOT NULL CHECK(length(start_boot_ns)=20 AND start_boot_ns NOT GLOB '*[^0-9]*' AND start_boot_ns<='18446744073709551615'),
 end_boot_ns TEXT NOT NULL CHECK(length(end_boot_ns)=20 AND end_boot_ns NOT GLOB '*[^0-9]*' AND end_boot_ns<='18446744073709551615'),
 delta_decimal TEXT NOT NULL CHECK(length(delta_decimal) BETWEEN 1 AND 39 AND delta_decimal NOT GLOB '*[^0-9]*'),
 PRIMARY KEY(segment_id,start_boot_ns,end_boot_ns),
 CHECK(start_boot_ns<end_boot_ns)
);
CREATE TABLE IF NOT EXISTS history_gaps (
 gap_id TEXT PRIMARY KEY,
 host_id TEXT NOT NULL,
 boot_id TEXT NOT NULL,
 start_boot_ns TEXT NOT NULL CHECK(length(start_boot_ns)=20 AND start_boot_ns NOT GLOB '*[^0-9]*' AND start_boot_ns<='18446744073709551615'),
 end_boot_ns TEXT NOT NULL CHECK(length(end_boot_ns)=20 AND end_boot_ns NOT GLOB '*[^0-9]*' AND end_boot_ns<='18446744073709551615'),
 reason_code TEXT NOT NULL,
 affected_count INTEGER NOT NULL CHECK(affected_count>0),
 CHECK(start_boot_ns<end_boot_ns)
);
CREATE INDEX IF NOT EXISTS idx_history_utc ON history_buckets(start_utc,end_utc);
CREATE INDEX IF NOT EXISTS idx_segments_key ON history_segments(host_id,boot_id,entity_id,metric_id,scope);
CREATE INDEX IF NOT EXISTS idx_bridges_end ON counter_bridges(segment_id,end_boot_ns);
PRAGMA user_version = 2;
COMMIT;

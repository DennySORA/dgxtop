//! Pure parsers for the procfs text formats this adapter reads
//! (`/proc/stat`, `/proc/loadavg`, `/proc/meminfo`, `/proc/diskstats`,
//! `/proc/<pid>/stat`, `/proc/cpuinfo`). Values stay exact `u64`; an absent
//! column and an unparsable column are distinct and never become 0.

use std::collections::{BTreeMap, BTreeSet};

use dgxtop_core::metric::{ids, native};

use super::arm_cores::arm_core_kinds;

/// One field of a kernel text record.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) enum Field<T> {
    Value(T),
    /// Not provided by this kernel (optional or newer column).
    Absent,
    /// Present but not a valid number.
    Invalid,
}

fn u64_field(token: Option<&str>) -> Field<u64> {
    match token {
        None => Field::Absent,
        Some(text) => text.parse::<u64>().map_or(Field::Invalid, Field::Value),
    }
}

/// `cpu` (aggregate) or `cpuN` (logical CPU N from the label).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum CpuLabel {
    Aggregate,
    Core(u32),
}

fn cpu_label(token: &str) -> Option<CpuLabel> {
    let suffix = token.strip_prefix("cpu")?;
    if suffix.is_empty() {
        return Some(CpuLabel::Aggregate);
    }
    if !suffix.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    suffix.parse::<u32>().ok().map(CpuLabel::Core)
}

/// The `/proc/stat` time columns emitted as counters. Guest columns are not
/// listed: they are already included in user/nice.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum StatColumn {
    User,
    Nice,
    System,
    Idle,
    Iowait,
    Irq,
    Softirq,
    Steal,
}

pub(crate) const STAT_COLUMNS: [StatColumn; 8] = [
    StatColumn::User,
    StatColumn::Nice,
    StatColumn::System,
    StatColumn::Idle,
    StatColumn::Iowait,
    StatColumn::Irq,
    StatColumn::Softirq,
    StatColumn::Steal,
];

impl StatColumn {
    fn position(self) -> usize {
        match self {
            Self::User => 0,
            Self::Nice => 1,
            Self::System => 2,
            Self::Idle => 3,
            Self::Iowait => 4,
            Self::Irq => 5,
            Self::Softirq => 6,
            Self::Steal => 7,
        }
    }

    pub(crate) fn channel(self) -> &'static str {
        match self {
            Self::User => "user",
            Self::Nice => "nice",
            Self::System => "system",
            Self::Idle => "idle",
            Self::Iowait => "iowait",
            Self::Irq => "irq",
            Self::Softirq => "softirq",
            Self::Steal => "steal",
        }
    }

    pub(crate) fn metric_id(self) -> &'static str {
        match self {
            Self::User => ids::CPU_TIME_USER,
            Self::Nice => ids::CPU_TIME_NICE,
            Self::System => ids::CPU_TIME_SYSTEM,
            Self::Idle => ids::CPU_TIME_IDLE,
            Self::Iowait => ids::CPU_TIME_IOWAIT,
            Self::Irq => ids::CPU_TIME_IRQ,
            Self::Softirq => ids::CPU_TIME_SOFTIRQ,
            Self::Steal => ids::CPU_TIME_STEAL,
        }
    }
}

/// One `cpu`/`cpuN` line of `/proc/stat`.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct CpuTimes {
    pub label: CpuLabel,
    columns: [Field<u64>; 8],
}

impl CpuTimes {
    pub(crate) fn column(&self, column: StatColumn) -> Field<u64> {
        self.columns[column.position()]
    }
}

/// CPU lines of `/proc/stat`; other lines, unlabelled garbage and duplicate
/// labels (first wins) are ignored.
pub(crate) fn parse_cpu_times(text: &str) -> Vec<CpuTimes> {
    let mut seen = BTreeSet::new();
    let mut lines = Vec::new();
    for line in text.lines() {
        let mut tokens = line.split_whitespace();
        let Some(label) = tokens.next().and_then(cpu_label) else {
            continue;
        };
        if !seen.insert(label) {
            continue;
        }
        let values: Vec<&str> = tokens.take(STAT_COLUMNS.len()).collect();
        let columns = STAT_COLUMNS.map(|column| u64_field(values.get(column.position()).copied()));
        lines.push(CpuTimes { label, columns });
    }
    lines
}

/// `/proc/loadavg` fields.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct LoadAverage {
    pub one: Field<f64>,
    pub five: Field<f64>,
    pub fifteen: Field<f64>,
    pub running: Field<u64>,
    pub total: Field<u64>,
}

fn load_field(token: Option<&str>) -> Field<f64> {
    match token {
        None => Field::Absent,
        Some(text) => match text.parse::<f64>() {
            Ok(value) if value.is_finite() && value >= 0.0 => Field::Value(value),
            _ => Field::Invalid,
        },
    }
}

pub(crate) fn parse_loadavg(text: &str) -> LoadAverage {
    let tokens: Vec<&str> = text.split_whitespace().collect();
    let (running, total) = match tokens.get(3) {
        None => (Field::Absent, Field::Absent),
        Some(tasks) => match tasks.split_once('/') {
            Some((running, total)) => (u64_field(Some(running)), u64_field(Some(total))),
            None => (Field::Invalid, Field::Invalid),
        },
    };
    LoadAverage {
        one: load_field(tokens.first().copied()),
        five: load_field(tokens.get(1).copied()),
        fifteen: load_field(tokens.get(2).copied()),
        running,
        total,
    }
}

/// The `/proc/meminfo` fields this adapter emits (all in kB = KiB).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum MemField {
    Total,
    Available,
    Buffers,
    Cached,
    SwapTotal,
    SwapFree,
}

pub(crate) const MEM_FIELDS: [MemField; 6] = [
    MemField::Total,
    MemField::Available,
    MemField::Buffers,
    MemField::Cached,
    MemField::SwapTotal,
    MemField::SwapFree,
];

impl MemField {
    pub(crate) fn kernel_name(self) -> &'static str {
        match self {
            Self::Total => "MemTotal",
            Self::Available => "MemAvailable",
            Self::Buffers => "Buffers",
            Self::Cached => "Cached",
            Self::SwapTotal => "SwapTotal",
            Self::SwapFree => "SwapFree",
        }
    }

    pub(crate) fn metric_id(self) -> &'static str {
        match self {
            Self::Total => ids::MEMORY_TOTAL,
            Self::Available => ids::MEMORY_AVAILABLE,
            Self::Buffers => ids::MEMORY_BUFFERS,
            Self::Cached => ids::MEMORY_CACHED,
            Self::SwapTotal => ids::MEMORY_SWAP_TOTAL,
            Self::SwapFree => ids::MEMORY_SWAP_FREE_RAW,
        }
    }

    fn from_kernel_name(name: &str) -> Option<Self> {
        MEM_FIELDS
            .into_iter()
            .find(|field| field.kernel_name() == name)
    }
}

/// Parsed `/proc/meminfo` (fields never seen are `Absent`).
#[derive(Debug, Clone, PartialEq, Default)]
pub(crate) struct MemInfo {
    values: BTreeMap<MemField, Field<u64>>,
}

impl MemInfo {
    pub(crate) fn field(&self, field: MemField) -> Field<u64> {
        self.values.get(&field).copied().unwrap_or(Field::Absent)
    }
}

pub(crate) fn parse_meminfo(text: &str) -> MemInfo {
    let mut info = MemInfo::default();
    for line in text.lines() {
        let Some((name, rest)) = line.split_once(':') else {
            continue;
        };
        let Some(field) = MemField::from_kernel_name(name.trim()) else {
            continue;
        };
        let mut tokens = rest.split_whitespace();
        let value = u64_field(tokens.next());
        // A listed field without a value or without the kB unit is malformed.
        let value = match (value, tokens.next()) {
            (Field::Value(kib), Some("kB")) => Field::Value(kib),
            _ => Field::Invalid,
        };
        info.values.entry(field).or_insert(value);
    }
    info
}

/// The `/proc/diskstats` fields this adapter emits.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum DiskField {
    ReadsCompleted,
    SectorsRead,
    ReadTime,
    WritesCompleted,
    SectorsWritten,
    WriteTime,
}

pub(crate) const DISK_FIELDS: [DiskField; 6] = [
    DiskField::ReadsCompleted,
    DiskField::SectorsRead,
    DiskField::ReadTime,
    DiskField::WritesCompleted,
    DiskField::SectorsWritten,
    DiskField::WriteTime,
];

impl DiskField {
    fn position(self) -> usize {
        match self {
            Self::ReadsCompleted => 0,
            Self::SectorsRead => 1,
            Self::ReadTime => 2,
            Self::WritesCompleted => 3,
            Self::SectorsWritten => 4,
            Self::WriteTime => 5,
        }
    }

    /// Token index after `major minor name` in the modern (≥ 11 field) format.
    fn modern_index(self) -> usize {
        match self {
            Self::ReadsCompleted => 0,
            Self::SectorsRead => 2,
            Self::ReadTime => 3,
            Self::WritesCompleted => 4,
            Self::SectorsWritten => 6,
            Self::WriteTime => 7,
        }
    }

    /// Token index in the legacy 4-field partition format (no times).
    fn legacy_index(self) -> Option<usize> {
        match self {
            Self::ReadsCompleted => Some(0),
            Self::SectorsRead => Some(1),
            Self::WritesCompleted => Some(2),
            Self::SectorsWritten => Some(3),
            Self::ReadTime | Self::WriteTime => None,
        }
    }

    pub(crate) fn channel(self) -> &'static str {
        match self {
            Self::ReadsCompleted => "reads_completed",
            Self::SectorsRead => "sectors_read",
            Self::ReadTime => "read_time_ms",
            Self::WritesCompleted => "writes_completed",
            Self::SectorsWritten => "sectors_written",
            Self::WriteTime => "write_time_ms",
        }
    }

    pub(crate) fn metric_id(self) -> &'static str {
        match self {
            Self::ReadsCompleted => ids::DISK_READS_COMPLETED,
            Self::SectorsRead => ids::DISK_SECTORS_READ,
            Self::ReadTime => ids::DISK_READ_TIME,
            Self::WritesCompleted => ids::DISK_WRITES_COMPLETED,
            Self::SectorsWritten => ids::DISK_SECTORS_WRITTEN,
            Self::WriteTime => ids::DISK_WRITE_TIME,
        }
    }

    pub(crate) fn native_unit(self) -> &'static str {
        match self {
            Self::ReadsCompleted | Self::WritesCompleted => native::COUNT,
            Self::SectorsRead | Self::SectorsWritten => native::SECTORS_512,
            Self::ReadTime | Self::WriteTime => native::MILLISECONDS,
        }
    }
}

/// Number of statistic tokens of the modern diskstats format (kernel ≥ 2.6.25).
const MODERN_DISK_STATS: usize = 11;
/// Number of statistic tokens of the legacy partition format.
const LEGACY_DISK_STATS: usize = 4;

/// One `/proc/diskstats` line.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct DiskLine {
    pub major: u32,
    pub minor: u32,
    pub name: String,
    fields: [Field<u64>; 6],
}

impl DiskLine {
    pub(crate) fn field(&self, field: DiskField) -> Field<u64> {
        self.fields[field.position()]
    }
}

/// Lines whose identity (`major minor name`) parses; malformed statistics
/// become `Invalid` fields instead of dropping the device.
pub(crate) fn parse_diskstats(text: &str) -> Vec<DiskLine> {
    text.lines().filter_map(parse_disk_line).collect()
}

fn parse_disk_line(line: &str) -> Option<DiskLine> {
    let tokens: Vec<&str> = line.split_whitespace().collect();
    let major = tokens.first()?.parse::<u32>().ok()?;
    let minor = tokens.get(1)?.parse::<u32>().ok()?;
    let name = (*tokens.get(2)?).to_owned();
    let stats = &tokens[3..];
    let fields = DISK_FIELDS.map(|field| disk_field(stats, field));
    Some(DiskLine {
        major,
        minor,
        name,
        fields,
    })
}

fn disk_field(stats: &[&str], field: DiskField) -> Field<u64> {
    if stats.len() >= MODERN_DISK_STATS {
        return u64_field(stats.get(field.modern_index()).copied());
    }
    if stats.len() == LEGACY_DISK_STATS {
        return match field.legacy_index() {
            Some(index) => u64_field(stats.get(index).copied()),
            None => Field::Absent,
        };
    }
    Field::Invalid
}

/// Identity and resource fields of `/proc/<pid>/stat`.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct PidStat {
    pub pid: i32,
    pub comm: String,
    /// The state letter (field 3: `R`, `S`, `Z`, …); `None` when absent.
    pub state: Option<char>,
    /// The parent's PID (field 4); `None` when unreadable.
    pub ppid: Option<i32>,
    pub start_time_ticks: u64,
    pub utime: Field<u64>,
    pub stime: Field<u64>,
    /// CPU time of its reaped children (fields 16 and 17).
    pub cutime: Field<u64>,
    pub cstime: Field<u64>,
    /// Niceness (field 19).
    pub nice: Option<i64>,
    /// Threads (field 20).
    pub num_threads: Option<u64>,
    pub rss_pages: Field<u64>,
    /// The wait status of a zombie (field 52, Linux 3.5+); zero to a reader
    /// that may not trace the process, `None` on older kernels.
    pub exit_code: Option<u32>,
}

/// Positions after the closing parenthesis of `comm` (field 3 is index 0).
const STAT_STATE: usize = 0;
const STAT_PPID: usize = 1;
const STAT_UTIME: usize = 11;
const STAT_STIME: usize = 12;
const STAT_CUTIME: usize = 13;
const STAT_CSTIME: usize = 14;
const STAT_NICE: usize = 16;
const STAT_NUM_THREADS: usize = 17;
const STAT_START_TIME: usize = 19;
const STAT_RSS: usize = 21;
const STAT_EXIT_CODE: usize = 49;

/// Parses from the LAST `)` because `comm` may contain spaces and
/// parentheses. `None` when pid or start time (the identity) is unusable.
pub(crate) fn parse_pid_stat(text: &str) -> Option<PidStat> {
    let open = text.find('(')?;
    let close = text.rfind(')')?;
    if close < open {
        return None;
    }
    let pid = text[..open].trim().parse::<i32>().ok()?;
    let comm = text[open + 1..close].to_owned();
    let rest: Vec<&str> = text[close + 1..].split_whitespace().collect();
    let Field::Value(start_time_ticks) = u64_field(rest.get(STAT_START_TIME).copied()) else {
        return None;
    };
    Some(PidStat {
        pid,
        comm,
        state: rest.get(STAT_STATE).and_then(|state| state.chars().next()),
        ppid: rest
            .get(STAT_PPID)
            .and_then(|ppid| ppid.parse::<i32>().ok()),
        start_time_ticks,
        utime: u64_field(rest.get(STAT_UTIME).copied()),
        stime: u64_field(rest.get(STAT_STIME).copied()),
        cutime: u64_field(rest.get(STAT_CUTIME).copied()),
        cstime: u64_field(rest.get(STAT_CSTIME).copied()),
        nice: rest
            .get(STAT_NICE)
            .and_then(|nice| nice.parse::<i64>().ok()),
        num_threads: rest
            .get(STAT_NUM_THREADS)
            .and_then(|threads| threads.parse::<u64>().ok()),
        rss_pages: u64_field(rest.get(STAT_RSS).copied()),
        exit_code: rest
            .get(STAT_EXIT_CODE)
            .and_then(|code| code.parse::<u32>().ok()),
    })
}

/// The CPU model `/proc/cpuinfo` names: its first non-empty `model name`,
/// else, as an arm64 kernel writes none, its kinds of cores
/// ([`arm_core_kinds`], D-132); `None` when it names neither.
pub(crate) fn parse_cpu_model(text: &str) -> Option<String> {
    text.lines()
        .filter_map(|line| line.split_once(':'))
        .find(|(key, value)| key.trim() == "model name" && !value.trim().is_empty())
        .map(|(_, value)| value.trim().to_owned())
        .or_else(|| arm_core_kinds(text))
}

#[cfg(test)]
mod tests {
    use super::*;

    const STAT: &str = "cpu  90223 16578 91354 40229674 6032 0 154 0 0 0\n\
cpu0 4304 373 5966 1251895 203 0 0 0 0 0\n\
cpu3 1 2 3 4\n\
cpu7 1 2 x 4 5 6 7 8\n\
cpu0 9 9 9 9 9 9 9 9\n\
cpuX 1 2 3\n\
cpu99999999999 1 2 3\n\
intr 15428341 9 0\n\
ctxt 18225208\n";

    #[test]
    fn cpu_lines_use_labels_and_keep_absent_columns_absent() {
        let lines = parse_cpu_times(STAT);
        let labels: Vec<CpuLabel> = lines.iter().map(|line| line.label).collect();
        assert_eq!(
            labels,
            vec![
                CpuLabel::Aggregate,
                CpuLabel::Core(0),
                CpuLabel::Core(3),
                CpuLabel::Core(7)
            ]
        );
        assert_eq!(lines[0].column(StatColumn::User), Field::Value(90_223));
        assert_eq!(lines[0].column(StatColumn::Steal), Field::Value(0));
        assert_eq!(lines[1].column(StatColumn::User), Field::Value(4304));
        assert_eq!(lines[2].column(StatColumn::Idle), Field::Value(4));
        assert_eq!(lines[2].column(StatColumn::Iowait), Field::Absent);
        assert_eq!(lines[2].column(StatColumn::Steal), Field::Absent);
        assert_eq!(lines[3].column(StatColumn::System), Field::Invalid);
        assert_eq!(lines[3].column(StatColumn::Steal), Field::Value(8));
    }

    #[test]
    fn u64_max_counter_survives() {
        let lines = parse_cpu_times("cpu 18446744073709551615 0 0 0 0 0 0 0\n");
        assert_eq!(lines[0].column(StatColumn::User), Field::Value(u64::MAX));
        let overflow = parse_cpu_times("cpu 18446744073709551616 0 0 0\n");
        assert_eq!(overflow[0].column(StatColumn::User), Field::Invalid);
    }

    #[test]
    fn loadavg_fields_parse_and_garbage_is_invalid() {
        let load = parse_loadavg("0.23 0.47 0.33 1/1591 71703\n");
        assert_eq!(load.one, Field::Value(0.23));
        assert_eq!(load.fifteen, Field::Value(0.33));
        assert_eq!(load.running, Field::Value(1));
        assert_eq!(load.total, Field::Value(1591));
        let garbage = parse_loadavg("nan -1 x 5\n");
        assert_eq!(garbage.one, Field::Invalid);
        assert_eq!(garbage.five, Field::Invalid);
        assert_eq!(garbage.running, Field::Invalid);
        let empty = parse_loadavg("");
        assert_eq!(empty.one, Field::Absent);
        assert_eq!(empty.total, Field::Absent);
    }

    #[test]
    fn meminfo_requires_kb_and_reports_missing_fields_absent() {
        let info = parse_meminfo(
            "MemTotal:       32654612 kB\nMemFree: 100 kB\nSwapFree: 100 kB\nBuffers: 7 MB\nCached:\nMemTotal: 1 kB\n",
        );
        assert_eq!(info.field(MemField::Total), Field::Value(32_654_612));
        assert_eq!(info.field(MemField::SwapFree), Field::Value(100));
        assert_eq!(info.field(MemField::Buffers), Field::Invalid);
        assert_eq!(info.field(MemField::Cached), Field::Invalid);
        assert_eq!(info.field(MemField::Available), Field::Absent);
    }

    #[test]
    fn diskstats_modern_legacy_and_malformed_lines() {
        let lines = parse_diskstats(
            " 259 0 nvme0n1 347806 38507 11509086 31169 554347 133357 82536402 2694532 0 35004 2729390 0 0 0 0 17013 3688\n\
               8 1 sda1 10 20 30 40\n\
               8 2 sda2 1 2 3\n\
             bad line\n",
        );
        assert_eq!(lines.len(), 3);
        assert_eq!((lines[0].major, lines[0].minor), (259, 0));
        assert_eq!(
            lines[0].field(DiskField::ReadsCompleted),
            Field::Value(347_806)
        );
        assert_eq!(
            lines[0].field(DiskField::SectorsRead),
            Field::Value(11_509_086)
        );
        assert_eq!(lines[0].field(DiskField::ReadTime), Field::Value(31_169));
        assert_eq!(
            lines[0].field(DiskField::WritesCompleted),
            Field::Value(554_347)
        );
        assert_eq!(
            lines[0].field(DiskField::SectorsWritten),
            Field::Value(82_536_402)
        );
        assert_eq!(
            lines[0].field(DiskField::WriteTime),
            Field::Value(2_694_532)
        );
        assert_eq!(lines[1].field(DiskField::SectorsWritten), Field::Value(40));
        assert_eq!(lines[1].field(DiskField::ReadTime), Field::Absent);
        assert_eq!(lines[2].field(DiskField::ReadsCompleted), Field::Invalid);
    }

    #[test]
    fn pid_stat_comm_may_contain_spaces_and_parentheses() {
        let text = "4242 (my (weird) prog) S 1 4242 4242 0 -1 4194560 100 0 0 0 \
                    250 50 7 3 25 5 12 0 90100 1234567 789 18446744073709551615";
        let stat = parse_pid_stat(text).unwrap();
        assert_eq!(stat.pid, 4242);
        assert_eq!(stat.comm, "my (weird) prog");
        assert_eq!(stat.utime, Field::Value(250));
        assert_eq!(stat.stime, Field::Value(50));
        assert_eq!(
            (stat.cutime, stat.cstime),
            (Field::Value(7), Field::Value(3))
        );
        assert_eq!((stat.nice, stat.num_threads), (Some(5), Some(12)));
        assert_eq!(stat.ppid, Some(1));
        assert_eq!(stat.start_time_ticks, 90_100);
        assert_eq!(stat.rss_pages, Field::Value(789));
        assert_eq!(stat.state, Some('S'));
        assert_eq!(stat.exit_code, None, "a short line has no exit code");
    }

    /// A zombie's wait status is the last field of a full line (D-76).
    #[test]
    fn pid_stat_reads_the_state_and_a_zombies_exit_code() {
        let mut fields: Vec<String> = (0..50).map(|_| "0".to_owned()).collect();
        fields[0] = "Z".to_owned();
        fields[19] = "777".to_owned();
        fields[49] = "9".to_owned();
        let text = format!("31 (python3) {}", fields.join(" "));
        let stat = parse_pid_stat(&text).unwrap();
        assert_eq!(stat.state, Some('Z'));
        assert_eq!(stat.start_time_ticks, 777);
        assert_eq!(stat.exit_code, Some(9));
    }

    /// `/proc/cpuinfo` of an arm64 host: one block per logical CPU with its
    /// implementer and part, as GB10's kernel writes them.
    fn arm_cpuinfo(parts: &[&str]) -> String {
        parts
            .iter()
            .enumerate()
            .map(|(cpu, part)| {
                format!(
                    "processor\t: {cpu}\nBogoMIPS\t: 2000.00\nFeatures\t: fp asimd\n\
                     CPU implementer\t: 0x41\nCPU architecture: 8\nCPU variant\t: 0x0\n\
                     CPU part\t: {part}\nCPU revision\t: 1\n\n"
                )
            })
            .collect()
    }

    /// x86 names its model; an arm64 kernel writes no `model name`, so its
    /// cores' kinds name the CPU, each with its count (GB10: ten Cortex-A725
    /// and ten Cortex-X925 cores, the kind of CPU 0 first).
    #[test]
    fn the_cpu_model_is_the_model_name_or_the_arm_cores() {
        let x86 = "processor\t: 0\nmodel name\t: Acme Core 9 7900X\n\n\
                   processor\t: 1\nmodel name\t: Acme Core 9 7900X\n";
        assert_eq!(parse_cpu_model(x86).as_deref(), Some("Acme Core 9 7900X"));
        let gb10: Vec<&str> = [["0xd87"; 5], ["0xd85"; 5], ["0xd87"; 5], ["0xd85"; 5]].concat();
        assert_eq!(
            parse_cpu_model(&arm_cpuinfo(&gb10)).as_deref(),
            Some("10× Cortex-A725 + 10× Cortex-X925")
        );
        let named = format!("model name\t: Acme Arm 1\n{}", arm_cpuinfo(&["0xd85"]));
        assert_eq!(parse_cpu_model(&named).as_deref(), Some("Acme Arm 1"));
        assert_eq!(parse_cpu_model("processor\t: 0\nflags\t: fpu\n"), None);
    }

    #[test]
    fn pid_stat_without_identity_is_rejected() {
        assert!(parse_pid_stat("garbage").is_none());
        assert!(parse_pid_stat("12 (x) S 1 2 3").is_none());
        assert!(parse_pid_stat(") 12 (x").is_none());
        let partial =
            parse_pid_stat("7 (x) S 1 1 1 0 -1 0 0 0 0 0 a b 0 0 20 0 1 0 55 0 z").unwrap();
        assert_eq!(partial.utime, Field::Invalid);
        assert_eq!(partial.rss_pages, Field::Invalid);
    }
}

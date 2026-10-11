use std::collections::HashMap;
use std::io;
use std::path::PathBuf;

use dgxtop_core::identity::{Pid, PidNamespace};
use dgxtop_core::testing::{FakeClock, boot_id, host_id};

use dgxtop_core::process_probe::ChildUsage;

use super::*;
use crate::common::EntryName;

const NAMESPACE: &str = "pid:[4026531836]";

/// `/proc` as a map: files by path and the targets of links.
#[derive(Default)]
struct MapFiles {
    files: HashMap<String, String>,
    links: HashMap<String, PathBuf>,
    dirs: HashMap<String, Vec<EntryName>>,
}

impl MapFiles {
    fn with_namespace() -> Self {
        let mut files = Self::default();
        files
            .links
            .insert("/proc/self/ns/pid".to_owned(), PathBuf::from(NAMESPACE));
        files
    }

    fn file(mut self, path: &str, text: &str) -> Self {
        self.files.insert(path.to_owned(), text.to_owned());
        self
    }

    fn link(mut self, path: &str, target: &str) -> Self {
        self.links.insert(path.to_owned(), PathBuf::from(target));
        self
    }

    /// A folder of `names`, folders or files.
    fn dir(mut self, path: &str, names: &[&str], is_dir: bool) -> Self {
        let entries = names
            .iter()
            .map(|name| EntryName {
                name: (*name).to_owned(),
                is_dir,
            })
            .collect();
        self.dirs.insert(path.to_owned(), entries);
        self
    }
}

fn missing() -> io::Error {
    io::Error::from(io::ErrorKind::NotFound)
}

impl SystemFiles for MapFiles {
    fn read_to_string(&self, path: &str) -> io::Result<String> {
        self.files.get(path).cloned().ok_or_else(missing)
    }

    fn read_dir(&self, path: &str) -> io::Result<Vec<EntryName>> {
        self.dirs
            .get(path)
            .map(|entries| {
                entries
                    .iter()
                    .map(|entry| EntryName {
                        name: entry.name.clone(),
                        is_dir: entry.is_dir,
                    })
                    .collect()
            })
            .ok_or_else(missing)
    }

    fn read_link(&self, path: &str) -> io::Result<PathBuf> {
        self.links.get(path).cloned().ok_or_else(missing)
    }

    fn canonicalize(&self, path: &str) -> io::Result<PathBuf> {
        Ok(PathBuf::from(path))
    }

    fn exists(&self, path: &str) -> bool {
        self.files.contains_key(path) || self.links.contains_key(path)
    }

    fn owner_uid(&self, _path: &str) -> io::Result<u32> {
        Ok(1000)
    }
}

/// A full `stat` line of `pid` in `state`, started at `start` ticks, with
/// the wait status `exit_code`.
fn stat_line(pid: i32, state: char, start: u64, exit_code: u32) -> String {
    let mut fields: Vec<String> = (0..50).map(|_| "0".to_owned()).collect();
    fields[0] = state.to_string();
    fields[19] = start.to_string();
    fields[49] = exit_code.to_string();
    format!("{pid} (python3) {}", fields.join(" "))
}

/// A `stat` line of `pid` under `parent`, with the fields `(position after
/// the name, value)` set.
fn stat_of(pid: i32, parent: i32, fields: &[(usize, i64)]) -> String {
    let mut line: Vec<String> = (0..50).map(|_| "0".to_owned()).collect();
    line[0] = "S".to_owned();
    line[1] = parent.to_string();
    line[19] = "900".to_owned();
    for (position, value) in fields {
        line[*position] = value.to_string();
    }
    format!("{pid} (worker) {}", line.join(" "))
}

const UTIME: usize = 11;
const STIME: usize = 12;
const CUTIME: usize = 13;
const CSTIME: usize = 14;
const NICE: usize = 16;
const THREADS: usize = 17;
const RSS: usize = 21;

fn key(pid: i32, start: u64) -> ProcessKey {
    ProcessKey {
        host_id: host_id("host-a"),
        boot_id: boot_id("boot-a"),
        pid_namespace: PidNamespace::new(NAMESPACE).unwrap(),
        pid: Pid::new(i64::from(pid)).unwrap(),
        start_time_ticks: start,
    }
}

fn probe(files: MapFiles, targets: &[ProbeTarget]) -> ProbeReport {
    let mut probe = ProcfsProcessProbe::new(Arc::new(files), FakeClock::new(5_000), Some(4096));
    probe.probe(targets)
}

fn target(pid: i32, start: u64, with_text: bool) -> ProbeTarget {
    ProbeTarget {
        key: key(pid, start),
        with_text,
    }
}

const IO: &str = "rchar: 10\nwchar: 20\nsyscr: 1\nsyscw: 2\nread_bytes: 4096\n\
write_bytes: 8192\ncancelled_write_bytes: 0\n";

#[test]
fn a_running_process_reports_its_text_and_io() {
    let files = MapFiles::with_namespace()
        .file("/proc/42/stat", &stat_line(42, 'S', 900, 0))
        .file("/proc/42/io", IO)
        .file(
            "/proc/42/cmdline",
            "/venv/bin/python\0-m\0acme_llm.train\0--epochs\x003\0",
        )
        .link("/proc/42/cwd", "/home/u/code/acme-llm")
        .file("/proc/vmstat", "nr_free_pages 1\noom_kill 3\n");
    let report = probe(files, &[target(42, 900, true)]);
    assert_eq!(report.oom_kills, Some(3));
    assert_eq!(report.read_boot_ns, 5_000);
    let reading = &report.readings[0];
    assert_eq!(reading.liveness, ProbeLiveness::Running);
    assert_eq!(
        reading.command.as_deref(),
        Some("/venv/bin/python -m acme_llm.train --epochs 3")
    );
    assert_eq!(reading.cwd.as_deref(), Some("/home/u/code/acme-llm"));
    assert_eq!(
        reading.io,
        Some(IoCounters {
            read_bytes: 4096,
            write_bytes: 8192
        })
    );
}

#[test]
fn text_is_read_only_when_asked() {
    let files = MapFiles::with_namespace()
        .file("/proc/42/stat", &stat_line(42, 'R', 900, 0))
        .file("/proc/42/cmdline", "python\0")
        .link("/proc/42/cwd", "/srv");
    let reading = &probe(files, &[target(42, 900, false)]).readings[0];
    assert_eq!(reading.liveness, ProbeLiveness::Running);
    assert_eq!(
        (reading.command.as_deref(), reading.cwd.as_deref()),
        (None, None)
    );
    assert_eq!(reading.io, None, "an unreadable io file is no counters");
}

/// A reused PID is another process: the old one is gone.
#[test]
fn a_pid_with_another_start_time_is_gone() {
    let files = MapFiles::with_namespace().file("/proc/42/stat", &stat_line(42, 'S', 901, 0));
    let report = probe(files, &[target(42, 900, true), target(43, 900, true)]);
    assert_eq!(report.readings[0].liveness, ProbeLiveness::Gone);
    assert_eq!(
        report.readings[1].liveness,
        ProbeLiveness::Gone,
        "no stat at all"
    );
    assert_eq!(report.oom_kills, None, "no vmstat");
}

/// A zombie's wait status says how it ended where dgxtop may read it.
#[test]
fn a_zombie_reports_its_exit_where_readable() {
    let files = MapFiles::with_namespace()
        .file("/proc/42/stat", &stat_line(42, 'Z', 900, 9))
        .file("/proc/42/io", IO)
        .file("/proc/43/stat", &stat_line(43, 'Z', 900, 0))
        .file("/proc/43/io", IO)
        .file("/proc/44/stat", &stat_line(44, 'Z', 900, 0));
    let report = probe(
        files,
        &[
            target(42, 900, true),
            target(43, 900, false),
            target(44, 900, false),
        ],
    );
    assert_eq!(
        report.readings[0].liveness,
        ProbeLiveness::Zombie(Some(ExitStatus::Signaled(9)))
    );
    assert_eq!(
        report.readings[0].command, None,
        "a zombie has no command line"
    );
    assert_eq!(
        report.readings[1].liveness,
        ProbeLiveness::Zombie(Some(ExitStatus::Exited(0)))
    );
    assert_eq!(
        report.readings[2].liveness,
        ProbeLiveness::Zombie(None),
        "a zero the kernel may show for want of permission is no status"
    );
}

#[test]
fn a_key_of_another_namespace_is_not_visible() {
    let files = MapFiles::default().file("/proc/42/stat", &stat_line(42, 'S', 900, 0));
    let report = probe(files, &[target(42, 900, true)]);
    assert_eq!(report.readings[0].liveness, ProbeLiveness::Gone);
}

#[test]
fn texts_are_cleaned_and_bounded_and_targets_capped() {
    let long = "x".repeat(MAX_PROBE_TEXT_BYTES * 2);
    let files = MapFiles::with_namespace()
        .file("/proc/42/stat", &stat_line(42, 'S', 900, 0))
        .file("/proc/42/cmdline", &format!("run\0{long}\0"))
        .link("/proc/42/cwd", "/srv/a\u{1b}[31mb");
    let reading = &probe(files, &[target(42, 900, true)]).readings[0];
    let command = reading.command.as_deref().unwrap();
    assert!(command.len() <= MAX_PROBE_TEXT_BYTES && command.ends_with('…'));
    assert!(!reading.cwd.as_deref().unwrap().contains('\u{1b}'));
    let many: Vec<ProbeTarget> = (1..=100).map(|pid| target(pid, 1, false)).collect();
    let report = probe(MapFiles::with_namespace(), &many);
    assert_eq!(report.readings.len(), MAX_PROBED_PROCESSES);
}

/// A training process with two dataloader workers, one of them with a
/// helper of its own: the descendants at any depth, their CPU time with
/// what reaped ones left, and their memory; its threads, swap and files.
#[test]
fn a_running_process_reports_its_descendants_threads_swap_and_files() {
    let files = MapFiles::with_namespace()
        .dir("/proc", &["1", "42", "50", "51", "52", "60", "self"], true)
        .file(
            "/proc/42/stat",
            &stat_of(
                42,
                1,
                &[(UTIME, 500), (CUTIME, 30), (CSTIME, 10), (THREADS, 24)],
            ),
        )
        .file("/proc/1/stat", &stat_of(1, 0, &[(UTIME, 9_999)]))
        .file(
            "/proc/50/stat",
            &stat_of(50, 42, &[(UTIME, 100), (STIME, 20), (RSS, 1_000)]),
        )
        .file(
            "/proc/51/stat",
            &stat_of(51, 42, &[(UTIME, 200), (CUTIME, 5), (RSS, 2_000)]),
        )
        .file("/proc/52/stat", &stat_of(52, 51, &[(UTIME, 7), (RSS, 10)]))
        .file("/proc/60/stat", &stat_of(60, 1, &[(UTIME, 77), (RSS, 5)]))
        .file("/proc/42/status", "Name:\tpython3\nVmSwap:\t    2048 kB\n")
        .dir("/proc/42/fd", &["0", "1", "2", "3"], false);
    let target = ProbeTarget {
        key: key(42, 900),
        with_text: false,
    };
    let reading = &probe(files, &[target]).readings[0];
    assert_eq!(reading.liveness, ProbeLiveness::Running);
    assert_eq!(reading.threads, Some(24));
    assert_eq!(reading.swap_bytes, Some(2 * 1024 * 1024));
    assert_eq!(reading.open_files, Some(4));
    assert_eq!(
        reading.children,
        Some(ChildUsage {
            processes: 3,
            // Its reaped children's 40, then 120, 205 and 7.
            cpu_ticks: 40 + 120 + 205 + 7,
            rss_bytes: Some(3_010 * 4096),
        })
    );
    assert_eq!(reading.details, None, "details come with the text");
}

/// Without a process list its descendants are unknown, never zero; an
/// unreadable `fd` folder is no count either.
#[test]
fn an_unreadable_process_list_is_no_descendants() {
    let files = MapFiles::with_namespace().file("/proc/42/stat", &stat_of(42, 1, &[]));
    let reading = &probe(files, &[target(42, 900, false)]).readings[0];
    assert_eq!(reading.liveness, ProbeLiveness::Running);
    assert_eq!(reading.children, None);
    assert_eq!(reading.open_files, None);
    assert_eq!(reading.swap_bytes, None);
}

/// A PID reused while `/proc` was read may close a loop; it ends.
#[test]
fn a_parent_loop_in_the_tree_ends() {
    let files = MapFiles::with_namespace()
        .dir("/proc", &["42", "50", "51"], true)
        .file("/proc/42/stat", &stat_of(42, 51, &[]))
        .file("/proc/50/stat", &stat_of(50, 42, &[(UTIME, 1)]))
        .file("/proc/51/stat", &stat_of(51, 50, &[(UTIME, 2)]));
    let reading = &probe(files, &[target(42, 900, false)]).readings[0];
    let children = reading.children.unwrap();
    assert_eq!((children.processes, children.cpu_ticks), (2, 3));
}

/// Read with the text: the parent, the program, the control group and
/// the niceness.
#[test]
fn the_details_name_the_parent_program_and_control_group() {
    let files = MapFiles::with_namespace()
        .file("/proc/42/stat", &stat_of(42, 7, &[(NICE, -5)]))
        .file("/proc/7/comm", "torchrun\n")
        .link("/proc/42/exe", "/venv/bin/python3.12")
        .file(
            "/proc/42/cgroup",
            "12:cpu,cpuacct:/old\n0::/user.slice/user-1000.slice/session-3.scope\n",
        );
    let reading = &probe(files, &[target(42, 900, true)]).readings[0];
    assert_eq!(
        reading.details,
        Some(RunDetails {
            parent_pid: Some(7),
            parent_name: "torchrun".to_owned(),
            executable: "/venv/bin/python3.12".to_owned(),
            cgroup: "/user.slice/user-1000.slice/session-3.scope".to_owned(),
            nice: Some(-5),
        })
    );
    // A process under init with nothing readable.
    let files = MapFiles::with_namespace().file("/proc/42/stat", &stat_of(42, 0, &[]));
    let details = probe(files, &[target(42, 900, true)]).readings[0]
        .details
        .clone()
        .unwrap();
    assert_eq!(
        details,
        RunDetails {
            nice: Some(0),
            ..RunDetails::default()
        }
    );
}

#[test]
fn a_cgroup_v1_host_names_its_first_hierarchy() {
    assert_eq!(
        cgroup_path("4:memory:/docker/abc\n1:name=systemd:/x\n").as_deref(),
        Some("/docker/abc")
    );
    assert_eq!(cgroup_path("").as_deref(), None);
    assert_eq!(cgroup_path("0::/\n").as_deref(), Some("/"));
}

#[test]
fn io_and_vmstat_fields_need_their_whole_name() {
    assert_eq!(
        field_value("oom_kill_x 5\noom_kill 2\n", "oom_kill"),
        Some(2)
    );
    assert_eq!(parse_io("read_bytes: 1\n"), None, "both counters or none");
}

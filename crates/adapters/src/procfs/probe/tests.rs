use std::collections::HashMap;
use std::io;
use std::path::PathBuf;

use dgxtop_core::identity::{Pid, PidNamespace};
use dgxtop_core::testing::{FakeClock, boot_id, host_id};

use super::*;
use crate::common::EntryName;

const NAMESPACE: &str = "pid:[4026531836]";

/// `/proc` as a map: files by path and the targets of links.
#[derive(Default)]
struct MapFiles {
    files: HashMap<String, String>,
    links: HashMap<String, PathBuf>,
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
}

fn missing() -> io::Error {
    io::Error::from(io::ErrorKind::NotFound)
}

impl SystemFiles for MapFiles {
    fn read_to_string(&self, path: &str) -> io::Result<String> {
        self.files.get(path).cloned().ok_or_else(missing)
    }

    fn read_dir(&self, _path: &str) -> io::Result<Vec<EntryName>> {
        Err(missing())
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
    let mut probe = ProcfsProcessProbe::new(Arc::new(files), FakeClock::new(5_000));
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

#[test]
fn io_and_vmstat_fields_need_their_whole_name() {
    assert_eq!(
        field_value("oom_kill_x 5\noom_kill 2\n", "oom_kill"),
        Some(2)
    );
    assert_eq!(parse_io("read_bytes: 1\n"), None, "both counters or none");
}

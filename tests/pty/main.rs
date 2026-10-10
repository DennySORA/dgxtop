//! PTY suite: runs the real `dgxtop` binary inside a pseudo terminal against
//! the shipped replay fixture (simulated data, isolated temporary HOME/XDG
//! directories) and checks keyboard handling, terminal restoration and
//! non-blocking output under a stalled reader (AT11, AT12, AT26, AT39).

// Test helpers may unwrap; pre_exec needs unsafe for setsid/TIOCSCTTY.
#![allow(clippy::unwrap_used, clippy::expect_used, unsafe_code)]

use std::io::{Read, Write};
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::time::{Duration, Instant};

use nix::fcntl::{FcntlArg, FdFlag, OFlag, fcntl};
use nix::pty::{OpenptyResult, Winsize, openpty};

const BINARY: &str = env!("CARGO_BIN_EXE_dgxtop");

fn fixture() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../adapters/fixtures/replay/demo.json")
}

/// Temporary HOME/XDG tree so the binary never touches the real user files.
struct Sandbox {
    root: PathBuf,
}

impl Sandbox {
    fn new(label: &str) -> Self {
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|elapsed| elapsed.as_nanos())
            .unwrap_or_default();
        let root = std::env::temp_dir().join(format!(
            "dgxtop-pty-{label}-{}-{unique}",
            std::process::id()
        ));
        for sub in ["home", "config", "state"] {
            std::fs::create_dir_all(root.join(sub)).unwrap();
        }
        Self { root }
    }

    fn apply(&self, command: &mut Command) {
        command
            .env_clear()
            .env("PATH", std::env::var("PATH").unwrap_or_default())
            .env("HOME", self.root.join("home"))
            .env("XDG_CONFIG_HOME", self.root.join("config"))
            .env("XDG_STATE_HOME", self.root.join("state"))
            .env("TERM", "xterm-256color")
            .env("LANG", "C.UTF-8");
    }
}

impl Drop for Sandbox {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

/// A child process attached to the slave side of a PTY.
struct PtySession {
    child: Child,
    master: std::fs::File,
    output: Vec<u8>,
    _sandbox: Sandbox,
}

impl PtySession {
    fn spawn(label: &str, columns: u16, rows: u16, arguments: &[&str]) -> Self {
        let size = Winsize {
            ws_row: rows,
            ws_col: columns,
            ws_xpixel: 0,
            ws_ypixel: 0,
        };
        let OpenptyResult { master, slave } = openpty(Some(&size), None).unwrap();
        // openpty(3) descriptors are inherited across exec: a child holding a
        // copy of the master would keep the terminal from ever hanging up.
        for descriptor in [&master, &slave] {
            fcntl(descriptor, FcntlArg::F_SETFD(FdFlag::FD_CLOEXEC)).unwrap();
        }
        let flags = OFlag::from_bits_truncate(fcntl(&master, FcntlArg::F_GETFL).unwrap());
        fcntl(&master, FcntlArg::F_SETFL(flags | OFlag::O_NONBLOCK)).unwrap();
        let sandbox = Sandbox::new(label);
        let mut command = Command::new(BINARY);
        command.args(arguments);
        sandbox.apply(&mut command);
        command
            .stdin(Stdio::from(slave.try_clone().unwrap()))
            .stdout(Stdio::from(slave.try_clone().unwrap()))
            .stderr(Stdio::from(slave));
        // SAFETY: only async-signal-safe calls (setsid, ioctl) run between fork and exec.
        unsafe {
            command.pre_exec(|| {
                if libc::setsid() < 0 {
                    return Err(std::io::Error::last_os_error());
                }
                if libc::ioctl(0, libc::TIOCSCTTY, 0) < 0 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
        let child = command.spawn().unwrap();
        Self {
            child,
            master: std::fs::File::from(master),
            output: Vec::new(),
            _sandbox: sandbox,
        }
    }

    /// Reads everything currently available (non-blocking).
    fn drain(&mut self) {
        let mut buffer = [0u8; 65536];
        loop {
            match self.master.read(&mut buffer) {
                Ok(0) => return,
                Ok(read) => self.output.extend_from_slice(&buffer[..read]),
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => return,
                // EIO once the child closed the slave.
                Err(_) => return,
            }
        }
    }

    fn text(&self) -> String {
        strip_escapes(&String::from_utf8_lossy(&self.output))
    }

    fn wait_for_text(&mut self, needle: &str, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            self.drain();
            if self.text().contains(needle) {
                return true;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        false
    }

    fn send(&mut self, bytes: &[u8]) {
        self.master.write_all(bytes).unwrap();
        self.master.flush().unwrap();
    }

    fn resize(&mut self, columns: u16, rows: u16) {
        let size = libc::winsize {
            ws_row: rows,
            ws_col: columns,
            ws_xpixel: 0,
            ws_ypixel: 0,
        };
        // SAFETY: TIOCSWINSZ reads one winsize struct from a valid pointer.
        let result = unsafe { libc::ioctl(self.master.as_raw_fd(), libc::TIOCSWINSZ, &size) };
        assert_eq!(result, 0);
    }

    /// Waits for exit while draining output; `None` on timeout.
    fn wait_exit(&mut self, timeout: Duration) -> Option<ExitStatus> {
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            self.drain();
            if let Some(status) = self.child.try_wait().unwrap() {
                self.drain();
                return Some(status);
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        None
    }
}

impl Drop for PtySession {
    fn drop(&mut self) {
        if self.child.try_wait().ok().flatten().is_none() {
            // Only our own disposable child is ever signalled.
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }
}

/// Removes CSI/OSC escape sequences so assertions see plain text.
fn strip_escapes(input: &str) -> String {
    dgxtop_core::text::sanitize_display(input, usize::MAX)
}

fn replay_args(fixture: &Path) -> Vec<String> {
    vec!["--replay".to_owned(), fixture.display().to_string()]
}

fn spawn_replay(label: &str, columns: u16, rows: u16) -> PtySession {
    let arguments = replay_args(&fixture());
    let borrowed: Vec<&str> = arguments.iter().map(String::as_str).collect();
    PtySession::spawn(label, columns, rows, &borrowed)
}

const STARTUP: Duration = Duration::from_secs(10);

#[test]
#[ignore = "end-to-end PTY run (opt-in): cargo test --test pty -- --ignored"]
fn live_mode_without_gpu_starts_isolated_hosts_and_quits() {
    // Real read-only adapter hosts (procfs, hwmon, thermal, powercap,
    // network, filesystem); NVML is not started (AT01, AT12: no GPU never
    // blocks startup and is explained instead of shown as zero).
    let mut session = PtySession::spawn("live", 160, 50, &["--no-gpu"]);
    assert!(
        session.wait_for_text("Overview", STARTUP),
        "no overview: {}",
        session.text()
    );
    assert!(
        session.wait_for_text("CPU", STARTUP),
        "no CPU panel: {}",
        session.text()
    );
    assert!(
        session.wait_for_text("GPU monitoring disabled", STARTUP),
        "GPU state not explained: {}",
        session.text()
    );
    assert!(
        !session.text().to_lowercase().contains("replay"),
        "live data must not be labeled replay"
    );
    session.send(b"q");
    let status = session
        .wait_exit(Duration::from_secs(5))
        .expect("q did not exit within 5 s");
    assert!(status.success(), "exit status {status:?}");
    assert!(String::from_utf8_lossy(&session.output).contains("\u{1b}[?1049l"));
}

#[test]
#[ignore = "end-to-end PTY run (opt-in): cargo test --test pty -- --ignored"]
fn replay_starts_shows_simulated_label_and_quits_with_q() {
    let mut session = spawn_replay("quit", 160, 50);
    assert!(
        session.wait_for_text("Overview", STARTUP),
        "no overview: {}",
        session.text()
    );
    assert!(
        session.text().to_lowercase().contains("replay"),
        "replay namespace not labeled"
    );
    session.send(b"q");
    let status = session
        .wait_exit(Duration::from_secs(5))
        .expect("q did not exit within 5 s");
    assert!(status.success(), "exit status {status:?}");
    // Alternate screen left: the terminal was restored.
    assert!(String::from_utf8_lossy(&session.output).contains("\u{1b}[?1049l"));
}

#[test]
#[ignore = "end-to-end PTY run (opt-in): cargo test --test pty -- --ignored"]
fn ctrl_c_quits_even_in_filter_mode_where_q_is_text() {
    let mut session = spawn_replay("filter", 160, 50);
    assert!(session.wait_for_text("Overview", STARTUP));
    session.send(b"3");
    session.send(b"/");
    session.send(b"q");
    assert!(
        session.wait_exit(Duration::from_millis(800)).is_none(),
        "q in filter mode must be text"
    );
    session.send(b"\x03");
    let status = session
        .wait_exit(Duration::from_secs(5))
        .expect("Ctrl-C did not exit");
    assert!(status.success());
}

#[test]
#[ignore = "end-to-end PTY run (opt-in): cargo test --test pty -- --ignored"]
fn tiny_terminal_shows_size_notice_and_still_quits() {
    let mut session = spawn_replay("tiny", 30, 8);
    assert!(
        session.wait_for_text("too small", STARTUP),
        "{}",
        session.text()
    );
    session.send(b"q");
    assert!(session.wait_exit(Duration::from_secs(5)).is_some());
}

#[test]
#[ignore = "end-to-end PTY run (opt-in): cargo test --test pty -- --ignored"]
fn resize_churn_keeps_running_and_quits() {
    let mut session = spawn_replay("resize", 160, 50);
    assert!(session.wait_for_text("Overview", STARTUP));
    for (columns, rows) in [(100, 30), (80, 24), (40, 10), (20, 5), (160, 50), (120, 40)] {
        session.resize(columns, rows);
        std::thread::sleep(Duration::from_millis(150));
        session.drain();
    }
    assert!(
        session.child.try_wait().unwrap().is_none(),
        "crashed during resize churn"
    );
    session.send(b"q");
    assert!(session.wait_exit(Duration::from_secs(5)).is_some());
}

#[test]
#[ignore = "end-to-end PTY run (opt-in): cargo test --test pty -- --ignored"]
fn stalled_reader_for_thirty_seconds_does_not_block_quit() {
    let mut session = spawn_replay("stall", 160, 50);
    assert!(session.wait_for_text("Overview", STARTUP));
    // Stop reading the PTY for 30 s: the kernel buffer fills and the
    // application's writes would block if they were not non-blocking.
    std::thread::sleep(Duration::from_secs(30));
    assert!(
        session.child.try_wait().unwrap().is_none(),
        "exited while the reader was stalled"
    );
    session.send(b"q");
    let started = Instant::now();
    let status = session
        .wait_exit(Duration::from_secs(5))
        .expect("q not handled after a 30 s stall");
    assert!(status.success());
    assert!(started.elapsed() < Duration::from_secs(5));
}

#[test]
#[ignore = "end-to-end PTY run (opt-in): cargo test --test pty -- --ignored"]
fn hangup_of_the_terminal_ends_the_process() {
    let mut session = spawn_replay("hangup", 120, 40);
    assert!(session.wait_for_text("Overview", STARTUP));
    // Replace the master with /dev/null so the PTY master closes (EIO on the slave side).
    let null = std::fs::File::open("/dev/null").unwrap();
    // SAFETY: duplicating a valid descriptor into a fresh owned file.
    let replacement = unsafe { std::fs::File::from_raw_fd(libc::dup(null.as_raw_fd())) };
    drop(std::mem::replace(&mut session.master, replacement));
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut exited = false;
    while Instant::now() < deadline {
        if session.child.try_wait().unwrap().is_some() {
            exited = true;
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    assert!(exited, "process kept running after the terminal hung up");
}

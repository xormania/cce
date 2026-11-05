//! Parent-process harness for end-to-end tests: launches the storage driver as a separate
//! process, enforces deadlines, and holds the independent expected-state oracle.

#![allow(dead_code)]

use std::io::{BufRead, BufReader, Write};
use std::process::{Child, ChildStdin, Command, ExitStatus, Stdio};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::time::{Duration, Instant};

use sha2::{Digest, Sha256};

pub mod story;

pub const DRIVER: &str = env!("CARGO_BIN_EXE_storage-driver");

/// Deadline for a single driver step; identifies hangs, never arranges interleavings.
pub const STEP_DEADLINE: Duration = Duration::from_secs(30);

pub mod resources;
pub use resources::test_dir;

/// How a driver step ended.
#[derive(Debug, PartialEq, Eq)]
pub enum Step {
    /// The driver answered with these lines (excluding the terminating `end`).
    Answered(Vec<String>),
    /// The driver exited before answering; lines seen so far and its exit status.
    Exited {
        lines: Vec<String>,
        code: Option<i32>,
    },
    /// No answer before the deadline.
    Hung { lines: Vec<String> },
}

/// A running driver process.
pub struct Driver {
    child: Child,
    stdin: Option<ChildStdin>,
    rx: Receiver<String>,
}

impl Driver {
    pub fn spawn() -> Self {
        Self::spawn_command(Command::new(DRIVER))
    }

    /// Spawns the driver through `bash` with `setup` run first (for example a resource limit);
    /// `exec` keeps the driver as the child the harness talks to and kills. A failing setup
    /// fails closed: the shell exits and the driver never starts.
    pub fn spawn_with_shell(setup: &str) -> Self {
        let mut cmd = Command::new("bash");
        cmd.arg("-c")
            .arg(format!("set -e; {setup}; exec \"$0\""))
            .arg(DRIVER);
        Self::spawn_command(cmd)
    }

    fn spawn_command(mut cmd: Command) -> Self {
        let mut child = cmd
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .expect("launch storage driver");
        let stdout = child.stdout.take().expect("driver stdout");
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines() {
                let Ok(line) = line else { break };
                if tx.send(line).is_err() {
                    break;
                }
            }
        });
        let stdin = child.stdin.take();
        Self { child, stdin, rx }
    }

    /// The driver's process id (the shell, if any, exec'd into the driver).
    pub fn pid(&self) -> u32 {
        self.child.id()
    }

    /// Sends one command and waits for its answer, an exit, or the deadline.
    pub fn step(&mut self, command: &str) -> Step {
        self.step_with_deadline(command, STEP_DEADLINE)
    }

    pub fn step_with_deadline(&mut self, command: &str, deadline: Duration) -> Step {
        if let Some(stdin) = self.stdin.as_mut() {
            let _ = writeln!(stdin, "{command}");
            let _ = stdin.flush();
        }
        let until = Instant::now() + deadline;
        let mut lines = Vec::new();
        loop {
            let left = until.saturating_duration_since(Instant::now());
            match self.rx.recv_timeout(left) {
                Ok(line) if line == "end" => return Step::Answered(lines),
                Ok(line) => lines.push(line),
                Err(RecvTimeoutError::Timeout) => return Step::Hung { lines },
                Err(RecvTimeoutError::Disconnected) => {
                    let status = self.reap(deadline);
                    return Step::Exited {
                        lines,
                        code: status.and_then(|s| s.code()),
                    };
                }
            }
        }
    }

    /// Waits until the driver prints `line` (used for `holding`), without expecting `end`.
    pub fn wait_for(&mut self, command: &str, line: &str) -> Step {
        if let Some(stdin) = self.stdin.as_mut() {
            let _ = writeln!(stdin, "{command}");
            let _ = stdin.flush();
        }
        let until = Instant::now() + STEP_DEADLINE;
        let mut lines = Vec::new();
        loop {
            let left = until.saturating_duration_since(Instant::now());
            match self.rx.recv_timeout(left) {
                Ok(l) if l == line => return Step::Answered(lines),
                Ok(l) => lines.push(l),
                Err(RecvTimeoutError::Timeout) => return Step::Hung { lines },
                Err(RecvTimeoutError::Disconnected) => {
                    let status = self.reap(STEP_DEADLINE);
                    return Step::Exited {
                        lines,
                        code: status.and_then(|s| s.code()),
                    };
                }
            }
        }
    }

    fn reap(&mut self, deadline: Duration) -> Option<ExitStatus> {
        let until = Instant::now() + deadline;
        loop {
            if let Ok(Some(s)) = self.child.try_wait() {
                return Some(s);
            }
            if Instant::now() >= until {
                return None;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    /// Kills the process without any graceful shutdown and confirms it is reaped.
    /// Returns the exit status, which must show termination by signal.
    pub fn kill(mut self) -> ExitStatus {
        self.child.kill().expect("kill driver");
        self.child.wait().expect("reap driver")
    }

    /// Closes stdin and waits for a normal exit.
    pub fn finish(mut self) -> Option<ExitStatus> {
        self.stdin.take();
        self.reap(STEP_DEADLINE)
    }
}

impl Drop for Driver {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Unwraps an answered step, failing the test with what actually happened.
#[track_caller]
pub fn answered(step: Step, what: &str) -> Vec<String> {
    match step {
        Step::Answered(lines) => {
            if let Some(err) = lines.iter().find(|l| l.starts_with("error ")) {
                panic!("{what}: driver reported {err}");
            }
            lines
        }
        other => panic!("{what}: expected an answer, got {other:?}"),
    }
}

/// A live process's peak and current resident memory (KiB) and open file descriptors, read by the
/// parent from Linux `/proc` (the declared platform).
pub fn proc_usage(pid: u32) -> (u64, u64, usize) {
    let status = std::fs::read_to_string(format!("/proc/{pid}/status")).expect("process status");
    let kib = |name: &str| -> u64 {
        status
            .lines()
            .find_map(|l| l.strip_prefix(name))
            .and_then(|v| v.trim().trim_end_matches("kB").trim().parse().ok())
            .unwrap_or_else(|| panic!("no {name} in /proc/{pid}/status"))
    };
    let fds = std::fs::read_dir(format!("/proc/{pid}/fd"))
        .expect("process fds")
        .count();
    (kib("VmHWM:"), kib("VmRSS:"), fds)
}

/// A live process's soft resource limit as Linux `/proc/<pid>/limits` reports it, read by the
/// parent; `None` when it is unlimited.
pub fn proc_limit(pid: u32, name: &str) -> Option<u64> {
    let limits = std::fs::read_to_string(format!("/proc/{pid}/limits")).expect("process limits");
    let soft = limits
        .lines()
        .find_map(|l| l.strip_prefix(name))
        .and_then(|rest| rest.split_whitespace().next())
        .unwrap_or_else(|| panic!("no {name} in /proc/{pid}/limits"));
    (soft != "unlimited").then(|| {
        soft.parse()
            .unwrap_or_else(|_| panic!("{name} soft limit {soft}"))
    })
}

pub fn hex(bytes: &[u8]) -> String {
    if bytes.is_empty() {
        return "-".into();
    }
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Independent mirror of the driver's generated stream (same definition, separate code).
pub fn gen_bytes(seed: u64, len: u64) -> Vec<u8> {
    (0..len)
        .map(|i| {
            let mut x = seed ^ i.wrapping_mul(0x9E37_79B9_7F4A_7C15);
            x ^= x >> 33;
            x = x.wrapping_mul(0xff51_afd7_ed55_8ccd);
            x ^= x >> 33;
            (x & 0xff) as u8
        })
        .collect()
}

pub fn sha256(bytes: &[u8]) -> [u8; 32] {
    Sha256::digest(bytes).into()
}

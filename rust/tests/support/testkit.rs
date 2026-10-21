#![allow(dead_code)]

use std::fs;
use std::io;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

pub const STACK_BYTES: usize = 512 * 1024;
pub const DEFAULT_SEED: u64 = 0x0DDB_1A5E_5BAD_5EED;
const CHILD_ENV: &str = "PDF_TESTKIT_CHILD";
const OK_EXIT: i32 = 80;
const PANIC_EXIT: i32 = 81;
const MEMORY_EXIT: i32 = 82;
const UNSUPPORTED_EXIT: i32 = 83;

#[derive(Clone, Debug)]
pub struct Rng(pub u64);

impl Rng {
    pub fn new(seed: u64) -> Self {
        Self(if seed == 0 { DEFAULT_SEED } else { seed })
    }

    pub fn next(&mut self) -> u64 {
        let mut x = if self.0 == 0 { DEFAULT_SEED } else { self.0 };
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        x
    }

    pub fn below(&mut self, n: usize) -> usize {
        if n == 0 { 0 } else { (self.next() % n as u64) as usize }
    }
}

pub fn mutate(rng: &mut Rng, seed: &[u8], other: &[u8]) -> Vec<u8> {
    const COUNTS: [u32; 12] = [0, 1, 2, 0x7f, 0x80, 0xff, 0x7fff, 0x8000, 0xffff, 0x7fff_ffff, 0x8000_0000, u32::MAX];
    let mut bytes = seed.to_vec();
    for _ in 0..1 + rng.below(4) {
        if bytes.is_empty() {
            bytes.push(rng.next() as u8);
        }
        let at = rng.below(bytes.len());
        match rng.below(8) {
            0 => bytes[at] ^= 1 << rng.below(8),
            1 => bytes[at] = rng.next() as u8,
            2 | 3 => {
                let count = COUNTS[rng.below(COUNTS.len())];
                let field = if rng.below(2) == 0 { count.to_le_bytes() } else { count.to_be_bytes() };
                let n = [1, 2, 4][rng.below(3)].min(bytes.len() - at);
                bytes[at..at + n].copy_from_slice(&field[..n]);
            }
            4 => bytes.truncate(at),
            5 => {
                let n = rng.below(64).min(bytes.len() - at);
                bytes.drain(at..at + n);
            }
            6 => {
                let n = rng.below(64).min(bytes.len() - at);
                let piece = bytes[at..at + n].to_vec();
                for _ in 0..1 + rng.below(8) {
                    bytes.splice(at..at, piece.iter().copied());
                }
            }
            _ if !other.is_empty() => {
                let from = rng.below(other.len());
                let n = rng.below(256).min(other.len() - from);
                bytes.splice(at..at, other[from..from + n].iter().copied());
            }
            _ => {}
        }
    }
    bytes
}

#[derive(Clone, Copy, Debug)]
pub struct Limits {
    pub stack: usize,
    pub deadline: Duration,
    pub rss_bytes: Option<u64>,
}

impl Limits {
    pub fn new(deadline: Duration) -> Self {
        Self { stack: STACK_BYTES, deadline, rss_bytes: None }
    }

    pub fn mutation(deadline: Duration) -> Self {
        Self { rss_bytes: if cfg!(target_os = "linux") { Some(512 << 20) } else { None }, ..Self::new(deadline) }
    }
}

#[derive(Clone, Debug)]
pub struct Report {
    pub elapsed: Duration,
    pub sampled_peak_rss_bytes: Option<u64>,
}

#[derive(Debug)]
pub enum Failure {
    Timeout(Duration),
    MemoryLimit { bytes: u64, limit: u64 },
    Panic,
    Crash(Option<i32>),
    Unsupported(String),
    Protocol(String),
    Io(io::Error),
}

impl std::fmt::Display for Failure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Timeout(t) => write!(f, "TIMEOUT: child killed and reaped after {t:?}"),
            Self::MemoryLimit { bytes, limit } => write!(f, "MEMORY_LIMIT: {bytes} bytes exceeds {limit}"),
            Self::Panic => write!(f, "PANIC: worker panicked"),
            Self::Crash(code) => write!(f, "CRASH: child exit {code:?}"),
            Self::Unsupported(s) => write!(f, "UNSUPPORTED: {s}"),
            Self::Protocol(s) => write!(f, "PROTOCOL: {s}"),
            Self::Io(e) => write!(f, "IO: {e}"),
        }
    }
}

impl std::error::Error for Failure {}

impl From<io::Error> for Failure {
    fn from(error: io::Error) -> Self { Self::Io(error) }
}

pub fn rss_bytes(pid: u32) -> Result<u64, Failure> {
    if !cfg!(target_os = "linux") {
        return Err(Failure::Unsupported("RSS monitoring requires Linux /proc; no memory bound was checked".into()));
    }
    let text = fs::read_to_string(format!("/proc/{pid}/status"))?;
    let value = text.lines().find_map(|line| {
        line.strip_prefix("VmHWM:").and_then(|v| v.split_whitespace().next()).and_then(|v| v.parse::<u64>().ok())
    }).ok_or_else(|| Failure::Unsupported("/proc status has no VmHWM".into()))?;
    value.checked_mul(1024).ok_or_else(|| Failure::Protocol("VmHWM overflow".into()))
}

struct Reap(Child);

impl Reap {
    fn stop(&mut self) -> Result<(), Failure> {
        if self.0.try_wait()?.is_none() {
            self.0.kill()?;
        }
        self.0.wait()?;
        Ok(())
    }
}

impl Drop for Reap {
    fn drop(&mut self) {
        let _ = self.stop();
    }
}

fn worker(limits: Limits, f: impl FnOnce() + Send + 'static) -> ! {
    let status = match std::thread::Builder::new().stack_size(limits.stack).spawn(f) {
        Ok(thread) => match thread.join() {
            Ok(()) => {
                if let Some(limit) = limits.rss_bytes {
                    match rss_bytes(std::process::id()) {
                        Ok(bytes) if bytes > limit => MEMORY_EXIT,
                        Ok(_) => OK_EXIT,
                        Err(_) => UNSUPPORTED_EXIT,
                    }
                } else {
                    OK_EXIT
                }
            }
            Err(_) => PANIC_EXIT,
        },
        Err(_) => UNSUPPORTED_EXIT,
    };
    std::process::exit(status)
}

pub fn run_limited(name: &str, limits: Limits, f: impl FnOnce() + Send + 'static) -> Result<Report, Failure> {
    if limits.stack == 0 || limits.deadline.is_zero() || limits.rss_bytes == Some(0) {
        return Err(Failure::Protocol("limits must be positive".into()));
    }
    if std::env::var(CHILD_ENV).ok().as_deref() == Some(name) {
        worker(limits, f);
    }
    if limits.rss_bytes.is_some() {
        rss_bytes(std::process::id())?;
    } else if !cfg!(target_os = "linux") {
        eprintln!("{name}: RSS_UNSUPPORTED (time and stack only; no memory bound checked)");
    }
    let start = Instant::now();
    let child = Command::new(std::env::current_exe()?)
        .args(["--exact", name, "--nocapture", "--test-threads=1", "--include-ignored"])
        .env(CHILD_ENV, name)
        .stdin(Stdio::null())
        .spawn()?;
    let mut child = Reap(child);
    let mut peak = None;
    loop {
        if let Some(status) = child.0.try_wait()? {
            return match status.code() {
                Some(OK_EXIT) => Ok(Report { elapsed: start.elapsed(), sampled_peak_rss_bytes: peak }),
                Some(PANIC_EXIT) => Err(Failure::Panic),
                Some(MEMORY_EXIT) => Err(Failure::MemoryLimit { bytes: peak.unwrap_or(0), limit: limits.rss_bytes.unwrap_or(0) }),
                Some(UNSUPPORTED_EXIT) => Err(Failure::Unsupported("worker could not enforce its limits".into())),
                Some(0) => Err(Failure::Protocol(format!("no bounded worker ran for exact test {name}"))),
                code => Err(Failure::Crash(code)),
            };
        }
        if start.elapsed() >= limits.deadline {
            child.stop()?;
            return Err(Failure::Timeout(start.elapsed()));
        }
        if cfg!(target_os = "linux") {
            match rss_bytes(child.0.id()) {
                Ok(bytes) => {
                    peak = Some(peak.unwrap_or(0).max(bytes));
                    if let Some(limit) = limits.rss_bytes {
                        if bytes > limit {
                            child.stop()?;
                            return Err(Failure::MemoryLimit { bytes, limit });
                        }
                    }
                }
                Err(_) if child.0.try_wait()?.is_some() => continue,
                Err(error) if limits.rss_bytes.is_some() => return Err(error),
                Err(_) => {}
            }
        }
        std::thread::sleep(Duration::from_millis(2));
    }
}

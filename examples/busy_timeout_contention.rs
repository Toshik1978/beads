//! Multi-process write-contention benchmark for `DEFAULT_BUSY_TIMEOUT_MS`.
//!
//! # Why this exists
//!
//! `src/storage/sqlite.rs` sets `PRAGMA busy_timeout` from
//! `DEFAULT_BUSY_TIMEOUT_MS`. That constant was 0, inherited from a storage
//! engine whose `busy_timeout` was a hot spin loop. C SQLite's busy handler
//! sleeps instead, so the original argument for 0 died with that engine and the
//! value had never been re-derived. Deciding it needs a timing experiment
//! rather than an argument, and this is that experiment; it is what moved the
//! constant to 5000, and it is what should be re-run before moving it again.
//!
//! # What it measures, and on whom
//!
//! The CLI is *not* the population under test. `src/main.rs` serializes every
//! mutating command behind a blocking flock on `.beads/.write.lock` before any
//! database open, and `config/mod.rs` resolves `.or(Some(30000))` before every
//! startup open, so the `br` binary neither reaches this constant nor contends
//! at the SQLite layer. The constant reaches exactly three callers:
//! `SqliteStorage::open`, `build_memory`, and library consumers that call
//! `SqliteStorage` directly. Only the last of those can produce real write
//! contention, so that is what this models: N processes, each holding its own
//! `SqliteStorage` opened at a given busy timeout, hammering `create_issue` on
//! one shared database file with no flock between them.
//!
//! Each cell of the sweep reports throughput (successful writes per second
//! across all writers), p50/p99/max per-write latency, and the failure count —
//! writes that exhausted `with_write_transaction`'s 8 jittered retries. Read
//! the max column, not p99: the distribution is bimodal enough that p99 lands
//! on the knee and hides the tail entirely. See [`RepReport`].
//!
//! # Running it
//!
//! ```sh
//! env -u RUSTUP_TOOLCHAIN cargo run --release --example busy_timeout_contention
//! ```
//!
//! Flags (shown with their defaults): `--timeouts 0,5000,30000`,
//! `--writers 2,4,8,16`, `--writes 40`, `--reps 3`. Contention only becomes
//! visible with enough writes to overlap — the numbers recorded on
//! `DEFAULT_BUSY_TIMEOUT_MS` came from `--writers 2,4,8,16,32 --writes 150`.
//! The `worker` subcommand is how the driver re-invokes itself as a child
//! process; it is not meant to be called by hand.
//!
//! Run it on the machine whose numbers you care about. One run on one host does
//! not settle the question for every host — that is precisely why this is a
//! committed, re-runnable harness rather than a number pasted into a comment.

use beads::model::Issue;
use beads::storage::SqliteStorage;
use std::env;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

/// Busy timeouts to sweep, in milliseconds. 0 is what the constant used to be,
/// 5000 is what it is now (and what `rusqlite` installs at open on its own),
/// 30000 is what the CLI has always resolved to.
const DEFAULT_TIMEOUTS_MS: &[u64] = &[0, 5_000, 30_000];
/// Concurrent writer processes per cell.
const DEFAULT_WRITERS: &[usize] = &[2, 4, 8, 16];
/// `create_issue` calls per writer process.
const DEFAULT_WRITES: usize = 40;
/// Repetitions per cell; reported values are the median across them.
const DEFAULT_REPS: usize = 3;
/// How far ahead of spawn the shared start instant is placed, so every writer
/// is already open and parked before any of them writes.
const START_DELAY_MS: u64 = 400;

struct Options {
    timeouts_ms: Vec<u64>,
    writers: Vec<usize>,
    writes: usize,
    reps: usize,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            timeouts_ms: DEFAULT_TIMEOUTS_MS.to_vec(),
            writers: DEFAULT_WRITERS.to_vec(),
            writes: DEFAULT_WRITES,
            reps: DEFAULT_REPS,
        }
    }
}

/// One writer process's contribution to a cell.
struct WorkerReport {
    latencies_us: Vec<u64>,
    failures: usize,
}

/// One (timeout, writers) cell, aggregated over all its writers.
///
/// `max_us` is reported alongside `p99_us` because the latency distribution
/// under `busy_timeout=0` is sharply bimodal: nearly every write wins its race
/// and costs ~0.1 ms, while the few that lose one pay the retry loop's 50 ms
/// backoff floor or a multiple of it. At that shape p99 lands on the knee and
/// understates the tail by three orders of magnitude, so it alone would hide
/// the very effect this benchmark exists to measure.
struct RepReport {
    throughput_per_sec: f64,
    p50_us: u64,
    p99_us: u64,
    max_us: u64,
    failures: usize,
}

fn parse_u64_list(raw: &str) -> Vec<u64> {
    raw.split(',')
        .filter_map(|item| item.trim().parse().ok())
        .collect()
}

fn parse_usize_list(raw: &str) -> Vec<usize> {
    raw.split(',')
        .filter_map(|item| item.trim().parse().ok())
        .collect()
}

fn parse_options(args: &[String]) -> Options {
    let mut options = Options::default();
    let mut index = 0;
    while index < args.len() {
        let flag = args[index].as_str();
        let value = args.get(index + 1).map(String::as_str).unwrap_or_default();
        match flag {
            "--timeouts" => options.timeouts_ms = parse_u64_list(value),
            "--writers" => options.writers = parse_usize_list(value),
            "--writes" => options.writes = value.parse().unwrap_or(DEFAULT_WRITES),
            "--reps" => options.reps = value.parse().unwrap_or(DEFAULT_REPS),
            other => {
                eprintln!("unknown flag '{other}'");
                std::process::exit(2);
            }
        }
        index += 2;
    }
    options
}

fn millis_since_epoch() -> u64 {
    u64::try_from(
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis(),
    )
    .unwrap_or(u64::MAX)
}

/// Park until the shared start instant so all writers contend from the same
/// moment instead of staggering by process-spawn cost.
fn sleep_until(start_millis: u64) {
    let now = millis_since_epoch();
    if start_millis > now {
        std::thread::sleep(Duration::from_millis(start_millis - now));
    }
}

fn percentile(sorted_us: &[u64], fraction: f64) -> u64 {
    if sorted_us.is_empty() {
        return 0;
    }
    let last = sorted_us.len() - 1;
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let index = ((sorted_us.len() as f64 * fraction).ceil() as usize).saturating_sub(1);
    sorted_us[index.min(last)]
}

fn median_f64(values: &mut [f64]) -> f64 {
    if values.is_empty() {
        return 0.0;
    }
    values.sort_by(f64::total_cmp);
    values[values.len() / 2]
}

fn median_u64(values: &mut [u64]) -> u64 {
    if values.is_empty() {
        return 0;
    }
    values.sort_unstable();
    values[values.len() / 2]
}

/// The child process: open at `timeout_ms`, wait for the shared start, write.
fn run_worker(args: &[String]) {
    let db_path = PathBuf::from(&args[0]);
    let timeout_ms: u64 = args[1].parse().expect("timeout_ms");
    let writes: usize = args[2].parse().expect("writes");
    let index: usize = args[3].parse().expect("worker index");
    let start_millis: u64 = args[4].parse().expect("start millis");

    let mut storage =
        SqliteStorage::open_with_timeout(&db_path, Some(timeout_ms)).expect("open storage");

    sleep_until(start_millis);

    let mut latencies_us = Vec::with_capacity(writes);
    let mut failures = 0usize;
    for sequence in 0..writes {
        let issue = Issue {
            id: format!("bench-{index}-{sequence}"),
            title: format!("busy timeout contention probe {index}/{sequence}"),
            ..Issue::default()
        };
        let began = Instant::now();
        let result = storage.create_issue(&issue, "busy-timeout-bench");
        let elapsed = u64::try_from(began.elapsed().as_micros()).unwrap_or(u64::MAX);
        if result.is_ok() {
            latencies_us.push(elapsed);
        } else {
            failures += 1;
        }
    }

    // One line, parsed by the driver: failures, then the latencies.
    let rendered: Vec<String> = latencies_us.iter().map(u64::to_string).collect();
    println!("{failures} {}", rendered.join(","));
}

fn parse_worker_line(line: &str) -> Option<WorkerReport> {
    let mut parts = line.split_whitespace();
    let failures = parts.next()?.parse().ok()?;
    let latencies_us = parts
        .next()
        .map(|raw| {
            raw.split(',')
                .filter_map(|item| item.parse().ok())
                .collect()
        })
        .unwrap_or_default();
    Some(WorkerReport {
        latencies_us,
        failures,
    })
}

/// Pre-apply the schema from the parent so the writers race on writes rather
/// than on first-open schema application.
fn prepare_database(db_path: &Path) {
    drop(SqliteStorage::open(db_path).expect("prepare database"));
}

fn run_rep(exe: &Path, timeout_ms: u64, writers: usize, writes: usize, cell: usize) -> RepReport {
    let dir = env::temp_dir().join(format!("beads_busy_bench_{}_{cell}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("bench dir");
    let db_path = dir.join("contention.db");
    prepare_database(&db_path);

    let start_millis = millis_since_epoch() + START_DELAY_MS;
    let children: Vec<_> = (0..writers)
        .map(|index| {
            Command::new(exe)
                .arg("worker")
                .arg(&db_path)
                .arg(timeout_ms.to_string())
                .arg(writes.to_string())
                .arg(index.to_string())
                .arg(start_millis.to_string())
                .stdout(std::process::Stdio::piped())
                .spawn()
                .expect("spawn worker")
        })
        .collect();

    sleep_until(start_millis);
    let wall_began = Instant::now();

    let mut reports = Vec::with_capacity(writers);
    for child in children {
        let output = child.wait_with_output().expect("worker output");
        let stdout = String::from_utf8_lossy(&output.stdout);
        let report = stdout
            .lines()
            .find_map(parse_worker_line)
            .expect("worker should emit a report line");
        reports.push(report);
    }
    let wall = wall_began.elapsed();
    let _ = std::fs::remove_dir_all(&dir);

    let mut latencies_us: Vec<u64> = reports
        .iter()
        .flat_map(|report| report.latencies_us.iter().copied())
        .collect();
    latencies_us.sort_unstable();
    let failures = reports.iter().map(|report| report.failures).sum();
    let seconds = wall.as_secs_f64().max(f64::MIN_POSITIVE);

    RepReport {
        throughput_per_sec: latencies_us.len() as f64 / seconds,
        p50_us: percentile(&latencies_us, 0.50),
        p99_us: percentile(&latencies_us, 0.99),
        max_us: latencies_us.last().copied().unwrap_or(0),
        failures,
    }
}

fn run_driver(options: &Options) {
    let exe = env::current_exe().expect("current exe");
    println!(
        "busy_timeout contention sweep: {} write(s)/writer, {} rep(s)/cell, median reported",
        options.writes, options.reps
    );
    println!();
    println!("| busy_timeout | writers | writes/s | p50 | p99 | max | failures |");
    println!("| ---: | ---: | ---: | ---: | ---: | ---: | ---: |");

    let mut cell = 0usize;
    for &timeout_ms in &options.timeouts_ms {
        for &writers in &options.writers {
            let mut throughputs = Vec::with_capacity(options.reps);
            let mut p50s = Vec::with_capacity(options.reps);
            let mut p99s = Vec::with_capacity(options.reps);
            let mut maxes = Vec::with_capacity(options.reps);
            let mut failures = 0usize;
            for _ in 0..options.reps {
                cell += 1;
                let rep = run_rep(&exe, timeout_ms, writers, options.writes, cell);
                throughputs.push(rep.throughput_per_sec);
                p50s.push(rep.p50_us);
                p99s.push(rep.p99_us);
                maxes.push(rep.max_us);
                failures += rep.failures;
            }
            println!(
                "| {timeout_ms} ms | {writers} | {:.0} | {:.1} ms | {:.1} ms | {:.1} ms | {failures} |",
                median_f64(&mut throughputs),
                median_u64(&mut p50s) as f64 / 1000.0,
                median_u64(&mut p99s) as f64 / 1000.0,
                median_u64(&mut maxes) as f64 / 1000.0,
            );
        }
    }
}

fn main() {
    let args: Vec<String> = env::args().skip(1).collect();
    if args.first().map(String::as_str) == Some("worker") {
        run_worker(&args[1..]);
        return;
    }
    run_driver(&parse_options(&args));
}

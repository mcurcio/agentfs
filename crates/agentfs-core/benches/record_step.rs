//! The synchronous step's cost (design §3.2, §18 test 13, §19.2 item 1),
//! without FUSE: what a change boundary pays before its call returns.
//!
//! ```sh
//! cargo bench -p agentfs-core --bench record_step
//! ```
//!
//! Cases, each timed per call and reported as percentiles in microseconds:
//!
//! - `freeze`: [`PendingStore::freeze`] alone of a clean (fsynced) file
//!   of each size, with the method it used (`reflink` on XFS and btrfs,
//!   else `copy`);
//! - `record close`: [`Recorder::record`] of a write of each size, one
//!   thread: the path rules, claims, record-time checks, the freeze, the
//!   journal append and its group-commit `fsync`;
//! - `record close 4k`, with each thread count: group commit under
//!   concurrency;
//! - `record unlink`: a removal of a tracked path (journal and `fsync`,
//!   nothing frozen).
//!
//! The commit loop runs in the background as in a mount
//! ([`Service::start`]); the bench waits for it to catch up between cases
//! so they do not interfere.
//!
//! Configured by the environment (docs/performance.md):
//!
//! - `AGENTFS_BENCH_DIR`: where the volume goes (default the system temp
//!   directory); its filesystem is what is measured;
//! - `AGENTFS_BENCH_SIZES`: comma-separated sizes (default
//!   `4k,64k,1m,16m`);
//! - `AGENTFS_BENCH_N`: calls per case (default 500), scaled down for
//!   large sizes so about 512 MiB is frozen per case, and at least 3;
//! - `AGENTFS_BENCH_THREADS`: thread counts for `record close 4k`
//!   (default `1,8`);
//! - `AGENTFS_BENCH_JSON`: a file to append one JSON object per case to;
//! - `AGENTFS_BENCH_EXPECT_FREEZE`: `reflink` or `copy`; any other freeze
//!   method exits 1 (CI's proof that XFS reflinks).

use std::fs::{File, OpenOptions};
use std::io::Write as _;
use std::os::unix::fs::FileExt as _;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::Arc;
use std::time::{Duration, Instant};

use agentfs_core::config::parse_size;
use agentfs_core::events::{Events, NullSink};
use agentfs_core::identity::Context;
use agentfs_core::paths::RepoPath;
use agentfs_core::pending::{FreezeMethod, PendingStore};
use agentfs_core::record::{Caller, EntryPoint, Trigger, Writer};
use agentfs_core::service::Service;
use agentfs_core::telemetry::Telemetry;
use agentfs_core::volume::{self, Input, Recorder, StateDir};

const CALLER: Caller = Caller {
    uid: 1000,
    gid: 1000,
    pid: 1,
};
const WRITER: Writer = Writer {
    caller: CALLER,
    entry: EntryPoint::Main,
};

/// About this much is frozen per size-scaled case.
const BYTES_PER_CASE: u64 = 512 << 20;

/// How long to wait for the commit loop to catch up after a case.
const CATCH_UP: Duration = Duration::from_secs(1800);

struct Options {
    dir: PathBuf,
    sizes: Vec<(String, u64)>,
    n: usize,
    threads: Vec<usize>,
    json: Option<PathBuf>,
    expect_freeze: Option<String>,
}

impl Options {
    fn from_env() -> Result<Self, String> {
        let var = |name: &str| std::env::var(name).ok().filter(|v| !v.trim().is_empty());
        let sizes = var("AGENTFS_BENCH_SIZES")
            .unwrap_or_else(|| "4k,64k,1m,16m".to_owned())
            .split(',')
            .map(|s| {
                let s = s.trim();
                parse_size(s)
                    .map(|n| (s.to_owned(), n))
                    .ok_or_else(|| format!("AGENTFS_BENCH_SIZES: {s:?} is not a size"))
            })
            .collect::<Result<_, _>>()?;
        let threads = var("AGENTFS_BENCH_THREADS")
            .unwrap_or_else(|| "1,8".to_owned())
            .split(',')
            .map(|t| match t.trim().parse::<usize>() {
                Ok(n) if n > 0 => Ok(n),
                _ => Err(format!(
                    "AGENTFS_BENCH_THREADS: {t:?} is not a thread count"
                )),
            })
            .collect::<Result<_, _>>()?;
        let n = match var("AGENTFS_BENCH_N") {
            None => 500,
            Some(n) => n
                .trim()
                .parse()
                .ok()
                .filter(|n| *n > 0)
                .ok_or_else(|| format!("AGENTFS_BENCH_N: {n:?} is not a count"))?,
        };
        let expect_freeze = var("AGENTFS_BENCH_EXPECT_FREEZE");
        if let Some(e) = &expect_freeze
            && !matches!(e.as_str(), "reflink" | "copy")
        {
            return Err(format!(
                "AGENTFS_BENCH_EXPECT_FREEZE: {e:?} is not reflink or copy"
            ));
        }
        Ok(Self {
            dir: var("AGENTFS_BENCH_DIR")
                .map(PathBuf::from)
                .unwrap_or_else(std::env::temp_dir),
            sizes,
            n,
            threads,
            json: var("AGENTFS_BENCH_JSON").map(PathBuf::from),
            expect_freeze,
        })
    }

    /// Calls for a case freezing `size` bytes each.
    fn calls(&self, size: u64) -> usize {
        let scaled = (BYTES_PER_CASE / size.max(1)) as usize;
        self.n.min(scaled).max(3)
    }
}

/// One case's results.
struct Case {
    name: String,
    size: Option<u64>,
    threads: usize,
    freeze: Option<FreezeMethod>,
    samples: Vec<Duration>,
}

impl Case {
    fn percentile(&self, sorted: &[Duration], q: f64) -> f64 {
        let i = ((sorted.len() - 1) as f64 * q).round() as usize;
        sorted[i].as_secs_f64() * 1e6
    }

    fn summary(&self) -> [f64; 5] {
        let mut sorted = self.samples.clone();
        sorted.sort();
        let mean =
            sorted.iter().map(Duration::as_secs_f64).sum::<f64>() / sorted.len() as f64 * 1e6;
        [
            self.percentile(&sorted, 0.5),
            self.percentile(&sorted, 0.9),
            self.percentile(&sorted, 0.99),
            sorted.last().map_or(0.0, |d| d.as_secs_f64() * 1e6),
            mean,
        ]
    }
}

fn method_name(method: Option<FreezeMethod>) -> &'static str {
    match method {
        Some(FreezeMethod::Reflink) => "reflink",
        Some(FreezeMethod::Copy) => "copy",
        None => "",
    }
}

/// The filesystem type of `path`, from `statfs`.
fn fs_type(path: &Path) -> String {
    let Ok(st) = rustix::fs::statfs(path) else {
        return "unknown".to_owned();
    };
    #[allow(clippy::unnecessary_cast)]
    match (st.f_type as i64 as u64) & 0xffff_ffff {
        0x5846_5342 => "xfs".to_owned(),
        0xef53 => "ext4".to_owned(),
        0x9123_683e => "btrfs".to_owned(),
        0x794c_7630 => "overlayfs".to_owned(),
        0x0102_1994 => "tmpfs".to_owned(),
        0x6573_5546 => "fuse".to_owned(),
        other => format!("0x{other:x}"),
    }
}

fn kernel() -> String {
    std::fs::read_to_string("/proc/sys/kernel/osrelease")
        .unwrap_or_default()
        .trim()
        .to_owned()
}

/// Writes `size` bytes of incompressible content (xorshift) to `path`.
fn write_file(path: &Path, size: u64, seed: u64) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut file = File::create(path)?;
    let mut x = seed | 1;
    let mut block = vec![0u8; (1 << 20).min(size as usize).max(1)];
    for chunk in block.chunks_mut(8) {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        let bytes = x.to_le_bytes();
        chunk.copy_from_slice(&bytes[..chunk.len()]);
    }
    let mut left = size;
    let mut n = 0u64;
    while left > 0 {
        let len = left.min(block.len() as u64) as usize;
        // Each block differs, so nothing repeats within a file.
        block[..8.min(len)].copy_from_slice(&n.to_le_bytes()[..8.min(len)]);
        file.write_all(&block[..len])?;
        left -= len as u64;
        n += 1;
    }
    file.sync_all()
}

/// Changes the first bytes of `path` in place, so it is a new version.
fn touch(path: &Path, version: u64) -> std::io::Result<()> {
    let file = OpenOptions::new().write(true).open(path)?;
    file.write_all_at(&version.to_le_bytes(), 0)
}

struct Bench {
    options: Options,
    tree: PathBuf,
    scratch: PathBuf,
    recorder: Recorder,
    cases: Vec<Case>,
}

impl Bench {
    fn write_input(&self, rel: &str) -> Input {
        Input::Write {
            path: RepoPath::new(rel).expect("bench paths are valid"),
            backing: self.tree.join(rel),
            executable: false,
        }
    }

    /// Waits for the commit loop to commit everything recorded so far.
    fn catch_up(&self) -> Result<(), String> {
        let Some(seq) = self.recorder.last_seq() else {
            return Ok(());
        };
        if self.recorder.wait_committed(seq, CATCH_UP) {
            Ok(())
        } else {
            Err(format!(
                "the commit loop did not reach seq {seq} in {CATCH_UP:?}"
            ))
        }
    }

    fn freeze(&mut self, label: &str, size: u64) -> Result<(), String> {
        let store = PendingStore::open(self.scratch.join("pending"))
            .map_err(|e| format!("opening the scratch pending store: {e}"))?;
        let src = self.scratch.join(format!("freeze-{label}"));
        write_file(&src, size, size).map_err(|e| format!("writing {}: {e}", src.display()))?;
        let mut samples = Vec::new();
        let mut method = None;
        for _ in 0..self.options.calls(size) {
            let start = Instant::now();
            let (id, m) = store.freeze(&src).map_err(|e| format!("freeze: {e}"))?;
            samples.push(start.elapsed());
            method = Some(m);
            store
                .remove([id])
                .map_err(|e| format!("removing a copy: {e}"))?;
        }
        std::fs::remove_file(&src).map_err(|e| e.to_string())?;
        self.cases.push(Case {
            name: format!("freeze {label}"),
            size: Some(size),
            threads: 1,
            freeze: method,
            samples,
        });
        Ok(())
    }

    fn record_close(&mut self, label: &str, size: u64) -> Result<(), String> {
        let rel = format!("close-{label}/file");
        let path = self.tree.join(&rel);
        write_file(&path, size, size ^ 0x5eed)
            .map_err(|e| format!("writing {}: {e}", path.display()))?;
        let mut samples = Vec::new();
        for i in 0..self.options.calls(size) {
            touch(&path, i as u64 + 1).map_err(|e| e.to_string())?;
            let input = self.write_input(&rel);
            let start = Instant::now();
            self.recorder
                .record(Trigger::Close, WRITER, vec![input])
                .map_err(|e| format!("record: {e}"))?;
            samples.push(start.elapsed());
        }
        self.catch_up()?;
        std::fs::remove_file(&path).map_err(|e| e.to_string())?;
        self.cases.push(Case {
            name: format!("record close {label}"),
            size: Some(size),
            threads: 1,
            freeze: None,
            samples,
        });
        Ok(())
    }

    fn record_threads(&mut self, threads: usize) -> Result<(), String> {
        let n = self.options.n;
        let content = vec![b'x'; 4096];
        let per = n.div_ceil(threads);
        let results: Vec<Result<Vec<Duration>, String>> = std::thread::scope(|s| {
            let handles: Vec<_> = (0..threads)
                .map(|t| {
                    let content = &content;
                    let this = &*self;
                    s.spawn(move || {
                        let mut samples = Vec::with_capacity(per);
                        for i in 0..per {
                            let rel = format!("threads-{threads}/{t}/f{}", i % 50);
                            let path = this.tree.join(&rel);
                            if let Some(parent) = path.parent() {
                                std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
                            }
                            let mut data = content.clone();
                            data[..8].copy_from_slice(&(i as u64).to_le_bytes());
                            std::fs::write(&path, &data).map_err(|e| e.to_string())?;
                            let input = this.write_input(&rel);
                            let start = Instant::now();
                            this.recorder
                                .record(Trigger::Close, WRITER, vec![input])
                                .map_err(|e| format!("record: {e}"))?;
                            samples.push(start.elapsed());
                        }
                        Ok(samples)
                    })
                })
                .collect();
            handles
                .into_iter()
                .map(|h| h.join().unwrap_or_else(|_| Err("a thread panicked".into())))
                .collect()
        });
        let mut samples = Vec::new();
        for r in results {
            samples.extend(r?);
        }
        self.catch_up()?;
        self.cases.push(Case {
            name: format!("record close 4k, {threads} thread(s)"),
            size: Some(4096),
            threads,
            freeze: None,
            samples,
        });
        Ok(())
    }

    fn record_unlink(&mut self) -> Result<(), String> {
        let n = self.options.n;
        for i in 0..n {
            let rel = format!("unlink/f{i}");
            let path = self.tree.join(&rel);
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
            }
            std::fs::write(&path, b"to be removed").map_err(|e| e.to_string())?;
            let input = self.write_input(&rel);
            self.recorder
                .record(Trigger::Close, WRITER, vec![input])
                .map_err(|e| format!("record: {e}"))?;
        }
        self.catch_up()?;
        let mut samples = Vec::with_capacity(n);
        for i in 0..n {
            let rel = format!("unlink/f{i}");
            std::fs::remove_file(self.tree.join(&rel)).map_err(|e| e.to_string())?;
            let input = Input::Remove {
                path: RepoPath::new(&rel).expect("bench paths are valid"),
            };
            let start = Instant::now();
            let recorded = self
                .recorder
                .record(Trigger::Unlink, WRITER, vec![input])
                .map_err(|e| format!("record: {e}"))?;
            samples.push(start.elapsed());
            if recorded.seq.is_none() {
                return Err(format!("the removal of {rel} was not recorded"));
            }
        }
        self.catch_up()?;
        self.cases.push(Case {
            name: "record unlink".to_owned(),
            size: None,
            threads: 1,
            freeze: None,
            samples,
        });
        Ok(())
    }
}

fn run() -> Result<ExitCode, String> {
    let options = Options::from_env()?;
    std::fs::create_dir_all(&options.dir)
        .map_err(|e| format!("creating {}: {e}", options.dir.display()))?;
    let work = tempfile::Builder::new()
        .prefix("agentfs-bench-")
        .tempdir_in(&options.dir)
        .map_err(|e| format!("a directory in {}: {e}", options.dir.display()))?;
    let fs = fs_type(work.path());
    let state = StateDir::new(work.path().join("state"));
    let tree = work.path().join("tree");
    let scratch = work.path().join("scratch");
    for dir in [&tree, &scratch] {
        std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    }
    volume::init(&state, "bench", None).map_err(|e| format!("init: {e}"))?;
    {
        // No repack in the middle of a case.
        let mut config =
            git2::Config::open(&state.git().join("config")).map_err(|e| format!("config: {e}"))?;
        config
            .set_str("agentfs.repackInterval", "off")
            .map_err(|e| format!("config: {e}"))?;
    }
    let events: Events = Arc::new(NullSink);
    let context = Context {
        volume: "bench".into(),
        replica: "bench".into(),
        pod_uid: "bench".into(),
        ..Context::default()
    };
    let (recorder, committer, _) =
        volume::open(&state, &tree, context, events.clone(), &Telemetry::noop())
            .map_err(|e| format!("open: {e}"))?;
    let service = Service::start(committer, Vec::new(), events)
        .map_err(|e| format!("starting the commit loop: {e}"))?;

    eprintln!(
        "record_step: {} on {fs}, kernel {}, n={}, sizes {:?}, threads {:?}",
        work.path().display(),
        kernel(),
        options.n,
        options.sizes.iter().map(|(l, _)| l).collect::<Vec<_>>(),
        options.threads
    );
    let mut bench = Bench {
        options,
        tree,
        scratch,
        recorder,
        cases: Vec::new(),
    };
    let sizes = bench.options.sizes.clone();
    for (label, size) in &sizes {
        eprintln!("freeze {label}");
        bench.freeze(label, *size)?;
    }
    for (label, size) in &sizes {
        eprintln!("record close {label}");
        bench.record_close(label, *size)?;
    }
    for threads in bench.options.threads.clone() {
        eprintln!("record close 4k, {threads} thread(s)");
        bench.record_threads(threads)?;
    }
    eprintln!("record unlink");
    bench.record_unlink()?;
    let report = service.shutdown(Duration::from_secs(5));
    report
        .final_commit
        .map_err(|e| format!("final commit: {e}"))?;

    let kernel = kernel();
    println!("### Record step on {fs} (kernel {kernel}), microseconds");
    println!();
    println!("| Case | Freeze | n | p50 | p90 | p99 | max | mean |");
    println!("|---|---|---|---|---|---|---|---|");
    let mut json = Vec::new();
    for case in &bench.cases {
        let [p50, p90, p99, max, mean] = case.summary();
        println!(
            "| {} | {} | {} | {p50:.0} | {p90:.0} | {p99:.0} | {max:.0} | {mean:.0} |",
            case.name,
            method_name(case.freeze),
            case.samples.len()
        );
        json.push(serde_json::json!({
            "suite": "record_step",
            "fs": fs,
            "kernel": kernel,
            "case": case.name,
            "size": case.size,
            "threads": case.threads,
            "freeze": case.freeze.map(|m| method_name(Some(m))),
            "n": case.samples.len(),
            "p50_us": p50,
            "p90_us": p90,
            "p99_us": p99,
            "max_us": max,
            "mean_us": mean,
        }));
    }
    if let Some(path) = &bench.options.json {
        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .map_err(|e| format!("opening {}: {e}", path.display()))?;
        for line in &json {
            writeln!(file, "{line}").map_err(|e| e.to_string())?;
        }
    }
    if let Some(expected) = &bench.options.expect_freeze {
        let wrong: Vec<&str> = bench
            .cases
            .iter()
            .filter(|c| c.freeze.is_some() && method_name(c.freeze) != expected)
            .map(|c| c.name.as_str())
            .collect();
        if !wrong.is_empty() {
            eprintln!(
                "AGENTFS_BENCH_EXPECT_FREEZE={expected}, but these froze otherwise on {fs}: \
                 {wrong:?}"
            );
            return Ok(ExitCode::FAILURE);
        }
        eprintln!("every freeze was a {expected}, as expected on {fs}");
    }
    Ok(ExitCode::SUCCESS)
}

fn main() -> ExitCode {
    // `cargo bench` passes `--bench`; `cargo test --benches` would pass
    // test-harness flags, which this bench ignores.
    match run() {
        Ok(code) => code,
        Err(e) => {
            eprintln!("record_step: {e}");
            ExitCode::FAILURE
        }
    }
}

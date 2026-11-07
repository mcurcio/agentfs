//! Replays the model's traces: `phase1-replay model|library|mount DIR
//! --min-scenarios N --min-seeds N [--only NAME]`. `DIR` holds the `*.trace` files
//! the model wrote (`AGENTFS_MODEL_TRACE_DIR`). Fewer in-scope traces than a floor
//! fail: a scenario that left the scope is a drop of coverage, not a pass. Every
//! mismatch is a finding: this prints each trace's first and exits 1, unless
//! `open-findings.txt` names that trace with its finding (VERIFYING.md, "What the
//! ignored tests are expected to show"), which then must still fail.

use agentfs_conformance::{against_library, against_model, against_mount, mountable};
use agentfs_model::calls::{Recorded, Scope, parse, scope_of};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::Mutex;

#[derive(Clone, Copy, PartialEq)]
enum Mode {
    Model,
    Library,
    Mount,
}

struct Args {
    mode: Mode,
    dir: PathBuf,
    min_scenarios: usize,
    min_seeds: usize,
    only: Option<String>,
}

fn args() -> Result<Args, String> {
    let mut it = std::env::args().skip(1);
    let usage = "usage: phase1-replay model|library|mount DIR --min-scenarios N --min-seeds N [--only NAME]";
    let mode = match it.next().as_deref() {
        Some("model") => Mode::Model,
        Some("library") => Mode::Library,
        Some("mount") => Mode::Mount,
        _ => return Err(usage.into()),
    };
    let dir = PathBuf::from(it.next().ok_or(usage)?);
    let (mut min_scenarios, mut min_seeds, mut only) = (None, None, None);
    while let Some(flag) = it.next() {
        let value = it.next().ok_or(usage)?;
        let number = || value.parse::<usize>().map_err(|_| usage.to_owned());
        match flag.as_str() {
            "--min-scenarios" => min_scenarios = Some(number()?),
            "--min-seeds" => min_seeds = Some(number()?),
            "--only" => only = Some(value),
            _ => return Err(usage.into()),
        }
    }
    Ok(Args {
        mode,
        dir,
        min_scenarios: min_scenarios.ok_or(usage)?,
        min_seeds: min_seeds.ok_or(usage)?,
        only,
    })
}

/// The traces that fail for an open finding: trace name to finding.
fn open_findings() -> BTreeMap<&'static str, &'static str> {
    include_str!("../../open-findings.txt")
        .lines()
        .filter(|l| !l.starts_with('#') && !l.is_empty())
        .filter_map(|l| l.split_once(' '))
        .map(|(finding, name)| (name, finding))
        .collect()
}

fn is_seed(name: &str) -> bool {
    name.contains(".seed")
}

fn main() -> ExitCode {
    let args = match args() {
        Ok(a) => a,
        Err(e) => {
            eprintln!("{e}");
            return ExitCode::from(2);
        }
    };
    let bin = std::env::var_os("AGENTFS_BIN").map(PathBuf::from);
    if args.mode == Mode::Mount && bin.is_none() {
        eprintln!("mount needs AGENTFS_BIN: a failpoints build of agentfs");
        return ExitCode::from(2);
    }
    let mut names: Vec<String> = match std::fs::read_dir(&args.dir) {
        Ok(entries) => entries
            .flatten()
            .filter_map(|e| e.file_name().into_string().ok())
            .filter(|n| n.ends_with(".trace"))
            .collect(),
        Err(e) => {
            eprintln!("{}: {e}", args.dir.display());
            return ExitCode::from(2);
        }
    };
    names.sort();
    let mut excluded: BTreeMap<String, usize> = BTreeMap::new();
    let mut work: Vec<(String, Vec<Recorded>)> = Vec::new();
    for name in names {
        if args
            .only
            .as_ref()
            .is_some_and(|o| !name.contains(o.as_str()))
        {
            continue;
        }
        let text = match std::fs::read_to_string(args.dir.join(&name)) {
            Ok(t) => t,
            Err(e) => {
                eprintln!("{name}: {e}");
                return ExitCode::from(2);
            }
        };
        let trace = match parse(&text) {
            Ok(t) => t,
            Err(e) => {
                eprintln!("{name}: {e}");
                return ExitCode::from(2);
            }
        };
        match scope_of(&trace) {
            Scope::OutOfScope(why) => *excluded.entry(why.to_owned()).or_default() += 1,
            Scope::Phase1 if args.mode == Mode::Mount && !mountable(&trace) => {
                *excluded.entry("not mountable".into()).or_default() += 1;
            }
            Scope::Phase1 => work.push((name, trace)),
        }
    }
    // A mount is real calls against the one host's kernel, one trace at a time; the
    // library and the model take every core.
    let cores = std::thread::available_parallelism().map_or(1, |n| n.get());
    let threads = if args.mode == Mode::Mount { 1 } else { cores };
    let next = Mutex::new(work.iter());
    let failures = Mutex::new(Vec::new());
    let passed = Mutex::new((0usize, 0usize));
    std::thread::scope(|s| {
        for _ in 0..threads {
            s.spawn(|| {
                loop {
                    let item = next.lock().unwrap_or_else(|e| e.into_inner()).next();
                    let Some((name, trace)) = item else {
                        return;
                    };
                    let result = match args.mode {
                        Mode::Model => against_model(trace),
                        Mode::Library => against_library(trace),
                        Mode::Mount => {
                            against_mount(trace, bin.as_deref().unwrap_or(Path::new("")))
                        }
                    };
                    match result {
                        Ok(()) => {
                            let mut p = passed.lock().unwrap_or_else(|e| e.into_inner());
                            if is_seed(name) {
                                p.1 += 1;
                            } else {
                                p.0 += 1;
                            }
                        }
                        Err(e) => failures
                            .lock()
                            .unwrap_or_else(|e| e.into_inner())
                            .push((name.clone(), e)),
                    }
                }
            });
        }
    });
    let failures = failures.into_inner().unwrap_or_else(|e| e.into_inner());
    let (scenarios, seeds) = passed.into_inner().unwrap_or_else(|e| e.into_inner());
    let open = open_findings();
    let mut unexpected = 0;
    let mut expected = 0;
    for (name, e) in &failures {
        match open.get(name.as_str()) {
            Some(finding) if args.mode != Mode::Model => {
                expected += 1;
                println!("open finding {finding} {name}: {e}");
            }
            _ => {
                unexpected += 1;
                println!("MISMATCH {name}: {e}");
            }
        }
    }
    let mut stale = 0;
    for (name, _) in &work {
        if let Some(finding) = open.get(name.as_str())
            && args.mode != Mode::Model
            && !failures.iter().any(|(n, _)| n == name)
        {
            stale += 1;
            println!("NO LONGER FAILS, finding {finding}: {name}: take it off open-findings.txt");
        }
    }
    println!(
        "replayed: {scenarios} scenario traces and {seeds} seed traces passed, {unexpected} mismatched, {expected} open findings; excluded: {excluded:?}"
    );
    if unexpected > 0 || stale > 0 {
        return ExitCode::FAILURE;
    }
    if args.only.is_none() && (scenarios < args.min_scenarios || seeds < args.min_seeds) {
        println!(
            "below the floors: {scenarios} of {} scenario traces, {seeds} of {} seed traces",
            args.min_scenarios, args.min_seeds
        );
        return ExitCode::FAILURE;
    }
    ExitCode::SUCCESS
}

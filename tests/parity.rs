//! Parity with the Python build: the binary, driven as a black box, must
//! produce the transcripts recorded from the Python sources before their
//! removal (run on Linux with Python 3.13).
//!
//! A port of `scripts/parity.py` and `scripts/bench_process.py`, which left
//! with the Python release; the comments that name them point at git history.
//! Each scenario in `tests/parity/scenarios.json`
//! runs the plugin the way Herdr does -- on a pseudo-terminal, against a fake
//! Herdr on a Unix socket, with a fake `ps` and `fc-match` on PATH -- and
//! records the screens after every step, the Herdr requests that change
//! something, the state files, the exit status and the printed text. The
//! result must equal `tests/parity/golden.json` after masking.
//!
//! Environment:
//! * `HERDR_BAR_PARITY_ONLY=<substring>` runs only matching scenarios.
//! * `HERDR_BAR_PARITY_JOBS=<n>` scenarios at once (default 4).
//! * `HERDR_BAR_PARITY_BLESS=1` rewrites `tests/parity/golden.json` from this
//!   run instead of comparing, for intended UI changes. Review the diff.
#![cfg(unix)]

#[allow(dead_code)]
#[path = "../src/par.rs"]
mod par;
#[allow(dead_code)]
#[path = "../src/pyjson.rs"]
mod pyjson;
#[allow(dead_code)]
#[path = "../src/pystr.rs"]
mod pystr;
#[allow(dead_code)]
#[path = "../src/sha256.rs"]
mod sha256;
#[allow(dead_code)]
#[path = "../src/unicode_tables.rs"]
mod unicode_tables;

#[path = "parity/herdr.rs"]
mod herdr;
#[path = "parity/machine.rs"]
mod machine;
#[path = "parity/output.rs"]
mod output;
#[path = "parity/runners.rs"]
mod runners;
#[path = "parity/screen.rs"]
mod screen;

use pyjson::{Map, Value};
use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

fn golden_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/parity/golden.json")
}

fn load(path: &PathBuf) -> Value {
    let text = std::fs::read_to_string(path).unwrap_or_else(|e| panic!("{}: {}", path.display(), e));
    pyjson::loads(&text).unwrap_or_else(|_| panic!("{}: not JSON", path.display()))
}

/// A scenario's transcript (or panic message), and whether it was rerun.
type Outcome = (Result<Value, String>, bool);

/// One scenario, or the panic message it died with.
fn run_one(machine: &Value, scenario: &Value) -> Result<Value, String> {
    let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| runners::run(machine, scenario)));
    outcome.map_err(|panic| {
        panic
            .downcast_ref::<String>()
            .cloned()
            .or_else(|| panic.downcast_ref::<&str>().map(|s| s.to_string()))
            .unwrap_or_else(|| "panic".into())
    })
}

fn transcript_text(result: &Result<Value, String>) -> String {
    match result {
        Ok(value) => output::dump(value),
        Err(message) => format!("<scenario failed: {}>", message),
    }
}

#[test]
fn parity() {
    let spec = load(&PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/parity/scenarios.json"));
    let machine = spec.get("machine").clone();
    let only = std::env::var("HERDR_BAR_PARITY_ONLY").unwrap_or_default();
    let bless = std::env::var("HERDR_BAR_PARITY_BLESS").map(|v| !v.is_empty() && v != "0").unwrap_or(false);
    let jobs: usize = std::env::var("HERDR_BAR_PARITY_JOBS").ok().and_then(|v| v.parse().ok()).unwrap_or(4).max(1);
    let golden = if bless && !golden_path().exists() { Value::Dict(Map::new()) } else { load(&golden_path()) };

    let scenarios: Vec<Value> = spec
        .get("scenarios")
        .list_or_empty()
        .iter()
        .filter(|s| s.get("name").as_str().unwrap_or("").contains(only.as_str()))
        .cloned()
        .collect();
    assert!(!scenarios.is_empty(), "no scenario matches {:?}", only);

    let queue: Arc<Mutex<VecDeque<usize>>> = Arc::new(Mutex::new((0..scenarios.len()).collect()));
    let results: Arc<Mutex<Vec<Option<Outcome>>>> = Arc::new(Mutex::new(vec![None; scenarios.len()]));
    let scenarios = Arc::new(scenarios);
    let started = std::time::Instant::now();
    std::thread::scope(|scope| {
        for _ in 0..jobs.min(scenarios.len()) {
            let (queue, results, scenarios, machine, golden) = (&queue, &results, &scenarios, &machine, &golden);
            scope.spawn(move || loop {
                let Some(index) = queue.lock().unwrap().pop_front() else { return };
                let scenario = &scenarios[index];
                let name = scenario.get("name").as_str().unwrap_or("").to_string();
                let mut result = run_one(machine, scenario);
                let mut retried = false;
                if bless {
                    // Record only what two runs in a row agree on.
                    let mut agreed = false;
                    for _ in 0..3 {
                        let again = run_one(machine, scenario);
                        agreed = transcript_text(&again) == transcript_text(&result);
                        result = again;
                        if agreed {
                            break;
                        }
                        eprintln!("parity: {} varied between runs, running again", name);
                        retried = true;
                    }
                    if !agreed {
                        result = Err("unstable: no two runs in a row agreed".into());
                    }
                } else {
                    let expected = golden.get(&name);
                    if !expected.is_null() && transcript_text(&result) != output::dump(expected) {
                        let first = transcript_text(&result);
                        let lines = output::diff("golden", "rust", &output::dump(expected), &first);
                        eprintln!("parity: {} differed, retrying once:\n{}", name, lines[..lines.len().min(14)].join("\n"));
                        retried = true;
                        result = run_one(machine, scenario);
                    }
                }
                results.lock().unwrap()[index] = Some((result, retried));
            });
        }
    });
    let results: Vec<Outcome> =
        Arc::try_unwrap(results).ok().unwrap().into_inner().unwrap().into_iter().map(|r| r.unwrap()).collect();
    eprintln!("parity: {} scenarios in {:.1}s", scenarios.len(), started.elapsed().as_secs_f64());

    if bless {
        let mut map = golden.as_dict().cloned().unwrap_or_default();
        let mut errors = Vec::new();
        for (scenario, (result, _)) in scenarios.iter().zip(results) {
            let name = scenario.get("name").as_str().unwrap_or("").to_string();
            match result {
                Ok(value) => map.insert(name, value),
                Err(message) => errors.push(format!("{}: {}", name, message)),
            }
        }
        assert!(errors.is_empty(), "scenarios failed, golden not written:\n{}", errors.join("\n"));
        std::fs::write(golden_path(), output::dump(&Value::Dict(map)) + "\n").unwrap();
        eprintln!("parity: wrote {}", golden_path().display());
        return;
    }

    let mut failures = Vec::new();
    let mut retries = 0;
    for (scenario, (result, retried)) in scenarios.iter().zip(results) {
        let name = scenario.get("name").as_str().unwrap_or("");
        retries += retried as usize;
        let expected = golden.get(name);
        if expected.is_null() {
            eprintln!("✗ {}: no golden entry (bless with HERDR_BAR_PARITY_BLESS=1)", name);
            failures.push(name.to_string());
            continue;
        }
        let (want, got) = (output::dump(expected), transcript_text(&result));
        if want == got {
            eprintln!("✓ {}{}", name, if retried { " (after a retry)" } else { "" });
            continue;
        }
        failures.push(name.to_string());
        eprintln!("✗ {}", name);
        let lines = output::diff(&format!("golden:{}", name), &format!("rust:{}", name), &want, &got);
        for line in lines.iter().take(120) {
            eprintln!("{}", line);
        }
        if lines.len() > 120 {
            eprintln!("  … {} more diff lines", lines.len() - 120);
        }
    }
    eprintln!("\n{}/{} scenarios differ, {} retried", failures.len(), scenarios.len(), retries);
    assert!(failures.is_empty(), "parity: scenarios differ: {}", failures.join(", "));
}

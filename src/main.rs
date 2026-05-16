//! `macprof` — CLI for the macos_profiler library.
//!
//! Subcommands:
//!   snapshot [-p PATTERN]...        one-shot JSON
//!   watch [-o OUT] [-i SEC] [-p P]  continuous TSV until Ctrl-C
//!   summarize FILE                  aggregate stats from a watch TSV

use std::env;
use std::fs::File;
use std::io::{BufRead, BufReader, Write};
use std::process::ExitCode;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use macos_profiler::{aggregate, snapshot, snapshot_to_json, Profiler};

const DEFAULT_TARGETS: &[&str] = &["hindsight", "ollama", "claude", "com.apple.Virtua", "Docker"];

fn usage() -> ! {
    eprintln!(
        "usage:
  macprof snapshot [-p PATTERN]...
  macprof watch    [-o OUT] [-i SEC] [-p PATTERN]...
  macprof summarize FILE

Patterns are case-insensitive substrings matched against the first 16
chars of the command name. Defaults: hindsight, ollama, claude, com.apple.Virtua, Docker."
    );
    std::process::exit(2);
}

fn main() -> ExitCode {
    let mut args: Vec<String> = env::args().skip(1).collect();
    if args.is_empty() {
        usage();
    }
    let cmd = args.remove(0);
    let result = match cmd.as_str() {
        "snapshot" => cmd_snapshot(&args),
        "watch" => cmd_watch(&args),
        "summarize" => cmd_summarize(&args),
        "-h" | "--help" | "help" => {
            usage();
        }
        other => {
            eprintln!("unknown command: {other}");
            usage();
        }
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("error: {e}");
            ExitCode::from(1)
        }
    }
}

fn parse_patterns(args: &[String]) -> Vec<String> {
    let mut out = Vec::new();
    let mut i = 0;
    while i < args.len() {
        if args[i] == "-p" || args[i] == "--process" {
            if let Some(v) = args.get(i + 1) {
                out.push(v.clone());
                i += 2;
                continue;
            }
        }
        i += 1;
    }
    if out.is_empty() {
        DEFAULT_TARGETS.iter().map(|s| s.to_string()).collect()
    } else {
        out
    }
}

fn parse_flag<'a>(args: &'a [String], short: &str, long: &str) -> Option<&'a str> {
    let mut i = 0;
    while i < args.len() {
        if args[i] == short || args[i] == long {
            return args.get(i + 1).map(|s| s.as_str());
        }
        i += 1;
    }
    None
}

fn cmd_snapshot(args: &[String]) -> Result<(), String> {
    let patterns = parse_patterns(args);
    let s = snapshot(&patterns)?;
    print!("{}", snapshot_to_json(&s));
    Ok(())
}

fn cmd_watch(args: &[String]) -> Result<(), String> {
    let patterns = parse_patterns(args);
    let interval_s: f64 = parse_flag(args, "-i", "--interval")
        .and_then(|s| s.parse().ok())
        .unwrap_or(5.0);
    let default_path = format!(
        "/tmp/macprof-{}.tsv",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0)
    );
    let out_path: String = parse_flag(args, "-o", "--output")
        .map(|s| s.to_string())
        .unwrap_or(default_path);

    let mut file = File::create(&out_path).map_err(|e| format!("create {out_path}: {e}"))?;
    let header = Profiler::tsv_header(&patterns);
    file.write_all(header.as_bytes()).ok();
    file.flush().ok();
    print!("{header}");

    eprintln!("writing to {out_path} (Ctrl-C to stop)");

    let stop = Arc::new(AtomicBool::new(false));
    {
        let stop = Arc::clone(&stop);
        ctrlc_set(move || stop.store(true, Ordering::Relaxed));
    }

    let mut profiler = Profiler::new(patterns.clone(), Duration::from_secs_f64(interval_s));
    profiler.start();
    let mut emitted = 0usize;
    while !stop.load(Ordering::Relaxed) {
        std::thread::sleep(Duration::from_millis(200));
        let samples = profiler.samples();
        while emitted < samples.len() {
            let row = Profiler::tsv_row(&samples[emitted], &patterns);
            file.write_all(row.as_bytes()).ok();
            file.flush().ok();
            print!("{row}");
            use std::io::Write as _;
            let _ = std::io::stdout().flush();
            emitted += 1;
        }
    }
    profiler.stop();
    eprintln!("stopped — {} samples → {}", emitted, out_path);
    Ok(())
}

// Minimal signal handler without external deps. macOS sends SIGINT on Ctrl-C
// via the terminal driver; just install a no-deps trampoline.
fn ctrlc_set<F: Fn() + Send + Sync + 'static>(f: F) {
    use std::sync::OnceLock;
    static HANDLER: OnceLock<Box<dyn Fn() + Send + Sync>> = OnceLock::new();
    let _ = HANDLER.set(Box::new(f));
    unsafe {
        libc_signal(2 /* SIGINT */, sigint_trampoline as usize);
        libc_signal(15 /* SIGTERM */, sigint_trampoline as usize);
    }
    extern "C" fn sigint_trampoline(_sig: i32) {
        if let Some(f) = HANDLER.get() {
            f();
        }
    }
}

extern "C" {
    fn signal(sig: i32, handler: usize) -> usize;
}

unsafe fn libc_signal(sig: i32, handler: usize) {
    signal(sig, handler);
}

fn cmd_summarize(args: &[String]) -> Result<(), String> {
    let path = args.first().ok_or("usage: macprof summarize FILE")?;
    let file = File::open(path).map_err(|e| format!("open {path}: {e}"))?;
    let mut rdr = BufReader::new(file).lines();
    let header = match rdr.next() {
        Some(Ok(h)) => h,
        _ => return Err("empty file".into()),
    };
    let cols: Vec<&str> = header.split('\t').collect();
    let mut rows: Vec<Vec<String>> = Vec::new();
    for line in rdr {
        let line = line.map_err(|e| e.to_string())?;
        rows.push(line.split('\t').map(|s| s.to_string()).collect());
    }
    if rows.is_empty() {
        println!("no samples");
        return Ok(());
    }
    println!(
        "# samples: {}    {} → {}",
        rows.len(),
        rows.first().and_then(|r| r.first()).map(|s| s.as_str()).unwrap_or("?"),
        rows.last().and_then(|r| r.first()).map(|s| s.as_str()).unwrap_or("?"),
    );
    println!();
    for (i, col) in cols.iter().enumerate() {
        if *col == "ts" {
            continue;
        }
        let vals: Vec<f64> = rows
            .iter()
            .filter_map(|r| r.get(i).and_then(|s| s.parse::<f64>().ok()))
            .collect();
        if vals.is_empty() {
            continue;
        }
        let agg = aggregate(&vals);
        println!(
            "  {:30}  mean={:8.2}  p95={:8.2}  peak={:8.2}",
            col, agg.mean, agg.p95, agg.peak
        );
    }
    // Thermal verdict
    if let Some(kt_idx) = cols.iter().position(|c| *c == "kernel_task_cpu") {
        let kts: Vec<f64> = rows
            .iter()
            .filter_map(|r| r.get(kt_idx).and_then(|s| s.parse::<f64>().ok()))
            .collect();
        let high = kts.iter().filter(|&&k| k > 30.0).count();
        let verdict = if high == 0 {
            "OK (no thermal pressure)".to_string()
        } else {
            format!("⚠ thermal pressure in {high}/{} samples (>30%)", kts.len())
        };
        println!("\n  thermal verdict: {verdict}");
    }
    Ok(())
}

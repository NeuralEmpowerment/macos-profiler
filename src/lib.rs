//! Sudoless macOS process profiler — CPU, memory, energy (power in mW).
//!
//! Wraps `top -l 2 -i 2 -stats command,cpu,mem,power,pid` because `top -l 1`
//! reports 0 for the POWER column (no interval to average over). The first
//! `top` sample is the baseline-zero; we read only the second.
//!
//! Library API:
//!
//! ```no_run
//! use macos_profiler::{snapshot, Profiler};
//! use std::time::Duration;
//!
//! let s = snapshot(&["ollama".to_string()]).unwrap();
//! println!("{}", s.processes["ollama"].power_mw);
//!
//! let mut p = Profiler::new(vec!["ollama".into()], Duration::from_secs(5));
//! p.start();
//! // ... do work ...
//! p.stop();
//! for s in p.samples() { println!("{}", s.total_power_mw); }
//! ```

use std::collections::HashMap;
use std::fmt::Write as _;
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

#[derive(Debug, Default, Clone)]
pub struct ProcessSample {
    pub cpu_pct: f64,
    pub power_mw: f64,
    pub rss_mb: f64,
    pub matches: u32,
}

#[derive(Debug, Clone)]
pub struct SystemSample {
    pub ts: String,
    pub total_power_mw: f64,
    pub kernel_task_cpu: f64,
    pub processes: HashMap<String, ProcessSample>,
}

/// One snapshot. Patterns are case-insensitive substring matches against
/// the command name (first 16 chars per `top`'s fixed-width formatting).
pub fn snapshot(patterns: &[String]) -> Result<SystemSample, String> {
    let raw = run_top()?;
    let rows = parse_rows(second_sample(&raw));
    let total_power: f64 = rows.iter().map(|r| r.power_mw).sum();
    let kt = rows
        .iter()
        .find(|r| r.command == "kernel_task")
        .map(|r| r.cpu_pct)
        .unwrap_or(0.0);
    let mut processes: HashMap<String, ProcessSample> = patterns
        .iter()
        .map(|p| (p.clone(), ProcessSample::default()))
        .collect();
    for row in &rows {
        let lc = row.command.to_lowercase();
        for pat in patterns {
            if lc.contains(&pat.to_lowercase()) {
                let entry = processes.get_mut(pat).unwrap();
                entry.cpu_pct += row.cpu_pct;
                entry.power_mw += row.power_mw;
                entry.rss_mb += row.rss_mb;
                entry.matches += 1;
            }
        }
    }
    Ok(SystemSample {
        ts: iso8601_now(),
        total_power_mw: total_power,
        kernel_task_cpu: kt,
        processes,
    })
}

#[derive(Debug)]
struct Row {
    command: String,
    cpu_pct: f64,
    rss_mb: f64,
    power_mw: f64,
    #[allow(dead_code)]
    pid: String,
}

fn run_top() -> Result<String, String> {
    let out = Command::new("top")
        .args([
            "-l", "2", "-i", "2", "-stats", "command,cpu,mem,power,pid", "-n", "300",
        ])
        .output()
        .map_err(|e| format!("failed to spawn top: {e}"))?;
    if !out.status.success() {
        return Err(format!("top exited {}", out.status));
    }
    String::from_utf8(out.stdout).map_err(|e| format!("top output not utf8: {e}"))
}

fn second_sample(raw: &str) -> &str {
    // Top prints two header blocks; only the second has non-zero POWER.
    // Walk lines, count Processes: occurrences, return everything after the
    // second one. Robust to whether the first occurrence has a leading \n.
    let mut count = 0;
    let mut offset = 0;
    for line in raw.split_inclusive('\n') {
        if line.starts_with("Processes:") {
            count += 1;
            if count == 2 {
                return &raw[offset..];
            }
        }
        offset += line.len();
    }
    ""
}

fn parse_rows(block: &str) -> Vec<Row> {
    let mut rows = Vec::new();
    let mut in_data = false;
    for line in block.lines() {
        if line.starts_with("COMMAND") {
            in_data = true;
            continue;
        }
        if !in_data {
            continue;
        }
        if line.len() < 17 {
            continue;
        }
        // Command is the first 16 chars (top pads right). Rest is whitespace-
        // separated: %CPU MEM POWER PID
        let cmd = line[..16].trim_end().to_string();
        let rest: Vec<&str> = line[16..].split_whitespace().collect();
        if rest.len() < 4 {
            continue;
        }
        let cpu = rest[0].parse::<f64>().ok();
        let mem_mb = parse_mem(rest[1]);
        let power = rest[2].parse::<f64>().ok();
        let pid = rest[3].to_string();
        if let (Some(cpu), Some(power)) = (cpu, power) {
            rows.push(Row {
                command: cmd,
                cpu_pct: cpu,
                rss_mb: mem_mb,
                power_mw: power,
                pid,
            });
        }
    }
    rows
}

fn parse_mem(token: &str) -> f64 {
    // "128M+", "4K", "2G", "256"  -> MB
    if token.is_empty() {
        return 0.0;
    }
    let trimmed = token.trim_end_matches(|c: char| c == '+' || c == '-');
    let unit = trimmed.chars().last();
    let (num_str, factor): (&str, f64) = match unit {
        Some('K') | Some('k') => (&trimmed[..trimmed.len() - 1], 1.0 / 1024.0),
        Some('M') | Some('m') => (&trimmed[..trimmed.len() - 1], 1.0),
        Some('G') | Some('g') => (&trimmed[..trimmed.len() - 1], 1024.0),
        _ => (trimmed, 1.0 / 1024.0 / 1024.0),
    };
    num_str.parse::<f64>().unwrap_or(0.0) * factor
}

fn iso8601_now() -> String {
    // Avoid pulling chrono. Format `date -Iseconds`-style via std SystemTime.
    use std::time::{SystemTime, UNIX_EPOCH};
    let dur = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    let secs = dur.as_secs() as i64;
    let (year, month, day, hour, min, sec) = epoch_to_ymdhms_local(secs);
    let offset = local_utc_offset_seconds();
    let sign = if offset >= 0 { '+' } else { '-' };
    let off_abs = offset.abs();
    format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}{}{:02}:{:02}",
        year, month, day, hour, min, sec,
        sign, off_abs / 3600, (off_abs % 3600) / 60,
    )
}

fn epoch_to_ymdhms_local(epoch_secs: i64) -> (i32, u32, u32, u32, u32, u32) {
    let local = epoch_secs + local_utc_offset_seconds() as i64;
    let days = local.div_euclid(86400);
    let secs_of_day = local.rem_euclid(86400) as u32;
    let (h, ms) = (secs_of_day / 3600, secs_of_day % 3600);
    let (mi, s) = (ms / 60, ms % 60);
    // days from 1970-01-01 → (y, m, d). Simple algorithm.
    let mut year = 1970i32;
    let mut d = days;
    loop {
        let leap = is_leap(year);
        let yd = if leap { 366 } else { 365 };
        if d >= yd as i64 {
            d -= yd as i64;
            year += 1;
        } else if d < 0 {
            year -= 1;
            let yd_prev = if is_leap(year) { 366 } else { 365 };
            d += yd_prev as i64;
        } else {
            break;
        }
    }
    let leap = is_leap(year);
    let days_in_month = [31u32, if leap { 29 } else { 28 }, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31];
    let mut month = 1u32;
    let mut d = d as u32;
    for &dm in &days_in_month {
        if d < dm {
            break;
        }
        d -= dm;
        month += 1;
    }
    (year, month, d + 1, h, mi, s)
}

fn is_leap(y: i32) -> bool {
    (y % 4 == 0 && y % 100 != 0) || y % 400 == 0
}

fn local_utc_offset_seconds() -> i32 {
    // Get the OS's current UTC offset by shelling out to `date +%z`.
    Command::new("date")
        .arg("+%z")
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .and_then(|s| {
            let s = s.trim();
            if s.len() != 5 { return None; }
            let sign = if &s[..1] == "-" { -1 } else { 1 };
            let h: i32 = s[1..3].parse().ok()?;
            let m: i32 = s[3..5].parse().ok()?;
            Some(sign * (h * 3600 + m * 60))
        })
        .unwrap_or(0)
}

/// Continuous sampling in a background thread.
pub struct Profiler {
    patterns: Vec<String>,
    interval: Duration,
    samples: Arc<Mutex<Vec<SystemSample>>>,
    stop: Arc<AtomicBool>,
    handle: Option<JoinHandle<()>>,
}

impl Profiler {
    pub fn new(patterns: Vec<String>, interval: Duration) -> Self {
        Self {
            patterns,
            interval,
            samples: Arc::new(Mutex::new(Vec::new())),
            stop: Arc::new(AtomicBool::new(false)),
            handle: None,
        }
    }

    pub fn start(&mut self) {
        if self.handle.is_some() {
            return;
        }
        let patterns = self.patterns.clone();
        let samples = Arc::clone(&self.samples);
        let stop = Arc::clone(&self.stop);
        let interval = self.interval;
        self.handle = Some(thread::spawn(move || {
            while !stop.load(Ordering::Relaxed) {
                let t0 = Instant::now();
                if let Ok(s) = snapshot(&patterns) {
                    if let Ok(mut v) = samples.lock() {
                        v.push(s);
                    }
                }
                // top -l 2 -i 2 already sleeps ~2s. Subtract that.
                let used = t0.elapsed();
                let want = interval.saturating_sub(used);
                let mut remaining = want;
                while !remaining.is_zero() && !stop.load(Ordering::Relaxed) {
                    let step = remaining.min(Duration::from_millis(200));
                    thread::sleep(step);
                    remaining = remaining.saturating_sub(step);
                }
            }
        }));
    }

    pub fn stop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }

    pub fn samples(&self) -> Vec<SystemSample> {
        self.samples.lock().map(|v| v.clone()).unwrap_or_default()
    }

    /// Tab-separated header for stream/file output.
    pub fn tsv_header(patterns: &[String]) -> String {
        let mut s = String::from("ts\ttotal_mw\tkernel_task_cpu");
        for p in patterns {
            let _ = write!(s, "\t{p}_cpu\t{p}_mw\t{p}_mb");
        }
        s.push('\n');
        s
    }

    /// One TSV row for a sample.
    pub fn tsv_row(sample: &SystemSample, patterns: &[String]) -> String {
        let mut s = format!(
            "{}\t{:.1}\t{:.1}",
            sample.ts, sample.total_power_mw, sample.kernel_task_cpu
        );
        for p in patterns {
            let ps = sample.processes.get(p).cloned().unwrap_or_default();
            let _ = write!(s, "\t{:.1}\t{:.1}\t{:.1}", ps.cpu_pct, ps.power_mw, ps.rss_mb);
        }
        s.push('\n');
        s
    }
}

impl Drop for Profiler {
    fn drop(&mut self) {
        self.stop();
    }
}

// -- Aggregation for `summarize` -----------------------------------------

pub struct ColumnAgg {
    pub mean: f64,
    pub p95: f64,
    pub peak: f64,
}

pub fn aggregate(values: &[f64]) -> ColumnAgg {
    if values.is_empty() {
        return ColumnAgg { mean: 0.0, p95: 0.0, peak: 0.0 };
    }
    let mean = values.iter().sum::<f64>() / values.len() as f64;
    let mut sorted: Vec<f64> = values.to_vec();
    sorted.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let p95_idx = ((sorted.len() as f64 * 0.95).ceil() as usize).saturating_sub(1).min(sorted.len() - 1);
    let peak = sorted[sorted.len() - 1];
    ColumnAgg { mean, p95: sorted[p95_idx], peak }
}

// -- Tiny manual JSON serializer (no serde dep) --------------------------

pub fn snapshot_to_json(s: &SystemSample) -> String {
    let mut out = String::from("{\n");
    let _ = writeln!(out, "  \"ts\": \"{}\",", s.ts);
    let _ = writeln!(out, "  \"total_power_mw\": {:.2},", s.total_power_mw);
    let _ = writeln!(out, "  \"kernel_task_cpu\": {:.2},", s.kernel_task_cpu);
    out.push_str("  \"processes\": {\n");
    let mut keys: Vec<&String> = s.processes.keys().collect();
    keys.sort();
    let last = keys.len().saturating_sub(1);
    for (i, k) in keys.iter().enumerate() {
        let ps = &s.processes[*k];
        let comma = if i == last { "" } else { "," };
        let _ = writeln!(
            out,
            "    \"{}\": {{\"cpu_pct\": {:.2}, \"power_mw\": {:.2}, \"rss_mb\": {:.2}, \"matches\": {}}}{}",
            k, ps.cpu_pct, ps.power_mw, ps.rss_mb, ps.matches, comma
        );
    }
    out.push_str("  }\n}\n");
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_mem() {
        assert_eq!(parse_mem("128M"), 128.0);
        assert_eq!(parse_mem("128M+"), 128.0);
        assert_eq!(parse_mem("2G"), 2048.0);
        assert!((parse_mem("1024K") - 1.0).abs() < 1e-6);
    }

    #[test]
    fn aggregate_basic() {
        let agg = aggregate(&[1.0, 2.0, 3.0, 4.0, 100.0]);
        assert!((agg.mean - 22.0).abs() < 1e-6);
        assert_eq!(agg.peak, 100.0);
    }
}

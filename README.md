# macos-profiler (`macprof`)

Sudoless macOS process profiler — CPU, memory, energy (power in mW).
Single static binary, stdlib + minimal deps.

```bash
cargo build --release
./target/release/macprof snapshot              # JSON snapshot
./target/release/macprof watch -i 5            # continuous TSV
./target/release/macprof summarize <file.tsv>  # aggregate stats
```

Uses `top -l 2 -stats command,cpu,mem,power,pid` under the hood so
no sudo is required. The POWER column is in mW per process per
sample interval — wrap a workload between two `snapshot` calls to
see what it cost.

## Usage as a library

```rust
use macos_profiler::{snapshot, Profiler};
use std::time::Duration;

let s = snapshot(&["ollama".to_string()]).unwrap();
println!("{}", s.processes["ollama"].power_mw);

let mut p = Profiler::new(vec!["ollama".into()], Duration::from_secs(5));
p.start();
// ... do work ...
p.stop();
for s in p.samples() { println!("{}", s.total_power_mw); }
```


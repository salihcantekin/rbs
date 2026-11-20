//! How long one user data save waits on the disk: what `UserSession::flush` and
//! `UserSession::run` make the UI thread wait for during a profile or collection switch,
//! an import or an export (docs/architecture/threads.md, performance-budgets spec,
//! "Desktop UI").
//!
//! One save of a switch is a collection file (`RBS_BENCH_KB`, default 64 KiB) and the
//! small file that says which ones are in use, each written atomically: a temporary
//! file flushed to the disk, renamed, and on Unix the directory flushed
//! (`write_atomic`). The bench writes into `RBS_BENCH_DIR` (default the temporary
//! directory; point it at the folder holding the user data to measure that disk) and
//! prints p50, p95 and max over `RBS_BENCH_ROUNDS` saves (default 50).
//!
//! `cargo bench -p rbs-user-data --bench write`

use std::time::{Duration, Instant};

fn main() {
    let kb: usize = env_or("RBS_BENCH_KB", 64);
    let rounds: usize = env_or("RBS_BENCH_ROUNDS", 50).max(1);
    let base = std::env::var_os("RBS_BENCH_DIR")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(std::env::temp_dir);
    let dir = base.join(format!("rbs-write-bench-{}", std::process::id()));
    if let Err(e) = std::fs::create_dir_all(&dir) {
        eprintln!("rbs bench: cannot make {}: {e}", dir.display());
        return;
    }
    let collection = vec![b'x'; kb * 1024];
    let current = vec![b'y'; 200];
    let mut saves = Vec::with_capacity(rounds);
    for _ in 0..rounds {
        let start = Instant::now();
        let written = rbs_user_data::document::write_atomic(&dir.join("c.toml"), &collection)
            .and_then(|()| {
                rbs_user_data::document::write_atomic(&dir.join("current.toml"), &current)
            });
        if let Err(e) = written {
            eprintln!("rbs bench: write failed: {e}");
            break;
        }
        saves.push(start.elapsed());
    }
    let _ = std::fs::remove_dir_all(&dir);
    saves.sort();
    let at = |q: f64| saves[((saves.len() - 1) as f64 * q).round() as usize];
    let ms = |d: Duration| d.as_secs_f64() * 1000.0;
    if saves.is_empty() {
        return;
    }
    println!(
        "rbs bench: user data save ({kb} KiB + current) in {}: rounds={} ms p50={:.2} p95={:.2} max={:.2}",
        base.display(),
        saves.len(),
        ms(at(0.5)),
        ms(at(0.95)),
        ms(saves[saves.len() - 1])
    );
}

fn env_or(name: &str, default: usize) -> usize {
    std::env::var(name)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

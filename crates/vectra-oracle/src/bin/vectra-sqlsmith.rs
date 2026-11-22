#![deny(unsafe_code)]

use std::path::PathBuf;
use std::time::Instant;
use vectra_oracle::run_sqlsmith_campaign;

fn main() {
    let mut seeds: u64 = 25_000;
    let mut start_seed: u64 = 0;
    let mut artifacts_dir = PathBuf::from("artifacts/failures");

    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--seeds" => {
                i += 1;
                seeds = args
                    .get(i)
                    .expect("--seeds requires integer argument")
                    .parse()
                    .expect("invalid --seeds");
            }
            "--start-seed" => {
                i += 1;
                start_seed = args
                    .get(i)
                    .expect("--start-seed requires integer argument")
                    .parse()
                    .expect("invalid --start-seed");
            }
            "--artifacts" => {
                i += 1;
                artifacts_dir =
                    PathBuf::from(args.get(i).expect("--artifacts requires path argument"));
            }
            other => {
                eprintln!("Unknown argument: {other}");
                std::process::exit(2);
            }
        }
        i += 1;
    }

    let t0 = Instant::now();
    match run_sqlsmith_campaign(start_seed, seeds, Some(&artifacts_dir)) {
        Ok(report) => {
            let secs = t0.elapsed().as_secs_f64();
            let qps = if secs > 0.0 {
                (report.seeds_run as f64) / secs
            } else {
                0.0
            };
            if let Some(fail) = report.first_failure {
                eprintln!(
                    "SQLSMITH MISMATCH at seed {} (after {}/{} seeds in {:.2}s):",
                    fail.seed, report.seeds_run, seeds, secs
                );
                eprintln!("  DIFF: {}", fail.diff);
                eprintln!("  ORIGINAL SQL: {}", fail.original_sql);
                eprintln!("  SHRUNK SQL:   {}", fail.shrunk_sql);
                if let Some(p) = fail.reproducer_path {
                    eprintln!("  REPRODUCER:   {}", p.display());
                }
                std::process::exit(1);
            }
            println!(
                "SQLsmith Differential Oracle PASSED: {} consecutive seeds ({} in 4 MiB forced-spill mode), 0 mismatches, 0 panics in {:.2}s ({:.0} queries/s)",
                report.seeds_run, report.spill_seeds_run, secs, qps
            );
        }
        Err(e) => {
            eprintln!("SQLsmith harness error: {e}");
            std::process::exit(1);
        }
    }
}

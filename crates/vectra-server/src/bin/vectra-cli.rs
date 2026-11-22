#![deny(unsafe_code)]

use clap::Parser;
use rustyline::DefaultEditor;
use std::sync::Arc;
use vectra_exec::{parse_byte_size, Database, ExecutionConfig};
use vectra_server::CliState;

#[derive(Parser, Debug)]
#[command(
    name = "vectra-cli",
    about = "VectraDB Interactive SQL REPL & Visual EXPLAIN ANALYZE Inspector"
)]
struct Args {
    /// Execute a single SQL string and exit
    #[arg(short = 'c', long)]
    command: Option<String>,

    /// Preload synthetic TPC-H dataset at scale factor (e.g. 0.01)
    #[arg(long)]
    tpch: Option<f64>,

    /// Hard BoundedMemoryPool ceiling (e.g. 64MB, 256MB)
    #[arg(long, default_value = "256MB")]
    memlimit: String,
}

fn main() {
    let args = Args::parse();
    let cfg = ExecutionConfig {
        memory_limit_bytes: parse_byte_size(&args.memlimit).expect("invalid --memlimit"),
        ..ExecutionConfig::default()
    };
    let db = Arc::new(Database::with_config(cfg));

    if let Some(sf) = args.tpch {
        let sum = vectra_tpch::load_tpch_into_database(&db, sf).expect("TPC-H load failed");
        eprintln!("Loaded TPC-H SF={sf} ({} total rows).", sum.total_rows());
    }

    let mut state = CliState::new(db);
    if let Some(cmd) = args.command {
        match state.execute_sql_formatted(&cmd) {
            Ok(out) => println!("{out}"),
            Err(e) => {
                eprintln!("ERROR [{}]: {e}", e.sqlstate());
                std::process::exit(1);
            }
        }
        return;
    }

    println!("VectraDB v0.1.0 — Vectorized Columnar OLAP Engine");
    println!("Type SQL ending with ';' or '.help' for meta-commands.");

    let mut rl = DefaultEditor::new().expect("failed to initialize REPL");
    let mut sql_buf = String::new();

    loop {
        let prompt = if sql_buf.is_empty() {
            "vectradb> "
        } else {
            "      -> "
        };
        match rl.readline(prompt) {
            Ok(line) => {
                let trimmed = line.trim();
                if sql_buf.is_empty() && trimmed.is_empty() {
                    continue;
                }
                if sql_buf.is_empty() && trimmed.starts_with('.') {
                    let _ = rl.add_history_entry(trimmed);
                    match state.execute_meta_command(trimmed) {
                        Ok(None) => break,
                        Ok(Some(msg)) => println!("{msg}"),
                        Err(e) => eprintln!("ERROR: {e}"),
                    }
                    continue;
                }
                sql_buf.push_str(&line);
                sql_buf.push('\n');
                if trimmed.ends_with(';') {
                    let query = sql_buf.trim().to_string();
                    sql_buf.clear();
                    let _ = rl.add_history_entry(&query);
                    match state.execute_sql_formatted(&query) {
                        Ok(out) => println!("{out}"),
                        Err(e) => eprintln!("ERROR [{}]: {e}", e.sqlstate()),
                    }
                }
            }
            Err(_) => break,
        }
    }
}

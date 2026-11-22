#![deny(unsafe_code)]

use clap::Parser;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use vectra_exec::{parse_byte_size, Database, ExecutionConfig};
use vectra_server::PgServer;

#[derive(Parser, Debug)]
#[command(
    name = "vectra-server",
    about = "VectraDB Async PostgreSQL v3 Wire Protocol Server"
)]
struct Args {
    /// TCP bind address (e.g. 127.0.0.1:5433)
    #[arg(long, default_value = "127.0.0.1:5433")]
    listen: String,

    /// Optional TPC-H scale factor to preload on startup (e.g. 0.01 or 0.1)
    #[arg(long)]
    tpch: Option<f64>,

    /// Default per-query BoundedMemoryPool ceiling (e.g. 256MB, 64MB)
    #[arg(long, default_value = "256MB")]
    memlimit: String,

    /// Default morsel-driven worker thread count
    #[arg(long)]
    threads: Option<usize>,
}

#[tokio::main]
async fn main() {
    let args = Args::parse();
    let mut cfg = ExecutionConfig {
        memory_limit_bytes: parse_byte_size(&args.memlimit).expect("invalid --memlimit"),
        ..ExecutionConfig::default()
    };
    if let Some(t) = args.threads {
        cfg.threads = t.max(1);
    }

    let db = Arc::new(Database::with_config(cfg));
    if let Some(sf) = args.tpch {
        println!("Preloading TPC-H SF={sf}...");
        let sum = vectra_tpch::load_tpch_into_database(&db, sf).expect("TPC-H preload failed");
        println!(
            "Loaded {} rows (lineitem={}).",
            sum.total_rows(),
            sum.lineitem_rows
        );
    }

    let server = Arc::new(PgServer::new(db));
    let (listener, local_addr) = server
        .bind(&args.listen)
        .await
        .expect("failed to bind TCP listener");
    println!("VectraDB PostgreSQL v3 server listening on {local_addr}");
    println!(
        "Connect with: psql -h {} -p {} -U vectra vectradb",
        local_addr.ip(),
        local_addr.port()
    );

    let shutdown = Arc::new(AtomicBool::new(false));
    server
        .serve(listener, shutdown)
        .await
        .expect("server error");
}

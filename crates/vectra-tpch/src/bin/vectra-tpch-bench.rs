#![deny(unsafe_code)]

use serde::Serialize;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;
use vectra_exec::{format_bytes, Database};
use vectra_tpch::{
    load_tpch_into_database, validate_all_22_against_sqlite, TpchLoadSummary, TPCH_QUERIES,
};

#[derive(Debug, Serialize)]
struct QueryBenchRow {
    query_id: usize,
    name: String,
    rows_out: usize,
    min_ms: f64,
    median_ms: f64,
    mean_ms: f64,
    peak_mem_bytes: usize,
    spilled_bytes: u64,
    zonemap_skipped: u64,
    zonemap_total: u64,
}

#[derive(Debug, Serialize)]
struct StorageBenchRow {
    table: String,
    rows: u64,
    uncompressed_bytes: usize,
    compressed_bytes: usize,
    ratio: f64,
}

#[derive(Debug, Serialize)]
struct TpchBenchmarkReport {
    scale_factor: f64,
    validate_sf: f64,
    validated_queries: usize,
    iterations: usize,
    threads: usize,
    load_ms: f64,
    summary: TpchLoadSummary,
    storage: Vec<StorageBenchRow>,
    queries: Vec<QueryBenchRow>,
    total_median_ms: f64,
    geomean_ms: f64,
}

fn main() {
    let mut sf: f64 = 0.1;
    let mut validate_sf: f64 = 0.01;
    let mut iters: usize = 3;
    let mut output_path = PathBuf::from("docs/benchmarks/tpch_results.md");

    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--sf" => {
                i += 1;
                sf = args
                    .get(i)
                    .expect("--sf requires value")
                    .parse()
                    .expect("invalid --sf");
            }
            "--validate-sf" => {
                i += 1;
                validate_sf = args
                    .get(i)
                    .expect("--validate-sf requires value")
                    .parse()
                    .expect("invalid --validate-sf");
            }
            "--iters" => {
                i += 1;
                iters = args
                    .get(i)
                    .expect("--iters requires value")
                    .parse()
                    .expect("invalid --iters");
            }
            "--output" => {
                i += 1;
                output_path = PathBuf::from(args.get(i).expect("--output requires path"));
            }
            other => {
                eprintln!("Unknown argument: {other}");
                std::process::exit(2);
            }
        }
        i += 1;
    }

    // Step 1: Differential validation of all 22/22 queries at `validate_sf` against SQLite!
    println!("[1/3] Validating all 22 TPC-H queries at SF={validate_sf} against SQLite oracle...");
    let v_t0 = Instant::now();
    let val_res = match validate_all_22_against_sqlite(validate_sf) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("TPC-H VALIDATION FAILED: {e}");
            std::process::exit(1);
        }
    };
    println!(
        "      Validated 22/22 queries against SQLite in {:.2}s (0 mismatches).",
        v_t0.elapsed().as_secs_f64()
    );

    // Step 2: Load SF dataset into VectraDB columnar storage & run ANALYZE.
    println!("[2/3] Generating & loading TPC-H SF={sf} into VectraDB columnar storage...");
    let db = Arc::new(Database::new());
    let load_t0 = Instant::now();
    let summary = load_tpch_into_database(&db, sf).expect("TPC-H dbgen/load failed");
    let load_ms = load_t0.elapsed().as_secs_f64() * 1000.0;
    println!(
        "      Loaded {} total rows (lineitem={}) in {:.1} ms.",
        summary.total_rows(),
        summary.lineitem_rows,
        load_ms
    );

    // Inspect columnar compression ratios across all 8 TPC-H tables.
    let snap = db.catalog().begin_txn();
    let mut storage = Vec::new();
    for (tname, _) in vectra_tpch::all_tpch_schemas() {
        let s = db.catalog().inspect_table(tname, snap).unwrap();
        let uncomp: usize = s.columns.iter().map(|c| c.uncompressed_bytes).sum();
        let comp: usize = s.columns.iter().map(|c| c.compressed_bytes).sum();
        let ratio = if comp == 0 {
            1.0
        } else {
            uncomp as f64 / comp as f64
        };
        storage.push(StorageBenchRow {
            table: tname.to_string(),
            rows: s.total_rows,
            uncompressed_bytes: uncomp,
            compressed_bytes: comp,
            ratio,
        });
    }
    let _ = db.catalog().commit_txn(snap.txn_id);

    // Step 3: Warm-up + timed iterations across all 22 TPC-H queries!
    println!(
        "[3/3] Benchmarking all 22 TPC-H queries at SF={sf} (1 warm-up + {iters} timed runs)..."
    );
    let mut sess = db.new_session();
    let threads = sess.config().threads;
    let mut rows = Vec::with_capacity(22);

    for q in &TPCH_QUERIES {
        // Warm-up run
        let _ = sess
            .execute_sql(q.sql)
            .unwrap_or_else(|e| panic!("TPC-H Q{:02} warm-up failed: {e}", q.id));

        let mut times_ms = Vec::with_capacity(iters);
        let mut out_rows = 0usize;
        let mut peak_mem = 0usize;
        let mut spilled = 0u64;
        let mut zm_skip = 0u64;
        let mut zm_total = 0u64;

        for _ in 0..iters {
            let t0 = Instant::now();
            let qr = sess
                .execute_sql(q.sql)
                .unwrap_or_else(|e| panic!("TPC-H Q{:02} timed run failed: {e}", q.id));
            let ms = t0.elapsed().as_secs_f64() * 1000.0;
            times_ms.push(ms);
            out_rows = qr.total_rows();
            if let Some(m) = qr.metrics {
                peak_mem = peak_mem.max(m.peak_memory_bytes);
                spilled = spilled.max(m.total_spilled_bytes);
                let sk: u64 = m.operators.values().map(|o| o.zonemap_blocks_skipped).sum();
                let tot: u64 = m.operators.values().map(|o| o.zonemap_blocks_total).sum();
                zm_skip = sk;
                zm_total = tot;
            }
        }

        times_ms.sort_by(|a, b| a.total_cmp(b));
        let min_ms = times_ms[0];
        let median_ms = times_ms[times_ms.len() / 2];
        let mean_ms = times_ms.iter().sum::<f64>() / (times_ms.len() as f64);

        println!(
            "      Q{:02}: median={median_ms:7.2} ms | min={min_ms:7.2} ms | rows={out_rows:5} | peak_ram={} | zm_skip={zm_skip}/{zm_total}",
            q.id,
            format_bytes(peak_mem as u64)
        );
        rows.push(QueryBenchRow {
            query_id: q.id,
            name: q.name.to_string(),
            rows_out: out_rows,
            min_ms,
            median_ms,
            mean_ms,
            peak_mem_bytes: peak_mem,
            spilled_bytes: spilled,
            zonemap_skipped: zm_skip,
            zonemap_total: zm_total,
        });
    }

    let total_median_ms: f64 = rows.iter().map(|r| r.median_ms).sum();
    let log_sum: f64 = rows.iter().map(|r| r.median_ms.max(0.01).ln()).sum();
    let geomean_ms = (log_sum / (rows.len() as f64)).exp();

    let report = TpchBenchmarkReport {
        scale_factor: sf,
        validate_sf,
        validated_queries: val_res.len(),
        iterations: iters,
        threads,
        load_ms,
        summary,
        storage,
        queries: rows,
        total_median_ms,
        geomean_ms,
    };

    if let Some(parent) = output_path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }

    // Write Markdown report
    let md = render_markdown(&report);
    std::fs::write(&output_path, md).expect("failed writing markdown benchmark report");

    // Write sibling JSON report
    let json_path = output_path.with_extension("json");
    let json = serde_json::to_string_pretty(&report).expect("json serialize failed");
    std::fs::write(&json_path, json).expect("failed writing json benchmark report");

    println!(
        "\nTPC-H SF={sf} Benchmark Complete: total_median={total_median_ms:.2} ms | geomean={geomean_ms:.2} ms"
    );
    println!(
        "Wrote {} and {}",
        output_path.display(),
        json_path.display()
    );
}

fn render_markdown(r: &TpchBenchmarkReport) -> String {
    let mut md = String::new();
    md.push_str("# VectraDB TPC-H Benchmark & Oracle Validation Report\n\n");
    md.push_str(
        "> Generated automatically by `vectra-tpch-bench` from live release-mode execution.\n\n",
    );
    md.push_str("## Environment & Run Summary\n\n");
    md.push_str(&format!(
        "- **OS / Arch**: `{}` / `{}`\n",
        std::env::consts::OS,
        std::env::consts::ARCH
    ));
    md.push_str(&format!("- **Worker Threads**: `{}`\n", r.threads));
    md.push_str(&format!("- **Differential Oracle Validation**: `{}/22` queries exact match vs bundled SQLite at `SF={}`\n", r.validated_queries, r.validate_sf));
    md.push_str(&format!(
        "- **Benchmark Scale Factor**: `SF={}` ({} total rows, {} `lineitem` rows)\n",
        r.scale_factor,
        r.summary.total_rows(),
        r.summary.lineitem_rows
    ));
    md.push_str(&format!(
        "- **Ingest + Compression + ANALYZE Time**: `{:.1} ms`\n",
        r.load_ms
    ));
    md.push_str(&format!(
        "- **Timed Iterations**: `1 warm-up + {} timed`\n",
        r.iterations
    ));
    md.push_str(&format!(
        "- **Total 22-Query Median Wall Time**: `{:.2} ms`\n",
        r.total_median_ms
    ));
    md.push_str(&format!(
        "- **22-Query Geometric Mean Latency**: `{:.2} ms`\n\n",
        r.geomean_ms
    ));

    md.push_str("## Columnar Storage & Compression Telemetry\n\n");
    md.push_str("| Table | Rows | Uncompressed | Compressed | Compression Ratio |\n");
    md.push_str("| --- | ---: | ---: | ---: | ---: |\n");
    let mut tot_u = 0usize;
    let mut tot_c = 0usize;
    for s in &r.storage {
        tot_u += s.uncompressed_bytes;
        tot_c += s.compressed_bytes;
        md.push_str(&format!(
            "| `{}` | {} | {} | {} | {:.2}x |\n",
            s.table,
            s.rows,
            format_bytes(s.uncompressed_bytes as u64),
            format_bytes(s.compressed_bytes as u64),
            s.ratio
        ));
    }
    let tot_ratio = if tot_c == 0 {
        1.0
    } else {
        tot_u as f64 / tot_c as f64
    };
    md.push_str(&format!(
        "| **Total** | **{}** | **{}** | **{}** | **{:.2}x** |\n\n",
        r.summary.total_rows(),
        format_bytes(tot_u as u64),
        format_bytes(tot_c as u64),
        tot_ratio
    ));

    md.push_str("## Query Latency & Runtime Telemetry (22/22 Queries)\n\n");
    md.push_str(
        "| Query | Rows | Min (ms) | Median (ms) | Mean (ms) | Peak Pool RAM | ZoneMap Pruned |\n",
    );
    md.push_str("| --- | ---: | ---: | ---: | ---: | ---: | ---: |\n");
    for q in &r.queries {
        md.push_str(&format!(
            "| {} | {} | {:.2} | {:.2} | {:.2} | {} | {}/{} |\n",
            q.name,
            q.rows_out,
            q.min_ms,
            q.median_ms,
            q.mean_ms,
            format_bytes(q.peak_mem_bytes as u64),
            q.zonemap_skipped,
            q.zonemap_total
        ));
    }
    md
}

use std::path::Path;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use vectra_exec::{Database, ExecutionConfig, SpillManager};
use vectra_types::{
    BoundedMemoryPool, DataChunk, Field, FlatVector, LogicalType, MemoryPool, ScalarValue, Schema,
    Vector, VECTOR_CAPACITY,
};

/// Order-independent 128-bit multiset fingerprint for comparing 1.5M-row outputs row-for-row
/// without keeping two full copies of 1.5M rows in RAM simultaneously!
#[derive(Debug, Default, PartialEq, Eq)]
struct MultisetDigest {
    rows: u64,
    xor_h1: u64,
    sum_h1: u128,
    sum_h2: u128,
}

impl MultisetDigest {
    fn observe_row(&mut self, row: &[ScalarValue]) {
        self.rows += 1;
        let mut h1 = 0x517c_c1b7_2722_0a95u64;
        let mut h2 = 0x9e37_79b9_7f4a_7c15u64;
        for (i, v) in row.iter().enumerate() {
            let vh = v.canonical_hash();
            h1 = h1.rotate_left(7) ^ vh.wrapping_mul(0xbf58_476d_1ce4_e5b9 ^ (i as u64 + 1));
            h2 = h2
                .wrapping_mul(0x94d0_49bb_1331_11eb)
                .wrapping_add(vh ^ ((i as u64) << 32));
        }
        self.xor_h1 ^= h1;
        self.sum_h1 = self.sum_h1.wrapping_add(u128::from(h1));
        self.sum_h2 = self.sum_h2.wrapping_add(u128::from(h2));
    }
}

/// Order-sensitive rolling 128-bit digest for verifying exact sorted/windowed row sequences!
#[derive(Debug, Default, PartialEq, Eq)]
struct OrderedDigest {
    rows: u64,
    s1: u64,
    s2: u64,
}

impl OrderedDigest {
    fn observe_row(&mut self, row: &[ScalarValue]) {
        self.rows += 1;
        let mut rh = 0x517c_c1b7_2722_0a95u64;
        for (i, v) in row.iter().enumerate() {
            let vh = v.canonical_hash();
            rh = rh.rotate_left(9) ^ vh.wrapping_mul(0xbf58_476d_1ce4_e5b9 ^ (i as u64 + 1));
        }
        self.s1 = self.s1.wrapping_mul(0x9e37_79b9_7f4a_7c15).wrapping_add(rh);
        self.s2 = (self.s2 ^ rh).rotate_left(13).wrapping_add(self.rows);
    }
}

fn count_vspill_files(dir: &Path) -> usize {
    if !dir.exists() {
        return 0;
    }
    std::fs::read_dir(dir)
        .unwrap()
        .filter_map(|e| e.ok())
        .filter(|e| {
            e.path()
                .extension()
                .and_then(|s| s.to_str())
                .is_some_and(|ext| ext == "vspill")
        })
        .count()
}

fn run_streaming_multiset(
    db: &Arc<Database>,
    sql: &str,
    mem_limit: usize,
    spill_dir: &Path,
) -> (MultisetDigest, usize, u64) {
    let pool = BoundedMemoryPool::new(mem_limit);
    let spill = SpillManager::new(spill_dir).unwrap();
    let mut sess = db.new_session();
    sess.set_memory_limit(mem_limit);
    sess.set_threads(1);

    let mut digest = MultisetDigest::default();
    let spilled_bytes;
    {
        let (_schema, stream, _ctx, _bundle) = sess
            .execute_query_stream_with_pool(
                sql,
                &[],
                pool.as_dyn(),
                Arc::clone(&spill),
                Arc::new(AtomicBool::new(false)),
            )
            .unwrap();
        for ch_res in stream {
            let ch = ch_res.unwrap();
            for r in 0..ch.len() {
                digest.observe_row(&ch.get_row(r));
            }
        }
        spilled_bytes = spill.total_spilled_bytes();
    }
    let peak = pool.peak_bytes();
    pool.assert_no_leaks();
    assert_eq!(spill.active_file_count(), 0, "active spill files leaked!");
    assert_eq!(
        count_vspill_files(spill_dir),
        0,
        "leftover .vspill files on disk!"
    );
    (digest, peak, spilled_bytes)
}

fn run_streaming_ordered(
    db: &Arc<Database>,
    sql: &str,
    mem_limit: usize,
    spill_dir: &Path,
    verify_sort_desc_str_asc_id: bool,
) -> (OrderedDigest, usize, u64) {
    let pool = BoundedMemoryPool::new(mem_limit);
    let spill = SpillManager::new(spill_dir).unwrap();
    let mut sess = db.new_session();
    sess.set_memory_limit(mem_limit);
    sess.set_threads(1);

    let mut digest = OrderedDigest::default();
    let mut prev_str: Option<Option<String>> = None;
    let mut prev_id: i64 = i64::MIN;
    let spilled_bytes;
    {
        let (_schema, stream, _ctx, _bundle) = sess
            .execute_query_stream_with_pool(
                sql,
                &[],
                pool.as_dyn(),
                Arc::clone(&spill),
                Arc::new(AtomicBool::new(false)),
            )
            .unwrap();
        for ch_res in stream {
            let ch = ch_res.unwrap();
            for r in 0..ch.len() {
                let row = ch.get_row(r);
                if verify_sort_desc_str_asc_id {
                    let cur_str = match &row[1] {
                        ScalarValue::Null => None,
                        ScalarValue::Utf8(s) => Some(s.clone()),
                        other => panic!("expected Utf8/Null, got {other:?}"),
                    };
                    let cur_id = match &row[0] {
                        ScalarValue::Int64(x) => *x,
                        other => panic!("expected Int64, got {other:?}"),
                    };
                    if let Some(ps) = &prev_str {
                        // ORDER BY str DESC NULLS LAST, id ASC
                        let ord = match (ps, &cur_str) {
                            (None, None) => std::cmp::Ordering::Equal,
                            (None, Some(_)) => std::cmp::Ordering::Greater, // invalid: non-null after null!
                            (Some(_), None) => std::cmp::Ordering::Less, // valid: null after non-null
                            (Some(a), Some(b)) => b.cmp(a),              // DESC: b <= a -> a >= b
                        };
                        assert!(
                            ord != std::cmp::Ordering::Greater,
                            "sort order violation: prev=({ps:?}, {prev_id}) cur=({cur_str:?}, {cur_id})"
                        );
                        if ord == std::cmp::Ordering::Equal {
                            assert!(
                                prev_id <= cur_id,
                                "tie-breaker sort order violation: {prev_id} > {cur_id}"
                            );
                        }
                    }
                    prev_str = Some(cur_str);
                    prev_id = cur_id;
                }
                digest.observe_row(&row);
            }
        }
        spilled_bytes = spill.total_spilled_bytes();
    }
    let peak = pool.peak_bytes();
    pool.assert_no_leaks();
    assert_eq!(spill.active_file_count(), 0);
    assert_eq!(count_vspill_files(spill_dir), 0);
    (digest, peak, spilled_bytes)
}

fn populate_large_dataset(db: &Arc<Database>, total_rows: usize) {
    let pool = BoundedMemoryPool::unbounded().as_dyn();
    let snap = db.catalog().begin_txn();

    // Primary 1,500,000-row table (~180 MB uncompressed with long >12B German strings, Decimal128, NULLs):
    //   id: BIGINT, grp: BIGINT (1,000,000 distinct groups!), dept: BIGINT (500 depts -> 3,000 rows/dept),
    //   ts: BIGINT, amt: DECIMAL(18, 2), label: VARCHAR (~64 bytes -> out-of-line German String!)
    let big_schema = Schema::new(vec![
        Field::new("id", LogicalType::Int64, false),
        Field::new("grp", LogicalType::Int64, false),
        Field::new("dept", LogicalType::Int64, false),
        Field::new("ts", LogicalType::Int64, false),
        Field::new(
            "amt",
            LogicalType::Decimal128 {
                precision: 18,
                scale: 2,
            },
            false,
        ),
        Field::new("label", LogicalType::Utf8, true),
    ]);
    let big_tbl = db.catalog().create_table("big", big_schema, false).unwrap();

    // Right table (~400,000 rows with long payload strings & some NULL keys) for FULL OUTER JOIN & LEFT MARK JOIN!
    let right_schema = Schema::new(vec![
        Field::new("rk", LogicalType::Int64, true),
        Field::new("payload", LogicalType::Utf8, false),
    ]);
    let right_tbl = db
        .catalog()
        .create_table("dim", right_schema, false)
        .unwrap();

    let mut id_vec = Vec::with_capacity(VECTOR_CAPACITY);
    let mut grp_vec = Vec::with_capacity(VECTOR_CAPACITY);
    let mut dept_vec = Vec::with_capacity(VECTOR_CAPACITY);
    let mut ts_vec = Vec::with_capacity(VECTOR_CAPACITY);
    let mut amt_vec = Vec::with_capacity(VECTOR_CAPACITY);
    let mut lbl_vec = Vec::with_capacity(VECTOR_CAPACITY);

    let mut inserted = 0usize;
    while inserted < total_rows {
        let take = (total_rows - inserted).min(VECTOR_CAPACITY);
        id_vec.clear();
        grp_vec.clear();
        dept_vec.clear();
        ts_vec.clear();
        amt_vec.clear();
        lbl_vec.clear();

        for j in 0..take {
            let i = (inserted + j) as i64;
            let mix = (i
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407)
                >> 16)
                .abs();
            id_vec.push(ScalarValue::Int64(i));
            // Exactly 1,000,000 distinct groups across 1,500,000 rows!
            grp_vec.push(ScalarValue::Int64(i % 1_000_000));
            // 500 departments (3,000 rows per partition for sliding window!)
            dept_vec.push(ScalarValue::Int64(mix % 500));
            ts_vec.push(ScalarValue::Int64(mix % 100_000));
            amt_vec.push(ScalarValue::Decimal128 {
                value: i128::from(((i % 10_000) + 1) * 100 + (i % 99)),
                precision: 18,
                scale: 2,
            });
            if i % 101 == 0 {
                lbl_vec.push(ScalarValue::Null);
            } else {
                lbl_vec.push(ScalarValue::Utf8(format!(
                    "vectradb_spill_payload_{:04}_{:08}_german_string_out_of_line",
                    mix % 4096,
                    i
                )));
            }
        }

        let chunk = DataChunk::new(
            vec![
                Vector::Flat(FlatVector::from_scalars(&id_vec, LogicalType::Int64, &pool).unwrap()),
                Vector::Flat(
                    FlatVector::from_scalars(&grp_vec, LogicalType::Int64, &pool).unwrap(),
                ),
                Vector::Flat(
                    FlatVector::from_scalars(&dept_vec, LogicalType::Int64, &pool).unwrap(),
                ),
                Vector::Flat(FlatVector::from_scalars(&ts_vec, LogicalType::Int64, &pool).unwrap()),
                Vector::Flat(
                    FlatVector::from_scalars(
                        &amt_vec,
                        LogicalType::Decimal128 {
                            precision: 18,
                            scale: 2,
                        },
                        &pool,
                    )
                    .unwrap(),
                ),
                Vector::Flat(FlatVector::from_scalars(&lbl_vec, LogicalType::Utf8, &pool).unwrap()),
            ],
            take,
        );
        big_tbl.insert_chunk(&chunk, snap, &pool).unwrap();
        inserted += take;
    }

    // Populate `dim` with 400,000 rows (~40 MB uncompressed -> exceeds 32 MB build budget!)
    let dim_rows = (total_rows / 4).max(40_000);
    let mut rk_vec = Vec::with_capacity(VECTOR_CAPACITY);
    let mut pay_vec = Vec::with_capacity(VECTOR_CAPACITY);
    let mut d_ins = 0usize;
    while d_ins < dim_rows {
        let take = (dim_rows - d_ins).min(VECTOR_CAPACITY);
        rk_vec.clear();
        pay_vec.clear();
        for j in 0..take {
            let i = (d_ins + j) as i64;
            if i % 997 == 0 {
                rk_vec.push(ScalarValue::Null);
            } else {
                // Overlaps [0..total_rows) AND includes unmatched right keys (> total_rows)!
                let k = if i % 5 == 0 {
                    (total_rows as i64) + i
                } else {
                    (i * 3) % (total_rows as i64)
                };
                rk_vec.push(ScalarValue::Int64(k));
            }
            pay_vec.push(ScalarValue::Utf8(format!(
                "dim_build_side_row_payload_value_{:08}_xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx",
                i
            )));
        }
        let chunk = DataChunk::new(
            vec![
                Vector::Flat(FlatVector::from_scalars(&rk_vec, LogicalType::Int64, &pool).unwrap()),
                Vector::Flat(FlatVector::from_scalars(&pay_vec, LogicalType::Utf8, &pool).unwrap()),
            ],
            take,
        );
        right_tbl.insert_chunk(&chunk, snap, &pool).unwrap();
        d_ins += take;
    }

    db.catalog().commit_txn(snap.txn_id).unwrap();
    db.analyze_table(None).unwrap();
}

/// M06 Hard Verification Gate:
/// Exact row-for-row forced-spill equivalence test under a tight 4 MB `BoundedMemoryPool`
/// comparing full materialized sorted rows directly against unconstrained 2 GB RAM!
#[test]
fn test_m06_exact_row_for_row_4mb_forced_spill_equivalence() {
    let spill_dir = std::env::temp_dir().join(format!("vectra_spill_4mb_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&spill_dir);
    let cfg = ExecutionConfig {
        spill_dir: spill_dir.clone(),
        threads: 1,
        ..ExecutionConfig::default()
    };
    let db = Arc::new(Database::with_config(cfg));
    populate_large_dataset(&db, 80_000);

    let tight = 3 * 1024 * 1024;
    let wide = 2 * 1024 * 1024 * 1024;

    // 1. FULL OUTER JOIN (4 MB vs 2 GB)
    let sql_foj = "SELECT b.id, d.rk, d.payload FROM big b FULL OUTER JOIN dim d ON b.id = d.rk";
    let (d_tight, peak_tight, spilled) = run_streaming_multiset(&db, sql_foj, tight, &spill_dir);
    let (d_wide, _, _) = run_streaming_multiset(&db, sql_foj, wide, &spill_dir);
    assert!(spilled > 0, "FULL OUTER JOIN must spill under 4 MB");
    assert!(peak_tight <= tight, "peak {peak_tight} > {tight}");
    assert_eq!(
        d_tight, d_wide,
        "FULL OUTER JOIN mismatch under 4 MB spill!"
    );

    // 2. LEFT MARK JOIN with NULLs (`IN`) (4 MB vs 2 GB)
    let sql_mark = "SELECT id, id IN (SELECT rk FROM dim) AS m FROM big";
    let (d_tight, peak_tight, spilled) = run_streaming_multiset(&db, sql_mark, tight, &spill_dir);
    let (d_wide, _, _) = run_streaming_multiset(&db, sql_mark, wide, &spill_dir);
    assert!(spilled > 0, "LEFT MARK JOIN must spill under 4 MB");
    assert!(peak_tight <= tight, "peak {peak_tight} > {tight}");
    assert_eq!(d_tight, d_wide, "LEFT MARK JOIN mismatch under 4 MB spill!");

    // 3. High-cardinality GROUP BY with COUNT(DISTINCT) + SUM(Decimal128) (4 MB vs 2 GB)
    let sql_agg = "SELECT grp, COUNT(DISTINCT dept), SUM(amt) FROM big GROUP BY grp";
    let (d_tight, peak_tight, spilled) = run_streaming_multiset(&db, sql_agg, tight, &spill_dir);
    let (d_wide, _, _) = run_streaming_multiset(&db, sql_agg, wide, &spill_dir);
    assert!(spilled > 0, "HashAggregate must spill under 4 MB");
    assert!(peak_tight <= tight, "peak {peak_tight} > {tight}");
    assert_eq!(d_tight, d_wide, "HashAggregate mismatch under 4 MB spill!");

    // 4. External Sort `ORDER BY label DESC NULLS LAST, id ASC` (4 MB vs 2 GB)
    let sql_sort = "SELECT id, label FROM big ORDER BY label DESC NULLS LAST, id ASC";
    let (d_tight, peak_tight, spilled) =
        run_streaming_ordered(&db, sql_sort, tight, &spill_dir, true);
    let (d_wide, _, _) = run_streaming_ordered(&db, sql_sort, wide, &spill_dir, true);
    assert!(spilled > 0, "ExternalSort must spill under 4 MB");
    assert!(peak_tight <= tight, "peak {peak_tight} > {tight}");
    assert_eq!(d_tight, d_wide, "ExternalSort mismatch under 4 MB spill!");

    // 5. Sliding-frame Window `SUM(amt) OVER (PARTITION BY dept ORDER BY ts ROWS BETWEEN 50 PRECEDING AND 10 FOLLOWING)`
    let sql_win = "SELECT id, dept, ts, SUM(amt) OVER (PARTITION BY dept ORDER BY ts, id ROWS BETWEEN 50 PRECEDING AND 10 FOLLOWING) FROM big";
    let (d_tight, peak_tight, spilled) =
        run_streaming_ordered(&db, sql_win, tight, &spill_dir, false);
    let (d_wide, _, _) = run_streaming_ordered(&db, sql_win, wide, &spill_dir, false);
    assert!(spilled > 0, "Window external sort must spill under 4 MB");
    assert!(peak_tight <= tight, "peak {peak_tight} > {tight}");
    assert_eq!(d_tight, d_wide, "Window mismatch under 4 MB spill!");

    let _ = std::fs::remove_dir_all(&spill_dir);
}

/// M06 Flagship Out-of-Core Torture Gate:
/// 1,500,000 rows (~180 MB uncompressed) strictly bounded under a 32 MB `BoundedMemoryPool`!
#[test]
fn test_m06_1_5m_rows_32mb_spill_torture_gate() {
    let spill_dir = std::env::temp_dir().join(format!("vectra_spill_32mb_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&spill_dir);
    let cfg = ExecutionConfig {
        spill_dir: spill_dir.clone(),
        threads: 1,
        ..ExecutionConfig::default()
    };
    let db = Arc::new(Database::with_config(cfg));
    populate_large_dataset(&db, 1_500_000);

    let limit_32mb = 32 * 1024 * 1024;
    let limit_2gb = 2 * 1024 * 1024 * 1024;

    // 1. FULL OUTER JOIN on 1.5M x 375K rows (~180 MB + ~40 MB) under 32 MB!
    let sql_foj = "SELECT b.id, d.rk FROM big b FULL OUTER JOIN dim d ON b.id = d.rk";
    let (d_32, peak_32, spill_32) = run_streaming_multiset(&db, sql_foj, limit_32mb, &spill_dir);
    let (d_2g, _, _) = run_streaming_multiset(&db, sql_foj, limit_2gb, &spill_dir);
    println!(
        "[32MB TORTURE] FULL OUTER JOIN: rows={} peak={:.2} MiB spilled={:.2} MiB",
        d_32.rows,
        peak_32 as f64 / 1_048_576.0,
        spill_32 as f64 / 1_048_576.0
    );
    assert!(spill_32 > 0, "1.5M FULL OUTER JOIN must spill under 32 MB");
    assert!(peak_32 <= limit_32mb, "peak {peak_32} exceeded 32 MB!");
    assert_eq!(d_32, d_2g, "1.5M FULL OUTER JOIN 32MB vs 2GB mismatch!");

    // 2. LEFT MARK JOIN (`IN` with NULLs) on 1.5M rows under 32 MB!
    let sql_mark = "SELECT id, id IN (SELECT rk FROM dim) AS m FROM big";
    let (d_32, peak_32, spill_32) = run_streaming_multiset(&db, sql_mark, limit_32mb, &spill_dir);
    let (d_2g, _, _) = run_streaming_multiset(&db, sql_mark, limit_2gb, &spill_dir);
    println!(
        "[32MB TORTURE] LEFT MARK JOIN: rows={} peak={:.2} MiB spilled={:.2} MiB",
        d_32.rows,
        peak_32 as f64 / 1_048_576.0,
        spill_32 as f64 / 1_048_576.0
    );
    assert!(spill_32 > 0, "1.5M LEFT MARK JOIN must spill under 32 MB");
    assert!(peak_32 <= limit_32mb, "peak {peak_32} exceeded 32 MB!");
    assert_eq!(d_32, d_2g, "1.5M LEFT MARK JOIN 32MB vs 2GB mismatch!");

    // 3. 1,000,000-group GROUP BY with COUNT(DISTINCT) + SUM(Decimal128) under 32 MB!
    let sql_agg = "SELECT grp, COUNT(DISTINCT dept), SUM(amt) FROM big GROUP BY grp";
    let (d_32, peak_32, spill_32) = run_streaming_multiset(&db, sql_agg, limit_32mb, &spill_dir);
    let (d_2g, _, _) = run_streaming_multiset(&db, sql_agg, limit_2gb, &spill_dir);
    println!(
        "[32MB TORTURE] 1M-GROUP AGG: groups={} peak={:.2} MiB spilled={:.2} MiB",
        d_32.rows,
        peak_32 as f64 / 1_048_576.0,
        spill_32 as f64 / 1_048_576.0
    );
    assert_eq!(d_32.rows, 1_000_000);
    assert!(spill_32 > 0, "1M-group HashAgg must spill under 32 MB");
    assert!(peak_32 <= limit_32mb, "peak {peak_32} exceeded 32 MB!");
    assert_eq!(d_32, d_2g, "1M-group HashAgg 32MB vs 2GB mismatch!");

    // 4. 1.5M-row External Sort `ORDER BY label DESC NULLS LAST, id ASC` under 32 MB!
    let sql_sort = "SELECT id, label FROM big ORDER BY label DESC NULLS LAST, id ASC";
    let (d_32, peak_32, spill_32) =
        run_streaming_ordered(&db, sql_sort, limit_32mb, &spill_dir, true);
    let (d_2g, _, _) = run_streaming_ordered(&db, sql_sort, limit_2gb, &spill_dir, true);
    println!(
        "[32MB TORTURE] 1.5M EXTERNAL SORT: rows={} peak={:.2} MiB spilled={:.2} MiB",
        d_32.rows,
        peak_32 as f64 / 1_048_576.0,
        spill_32 as f64 / 1_048_576.0
    );
    assert_eq!(d_32.rows, 1_500_000);
    assert!(spill_32 > 0, "1.5M ExternalSort must spill under 32 MB");
    assert!(peak_32 <= limit_32mb, "peak {peak_32} exceeded 32 MB!");
    assert_eq!(d_32, d_2g, "1.5M ExternalSort 32MB vs 2GB mismatch!");

    // 5. 1.5M-row Sliding-Frame Window `ROWS BETWEEN 50 PRECEDING AND 10 FOLLOWING` under 32 MB!
    let sql_win = "SELECT id, dept, ts, SUM(amt) OVER (PARTITION BY dept ORDER BY ts, id ROWS BETWEEN 50 PRECEDING AND 10 FOLLOWING) FROM big";
    let (d_32, peak_32, spill_32) =
        run_streaming_ordered(&db, sql_win, limit_32mb, &spill_dir, false);
    let (d_2g, _, _) = run_streaming_ordered(&db, sql_win, limit_2gb, &spill_dir, false);
    println!(
        "[32MB TORTURE] 1.5M SLIDING WINDOW: rows={} peak={:.2} MiB spilled={:.2} MiB",
        d_32.rows,
        peak_32 as f64 / 1_048_576.0,
        spill_32 as f64 / 1_048_576.0
    );
    assert_eq!(d_32.rows, 1_500_000);
    assert!(spill_32 > 0, "1.5M Window must spill under 32 MB");
    assert!(peak_32 <= limit_32mb, "peak {peak_32} exceeded 32 MB!");
    assert_eq!(d_32, d_2g, "1.5M Window 32MB vs 2GB mismatch!");

    let _ = std::fs::remove_dir_all(&spill_dir);
}

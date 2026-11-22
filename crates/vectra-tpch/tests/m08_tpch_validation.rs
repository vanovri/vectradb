use vectra_tpch::validate_all_22_against_sqlite;

#[test]
fn test_m08_all_22_tpch_queries_match_sqlite_at_sf_0_01() {
    let results = validate_all_22_against_sqlite(0.01)
        .expect("all 22 TPC-H queries must match SQLite ground truth at SF=0.01");
    assert_eq!(results.len(), 22);
    for r in &results {
        println!(
            "TPC-H Q{:02}: MATCHED SQLite (rows={}, vectra={:.2} ms)",
            r.query_id, r.rows, r.elapsed_ms
        );
    }
}

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use vectra_exec::{Database, ExecutionConfig};
use vectra_types::{ScalarValue, VectraError};

fn new_db() -> Arc<Database> {
    Arc::new(Database::new())
}

fn scalar_i64(v: &ScalarValue) -> i64 {
    match v {
        ScalarValue::Int64(x) => *x,
        ScalarValue::Int32(x) => i64::from(*x),
        other => panic!("expected int, got {other:?}"),
    }
}

#[test]
fn test_all_11_join_modes_and_3valued_left_mark() {
    let db = new_db();
    let mut sess = db.new_session();

    sess.execute_sql(
        "CREATE TABLE l (k BIGINT, v VARCHAR);
         CREATE TABLE r (k BIGINT, w VARCHAR);
         INSERT INTO l VALUES (1, 'a'), (2, 'b'), (3, 'c'), (NULL, 'n1');
         INSERT INTO r VALUES (2, 'x'), (3, 'y1'), (3, 'y2'), (4, 'z'), (NULL, 'n2');",
    )
    .unwrap();

    // INNER JOIN (NULL != NULL)
    let res = sess
        .execute_sql("SELECT l.k, r.w FROM l INNER JOIN r ON l.k = r.k ORDER BY l.k, r.w")
        .unwrap();
    assert_eq!(res.total_rows(), 3);

    // LEFT OUTER JOIN
    let res = sess
        .execute_sql("SELECT l.k, r.w FROM l LEFT JOIN r ON l.k = r.k")
        .unwrap();
    assert_eq!(res.total_rows(), 5); // 1(null), 2(x), 3(y1), 3(y2), NULL(null)

    // RIGHT OUTER JOIN
    let res = sess
        .execute_sql("SELECT l.k, r.k FROM l RIGHT JOIN r ON l.k = r.k")
        .unwrap();
    assert_eq!(res.total_rows(), 5); // 2, 3, 3, 4(null), NULL(null)

    // FULL OUTER JOIN
    let res = sess
        .execute_sql("SELECT l.k, r.k FROM l FULL OUTER JOIN r ON l.k = r.k")
        .unwrap();
    assert_eq!(res.total_rows(), 7); // 1, 2, 3, 3, NULL(l), 4, NULL(r)

    // CROSS JOIN
    let res = sess
        .execute_sql("SELECT l.k, r.k FROM l CROSS JOIN r")
        .unwrap();
    assert_eq!(res.total_rows(), 20);

    // LEFT SEMI (EXISTS)
    let res = sess
        .execute_sql(
            "SELECT l.k FROM l WHERE EXISTS (SELECT 1 FROM r WHERE r.k = l.k) ORDER BY l.k",
        )
        .unwrap();
    let ks: Vec<i64> = res
        .to_scalar_rows()
        .iter()
        .map(|r| scalar_i64(&r[0]))
        .collect();
    assert_eq!(ks, vec![2, 3]);

    // LEFT ANTI (NOT EXISTS)
    let res = sess
        .execute_sql(
            "SELECT l.v FROM l WHERE NOT EXISTS (SELECT 1 FROM r WHERE r.k = l.k) ORDER BY l.v",
        )
        .unwrap();
    assert_eq!(res.total_rows(), 2); // 'a' and 'n1'

    // LEFT MARK with 3-valued NULL semantics for IN / NOT IN!
    // r.k contains {2, 3, 3, 4, NULL}:
    // - 1 NOT IN (2, 3, 4, NULL) -> NULL (filtered out!)
    // - 2 NOT IN (2, 3, 4, NULL) -> FALSE
    // - NULL NOT IN (2, 3, 4, NULL) -> NULL
    let res = sess
        .execute_sql("SELECT l.v FROM l WHERE l.k NOT IN (SELECT r.k FROM r)")
        .unwrap();
    assert_eq!(
        res.total_rows(),
        0,
        "NOT IN with NULL in RHS must return 0 rows!"
    );

    // Removing NULL from RHS makes `1 NOT IN (2, 3, 4)` TRUE while `NULL NOT IN (2, 3, 4)` stays NULL!
    let res = sess
        .execute_sql("SELECT l.v FROM l WHERE l.k NOT IN (SELECT r.k FROM r WHERE r.k IS NOT NULL)")
        .unwrap();
    assert_eq!(
        res.to_scalar_rows(),
        vec![vec![ScalarValue::Utf8("a".into())]]
    );

    // Empty RHS: `NULL NOT IN ()` is TRUE!
    let res = sess
        .execute_sql(
            "SELECT l.v FROM l WHERE l.k NOT IN (SELECT r.k FROM r WHERE r.k = 999) ORDER BY l.v",
        )
        .unwrap();
    assert_eq!(res.total_rows(), 4);

    // OR EXISTS (preserves LeftMark rather than Semi!)
    let res = sess
        .execute_sql(
            "SELECT l.v FROM l WHERE l.k = 1 OR EXISTS (SELECT 1 FROM r WHERE r.k = l.k) ORDER BY l.v",
        )
        .unwrap();
    assert_eq!(res.total_rows(), 3); // 'a', 'b', 'c'

    // SINGLE scalar subquery: ok when <=1 match, error when >1 matches!
    let ok_scalar = sess
        .execute_sql("SELECT l.k, (SELECT r.w FROM r WHERE r.k = l.k) FROM l WHERE l.k IN (1, 2) ORDER BY l.k")
        .unwrap();
    assert_eq!(
        ok_scalar.to_scalar_rows(),
        vec![
            vec![ScalarValue::Int64(1), ScalarValue::Null],
            vec![ScalarValue::Int64(2), ScalarValue::Utf8("x".into())],
        ]
    );
    let err_scalar =
        sess.execute_sql("SELECT (SELECT r.w FROM r WHERE r.k = l.k) FROM l WHERE l.k = 3");
    assert!(
        err_scalar.is_err(),
        "scalar subquery returning 2 rows must fail"
    );
}

#[test]
fn test_all_12_aggregates_grouping_sets_rollup_cube() {
    let db = new_db();
    let mut sess = db.new_session();

    sess.execute_sql(
        "CREATE TABLE sales (dept VARCHAR, region VARCHAR, amt DECIMAL(10, 2), qty BIGINT, flag BOOLEAN);
         INSERT INTO sales VALUES
           ('eng', 'us', 100.50, 2, true),
           ('eng', 'us', 200.50, 4, true),
           ('eng', 'eu', 150.00, 4, false),
           ('ops', 'us', 50.00,  1, true),
           ('ops', 'eu', NULL,   NULL, NULL);",
    )
    .unwrap();

    // Global aggregate including all 12 funcs + DISTINCT + FILTER
    let res = sess
        .execute_sql(
            "SELECT
                COUNT(*),
                COUNT(amt),
                COUNT(DISTINCT qty),
                SUM(amt),
                AVG(amt),
                MIN(qty),
                MAX(qty),
                STDDEV_SAMP(qty),
                STDDEV_POP(qty),
                BOOL_AND(flag),
                BOOL_OR(flag),
                STRING_AGG(DISTINCT dept, '|'),
                APPROX_COUNT_DISTINCT(qty),
                COUNT(*) FILTER (WHERE dept = 'eng')
             FROM sales",
        )
        .unwrap();
    let row = &res.to_scalar_rows()[0];
    assert_eq!(row[0], ScalarValue::Int64(5));
    assert_eq!(row[1], ScalarValue::Int64(4));
    assert_eq!(row[2], ScalarValue::Int64(3)); // {1, 2, 4}
    assert_eq!(
        row[3],
        ScalarValue::Decimal128 {
            value: 50100,
            precision: 38,
            scale: 2
        }
    );
    assert_eq!(row[5], ScalarValue::Int64(1));
    assert_eq!(row[6], ScalarValue::Int64(4));
    assert!(matches!(row[7], ScalarValue::Float64(v) if (v - 1.5).abs() < 1e-6));
    assert!(matches!(row[8], ScalarValue::Float64(v) if (v - 1.2990381).abs() < 1e-4));
    assert_eq!(row[9], ScalarValue::Boolean(false));
    assert_eq!(row[10], ScalarValue::Boolean(true));
    assert_eq!(row[11], ScalarValue::Utf8("eng|ops".into()));
    assert!(scalar_i64(&row[12]) >= 2 && scalar_i64(&row[12]) <= 4);
    assert_eq!(row[13], ScalarValue::Int64(3));

    // ROLLUP(dept, region): 4 detail + 2 dept subtotals + 1 grand total = 7 rows
    let rollup = sess
        .execute_sql("SELECT dept, region, COUNT(*) FROM sales GROUP BY ROLLUP(dept, region)")
        .unwrap();
    assert_eq!(rollup.total_rows(), 7);

    // CUBE(dept, region): 4 detail + 2 dept + 2 region + 1 grand total = 9 rows
    let cube = sess
        .execute_sql("SELECT dept, region, COUNT(*) FROM sales GROUP BY CUBE(dept, region)")
        .unwrap();
    assert_eq!(cube.total_rows(), 9);
}

#[test]
fn test_window_functions_segment_tree_frames_and_set_ops_and_recursive_cte() {
    let db = new_db();
    let mut sess = db.new_session();

    sess.execute_sql(
        "CREATE TABLE emp (dept VARCHAR, sal BIGINT, id BIGINT);
         INSERT INTO emp VALUES
           ('a', 10, 1), ('a', 20, 2), ('a', 20, 3), ('a', 40, 4),
           ('b', 100, 5), ('b', 200, 6);",
    )
    .unwrap();

    let win = sess
        .execute_sql(
            "SELECT
                id,
                ROW_NUMBER() OVER (PARTITION BY dept ORDER BY sal, id),
                RANK() OVER (PARTITION BY dept ORDER BY sal),
                DENSE_RANK() OVER (PARTITION BY dept ORDER BY sal),
                NTILE(2) OVER (PARTITION BY dept ORDER BY sal, id),
                LAG(sal, 1, -1) OVER (PARTITION BY dept ORDER BY sal, id),
                LEAD(sal, 1, -1) OVER (PARTITION BY dept ORDER BY sal, id),
                FIRST_VALUE(sal) OVER (PARTITION BY dept ORDER BY sal, id ROWS BETWEEN 1 PRECEDING AND 1 FOLLOWING),
                LAST_VALUE(sal) OVER (PARTITION BY dept ORDER BY sal, id ROWS BETWEEN 1 PRECEDING AND 1 FOLLOWING),
                SUM(sal) OVER (PARTITION BY dept ORDER BY sal, id ROWS BETWEEN 1 PRECEDING AND 1 FOLLOWING)
             FROM emp
             ORDER BY id",
        )
        .unwrap();
    let rows = win.to_scalar_rows();
    assert_eq!(rows.len(), 6);
    // id=1: rn=1, rank=1, dense=1, ntile=1, lag=-1, lead=20, first=10, last=20, sum=30
    assert_eq!(scalar_i64(&rows[0][1]), 1);
    assert_eq!(scalar_i64(&rows[0][2]), 1);
    assert_eq!(scalar_i64(&rows[0][3]), 1);
    assert_eq!(scalar_i64(&rows[0][5]), -1);
    assert_eq!(scalar_i64(&rows[0][6]), 20);
    assert_eq!(scalar_i64(&rows[0][7]), 10);
    assert_eq!(scalar_i64(&rows[0][8]), 20);
    assert_eq!(scalar_i64(&rows[0][9]), 30);
    // id=3 (tied sal=20): rn=3, rank=2, dense=2, sum(20+20+40)=80
    assert_eq!(scalar_i64(&rows[2][1]), 3);
    assert_eq!(scalar_i64(&rows[2][2]), 2);
    assert_eq!(scalar_i64(&rows[2][3]), 2);
    assert_eq!(scalar_i64(&rows[2][9]), 80);

    // Set operations: UNION, INTERSECT [ALL], EXCEPT [ALL] with NULLs
    let set_q = sess
        .execute_sql(
            "(SELECT 1 AS x UNION ALL SELECT 1 UNION ALL SELECT 2 UNION ALL SELECT NULL)
             INTERSECT ALL
             (SELECT 1 UNION ALL SELECT NULL UNION ALL SELECT NULL)",
        )
        .unwrap();
    assert_eq!(set_q.total_rows(), 2); // one `1`, one `NULL`

    let except_q = sess
        .execute_sql(
            "(SELECT 1 AS x UNION ALL SELECT 1 UNION ALL SELECT 2)
             EXCEPT ALL
             (SELECT 1)",
        )
        .unwrap();
    assert_eq!(except_q.total_rows(), 2); // one `1`, one `2`

    // Recursive CTE: graph reachability with cycle (UNION terminates cleanly!)
    sess.execute_sql(
        "CREATE TABLE edges (src BIGINT, dst BIGINT);
         INSERT INTO edges VALUES (1, 2), (2, 3), (3, 4), (4, 2);",
    )
    .unwrap();
    let rec = sess
        .execute_sql(
            "WITH RECURSIVE reach(n) AS (
                SELECT 1
                UNION
                SELECT e.dst FROM reach r JOIN edges e ON r.n = e.src
             )
             SELECT n FROM reach ORDER BY n",
        )
        .unwrap();
    let reached: Vec<i64> = rec
        .to_scalar_rows()
        .iter()
        .map(|r| scalar_i64(&r[0]))
        .collect();
    assert_eq!(reached, vec![1, 2, 3, 4]);
}

#[test]
fn test_mvcc_dml_copy_explain_analyze_and_cancellation() {
    let db = new_db();
    let mut s1 = db.new_session();
    let mut s2 = db.new_session();

    s1.execute_sql(
        "CREATE TABLE kv (k BIGINT, v BIGINT);
         INSERT INTO kv VALUES (1, 10), (2, 20), (3, 30);",
    )
    .unwrap();

    // MVCC Snapshot Isolation: s1 starts txn and updates k=1; s2 still sees old value until COMMIT!
    s1.execute_sql("BEGIN").unwrap();
    s1.execute_sql("UPDATE kv SET v = 99 WHERE k = 1").unwrap();
    let s2_before = s2.execute_sql("SELECT v FROM kv WHERE k = 1").unwrap();
    assert_eq!(scalar_i64(&s2_before.to_scalar_rows()[0][0]), 10);
    s1.execute_sql("COMMIT").unwrap();
    let s2_after = s2.execute_sql("SELECT v FROM kv WHERE k = 1").unwrap();
    assert_eq!(scalar_i64(&s2_after.to_scalar_rows()[0][0]), 99);

    // ROLLBACK discards DELETE
    s1.execute_sql("BEGIN").unwrap();
    s1.execute_sql("DELETE FROM kv WHERE k = 2").unwrap();
    assert_eq!(
        s1.execute_sql("SELECT COUNT(*) FROM kv")
            .unwrap()
            .to_scalar_rows()[0][0],
        ScalarValue::Int64(2)
    );
    s1.execute_sql("ROLLBACK").unwrap();
    assert_eq!(
        s1.execute_sql("SELECT COUNT(*) FROM kv")
            .unwrap()
            .to_scalar_rows()[0][0],
        ScalarValue::Int64(3)
    );

    // ANALYZE + EXPLAIN ANALYZE
    s1.execute_sql("ANALYZE kv").unwrap();
    let exp = s1
        .execute_sql("EXPLAIN ANALYZE SELECT SUM(v) FROM kv WHERE k >= 1")
        .unwrap();
    let text: String = exp
        .to_scalar_rows()
        .into_iter()
        .map(|r| r[0].to_display_string())
        .collect::<Vec<_>>()
        .join("\n");
    assert!(text.contains("EXPLAIN ANALYZE"));
    assert!(text.contains("ColumnarScan"));
    assert!(text.contains("act="));

    // COPY TO / FROM Parquet & CSV roundtrip
    let tmp = std::env::temp_dir().join(format!("vectra_m06_copy_{}", std::process::id()));
    std::fs::create_dir_all(&tmp).unwrap();
    let pq_path = tmp.join("kv.parquet");
    let csv_path = tmp.join("kv.csv");
    s1.execute_sql(&format!(
        "COPY kv TO '{}' (FORMAT PARQUET)",
        pq_path.display()
    ))
    .unwrap();
    s1.execute_sql(&format!(
        "COPY kv TO '{}' (FORMAT CSV, HEADER TRUE)",
        csv_path.display()
    ))
    .unwrap();

    s1.execute_sql("CREATE TABLE kv2 (k BIGINT, v BIGINT);")
        .unwrap();
    s1.execute_sql(&format!(
        "COPY kv2 FROM '{}' (FORMAT PARQUET)",
        pq_path.display()
    ))
    .unwrap();
    assert_eq!(
        s1.execute_sql("SELECT SUM(v) FROM kv2")
            .unwrap()
            .to_scalar_rows()[0][0],
        ScalarValue::Int64(149)
    );
    let _ = std::fs::remove_dir_all(&tmp);

    // Cooperative Cancellation check
    let cancel = Arc::new(AtomicBool::new(true));
    let res = s1.execute_sql_with_params_and_cancel(
        "SELECT COUNT(*) FROM generate_series(1, 1000000)",
        &[],
        cancel,
    );
    assert!(matches!(res, Err(VectraError::Cancelled)));

    // Parameterized query ($1, $2)
    let pres = s1
        .execute_sql_params(
            "SELECT k, v FROM kv WHERE k = $1 AND v > $2",
            &[ScalarValue::Int64(1), ScalarValue::Int64(50)],
        )
        .unwrap();
    assert_eq!(pres.total_rows(), 1);

    // SET / SHOW
    let cfg = ExecutionConfig::default();
    assert!(cfg.threads >= 1);
    s1.execute_sql("SET memory_limit = '64MB'").unwrap();
    let sh = s1.execute_sql("SHOW memory_limit").unwrap();
    assert!(sh.to_scalar_rows()[0][0]
        .to_display_string()
        .contains("64.00 MiB"));
    assert!(!AtomicBool::new(false).load(Ordering::Relaxed));
}

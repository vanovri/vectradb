use std::path::PathBuf;
use vectra_oracle::{run_slt_directory, shrink_failing_sql, write_failure_reproducer};

#[test]
fn test_m07_sqllogictest_5000_plus_assertions() {
    let slt_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/slt");
    let stats = run_slt_directory(&slt_dir).expect("sqllogictest suite must pass 100%");
    println!(
        "sqllogictest suite PASSED: {} files, {} statements, {} queries, {} total assertions",
        stats.files_run, stats.statements_run, stats.queries_run, stats.total_assertions
    );
    assert!(
        stats.files_run >= 25,
        "expected >= 25 .slt files, ran {}",
        stats.files_run
    );
    assert!(
        stats.total_assertions >= 5_000,
        "expected >= 5,000 SQL assertions, ran {}",
        stats.total_assertions
    );
}

#[test]
fn test_m07_ast_delta_debugging_shrinker() {
    // Inject a synthetic mismatch predicate that only triggers when `a.k = 7` appears in the query,
    // and verify the AST delta-debugger shrinks a bloated multi-join/ORDER BY/LIMIT query!
    let bloated = "SELECT a.id, a.k, b.v FROM t0 a INNER JOIN t1 b ON a.k = b.k WHERE a.v > 0 AND a.k = 7 AND b.v < 100 ORDER BY a.id DESC LIMIT 10 OFFSET 2";
    let shrunk = shrink_failing_sql(bloated, |sql| sql.contains("a.k = 7"));
    assert!(
        shrunk.len() < bloated.len() / 2,
        "expected dramatic shrink, got:\n  orig:   {bloated}\n  shrunk: {shrunk}"
    );
    assert!(shrunk.contains("a.k = 7"));

    let tmp = std::env::temp_dir().join(format!("vectra_shrink_test_{}", std::process::id()));
    let path = write_failure_reproducer(
        &tmp,
        42,
        "CREATE TABLE t0 (id BIGINT, k BIGINT);",
        bloated,
        &shrunk,
        "synthetic diff",
    )
    .unwrap();
    let content = std::fs::read_to_string(&path).unwrap();
    assert!(content.contains("seed 42"));
    assert!(content.contains(&shrunk));
    let _ = std::fs::remove_dir_all(&tmp);
}

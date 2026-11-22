use crate::dbgen::{generate_tpch_streaming, load_tpch_into_database};
use crate::queries::{translate_tpch_to_sqlite, TPCH_QUERIES};
use std::sync::Arc;
use vectra_exec::Database;
use vectra_oracle::{compare_oracle_results, SqliteOracle};
use vectra_types::{BoundedMemoryPool, Result, ScalarValue, VectraError};

#[derive(Debug, Clone)]
pub struct QueryValidationResult {
    pub query_id: usize,
    pub rows: usize,
    pub elapsed_ms: f64,
}

const SQLITE_TPCH_DDL: &str = r#"
CREATE TABLE region (r_regionkey INTEGER, r_name TEXT, r_comment TEXT);
CREATE TABLE nation (n_nationkey INTEGER, n_name TEXT, n_regionkey INTEGER, n_comment TEXT);
CREATE TABLE supplier (s_suppkey INTEGER, s_name TEXT, s_address TEXT, s_nationkey INTEGER, s_phone TEXT, s_acctbal REAL, s_comment TEXT);
CREATE TABLE part (p_partkey INTEGER, p_name TEXT, p_mfgr TEXT, p_brand TEXT, p_type TEXT, p_size INTEGER, p_container TEXT, p_retailprice REAL, p_comment TEXT);
CREATE TABLE partsupp (ps_partkey INTEGER, ps_suppkey INTEGER, ps_availqty INTEGER, ps_supplycost REAL, ps_comment TEXT);
CREATE TABLE customer (c_custkey INTEGER, c_name TEXT, c_address TEXT, c_nationkey INTEGER, c_phone TEXT, c_acctbal REAL, c_mktsegment TEXT, c_comment TEXT);
CREATE TABLE orders (o_orderkey INTEGER, o_custkey INTEGER, o_orderstatus TEXT, o_totalprice REAL, o_orderdate TEXT, o_orderpriority TEXT, o_clerk TEXT, o_shippriority INTEGER, o_comment TEXT);
CREATE TABLE lineitem (l_orderkey INTEGER, l_partkey INTEGER, l_suppkey INTEGER, l_linenumber INTEGER, l_quantity REAL, l_extendedprice REAL, l_discount REAL, l_tax REAL, l_returnflag TEXT, l_linestatus TEXT, l_shipdate TEXT, l_commitdate TEXT, l_receiptdate TEXT, l_shipinstruct TEXT, l_shipmode TEXT, l_comment TEXT);
"#;

fn scalar_to_sql_literal(v: &ScalarValue) -> String {
    match v {
        ScalarValue::Null => "NULL".to_string(),
        ScalarValue::Boolean(b) => if *b { "1" } else { "0" }.to_string(),
        ScalarValue::Int8(x) => x.to_string(),
        ScalarValue::Int16(x) => x.to_string(),
        ScalarValue::Int32(x) => x.to_string(),
        ScalarValue::Int64(x) => x.to_string(),
        ScalarValue::Float64(x) => format!("{x}"),
        ScalarValue::Decimal128 { value, scale, .. } => {
            let f = (*value as f64) / 10f64.powi(i32::from(*scale));
            format!("{f:.2}")
        }
        ScalarValue::Date32(_) | ScalarValue::TimestampMicros(_) => {
            format!("'{}'", v.to_display_string())
        }
        ScalarValue::Utf8(s) => format!("'{}'", s.replace('\'', "''")),
        ScalarValue::Binary(b) => format!("'{b:?}'"),
    }
}

/// Loads identical synthetic TPC-H data at `sf` into BOTH VectraDB and `SqliteOracle`,
/// then validates all 22/22 official TPC-H queries row-for-row against SQLite!
pub fn validate_all_22_against_sqlite(sf: f64) -> Result<Vec<QueryValidationResult>> {
    let db = Arc::new(Database::new());
    load_tpch_into_database(&db, sf)?;

    let sqlite = SqliteOracle::open_in_memory()?;
    sqlite.execute_batch("PRAGMA synchronous = OFF; PRAGMA journal_mode = MEMORY;")?;
    sqlite.execute_batch(SQLITE_TPCH_DDL)?;

    let pool = BoundedMemoryPool::unbounded().as_dyn();
    sqlite.execute_batch("BEGIN TRANSACTION;")?;
    generate_tpch_streaming(sf, &pool, |tname, _schema, chunk| {
        let mut batch_sql = String::with_capacity(chunk.len() * 96);
        let mut r = 0usize;
        while r < chunk.len() {
            let end = (r + 100).min(chunk.len());
            batch_sql.clear();
            batch_sql.push_str("INSERT INTO ");
            batch_sql.push_str(tname);
            batch_sql.push_str(" VALUES ");
            for row_i in r..end {
                if row_i > r {
                    batch_sql.push(',');
                }
                batch_sql.push('(');
                let row = chunk.get_row(row_i);
                for (ci, val) in row.iter().enumerate() {
                    if ci > 0 {
                        batch_sql.push(',');
                    }
                    batch_sql.push_str(&scalar_to_sql_literal(val));
                }
                batch_sql.push(')');
            }
            batch_sql.push(';');
            sqlite.execute_batch(&batch_sql)?;
            r = end;
        }
        Ok(())
    })?;
    sqlite.execute_batch("COMMIT;")?;

    let mut sess = db.new_session();
    let mut results = Vec::with_capacity(22);

    for q in &TPCH_QUERIES {
        let t0 = std::time::Instant::now();
        let qr = sess.execute_sql(q.sql).map_err(|e| {
            VectraError::ExecutionError(format!("TPC-H Q{:02} VectraDB failed: {e}", q.id))
        })?;
        let elapsed_ms = t0.elapsed().as_secs_f64() * 1000.0;
        let v_rows = qr.to_scalar_rows();

        let sq_sql = translate_tpch_to_sqlite(q.sql);
        let s_rows = sqlite.query(&sq_sql).map_err(|e| {
            VectraError::ExecutionError(format!(
                "TPC-H Q{:02} SQLite oracle failed: {e}\nSQL:\n{sq_sql}",
                q.id
            ))
        })?;

        // Use relative tolerance 1e-3 (IEEE-754 REAL in SQLite vs exact Decimal128 in VectraDB).
        if let Err(diff) = compare_oracle_results(&v_rows, &s_rows, q.ordered, 1e-3) {
            return Err(VectraError::ExecutionError(format!(
                "TPC-H Q{:02} ({}) differential mismatch vs SQLite:\n{diff}",
                q.id, q.name
            )));
        }

        results.push(QueryValidationResult {
            query_id: q.id,
            rows: v_rows.len(),
            elapsed_ms,
        });
    }

    Ok(results)
}

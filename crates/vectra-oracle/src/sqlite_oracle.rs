use rusqlite::{types::ValueRef, Connection};
use std::cmp::Ordering;
use vectra_types::{Result, ScalarValue, VectraError};

/// Normalized comparable value returned by either VectraDB or the bundled SQLite oracle.
#[derive(Debug, Clone)]
pub enum OracleValue {
    Null,
    Int(i64),
    Float(f64),
    Text(String),
}

impl OracleValue {
    pub fn from_vectra(v: &ScalarValue) -> Self {
        match v {
            ScalarValue::Null => Self::Null,
            ScalarValue::Boolean(b) => Self::Int(i64::from(*b)),
            ScalarValue::Int8(x) => Self::Int(i64::from(*x)),
            ScalarValue::Int16(x) => Self::Int(i64::from(*x)),
            ScalarValue::Int32(x) => Self::Int(i64::from(*x)),
            ScalarValue::Int64(x) => Self::Int(*x),
            ScalarValue::Float64(x) => {
                if x.is_nan() {
                    Self::Null
                } else {
                    Self::Float(if *x == 0.0 { 0.0 } else { *x })
                }
            }
            ScalarValue::Decimal128 { value, scale, .. } => {
                let f = (*value as f64) / 10f64.powi(i32::from(*scale));
                Self::Float(if f == 0.0 { 0.0 } else { f })
            }
            ScalarValue::Date32(_) | ScalarValue::TimestampMicros(_) => {
                Self::Text(v.to_display_string())
            }
            ScalarValue::Utf8(s) => Self::Text(s.clone()),
            ScalarValue::Binary(b) => Self::Text(format!("{b:?}")),
        }
    }

    pub fn approx_eq(&self, other: &Self, eps: f64) -> bool {
        match (self, other) {
            (Self::Null, Self::Null) => true,
            (Self::Int(a), Self::Int(b)) => a == b,
            (Self::Int(a), Self::Float(b)) | (Self::Float(b), Self::Int(a)) => {
                let fa = *a as f64;
                let diff = (fa - *b).abs();
                diff <= eps || diff <= eps * fa.abs().max(b.abs())
            }
            (Self::Float(a), Self::Float(b)) => {
                let diff = (a - b).abs();
                diff <= eps || diff <= eps * a.abs().max(b.abs())
            }
            (Self::Text(a), Self::Text(b)) => a == b,
            _ => false,
        }
    }

    fn sort_key(&self) -> (u8, i64, String) {
        match self {
            Self::Null => (0, 0, String::new()),
            Self::Int(x) => (1, *x, format!("{x:020}")),
            Self::Float(x) => {
                // Quantize slightly so approximate float matches sort next to each other.
                let q = (x * 1e4).round() as i64;
                (1, q, format!("{x:.4}"))
            }
            Self::Text(s) => (2, 0, s.clone()),
        }
    }

    pub fn cmp_canonical(&self, other: &Self) -> Ordering {
        match (self, other) {
            (Self::Null, Self::Null) => Ordering::Equal,
            (Self::Null, _) => Ordering::Less,
            (_, Self::Null) => Ordering::Greater,
            (Self::Int(a), Self::Int(b)) => a.cmp(b),
            (Self::Int(a), Self::Float(b)) => (*a as f64).total_cmp(b),
            (Self::Float(a), Self::Int(b)) => a.total_cmp(&(*b as f64)),
            (Self::Float(a), Self::Float(b)) => a.total_cmp(b),
            (Self::Text(a), Self::Text(b)) => a.cmp(b),
            _ => self.sort_key().0.cmp(&other.sort_key().0),
        }
    }
}

/// Bundled SQLite 3.x reference oracle isolated strictly inside `vectra-oracle`.
pub struct SqliteOracle {
    conn: Connection,
}

impl SqliteOracle {
    pub fn open_in_memory() -> Result<Self> {
        let conn = Connection::open_in_memory()
            .map_err(|e| VectraError::ExecutionError(format!("sqlite open error: {e}")))?;
        // Match SQL standard / VectraDB case-sensitive ASCII LIKE semantics.
        conn.execute_batch("PRAGMA case_sensitive_like = ON;")
            .map_err(|e| VectraError::ExecutionError(format!("sqlite pragma error: {e}")))?;
        Ok(Self { conn })
    }

    pub fn execute_batch(&self, sql: &str) -> Result<()> {
        self.conn
            .execute_batch(sql)
            .map_err(|e| VectraError::ExecutionError(format!("sqlite exec error: {e}")))
    }

    pub fn query(&self, sql: &str) -> Result<Vec<Vec<OracleValue>>> {
        let mut stmt = self
            .conn
            .prepare(sql)
            .map_err(|e| VectraError::ExecutionError(format!("sqlite prepare error: {e}")))?;
        let col_count = stmt.column_count();
        let rows_iter = stmt
            .query_map([], |row| {
                let mut vals = Vec::with_capacity(col_count);
                for i in 0..col_count {
                    let v = match row.get_ref(i)? {
                        ValueRef::Null => OracleValue::Null,
                        ValueRef::Integer(n) => OracleValue::Int(n),
                        ValueRef::Real(f) => {
                            if f.is_nan() {
                                OracleValue::Null
                            } else {
                                OracleValue::Float(if f == 0.0 { 0.0 } else { f })
                            }
                        }
                        ValueRef::Text(bytes) => {
                            OracleValue::Text(String::from_utf8_lossy(bytes).into_owned())
                        }
                        ValueRef::Blob(b) => OracleValue::Text(format!("{b:?}")),
                    };
                    vals.push(v);
                }
                Ok(vals)
            })
            .map_err(|e| VectraError::ExecutionError(format!("sqlite query error: {e}")))?;

        let mut out = Vec::new();
        for r in rows_iter {
            out.push(r.map_err(|e| VectraError::ExecutionError(format!("sqlite row error: {e}")))?);
        }
        Ok(out)
    }
}

/// Compares VectraDB scalar rows against SQLite `OracleValue` rows, returning `Err(diff_description)`
/// on any row count or value discrepancy.
pub fn compare_oracle_results(
    vectra_rows: &[Vec<ScalarValue>],
    sqlite_rows: &[Vec<OracleValue>],
    ordered: bool,
    float_eps: f64,
) -> std::result::Result<(), String> {
    if vectra_rows.len() != sqlite_rows.len() {
        return Err(format!(
            "row count mismatch: vectra={} vs sqlite={}",
            vectra_rows.len(),
            sqlite_rows.len()
        ));
    }

    let mut v_norm: Vec<Vec<OracleValue>> = vectra_rows
        .iter()
        .map(|r| r.iter().map(OracleValue::from_vectra).collect())
        .collect();
    let mut s_norm: Vec<Vec<OracleValue>> = sqlite_rows.to_vec();

    if !ordered {
        let row_cmp = |a: &Vec<OracleValue>, b: &Vec<OracleValue>| -> Ordering {
            for (x, y) in a.iter().zip(b.iter()) {
                let c = x.cmp_canonical(y);
                if c != Ordering::Equal {
                    return c;
                }
            }
            a.len().cmp(&b.len())
        };
        v_norm.sort_by(row_cmp);
        s_norm.sort_by(row_cmp);
    }

    for (r_idx, (vr, sr)) in v_norm.iter().zip(s_norm.iter()).enumerate() {
        if vr.len() != sr.len() {
            return Err(format!(
                "column count mismatch at row {r_idx}: vectra={} vs sqlite={}",
                vr.len(),
                sr.len()
            ));
        }
        for (c_idx, (vv, sv)) in vr.iter().zip(sr.iter()).enumerate() {
            if !vv.approx_eq(sv, float_eps) {
                return Err(format!(
                    "value mismatch at row {r_idx} col {c_idx}: vectra={vv:?} vs sqlite={sv:?}\n  vectra row: {vr:?}\n  sqlite row: {sr:?}"
                ));
            }
        }
    }
    Ok(())
}

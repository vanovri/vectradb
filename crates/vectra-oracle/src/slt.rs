use crate::md5::md5_hex;
use std::path::Path;
use std::sync::Arc;
use vectra_exec::Database;
use vectra_types::{Result, ScalarValue, VectraError};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SltSortMode {
    NoSort,
    RowSort,
    ValueSort,
}

#[derive(Debug, Clone)]
pub enum SltExpected {
    Values(Vec<String>),
    Hash { count: usize, md5: String },
}

#[derive(Debug, Clone)]
pub enum SltRecord {
    StatementOk {
        line: usize,
        sql: String,
    },
    StatementError {
        line: usize,
        expected_substr: Option<String>,
        sql: String,
    },
    Query {
        line: usize,
        col_types: String,
        sort_mode: SltSortMode,
        sql: String,
        expected: SltExpected,
    },
    Halt,
}

#[derive(Debug, Default, Clone, Copy)]
pub struct SltStats {
    pub files_run: usize,
    pub statements_run: usize,
    pub queries_run: usize,
    pub total_assertions: usize,
}

impl SltStats {
    pub fn merge(&mut self, other: Self) {
        self.files_run += other.files_run;
        self.statements_run += other.statements_run;
        self.queries_run += other.queries_run;
        self.total_assertions += other.total_assertions;
    }
}

/// Parses a standard `.slt` (`sqllogictest`) file into executable records.
pub fn parse_slt(src: &str) -> Result<Vec<SltRecord>> {
    let lines: Vec<&str> = src.lines().collect();
    let mut records = Vec::new();
    let mut i = 0usize;

    while i < lines.len() {
        let raw = lines[i].trim();
        let line_no = i + 1;
        if raw.is_empty() || raw.starts_with('#') {
            i += 1;
            continue;
        }
        if raw == "halt" {
            records.push(SltRecord::Halt);
            break;
        }
        if raw.starts_with("skipif ") || raw.starts_with("onlyif ") {
            // Standard SLT directives: skipif sqlite applies only to sqlite, not vectra.
            let is_only_other = raw.starts_with("onlyif ") && !raw.contains("vectra");
            let is_skip_vectra = raw.starts_with("skipif ") && raw.contains("vectra");
            i += 1;
            if is_only_other || is_skip_vectra {
                // Skip next record completely until blank line.
                while i < lines.len() && !lines[i].trim().is_empty() {
                    i += 1;
                }
            }
            continue;
        }

        let tokens: Vec<&str> = raw.split_whitespace().collect();
        match tokens.first().copied() {
            Some("statement") => {
                let kind = tokens.get(1).copied().unwrap_or("");
                let expected_err = if kind == "error" && tokens.len() > 2 {
                    Some(tokens[2..].join(" "))
                } else {
                    None
                };
                i += 1;
                let mut sql_lines = Vec::new();
                while i < lines.len() && !lines[i].trim().is_empty() {
                    sql_lines.push(lines[i]);
                    i += 1;
                }
                let sql = sql_lines.join("\n");
                match kind {
                    "ok" => records.push(SltRecord::StatementOk { line: line_no, sql }),
                    "error" => records.push(SltRecord::StatementError {
                        line: line_no,
                        expected_substr: expected_err,
                        sql,
                    }),
                    other => {
                        return Err(VectraError::ParseError {
                            message: format!(
                                "SLT line {line_no}: unknown statement kind '{other}'"
                            ),
                            span: None,
                            rendered: raw.to_string(),
                        });
                    }
                }
            }
            Some("query") => {
                let col_types = tokens.get(1).copied().unwrap_or("").to_string();
                let sort_mode = match tokens.get(2).copied() {
                    Some("rowsort") => SltSortMode::RowSort,
                    Some("valuesort") => SltSortMode::ValueSort,
                    _ => SltSortMode::NoSort,
                };
                i += 1;
                let mut sql_lines = Vec::new();
                while i < lines.len() && lines[i].trim() != "----" && !lines[i].trim().is_empty() {
                    sql_lines.push(lines[i]);
                    i += 1;
                }
                let sql = sql_lines.join("\n");
                let mut expected_lines = Vec::new();
                if i < lines.len() && lines[i].trim() == "----" {
                    i += 1;
                    while i < lines.len() && !lines[i].trim().is_empty() {
                        expected_lines.push(lines[i].to_string());
                        i += 1;
                    }
                }
                let expected = if expected_lines.len() == 1 {
                    if let Some((cnt, hash)) = parse_hash_line(&expected_lines[0]) {
                        SltExpected::Hash {
                            count: cnt,
                            md5: hash,
                        }
                    } else {
                        SltExpected::Values(expected_lines)
                    }
                } else {
                    SltExpected::Values(expected_lines)
                };
                records.push(SltRecord::Query {
                    line: line_no,
                    col_types,
                    sort_mode,
                    sql,
                    expected,
                });
            }
            _ => {
                return Err(VectraError::ParseError {
                    message: format!("SLT line {line_no}: unexpected directive '{raw}'"),
                    span: None,
                    rendered: raw.to_string(),
                });
            }
        }
    }
    Ok(records)
}

fn parse_hash_line(line: &str) -> Option<(usize, String)> {
    // Format: "<N> values hashing to <32-hex>"
    let parts: Vec<&str> = line.split_whitespace().collect();
    if parts.len() == 5 && parts[1] == "values" && parts[2] == "hashing" && parts[3] == "to" {
        let count = parts[0].parse::<usize>().ok()?;
        let md5 = parts[4].to_ascii_lowercase();
        if md5.len() == 32 && md5.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Some((count, md5));
        }
    }
    None
}

/// Formats a `ScalarValue` according to standard `sqllogictest` column type specifiers (`I`, `R`, `T`, `B`).
pub fn format_slt_value(v: &ScalarValue, type_char: char) -> String {
    if v.is_null() {
        return "NULL".to_string();
    }
    match type_char {
        'I' => match v {
            ScalarValue::Boolean(b) => i64::from(*b).to_string(),
            ScalarValue::Int8(x) => x.to_string(),
            ScalarValue::Int16(x) => x.to_string(),
            ScalarValue::Int32(x) => x.to_string(),
            ScalarValue::Int64(x) => x.to_string(),
            ScalarValue::Float64(x) => (*x as i64).to_string(),
            ScalarValue::Decimal128 { value, scale, .. } => {
                let f = (*value as f64) / 10f64.powi(i32::from(*scale));
                (f.trunc() as i64).to_string()
            }
            other => other.to_display_string(),
        },
        'R' => {
            let f = match v {
                ScalarValue::Float64(x) => *x,
                ScalarValue::Int8(x) => f64::from(*x),
                ScalarValue::Int16(x) => f64::from(*x),
                ScalarValue::Int32(x) => f64::from(*x),
                ScalarValue::Int64(x) => *x as f64,
                ScalarValue::Decimal128 { value, scale, .. } => {
                    (*value as f64) / 10f64.powi(i32::from(*scale))
                }
                _ => 0.0,
            };
            let clean = if f == 0.0 { 0.0 } else { f };
            format!("{clean:.3}")
        }
        'B' => match v {
            ScalarValue::Boolean(b) => if *b { "true" } else { "false" }.to_string(),
            ScalarValue::Int64(x) => if *x != 0 { "true" } else { "false" }.to_string(),
            other => other.to_display_string(),
        },
        _ => {
            let s = v.to_display_string();
            if s.is_empty() {
                "(empty)".to_string()
            } else {
                s
            }
        }
    }
}

/// Runs a single `.slt` script against a fresh `Database` instance.
pub fn run_slt_str(label: &str, src: &str) -> Result<SltStats> {
    let records = parse_slt(src)?;
    let db = Arc::new(Database::new());
    let mut sess = db.new_session();
    let mut stats = SltStats {
        files_run: 1,
        ..Default::default()
    };

    for rec in records {
        match rec {
            SltRecord::Halt => break,
            SltRecord::StatementOk { line, sql } => {
                sess.execute_sql(&sql).map_err(|e| {
                    VectraError::ExecutionError(format!(
                        "{label}:{line}: `statement ok` failed:\nSQL: {sql}\nERR: {e}"
                    ))
                })?;
                stats.statements_run += 1;
                stats.total_assertions += 1;
            }
            SltRecord::StatementError {
                line,
                expected_substr,
                sql,
            } => {
                match sess.execute_sql(&sql) {
                    Ok(_) => {
                        return Err(VectraError::ExecutionError(format!(
                            "{label}:{line}: `statement error` unexpectedly succeeded:\nSQL: {sql}"
                        )));
                    }
                    Err(e) => {
                        if let Some(sub) = expected_substr {
                            let msg = e.to_string().to_ascii_lowercase();
                            if !msg.contains(&sub.to_ascii_lowercase()) {
                                return Err(VectraError::ExecutionError(format!(
                                    "{label}:{line}: error '{e}' did not contain '{sub}'\nSQL: {sql}"
                                )));
                            }
                        }
                    }
                }
                stats.statements_run += 1;
                stats.total_assertions += 1;
            }
            SltRecord::Query {
                line,
                col_types,
                sort_mode,
                sql,
                expected,
            } => {
                let qr = sess.execute_sql(&sql).map_err(|e| {
                    VectraError::ExecutionError(format!(
                        "{label}:{line}: query execution failed:\nSQL: {sql}\nERR: {e}"
                    ))
                })?;
                let type_chars: Vec<char> = col_types.chars().collect();
                let raw_rows = qr.to_scalar_rows();

                let mut formatted_rows: Vec<Vec<String>> = Vec::with_capacity(raw_rows.len());
                for row in &raw_rows {
                    if !type_chars.is_empty() && row.len() != type_chars.len() {
                        return Err(VectraError::ExecutionError(format!(
                            "{label}:{line}: expected {} columns ({col_types}), got {}\nSQL: {sql}",
                            type_chars.len(),
                            row.len()
                        )));
                    }
                    let frow: Vec<String> = row
                        .iter()
                        .enumerate()
                        .map(|(c, v)| {
                            let tc = type_chars.get(c).copied().unwrap_or('T');
                            format_slt_value(v, tc)
                        })
                        .collect();
                    formatted_rows.push(frow);
                }

                let mut flat_vals: Vec<String> = match sort_mode {
                    SltSortMode::NoSort => formatted_rows.into_iter().flatten().collect(),
                    SltSortMode::RowSort => {
                        formatted_rows.sort();
                        formatted_rows.into_iter().flatten().collect()
                    }
                    SltSortMode::ValueSort => {
                        let mut v: Vec<String> = formatted_rows.into_iter().flatten().collect();
                        v.sort();
                        v
                    }
                };

                match expected {
                    SltExpected::Values(exp) => {
                        // Support both single-value-per-line and tab-separated-row-per-line formats.
                        let exp_flat: Vec<String> = if !type_chars.is_empty()
                            && exp.len() * type_chars.len() == flat_vals.len()
                            && exp.iter().all(|l| l.contains('\t'))
                        {
                            exp.iter()
                                .flat_map(|l| l.split('\t').map(ToString::to_string))
                                .collect()
                        } else {
                            exp
                        };
                        if flat_vals != exp_flat {
                            return Err(VectraError::ExecutionError(format!(
                                "{label}:{line}: query result mismatch!\nSQL: {sql}\nEXPECTED ({} vals): {:?}\nACTUAL   ({} vals): {:?}",
                                exp_flat.len(),
                                &exp_flat[..exp_flat.len().min(20)],
                                flat_vals.len(),
                                &flat_vals[..flat_vals.len().min(20)],
                            )));
                        }
                    }
                    SltExpected::Hash { count, md5 } => {
                        if flat_vals.len() != count {
                            return Err(VectraError::ExecutionError(format!(
                                "{label}:{line}: expected {count} hashed values, got {}\nSQL: {sql}",
                                flat_vals.len()
                            )));
                        }
                        let mut payload = String::new();
                        for v in flat_vals.drain(..) {
                            payload.push_str(&v);
                            payload.push('\n');
                        }
                        let actual_md5 = md5_hex(payload.as_bytes());
                        if actual_md5 != md5 {
                            return Err(VectraError::ExecutionError(format!(
                                "{label}:{line}: MD5 hash mismatch: expected {md5}, got {actual_md5}\nSQL: {sql}"
                            )));
                        }
                    }
                }
                stats.queries_run += 1;
                stats.total_assertions += 1;
            }
        }
    }

    Ok(stats)
}

/// Runs a `.slt` file from disk.
pub fn run_slt_file(path: &Path) -> Result<SltStats> {
    let src = std::fs::read_to_string(path)?;
    run_slt_str(&path.display().to_string(), &src)
}

/// Runs every `.slt` file in `dir` in deterministic sorted order.
pub fn run_slt_directory(dir: &Path) -> Result<SltStats> {
    let mut files: Vec<std::path::PathBuf> = std::fs::read_dir(dir)?
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.extension().and_then(|s| s.to_str()) == Some("slt"))
        .collect();
    files.sort();
    let mut total = SltStats::default();
    for path in files {
        let s = run_slt_file(&path)?;
        total.merge(s);
    }
    Ok(total)
}

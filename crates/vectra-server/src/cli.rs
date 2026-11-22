use std::sync::Arc;
use std::time::Instant;
use vectra_exec::{format_bytes, parse_byte_size, Database, QueryResult, Session};
use vectra_types::Result;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutputMode {
    Table,
    Csv,
    Json,
}

pub struct CliState {
    pub db: Arc<Database>,
    pub session: Session,
    pub mode: OutputMode,
    pub timer: bool,
}

impl CliState {
    pub fn new(db: Arc<Database>) -> Self {
        let session = db.new_session();
        Self {
            db,
            session,
            mode: OutputMode::Table,
            timer: true,
        }
    }

    /// Executes a `.meta` command, returning `Ok(Some(output))` (or `Ok(None)` on `.quit`/`.exit`).
    pub fn execute_meta_command(&mut self, line: &str) -> Result<Option<String>> {
        let parts: Vec<&str> = line.split_whitespace().collect();
        let cmd = parts.first().copied().unwrap_or("");
        match cmd {
            ".quit" | ".exit" => Ok(None),
            ".help" => Ok(Some(
                "VectraDB CLI Meta-Commands:\n\
                 \x20 .help                  Show this help message\n\
                 \x20 .tables                List all tables in catalog\n\
                 \x20 .schema [table]        Display table schema(s)\n\
                 \x20 .stats [table]         Display columnar codecs, compression & ZoneMaps\n\
                 \x20 .tpch [sf]             Generate & load TPC-H dataset (default SF=0.01)\n\
                 \x20 .mode table|csv|json   Set output rendering format\n\
                 \x20 .timer on|off          Toggle query wall-time & RAM telemetry footer\n\
                 \x20 .memlimit <size>       Set hard BoundedMemoryPool limit (e.g. 32MB)\n\
                 \x20 .threads <n>           Set morsel-driven worker thread count\n\
                 \x20 .quit / .exit          Exit VectraDB CLI"
                    .to_string(),
            )),
            ".tables" => {
                let tables = self.db.catalog().list_tables();
                if tables.is_empty() {
                    Ok(Some("(no tables)".into()))
                } else {
                    Ok(Some(tables.join("\n")))
                }
            }
            ".schema" => {
                let targets = match parts.get(1) {
                    Some(t) => vec![(*t).to_string()],
                    None => self.db.catalog().list_tables(),
                };
                let mut out = String::new();
                for t in targets {
                    let tbl = self.db.catalog().get_table(&t)?;
                    out.push_str(&format!("CREATE TABLE {} (\n", tbl.name));
                    for (i, f) in tbl.schema().fields.iter().enumerate() {
                        let comma = if i + 1 < tbl.schema().fields.len() {
                            ","
                        } else {
                            ""
                        };
                        let nn = if f.nullable { "" } else { " NOT NULL" };
                        out.push_str(&format!("  {} {}{nn}{comma}\n", f.name, f.logical_type));
                    }
                    out.push_str(");\n");
                }
                Ok(Some(out.trim_end().to_string()))
            }
            ".stats" => {
                let snap = self.db.catalog().begin_txn();
                let targets = match parts.get(1) {
                    Some(t) => vec![(*t).to_string()],
                    None => self.db.catalog().list_tables(),
                };
                let mut out = String::new();
                for t in targets {
                    let sum = self.db.catalog().inspect_table(&t, snap)?;
                    out.push_str(&format!(
                        "Table `{}`: {} rows, {} RowGroups, {} SubBlocks\n",
                        sum.table_name, sum.total_rows, sum.row_groups, sum.sub_blocks
                    ));
                    for c in sum.columns {
                        out.push_str(&format!(
                            "  - {:<18} {:<16} codec={:<10} ratio={:.2}x ({} -> {}) nulls={}\n",
                            c.name,
                            c.logical_type,
                            c.dominant_codec,
                            c.compression_ratio,
                            format_bytes(c.uncompressed_bytes as u64),
                            format_bytes(c.compressed_bytes as u64),
                            c.null_count
                        ));
                    }
                }
                let _ = self.db.catalog().commit_txn(snap.txn_id);
                Ok(Some(out.trim_end().to_string()))
            }
            ".tpch" => {
                let sf: f64 = parts.get(1).and_then(|s| s.parse().ok()).unwrap_or(0.01);
                let t0 = Instant::now();
                let sum = vectra_tpch::load_tpch_into_database(&self.db, sf)?;
                Ok(Some(format!(
                    "Loaded TPC-H SF={sf} ({} total rows, {} lineitem rows) in {:.1} ms.",
                    sum.total_rows(),
                    sum.lineitem_rows,
                    t0.elapsed().as_secs_f64() * 1000.0
                )))
            }
            ".mode" => match parts.get(1).copied().unwrap_or("") {
                "table" => {
                    self.mode = OutputMode::Table;
                    Ok(Some("Output mode: table".into()))
                }
                "csv" => {
                    self.mode = OutputMode::Csv;
                    Ok(Some("Output mode: csv".into()))
                }
                "json" => {
                    self.mode = OutputMode::Json;
                    Ok(Some("Output mode: json".into()))
                }
                other => Ok(Some(format!(
                    "Unknown mode '{other}' (expected table|csv|json)"
                ))),
            },
            ".timer" => match parts.get(1).copied().unwrap_or("on") {
                "on" => {
                    self.timer = true;
                    Ok(Some("Timer: ON".into()))
                }
                "off" => {
                    self.timer = false;
                    Ok(Some("Timer: OFF".into()))
                }
                _ => Ok(Some("Usage: .timer on|off".into())),
            },
            ".memlimit" => {
                let spec = parts.get(1).copied().unwrap_or("256MB");
                let bytes = parse_byte_size(spec)?;
                self.session.set_memory_limit(bytes);
                Ok(Some(format!(
                    "BoundedMemoryPool ceiling set to {}",
                    format_bytes(bytes as u64)
                )))
            }
            ".threads" => {
                let n: usize = parts
                    .get(1)
                    .and_then(|s| s.parse().ok())
                    .unwrap_or(1)
                    .max(1);
                self.session.set_threads(n);
                Ok(Some(format!("Morsel worker threads set to {n}")))
            }
            _ => Ok(Some(format!(
                "Unknown meta-command '{cmd}'. Type .help for available commands."
            ))),
        }
    }

    pub fn execute_sql_formatted(&mut self, sql: &str) -> Result<String> {
        let t0 = Instant::now();
        let qr = self.session.execute_sql(sql)?;
        let elapsed_ms = t0.elapsed().as_secs_f64() * 1000.0;
        let mut out = format_query_result(&qr, self.mode);
        if self.timer {
            let mem_info = qr
                .metrics
                .as_ref()
                .map(|m| {
                    format!(
                        " | peak_ram={} | spilled={}",
                        format_bytes(m.peak_memory_bytes as u64),
                        format_bytes(m.total_spilled_bytes)
                    )
                })
                .unwrap_or_default();
            if !out.is_empty() && !out.ends_with('\n') {
                out.push('\n');
            }
            out.push_str(&format!(
                "[{} | {:.2} ms{mem_info}]",
                qr.command_tag, elapsed_ms
            ));
        }
        Ok(out)
    }
}

pub fn format_query_result(qr: &QueryResult, mode: OutputMode) -> String {
    if qr.schema.is_empty() {
        return qr.command_tag.clone();
    }
    let headers: Vec<String> = qr.schema.fields.iter().map(|f| f.name.clone()).collect();
    let rows: Vec<Vec<String>> = qr
        .to_scalar_rows()
        .into_iter()
        .map(|r| {
            r.into_iter()
                .map(|v| {
                    if v.is_null() {
                        "NULL".to_string()
                    } else {
                        v.to_display_string()
                    }
                })
                .collect()
        })
        .collect();

    match mode {
        OutputMode::Csv => {
            let mut out = headers.join(",");
            out.push('\n');
            for r in rows {
                out.push_str(&r.join(","));
                out.push('\n');
            }
            out
        }
        OutputMode::Json => {
            let json_rows: Vec<serde_json::Value> = rows
                .into_iter()
                .map(|r| {
                    let mut map = serde_json::Map::new();
                    for (h, v) in headers.iter().zip(r) {
                        map.insert(h.clone(), serde_json::Value::String(v));
                    }
                    serde_json::Value::Object(map)
                })
                .collect();
            serde_json::to_string_pretty(&json_rows).unwrap_or_else(|_| "[]".into())
        }
        OutputMode::Table => format_unicode_box_table(&headers, &rows),
    }
}

pub fn format_unicode_box_table(headers: &[String], rows: &[Vec<String>]) -> String {
    let ncols = headers.len();
    let mut widths: Vec<usize> = headers.iter().map(|h| h.chars().count()).collect();
    for r in rows {
        for (i, cell) in r.iter().enumerate().take(ncols) {
            widths[i] = widths[i].max(cell.chars().count());
        }
    }

    let make_border = |l: &str, m: &str, r: &str| -> String {
        let mut s = String::from(l);
        for (i, &w) in widths.iter().enumerate() {
            s.push_str(&"─".repeat(w + 2));
            if i + 1 < ncols {
                s.push_str(m);
            }
        }
        s.push_str(r);
        s.push('\n');
        s
    };

    let mut out = String::new();
    out.push_str(&make_border("┌", "┬", "┐"));
    out.push('│');
    for (i, h) in headers.iter().enumerate() {
        out.push_str(&format!(" {:width$} │", h, width = widths[i]));
    }
    out.push('\n');
    out.push_str(&make_border("├", "┼", "┤"));
    for r in rows {
        out.push('│');
        for (i, cell) in r.iter().enumerate().take(ncols) {
            out.push_str(&format!(" {:width$} │", cell, width = widths[i]));
        }
        out.push('\n');
    }
    out.push_str(&make_border("└", "┴", "┘"));
    out
}

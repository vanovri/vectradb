//! RFC-4180 compliant chunked CSV parser with custom delimiters, quoted fields,
//! automatic schema inference, and direct vectorized `DataChunk` loading.

use std::sync::Arc;
use vectra_types::{
    parse_date32, parse_decimal, parse_timestamp_micros, DataChunk, Field, LogicalType, MemoryPool,
    Result, ScalarValue, Schema, Vector, VectraError, VECTOR_CAPACITY,
};

#[derive(Debug, Clone)]
pub struct CsvOptions {
    pub delimiter: u8,
    pub has_header: bool,
    pub null_literal: String,
    pub batch_size: usize,
}

impl Default for CsvOptions {
    fn default() -> Self {
        Self {
            delimiter: b',',
            has_header: true,
            null_literal: String::new(),
            batch_size: VECTOR_CAPACITY,
        }
    }
}

/// Parses a single RFC-4180 CSV line (supporting `"` quotes and `""` escaped quotes).
pub fn parse_csv_record(line: &str, delimiter: u8) -> Vec<String> {
    let bytes = line.as_bytes();
    let mut fields = Vec::new();
    let mut i = 0usize;
    let n = bytes.len();

    while i <= n {
        if i == n {
            if !fields.is_empty() && bytes.last() == Some(&delimiter) {
                fields.push(String::new());
            }
            break;
        }
        if bytes[i] == b'"' {
            i += 1;
            let mut val = String::new();
            while i < n {
                if bytes[i] == b'"' {
                    if i + 1 < n && bytes[i + 1] == b'"' {
                        val.push('"');
                        i += 2;
                    } else {
                        i += 1;
                        break;
                    }
                } else {
                    let start = i;
                    while i < n && bytes[i] != b'"' {
                        i += 1;
                    }
                    val.push_str(&line[start..i]);
                }
            }
            fields.push(val);
            if i < n && bytes[i] == delimiter {
                i += 1;
                if i == n {
                    fields.push(String::new());
                    break;
                }
            }
        } else {
            let start = i;
            while i < n && bytes[i] != delimiter {
                i += 1;
            }
            fields.push(line[start..i].to_string());
            if i < n && bytes[i] == delimiter {
                i += 1;
                if i == n {
                    fields.push(String::new());
                    break;
                }
            } else {
                break;
            }
        }
    }
    fields
}

/// Infers a `Schema` from CSV text by sampling up to 256 data rows.
pub fn infer_csv_schema(csv_text: &str, opts: &CsvOptions) -> Result<Schema> {
    let mut lines = csv_text
        .lines()
        .map(|l| l.trim_end_matches('\r'))
        .filter(|l| !l.is_empty());
    let first_line = lines
        .next()
        .ok_or_else(|| VectraError::CorruptFormat("empty CSV input".into()))?;
    let first_fields = parse_csv_record(first_line, opts.delimiter);
    let num_cols = first_fields.len();

    let (names, sample_rows): (Vec<String>, Vec<Vec<String>>) = if opts.has_header {
        let rows: Vec<Vec<String>> = lines
            .take(256)
            .map(|l| parse_csv_record(l, opts.delimiter))
            .collect();
        (first_fields, rows)
    } else {
        let names = (0..num_cols).map(|i| format!("c{i}")).collect();
        let mut rows = vec![first_fields];
        rows.extend(lines.take(255).map(|l| parse_csv_record(l, opts.delimiter)));
        (names, rows)
    };

    let mut fields = Vec::with_capacity(num_cols);
    for (c, name) in names.into_iter().enumerate() {
        let mut all_bool = true;
        let mut all_int = true;
        let mut all_float = true;
        let mut all_date = true;
        let mut seen_non_null = false;

        for row in &sample_rows {
            let cell = row.get(c).map(|s| s.trim()).unwrap_or("");
            if cell.is_empty() || cell == opts.null_literal || cell.eq_ignore_ascii_case("NULL") {
                continue;
            }
            seen_non_null = true;
            if !matches!(cell.to_ascii_lowercase().as_str(), "true" | "false") {
                all_bool = false;
            }
            if cell.parse::<i64>().is_err() {
                all_int = false;
            }
            if cell.parse::<f64>().is_err() {
                all_float = false;
            }
            if parse_date32(cell).is_err() {
                all_date = false;
            }
        }

        let ty = if !seen_non_null {
            LogicalType::Utf8
        } else if all_bool {
            LogicalType::Boolean
        } else if all_int {
            LogicalType::Int64
        } else if all_float {
            LogicalType::Float64
        } else if all_date {
            LogicalType::Date32
        } else {
            LogicalType::Utf8
        };
        fields.push(Field::new(name, ty, true));
    }

    Ok(Schema::new(fields))
}

/// Loads CSV text into typed `DataChunk` batches matching `schema` (or inferred schema if `None`).
pub fn load_csv_chunks(
    csv_text: &str,
    schema_opt: Option<&Schema>,
    opts: &CsvOptions,
    pool: &Arc<dyn MemoryPool>,
) -> Result<(Schema, Vec<DataChunk>)> {
    let schema = match schema_opt {
        Some(s) => s.clone(),
        None => infer_csv_schema(csv_text, opts)?,
    };
    let mut lines = csv_text
        .lines()
        .map(|l| l.trim_end_matches('\r'))
        .filter(|l| !l.is_empty());
    if opts.has_header {
        let _ = lines.next();
    }

    let num_cols = schema.len();
    let batch_size = opts.batch_size.max(1);
    let mut chunks = Vec::new();
    let mut col_bufs: Vec<Vec<ScalarValue>> = (0..num_cols)
        .map(|_| Vec::with_capacity(batch_size))
        .collect();
    let mut buffered = 0usize;

    for line in lines {
        let rec = parse_csv_record(line, opts.delimiter);
        for (c, field) in schema.fields.iter().enumerate() {
            let cell = rec.get(c).map(|s| s.as_str()).unwrap_or("");
            let trimmed = cell.trim();
            let val = if trimmed.is_empty()
                || trimmed == opts.null_literal
                || trimmed.eq_ignore_ascii_case("NULL")
            {
                ScalarValue::Null
            } else {
                parse_csv_cell(trimmed, field.logical_type)?
            };
            col_bufs[c].push(val);
        }
        buffered += 1;
        if buffered >= batch_size {
            chunks.push(flush_csv_batch(&schema, &mut col_bufs, buffered, pool)?);
            buffered = 0;
        }
    }

    if buffered > 0 {
        chunks.push(flush_csv_batch(&schema, &mut col_bufs, buffered, pool)?);
    }

    Ok((schema, chunks))
}

fn flush_csv_batch(
    schema: &Schema,
    col_bufs: &mut [Vec<ScalarValue>],
    rows: usize,
    pool: &Arc<dyn MemoryPool>,
) -> Result<DataChunk> {
    let mut vecs = Vec::with_capacity(schema.len());
    for (c, field) in schema.fields.iter().enumerate() {
        vecs.push(Vector::from_scalars(
            &col_bufs[c],
            field.logical_type,
            pool,
        )?);
        col_bufs[c].clear();
    }
    Ok(DataChunk::new(vecs, rows))
}

fn parse_csv_cell(s: &str, ty: LogicalType) -> Result<ScalarValue> {
    let bad = || VectraError::InvalidCast {
        from: "CSV".into(),
        to: ty.to_string(),
        value: s.to_string(),
    };
    match ty {
        LogicalType::Null => Ok(ScalarValue::Null),
        LogicalType::Boolean => match s.to_ascii_lowercase().as_str() {
            "true" | "t" | "1" => Ok(ScalarValue::Boolean(true)),
            "false" | "f" | "0" => Ok(ScalarValue::Boolean(false)),
            _ => Err(bad()),
        },
        LogicalType::Int8 => Ok(ScalarValue::Int8(s.parse().map_err(|_| bad())?)),
        LogicalType::Int16 => Ok(ScalarValue::Int16(s.parse().map_err(|_| bad())?)),
        LogicalType::Int32 => Ok(ScalarValue::Int32(s.parse().map_err(|_| bad())?)),
        LogicalType::Int64 => Ok(ScalarValue::Int64(s.parse().map_err(|_| bad())?)),
        LogicalType::Float64 => Ok(ScalarValue::Float64(s.parse().map_err(|_| bad())?)),
        LogicalType::Decimal128 { precision, scale } => Ok(ScalarValue::Decimal128 {
            value: parse_decimal(s, precision, scale)?,
            precision,
            scale,
        }),
        LogicalType::Date32 => Ok(ScalarValue::Date32(parse_date32(s)?)),
        LogicalType::TimestampMicros => {
            Ok(ScalarValue::TimestampMicros(parse_timestamp_micros(s)?))
        }
        LogicalType::Utf8 => Ok(ScalarValue::Utf8(s.to_string())),
        LogicalType::Binary => Ok(ScalarValue::Binary(s.as_bytes().to_vec())),
    }
}

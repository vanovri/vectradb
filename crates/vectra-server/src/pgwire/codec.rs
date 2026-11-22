use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use vectra_types::{LogicalType, Result, ScalarValue, Schema, VectraError};

pub const PROTOCOL_VERSION_3_0: i32 = 196_608; // 0x0003_0000
pub const SSL_REQUEST_CODE: i32 = 80_877_103;
pub const CANCEL_REQUEST_CODE: i32 = 80_877_102;

// Standard PostgreSQL Type OIDs
pub const PG_OID_BOOL: i32 = 16;
pub const PG_OID_BYTEA: i32 = 17;
pub const PG_OID_INT8: i32 = 20;
pub const PG_OID_INT2: i32 = 21;
pub const PG_OID_INT4: i32 = 23;
pub const PG_OID_TEXT: i32 = 25;
pub const PG_OID_FLOAT8: i32 = 701;
pub const PG_OID_DATE: i32 = 1082;
pub const PG_OID_TIMESTAMP: i32 = 1114;
pub const PG_OID_NUMERIC: i32 = 1700;

pub fn logical_type_to_pg_oid(ty: LogicalType) -> (i32, i16) {
    match ty {
        LogicalType::Boolean => (PG_OID_BOOL, 1),
        LogicalType::Int8 | LogicalType::Int16 => (PG_OID_INT2, 2),
        LogicalType::Int32 => (PG_OID_INT4, 4),
        LogicalType::Int64 => (PG_OID_INT8, 8),
        LogicalType::Float64 => (PG_OID_FLOAT8, 8),
        LogicalType::Decimal128 { .. } => (PG_OID_NUMERIC, -1),
        LogicalType::Date32 => (PG_OID_DATE, 4),
        LogicalType::TimestampMicros => (PG_OID_TIMESTAMP, 8),
        LogicalType::Binary => (PG_OID_BYTEA, -1),
        LogicalType::Utf8 | LogicalType::Null => (PG_OID_TEXT, -1),
    }
}

pub fn parse_pg_param(oid: i32, raw: Option<&[u8]>) -> Result<ScalarValue> {
    let Some(bytes) = raw else {
        return Ok(ScalarValue::Null);
    };
    let s = std::str::from_utf8(bytes)
        .map_err(|e| VectraError::ExecutionError(format!("invalid UTF-8 parameter: {e}")))?;
    match oid {
        PG_OID_BOOL => {
            let b = matches!(
                s.to_ascii_lowercase().as_str(),
                "t" | "true" | "1" | "on" | "yes"
            );
            Ok(ScalarValue::Boolean(b))
        }
        PG_OID_INT2 | PG_OID_INT4 | PG_OID_INT8 => {
            let v: i64 = s.trim().parse().map_err(|_| VectraError::InvalidCast {
                from: "Utf8".into(),
                to: "Int64".into(),
                value: s.to_string(),
            })?;
            Ok(ScalarValue::Int64(v))
        }
        PG_OID_FLOAT8 => {
            let v: f64 = s.trim().parse().map_err(|_| VectraError::InvalidCast {
                from: "Utf8".into(),
                to: "Float64".into(),
                value: s.to_string(),
            })?;
            Ok(ScalarValue::Float64(v))
        }
        PG_OID_DATE => Ok(ScalarValue::Date32(vectra_types::parse_date32(s)?)),
        PG_OID_TIMESTAMP => Ok(ScalarValue::TimestampMicros(
            vectra_types::parse_timestamp_micros(s)?,
        )),
        PG_OID_BYTEA => Ok(ScalarValue::Binary(bytes.to_vec())),
        PG_OID_TEXT => Ok(ScalarValue::Utf8(s.to_string())),
        _ => {
            // Untyped (OID 0): infer integer, float, boolean, or fallback to text cleanly.
            let t = s.trim();
            if let Ok(n) = t.parse::<i64>() {
                Ok(ScalarValue::Int64(n))
            } else if let Ok(f) = t.parse::<f64>() {
                Ok(ScalarValue::Float64(f))
            } else if t.eq_ignore_ascii_case("true") {
                Ok(ScalarValue::Boolean(true))
            } else if t.eq_ignore_ascii_case("false") {
                Ok(ScalarValue::Boolean(false))
            } else {
                Ok(ScalarValue::Utf8(s.to_string()))
            }
        }
    }
}

#[derive(Debug, Clone)]
pub enum StartupPacket {
    SslRequest,
    CancelRequest { pid: i32, secret_key: i32 },
    Startup { params: Vec<(String, String)> },
}

#[derive(Debug, Clone)]
pub enum FrontendMessage {
    SimpleQuery(String),
    Parse {
        stmt_name: String,
        sql: String,
        param_oids: Vec<i32>,
    },
    Bind {
        portal_name: String,
        stmt_name: String,
        param_values: Vec<Option<Vec<u8>>>,
    },
    Describe {
        kind: u8, // b'S' or b'P'
        name: String,
    },
    Execute {
        portal_name: String,
        max_rows: i32,
    },
    Close {
        kind: u8,
        name: String,
    },
    Sync,
    Flush,
    Terminate,
}

#[derive(Debug, Clone)]
pub enum BackendMessage {
    AuthenticationOk,
    ParameterStatus { name: String, value: String },
    BackendKeyData { pid: i32, secret_key: i32 },
    ReadyForQuery(u8),
    RowDescription(Vec<(String, i32, i16)>),
    ParameterDescription(Vec<i32>),
    NoData,
    DataRow(Vec<Option<String>>),
    CommandComplete(String),
    EmptyQueryResponse,
    ParseComplete,
    BindComplete,
    CloseComplete,
    PortalSuspended,
    ErrorResponse { sqlstate: String, message: String },
}

pub async fn read_startup_packet<R: AsyncRead + Unpin>(
    reader: &mut R,
) -> Result<Option<StartupPacket>> {
    let mut len_buf = [0u8; 4];
    match reader.read_exact(&mut len_buf).await {
        Ok(_) => {}
        Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(e) => return Err(VectraError::Io(e)),
    }
    let total_len = i32::from_be_bytes(len_buf);
    if !(8..=65_536).contains(&total_len) {
        return Err(VectraError::CorruptFormat(format!(
            "invalid PG startup packet length {total_len}"
        )));
    }
    let mut body = vec![0u8; (total_len - 4) as usize];
    reader.read_exact(&mut body).await?;
    let code = i32::from_be_bytes(body[0..4].try_into().unwrap());
    match code {
        SSL_REQUEST_CODE => Ok(Some(StartupPacket::SslRequest)),
        CANCEL_REQUEST_CODE => {
            if body.len() < 12 {
                return Err(VectraError::CorruptFormat("truncated CancelRequest".into()));
            }
            let pid = i32::from_be_bytes(body[4..8].try_into().unwrap());
            let secret_key = i32::from_be_bytes(body[8..12].try_into().unwrap());
            Ok(Some(StartupPacket::CancelRequest { pid, secret_key }))
        }
        PROTOCOL_VERSION_3_0 => {
            let mut params = Vec::new();
            let mut pos = 4usize;
            while pos < body.len() && body[pos] != 0 {
                let k = read_cstr(&body, &mut pos)?;
                let v = read_cstr(&body, &mut pos)?;
                params.push((k, v));
            }
            Ok(Some(StartupPacket::Startup { params }))
        }
        other => Err(VectraError::CorruptFormat(format!(
            "unsupported PG startup code {other}"
        ))),
    }
}

pub async fn read_frontend_message<R: AsyncRead + Unpin>(
    reader: &mut R,
) -> Result<Option<FrontendMessage>> {
    let mut hdr = [0u8; 5];
    match reader.read_exact(&mut hdr).await {
        Ok(_) => {}
        Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(e) => return Err(VectraError::Io(e)),
    }
    let tag = hdr[0];
    let len = i32::from_be_bytes([hdr[1], hdr[2], hdr[3], hdr[4]]);
    if !(4..=64 * 1024 * 1024).contains(&len) {
        return Err(VectraError::CorruptFormat(format!(
            "invalid PG message length {len}"
        )));
    }
    let mut body = vec![0u8; (len - 4) as usize];
    if !body.is_empty() {
        reader.read_exact(&mut body).await?;
    }

    match tag {
        b'Q' => {
            let mut pos = 0;
            let sql = read_cstr(&body, &mut pos)?;
            Ok(Some(FrontendMessage::SimpleQuery(sql)))
        }
        b'P' => {
            let mut pos = 0;
            let stmt_name = read_cstr(&body, &mut pos)?;
            let sql = read_cstr(&body, &mut pos)?;
            let nparams = read_i16(&body, &mut pos)? as usize;
            let mut param_oids = Vec::with_capacity(nparams);
            for _ in 0..nparams {
                param_oids.push(read_i32(&body, &mut pos)?);
            }
            Ok(Some(FrontendMessage::Parse {
                stmt_name,
                sql,
                param_oids,
            }))
        }
        b'B' => {
            let mut pos = 0;
            let portal_name = read_cstr(&body, &mut pos)?;
            let stmt_name = read_cstr(&body, &mut pos)?;
            let n_pfmt = read_i16(&body, &mut pos)? as usize;
            for _ in 0..n_pfmt {
                let _ = read_i16(&body, &mut pos)?;
            }
            let n_pvals = read_i16(&body, &mut pos)? as usize;
            let mut param_values = Vec::with_capacity(n_pvals);
            for _ in 0..n_pvals {
                let vlen = read_i32(&body, &mut pos)?;
                if vlen < 0 {
                    param_values.push(None);
                } else {
                    let ul = vlen as usize;
                    if pos + ul > body.len() {
                        return Err(VectraError::CorruptFormat("truncated Bind param".into()));
                    }
                    param_values.push(Some(body[pos..pos + ul].to_vec()));
                    pos += ul;
                }
            }
            // Skip result format codes if present.
            Ok(Some(FrontendMessage::Bind {
                portal_name,
                stmt_name,
                param_values,
            }))
        }
        b'D' => {
            if body.is_empty() {
                return Err(VectraError::CorruptFormat("empty Describe body".into()));
            }
            let kind = body[0];
            let mut pos = 1;
            let name = read_cstr(&body, &mut pos)?;
            Ok(Some(FrontendMessage::Describe { kind, name }))
        }
        b'E' => {
            let mut pos = 0;
            let portal_name = read_cstr(&body, &mut pos)?;
            let max_rows = read_i32(&body, &mut pos)?;
            Ok(Some(FrontendMessage::Execute {
                portal_name,
                max_rows,
            }))
        }
        b'C' => {
            if body.is_empty() {
                return Err(VectraError::CorruptFormat("empty Close body".into()));
            }
            let kind = body[0];
            let mut pos = 1;
            let name = read_cstr(&body, &mut pos)?;
            Ok(Some(FrontendMessage::Close { kind, name }))
        }
        b'S' => Ok(Some(FrontendMessage::Sync)),
        b'H' => Ok(Some(FrontendMessage::Flush)),
        b'X' => Ok(Some(FrontendMessage::Terminate)),
        other => Err(VectraError::CorruptFormat(format!(
            "unsupported PG frontend tag '{}'",
            other as char
        ))),
    }
}

pub fn encode_backend_message(msg: &BackendMessage, out: &mut Vec<u8>) {
    let mut payload = Vec::new();
    let tag: u8 = match msg {
        BackendMessage::AuthenticationOk => {
            payload.extend_from_slice(&0i32.to_be_bytes());
            b'R'
        }
        BackendMessage::ParameterStatus { name, value } => {
            payload.extend_from_slice(name.as_bytes());
            payload.push(0);
            payload.extend_from_slice(value.as_bytes());
            payload.push(0);
            b'S'
        }
        BackendMessage::BackendKeyData { pid, secret_key } => {
            payload.extend_from_slice(&pid.to_be_bytes());
            payload.extend_from_slice(&secret_key.to_be_bytes());
            b'K'
        }
        BackendMessage::ReadyForQuery(status) => {
            payload.push(*status);
            b'Z'
        }
        BackendMessage::RowDescription(cols) => {
            payload.extend_from_slice(&(cols.len() as i16).to_be_bytes());
            for (name, oid, typlen) in cols {
                payload.extend_from_slice(name.as_bytes());
                payload.push(0);
                payload.extend_from_slice(&0i32.to_be_bytes()); // table OID
                payload.extend_from_slice(&0i16.to_be_bytes()); // attr num
                payload.extend_from_slice(&oid.to_be_bytes());
                payload.extend_from_slice(&typlen.to_be_bytes());
                payload.extend_from_slice(&(-1i32).to_be_bytes()); // typmod
                payload.extend_from_slice(&0i16.to_be_bytes()); // text format = 0
            }
            b'T'
        }
        BackendMessage::ParameterDescription(oids) => {
            payload.extend_from_slice(&(oids.len() as i16).to_be_bytes());
            for &oid in oids {
                payload.extend_from_slice(&oid.to_be_bytes());
            }
            b't'
        }
        BackendMessage::NoData => b'n',
        BackendMessage::DataRow(fields) => {
            payload.extend_from_slice(&(fields.len() as i16).to_be_bytes());
            for f in fields {
                match f {
                    None => payload.extend_from_slice(&(-1i32).to_be_bytes()),
                    Some(s) => {
                        payload.extend_from_slice(&(s.len() as i32).to_be_bytes());
                        payload.extend_from_slice(s.as_bytes());
                    }
                }
            }
            b'D'
        }
        BackendMessage::CommandComplete(tag_str) => {
            payload.extend_from_slice(tag_str.as_bytes());
            payload.push(0);
            b'C'
        }
        BackendMessage::EmptyQueryResponse => b'I',
        BackendMessage::ParseComplete => b'1',
        BackendMessage::BindComplete => b'2',
        BackendMessage::CloseComplete => b'3',
        BackendMessage::PortalSuspended => b's',
        BackendMessage::ErrorResponse { sqlstate, message } => {
            payload.push(b'S');
            payload.extend_from_slice(b"ERROR\0");
            payload.push(b'V');
            payload.extend_from_slice(b"ERROR\0");
            payload.push(b'C');
            payload.extend_from_slice(sqlstate.as_bytes());
            payload.push(0);
            payload.push(b'M');
            payload.extend_from_slice(message.as_bytes());
            payload.push(0);
            payload.push(0);
            b'E'
        }
    };
    out.push(tag);
    out.extend_from_slice(&((payload.len() + 4) as i32).to_be_bytes());
    out.extend_from_slice(&payload);
}

pub async fn write_backend_message<W: AsyncWrite + Unpin>(
    writer: &mut W,
    msg: &BackendMessage,
) -> Result<()> {
    let mut buf = Vec::with_capacity(128);
    encode_backend_message(msg, &mut buf);
    writer.write_all(&buf).await?;
    Ok(())
}

pub fn schema_to_row_description(schema: &Schema) -> BackendMessage {
    let cols = schema
        .fields
        .iter()
        .map(|f| {
            let (oid, typlen) = logical_type_to_pg_oid(f.logical_type);
            (f.name.clone(), oid, typlen)
        })
        .collect();
    BackendMessage::RowDescription(cols)
}

pub fn format_pg_scalar(v: &ScalarValue) -> Option<String> {
    if v.is_null() {
        None
    } else if let ScalarValue::Boolean(b) = v {
        Some(if *b { "t" } else { "f" }.to_string())
    } else {
        Some(v.to_display_string())
    }
}

fn read_cstr(buf: &[u8], pos: &mut usize) -> Result<String> {
    let start = *pos;
    while *pos < buf.len() && buf[*pos] != 0 {
        *pos += 1;
    }
    let s = String::from_utf8_lossy(&buf[start..*pos]).into_owned();
    if *pos < buf.len() {
        *pos += 1;
    }
    Ok(s)
}

fn read_i16(buf: &[u8], pos: &mut usize) -> Result<i16> {
    if *pos + 2 > buf.len() {
        return Err(VectraError::CorruptFormat("truncated PG i16".into()));
    }
    let v = i16::from_be_bytes(buf[*pos..*pos + 2].try_into().unwrap());
    *pos += 2;
    Ok(v)
}

fn read_i32(buf: &[u8], pos: &mut usize) -> Result<i32> {
    if *pos + 4 > buf.len() {
        return Err(VectraError::CorruptFormat("truncated PG i32".into()));
    }
    let v = i32::from_be_bytes(buf[*pos..*pos + 4].try_into().unwrap());
    *pos += 4;
    Ok(v)
}

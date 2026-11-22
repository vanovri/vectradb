use crate::pgwire::codec::{
    encode_backend_message, format_pg_scalar, parse_pg_param, read_frontend_message,
    read_startup_packet, schema_to_row_description, write_backend_message, BackendMessage,
    FrontendMessage, StartupPacket,
};
use parking_lot::Mutex;
use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, AtomicI32, Ordering};
use std::sync::Arc;
use tokio::io::{AsyncWriteExt, BufReader, BufWriter};
use tokio::net::{TcpListener, TcpStream};
use vectra_exec::{chunk_from_scalar_rows, Database, QueryResult, Session};
use vectra_types::{BoundedMemoryPool, Field, LogicalType, Result, ScalarValue, Schema};

type ActiveCancelRegistry = Arc<Mutex<HashMap<(i32, i32), Arc<AtomicBool>>>>;

struct PreparedStmt {
    sql: String,
    param_oids: Vec<i32>,
}

struct BoundPortal {
    stmt_name: String,
    params: Vec<ScalarValue>,
    /// Lazily populated rows on first Execute so `PortalSuspended` (`max_rows > 0`) resumes cleanly!
    cached: Option<(Schema, Vec<Vec<ScalarValue>>, usize, String)>,
}

/// Async PostgreSQL v3 Wire Protocol TCP Server backed by `Arc<Database>`.
pub struct PgServer {
    db: Arc<Database>,
    next_pid: Arc<AtomicI32>,
    active_cancels: ActiveCancelRegistry,
}

impl PgServer {
    pub fn new(db: Arc<Database>) -> Self {
        Self {
            db,
            next_pid: Arc::new(AtomicI32::new(1000)),
            active_cancels: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// Binds to `addr`, returning the bound `SocketAddr` (supports ephemeral `:0`) and serving loop.
    pub async fn bind(&self, addr: &str) -> Result<(TcpListener, SocketAddr)> {
        let listener = TcpListener::bind(addr).await?;
        let local = listener.local_addr()?;
        Ok((listener, local))
    }

    /// Runs the accept loop on `listener` until `shutdown` fires.
    pub async fn serve(
        self: Arc<Self>,
        listener: TcpListener,
        shutdown: Arc<AtomicBool>,
    ) -> Result<()> {
        loop {
            if shutdown.load(Ordering::Relaxed) {
                break;
            }
            let accept_res = tokio::select! {
                res = listener.accept() => res,
                _ = tokio::time::sleep(std::time::Duration::from_millis(50)) => continue,
            };
            let (stream, _peer) = match accept_res {
                Ok(s) => s,
                Err(_) => continue,
            };
            let _ = stream.set_nodelay(true);
            let server = Arc::clone(&self);
            tokio::spawn(async move {
                let _ = server.handle_connection(stream).await;
            });
        }
        Ok(())
    }

    async fn handle_connection(&self, stream: TcpStream) -> Result<()> {
        let (read_half, write_half) = stream.into_split();
        let mut reader = BufReader::new(read_half);
        let mut writer = BufWriter::new(write_half);

        // 1. Handshake: handle optional SSLRequest ('N'), CancelRequest, or StartupMessage.
        loop {
            let Some(pkt) = read_startup_packet(&mut reader).await? else {
                return Ok(());
            };
            match pkt {
                StartupPacket::SslRequest => {
                    writer.write_all(b"N").await?;
                    writer.flush().await?;
                }
                StartupPacket::CancelRequest { pid, secret_key } => {
                    if let Some(flag) = self.active_cancels.lock().get(&(pid, secret_key)).cloned()
                    {
                        flag.store(true, Ordering::Release);
                    }
                    return Ok(());
                }
                StartupPacket::Startup { .. } => break,
            }
        }

        let pid = self.next_pid.fetch_add(1, Ordering::Relaxed);
        let secret_key = (pid.wrapping_mul(1_103_515_245).wrapping_add(12_345)) & 0x7fff_ffff;
        let cancel_flag = Arc::new(AtomicBool::new(false));
        self.active_cancels
            .lock()
            .insert((pid, secret_key), Arc::clone(&cancel_flag));

        let mut init_buf = Vec::with_capacity(256);
        encode_backend_message(&BackendMessage::AuthenticationOk, &mut init_buf);
        for (k, v) in [
            ("server_version", "16.0 (VectraDB 0.1.0)"),
            ("client_encoding", "UTF8"),
            ("server_encoding", "UTF8"),
            ("DateStyle", "ISO, MDY"),
            ("TimeZone", "UTC"),
            ("standard_conforming_strings", "on"),
            ("integer_datetimes", "on"),
        ] {
            encode_backend_message(
                &BackendMessage::ParameterStatus {
                    name: k.into(),
                    value: v.into(),
                },
                &mut init_buf,
            );
        }
        encode_backend_message(
            &BackendMessage::BackendKeyData { pid, secret_key },
            &mut init_buf,
        );
        encode_backend_message(&BackendMessage::ReadyForQuery(b'I'), &mut init_buf);
        writer.write_all(&init_buf).await?;
        writer.flush().await?;

        // Per-connection session wrapped in Arc<Mutex<Session>> so blocking queries run on spawn_blocking.
        let session = Arc::new(Mutex::new(self.db.new_session()));
        let mut stmts: HashMap<String, PreparedStmt> = HashMap::new();
        let mut portals: HashMap<String, BoundPortal> = HashMap::new();

        // 2. Main Message Loop
        let loop_res: Result<()> = async {
            while let Some(msg) = read_frontend_message(&mut reader).await? {
                match msg {
                    FrontendMessage::Terminate => break,
                    FrontendMessage::SimpleQuery(sql) => {
                        let trimmed = sql.trim();
                        if trimmed.is_empty() {
                            write_backend_message(&mut writer, &BackendMessage::EmptyQueryResponse)
                                .await?;
                            let st = txn_status_byte(&session);
                            write_backend_message(&mut writer, &BackendMessage::ReadyForQuery(st))
                                .await?;
                            writer.flush().await?;
                            continue;
                        }

                        cancel_flag.store(false, Ordering::Release);
                        let res = execute_compat_or_sql(
                            Arc::clone(&self.db),
                            Arc::clone(&session),
                            trimmed.to_string(),
                            Vec::new(),
                            Arc::clone(&cancel_flag),
                        )
                        .await;

                        match res {
                            Ok(qr) => {
                                write_query_result(&mut writer, &qr).await?;
                            }
                            Err(e) => {
                                write_backend_message(
                                    &mut writer,
                                    &BackendMessage::ErrorResponse {
                                        sqlstate: e.sqlstate().to_string(),
                                        message: e.to_string(),
                                    },
                                )
                                .await?;
                            }
                        }
                        let st = txn_status_byte(&session);
                        write_backend_message(&mut writer, &BackendMessage::ReadyForQuery(st))
                            .await?;
                        writer.flush().await?;
                    }
                    FrontendMessage::Parse {
                        stmt_name,
                        sql,
                        param_oids,
                    } => {
                        // Validate syntax immediately during Parse!
                        let parse_ok =
                            is_pg_compat_query(&sql) || vectra_sql::parse_sql(&sql).is_ok();
                        if parse_ok {
                            stmts.insert(stmt_name, PreparedStmt { sql, param_oids });
                            write_backend_message(&mut writer, &BackendMessage::ParseComplete)
                                .await?;
                        } else if let Err(e) = vectra_sql::parse_sql(&sql) {
                            write_backend_message(
                                &mut writer,
                                &BackendMessage::ErrorResponse {
                                    sqlstate: e.sqlstate().to_string(),
                                    message: e.to_string(),
                                },
                            )
                            .await?;
                        }
                    }
                    FrontendMessage::Bind {
                        portal_name,
                        stmt_name,
                        param_values,
                    } => {
                        let Some(ps) = stmts.get(&stmt_name) else {
                            write_backend_message(
                                &mut writer,
                                &BackendMessage::ErrorResponse {
                                    sqlstate: "26000".into(),
                                    message: format!(
                                        "prepared statement '{stmt_name}' does not exist"
                                    ),
                                },
                            )
                            .await?;
                            continue;
                        };
                        let mut parsed_params = Vec::with_capacity(param_values.len());
                        let mut bind_err = None;
                        for (idx, raw) in param_values.iter().enumerate() {
                            let oid = ps.param_oids.get(idx).copied().unwrap_or(0);
                            match parse_pg_param(oid, raw.as_deref()) {
                                Ok(v) => parsed_params.push(v),
                                Err(e) => {
                                    bind_err = Some(e);
                                    break;
                                }
                            }
                        }
                        if let Some(e) = bind_err {
                            write_backend_message(
                                &mut writer,
                                &BackendMessage::ErrorResponse {
                                    sqlstate: e.sqlstate().to_string(),
                                    message: e.to_string(),
                                },
                            )
                            .await?;
                        } else {
                            portals.insert(
                                portal_name,
                                BoundPortal {
                                    stmt_name,
                                    params: parsed_params,
                                    cached: None,
                                },
                            );
                            write_backend_message(&mut writer, &BackendMessage::BindComplete)
                                .await?;
                        }
                    }
                    FrontendMessage::Describe { kind, name } => {
                        let stmt_opt = if kind == b'S' {
                            stmts.get(&name)
                        } else {
                            portals.get(&name).and_then(|p| stmts.get(&p.stmt_name))
                        };
                        let Some(ps) = stmt_opt else {
                            write_backend_message(
                                &mut writer,
                                &BackendMessage::ErrorResponse {
                                    sqlstate: "34000".into(),
                                    message: format!("unknown statement/portal '{name}'"),
                                },
                            )
                            .await?;
                            continue;
                        };
                        if kind == b'S' {
                            write_backend_message(
                                &mut writer,
                                &BackendMessage::ParameterDescription(ps.param_oids.clone()),
                            )
                            .await?;
                        }
                        // Compute output RowDescription cleanly if it's a query!
                        let desc_params: Vec<ScalarValue> = if kind == b'P' {
                            portals
                                .get(&name)
                                .map(|p| p.params.clone())
                                .unwrap_or_default()
                        } else {
                            ps.param_oids
                                .iter()
                                .map(|&oid| dummy_scalar_for_oid(oid))
                                .collect()
                        };
                        let schema_opt = {
                            let s_guard = session.lock();
                            s_guard
                                .optimize_query_sql(&ps.sql, &desc_params)
                                .ok()
                                .map(|b| b.physical.schema)
                        };
                        match schema_opt {
                            Some(schema) if !schema.is_empty() => {
                                write_backend_message(
                                    &mut writer,
                                    &schema_to_row_description(&schema),
                                )
                                .await?;
                            }
                            _ => {
                                write_backend_message(&mut writer, &BackendMessage::NoData).await?;
                            }
                        }
                    }
                    FrontendMessage::Execute {
                        portal_name,
                        max_rows,
                    } => {
                        let Some(portal) = portals.get_mut(&portal_name) else {
                            write_backend_message(
                                &mut writer,
                                &BackendMessage::ErrorResponse {
                                    sqlstate: "34000".into(),
                                    message: format!("portal '{portal_name}' does not exist"),
                                },
                            )
                            .await?;
                            continue;
                        };
                        if portal.cached.is_none() {
                            let Some(ps) = stmts.get(&portal.stmt_name) else {
                                write_backend_message(
                                    &mut writer,
                                    &BackendMessage::ErrorResponse {
                                        sqlstate: "26000".into(),
                                        message: "statement closed".into(),
                                    },
                                )
                                .await?;
                                continue;
                            };
                            cancel_flag.store(false, Ordering::Release);
                            let res = execute_compat_or_sql(
                                Arc::clone(&self.db),
                                Arc::clone(&session),
                                ps.sql.clone(),
                                portal.params.clone(),
                                Arc::clone(&cancel_flag),
                            )
                            .await;
                            match res {
                                Ok(qr) => {
                                    let rows = qr.to_scalar_rows();
                                    portal.cached = Some((qr.schema, rows, 0, qr.command_tag));
                                }
                                Err(e) => {
                                    write_backend_message(
                                        &mut writer,
                                        &BackendMessage::ErrorResponse {
                                            sqlstate: e.sqlstate().to_string(),
                                            message: e.to_string(),
                                        },
                                    )
                                    .await?;
                                    continue;
                                }
                            }
                        }

                        let (_schema, rows, cursor, tag) = portal.cached.as_mut().unwrap();
                        let take = if max_rows <= 0 {
                            rows.len().saturating_sub(*cursor)
                        } else {
                            (max_rows as usize).min(rows.len().saturating_sub(*cursor))
                        };
                        let mut out_buf = Vec::with_capacity(take * 64 + 32);
                        for row in &rows[*cursor..*cursor + take] {
                            let fields: Vec<Option<String>> =
                                row.iter().map(format_pg_scalar).collect();
                            encode_backend_message(&BackendMessage::DataRow(fields), &mut out_buf);
                        }
                        *cursor += take;

                        if max_rows > 0 && *cursor < rows.len() {
                            encode_backend_message(&BackendMessage::PortalSuspended, &mut out_buf);
                        } else {
                            encode_backend_message(
                                &BackendMessage::CommandComplete(tag.clone()),
                                &mut out_buf,
                            );
                        }
                        writer.write_all(&out_buf).await?;
                    }
                    FrontendMessage::Close { kind, name } => {
                        if kind == b'S' {
                            stmts.remove(&name);
                        } else {
                            portals.remove(&name);
                        }
                        write_backend_message(&mut writer, &BackendMessage::CloseComplete).await?;
                    }
                    FrontendMessage::Flush => {
                        writer.flush().await?;
                    }
                    FrontendMessage::Sync => {
                        let st = txn_status_byte(&session);
                        write_backend_message(&mut writer, &BackendMessage::ReadyForQuery(st))
                            .await?;
                        writer.flush().await?;
                    }
                }
            }
            Ok(())
        }
        .await;

        self.active_cancels.lock().remove(&(pid, secret_key));
        loop_res
    }
}

fn txn_status_byte(session: &Arc<Mutex<Session>>) -> u8 {
    if session.lock().in_transaction() {
        b'T'
    } else {
        b'I'
    }
}

fn dummy_scalar_for_oid(oid: i32) -> ScalarValue {
    match oid {
        crate::pgwire::codec::PG_OID_BOOL => ScalarValue::Boolean(false),
        crate::pgwire::codec::PG_OID_INT2
        | crate::pgwire::codec::PG_OID_INT4
        | crate::pgwire::codec::PG_OID_INT8 => ScalarValue::Int64(0),
        crate::pgwire::codec::PG_OID_FLOAT8 => ScalarValue::Float64(0.0),
        crate::pgwire::codec::PG_OID_DATE => ScalarValue::Date32(0),
        crate::pgwire::codec::PG_OID_TIMESTAMP => ScalarValue::TimestampMicros(0),
        _ => ScalarValue::Utf8(String::new()),
    }
}

async fn write_query_result<W: tokio::io::AsyncWrite + Unpin>(
    writer: &mut W,
    qr: &QueryResult,
) -> Result<()> {
    let mut buf = Vec::with_capacity(512);
    if !qr.schema.is_empty() {
        encode_backend_message(&schema_to_row_description(&qr.schema), &mut buf);
        for chunk in &qr.chunks {
            for r in 0..chunk.len() {
                let row = chunk.get_row(r);
                let fields: Vec<Option<String>> = row.iter().map(format_pg_scalar).collect();
                encode_backend_message(&BackendMessage::DataRow(fields), &mut buf);
            }
            if buf.len() >= 64 * 1024 {
                writer.write_all(&buf).await?;
                buf.clear();
            }
        }
    }
    encode_backend_message(
        &BackendMessage::CommandComplete(qr.command_tag.clone()),
        &mut buf,
    );
    writer.write_all(&buf).await?;
    Ok(())
}

fn is_pg_compat_query(sql: &str) -> bool {
    let low = sql.trim().trim_end_matches(';').to_ascii_lowercase();
    low == "select version()"
        || low == "select current_database()"
        || low == "select current_schema()"
        || low.contains("information_schema.tables")
        || low.contains("pg_catalog.pg_tables")
}

async fn execute_compat_or_sql(
    db: Arc<Database>,
    session: Arc<Mutex<Session>>,
    sql: String,
    params: Vec<ScalarValue>,
    cancelled: Arc<AtomicBool>,
) -> Result<QueryResult> {
    let low = sql.trim().trim_end_matches(';').to_ascii_lowercase();
    let pool = BoundedMemoryPool::unbounded().as_dyn();

    if low == "select version()" {
        let schema = Schema::new(vec![Field::new("version", LogicalType::Utf8, false)]);
        let ch = chunk_from_scalar_rows(
            &[LogicalType::Utf8],
            &[vec![ScalarValue::Utf8(
                "PostgreSQL 16.0 (VectraDB 0.1.0 Vectorized Columnar OLAP Engine)".into(),
            )]],
            &pool,
        )?;
        return Ok(QueryResult {
            schema,
            chunks: vec![ch],
            affected_rows: None,
            command_tag: "SELECT 1".into(),
            metrics: None,
            bundle: None,
        });
    }
    if low == "select current_database()" {
        let schema = Schema::new(vec![Field::new(
            "current_database",
            LogicalType::Utf8,
            false,
        )]);
        let ch = chunk_from_scalar_rows(
            &[LogicalType::Utf8],
            &[vec![ScalarValue::Utf8("vectradb".into())]],
            &pool,
        )?;
        return Ok(QueryResult {
            schema,
            chunks: vec![ch],
            affected_rows: None,
            command_tag: "SELECT 1".into(),
            metrics: None,
            bundle: None,
        });
    }
    if low == "select current_schema()" {
        let schema = Schema::new(vec![Field::new("current_schema", LogicalType::Utf8, false)]);
        let ch = chunk_from_scalar_rows(
            &[LogicalType::Utf8],
            &[vec![ScalarValue::Utf8("public".into())]],
            &pool,
        )?;
        return Ok(QueryResult {
            schema,
            chunks: vec![ch],
            affected_rows: None,
            command_tag: "SELECT 1".into(),
            metrics: None,
            bundle: None,
        });
    }
    if low.contains("information_schema.tables") || low.contains("pg_catalog.pg_tables") {
        let tables = db.catalog().list_tables();
        let schema = Schema::new(vec![
            Field::new("table_schema", LogicalType::Utf8, false),
            Field::new("table_name", LogicalType::Utf8, false),
            Field::new("table_type", LogicalType::Utf8, false),
        ]);
        let rows: Vec<Vec<ScalarValue>> = tables
            .into_iter()
            .map(|t| {
                vec![
                    ScalarValue::Utf8("public".into()),
                    ScalarValue::Utf8(t),
                    ScalarValue::Utf8("BASE TABLE".into()),
                ]
            })
            .collect();
        let count = rows.len();
        let chunks = if rows.is_empty() {
            Vec::new()
        } else {
            vec![chunk_from_scalar_rows(
                &schema.logical_types(),
                &rows,
                &pool,
            )?]
        };
        return Ok(QueryResult {
            schema,
            chunks,
            affected_rows: None,
            command_tag: format!("SELECT {count}"),
            metrics: None,
            bundle: None,
        });
    }

    tokio::task::spawn_blocking(move || {
        let mut guard = session.lock();
        guard.execute_sql_with_params_and_cancel(&sql, &params, cancelled)
    })
    .await
    .map_err(|e| vectra_types::VectraError::Internal(format!("worker join error: {e}")))?
}

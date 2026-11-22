use crate::pgwire::codec::{CANCEL_REQUEST_CODE, PROTOCOL_VERSION_3_0, SSL_REQUEST_CODE};
use std::net::SocketAddr;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use vectra_types::{Result, VectraError};

#[derive(Debug, Clone)]
pub struct SimpleQueryOutput {
    pub columns: Vec<(String, i32)>,
    pub rows: Vec<Vec<Option<String>>>,
    pub command_tag: String,
    pub ready_status: u8,
    pub suspended: bool,
}

#[derive(Debug, Clone)]
pub struct PgWireError {
    pub sqlstate: String,
    pub message: String,
    pub ready_status: u8,
}

/// Async PostgreSQL v3 binary wire protocol test & driver client speaking raw TCP packets.
pub struct PgWireClient {
    stream: TcpStream,
    pub pid: i32,
    pub secret_key: i32,
    pub server_params: Vec<(String, String)>,
}

impl PgWireClient {
    pub async fn connect(
        addr: SocketAddr,
        user: &str,
        database: &str,
        try_ssl: bool,
    ) -> Result<Self> {
        let mut stream = TcpStream::connect(addr).await?;
        let _ = stream.set_nodelay(true);

        if try_ssl {
            let mut ssl = Vec::with_capacity(8);
            ssl.extend_from_slice(&8i32.to_be_bytes());
            ssl.extend_from_slice(&SSL_REQUEST_CODE.to_be_bytes());
            stream.write_all(&ssl).await?;
            let mut resp = [0u8; 1];
            stream.read_exact(&mut resp).await?;
            if resp[0] != b'N' {
                return Err(VectraError::CorruptFormat(format!(
                    "expected SSL 'N', got {}",
                    resp[0] as char
                )));
            }
        }

        // Send StartupMessage
        let mut body = Vec::new();
        body.extend_from_slice(&PROTOCOL_VERSION_3_0.to_be_bytes());
        body.extend_from_slice(b"user\0");
        body.extend_from_slice(user.as_bytes());
        body.push(0);
        body.extend_from_slice(b"database\0");
        body.extend_from_slice(database.as_bytes());
        body.push(0);
        body.push(0);

        let mut pkt = Vec::with_capacity(4 + body.len());
        pkt.extend_from_slice(&((body.len() + 4) as i32).to_be_bytes());
        pkt.extend_from_slice(&body);
        stream.write_all(&pkt).await?;

        let mut pid = 0i32;
        let mut secret_key = 0i32;
        let mut server_params = Vec::new();

        loop {
            let (tag, payload) = read_backend_frame(&mut stream).await?;
            match tag {
                b'R' => {}
                b'S' => {
                    let parts: Vec<&[u8]> = payload.split(|&b| b == 0).collect();
                    if parts.len() >= 2 {
                        server_params.push((
                            String::from_utf8_lossy(parts[0]).into_owned(),
                            String::from_utf8_lossy(parts[1]).into_owned(),
                        ));
                    }
                }
                b'K' => {
                    pid = i32::from_be_bytes(payload[0..4].try_into().unwrap());
                    secret_key = i32::from_be_bytes(payload[4..8].try_into().unwrap());
                }
                b'Z' => break,
                b'E' => {
                    let (code, msg) = parse_error_fields(&payload);
                    return Err(VectraError::ExecutionError(format!(
                        "startup error [{code}]: {msg}"
                    )));
                }
                _ => {}
            }
        }

        Ok(Self {
            stream,
            pid,
            secret_key,
            server_params,
        })
    }

    /// Sends an out-of-band PostgreSQL v3 `CancelRequest` over a fresh TCP connection!
    pub async fn send_cancel_request(addr: SocketAddr, pid: i32, secret_key: i32) -> Result<()> {
        let mut s = TcpStream::connect(addr).await?;
        let mut pkt = Vec::with_capacity(16);
        pkt.extend_from_slice(&16i32.to_be_bytes());
        pkt.extend_from_slice(&CANCEL_REQUEST_CODE.to_be_bytes());
        pkt.extend_from_slice(&pid.to_be_bytes());
        pkt.extend_from_slice(&secret_key.to_be_bytes());
        s.write_all(&pkt).await?;
        s.flush().await?;
        Ok(())
    }

    /// Executes a Simple Query (`'Q'`) returning either `Ok(SimpleQueryOutput)` or `Err(PgWireError)`.
    pub async fn simple_query(
        &mut self,
        sql: &str,
    ) -> std::result::Result<SimpleQueryOutput, PgWireError> {
        let mut pkt = Vec::with_capacity(6 + sql.len());
        pkt.push(b'Q');
        pkt.extend_from_slice(&((sql.len() + 5) as i32).to_be_bytes());
        pkt.extend_from_slice(sql.as_bytes());
        pkt.push(0);
        self.stream.write_all(&pkt).await.map_err(|e| PgWireError {
            sqlstate: "08006".into(),
            message: e.to_string(),
            ready_status: b'I',
        })?;

        self.read_until_ready().await
    }

    /// Executes an Extended Query pipeline: `Parse -> Bind -> Describe(P) -> Execute(max_rows) -> Sync`.
    pub async fn extended_parse_bind_execute(
        &mut self,
        stmt_name: &str,
        portal_name: &str,
        sql: &str,
        param_oids: &[i32],
        params: &[Option<&str>],
        max_rows: i32,
    ) -> std::result::Result<SimpleQueryOutput, PgWireError> {
        let mut buf = Vec::new();

        // 'P' Parse
        let mut p = Vec::new();
        p.extend_from_slice(stmt_name.as_bytes());
        p.push(0);
        p.extend_from_slice(sql.as_bytes());
        p.push(0);
        p.extend_from_slice(&(param_oids.len() as i16).to_be_bytes());
        for &oid in param_oids {
            p.extend_from_slice(&oid.to_be_bytes());
        }
        push_frame(b'P', &p, &mut buf);

        // 'B' Bind
        let mut b = Vec::new();
        b.extend_from_slice(portal_name.as_bytes());
        b.push(0);
        b.extend_from_slice(stmt_name.as_bytes());
        b.push(0);
        b.extend_from_slice(&0i16.to_be_bytes()); // 0 param formats (all text)
        b.extend_from_slice(&(params.len() as i16).to_be_bytes());
        for pv in params {
            match pv {
                None => b.extend_from_slice(&(-1i32).to_be_bytes()),
                Some(s) => {
                    b.extend_from_slice(&(s.len() as i32).to_be_bytes());
                    b.extend_from_slice(s.as_bytes());
                }
            }
        }
        b.extend_from_slice(&0i16.to_be_bytes()); // 0 result formats (all text)
        push_frame(b'B', &b, &mut buf);

        // 'D' Describe Portal
        let mut d = Vec::new();
        d.push(b'P');
        d.extend_from_slice(portal_name.as_bytes());
        d.push(0);
        push_frame(b'D', &d, &mut buf);

        // 'E' Execute
        let mut e = Vec::new();
        e.extend_from_slice(portal_name.as_bytes());
        e.push(0);
        e.extend_from_slice(&max_rows.to_be_bytes());
        push_frame(b'E', &e, &mut buf);

        // 'S' Sync
        push_frame(b'S', &[], &mut buf);

        self.stream.write_all(&buf).await.map_err(|e| PgWireError {
            sqlstate: "08006".into(),
            message: e.to_string(),
            ready_status: b'I',
        })?;
        self.read_until_ready().await
    }

    /// Resumes a suspended portal with another `Execute(max_rows) + Sync`.
    pub async fn execute_portal(
        &mut self,
        portal_name: &str,
        max_rows: i32,
    ) -> std::result::Result<SimpleQueryOutput, PgWireError> {
        let mut buf = Vec::new();
        let mut e = Vec::new();
        e.extend_from_slice(portal_name.as_bytes());
        e.push(0);
        e.extend_from_slice(&max_rows.to_be_bytes());
        push_frame(b'E', &e, &mut buf);
        push_frame(b'S', &[], &mut buf);
        self.stream.write_all(&buf).await.map_err(|e| PgWireError {
            sqlstate: "08006".into(),
            message: e.to_string(),
            ready_status: b'I',
        })?;
        self.read_until_ready().await
    }

    pub async fn terminate(mut self) -> Result<()> {
        let mut buf = Vec::new();
        push_frame(b'X', &[], &mut buf);
        let _ = self.stream.write_all(&buf).await;
        Ok(())
    }

    async fn read_until_ready(&mut self) -> std::result::Result<SimpleQueryOutput, PgWireError> {
        let mut columns = Vec::new();
        let mut rows = Vec::new();
        let mut command_tag = String::new();
        let mut suspended = false;
        let mut err_opt: Option<(String, String)> = None;

        loop {
            let (tag, payload) =
                read_backend_frame(&mut self.stream)
                    .await
                    .map_err(|e| PgWireError {
                        sqlstate: "08006".into(),
                        message: e.to_string(),
                        ready_status: b'I',
                    })?;
            match tag {
                b'T' => {
                    columns = parse_row_description(&payload);
                }
                b'D' => {
                    rows.push(parse_data_row(&payload));
                }
                b'C' => {
                    command_tag =
                        String::from_utf8_lossy(payload.strip_suffix(&[0]).unwrap_or(&payload))
                            .into_owned();
                }
                b's' => {
                    suspended = true;
                }
                b'E' => {
                    err_opt = Some(parse_error_fields(&payload));
                }
                b'Z' => {
                    let ready = payload.first().copied().unwrap_or(b'I');
                    if let Some((sqlstate, message)) = err_opt {
                        return Err(PgWireError {
                            sqlstate,
                            message,
                            ready_status: ready,
                        });
                    }
                    return Ok(SimpleQueryOutput {
                        columns,
                        rows,
                        command_tag,
                        ready_status: ready,
                        suspended,
                    });
                }
                _ => {}
            }
        }
    }
}

fn push_frame(tag: u8, payload: &[u8], out: &mut Vec<u8>) {
    out.push(tag);
    out.extend_from_slice(&((payload.len() + 4) as i32).to_be_bytes());
    out.extend_from_slice(payload);
}

async fn read_backend_frame(stream: &mut TcpStream) -> Result<(u8, Vec<u8>)> {
    let mut hdr = [0u8; 5];
    stream.read_exact(&mut hdr).await?;
    let tag = hdr[0];
    let len = i32::from_be_bytes([hdr[1], hdr[2], hdr[3], hdr[4]]) as usize;
    let mut payload = vec![0u8; len.saturating_sub(4)];
    if !payload.is_empty() {
        stream.read_exact(&mut payload).await?;
    }
    Ok((tag, payload))
}

fn parse_row_description(payload: &[u8]) -> Vec<(String, i32)> {
    if payload.len() < 2 {
        return Vec::new();
    }
    let ncols = i16::from_be_bytes([payload[0], payload[1]]) as usize;
    let mut pos = 2usize;
    let mut cols = Vec::with_capacity(ncols);
    for _ in 0..ncols {
        let start = pos;
        while pos < payload.len() && payload[pos] != 0 {
            pos += 1;
        }
        let name = String::from_utf8_lossy(&payload[start..pos]).into_owned();
        pos += 1; // skip NUL
                  // table_oid(4) + attr(2) + type_oid(4) + typlen(2) + typmod(4) + fmt(2) = 18
        if pos + 18 <= payload.len() {
            let oid = i32::from_be_bytes(payload[pos + 6..pos + 10].try_into().unwrap());
            cols.push((name, oid));
            pos += 18;
        }
    }
    cols
}

fn parse_data_row(payload: &[u8]) -> Vec<Option<String>> {
    if payload.len() < 2 {
        return Vec::new();
    }
    let ncols = i16::from_be_bytes([payload[0], payload[1]]) as usize;
    let mut pos = 2usize;
    let mut out = Vec::with_capacity(ncols);
    for _ in 0..ncols {
        if pos + 4 > payload.len() {
            break;
        }
        let len = i32::from_be_bytes(payload[pos..pos + 4].try_into().unwrap());
        pos += 4;
        if len < 0 {
            out.push(None);
        } else {
            let ul = len as usize;
            let s = String::from_utf8_lossy(&payload[pos..pos + ul]).into_owned();
            pos += ul;
            out.push(Some(s));
        }
    }
    out
}

fn parse_error_fields(payload: &[u8]) -> (String, String) {
    let mut code = String::new();
    let mut msg = String::new();
    let mut pos = 0usize;
    while pos < payload.len() && payload[pos] != 0 {
        let field_type = payload[pos];
        pos += 1;
        let start = pos;
        while pos < payload.len() && payload[pos] != 0 {
            pos += 1;
        }
        let val = String::from_utf8_lossy(&payload[start..pos]).into_owned();
        pos += 1;
        match field_type {
            b'C' => code = val,
            b'M' => msg = val,
            _ => {}
        }
    }
    (code, msg)
}

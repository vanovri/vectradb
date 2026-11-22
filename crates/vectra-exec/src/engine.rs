use crate::context::{ExecutionConfig, ExecutionContext};
use crate::executor::{collect_chunks, execute_physical_plan};
use crate::metrics::{format_explain_analyze, ExecutionMetricsSnapshot, WallTimer};
use crate::spill::SpillManager;
use crate::stream::{chunk_from_scalar_rows, ChunkStream};
use parking_lot::RwLock;
use std::collections::HashMap;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use vectra_expr::{cast_scalar, eval_expr_scalar_reference, PhysicalExpr};
use vectra_optimizer::{OptimizedBundle, Optimizer, StatsCatalog};
use vectra_sql::{
    parse_sql, Binder, BoundCol, ColId, ColIdGen, CopyFormat, ExplainFormat, LogicalExpr,
    LogicalPlan, LogicalStatement,
};
use vectra_storage::{
    load_csv_chunks, read_parquet_bytes, write_parquet_bytes, Catalog, CsvOptions,
    ParquetCompression, Snapshot,
};
use vectra_types::{
    BoundedMemoryPool, DataChunk, Field, LogicalType, MemoryPool, Result, ScalarValue, Schema,
    VectraError, VECTOR_CAPACITY,
};

/// Result returned from executing a SQL statement through `Session`.
#[derive(Debug)]
pub struct QueryResult {
    pub schema: Schema,
    pub chunks: Vec<DataChunk>,
    pub affected_rows: Option<u64>,
    pub command_tag: String,
    pub metrics: Option<ExecutionMetricsSnapshot>,
    pub bundle: Option<OptimizedBundle>,
}

impl QueryResult {
    pub fn total_rows(&self) -> usize {
        self.chunks.iter().map(DataChunk::len).sum()
    }

    pub fn to_scalar_rows(&self) -> Vec<Vec<ScalarValue>> {
        let mut out = Vec::with_capacity(self.total_rows());
        for ch in &self.chunks {
            for r in 0..ch.len() {
                out.push(ch.get_row(r));
            }
        }
        out
    }
}

/// Thread-safe VectraDB database instance holding the MVCC `Catalog`, optimizer `StatsCatalog`,
/// SQL views, and default execution configuration.
pub struct Database {
    catalog: Arc<Catalog>,
    stats: StatsCatalog,
    views: RwLock<HashMap<String, String>>,
    default_config: ExecutionConfig,
}

impl Default for Database {
    fn default() -> Self {
        Self::new()
    }
}

impl Database {
    pub fn new() -> Self {
        Self::with_config(ExecutionConfig::default())
    }

    pub fn with_config(default_config: ExecutionConfig) -> Self {
        Self {
            catalog: Arc::new(Catalog::new()),
            stats: StatsCatalog::new(),
            views: RwLock::new(HashMap::new()),
            default_config,
        }
    }

    pub fn from_catalog(catalog: Catalog, default_config: ExecutionConfig) -> Self {
        Self {
            catalog: Arc::new(catalog),
            stats: StatsCatalog::new(),
            views: RwLock::new(HashMap::new()),
            default_config,
        }
    }

    pub fn catalog(&self) -> &Arc<Catalog> {
        &self.catalog
    }

    pub fn stats(&self) -> &StatsCatalog {
        &self.stats
    }

    pub fn views_snapshot(&self) -> HashMap<String, String> {
        self.views.read().clone()
    }

    pub fn default_config(&self) -> &ExecutionConfig {
        &self.default_config
    }

    /// Recomputes HyperLogLog & 64-bucket Equi-Height Histogram stats for one or all tables.
    pub fn analyze_table(&self, table_name: Option<&str>) -> Result<usize> {
        let snap = self.catalog.begin_txn();
        let pool = BoundedMemoryPool::unbounded().as_dyn();
        let targets = match table_name {
            Some(t) => vec![t.to_string()],
            None => self.catalog.list_tables(),
        };
        let mut analyzed = 0usize;
        for tname in targets {
            let table = self.catalog.get_table(&tname)?;
            self.stats
                .analyze_table(&table, self.catalog.transaction_manager(), snap, &pool)?;
            analyzed += 1;
        }
        let _ = self.catalog.commit_txn(snap.txn_id);
        Ok(analyzed)
    }

    /// Opens a new isolated client session (with its own explicit MVCC txn & config overrides).
    pub fn new_session(self: &Arc<Self>) -> Session {
        Session {
            db: Arc::clone(self),
            config: self.default_config.clone(),
            active_txn: None,
            session_vars: default_pg_session_vars(),
        }
    }
}

fn default_pg_session_vars() -> HashMap<String, String> {
    let mut m = HashMap::new();
    m.insert("server_version".into(), "16.0 (VectraDB 0.1.0)".into());
    m.insert("client_encoding".into(), "UTF8".into());
    m.insert("server_encoding".into(), "UTF8".into());
    m.insert("standard_conforming_strings".into(), "on".into());
    m.insert("timezone".into(), "UTC".into());
    m.insert("datestyle".into(), "ISO, MDY".into());
    m.insert("search_path".into(), "public".into());
    m.insert("transaction_isolation".into(), "repeatable read".into());
    m
}

/// Interactive or server SQL session supporting explicit MVCC transactions, parameterized queries,
/// `EXPLAIN ANALYZE`, memory limits, and streaming query execution.
pub struct Session {
    db: Arc<Database>,
    config: ExecutionConfig,
    active_txn: Option<Snapshot>,
    session_vars: HashMap<String, String>,
}

impl Session {
    pub fn database(&self) -> &Arc<Database> {
        &self.db
    }

    pub fn config(&self) -> &ExecutionConfig {
        &self.config
    }

    pub fn config_mut(&mut self) -> &mut ExecutionConfig {
        &mut self.config
    }

    pub fn in_transaction(&self) -> bool {
        self.active_txn.is_some()
    }

    pub fn set_memory_limit(&mut self, bytes: usize) {
        self.config.memory_limit_bytes = bytes;
    }

    pub fn set_threads(&mut self, threads: usize) {
        self.config.threads = threads.max(1);
    }

    /// Executes one or more semicolon-separated SQL statements, returning the final statement's result.
    pub fn execute_sql(&mut self, sql: &str) -> Result<QueryResult> {
        self.execute_sql_with_params_and_cancel(sql, &[], Arc::new(AtomicBool::new(false)))
    }

    /// Executes SQL with positional `$1..$n` parameters.
    pub fn execute_sql_params(&mut self, sql: &str, params: &[ScalarValue]) -> Result<QueryResult> {
        self.execute_sql_with_params_and_cancel(sql, params, Arc::new(AtomicBool::new(false)))
    }

    /// Executes SQL with `$1..$n` parameters and an external cooperative cancellation flag.
    pub fn execute_sql_with_params_and_cancel(
        &mut self,
        sql: &str,
        params: &[ScalarValue],
        cancelled: Arc<AtomicBool>,
    ) -> Result<QueryResult> {
        let stmts = parse_sql(sql)?;
        if stmts.is_empty() {
            return Ok(QueryResult {
                schema: Schema::empty(),
                chunks: Vec::new(),
                affected_rows: None,
                command_tag: String::new(),
                metrics: None,
                bundle: None,
            });
        }
        let mut last = None;
        for stmt in stmts {
            last = Some(self.execute_parsed_statement(&stmt, params, Arc::clone(&cancelled))?);
        }
        Ok(last.unwrap())
    }

    /// Compiles and opens a streaming `ChunkStream` for a `SELECT` query using a caller-supplied
    /// `BoundedMemoryPool` and `SpillManager` (ideal for 32 MB spill torture tests and PG wire streaming!).
    pub fn execute_query_stream_with_pool(
        &mut self,
        sql: &str,
        params: &[ScalarValue],
        pool: Arc<dyn MemoryPool>,
        spill: Arc<SpillManager>,
        cancelled: Arc<AtomicBool>,
    ) -> Result<(Schema, ChunkStream, ExecutionContext, OptimizedBundle)> {
        let stmts = parse_sql(sql)?;
        if stmts.len() != 1 {
            return Err(VectraError::ExecutionError(
                "streaming execution requires exactly one SELECT statement".into(),
            ));
        }
        let views = self.db.views.read().clone();
        let mut binder = Binder::with_params(&self.db.catalog, &views, params);
        let bound = binder.bind_statement(&stmts[0])?;
        let mut col_gen = ColIdGen::new();
        col_gen.set_min_next(binder.next_col_id());

        let LogicalStatement::Query(lp) = bound else {
            return Err(VectraError::ExecutionError(
                "streaming execution only supports SELECT queries".into(),
            ));
        };

        let bundle = Optimizer::new(&self.db.stats).optimize_full(*lp, &mut col_gen)?;

        let (snap, auto_txn) = match self.active_txn {
            Some(s) => (s, false),
            None => (self.db.catalog.begin_txn(), true),
        };
        if auto_txn {
            // Read-only snapshot stays valid after closing the empty auto-txn handle.
            let _ = self.db.catalog.commit_txn(snap.txn_id);
        }

        let ctx = ExecutionContext::with_pool(
            Arc::clone(&self.db.catalog),
            snap,
            pool,
            spill,
            self.config.clone(),
        )
        .with_cancel_flag(cancelled);
        ctx.metrics.init_from_plan(&bundle.physical);

        let schema = bundle.physical.schema.clone();
        let stream = execute_physical_plan(&bundle.physical, &ctx)?;
        Ok((schema, stream, ctx, bundle))
    }

    /// Compiles a query into its full 4-stage `OptimizedBundle` (for `EXPLAIN` & M10 Studio).
    pub fn optimize_query_sql(&self, sql: &str, params: &[ScalarValue]) -> Result<OptimizedBundle> {
        let stmts = parse_sql(sql)?;
        if stmts.len() != 1 {
            return Err(VectraError::ExecutionError(
                "expected a single query statement".into(),
            ));
        }
        let views = self.db.views.read().clone();
        let mut binder = Binder::with_params(&self.db.catalog, &views, params);
        let bound = binder.bind_statement(&stmts[0])?;
        let mut col_gen = ColIdGen::new();
        col_gen.set_min_next(binder.next_col_id());
        let LogicalStatement::Query(lp) = bound else {
            return Err(VectraError::ExecutionError(
                "statement is not a SELECT query".into(),
            ));
        };
        Optimizer::new(&self.db.stats).optimize_full(*lp, &mut col_gen)
    }

    fn acquire_txn(&self) -> (Snapshot, bool) {
        match self.active_txn {
            Some(s) => (s, false),
            None => (self.db.catalog.begin_txn(), true),
        }
    }

    fn finish_auto_txn(&self, snap: Snapshot, auto_txn: bool, ok: bool) -> Result<()> {
        if auto_txn {
            if ok {
                self.db.catalog.commit_txn(snap.txn_id)?;
            } else {
                let _ = self.db.catalog.rollback_txn(snap.txn_id);
            }
        }
        Ok(())
    }

    fn execute_parsed_statement(
        &mut self,
        stmt: &vectra_sql::Statement,
        params: &[ScalarValue],
        cancelled: Arc<AtomicBool>,
    ) -> Result<QueryResult> {
        let views = self.db.views.read().clone();
        let mut binder = Binder::with_params(&self.db.catalog, &views, params);
        let bound = binder.bind_statement(stmt)?;
        let mut col_gen = ColIdGen::new();
        col_gen.set_min_next(binder.next_col_id());

        match bound {
            LogicalStatement::Query(lp) => {
                let (snap, auto_txn) = self.acquire_txn();
                let res = self.run_logical_query(*lp, &mut col_gen, snap, cancelled);
                self.finish_auto_txn(snap, auto_txn, res.is_ok())?;
                res
            }
            LogicalStatement::CreateTable {
                name,
                if_not_exists,
                or_replace,
                schema,
                as_plan,
            } => {
                if or_replace {
                    let _ = self.db.catalog.drop_table(&name, true);
                }
                let table = self.db.catalog.create_table(&name, schema, if_not_exists)?;
                let mut inserted = 0u64;
                if let Some(plan) = as_plan {
                    let (snap, auto_txn) = self.acquire_txn();
                    let res = (|| -> Result<u64> {
                        let qr = self.run_logical_query(*plan, &mut col_gen, snap, cancelled)?;
                        let pool = BoundedMemoryPool::new(self.config.memory_limit_bytes).as_dyn();
                        let mut cnt = 0u64;
                        for ch in &qr.chunks {
                            cnt += table.insert_chunk(ch, snap, &pool)? as u64;
                        }
                        Ok(cnt)
                    })();
                    let ok = res.is_ok();
                    self.finish_auto_txn(snap, auto_txn, ok)?;
                    inserted = res?;
                }
                Ok(QueryResult {
                    schema: Schema::empty(),
                    chunks: Vec::new(),
                    affected_rows: Some(inserted),
                    command_tag: "CREATE TABLE".into(),
                    metrics: None,
                    bundle: None,
                })
            }
            LogicalStatement::CreateView {
                name,
                or_replace,
                sql_query,
            } => {
                let key = name.to_ascii_lowercase();
                let mut vg = self.db.views.write();
                if vg.contains_key(&key) && !or_replace {
                    return Err(VectraError::BindError(format!(
                        "view '{name}' already exists"
                    )));
                }
                vg.insert(key, sql_query);
                Ok(QueryResult {
                    schema: Schema::empty(),
                    chunks: Vec::new(),
                    affected_rows: Some(0),
                    command_tag: "CREATE VIEW".into(),
                    metrics: None,
                    bundle: None,
                })
            }
            LogicalStatement::DropTable { name, if_exists } => {
                self.db.catalog.drop_table(&name, if_exists)?;
                Ok(QueryResult {
                    schema: Schema::empty(),
                    chunks: Vec::new(),
                    affected_rows: Some(0),
                    command_tag: "DROP TABLE".into(),
                    metrics: None,
                    bundle: None,
                })
            }
            LogicalStatement::DropView { name, if_exists } => {
                let key = name.to_ascii_lowercase();
                let mut vg = self.db.views.write();
                if vg.remove(&key).is_none() && !if_exists {
                    return Err(VectraError::BindError(format!(
                        "view '{name}' does not exist"
                    )));
                }
                Ok(QueryResult {
                    schema: Schema::empty(),
                    chunks: Vec::new(),
                    affected_rows: Some(0),
                    command_tag: "DROP VIEW".into(),
                    metrics: None,
                    bundle: None,
                })
            }
            LogicalStatement::Insert {
                table_name,
                target_col_indices,
                source,
            } => {
                let table = self.db.catalog.get_table(&table_name)?;
                let (snap, auto_txn) = self.acquire_txn();
                let res = (|| -> Result<u64> {
                    let qr = self.run_logical_query(*source, &mut col_gen, snap, cancelled)?;
                    let pool = BoundedMemoryPool::new(self.config.memory_limit_bytes).as_dyn();
                    let tbl_types = table.schema().logical_types();
                    let mut total = 0u64;

                    for ch in &qr.chunks {
                        let n = ch.len();
                        if n == 0 {
                            continue;
                        }
                        let mut mapped_rows = Vec::with_capacity(n);
                        for r in 0..n {
                            let src_row = ch.get_row(r);
                            let mut dst_row = vec![ScalarValue::Null; tbl_types.len()];
                            for (src_i, &dst_i) in target_col_indices.iter().enumerate() {
                                let v = &src_row[src_i];
                                dst_row[dst_i] =
                                    if v.is_null() || v.logical_type() == tbl_types[dst_i] {
                                        v.clone()
                                    } else {
                                        cast_scalar(v, tbl_types[dst_i], false)?
                                    };
                            }
                            mapped_rows.push(dst_row);
                        }
                        let mapped_chunk = chunk_from_scalar_rows(&tbl_types, &mapped_rows, &pool)?;
                        total += table.insert_chunk(&mapped_chunk, snap, &pool)? as u64;
                    }
                    Ok(total)
                })();
                let ok = res.is_ok();
                self.finish_auto_txn(snap, auto_txn, ok)?;
                let inserted = res?;
                Ok(QueryResult {
                    schema: Schema::empty(),
                    chunks: Vec::new(),
                    affected_rows: Some(inserted),
                    command_tag: format!("INSERT 0 {inserted}"),
                    metrics: None,
                    bundle: None,
                })
            }
            LogicalStatement::Delete {
                table_name,
                filter,
                table_cols,
            } => {
                let table = self.db.catalog.get_table(&table_name)?;
                let phys_filter = match &filter {
                    Some(f) => Some(compile_table_expr(f, &table_cols)?),
                    None => None,
                };
                let (snap, auto_txn) = self.acquire_txn();
                let pool = BoundedMemoryPool::new(self.config.memory_limit_bytes).as_dyn();
                let res = table.delete_where(
                    |row| match &phys_filter {
                        None => Ok(true),
                        Some(p) => Ok(matches!(
                            eval_expr_scalar_reference(p, row)?,
                            ScalarValue::Boolean(true)
                        )),
                    },
                    self.db.catalog.transaction_manager(),
                    snap,
                    &pool,
                );
                let ok = res.is_ok();
                self.finish_auto_txn(snap, auto_txn, ok)?;
                let deleted = res? as u64;
                Ok(QueryResult {
                    schema: Schema::empty(),
                    chunks: Vec::new(),
                    affected_rows: Some(deleted),
                    command_tag: format!("DELETE {deleted}"),
                    metrics: None,
                    bundle: None,
                })
            }
            LogicalStatement::Update {
                table_name,
                assignments,
                filter,
                table_cols,
            } => {
                let table = self.db.catalog.get_table(&table_name)?;
                let phys_filter = match &filter {
                    Some(f) => Some(compile_table_expr(f, &table_cols)?),
                    None => None,
                };
                let mut phys_assigns = Vec::with_capacity(assignments.len());
                for (col_idx, expr) in &assignments {
                    phys_assigns.push((*col_idx, compile_table_expr(expr, &table_cols)?));
                }
                let tbl_types = table.schema().logical_types();
                let (snap, auto_txn) = self.acquire_txn();
                let pool = BoundedMemoryPool::new(self.config.memory_limit_bytes).as_dyn();

                let res = (|| -> Result<u64> {
                    let mut updated_rows: Vec<Vec<ScalarValue>> = Vec::new();
                    let deleted = table.delete_where(
                        |row| {
                            let matches = match &phys_filter {
                                None => true,
                                Some(p) => matches!(
                                    eval_expr_scalar_reference(p, row)?,
                                    ScalarValue::Boolean(true)
                                ),
                            };
                            if matches {
                                let mut new_row = row.to_vec();
                                for (col_idx, expr) in &phys_assigns {
                                    let val = eval_expr_scalar_reference(expr, row)?;
                                    new_row[*col_idx] = if val.is_null()
                                        || val.logical_type() == tbl_types[*col_idx]
                                    {
                                        val
                                    } else {
                                        cast_scalar(&val, tbl_types[*col_idx], false)?
                                    };
                                }
                                updated_rows.push(new_row);
                            }
                            Ok(matches)
                        },
                        self.db.catalog.transaction_manager(),
                        snap,
                        &pool,
                    )?;

                    let mut off = 0;
                    while off < updated_rows.len() {
                        let take = (updated_rows.len() - off).min(VECTOR_CAPACITY);
                        let ch = chunk_from_scalar_rows(
                            &tbl_types,
                            &updated_rows[off..off + take],
                            &pool,
                        )?;
                        table.insert_chunk(&ch, snap, &pool)?;
                        off += take;
                    }
                    Ok(deleted as u64)
                })();
                let ok = res.is_ok();
                self.finish_auto_txn(snap, auto_txn, ok)?;
                let updated = res?;
                Ok(QueryResult {
                    schema: Schema::empty(),
                    chunks: Vec::new(),
                    affected_rows: Some(updated),
                    command_tag: format!("UPDATE {updated}"),
                    metrics: None,
                    bundle: None,
                })
            }
            LogicalStatement::Copy {
                table_name,
                query,
                path,
                to_file,
                format,
                delimiter,
                header,
            } => {
                #[cfg(target_arch = "wasm32")]
                {
                    let _ = (table_name, query, path, to_file, format, delimiter, header);
                    Err(VectraError::ExecutionError(
                        "COPY TO/FROM file path is not supported on wasm32".into(),
                    ))
                }
                #[cfg(not(target_arch = "wasm32"))]
                {
                    self.execute_copy(
                        table_name,
                        query.map(|q| *q),
                        &path,
                        to_file,
                        format,
                        delimiter,
                        header,
                        &mut col_gen,
                        cancelled,
                    )
                }
            }
            LogicalStatement::Analyze { table_name } => {
                let n = self.db.analyze_table(table_name.as_deref())?;
                Ok(QueryResult {
                    schema: Schema::empty(),
                    chunks: Vec::new(),
                    affected_rows: Some(n as u64),
                    command_tag: "ANALYZE".into(),
                    metrics: None,
                    bundle: None,
                })
            }
            LogicalStatement::Begin => {
                if self.active_txn.is_some() {
                    return Err(VectraError::ExecutionError(
                        "transaction already in progress".into(),
                    ));
                }
                self.active_txn = Some(self.db.catalog.begin_txn());
                Ok(QueryResult {
                    schema: Schema::empty(),
                    chunks: Vec::new(),
                    affected_rows: Some(0),
                    command_tag: "BEGIN".into(),
                    metrics: None,
                    bundle: None,
                })
            }
            LogicalStatement::Commit => {
                let snap = self.active_txn.take().ok_or_else(|| {
                    VectraError::ExecutionError("no transaction in progress to COMMIT".into())
                })?;
                self.db.catalog.commit_txn(snap.txn_id)?;
                Ok(QueryResult {
                    schema: Schema::empty(),
                    chunks: Vec::new(),
                    affected_rows: Some(0),
                    command_tag: "COMMIT".into(),
                    metrics: None,
                    bundle: None,
                })
            }
            LogicalStatement::Rollback => {
                let snap = self.active_txn.take().ok_or_else(|| {
                    VectraError::ExecutionError("no transaction in progress to ROLLBACK".into())
                })?;
                self.db.catalog.rollback_txn(snap.txn_id)?;
                Ok(QueryResult {
                    schema: Schema::empty(),
                    chunks: Vec::new(),
                    affected_rows: Some(0),
                    command_tag: "ROLLBACK".into(),
                    metrics: None,
                    bundle: None,
                })
            }
            LogicalStatement::Explain {
                format,
                bound_plan,
                unnested_plan,
            } => {
                let bundle =
                    Optimizer::new(&self.db.stats).optimize_full(*bound_plan, &mut col_gen)?;
                let mut explain_metrics = None;
                let text = match format {
                    ExplainFormat::Logical => bundle.bound_logical.format_tree(),
                    ExplainFormat::Unnested => unnested_plan.format_tree(),
                    ExplainFormat::Physical => bundle.physical.format_tree(),
                    ExplainFormat::Analyze => {
                        let (snap, auto_txn) = self.acquire_txn();
                        let res = self.execute_bundle(&bundle, snap, cancelled);
                        self.finish_auto_txn(snap, auto_txn, res.is_ok())?;
                        let qr = res?;
                        let m = qr.metrics.unwrap_or_default();
                        let s = format_explain_analyze(&bundle.physical, &m);
                        explain_metrics = Some(m);
                        s
                    }
                    ExplainFormat::Json => serde_json::to_string_pretty(&bundle)
                        .map_err(|e| VectraError::Internal(format!("EXPLAIN JSON error: {e}")))?,
                };
                let schema = Schema::new(vec![Field::new("QUERY PLAN", LogicalType::Utf8, false)]);
                let pool = BoundedMemoryPool::unbounded().as_dyn();
                let rows: Vec<Vec<ScalarValue>> = text
                    .lines()
                    .map(|l| vec![ScalarValue::Utf8(l.to_string())])
                    .collect();
                let chunk = chunk_from_scalar_rows(&[LogicalType::Utf8], &rows, &pool)?;
                Ok(QueryResult {
                    schema,
                    chunks: if chunk.is_empty() {
                        Vec::new()
                    } else {
                        vec![chunk]
                    },
                    affected_rows: None,
                    command_tag: "EXPLAIN".into(),
                    metrics: explain_metrics,
                    bundle: Some(bundle),
                })
            }
            LogicalStatement::Set { variable, value } => {
                let var = variable.to_ascii_lowercase();
                let trimmed = value.trim().trim_matches('\'').to_string();
                match var.as_str() {
                    "memory_limit" => {
                        self.config.memory_limit_bytes = parse_byte_size(&trimmed)?;
                    }
                    "threads" => {
                        let n: usize = trimmed.parse().map_err(|_| {
                            VectraError::ExecutionError(format!(
                                "invalid threads value '{trimmed}'"
                            ))
                        })?;
                        self.config.threads = n.max(1);
                    }
                    "max_recursive_iterations" => {
                        let n: usize = trimmed.parse().map_err(|_| {
                            VectraError::ExecutionError(format!(
                                "invalid max_recursive_iterations '{trimmed}'"
                            ))
                        })?;
                        self.config.max_recursive_iterations = n.max(1);
                    }
                    _ => {}
                }
                self.session_vars.insert(var, trimmed);
                Ok(QueryResult {
                    schema: Schema::empty(),
                    chunks: Vec::new(),
                    affected_rows: Some(0),
                    command_tag: "SET".into(),
                    metrics: None,
                    bundle: None,
                })
            }
            LogicalStatement::Show { variable } => {
                let var = variable.to_ascii_lowercase();
                let val = match var.as_str() {
                    "memory_limit" => {
                        crate::metrics::format_bytes(self.config.memory_limit_bytes as u64)
                    }
                    "threads" => self.config.threads.to_string(),
                    "max_recursive_iterations" => self.config.max_recursive_iterations.to_string(),
                    _ => self.session_vars.get(&var).cloned().unwrap_or_default(),
                };
                let schema = Schema::new(vec![Field::new(variable, LogicalType::Utf8, false)]);
                let pool = BoundedMemoryPool::unbounded().as_dyn();
                let chunk = chunk_from_scalar_rows(
                    &[LogicalType::Utf8],
                    &[vec![ScalarValue::Utf8(val)]],
                    &pool,
                )?;
                Ok(QueryResult {
                    schema,
                    chunks: vec![chunk],
                    affected_rows: None,
                    command_tag: "SHOW".into(),
                    metrics: None,
                    bundle: None,
                })
            }
        }
    }

    fn run_logical_query(
        &self,
        bound_plan: LogicalPlan,
        col_gen: &mut ColIdGen,
        snap: Snapshot,
        cancelled: Arc<AtomicBool>,
    ) -> Result<QueryResult> {
        let bundle = Optimizer::new(&self.db.stats).optimize_full(bound_plan, col_gen)?;
        self.execute_bundle(&bundle, snap, cancelled)
    }

    fn execute_bundle(
        &self,
        bundle: &OptimizedBundle,
        snap: Snapshot,
        cancelled: Arc<AtomicBool>,
    ) -> Result<QueryResult> {
        let total_timer = WallTimer::start();
        let ctx = ExecutionContext::new(Arc::clone(&self.db.catalog), snap, self.config.clone())?
            .with_cancel_flag(cancelled);
        ctx.metrics.init_from_plan(&bundle.physical);
        let stream = execute_physical_plan(&bundle.physical, &ctx)?;
        let chunks = collect_chunks(stream, &ctx)?;
        ctx.metrics
            .set_totals(total_timer.elapsed_nanos(), ctx.pool.peak_bytes());
        let rows: usize = chunks.iter().map(DataChunk::len).sum();
        Ok(QueryResult {
            schema: bundle.physical.schema.clone(),
            chunks,
            affected_rows: None,
            command_tag: format!("SELECT {rows}"),
            metrics: Some(ctx.metrics.snapshot()),
            bundle: Some(bundle.clone()),
        })
    }

    #[cfg(not(target_arch = "wasm32"))]
    #[allow(clippy::too_many_arguments)]
    fn execute_copy(
        &mut self,
        table_name: Option<String>,
        query: Option<LogicalPlan>,
        path: &str,
        to_file: bool,
        format: CopyFormat,
        delimiter: u8,
        header: bool,
        col_gen: &mut ColIdGen,
        cancelled: Arc<AtomicBool>,
    ) -> Result<QueryResult> {
        let (snap, auto_txn) = self.acquire_txn();
        let pool = BoundedMemoryPool::new(self.config.memory_limit_bytes).as_dyn();

        if to_file {
            let res = (|| -> Result<u64> {
                let (schema, chunks) = if let Some(q) = query {
                    let qr = self.run_logical_query(q, col_gen, snap, cancelled)?;
                    (qr.schema, qr.chunks)
                } else if let Some(tname) = table_name {
                    let tbl = self.db.catalog.get_table(&tname)?;
                    let (chunks, _) = tbl.scan(
                        None,
                        None,
                        self.db.catalog.transaction_manager(),
                        snap,
                        &pool,
                    )?;
                    (tbl.schema().clone(), chunks)
                } else {
                    return Err(VectraError::ExecutionError(
                        "COPY TO requires table or query".into(),
                    ));
                };
                let rows: u64 = chunks.iter().map(|c| c.len() as u64).sum();
                match format {
                    CopyFormat::Parquet => {
                        let bytes = write_parquet_bytes(
                            &schema,
                            &chunks,
                            ParquetCompression::Snappy,
                            true,
                        )?;
                        std::fs::write(path, bytes)?;
                    }
                    CopyFormat::Csv => {
                        let mut out = String::new();
                        let delim = delimiter as char;
                        if header {
                            for (i, f) in schema.fields.iter().enumerate() {
                                if i > 0 {
                                    out.push(delim);
                                }
                                out.push_str(&csv_escape(&f.name, delim));
                            }
                            out.push('\n');
                        }
                        for ch in &chunks {
                            for r in 0..ch.len() {
                                let row = ch.get_row(r);
                                for (i, v) in row.iter().enumerate() {
                                    if i > 0 {
                                        out.push(delim);
                                    }
                                    if !v.is_null() {
                                        out.push_str(&csv_escape(&v.to_display_string(), delim));
                                    }
                                }
                                out.push('\n');
                            }
                        }
                        std::fs::write(path, out)?;
                    }
                }
                Ok(rows)
            })();
            let ok = res.is_ok();
            self.finish_auto_txn(snap, auto_txn, ok)?;
            let cnt = res?;
            Ok(QueryResult {
                schema: Schema::empty(),
                chunks: Vec::new(),
                affected_rows: Some(cnt),
                command_tag: format!("COPY {cnt}"),
                metrics: None,
                bundle: None,
            })
        } else {
            let tname = table_name.ok_or_else(|| {
                VectraError::ExecutionError("COPY FROM requires a target table".into())
            })?;
            let tbl = self.db.catalog.get_table(&tname)?;
            let res = (|| -> Result<u64> {
                let bytes = std::fs::read(path)?;
                let chunks = match format {
                    CopyFormat::Parquet => {
                        let (_s, chs, _) = read_parquet_bytes(&bytes, None, None, &pool)?;
                        chs
                    }
                    CopyFormat::Csv => {
                        let text = String::from_utf8(bytes).map_err(|e| {
                            VectraError::ExecutionError(format!("invalid UTF-8 CSV: {e}"))
                        })?;
                        let opts = CsvOptions {
                            delimiter,
                            has_header: header,
                            ..CsvOptions::default()
                        };
                        let (_s, chs) = load_csv_chunks(&text, Some(tbl.schema()), &opts, &pool)?;
                        chs
                    }
                };
                let mut total = 0u64;
                for ch in &chunks {
                    total += tbl.insert_chunk(ch, snap, &pool)? as u64;
                }
                Ok(total)
            })();
            let ok = res.is_ok();
            self.finish_auto_txn(snap, auto_txn, ok)?;
            let cnt = res?;
            Ok(QueryResult {
                schema: Schema::empty(),
                chunks: Vec::new(),
                affected_rows: Some(cnt),
                command_tag: format!("COPY {cnt}"),
                metrics: None,
                bundle: None,
            })
        }
    }
}

#[cfg(not(target_arch = "wasm32"))]
fn csv_escape(s: &str, delim: char) -> String {
    if s.contains(delim) || s.contains('"') || s.contains('\n') || s.contains('\r') {
        format!("\"{}\"", s.replace('"', "\"\""))
    } else {
        s.to_string()
    }
}

/// Parses human-readable byte limits like `32MB`, `512MiB`, `1GB`, `4096KB`, or raw byte integers.
pub fn parse_byte_size(s: &str) -> Result<usize> {
    let up = s.trim().to_ascii_uppercase();
    let (num_str, mult): (&str, u128) =
        if let Some(n) = up.strip_suffix("GIB").or_else(|| up.strip_suffix("GB")) {
            (n.trim(), 1024 * 1024 * 1024)
        } else if let Some(n) = up.strip_suffix("MIB").or_else(|| up.strip_suffix("MB")) {
            (n.trim(), 1024 * 1024)
        } else if let Some(n) = up.strip_suffix("KIB").or_else(|| up.strip_suffix("KB")) {
            (n.trim(), 1024)
        } else if let Some(n) = up.strip_suffix('B') {
            (n.trim(), 1)
        } else {
            (up.as_str(), 1)
        };
    let val: u128 = num_str.parse().map_err(|_| {
        VectraError::ExecutionError(format!("invalid byte size specification '{s}'"))
    })?;
    let bytes = val.saturating_mul(mult).min(usize::MAX as u128) as usize;
    Ok(bytes.max(1024 * 1024))
}

fn compile_table_expr(expr: &LogicalExpr, table_cols: &[BoundCol]) -> Result<PhysicalExpr> {
    let map: HashMap<ColId, usize> = table_cols
        .iter()
        .enumerate()
        .map(|(i, c)| (c.id, i))
        .collect();
    compile_le(expr, &map)
}

fn compile_le(expr: &LogicalExpr, map: &HashMap<ColId, usize>) -> Result<PhysicalExpr> {
    match expr {
        LogicalExpr::Col {
            id,
            name,
            data_type,
        } => {
            let index = *map.get(id).ok_or_else(|| {
                VectraError::Internal(format!("column {id:?} missing in DML table scope"))
            })?;
            Ok(PhysicalExpr::ColumnRef {
                index,
                name: name.clone(),
                data_type: *data_type,
            })
        }
        LogicalExpr::CorrelatedCol { name, .. } => Err(VectraError::ExecutionError(format!(
            "unsupported correlated reference '{name}' in DML expression"
        ))),
        LogicalExpr::Literal { value, data_type } => Ok(PhysicalExpr::Literal {
            value: value.clone(),
            data_type: *data_type,
        }),
        LogicalExpr::BinaryOp {
            op,
            left,
            right,
            return_type,
        } => Ok(PhysicalExpr::BinaryOp {
            op: *op,
            left: Box::new(compile_le(left, map)?),
            right: Box::new(compile_le(right, map)?),
            return_type: *return_type,
        }),
        LogicalExpr::UnaryOp {
            op,
            expr,
            return_type,
        } => Ok(PhysicalExpr::UnaryOp {
            op: *op,
            expr: Box::new(compile_le(expr, map)?),
            return_type: *return_type,
        }),
        LogicalExpr::Cast {
            expr,
            target_type,
            try_cast,
        } => Ok(PhysicalExpr::Cast {
            expr: Box::new(compile_le(expr, map)?),
            target_type: *target_type,
            try_cast: *try_cast,
        }),
        LogicalExpr::CaseWhen {
            branches,
            else_expr,
            return_type,
        } => {
            let mut pb = Vec::with_capacity(branches.len());
            for (c, t) in branches {
                pb.push((compile_le(c, map)?, compile_le(t, map)?));
            }
            let pe = match else_expr {
                Some(e) => Some(Box::new(compile_le(e, map)?)),
                None => None,
            };
            Ok(PhysicalExpr::CaseWhen {
                branches: pb,
                else_expr: pe,
                return_type: *return_type,
            })
        }
        LogicalExpr::Coalesce { args, return_type } => Ok(PhysicalExpr::Coalesce {
            args: args
                .iter()
                .map(|a| compile_le(a, map))
                .collect::<Result<Vec<_>>>()?,
            return_type: *return_type,
        }),
        LogicalExpr::InList {
            expr,
            list,
            negated,
        } => Ok(PhysicalExpr::InList {
            expr: Box::new(compile_le(expr, map)?),
            list: list
                .iter()
                .map(|i| compile_le(i, map))
                .collect::<Result<Vec<_>>>()?,
            negated: *negated,
        }),
        LogicalExpr::Like {
            expr,
            pattern,
            escape,
            case_insensitive,
            negated,
        } => Ok(PhysicalExpr::Like {
            expr: Box::new(compile_le(expr, map)?),
            pattern: Box::new(compile_le(pattern, map)?),
            escape: *escape,
            case_insensitive: *case_insensitive,
            negated: *negated,
        }),
        LogicalExpr::Between {
            expr,
            low,
            high,
            negated,
        } => Ok(PhysicalExpr::Between {
            expr: Box::new(compile_le(expr, map)?),
            low: Box::new(compile_le(low, map)?),
            high: Box::new(compile_le(high, map)?),
            negated: *negated,
        }),
        LogicalExpr::ScalarFunc {
            func,
            args,
            return_type,
        } => Ok(PhysicalExpr::ScalarFunc {
            func: *func,
            args: args
                .iter()
                .map(|a| compile_le(a, map))
                .collect::<Result<Vec<_>>>()?,
            return_type: *return_type,
        }),
    }
}

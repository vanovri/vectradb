use crate::shrinker::{shrink_failing_sql, write_failure_reproducer};
use crate::sqlite_oracle::{compare_oracle_results, SqliteOracle};
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use vectra_exec::{Database, ExecutionConfig, SpillManager};
use vectra_types::{BoundedMemoryPool, Result, ScalarValue, SplitMix64};

trait RngExt {
    fn gen_range_u64(&mut self, lo: u64, hi_exclusive: u64) -> u64;
}

impl RngExt for SplitMix64 {
    #[inline]
    fn gen_range_u64(&mut self, lo: u64, hi_exclusive: u64) -> u64 {
        lo + (self.next_u64() % (hi_exclusive - lo))
    }
}

/// Static multi-table dataset loaded identically into both VectraDB and SQLite for SQLsmith fuzzing.
/// Covers `BIGINT`, `DOUBLE`, `VARCHAR`, `BOOLEAN`, NULLs, duplicates, negatives, zero, and empty strings.
pub const SQLSMITH_TABLES: [&str; 6] = ["t0", "t1", "t2", "t3", "t4", "t5"];

pub fn build_sqlsmith_setup_sql() -> String {
    let mut rng = SplitMix64::new(0x517c_c1b7_2722_0a95);
    let mut sql = String::new();
    let words = [
        "alpha",
        "beta",
        "gamma",
        "delta",
        "eps",
        "ALPHA",
        "Beta",
        "",
        "a_b",
        "foo_bar",
        "long_german_string_payload_alpha_01",
        "long_german_string_payload_beta_02",
    ];

    for (ti, tname) in SQLSMITH_TABLES.iter().enumerate() {
        sql.push_str(&format!(
            "CREATE TABLE {tname} (id BIGINT, k BIGINT, v BIGINT, f DOUBLE, s VARCHAR, b BOOLEAN);\n"
        ));
        let nrows = 12 + ti * 3;
        for row_i in 0..nrows {
            let id = (row_i as i64) + 1;
            let k = if rng.gen_range_u64(0, 6) == 0 {
                "NULL".to_string()
            } else {
                (rng.gen_range_u64(0, 8) as i64 - 2).to_string()
            };
            let v = if rng.gen_range_u64(0, 5) == 0 {
                "NULL".to_string()
            } else {
                (rng.gen_range_u64(0, 41) as i64 - 20).to_string()
            };
            let f = if rng.gen_range_u64(0, 6) == 0 {
                "NULL".to_string()
            } else {
                let raw = (rng.gen_range_u64(0, 200) as i64 - 100) as f64 / 4.0;
                format!("{raw:.2}")
            };
            let s = if rng.gen_range_u64(0, 6) == 0 {
                "NULL".to_string()
            } else {
                let w = words[rng.gen_range_u64(0, words.len() as u64) as usize];
                format!("'{w}'")
            };
            let b = match rng.gen_range_u64(0, 5) {
                0 => "NULL",
                1 | 2 => "TRUE",
                _ => "FALSE",
            };
            sql.push_str(&format!(
                "INSERT INTO {tname} VALUES ({id}, {k}, {v}, {f}, {s}, {b});\n"
            ));
        }
    }
    sql
}

#[derive(Debug, Clone)]
pub struct GeneratedQuery {
    pub sql: String,
    pub ordered: bool,
    pub category: &'static str,
}

#[derive(Debug, Clone)]
struct TableRef {
    alias: String,
}

/// Deterministic type-aware SQLsmith generator covering joins (1..6 tables), Kleene 3-valued logic,
/// CASE/COALESCE/IN/LIKE, correlated & uncorrelated subqueries, GROUP BY/HAVING, Window functions,
/// Set operations, and ORDER BY/LIMIT/OFFSET.
pub fn generate_sqlsmith_query(seed: u64) -> GeneratedQuery {
    let mut rng = SplitMix64::new(seed ^ 0xdead_beef_cafe_1234);
    match rng.gen_range_u64(0, 10) {
        0 | 1 => gen_multiway_join_query(&mut rng),
        2 | 3 => gen_subquery_query(&mut rng),
        4 | 5 => gen_aggregate_having_query(&mut rng),
        6 => gen_window_query(&mut rng),
        7 => gen_set_op_query(&mut rng),
        8 => gen_kleene_expr_query(&mut rng),
        _ => gen_ordered_limit_query(&mut rng),
    }
}

fn gen_from_joins(
    rng: &mut SplitMix64,
    min_tables: usize,
    max_tables: usize,
) -> (String, Vec<TableRef>) {
    let n = rng.gen_range_u64(min_tables as u64, (max_tables + 1) as u64) as usize;
    let mut refs = Vec::with_capacity(n);
    let t0 = SQLSMITH_TABLES[rng.gen_range_u64(0, SQLSMITH_TABLES.len() as u64) as usize];
    let mut from = format!("{t0} a0");
    refs.push(TableRef {
        alias: "a0".to_string(),
    });

    for idx in 1..n {
        let t = SQLSMITH_TABLES[rng.gen_range_u64(0, SQLSMITH_TABLES.len() as u64) as usize];
        let alias = format!("a{idx}");
        let prev = &refs[rng.gen_range_u64(0, refs.len() as u64) as usize].alias;
        let join_kind = match rng.gen_range_u64(0, 5) {
            0 | 1 => "INNER JOIN",
            2 => "LEFT JOIN",
            3 => "RIGHT JOIN",
            _ => "FULL OUTER JOIN",
        };
        let on_clause = match rng.gen_range_u64(0, 4) {
            0 => format!("{prev}.k = {alias}.k"),
            1 => format!("{prev}.k = {alias}.k AND {prev}.id <= {alias}.id + 5"),
            2 => format!("{prev}.id = {alias}.k"),
            _ => format!("{prev}.s = {alias}.s"),
        };
        from.push_str(&format!(" {join_kind} {t} {alias} ON {on_clause}"));
        refs.push(TableRef { alias });
    }
    (from, refs)
}

fn gen_int_expr(rng: &mut SplitMix64, refs: &[TableRef], depth: usize) -> String {
    let a = &refs[rng.gen_range_u64(0, refs.len() as u64) as usize].alias;
    if depth == 0 {
        return match rng.gen_range_u64(0, 6) {
            0 => format!("{a}.id"),
            1 => format!("{a}.k"),
            2 => format!("{a}.v"),
            3 => (rng.gen_range_u64(0, 15) as i64 - 5).to_string(),
            4 => format!("COALESCE({a}.v, {a}.k, 0)"),
            _ => format!("ABS({a}.v)"),
        };
    }
    match rng.gen_range_u64(0, 6) {
        0 => format!(
            "({} + {})",
            gen_int_expr(rng, refs, depth - 1),
            gen_int_expr(rng, refs, depth - 1)
        ),
        1 => format!(
            "({} - {})",
            gen_int_expr(rng, refs, depth - 1),
            gen_int_expr(rng, refs, depth - 1)
        ),
        2 => {
            let rhs = (rng.gen_range_u64(1, 7) as i64) - 3;
            let safe_rhs = if rhs == 0 { 2 } else { rhs };
            format!("({} * {safe_rhs})", gen_int_expr(rng, refs, depth - 1))
        }
        3 => {
            let d = (rng.gen_range_u64(1, 6)) as i64;
            format!("({} / {d})", gen_int_expr(rng, refs, depth - 1))
        }
        4 => format!(
            "CASE WHEN {} THEN {} ELSE {} END",
            gen_bool_expr(rng, refs, depth - 1),
            gen_int_expr(rng, refs, depth - 1),
            gen_int_expr(rng, refs, depth - 1)
        ),
        _ => format!("NULLIF({a}.k, 0)"),
    }
}

fn gen_bool_expr(rng: &mut SplitMix64, refs: &[TableRef], depth: usize) -> String {
    let a = &refs[rng.gen_range_u64(0, refs.len() as u64) as usize].alias;
    let b = &refs[rng.gen_range_u64(0, refs.len() as u64) as usize].alias;
    if depth == 0 {
        return match rng.gen_range_u64(0, 11) {
            0 => format!("{a}.k = {b}.k"),
            1 => format!("{a}.v < {}", rng.gen_range_u64(0, 20) as i64 - 10),
            2 => format!("{a}.v >= {b}.k"),
            3 => format!("{a}.k IS NULL"),
            4 => format!("{a}.v IS NOT NULL"),
            5 => format!("{a}.b"),
            6 => format!("NOT {a}.b"),
            7 => format!("{a}.k IN (-1, 0, 1, 2, NULL)"),
            8 => format!("{a}.k NOT IN (1, 3)"),
            9 => format!("{a}.v BETWEEN -5 AND 10"),
            _ => {
                let pat = match rng.gen_range_u64(0, 4) {
                    0 => "'a%'",
                    1 => "'%beta%'",
                    2 => "'_lpha'",
                    _ => "'long_german%'",
                };
                format!("{a}.s LIKE {pat}")
            }
        };
    }
    match rng.gen_range_u64(0, 5) {
        0 | 1 => format!(
            "({} AND {})",
            gen_bool_expr(rng, refs, depth - 1),
            gen_bool_expr(rng, refs, depth - 1)
        ),
        2 | 3 => format!(
            "({} OR {})",
            gen_bool_expr(rng, refs, depth - 1),
            gen_bool_expr(rng, refs, depth - 1)
        ),
        _ => format!("(NOT ({}))", gen_bool_expr(rng, refs, depth - 1)),
    }
}

fn gen_multiway_join_query(rng: &mut SplitMix64) -> GeneratedQuery {
    // Cap at 4 tables when outer joins can fan out, or up to 6 inner joins!
    let max_t = if rng.gen_range_u64(0, 3) == 0 { 6 } else { 3 };
    let (from, refs) = if max_t == 6 {
        // 4..6 way INNER/LEFT join chain
        let n = rng.gen_range_u64(4, 7) as usize;
        let mut r = vec![TableRef {
            alias: "a0".to_string(),
        }];
        let mut f = format!(
            "{} a0",
            SQLSMITH_TABLES[rng.gen_range_u64(0, SQLSMITH_TABLES.len() as u64) as usize]
        );
        for idx in 1..n {
            let t = SQLSMITH_TABLES[rng.gen_range_u64(0, SQLSMITH_TABLES.len() as u64) as usize];
            let alias = format!("a{idx}");
            let prev = &r[rng.gen_range_u64(0, r.len() as u64) as usize].alias;
            let jt = if rng.gen_range_u64(0, 3) == 0 {
                "LEFT JOIN"
            } else {
                "INNER JOIN"
            };
            f.push_str(&format!(" {jt} {t} {alias} ON {prev}.k = {alias}.id"));
            r.push(TableRef { alias });
        }
        (f, r)
    } else {
        gen_from_joins(rng, 2, 3)
    };

    let e1 = gen_int_expr(rng, &refs, 1);
    let e2 = format!("{}.s", refs[refs.len() - 1].alias);
    let where_clause = if rng.gen_range_u64(0, 3) > 0 {
        format!(" WHERE {}", gen_bool_expr(rng, &refs, 1))
    } else {
        String::new()
    };
    let distinct = if rng.gen_range_u64(0, 4) == 0 {
        "DISTINCT "
    } else {
        ""
    };
    GeneratedQuery {
        sql: format!("SELECT {distinct}{e1}, {e2} FROM {from}{where_clause}"),
        ordered: false,
        category: "multiway_join",
    }
}

fn gen_subquery_query(rng: &mut SplitMix64) -> GeneratedQuery {
    let t_outer = SQLSMITH_TABLES[rng.gen_range_u64(0, SQLSMITH_TABLES.len() as u64) as usize];
    let t_inner = SQLSMITH_TABLES[rng.gen_range_u64(0, SQLSMITH_TABLES.len() as u64) as usize];
    let sql = match rng.gen_range_u64(0, 8) {
        0 => format!(
            "SELECT o.id, o.k FROM {t_outer} o WHERE EXISTS (SELECT 1 FROM {t_inner} i WHERE i.k = o.k AND i.v > 0)"
        ),
        1 => format!(
            "SELECT o.id, o.v FROM {t_outer} o WHERE NOT EXISTS (SELECT 1 FROM {t_inner} i WHERE i.k = o.k)"
        ),
        2 => format!(
            "SELECT o.id, o.k FROM {t_outer} o WHERE o.k IN (SELECT i.k FROM {t_inner} i WHERE i.v IS NOT NULL)"
        ),
        3 => format!(
            "SELECT o.id, o.k FROM {t_outer} o WHERE o.k NOT IN (SELECT i.k FROM {t_inner} i WHERE i.v > 5)"
        ),
        4 => format!(
            "SELECT o.id, o.k FROM {t_outer} o WHERE o.k NOT IN (SELECT i.k FROM {t_inner} i WHERE i.k IS NOT NULL AND i.v > 0)"
        ),
        5 => format!(
            "SELECT o.id, o.v FROM {t_outer} o WHERE o.v > 5 OR EXISTS (SELECT 1 FROM {t_inner} i WHERE i.k = o.k AND i.b)"
        ),
        6 => format!(
            "SELECT o.id, (SELECT MAX(i.v) FROM {t_inner} i WHERE i.k = o.k) AS mx FROM {t_outer} o"
        ),
        _ => format!(
            "SELECT o.id, (SELECT COUNT(*) FROM {t_inner} i WHERE i.k = o.k AND i.v >= 0) AS cnt FROM {t_outer} o WHERE o.id <= 10"
        ),
    };
    GeneratedQuery {
        sql,
        ordered: false,
        category: "subquery",
    }
}

fn gen_aggregate_having_query(rng: &mut SplitMix64) -> GeneratedQuery {
    let (from, refs) = gen_from_joins(rng, 1, 2);
    let a = &refs[0].alias;
    let b = &refs[refs.len() - 1].alias;
    let where_clause = if rng.gen_range_u64(0, 2) == 0 {
        format!(" WHERE {}", gen_bool_expr(rng, &refs, 1))
    } else {
        String::new()
    };

    if rng.gen_range_u64(0, 5) == 0 {
        // Global aggregate
        let sql = format!(
            "SELECT COUNT(*), COUNT({a}.v), COUNT(DISTINCT {a}.k), SUM({a}.v), MIN({b}.v), MAX({b}.v), AVG({a}.f) FROM {from}{where_clause}"
        );
        return GeneratedQuery {
            sql,
            ordered: false,
            category: "global_agg",
        };
    }

    let having = match rng.gen_range_u64(0, 4) {
        0 => " HAVING COUNT(*) >= 2".to_string(),
        1 => format!(" HAVING SUM({a}.v) > 0 OR COUNT({a}.v) = 0"),
        2 => format!(" HAVING COUNT(DISTINCT {b}.v) >= 1"),
        _ => String::new(),
    };
    let sql = format!(
        "SELECT {a}.k, COUNT(*), COUNT(DISTINCT {b}.v), SUM({a}.v), MIN({b}.s), AVG({a}.f) FROM {from}{where_clause} GROUP BY {a}.k{having}"
    );
    GeneratedQuery {
        sql,
        ordered: false,
        category: "group_agg",
    }
}

fn gen_window_query(rng: &mut SplitMix64) -> GeneratedQuery {
    let t = SQLSMITH_TABLES[rng.gen_range_u64(0, SQLSMITH_TABLES.len() as u64) as usize];
    let sql = match rng.gen_range_u64(0, 5) {
        0 => format!(
            "SELECT id, k, ROW_NUMBER() OVER (PARTITION BY k ORDER BY id ASC), SUM(v) OVER (PARTITION BY k ORDER BY id ASC ROWS BETWEEN 1 PRECEDING AND 1 FOLLOWING) FROM {t}"
        ),
        1 => format!(
            "SELECT id, k, RANK() OVER (PARTITION BY k ORDER BY v ASC NULLS LAST), DENSE_RANK() OVER (PARTITION BY k ORDER BY v ASC NULLS LAST) FROM {t}"
        ),
        2 => format!(
            "SELECT id, SUM(v) OVER (PARTITION BY k), MIN(v) OVER (PARTITION BY k), MAX(v) OVER (PARTITION BY k) FROM {t}"
        ),
        3 => format!(
            "SELECT id, COUNT(*) OVER (ORDER BY id ASC ROWS BETWEEN UNBOUNDED PRECEDING AND CURRENT ROW), SUM(v) OVER (ORDER BY id ASC ROWS BETWEEN 2 PRECEDING AND CURRENT ROW) FROM {t}"
        ),
        _ => format!(
            "SELECT id, LAG(v, 1, -99) OVER (PARTITION BY k ORDER BY id ASC), LEAD(v, 1, -99) OVER (PARTITION BY k ORDER BY id ASC) FROM {t}"
        ),
    };
    GeneratedQuery {
        sql,
        ordered: false,
        category: "window",
    }
}

fn gen_set_op_query(rng: &mut SplitMix64) -> GeneratedQuery {
    let t1 = SQLSMITH_TABLES[rng.gen_range_u64(0, SQLSMITH_TABLES.len() as u64) as usize];
    let t2 = SQLSMITH_TABLES[rng.gen_range_u64(0, SQLSMITH_TABLES.len() as u64) as usize];
    let op = match rng.gen_range_u64(0, 4) {
        0 => "UNION",
        1 => "UNION ALL",
        2 => "INTERSECT",
        _ => "EXCEPT",
    };
    let sql = format!("SELECT k, b FROM {t1} {op} SELECT k, b FROM {t2}");
    GeneratedQuery {
        sql,
        ordered: false,
        category: "set_op",
    }
}

fn gen_kleene_expr_query(rng: &mut SplitMix64) -> GeneratedQuery {
    let t = SQLSMITH_TABLES[rng.gen_range_u64(0, SQLSMITH_TABLES.len() as u64) as usize];
    let refs = vec![TableRef {
        alias: "a0".to_string(),
    }];
    let p1 = gen_bool_expr(rng, &refs, 2);
    let e1 = gen_int_expr(rng, &refs, 2);
    let pred = gen_bool_expr(rng, &refs, 1);
    let sql = format!("SELECT a0.id, {e1}, {p1} FROM {t} a0 WHERE {pred}");
    GeneratedQuery {
        sql,
        ordered: false,
        category: "kleene_expr",
    }
}

fn gen_ordered_limit_query(rng: &mut SplitMix64) -> GeneratedQuery {
    let t = SQLSMITH_TABLES[rng.gen_range_u64(0, SQLSMITH_TABLES.len() as u64) as usize];
    let dir1 = if rng.gen_range_u64(0, 2) == 0 {
        "ASC"
    } else {
        "DESC"
    };
    let nulls1 = if rng.gen_range_u64(0, 2) == 0 {
        "NULLS FIRST"
    } else {
        "NULLS LAST"
    };
    let col1 = match rng.gen_range_u64(0, 3) {
        0 => "v",
        1 => "s",
        _ => "k",
    };
    let limit = rng.gen_range_u64(1, 12);
    let offset = rng.gen_range_u64(0, 5);
    // Unique primary key `id ASC` guarantees deterministic tie breaking across VectraDB & SQLite!
    let sql = format!(
        "SELECT id, k, v, s FROM {t} ORDER BY {col1} {dir1} {nulls1}, id ASC LIMIT {limit} OFFSET {offset}"
    );
    GeneratedQuery {
        sql,
        ordered: true,
        category: "ordered_limit",
    }
}

#[derive(Debug, Clone)]
pub struct SqlsmithReport {
    pub seeds_run: u64,
    pub spill_seeds_run: u64,
    pub mismatches: u64,
    pub first_failure: Option<SqlsmithFailure>,
}

#[derive(Debug, Clone)]
pub struct SqlsmithFailure {
    pub seed: u64,
    pub original_sql: String,
    pub shrunk_sql: String,
    pub diff: String,
    pub reproducer_path: Option<PathBuf>,
}

/// Runs `num_seeds` consecutive differential SQLsmith tests starting from `start_seed`.
/// Every 10th seed (`seed % 10 == 0`) additionally executes VectraDB under a hard 4 MB
/// `BoundedMemoryPool` forced-spill budget!
pub fn run_sqlsmith_campaign(
    start_seed: u64,
    num_seeds: u64,
    artifacts_dir: Option<&Path>,
) -> Result<SqlsmithReport> {
    let setup_sql = build_sqlsmith_setup_sql();
    let sqlite = SqliteOracle::open_in_memory()?;
    sqlite.execute_batch(&setup_sql)?;

    let spill_dir = std::env::temp_dir().join(format!(
        "vectra_sqlsmith_spill_{}_{}",
        std::process::id(),
        start_seed
    ));
    let _ = std::fs::remove_dir_all(&spill_dir);
    let cfg = ExecutionConfig {
        spill_dir: spill_dir.clone(),
        threads: 1,
        ..ExecutionConfig::default()
    };
    let db = Arc::new(Database::with_config(cfg));
    let mut sess = db.new_session();
    sess.execute_sql(&setup_sql)?;
    db.analyze_table(None)?;

    let mut report = SqlsmithReport {
        seeds_run: 0,
        spill_seeds_run: 0,
        mismatches: 0,
        first_failure: None,
    };

    for offset in 0..num_seeds {
        let seed = start_seed + offset;
        let gen = generate_sqlsmith_query(seed);
        let force_spill = seed.is_multiple_of(10);

        let check_one =
            |sql: &str, sess: &mut vectra_exec::Session| -> std::result::Result<(), String> {
                let sqlite_rows = match sqlite.query(sql) {
                    Ok(r) => r,
                    Err(e) => return Err(format!("SQLite oracle error: {e}")),
                };
                let vectra_rows = if force_spill {
                    let pool = BoundedMemoryPool::new(4 * 1024 * 1024);
                    let spill = SpillManager::new(&spill_dir)
                        .map_err(|e| format!("spill init error: {e}"))?;
                    sess.set_memory_limit(4 * 1024 * 1024);
                    let (_schema, stream, _ctx, _bundle) = sess
                        .execute_query_stream_with_pool(
                            sql,
                            &[],
                            pool.as_dyn(),
                            Arc::clone(&spill),
                            Arc::new(AtomicBool::new(false)),
                        )
                        .map_err(|e| format!("VectraDB (4MB spill) error: {e}"))?;
                    let mut rows: Vec<Vec<ScalarValue>> = Vec::new();
                    for ch in stream {
                        let c = ch.map_err(|e| format!("VectraDB stream error: {e}"))?;
                        for r in 0..c.len() {
                            rows.push(c.get_row(r));
                        }
                    }
                    rows
                } else {
                    sess.set_memory_limit(128 * 1024 * 1024);
                    sess.execute_sql(sql)
                        .map_err(|e| format!("VectraDB error: {e}"))?
                        .to_scalar_rows()
                };

                compare_oracle_results(&vectra_rows, &sqlite_rows, gen.ordered, 1e-4)
            };

        report.seeds_run += 1;
        if force_spill {
            report.spill_seeds_run += 1;
        }

        if let Err(diff) = check_one(&gen.sql, &mut sess) {
            report.mismatches += 1;
            let orig_was_err = diff.contains("VectraDB error:") || diff.contains("VectraDB (4MB");
            let shrunk = shrink_failing_sql(&gen.sql, |cand| {
                let Ok(sr) = sqlite.query(cand) else {
                    return false;
                };
                match sess.execute_sql(cand) {
                    Ok(vr) => compare_oracle_results(&vr.to_scalar_rows(), &sr, gen.ordered, 1e-4)
                        .is_err(),
                    Err(_) => orig_was_err,
                }
            });
            let reproducer_path = match artifacts_dir {
                Some(dir) => {
                    write_failure_reproducer(dir, seed, &setup_sql, &gen.sql, &shrunk, &diff).ok()
                }
                None => None,
            };
            report.first_failure = Some(SqlsmithFailure {
                seed,
                original_sql: gen.sql,
                shrunk_sql: shrunk,
                diff,
                reproducer_path,
            });
            break;
        }
    }

    let _ = std::fs::remove_dir_all(&spill_dir);
    Ok(report)
}

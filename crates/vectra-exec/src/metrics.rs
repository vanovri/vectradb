use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use vectra_optimizer::PhysicalPlan;

/// High-resolution wall clock timer that compiles cleanly on both native and `wasm32`.
#[derive(Debug, Clone, Copy)]
pub struct WallTimer {
    #[cfg(not(target_arch = "wasm32"))]
    start: std::time::Instant,
}

impl WallTimer {
    #[inline]
    pub fn start() -> Self {
        Self {
            #[cfg(not(target_arch = "wasm32"))]
            start: std::time::Instant::now(),
        }
    }

    #[inline]
    pub fn elapsed_nanos(&self) -> u64 {
        #[cfg(not(target_arch = "wasm32"))]
        {
            self.start.elapsed().as_nanos().min(u64::MAX as u128) as u64
        }
        #[cfg(target_arch = "wasm32")]
        {
            0
        }
    }
}

/// Runtime telemetry collected for a single `PhysicalPlan` operator node.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct OperatorMetrics {
    pub plan_id: usize,
    pub op_name: String,
    pub estimated_rows: f64,
    pub input_rows: u64,
    pub output_rows: u64,
    pub chunks_processed: u64,
    pub elapsed_nanos: u64,
    pub peak_memory_bytes: usize,
    pub spilled_bytes: u64,
    pub zonemap_blocks_skipped: u64,
    pub zonemap_blocks_total: u64,
}

impl OperatorMetrics {
    /// Computes cardinality estimation q-error: `max(est / act, act / est)` clamped at >= 1.0.
    pub fn q_error(&self) -> f64 {
        let est = self.estimated_rows.max(1.0);
        let act = (self.output_rows as f64).max(1.0);
        (est / act).max(act / est)
    }
}

/// Serializable snapshot of query-wide execution metrics.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ExecutionMetricsSnapshot {
    pub total_elapsed_nanos: u64,
    pub peak_memory_bytes: usize,
    pub total_spilled_bytes: u64,
    pub operators: BTreeMap<usize, OperatorMetrics>,
}

/// Thread-safe execution telemetry collector shared across all morsel worker threads.
#[derive(Debug, Default)]
pub struct ExecutionMetrics {
    inner: Mutex<ExecutionMetricsSnapshot>,
}

impl ExecutionMetrics {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn init_from_plan(&self, plan: &PhysicalPlan) {
        let mut g = self.inner.lock();
        fn visit(p: &PhysicalPlan, map: &mut BTreeMap<usize, OperatorMetrics>) {
            map.entry(p.plan_id).or_insert_with(|| OperatorMetrics {
                plan_id: p.plan_id,
                op_name: p.op_name().to_string(),
                estimated_rows: p.estimated_rows,
                ..Default::default()
            });
            for child in p.children() {
                visit(child, map);
            }
        }
        visit(plan, &mut g.operators);
    }

    pub fn record_input(&self, plan_id: usize, rows: u64) {
        let mut g = self.inner.lock();
        let entry = g
            .operators
            .entry(plan_id)
            .or_insert_with(|| OperatorMetrics {
                plan_id,
                ..Default::default()
            });
        entry.input_rows += rows;
    }

    pub fn record_output(&self, plan_id: usize, rows: u64, chunks: u64, elapsed_nanos: u64) {
        let mut g = self.inner.lock();
        let entry = g
            .operators
            .entry(plan_id)
            .or_insert_with(|| OperatorMetrics {
                plan_id,
                ..Default::default()
            });
        entry.output_rows += rows;
        entry.chunks_processed += chunks;
        entry.elapsed_nanos += elapsed_nanos;
    }

    pub fn add_elapsed(&self, plan_id: usize, elapsed_nanos: u64) {
        let mut g = self.inner.lock();
        let entry = g
            .operators
            .entry(plan_id)
            .or_insert_with(|| OperatorMetrics {
                plan_id,
                ..Default::default()
            });
        entry.elapsed_nanos += elapsed_nanos;
    }

    pub fn record_peak_memory(&self, plan_id: usize, bytes: usize) {
        let mut g = self.inner.lock();
        let entry = g
            .operators
            .entry(plan_id)
            .or_insert_with(|| OperatorMetrics {
                plan_id,
                ..Default::default()
            });
        if bytes > entry.peak_memory_bytes {
            entry.peak_memory_bytes = bytes;
        }
        if bytes > g.peak_memory_bytes {
            g.peak_memory_bytes = bytes;
        }
    }

    pub fn record_spill(&self, plan_id: usize, bytes: u64) {
        let mut g = self.inner.lock();
        g.total_spilled_bytes += bytes;
        let entry = g
            .operators
            .entry(plan_id)
            .or_insert_with(|| OperatorMetrics {
                plan_id,
                ..Default::default()
            });
        entry.spilled_bytes += bytes;
    }

    pub fn record_zonemap(&self, plan_id: usize, skipped: u64, total: u64) {
        let mut g = self.inner.lock();
        let entry = g
            .operators
            .entry(plan_id)
            .or_insert_with(|| OperatorMetrics {
                plan_id,
                ..Default::default()
            });
        entry.zonemap_blocks_skipped += skipped;
        entry.zonemap_blocks_total += total;
    }

    pub fn set_totals(&self, total_elapsed_nanos: u64, peak_memory_bytes: usize) {
        let mut g = self.inner.lock();
        g.total_elapsed_nanos = total_elapsed_nanos;
        if peak_memory_bytes > g.peak_memory_bytes {
            g.peak_memory_bytes = peak_memory_bytes;
        }
    }

    pub fn snapshot(&self) -> ExecutionMetricsSnapshot {
        self.inner.lock().clone()
    }
}

/// Formats a rich box-drawing `EXPLAIN ANALYZE` tree with wall-time bars, `Est` vs `Act` q-error,
/// peak RAM, spilled bytes, and ZoneMap pruning telemetry.
pub fn format_explain_analyze(plan: &PhysicalPlan, metrics: &ExecutionMetricsSnapshot) -> String {
    let mut out = String::new();
    let total_ns = metrics
        .total_elapsed_nanos
        .max(
            metrics
                .operators
                .values()
                .map(|m| m.elapsed_nanos)
                .max()
                .unwrap_or(1),
        )
        .max(1);

    out.push_str(&format!(
        "EXPLAIN ANALYZE (total={:.2} ms, peak_ram={}, spilled={})\n",
        (metrics.total_elapsed_nanos as f64) / 1_000_000.0,
        format_bytes(metrics.peak_memory_bytes as u64),
        format_bytes(metrics.total_spilled_bytes)
    ));
    fmt_analyze_node(plan, metrics, total_ns, "", true, &mut out);
    out
}

fn fmt_analyze_node(
    plan: &PhysicalPlan,
    metrics: &ExecutionMetricsSnapshot,
    total_ns: u64,
    prefix: &str,
    is_last: bool,
    out: &mut String,
) {
    let connector = if prefix.is_empty() {
        ""
    } else if is_last {
        "└─ "
    } else {
        "├─ "
    };
    let m = metrics.operators.get(&plan.plan_id);
    let act = m.map_or(0, |x| x.output_rows);
    let ns = m.map_or(0, |x| x.elapsed_nanos);
    let ms = (ns as f64) / 1_000_000.0;
    let frac = ((ns as f64) / (total_ns as f64)).clamp(0.0, 1.0);
    let bar_filled = (frac * 10.0).round() as usize;
    let bar = format!(
        "[{}{}]",
        "█".repeat(bar_filled),
        "░".repeat(10 - bar_filled)
    );
    let q_err = m.map_or(1.0, OperatorMetrics::q_error);
    let q_tag = if q_err > 10.0 {
        format!(" q_err={q_err:.1}x!")
    } else {
        format!(" q_err={q_err:.1}x")
    };
    let spill_tag = m
        .filter(|x| x.spilled_bytes > 0)
        .map(|x| format!(" spill={}", format_bytes(x.spilled_bytes)))
        .unwrap_or_default();
    let zm_tag = m
        .filter(|x| x.zonemap_blocks_total > 0)
        .map(|x| {
            format!(
                " zm_skip={}/{}",
                x.zonemap_blocks_skipped, x.zonemap_blocks_total
            )
        })
        .unwrap_or_default();

    out.push_str(&format!(
        "{prefix}{connector}{} [id={}] {bar} {ms:.2}ms | est={:.0} act={act}{q_tag}{spill_tag}{zm_tag}\n",
        plan.op_name(),
        plan.plan_id,
        plan.estimated_rows
    ));

    let next_prefix = if prefix.is_empty() {
        "  ".to_string()
    } else if is_last {
        format!("{prefix}   ")
    } else {
        format!("{prefix}│  ")
    };
    let children = plan.children();
    for (idx, child) in children.iter().enumerate() {
        fmt_analyze_node(
            child,
            metrics,
            total_ns,
            &next_prefix,
            idx + 1 == children.len(),
            out,
        );
    }
}

pub fn format_bytes(bytes: u64) -> String {
    const KIB: f64 = 1024.0;
    const MIB: f64 = 1024.0 * 1024.0;
    const GIB: f64 = 1024.0 * 1024.0 * 1024.0;
    let b = bytes as f64;
    if b >= GIB {
        format!("{:.2} GiB", b / GIB)
    } else if b >= MIB {
        format!("{:.2} MiB", b / MIB)
    } else if b >= KIB {
        format!("{:.1} KiB", b / KIB)
    } else {
        format!("{bytes} B")
    }
}

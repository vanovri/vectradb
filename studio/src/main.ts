import "./styles.css";
import { renderDagSvg } from "./dag";
import { QUERY_PRESETS } from "./presets";
import type {
  ExecuteResponseDto,
  LoadResponseDto,
  StagesDto,
  StorageInspectDto
} from "./types";
import initWasm, { VectraWasmDb } from "./wasm/vectra_wasm";

type ActiveTab = "dag" | "results" | "explain" | "storage";
type StageKey = keyof StagesDto;

function fmtBytes(bytes: number): string {
  if (bytes >= 1024 * 1024) return `${(bytes / (1024 * 1024)).toFixed(2)} MiB`;
  if (bytes >= 1024) return `${(bytes / 1024).toFixed(1)} KiB`;
  return `${bytes} B`;
}

function escapeHtml(s: string): string {
  return s
    .replace(/&/g, "&amp;")
    .replace(/</g, "&lt;")
    .replace(/>/g, "&gt;")
    .replace(/"/g, "&quot;");
}

async function bootstrap(): Promise<void> {
  const rootEl = document.getElementById("app");
  if (!rootEl) return;
  const root: HTMLElement = rootEl;
  root.innerHTML = `<div style="padding:24px;font-family:monospace;color:#22d3ee">Initializing VectraDB WebAssembly Engine…</div>`;

  await initWasm();
  const db = new VectraWasmDb(64);
  const initialLoad: LoadResponseDto = JSON.parse(db.load_tpch(0.005));

  let activeTab: ActiveTab = "dag";
  let activeStage: StageKey = "physical";
  let selectedTable = initialLoad.tables.includes("lineitem")
    ? "lineitem"
    : initialLoad.tables[0] ?? "";
  let memLimitMb = 64;
  let currentSql = QUERY_PRESETS[1].sql;
  let lastExec: ExecuteResponseDto = JSON.parse(db.execute_json(currentSql));
  let tables: string[] = initialLoad.tables;
  let statusBanner = initialLoad.message;

  function render(): void {
    const stageLabels: [StageKey, string][] = [
      ["bound_logical", "1. Bound Logical"],
      ["unnested_logical", "2. Unnested (Neumann-Kemper)"],
      ["optimized_logical", "3. Optimized (DPccp)"],
      ["physical", "4. Physical + Live Metrics"]
    ];

    let tabHtml = "";
    if (!lastExec.ok && lastExec.error) {
      tabHtml = `<div class="error-box">ERROR [${escapeHtml(lastExec.error.sqlstate)}]\n${escapeHtml(lastExec.error.message)}</div>`;
    } else if (activeTab === "dag") {
      if (lastExec.stages) {
        const pills = stageLabels
          .map(
            ([k, lbl]) =>
              `<button class="stage-pill ${activeStage === k ? "active" : ""}" data-stage="${k}">${lbl}</button>`
          )
          .join("");
        tabHtml = `<div class="stage-pills">${pills}</div><div class="dag-container">${renderDagSvg(lastExec.stages[activeStage])}</div>`;
      } else {
        tabHtml = `<div class="explain-pre">${escapeHtml(lastExec.command_tag || "OK")}</div>`;
      }
    } else if (activeTab === "results") {
      if (lastExec.columns.length === 0) {
        tabHtml = `<div class="explain-pre">${escapeHtml(lastExec.command_tag || "Statement completed")}</div>`;
      } else {
        const ths = lastExec.columns
          .map(
            (c) =>
              `<th>${escapeHtml(c.name)} <span style="color:#64748b;font-weight:400">${escapeHtml(c.data_type)}</span></th>`
          )
          .join("");
        const trs = lastExec.rows
          .map(
            (r) =>
              `<tr>${r
                .map((v) =>
                  v === null
                    ? `<td><span class="null-pill">NULL</span></td>`
                    : `<td>${escapeHtml(v)}</td>`
                )
                .join("")}</tr>`
          )
          .join("");
        tabHtml = `<table class="grid-table"><thead><tr>${ths}</tr></thead><tbody>${trs}</tbody></table>`;
      }
    } else if (activeTab === "explain") {
      tabHtml = `<pre class="explain-pre">${escapeHtml(lastExec.explain_analyze ?? "No execution plan for this statement.")}</pre>`;
    } else if (activeTab === "storage") {
      const inspect: StorageInspectDto = selectedTable
        ? JSON.parse(db.inspect_storage_layout(selectedTable))
        : { ok: false, summary: null, byte_views: [], error: "No table selected" };
      const tableSel = tables
        .map(
          (t) =>
            `<option value="${escapeHtml(t)}" ${t === selectedTable ? "selected" : ""}>${escapeHtml(t)}</option>`
        )
        .join("");

      let colsTable = "";
      if (inspect.summary) {
        const rows = inspect.summary.columns
          .map(
            (c) => `<tr>
              <td>${escapeHtml(c.name)}</td>
              <td>${escapeHtml(c.logical_type)}</td>
              <td><span style="color:#22d3ee">${escapeHtml(c.dominant_codec)}</span></td>
              <td>${fmtBytes(c.uncompressed_bytes)} -> ${fmtBytes(c.compressed_bytes)} (${c.compression_ratio.toFixed(2)}x)</td>
              <td>${escapeHtml(c.min_value ?? "-")} .. ${escapeHtml(c.max_value ?? "-")}</td>
            </tr>`
          )
          .join("");
        colsTable = `<table class="grid-table" style="margin-bottom:16px">
          <thead><tr><th>Column</th><th>Type</th><th>Codec</th><th>Compression</th><th>ZoneMap Min..Max</th></tr></thead>
          <tbody>${rows}</tbody>
        </table>`;
      }

      const xrays = inspect.byte_views
        .map((bv) => {
          const cells = bv.bytes_hex
            .map((hex, i) => {
              const cls =
                i < 4
                  ? "hex-len"
                  : i < 8
                    ? "hex-prefix"
                    : bv.is_inline
                      ? "hex-inline"
                      : "hex-ptr";
              return `<div class="hex-cell ${cls}" title="byte ${i}">${hex}</div>`;
            })
            .join("");
          return `<div class="xray-card">
            <div style="display:flex;justify-content:space-between;font-family:var(--mono);font-size:11.5px">
              <span><strong>${escapeHtml(bv.column)}</strong>: "${escapeHtml(bv.value)}"</span>
              <span style="color:${bv.is_inline ? "#34d399" : "#f59e0b"}">${bv.is_inline ? "INLINE <=12B" : "OUT-OF-LINE >12B (prefix + buf_idx + offset)"} · len=${bv.byte_len}</span>
            </div>
            <div class="hex-strip">${cells}</div>
          </div>`;
        })
        .join("");

      tabHtml = `<div style="margin-bottom:12px;display:flex;align-items:center;gap:10px">
        <label style="color:var(--muted);font-size:12px">Table:</label>
        <select id="storage-table-select" class="btn">${tableSel}</select>
      </div>
      ${colsTable}
      <h4 class="section-title">16-Byte Umbra German String ByteView X-Ray ([len:4B] [prefix:4B] [inline or buf_idx+offset:8B])</h4>
      ${xrays || `<div style="color:var(--muted)">No UTF-8 string columns sampled in this table.</div>`}`;
    }

    root.innerHTML = `<div class="studio-shell">
      <header class="topbar">
        <div class="brand">
          <span class="brand-mark">V</span>
          <span>VectraDB Studio</span>
          <span class="brand-sub">WASM Vectorized Columnar OLAP Engine · Cascades/DPccp · Out-of-Core Spill</span>
        </div>
        <div class="topbar-controls">
          <label class="mem-control">
            RAM Ceiling:
            <input id="mem-slider" type="range" min="2" max="256" step="2" value="${memLimitMb}" />
            <strong>${memLimitMb} MiB</strong>
          </label>
          <button id="btn-reload-tpch" class="btn">Reload TPC-H</button>
          <button id="btn-run" class="btn btn-primary">Run SQL (Ctrl+Enter)</button>
        </div>
      </header>
      <div class="workbench">
        <aside class="sidebar">
          <div>
            <h3 class="section-title">Query Presets</h3>
            <div class="preset-list">
              ${QUERY_PRESETS.map(
                (p) => `<button class="preset-btn" data-preset="${p.id}">
                  <span class="preset-title">${escapeHtml(p.title)}</span>
                  <span class="preset-badge">${escapeHtml(p.badge)}</span>
                </button>`
              ).join("")}
            </div>
          </div>
          <div>
            <h3 class="section-title">Import Local Data</h3>
            <div id="dropzone" class="dropzone">Drop .csv or .parquet file here to ingest directly in WASM</div>
          </div>
          <div>
            <h3 class="section-title">Catalog (${tables.length} Tables)</h3>
            <div style="font-family:var(--mono);font-size:11.5px;color:var(--muted);display:flex;flex-direction:column;gap:3px">
              ${tables.map((t) => `<span>· ${escapeHtml(t)}</span>`).join("")}
            </div>
          </div>
        </aside>
        <main class="main-col">
          <section class="editor-pane">
            <div class="editor-bar">
              <div class="telemetry-strip">
                <span>Status: <span class="val">${escapeHtml(statusBanner)}</span></span>
                <span>Rows: <span class="val">${lastExec.total_rows}</span></span>
                <span>Peak Pool RAM: <span class="val">${fmtBytes(lastExec.peak_memory_bytes)}</span></span>
                <span>Spilled: <span class="${lastExec.total_spilled_bytes > 0 ? "spill-hot" : "val"}">${fmtBytes(lastExec.total_spilled_bytes)}</span></span>
              </div>
            </div>
            <textarea id="sql-input" class="sql-editor" spellcheck="false">${escapeHtml(currentSql)}</textarea>
          </section>
          <section class="output-pane">
            <div class="tabs">
              <button class="tab-btn ${activeTab === "dag" ? "active" : ""}" data-tab="dag">4-Stage Plan DAG</button>
              <button class="tab-btn ${activeTab === "results" ? "active" : ""}" data-tab="results">Results (${lastExec.total_rows})</button>
              <button class="tab-btn ${activeTab === "explain" ? "active" : ""}" data-tab="explain">EXPLAIN ANALYZE</button>
              <button class="tab-btn ${activeTab === "storage" ? "active" : ""}" data-tab="storage">Storage &amp; ByteView X-Ray</button>
            </div>
            <div class="tab-body">${tabHtml}</div>
          </section>
        </main>
      </div>
    </div>`;

    wireEvents();
  }

  function runCurrent(): void {
    const ta = document.getElementById("sql-input") as HTMLTextAreaElement | null;
    if (ta) currentSql = ta.value;
    const t0 = performance.now();
    lastExec = JSON.parse(db.execute_json(currentSql));
    const ms = (performance.now() - t0).toFixed(1);
    tables = JSON.parse(db.list_tables_json());
    statusBanner = lastExec.ok ? `${lastExec.command_tag} in ${ms} ms` : "Query error";
    render();
  }

  function wireEvents(): void {
    document.getElementById("btn-run")?.addEventListener("click", runCurrent);
    document.getElementById("sql-input")?.addEventListener("keydown", (ev) => {
      if ((ev.ctrlKey || ev.metaKey) && ev.key === "Enter") {
        ev.preventDefault();
        runCurrent();
      }
    });
    document.getElementById("mem-slider")?.addEventListener("change", (ev) => {
      memLimitMb = Number((ev.target as HTMLInputElement).value);
      db.set_memory_limit_mb(memLimitMb);
      runCurrent();
    });
    document.getElementById("btn-reload-tpch")?.addEventListener("click", () => {
      const resp: LoadResponseDto = JSON.parse(db.load_tpch(0.005));
      tables = resp.tables;
      statusBanner = resp.message;
      runCurrent();
    });
    document.querySelectorAll<HTMLButtonElement>("[data-preset]").forEach((btn) => {
      btn.addEventListener("click", () => {
        const preset = QUERY_PRESETS.find((p) => p.id === btn.dataset.preset);
        if (!preset) return;
        currentSql = preset.sql;
        if (preset.memoryMb) {
          memLimitMb = preset.memoryMb;
          db.set_memory_limit_mb(memLimitMb);
        }
        runCurrent();
      });
    });
    document.querySelectorAll<HTMLButtonElement>("[data-tab]").forEach((btn) => {
      btn.addEventListener("click", () => {
        activeTab = btn.dataset.tab as ActiveTab;
        render();
      });
    });
    document.querySelectorAll<HTMLButtonElement>("[data-stage]").forEach((btn) => {
      btn.addEventListener("click", () => {
        activeStage = btn.dataset.stage as StageKey;
        render();
      });
    });
    document.getElementById("storage-table-select")?.addEventListener("change", (ev) => {
      selectedTable = (ev.target as HTMLSelectElement).value;
      render();
    });

    const dz = document.getElementById("dropzone");
    dz?.addEventListener("dragover", (e) => {
      e.preventDefault();
      dz.classList.add("dragover");
    });
    dz?.addEventListener("dragleave", () => dz.classList.remove("dragover"));
    dz?.addEventListener("drop", async (e) => {
      e.preventDefault();
      dz.classList.remove("dragover");
      const file = e.dataTransfer?.files?.[0];
      if (!file) return;
      const tname = file.name.replace(/\.[^.]+$/, "").replace(/[^a-zA-Z0-9_]/g, "_").toLowerCase();
      let resp: LoadResponseDto;
      if (file.name.toLowerCase().endsWith(".parquet")) {
        const bytes = new Uint8Array(await file.arrayBuffer());
        resp = JSON.parse(db.import_parquet(tname, bytes));
      } else {
        const text = await file.text();
        resp = JSON.parse(db.import_csv(tname, text));
      }
      tables = resp.tables;
      selectedTable = tname;
      statusBanner = resp.ok ? resp.message : `Import failed: ${resp.error}`;
      currentSql = `SELECT * FROM ${tname} LIMIT 50;`;
      runCurrent();
    });
  }

  render();
}

bootstrap().catch((err) => {
  const root = document.getElementById("app");
  if (root) {
    root.innerHTML = `<div class="error-box">WASM Bootstrap Error:\n${String(err)}</div>`;
  }
});

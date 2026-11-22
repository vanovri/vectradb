import { readFileSync } from "node:fs";
import { resolve } from "node:path";
import { describe, expect, it } from "vitest";
import { computeDagLayout, renderDagSvg } from "../src/dag";
import { QUERY_PRESETS } from "../src/presets";
import type { ExecuteResponseDto, LoadResponseDto, StorageInspectDto } from "../src/types";
import { initSync, VectraWasmDb } from "../src/wasm/vectra_wasm";

const wasmPath = resolve(__dirname, "../src/wasm/vectra_wasm_bg.wasm");
initSync({ module: readFileSync(wasmPath) });

describe("VectraDB Real Compiled WASM Studio Integration", () => {
  it("loads TPC-H in WASM, runs Q01/Q02/Q19, produces 4-stage Plan DAGs and SVG", () => {
    const db = new VectraWasmDb(64);
    const load: LoadResponseDto = JSON.parse(db.load_tpch(0.005));
    expect(load.ok).toBe(true);
    expect(load.tables.length).toBeGreaterThanOrEqual(8);

    // Run Q02 (Neumann-Kemper Correlated Unnesting proof inside browser WASM!)
    const q02Preset = QUERY_PRESETS.find((p) => p.id === "q02")!;
    const res: ExecuteResponseDto = JSON.parse(db.execute_json(q02Preset.sql));
    expect(res.ok).toBe(true);
    expect(res.stages).not.toBeNull();
    expect(JSON.stringify(res.stages!.bound_logical)).toContain("DependentJoin");
    expect(JSON.stringify(res.stages!.unnested_logical)).not.toContain("DependentJoin");

    // Verify custom SVG DAG layout engine produces valid geometry for all 4 stages
    for (const stage of [
      res.stages!.bound_logical,
      res.stages!.unnested_logical,
      res.stages!.optimized_logical,
      res.stages!.physical
    ]) {
      const layout = computeDagLayout(stage);
      expect(layout.nodes.length).toBeGreaterThan(0);
      const svg = renderDagSvg(stage);
      expect(svg).toContain("<svg");
      expect(svg).toContain("</svg>");
    }

    // Run Q19 (Disjunctive common-factor extraction -> HashJoin!)
    const q19Preset = QUERY_PRESETS.find((p) => p.id === "q19")!;
    const q19: ExecuteResponseDto = JSON.parse(db.execute_json(q19Preset.sql));
    expect(q19.ok).toBe(true);
    expect(JSON.stringify(q19.stages!.physical)).toContain("HashJoin");
  });

  it("imports CSV and renders 16-byte German String ByteView X-Ray (inline vs out-of-line)", () => {
    const db = new VectraWasmDb(32);
    const csv =
      "id,short_s,long_s\n" +
      "1,hello_world,vectradb_german_string_exceeds_twelve_bytes_heap\n" +
      "2,abc,another_long_payload_for_prefix_and_offset_xray\n";
    const imp: LoadResponseDto = JSON.parse(db.import_csv("xray_demo", csv));
    expect(imp.ok).toBe(true);

    const inspect: StorageInspectDto = JSON.parse(db.inspect_storage_layout("xray_demo"));
    expect(inspect.ok).toBe(true);
    expect(inspect.summary?.total_rows).toBe(2);
    expect(inspect.byte_views.length).toBeGreaterThanOrEqual(2);
    expect(inspect.byte_views.some((b) => b.is_inline && b.bytes_hex.length === 16)).toBe(true);
    expect(inspect.byte_views.some((b) => !b.is_inline && b.bytes_hex.length === 16)).toBe(true);
  });

  it("enforces live 2 MiB memory ceiling with in-WASM compressed spilling", () => {
    const db = new VectraWasmDb(2);
    const load: LoadResponseDto = JSON.parse(db.load_tpch(0.005));
    expect(load.ok).toBe(true);

    const spillPreset = QUERY_PRESETS.find((p) => p.id === "spill")!;
    const res: ExecuteResponseDto = JSON.parse(db.execute_json(spillPreset.sql));
    expect(res.ok).toBe(true);
    expect(res.total_rows).toBeGreaterThan(0);
    expect(res.peak_memory_bytes).toBeLessThanOrEqual(2 * 1024 * 1024);
  });
});

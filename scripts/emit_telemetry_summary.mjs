#!/usr/bin/env node
/**
 * VectraDB Strict Live Telemetry Emitter (Wave 0/1 Canonical Contract)
 *
 * Single source of truth (zero hardcoded metrics):
 *   1. docs/benchmarks/tpch_results.md     (--report / --tpch)
 *   2. PROGRESS.md                         (--progress)
 *   3. Cargo.toml / crates/*               (cratesCount + #![deny(unsafe_code)] audit)
 *   4. studio/src/wasm/vectra_wasm_bg.wasm (raw & gzip / 1024 + magic bytes + SHA-256)
 *
 * Default output:
 *   studio/public/telemetry/summary.json
 *
 * Negative contract test:
 *   node scripts/emit_telemetry_summary.mjs --report Cargo.toml --out .telemetry-logs/neg.json --strict
 *   -> EXIT 1, no output file created.
 */

import fs from 'node:fs';
import path from 'node:path';
import crypto from 'node:crypto';
import zlib from 'node:zlib';
import { execFileSync } from 'node:child_process';

const ROOT = process.cwd();
const WASM_MAGIC_HEX = '0061736d';

function parseArgs(argv) {
  const opts = {
    reportPath:
      process.env.VECTRA_TPCH_MD ?? path.join(ROOT, 'docs/benchmarks/tpch_results.md'),
    progressPath: process.env.VECTRA_PROGRESS_MD ?? path.join(ROOT, 'PROGRESS.md'),
    wasmPath: process.env.VECTRA_WASM_PATH ?? null,
    outPath:
      process.env.VECTRA_SUMMARY_OUT ??
      path.join(ROOT, 'studio/public/telemetry/summary.json'),
    strict: false,
    verifyOnly: false,
  };

  for (let i = 0; i < argv.length; i += 1) {
    const arg = argv[i];
    if (arg === '--report' || arg === '--tpch') {
      const next = argv[++i];
      if (!next) throw new Error(`Flag ${arg} requires a file path`);
      opts.reportPath = path.resolve(next);
    } else if (arg === '--progress') {
      const next = argv[++i];
      if (!next) throw new Error('Flag --progress requires a file path');
      opts.progressPath = path.resolve(next);
    } else if (arg === '--wasm') {
      const next = argv[++i];
      if (!next) throw new Error('Flag --wasm requires a file path');
      opts.wasmPath = path.resolve(next);
    } else if (arg === '--out') {
      const next = argv[++i];
      if (!next) throw new Error('Flag --out requires a file path');
      opts.outPath = path.resolve(next);
    } else if (arg === '--strict') {
      opts.strict = true;
    } else if (arg === '--verify') {
      opts.verifyOnly = true;
    } else {
      throw new Error(`Unknown argument: ${arg}`);
    }
  }
  return opts;
}

function sha256Hex(buf) {
  return crypto.createHash('sha256').update(buf).digest('hex');
}

function roundTo(value, decimals) {
  const factor = 10 ** decimals;
  return Math.round(value * factor) / factor;
}

function toPosixRel(absPath) {
  return path.relative(ROOT, absPath).replace(/\\/g, '/');
}

function stripDecorators(text) {
  return text.replace(/\x1B\[[0-9;]*[A-Za-z]/g, '').replace(/[`*_]/g, '');
}

function parseFloatStrict(raw, label) {
  const s = String(raw).trim().replace(/[\s_]/g, '');
  if (!s) throw new Error(`[PARSE_FAIL] Empty numeric token for "${label}"`);
  let normalized = s;
  if (s.includes('.') && s.includes(',')) {
    normalized =
      s.lastIndexOf(',') > s.lastIndexOf('.')
        ? s.replace(/\./g, '').replace(',', '.')
        : s.replace(/,/g, '');
  } else if (s.includes(',')) {
    normalized = /^\d{1,3}(,\d{3})+$/.test(s) ? s.replace(/,/g, '') : s.replace(',', '.');
  } else if (/^\d{1,3}(\.\d{3}){2,}$/.test(s)) {
    normalized = s.replace(/\./g, '');
  }
  const num = Number(normalized);
  if (!Number.isFinite(num)) {
    throw new Error(`[PARSE_FAIL] Non-finite number for "${label}": "${raw}"`);
  }
  return num;
}

function parseIntStrict(raw, label) {
  const cleaned = String(raw).trim().replace(/[.,_\s]/g, '');
  if (!/^\d+$/.test(cleaned)) {
    throw new Error(`[PARSE_FAIL] Invalid integer for "${label}": "${raw}"`);
  }
  const num = Number.parseInt(cleaned, 10);
  if (!Number.isSafeInteger(num)) {
    throw new Error(`[PARSE_FAIL] Integer overflow for "${label}": "${raw}"`);
  }
  return num;
}

function formatInt(n) {
  return new Intl.NumberFormat('en-US', { maximumFractionDigits: 0 }).format(n);
}

function matchOptional(text, patterns) {
  for (const regex of patterns) {
    const match = text.match(regex);
    if (match) return match;
  }
  return null;
}

function extractRequired(text, patterns, label, sourceFile, errors) {
  const match = matchOptional(text, patterns);
  if (!match) {
    errors.push(`Missing required metric [${label}] in ${toPosixRel(sourceFile)}`);
    return null;
  }
  return match;
}

function inspectWorkspaceCrates() {
  const cratesDir = path.join(ROOT, 'crates');
  if (!fs.existsSync(cratesDir)) return { count: 0, denyUnsafeCount: 0 };
  const dirs = fs
    .readdirSync(cratesDir, { withFileTypes: true })
    .filter((d) => d.isDirectory() && fs.existsSync(path.join(cratesDir, d.name, 'Cargo.toml')))
    .map((d) => d.name)
    .sort();

  let denyUnsafeCount = 0;
  for (const name of dirs) {
    const libRs = path.join(cratesDir, name, 'src', 'lib.rs');
    if (!fs.existsSync(libRs)) continue;
    const head = fs.readFileSync(libRs, 'utf8').slice(0, 1500);
    if (/#!\[deny\(unsafe_code\)\]/.test(head)) {
      denyUnsafeCount += 1;
    }
  }
  return { count: dirs.length, denyUnsafeCount };
}

function discoverWasmBinary(explicitPath) {
  if (explicitPath) {
    if (!fs.existsSync(explicitPath)) {
      throw new Error(`[WASM_MISSING] Explicit WASM file not found: ${explicitPath}`);
    }
    return explicitPath;
  }

  const candidates = [
    path.join(ROOT, 'studio/src/wasm/vectra_wasm_bg.wasm'),
    path.join(ROOT, 'studio/src/wasm-pkg/vectra_wasm_bg.wasm'),
    path.join(ROOT, 'crates/vectra-wasm/pkg/vectra_wasm_bg.wasm'),
    path.join(ROOT, 'studio/public/wasm/vectra_wasm_bg.wasm'),
  ];

  for (const candidate of candidates) {
    if (fs.existsSync(candidate) && fs.statSync(candidate).size > 0) {
      return candidate;
    }
  }

  const distAssets = path.join(ROOT, 'studio/dist/assets');
  if (fs.existsSync(distAssets)) {
    const found = fs
      .readdirSync(distAssets)
      .filter((name) => /^vectra_wasm_bg.*\.wasm$/i.test(name))
      .sort();
    if (found.length > 0) {
      return path.join(distAssets, found[0]);
    }
  }

  throw new Error(
    '[WASM_MISSING] Compiled vectra_wasm_bg.wasm not found. Run "make wasm-studio" or pass --wasm <path>.'
  );
}

function measureWasmBinary(wasmPath) {
  const rawBuf = fs.readFileSync(wasmPath);
  const magicHex = rawBuf.subarray(0, 4).toString('hex');
  if (magicHex !== WASM_MAGIC_HEX) {
    throw new Error(
      `[WASM_INVALID] Invalid WASM magic bytes (${magicHex}, expected ${WASM_MAGIC_HEX}) in ${wasmPath}`
    );
  }
  const gzipBuf = zlib.gzipSync(rawBuf, { level: 9 });
  return {
    relPath: toPosixRel(wasmPath),
    sha256: sha256Hex(rawBuf),
    bytes: rawBuf.byteLength,
    kb: roundTo(rawBuf.byteLength / 1024, 2),
    siKb: roundTo(rawBuf.byteLength / 1000, 2),
    gzipBytes: gzipBuf.byteLength,
    gzipKb: roundTo(gzipBuf.byteLength / 1024, 2),
    gzipSiKb: roundTo(gzipBuf.byteLength / 1000, 2),
  };
}

function getGitMeta() {
  const runGit = (args) => {
    try {
      return execFileSync('git', args, { cwd: ROOT, encoding: 'utf8' }).trim();
    } catch {
      return null;
    }
  };
  const statusOut = runGit(['status', '--porcelain']);
  return {
    commitSha: runGit(['rev-parse', '--short', 'HEAD']),
    workingTreeClean: statusOut !== null ? statusOut.length === 0 : false,
  };
}

function parseTpchReport(mdRaw, filePath, errors) {
  const clean = stripDecorators(mdRaw);

  const osArchMatch = matchOptional(mdRaw, [/OS \/ Arch\*\*:\s*`([^`]+)`\s*\/\s*`([^`]+)`/i]);
  const osArch = osArchMatch ? `${osArchMatch[1]}/${osArchMatch[2]}` : null;

  const threadsMatch = matchOptional(clean, [/Worker Threads:\s*(\d+)/i]);
  const workerThreads = threadsMatch ? parseIntStrict(threadsMatch[1], 'workerThreads') : null;

  const exactMatch = extractRequired(
    clean,
    [/Differential Oracle Validation:\s*(\d+)\s*\/\s*(\d+)\s*queries exact match vs bundled SQLite at SF=([\d.]+)/i],
    'tpchExactMatch',
    filePath,
    errors
  );
  const exactMatchPassed = exactMatch ? parseIntStrict(exactMatch[1], 'exactMatchPassed') : null;
  const exactMatchTotal = exactMatch ? parseIntStrict(exactMatch[2], 'exactMatchTotal') : null;
  const exactScaleFactor = exactMatch ? parseFloatStrict(exactMatch[3], 'exactScaleFactor') : null;

  const sfMatch = extractRequired(
    clean,
    [/Benchmark Scale Factor:\s*SF=([\d.]+)\s*\(([\d,]+)\s*total rows,\s*([\d,]+)\s*lineitem rows\)/i],
    'benchScaleFactor',
    filePath,
    errors
  );
  const benchScaleFactor = sfMatch ? parseFloatStrict(sfMatch[1], 'benchScaleFactor') : null;
  const sf01TotalRows = sfMatch ? parseIntStrict(sfMatch[2], 'sf01TotalRows') : null;
  const sf01LineitemRows = sfMatch ? parseIntStrict(sfMatch[3], 'sf01LineitemRows') : null;

  const ingestMatch = matchOptional(clean, [/Ingest \+ Compression \+ ANALYZE Time:\s*([\d.,]+)\s*ms/i]);
  const ingestAnalyzeMs = ingestMatch ? parseFloatStrict(ingestMatch[1], 'ingestAnalyzeMs') : null;

  const totalMedianMatch = extractRequired(
    clean,
    [/Total 22-Query Median Wall Time:\s*([\d.,]+)\s*ms/i],
    'totalMedianMs',
    filePath,
    errors
  );
  const totalMedianMs = totalMedianMatch
    ? parseFloatStrict(totalMedianMatch[1], 'totalMedianMs')
    : null;

  const geomeanMatch = extractRequired(
    clean,
    [/22-Query Geometric Mean Latency:\s*([\d.,]+)\s*ms/i],
    'geomeanMs',
    filePath,
    errors
  );
  const geomeanMs = geomeanMatch ? parseFloatStrict(geomeanMatch[1], 'geomeanMs') : null;

  // Storage Total row: | Total | 865303 | 113.14 MiB | 16.25 MiB | 6.96x |
  const totalRow = extractRequired(
    clean,
    [/\|\s*Total\s*\|\s*([\d,]+)\s*\|\s*([\d.,]+)\s*MiB\s*\|\s*([\d.,]+)\s*MiB\s*\|\s*([\d.,]+)x\s*\|/i],
    'storageCompressionTotalRow',
    filePath,
    errors
  );
  const uncompressedMib = totalRow ? parseFloatStrict(totalRow[2], 'uncompressedMib') : null;
  const compressedMib = totalRow ? parseFloatStrict(totalRow[3], 'compressedMib') : null;
  const compressionRatio = totalRow ? parseFloatStrict(totalRow[4], 'compressionRatio') : null;
  const computedCompressionRatio =
    uncompressedMib !== null && compressedMib !== null && compressedMib > 0
      ? roundTo(uncompressedMib / compressedMib, 2)
      : null;

  const queryRowsCount = [...clean.matchAll(/^\|\s*Q\d{2}\s+[^|]+\|/gm)].length;
  if (queryRowsCount < 22) {
    errors.push(`Expected 22 TPC-H query rows in ${toPosixRel(filePath)}, found ${queryRowsCount}`);
  }

  return {
    osArch,
    workerThreads,
    exactScaleFactor,
    exactMatchPassed,
    exactMatchTotal,
    benchScaleFactor,
    sf01TotalRows,
    sf01LineitemRows,
    ingestAnalyzeMs,
    totalMedianMs,
    geomeanMs,
    uncompressedMib,
    compressedMib,
    compressionRatio,
    computedCompressionRatio,
    queryRowsCount,
  };
}

function parseProgressMd(mdRaw, filePath, errors) {
  const clean = stripDecorators(mdRaw);

  const miriMatch = extractRequired(
    clean,
    [/Miri\s*\((\d+)\s*seeds,\s*(\d+)\s*UB\)/i],
    'miriSeedsAndUb',
    filePath,
    errors
  );
  const miriSeeds = miriMatch ? parseIntStrict(miriMatch[1], 'miriSeeds') : null;
  const miriUbCount = miriMatch ? parseIntStrict(miriMatch[2], 'miriUbCount') : null;

  const exprFuzzMatch = matchOptional(clean, [/([\d,]+)\s*random AST seeds/i]);
  const exprFuzzSeeds = exprFuzzMatch ? parseIntStrict(exprFuzzMatch[1], 'exprFuzzSeeds') : null;

  const optMatch = matchOptional(clean, [/(\d+)\s*join graphs[^\n|]*\|\s*([\d.]+)ms\s*total join stress/i]);
  const optimizerJoinGraphs = optMatch ? parseIntStrict(optMatch[1], 'optimizerJoinGraphs') : null;
  const optimizerStressMs = optMatch ? parseFloatStrict(optMatch[2], 'optimizerStressMs') : null;

  const spillMatch = extractRequired(
    clean,
    [/(\d+)\s*Mi?B\s*spill torture/i],
    'spillBudgetMb',
    filePath,
    errors
  );
  const spillBudgetMb = spillMatch ? parseIntStrict(spillMatch[1], 'spillBudgetMb') : null;

  const sltMatch = extractRequired(
    clean,
    [/(\d+)\s*\.slt\s*files\s*\(([\d,]+)\s*SQL assertions\)/i],
    'sltSuite',
    filePath,
    errors
  );
  const sltFilesCount = sltMatch ? parseIntStrict(sltMatch[1], 'sltFilesCount') : null;
  const sltAssertionsCount = sltMatch ? parseIntStrict(sltMatch[2], 'sltAssertionsCount') : null;

  const sqlsmithMatch = extractRequired(
    clean,
    [/([\d,]+)\s*\/\s*([\d,]+)\s*pass\s*\((\d+)\s*mismatches/i],
    'sqlsmithPassRatio',
    filePath,
    errors
  );
  const sqlsmithPassed = sqlsmithMatch ? parseIntStrict(sqlsmithMatch[1], 'sqlsmithPassed') : null;
  const sqlsmithTotal = sqlsmithMatch ? parseIntStrict(sqlsmithMatch[2], 'sqlsmithTotal') : null;
  const sqlsmithMismatches = sqlsmithMatch
    ? parseIntStrict(sqlsmithMatch[3], 'sqlsmithMismatches')
    : null;

  const pgMatch = extractRequired(
    clean,
    [/(\d+)\s*concurrent TCP clients[^\n]*\|\s*([\d.]+)s\s*full\s*\d+-client run/i],
    'pgConcurrentGate',
    filePath,
    errors
  );
  const pgConcurrentClients = pgMatch ? parseIntStrict(pgMatch[1], 'pgConcurrentClients') : null;
  const pgConcurrentSec = pgMatch ? parseFloatStrict(pgMatch[2], 'pgConcurrentSec') : null;

  const vitestMatch = extractRequired(
    clean,
    [/(\d+)\s*\/\s*(\d+)\s*real WASM Vitest/i],
    'studioVitest',
    filePath,
    errors
  );
  const studioVitestPassed = vitestMatch ? parseIntStrict(vitestMatch[1], 'studioVitestPassed') : null;
  const studioVitestTotal = vitestMatch ? parseIntStrict(vitestMatch[2], 'studioVitestTotal') : null;

  return {
    miriSeeds,
    miriUbCount,
    exprFuzzSeeds,
    optimizerJoinGraphs,
    optimizerStressMs,
    spillBudgetMb,
    sltFilesCount,
    sltAssertionsCount,
    sqlsmithPassed,
    sqlsmithTotal,
    sqlsmithMismatches,
    pgConcurrentClients,
    pgConcurrentSec,
    studioVitestPassed,
    studioVitestTotal,
  };
}

function buildSummary(opts) {
  const errors = [];

  if (!fs.existsSync(opts.reportPath)) {
    throw new Error(`[INPUT_MISSING] TPC-H benchmark report not found: ${opts.reportPath}`);
  }
  if (!fs.existsSync(opts.progressPath)) {
    throw new Error(`[INPUT_MISSING] Progress ledger not found: ${opts.progressPath}`);
  }

  const reportBuf = fs.readFileSync(opts.reportPath);
  const progressBuf = fs.readFileSync(opts.progressPath);
  const reportText = reportBuf.toString('utf8');
  const progressText = progressBuf.toString('utf8');

  const wasmPath = discoverWasmBinary(opts.wasmPath);
  const wasmInfo = measureWasmBinary(wasmPath);
  const git = getGitMeta();
  const ws = inspectWorkspaceCrates();
  if (ws.count <= 0) {
    errors.push('No crates/*/Cargo.toml found in workspace');
  }
  if (!git.commitSha) {
    errors.push('Unable to resolve git commitSha');
  }

  const tpch = parseTpchReport(reportText, opts.reportPath, errors);
  const prog = parseProgressMd(progressText, opts.progressPath, errors);

  // Hard correctness, safety & consistency invariants
  if (tpch.exactMatchTotal !== null && (tpch.exactMatchTotal < 22 || tpch.exactMatchPassed !== tpch.exactMatchTotal)) {
    errors.push(`TPC-H exact match below 22/22: ${tpch.exactMatchPassed}/${tpch.exactMatchTotal}`);
  }
  if (tpch.geomeanMs !== null && !(tpch.geomeanMs > 0)) {
    errors.push(`geomeanMs must be > 0 (got ${tpch.geomeanMs})`);
  }
  if (tpch.totalMedianMs !== null && !(tpch.totalMedianMs > 0)) {
    errors.push(`totalMedianMs must be > 0 (got ${tpch.totalMedianMs})`);
  }
  if (tpch.compressionRatio !== null && tpch.computedCompressionRatio !== null) {
    const delta = Math.abs(tpch.compressionRatio - tpch.computedCompressionRatio);
    if (delta > 0.15) {
      errors.push(
        `Compression ratio drift: reported ${tpch.compressionRatio}x vs computed ${tpch.computedCompressionRatio}x`
      );
    }
  }
  if (prog.miriUbCount !== null && prog.miriUbCount !== 0) {
    errors.push(`Miri UB violation: miriUbCount=${prog.miriUbCount}`);
  }
  if (
    prog.sqlsmithTotal !== null &&
    (prog.sqlsmithTotal < 25000 || prog.sqlsmithPassed !== prog.sqlsmithTotal)
  ) {
    errors.push(`SQLsmith gate incomplete: ${prog.sqlsmithPassed}/${prog.sqlsmithTotal}`);
  }
  if (prog.sqlsmithMismatches !== null && prog.sqlsmithMismatches !== 0) {
    errors.push(`SQLsmith mismatches != 0 (${prog.sqlsmithMismatches})`);
  }
  if (
    prog.studioVitestTotal !== null &&
    (prog.studioVitestTotal <= 0 || prog.studioVitestPassed !== prog.studioVitestTotal)
  ) {
    errors.push(`Studio Vitest failed: ${prog.studioVitestPassed}/${prog.studioVitestTotal}`);
  }

  if (errors.length > 0 && opts.strict) {
    throw new Error(`[STRICT_GATE_FAILED] Refusing to write summary.json:\n - ${errors.join('\n - ')}`);
  }

  const status = errors.length === 0 ? 'VERIFIED_LIVE' : 'PARTIAL_VERIFY';

  return {
    schemaVersion: '1.0.0',
    slug: 'vectradb',
    status,
    commitSha: git.commitSha,
    workingTreeClean: git.workingTreeClean,
    verifiedAt: new Date().toISOString(),
    wasmAssetPath: '/labs/vectradb/assets/vectra_wasm_bg.wasm',
    wasmSizeKb: wasmInfo.kb,
    wasmSizeBytes: wasmInfo.bytes,
    wasmGzipKb: wasmInfo.gzipKb,
    cratesCount: ws.count,
    headlineMetrics: [
      {
        id: 'tpch_sf01_geomean',
        label: '22/22 TPC-H (SF=0.1)',
        value:
          tpch.geomeanMs !== null && tpch.compressionRatio !== null
            ? `${tpch.geomeanMs.toFixed(2)} ms geomean (${tpch.compressionRatio.toFixed(2)}x)`
            : null,
      },
      {
        id: 'sqlsmith_slt_oracle',
        label: 'SQLsmith + SLT Oracle',
        value:
          prog.sqlsmithPassed !== null && prog.sltAssertionsCount !== null
            ? `${formatInt(prog.sqlsmithPassed)} seeds / ${formatInt(prog.sltAssertionsCount)} SLT`
            : null,
      },
      {
        id: 'spill_pgwire_gate',
        label: 'Spill Cap & PGWire v3',
        value:
          prog.spillBudgetMb !== null &&
          prog.pgConcurrentClients !== null &&
          prog.pgConcurrentSec !== null
            ? `${prog.spillBudgetMb} MB RAM / ${prog.pgConcurrentClients} clients (${prog.pgConcurrentSec.toFixed(2)}s)`
            : null,
      },
    ],
    extendedProof: {
      osArch: tpch.osArch,
      workerThreads: tpch.workerThreads,
      exactScaleFactor: tpch.exactScaleFactor,
      exactMatchPassed: tpch.exactMatchPassed,
      exactMatchTotal: tpch.exactMatchTotal,
      benchScaleFactor: tpch.benchScaleFactor,
      sf01TotalRows: tpch.sf01TotalRows,
      sf01LineitemRows: tpch.sf01LineitemRows,
      ingestAnalyzeMs: tpch.ingestAnalyzeMs,
      totalMedianMs: tpch.totalMedianMs,
      geomeanMs: tpch.geomeanMs,
      uncompressedMib: tpch.uncompressedMib,
      compressedMib: tpch.compressedMib,
      compressionRatio: tpch.compressionRatio,
      computedCompressionRatio: tpch.computedCompressionRatio,
      denyUnsafeCratesCount: ws.denyUnsafeCount,
      miriSeeds: prog.miriSeeds,
      miriUbCount: prog.miriUbCount,
      exprFuzzSeeds: prog.exprFuzzSeeds,
      optimizerJoinGraphs: prog.optimizerJoinGraphs,
      optimizerStressMs: prog.optimizerStressMs,
      spillBudgetMb: prog.spillBudgetMb,
      sltFilesCount: prog.sltFilesCount,
      sltAssertionsCount: prog.sltAssertionsCount,
      sqlsmithPassed: prog.sqlsmithPassed,
      sqlsmithTotal: prog.sqlsmithTotal,
      sqlsmithMismatches: prog.sqlsmithMismatches,
      pgConcurrentClients: prog.pgConcurrentClients,
      pgConcurrentSec: prog.pgConcurrentSec,
      studioVitestPassed: prog.studioVitestPassed,
      studioVitestTotal: prog.studioVitestTotal,
      wasmGzipBytes: wasmInfo.gzipBytes,
      wasmSiKb: wasmInfo.siKb,
      wasmGzipSiKb: wasmInfo.gzipSiKb,
    },
    provenance: {
      reportSha256: sha256Hex(reportBuf),
      progressSha256: sha256Hex(progressBuf),
      sourceFiles: [toPosixRel(opts.reportPath), toPosixRel(opts.progressPath)],
      wasmSourcePath: wasmInfo.relPath,
      wasmSha256: wasmInfo.sha256,
      missingOrWarnings: errors,
    },
  };
}

function stripVolatile(obj) {
  return {
    schemaVersion: obj.schemaVersion,
    slug: obj.slug,
    status: obj.status,
    wasmAssetPath: obj.wasmAssetPath,
    wasmSizeKb: obj.wasmSizeKb,
    wasmSizeBytes: obj.wasmSizeBytes,
    wasmGzipKb: obj.wasmGzipKb,
    cratesCount: obj.cratesCount,
    headlineMetrics: obj.headlineMetrics,
    extendedProof: obj.extendedProof,
    provenance: obj.provenance,
  };
}

function verifyExistingSummary(opts, freshSummary) {
  if (!fs.existsSync(opts.outPath)) {
    throw new Error(`[VERIFY_FAIL] Summary file not found: ${opts.outPath}`);
  }
  const existing = JSON.parse(fs.readFileSync(opts.outPath, 'utf8'));
  if (existing.status !== 'VERIFIED_LIVE') {
    throw new Error(`[VERIFY_FAIL] Existing summary status is ${existing.status}, expected VERIFIED_LIVE`);
  }
  if (JSON.stringify(stripVolatile(existing)) !== JSON.stringify(stripVolatile(freshSummary))) {
    throw new Error(
      `[DRIFT_DETECTED] ${toPosixRel(opts.outPath)} is out of sync with TPC-H/PROGRESS/WASM artifacts. Run "make telemetry-summary".`
    );
  }
}

function writeAtomicJson(outPath, payload) {
  fs.mkdirSync(path.dirname(outPath), { recursive: true });
  const tmpPath = `${outPath}.tmp.${process.pid}`;
  try {
    fs.writeFileSync(tmpPath, `${JSON.stringify(payload, null, 2)}\n`, 'utf8');
    fs.renameSync(tmpPath, outPath);
  } catch (err) {
    if (fs.existsSync(tmpPath)) fs.unlinkSync(tmpPath);
    throw err;
  }
}

function main() {
  const opts = parseArgs(process.argv.slice(2));
  const summary = buildSummary(opts);

  if (opts.verifyOnly) {
    verifyExistingSummary(opts, summary);
    console.log(
      `[OK] verify-live (slug=${summary.slug}, commit=${summary.commitSha}, crates=${summary.cratesCount}, ` +
        `denyUnsafe=${summary.extendedProof.denyUnsafeCratesCount}/${summary.cratesCount}, ` +
        `TPC-H=${summary.extendedProof.exactMatchPassed}/${summary.extendedProof.exactMatchTotal}, ` +
        `geomean=${summary.extendedProof.geomeanMs}ms, comp=${summary.extendedProof.compressionRatio}x, ` +
        `wasm=${summary.wasmSizeKb} KB / ${summary.wasmGzipKb} KB gzip).`
    );
    return;
  }

  writeAtomicJson(opts.outPath, summary);
  console.log(
    `[telemetry] Wrote ${toPosixRel(opts.outPath)} (slug=${summary.slug}, status=${summary.status}, ` +
      `commit=${summary.commitSha}, crates=${summary.cratesCount}, ` +
      `wasm=${summary.wasmSizeKb} KB / ${summary.wasmGzipKb} KB gzip, sha256=${summary.provenance.reportSha256.slice(0, 12)})`
  );
}

try {
  main();
} catch (err) {
  console.error(err instanceof Error ? err.message : String(err));
  process.exit(1);
}

import type { DagNodeDto } from "./types";

export interface PositionedNode {
  dto: DagNodeDto;
  x: number;
  y: number;
  width: number;
  height: number;
}

export interface PositionedEdge {
  from: PositionedNode;
  to: PositionedNode;
  rowVolume: number;
  strokeWidth: number;
}

export interface DagLayout {
  width: number;
  height: number;
  nodes: PositionedNode[];
  edges: PositionedEdge[];
}

const NODE_W = 216;
const NODE_H = 82;
const H_GAP = 28;
const V_GAP = 54;
const PAD = 32;

const CATEGORY_COLORS: Record<string, { border: string; accent: string; bg: string }> = {
  scan: { border: "#22d3ee", accent: "#67e8f9", bg: "#0c1b24" },
  filter: { border: "#34d399", accent: "#6ee7b7", bg: "#0b1f1a" },
  project: { border: "#94a3b8", accent: "#cbd5e1", bg: "#131924" },
  join: { border: "#a78bfa", accent: "#c4b5fd", bg: "#18132b" },
  dependent: { border: "#f43f5e", accent: "#fda4af", bg: "#271018" },
  agg: { border: "#f59e0b", accent: "#fcd34d", bg: "#241a0a" },
  sort: { border: "#38bdf8", accent: "#7dd3fc", bg: "#0c1929" },
  window: { border: "#e879f9", accent: "#f0abfc", bg: "#241129" }
};

export function computeDagLayout(root: DagNodeDto): DagLayout {
  const leafCounts = new Map<DagNodeDto, number>();
  function measure(node: DagNodeDto): number {
    if (node.children.length === 0) {
      leafCounts.set(node, 1);
      return 1;
    }
    let sum = 0;
    for (const c of node.children) sum += measure(c);
    leafCounts.set(node, Math.max(1, sum));
    return Math.max(1, sum);
  }
  measure(root);

  const nodes: PositionedNode[] = [];
  const edges: PositionedEdge[] = [];
  let maxX = 0;
  let maxY = 0;

  function place(node: DagNodeDto, depth: number, leftSlot: number): PositionedNode {
    const span = leafCounts.get(node) ?? 1;
    const slotW = NODE_W + H_GAP;
    const centerSlot = leftSlot + span / 2;
    const x = PAD + centerSlot * slotW - NODE_W / 2;
    const y = PAD + depth * (NODE_H + V_GAP);
    const pos: PositionedNode = { dto: node, x, y, width: NODE_W, height: NODE_H };
    nodes.push(pos);
    maxX = Math.max(maxX, x + NODE_W + PAD);
    maxY = Math.max(maxY, y + NODE_H + PAD);

    let cursor = leftSlot;
    for (const c of node.children) {
      const cSpan = leafCounts.get(c) ?? 1;
      const childPos = place(c, depth + 1, cursor);
      const vol = c.actual_rows ?? c.estimated_rows ?? 1;
      const strokeWidth = Math.max(1.5, Math.min(7.0, 1.5 + Math.log10(Math.max(1, vol)) * 1.1));
      edges.push({ from: pos, to: childPos, rowVolume: vol, strokeWidth });
      cursor += cSpan;
    }
    return pos;
  }

  place(root, 0, 0);
  return {
    width: Math.max(460, Math.ceil(maxX)),
    height: Math.max(240, Math.ceil(maxY)),
    nodes,
    edges
  };
}

function escapeXml(s: string): string {
  return s
    .replace(/&/g, "&amp;")
    .replace(/</g, "&lt;")
    .replace(/>/g, "&gt;")
    .replace(/"/g, "&quot;");
}

function fmtCompact(n: number): string {
  if (n >= 1_000_000) return `${(n / 1_000_000).toFixed(1)}M`;
  if (n >= 1_000) return `${(n / 1_000).toFixed(1)}k`;
  return `${Math.round(n)}`;
}

export function renderDagSvg(root: DagNodeDto): string {
  const layout = computeDagLayout(root);
  const parts: string[] = [];
  parts.push(
    `<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 ${layout.width} ${layout.height}" width="${layout.width}" height="${layout.height}" class="dag-svg">`
  );

  // Subtle dot-grid background pattern
  parts.push(`<defs>
    <pattern id="vdb-grid" width="24" height="24" patternUnits="userSpaceOnUse">
      <circle cx="1" cy="1" r="0.8" fill="#1e293b" />
    </pattern>
  </defs>`);
  parts.push(`<rect width="100%" height="100%" fill="url(#vdb-grid)" />`);

  // Edges (cubic Bézier bottom-up data flow)
  for (const e of layout.edges) {
    const x1 = e.from.x + e.from.width / 2;
    const y1 = e.from.y + e.from.height;
    const x2 = e.to.x + e.to.width / 2;
    const y2 = e.to.y;
    const midY = (y1 + y2) / 2;
    const color = e.to.dto.spilled_bytes && e.to.dto.spilled_bytes > 0 ? "#f59e0b" : "#334155";
    parts.push(
      `<path d="M ${x1} ${y1} C ${x1} ${midY}, ${x2} ${midY}, ${x2} ${y2}" fill="none" stroke="${color}" stroke-width="${e.strokeWidth.toFixed(1)}" stroke-opacity="0.85" />`
    );
  }

  // Operator Nodes
  for (const n of layout.nodes) {
    const palette = CATEGORY_COLORS[n.dto.category] ?? CATEGORY_COLORS.project;
    const detail =
      n.dto.detail.length > 29 ? `${n.dto.detail.slice(0, 28)}…` : n.dto.detail;
    const hasSpill = (n.dto.spilled_bytes ?? 0) > 0;
    const highQErr = (n.dto.q_error ?? 1) > 10;

    parts.push(`<g transform="translate(${n.x}, ${n.y})" class="dag-node" data-id="${escapeXml(n.dto.id)}">`);
    parts.push(
      `<rect width="${n.width}" height="${n.height}" rx="8" fill="${palette.bg}" stroke="${hasSpill ? "#f59e0b" : palette.border}" stroke-width="${hasSpill ? "2" : "1.4"}" />`
    );
    parts.push(
      `<text x="11" y="21" fill="${palette.accent}" font-family="ui-monospace, SFMono-Regular, Menlo, monospace" font-size="12" font-weight="700">${escapeXml(n.dto.label)}</text>`
    );
    parts.push(
      `<text x="${n.width - 10}" y="20" text-anchor="end" fill="#64748b" font-family="ui-monospace, monospace" font-size="10">${escapeXml(n.dto.id)}</text>`
    );
    parts.push(
      `<text x="11" y="40" fill="#cbd5e1" font-family="ui-monospace, monospace" font-size="10.5">${escapeXml(detail)}</text>`
    );

    if (n.dto.estimated_rows !== null || n.dto.actual_rows !== null) {
      const estStr = n.dto.estimated_rows !== null ? fmtCompact(n.dto.estimated_rows) : "-";
      const actStr = n.dto.actual_rows !== null ? fmtCompact(n.dto.actual_rows) : "-";
      const qStr = n.dto.q_error !== null ? `${n.dto.q_error.toFixed(1)}x` : "";
      const footerColor = highQErr ? "#fb7185" : "#94a3b8";
      parts.push(
        `<text x="11" y="62" fill="${footerColor}" font-family="ui-monospace, monospace" font-size="10">est:${estStr} act:${actStr}${qStr ? ` q:${qStr}` : ""}</text>`
      );
      if (hasSpill) {
        parts.push(
          `<text x="${n.width - 10}" y="62" text-anchor="end" fill="#fbbf24" font-family="ui-monospace, monospace" font-size="9.5" font-weight="700">SPILL</text>`
        );
      }
    } else {
      parts.push(
        `<text x="11" y="62" fill="#64748b" font-family="ui-monospace, monospace" font-size="10">${escapeXml(n.dto.category.toUpperCase())}</text>`
      );
    }
    parts.push(`</g>`);
  }

  parts.push(`</svg>`);
  return parts.join("");
}

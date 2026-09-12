// Pure painting helpers: byte-accurate span composition, the display-line
// model shared by the overview and file panes, gutter width, and a
// prefix-height index for measured (wrapped) rows. No DOM here; render.ts
// turns these into elements.

import type {
  AnchoredComment,
  Cell,
  Comment,
  FileView,
  HlSpan,
  Range,
  Row,
  RowAnchor,
} from "./protocol";

// -------------------------------------------------------------- segments

/** One styled run of a cell's text. */
export interface Segment {
  text: string;
  fg: string | null;
  bold: boolean;
  italic: boolean;
  inWord: boolean;
  inHit: boolean;
}

const encoder = new TextEncoder();
const decoder = new TextDecoder("utf-8", { fatal: false });

/** True when `i` does not fall inside a multi-byte sequence. */
function isCharBoundary(bytes: Uint8Array, i: number): boolean {
  if (i <= 0 || i >= bytes.length) return true;
  return ((bytes[i] ?? 0) & 0xc0) !== 0x80;
}

/** Snap a byte offset down to the nearest char boundary inside `bytes`. */
function snapDown(bytes: Uint8Array, i: number): number {
  let at = Math.max(0, Math.min(i, bytes.length));
  while (!isCharBoundary(bytes, at)) at -= 1;
  return at;
}

/**
 * Split a cell's text at every highlight, word-diff, and search boundary.
 * All offsets are UTF-8 byte offsets from the core; they are clamped to
 * the encoded length and snapped to char boundaries before slicing, and
 * every byte is decoded exactly once, so accents, CJK, combining
 * sequences, and emoji survive intact.
 */
export function composeSegments(cell: Cell, hits: readonly Range[]): Segment[] {
  const bytes = encoder.encode(cell.text);
  const len = bytes.length;
  const bounds = new Set<number>([0, len]);
  const add = (n: number) => bounds.add(snapDown(bytes, n));
  for (const s of cell.hl) {
    add(s.start);
    add(s.end);
  }
  for (const r of cell.wordRanges) {
    add(r.start);
    add(r.end);
  }
  for (const h of hits) {
    add(h.start);
    add(h.end);
  }
  const sorted = [...bounds].sort((a, b) => a - b);
  const out: Segment[] = [];
  for (let i = 0; i + 1 < sorted.length; i++) {
    const a = sorted[i] ?? 0;
    const b = sorted[i + 1] ?? 0;
    if (a >= b) continue;
    const span = cell.hl.find((s) => s.start <= a && a < s.end);
    out.push({
      text: decoder.decode(bytes.subarray(a, b)),
      fg: span?.fg ?? null,
      bold: span?.bold ?? false,
      italic: span?.italic ?? false,
      inWord: cell.wordRanges.some((r) => r.start <= a && a < r.end),
      inHit: hits.some((h) => h.start <= a && a < h.end),
    });
  }
  if (out.length === 0) out.push({ text: cell.text, fg: null, bold: false, italic: false, inWord: false, inHit: false });
  return out;
}

/** Highlight spans alone, for callers that only need colour runs. */
export function spanAt(hl: readonly HlSpan[], offset: number): HlSpan | undefined {
  return hl.find((s) => s.start <= offset && offset < s.end);
}

// ---------------------------------------------------------------- gutter

/** Digits needed for the widest line number in `rows` (at least 3). */
export function gutterDigits(rows: readonly Row[]): number {
  let max = 1;
  for (const row of rows) {
    if (row.type === "unified") max = Math.max(max, row.oldNum ?? 0, row.newNum ?? 0);
    else if (row.type === "split") max = Math.max(max, row.left.line ?? 0, row.right.line ?? 0);
  }
  return Math.max(String(max).length, 3);
}

// ---------------------------------------------------------- display model

/** A comment as the panes show it, whichever pane it came from. */
export interface CommentRecord {
  comment: Comment;
  /** Row anchor in the file pane; null in the overview. */
  anchor: RowAnchor | null;
  wasPath: string | null;
  unattached: boolean;
}

export type DisplayLine =
  | { kind: "notice"; text: string }
  | { kind: "section"; text: string }
  | { kind: "blank" }
  | { kind: "row"; rowIndex: number }
  | { kind: "chead"; record: CommentRecord }
  | { kind: "cline"; record: CommentRecord; text: string; role?: "response" }
  | { kind: "cfoot"; record: CommentRecord };

export type Pane =
  | { kind: "overview"; comments: CommentRecord[] }
  | { kind: "file"; view: FileView; comments: AnchoredComment[] };

function card(out: DisplayLine[], record: CommentRecord): void {
  out.push({ kind: "chead", record });
  for (const line of record.comment.body.split("\n")) out.push({ kind: "cline", record, text: line });
  if (record.comment.response !== undefined) {
    out.push({ kind: "cline", record, text: `↳ ${record.comment.response}`, role: "response" });
  }
  if (record.unattached && record.comment.snippet !== undefined) {
    out.push({ kind: "cline", record, text: `snippet: ${record.comment.snippet}` });
  }
  out.push({ kind: "cfoot", record });
}

export function toRecord(anchored: AnchoredComment): CommentRecord {
  return { comment: anchored.comment, anchor: anchored.anchor, wasPath: anchored.wasPath, unattached: false };
}

/**
 * The flat list of visual lines for a pane. Rowless comments (file level,
 * or a line comment on a view with no rows on its side) sit in the file
 * group under the banner; anchored comments follow their row.
 */
export function buildDisplay(pane: Pane): DisplayLine[] {
  const out: DisplayLine[] = [];
  if (pane.kind === "overview") {
    const reviewLevel = pane.comments.filter((c) => !c.unattached);
    const unattached = pane.comments.filter((c) => c.unattached);
    if (pane.comments.length === 0) {
      out.push({ kind: "notice", text: "no review-level comments; press R to add one" });
    }
    for (const record of reviewLevel) card(out, record);
    if (unattached.length > 0) {
      out.push({ kind: "blank" });
      out.push({
        kind: "section",
        text: `unattached (${unattached.length}) - files no longer in this diff`,
      });
      for (const record of unattached) card(out, record);
    }
    return out;
  }
  const { view } = pane;
  if (view.kind.kind === "binary") out.push({ kind: "notice", text: view.kind.desc });
  if (view.kind.kind === "toolarge") {
    out.push({
      kind: "notice",
      text: `diff too large to render (+${view.adds} -${view.dels} lines); skipped`,
    });
  }
  const records = pane.comments.map(toRecord);
  for (const record of records) {
    if (record.anchor?.row === null) card(out, record);
  }
  view.rows.forEach((_row, i) => {
    out.push({ kind: "row", rowIndex: i });
    for (const record of records) {
      if (record.anchor?.row === i) card(out, record);
    }
  });
  return out;
}

/** Stable identity of a display line for cursor preservation across rebuilds. */
export function lineKey(line: DisplayLine, view: FileView | null): string | null {
  switch (line.kind) {
    case "row": {
      const row = view?.rows[line.rowIndex];
      return row ? `r:${row.key}` : `i:${line.rowIndex}`;
    }
    case "chead":
      return `c:${line.record.comment.id}`;
    default:
      return null;
  }
}

// ----------------------------------------------------------- height index

/**
 * Prefix sums over per-row heights (Fenwick tree) so a measured, wrapped
 * layout can map a scroll offset to a row and a row to its top in
 * O(log n), with O(log n) updates when a row is re-measured.
 */
export class HeightIndex {
  private tree: number[];
  private heights: number[];
  readonly length: number;

  constructor(length: number, defaultHeight: number) {
    this.length = length;
    this.heights = new Array<number>(length).fill(defaultHeight);
    this.tree = new Array<number>(length + 1).fill(0);
    for (let i = 0; i < length; i++) this.add(i, defaultHeight);
  }

  private add(i: number, delta: number): void {
    for (let j = i + 1; j <= this.length; j += j & -j) this.tree[j] = (this.tree[j] ?? 0) + delta;
  }

  /** Height of row `i`. */
  get(i: number): number {
    return this.heights[i] ?? 0;
  }

  set(i: number, height: number): void {
    if (i < 0 || i >= this.length) return;
    const delta = height - (this.heights[i] ?? 0);
    if (delta === 0) return;
    this.heights[i] = height;
    this.add(i, delta);
  }

  /** Sum of the heights of rows `0..i` (exclusive): row `i`'s top. */
  prefix(i: number): number {
    let sum = 0;
    for (let j = Math.min(i, this.length); j > 0; j -= j & -j) sum += this.tree[j] ?? 0;
    return sum;
  }

  total(): number {
    return this.prefix(this.length);
  }

  /** The row containing vertical offset `y` (clamped to the last row). */
  indexAt(y: number): number {
    if (this.length === 0) return 0;
    let pos = 0;
    let remaining = Math.max(0, y);
    let step = 1;
    while (step * 2 <= this.length) step *= 2;
    for (; step > 0; step >>= 1) {
      const next = pos + step;
      const width = this.tree[next] ?? 0;
      if (next <= this.length && width <= remaining) {
        pos = next;
        remaining -= width;
      }
    }
    return Math.min(pos, this.length - 1);
  }
}

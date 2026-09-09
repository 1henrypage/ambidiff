// Typed wrapper over the wasm-bindgen exports (`crates/core/src/wasm.rs`).
// Every export returns a JSON string, `{"error": msg}` on failure; this
// module is the only place that parses that convention and decodes the
// result through protocol.ts, so nothing downstream touches raw wasm JSON.

import {
  type AnchoredComment,
  type CommandSpec,
  type ExpansionResult,
  type FileEntry,
  type FileFilter,
  type FileView,
  type GapInfo,
  type ProjectionSnapshot,
  type SearchMatch,
  type ThemeWire,
  type ViewMode,
  type ViewOptionsWire,
  decodeAnchoredComment,
  decodeCommandTable,
  decodeExpansionResult,
  decodeFileView,
  decodeGap,
  decodeProjectionSnapshot,
  decodeSearchMatch,
} from "./protocol";

export class CoreError extends Error {
  constructor(message: string) {
    super(message);
    this.name = "CoreError";
  }
}

/** The subset of the generated wasm-bindgen module the page depends on. */
export interface WasmExports {
  ad_version(): string;
  ad_set_review(content: string): string;
  ad_set_files(filesJson: string): string;
  ad_load_file(path: string, rawDiff: string, optsJson: string): string;
  ad_load_placeholder(path: string, kindJson: string, optsJson: string): string;
  ad_projection(optsJson: string): string;
  ad_file_projection(path: string): string;
  ad_set_view_options(optsJson: string): string;
  ad_pending_expansions(path: string): string;
  ad_expand(path: string, gapId: string, content: string): string;
  ad_search(path: string, query: string): string;
  ad_anchor_target(path: string, row: number, cell: string): string;
  ad_commands(): string;
}

export interface ReviewCounts {
  open: number;
  addressed: number;
  resolved: number;
  reopened: number;
}

export interface ReviewSummary {
  name: string;
  revision: number;
  counts: ReviewCounts;
  readOnly: boolean;
  warnings: string[];
}

export type PlaceholderKind =
  | { kind: "binary"; desc: string }
  | { kind: "toolarge"; adds: number; dels: number };

export interface LoadOptions extends ViewOptionsWire {
  oldTotalLines?: number;
}

export interface FileProjectionResult {
  view: FileView;
  comments: AnchoredComment[];
}

export interface ExpandInstall {
  expansion: ExpansionResult;
  view: FileView;
  comments: AnchoredComment[];
}

export interface AnchorTarget {
  side: "old" | "new";
  line: number;
}

export interface Core {
  setReview(content: string): ReviewSummary;
  setFiles(files: FileEntry[]): number;
  setViewOptions(mode: ViewMode, wordDiff: boolean, theme: ThemeWire): void;
  projection(filter: FileFilter, collapsed: readonly string[]): ProjectionSnapshot;
  loadFile(path: string, raw: string, opts: LoadOptions): FileView;
  loadPlaceholder(path: string, kind: PlaceholderKind, opts: ViewOptionsWire): FileView;
  fileProjection(path: string): FileProjectionResult;
  pendingExpansions(path: string): GapInfo[];
  expand(path: string, gapId: string, content: string): ExpandInstall;
  search(path: string, query: string): SearchMatch[];
  anchorTarget(path: string, row: number, cell: "auto" | "left" | "right"): AnchorTarget | null;
  commands(): CommandSpec[];
}

function parseJson(json: string, what: string): unknown {
  let value: unknown;
  try {
    value = JSON.parse(json);
  } catch (e) {
    throw new CoreError(`${what}: malformed JSON from core: ${String(e)}`);
  }
  if (value !== null && typeof value === "object" && !Array.isArray(value) && "error" in value) {
    throw new CoreError(String((value as { error: unknown }).error));
  }
  return value;
}

export function createCore(wasm: WasmExports): Core {
  return {
    setReview(content) {
      const raw = parseJson(wasm.ad_set_review(content), "setReview") as {
        name: string;
        revision: number;
        counts: ReviewCounts;
        readOnly: boolean;
        warnings: string[];
      };
      return {
        name: raw.name,
        revision: raw.revision,
        counts: raw.counts,
        readOnly: raw.readOnly,
        warnings: raw.warnings,
      };
    },
    setFiles(files) {
      const raw = parseJson(wasm.ad_set_files(JSON.stringify(files)), "setFiles") as {
        generation: number;
      };
      return raw.generation;
    },
    setViewOptions(mode, wordDiff, theme) {
      parseJson(
        wasm.ad_set_view_options(JSON.stringify({ mode, wordDiff, theme })),
        "setViewOptions",
      );
    },
    projection(filter, collapsed) {
      const raw = parseJson(
        wasm.ad_projection(JSON.stringify({ filter, collapsed: [...collapsed] })),
        "projection",
      );
      return decodeProjectionSnapshot(raw);
    },
    loadFile(path, raw, opts) {
      const view = parseJson(wasm.ad_load_file(path, raw, JSON.stringify(opts)), "loadFile");
      return decodeFileView(view);
    },
    loadPlaceholder(path, kind, opts) {
      const view = parseJson(
        wasm.ad_load_placeholder(path, JSON.stringify(kind), JSON.stringify(opts)),
        "loadPlaceholder",
      );
      return decodeFileView(view);
    },
    fileProjection(path) {
      const raw = parseJson(wasm.ad_file_projection(path), "fileProjection") as {
        view: unknown;
        comments: unknown;
      };
      return {
        view: decodeFileView(raw.view),
        comments: Array.isArray(raw.comments) ? raw.comments.map((c) => decodeAnchoredComment(c)) : [],
      };
    },
    pendingExpansions(path) {
      const raw = parseJson(wasm.ad_pending_expansions(path), "pendingExpansions");
      return Array.isArray(raw) ? raw.map((g) => decodeGap(g)) : [];
    },
    expand(path, gapId, content) {
      const raw = parseJson(wasm.ad_expand(path, gapId, content), "expand") as {
        expansion: unknown;
        view: unknown;
        comments: unknown;
      };
      return {
        expansion: decodeExpansionResult(raw.expansion),
        view: decodeFileView(raw.view),
        comments: Array.isArray(raw.comments) ? raw.comments.map((c) => decodeAnchoredComment(c)) : [],
      };
    },
    search(path, query) {
      const raw = parseJson(wasm.ad_search(path, query), "search");
      return Array.isArray(raw) ? raw.map((m) => decodeSearchMatch(m)) : [];
    },
    anchorTarget(path, row, cell) {
      const raw = parseJson(wasm.ad_anchor_target(path, row, cell), "anchorTarget");
      if (raw === null) return null;
      const o = raw as { side: "old" | "new"; line: number };
      return { side: o.side, line: o.line };
    },
    commands() {
      const raw = parseJson(wasm.ad_commands(), "commands");
      return decodeCommandTable(raw);
    },
  };
}

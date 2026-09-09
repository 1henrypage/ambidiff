// A minimal, in-memory stand-in for the wasm-backed `Core`: enough state
// to drive `state.ts`'s tests without loading real wasm. Every method
// mirrors `Core`'s contract but the "derivation" is whatever the test
// configured, not a real diff computation.
import {
  CoreError,
  type AnchorTarget as CoreAnchorTarget,
  type Core,
  type ExpandInstall,
  type FileProjectionResult,
  type ReviewSummary,
} from "../core";
import type {
  CommandSpec,
  FileEntry,
  FileFilter,
  FileView,
  GapInfo,
  OverviewCommentOwned,
  ProjectionSnapshot,
  SearchMatch,
  ThemeWire,
  ViewMode,
} from "../protocol";

export function emptyView(path: string): FileView {
  return {
    path,
    status: "modified",
    kind: { kind: "text" },
    mode: "unified",
    adds: 0,
    dels: 0,
    oldMissingNewline: false,
    newMissingNewline: false,
    rows: [
      { type: "hunkHeader", key: "h0", hunk: 0, text: "@@ -1 +1 @@" },
      {
        type: "unified",
        key: "r0",
        hunk: 0,
        oldNum: 1,
        newNum: 1,
        cell: { kind: "context", text: path, wordRanges: [], hl: [] },
        isExpansion: false,
      },
    ],
  };
}

function placeholderView(path: string, kind: { kind: "binary"; desc: string } | { kind: "toolarge"; adds: number; dels: number }): FileView {
  return {
    path,
    status: "modified",
    kind: kind.kind === "binary" ? { kind: "binary", desc: kind.desc } : { kind: "toolarge" },
    mode: "unified",
    adds: kind.kind === "toolarge" ? kind.adds : 0,
    dels: kind.kind === "toolarge" ? kind.dels : 0,
    oldMissingNewline: false,
    newMissingNewline: false,
    rows: [],
  };
}

export class FakeCore implements Core {
  files: FileEntry[] = [];
  generation = 0;
  views = new Map<string, FileView>();
  commentsByPath = new Map<string, FileProjectionResult["comments"]>();
  overviewComments: OverviewCommentOwned[] = [];
  gaps = new Map<string, GapInfo[]>();
  nextExpand: ExpandInstall | null = null;
  /** Set to make the next `setReview` throw once. */
  nextReviewError: string | null = null;
  summary: ReviewSummary = {
    name: "r",
    revision: 1,
    counts: { open: 0, addressed: 0, resolved: 0, reopened: 0 },
    readOnly: false,
    warnings: [],
  };
  optionsApplied: { mode: ViewMode; wordDiff: boolean; theme: ThemeWire }[] = [];
  anchorResult: CoreAnchorTarget | null = null;
  commandTable: CommandSpec[] = [];
  setReviewCalls: string[] = [];

  setReview(content: string): ReviewSummary {
    this.setReviewCalls.push(content);
    if (this.nextReviewError !== null) {
      const message = this.nextReviewError;
      this.nextReviewError = null;
      throw new CoreError(message);
    }
    return this.summary;
  }

  setFiles(files: FileEntry[]): number {
    this.files = files;
    this.generation += 1;
    return this.generation;
  }

  setViewOptions(mode: ViewMode, wordDiff: boolean, theme: ThemeWire): void {
    this.optionsApplied.push({ mode, wordDiff, theme });
  }

  private countsFor(path: string): { todo: number; total: number } {
    const list = this.commentsByPath.get(path) ?? [];
    const todo = list.filter((c) => c.comment.status === "open" || c.comment.status === "reopened").length;
    return { todo, total: list.length };
  }

  private passesFilter(filter: FileFilter, path: string): boolean {
    const c = this.countsFor(path);
    if (filter === "annotated") return c.total > 0;
    if (filter === "unreviewed") return c.total === 0;
    return true;
  }

  projection(filter: FileFilter, _collapsed: readonly string[]): ProjectionSnapshot {
    const filtered = this.files.filter((f) => this.passesFilter(filter, f.path));
    const counts: Record<string, { todo: number; total: number }> = {};
    for (const f of this.files) counts[f.path] = this.countsFor(f.path);
    return {
      filter,
      files: filtered,
      tree: filtered.map((f) => ({
        depth: 0,
        name: f.path,
        path: f.path,
        isDir: false,
        collapsed: false,
        fileIndex: this.files.indexOf(f),
        commentsTodo: this.countsFor(f.path).todo,
        commentsTotal: this.countsFor(f.path).total,
      })),
      counts,
      reviewLevelComments: this.overviewComments.filter((c) => !c.unattached).length,
      unattachedComments: this.overviewComments.filter((c) => c.unattached).length,
      overview: this.overviewComments,
    };
  }

  loadFile(path: string, _raw: string, _opts: unknown): FileView {
    const view = this.views.get(path) ?? emptyView(path);
    this.views.set(path, view);
    return view;
  }

  loadPlaceholder(
    path: string,
    kind: { kind: "binary"; desc: string } | { kind: "toolarge"; adds: number; dels: number },
    _opts: unknown,
  ): FileView {
    const view = placeholderView(path, kind);
    this.views.set(path, view);
    return view;
  }

  fileProjection(path: string): FileProjectionResult {
    return { view: this.views.get(path) ?? emptyView(path), comments: this.commentsByPath.get(path) ?? [] };
  }

  pendingExpansions(path: string): GapInfo[] {
    return this.gaps.get(path) ?? [];
  }

  expand(_path: string, _gapId: string, _content: string): ExpandInstall {
    if (!this.nextExpand) throw new CoreError("no expand configured for this test");
    return this.nextExpand;
  }

  search(_path: string, _query: string): SearchMatch[] {
    return [];
  }

  anchorTarget(): CoreAnchorTarget | null {
    return this.anchorResult;
  }

  commands(): CommandSpec[] {
    return this.commandTable;
  }
}

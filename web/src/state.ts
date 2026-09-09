// The single read model for the browser painter: everything render.ts
// paints and everything commands.ts calls comes from here. Owns navigation
// (B14: a stale response never clobbers a newer one), the pane (B19: only
// a successful write or a successful load replaces it; every error path
// leaves it alone and flashes), and the projection (B15: recomputed on
// every review and diff broadcast, never just patched in place).

import {
  type Core,
  type LoadOptions,
  type ReviewSummary,
} from "./core";
import {
  type Comment,
  type FileEntry,
  type FileFilter,
  type ProjectionSnapshot,
  type SearchMatch,
  type Side,
  type SkippedPath,
  type ThemeWire,
  type ViewMode,
  type ViewOptionsWire,
  DEFAULT_VIEW_OPTIONS,
} from "./protocol";
import { type CommentRecord, type DisplayLine, type Pane, buildDisplay, lineKey } from "./paint";
import { type RequestBody, type TransportApi, TransportError } from "./transport";

/** A write was refused locally: the caller never touched the network. */
export class WriteBlocked extends Error {
  constructor(
    public readonly reason: "disconnected" | "readOnly" | "empty",
    message: string,
  ) {
    super(message);
    this.name = "WriteBlocked";
  }
}

export type Nav = { kind: "overview" } | { kind: "file"; path: string };

export interface Diagnostics {
  reviewError: string | null;
  sourceError: string | null;
  skipped: SkippedPath[];
  warnings: string[];
  readOnly: boolean;
  readOnlyReason: string | null;
}

export interface SearchState {
  query: string;
  matches: SearchMatch[];
  current: number;
  active: boolean;
}

export type Focus = "tree" | "diff";
export type ActiveCell = "auto" | "left" | "right";

function nextFilter(f: FileFilter): FileFilter {
  return f === "all" ? "annotated" : f === "annotated" ? "unreviewed" : "all";
}

function overviewPane(snapshot: ProjectionSnapshot | null): Pane {
  const comments: CommentRecord[] = (snapshot?.overview ?? []).map((o) => ({
    comment: o.comment,
    anchor: null,
    wasPath: null,
    unattached: o.unattached,
  }));
  return { kind: "overview", comments };
}

export class Store {
  summary: ReviewSummary | null = null;
  projection: ProjectionSnapshot | null = null;
  generation = 0;
  files: FileEntry[] = [];
  nav: Nav = { kind: "overview" };
  navGen = 0;
  pane: Pane | null = null;
  display: DisplayLine[] = [];
  cursor = 0;
  activeCell: ActiveCell = "auto";
  focus: Focus = "diff";
  treeCursor = 0;
  options: ViewOptionsWire = { ...DEFAULT_VIEW_OPTIONS };
  showTree = true;
  showLineNumbers = true;
  wrap = false;
  filter: FileFilter = "all";
  collapsed = new Set<string>();
  search: SearchState = { query: "", matches: [], current: 0, active: false };
  diagnostics: Diagnostics = {
    reviewError: null,
    sourceError: null,
    skipped: [],
    warnings: [],
    readOnly: false,
    readOnlyReason: null,
  };
  commands: import("./protocol").CommandSpec[] = [];
  flash: string | null = null;

  connectionState: import("./transport").ConnectionState = "disconnected";

  private booted = false;
  private token = "";
  private listeners = new Set<() => void>();

  constructor(
    private readonly core: Core,
    private readonly transport: TransportApi,
  ) {}

  subscribe(fn: () => void): () => void {
    this.listeners.add(fn);
    return () => this.listeners.delete(fn);
  }

  private emit(): void {
    for (const fn of this.listeners) fn();
  }

  private paneIdentity(): Nav {
    if (this.pane === null) return { kind: "overview" };
    return this.pane.kind === "overview" ? { kind: "overview" } : { kind: "file", path: this.pane.view.path };
  }

  private currentCursorKey(): string | null {
    const line = this.display[this.cursor];
    if (!line) return null;
    return lineKey(line, this.pane?.kind === "file" ? this.pane.view : null);
  }

  private installPane(pane: Pane, preserveCursor: boolean): void {
    const key = preserveCursor ? this.currentCursorKey() : null;
    this.pane = pane;
    this.display = buildDisplay(pane);
    if (key !== null) {
      const idx = this.display.findIndex(
        (d) => lineKey(d, pane.kind === "file" ? pane.view : null) === key,
      );
      this.cursor = idx >= 0 ? idx : Math.min(this.cursor, Math.max(0, this.display.length - 1));
    } else {
      this.cursor = Math.min(this.cursor, Math.max(0, this.display.length - 1));
    }
    if (this.display.length === 0) this.cursor = 0;
    this.refreshSearchMatches();
  }

  // ------------------------------------------------------------- booting

  async connect(token: string): Promise<void> {
    this.token = token;
    try {
      const hello = await this.transport.connect(token);
      this.commands = this.core.commands();
      this.applySnapshot(hello);
      this.emit();
    } catch (e) {
      this.diagnostics.reviewError = e instanceof Error ? e.message : String(e);
      this.emit();
    }
  }

  /** Whether the transport currently has a live connection. */
  get connected(): boolean {
    return this.transport.connected;
  }

  /**
   * Re-authenticate after a dropped connection. The server's `hello` is
   * always a full `SnapshotFields` payload (that is the wire contract), so
   * it goes through the same `applySnapshot` as boot; no follow-up
   * `refresh` is needed.
   */
  async reconnect(token: string = this.token): Promise<void> {
    this.token = token;
    try {
      const hello = await this.transport.connect(token);
      this.applySnapshot(hello);
      this.emit();
    } catch (e) {
      this.flash = e instanceof Error ? e.message : String(e);
      this.emit();
    }
  }

  async refresh(): Promise<void> {
    try {
      const resp = await this.transport.request({ type: "refresh" });
      if (resp.type === "snapshot") this.applySnapshot(resp);
      this.emit();
    } catch (e) {
      this.flash = e instanceof Error ? e.message : String(e);
      this.emit();
    }
  }

  private applySnapshot(fields: {
    review: string;
    files: FileEntry[];
    sourceError: string | null;
    skipped: SkippedPath[];
    warnings: string[];
    readOnly: boolean;
    readOnlyReason: string | null;
  }): void {
    try {
      this.summary = this.core.setReview(fields.review);
      this.diagnostics.reviewError = null;
    } catch (e) {
      this.diagnostics.reviewError = e instanceof Error ? e.message : String(e);
      // previous summary is kept deliberately (B21).
    }
    this.files = fields.files;
    this.generation = this.core.setFiles(fields.files);
    this.diagnostics.sourceError = fields.sourceError;
    this.diagnostics.skipped = fields.skipped;
    this.diagnostics.warnings = fields.warnings;
    this.diagnostics.readOnly = fields.readOnly;
    this.diagnostics.readOnlyReason = fields.readOnlyReason;
    this.reproject();
    this.reconcileNav();
  }

  private reproject(): void {
    this.projection = this.core.projection(this.filter, [...this.collapsed]);
  }

  /** Bring `nav`/`pane` back in line with the current file listing (B14). */
  private reconcileNav(): void {
    if (this.nav.kind === "file") {
      const path = this.nav.path;
      if (this.files.some((f) => f.path === path)) {
        this.openFile(path, true);
        return;
      }
      const renamed = this.files.find((f) => f.oldPath === path);
      if (renamed) {
        this.openFile(renamed.path, false);
        return;
      }
      this.flash = `${path} is no longer in this diff`;
      this.openOverview();
      return;
    }
    if (!this.booted && this.files.length > 0) {
      this.booted = true;
      this.openFile(this.files[0]?.path ?? "", false);
      return;
    }
    this.booted = true;
    this.installPane(overviewPane(this.projection), true);
  }

  // ------------------------------------------------------------ broadcasts

  onReviewChanged(msg: {
    review: string;
    warnings: string[];
    readOnly: boolean;
    readOnlyReason: string | null;
  }): void {
    try {
      this.summary = this.core.setReview(msg.review);
      this.diagnostics.reviewError = null;
    } catch (e) {
      this.diagnostics.reviewError = e instanceof Error ? e.message : String(e);
    }
    this.diagnostics.readOnly = msg.readOnly;
    this.diagnostics.readOnlyReason = msg.readOnlyReason;
    this.diagnostics.warnings = msg.warnings;
    this.reproject();
    if (this.nav.kind === "overview") {
      this.installPane(overviewPane(this.projection), true);
    } else {
      const fp = this.core.fileProjection(this.nav.path);
      this.installPane({ kind: "file", view: fp.view, comments: fp.comments }, true);
    }
    this.emit();
  }

  onDiffChanged(msg: {
    files: FileEntry[];
    skipped: SkippedPath[];
    sourceError: string | null;
  }): void {
    this.files = msg.files;
    this.generation = this.core.setFiles(msg.files);
    this.diagnostics.sourceError = msg.sourceError;
    this.diagnostics.skipped = msg.skipped;
    this.reproject();
    this.reconcileNav();
    this.emit();
  }

  onConnection(state: import("./transport").ConnectionState): void {
    this.connectionState = state;
    if (state === "disconnected" && this.booted) this.flash = "connection lost";
    this.emit();
  }

  // -------------------------------------------------------------- nav

  openOverview(): void {
    this.navGen += 1;
    this.nav = { kind: "overview" };
    this.installPane(overviewPane(this.projection), false);
    this.emit();
  }

  openFile(path: string, preserve: boolean): void {
    this.navGen += 1;
    const gen = this.navGen;
    this.nav = { kind: "file", path };
    this.transport
      .request({ type: "getFile", path })
      .then((resp) => {
        if (this.navGen !== gen || resp.type !== "file") return;
        if (resp.tooLarge) {
          this.core.loadPlaceholder(
            path,
            { kind: "toolarge", adds: resp.tooLarge.adds, dels: resp.tooLarge.dels },
            this.options,
          );
          const fp = this.core.fileProjection(path);
          this.installPane({ kind: "file", view: fp.view, comments: fp.comments }, preserve);
          this.emit();
          return;
        }
        const opts: LoadOptions = { ...this.options };
        if (resp.oldTotalLines !== undefined) opts.oldTotalLines = resp.oldTotalLines;
        this.core.loadFile(path, resp.raw ?? "", opts);
        const fp = this.core.fileProjection(path);
        this.installPane({ kind: "file", view: fp.view, comments: fp.comments }, preserve);
        this.emit();
        this.restoreExpansions(path, gen);
      })
      .catch((e: unknown) => {
        if (this.navGen !== gen) return;
        this.nav = this.paneIdentity();
        this.flash = e instanceof TransportError ? e.message : e instanceof Error ? e.message : String(e);
        this.emit();
      });
  }

  private restoreExpansions(path: string, gen: number): void {
    const pending = this.core.pendingExpansions(path);
    if (pending.length === 0) return;
    this.transport
      .request({ type: "getSrc", path })
      .then((resp) => {
        if (this.navGen !== gen || resp.type !== "src") return;
        let last: ReturnType<Core["expand"]> | null = null;
        for (const gap of pending) {
          try {
            last = this.core.expand(path, gap.id, resp.content);
          } catch {
            // the gap may no longer exist in the freshly-loaded view
          }
        }
        if (last) {
          this.installPane({ kind: "file", view: last.view, comments: last.comments }, true);
          this.emit();
        }
      })
      .catch(() => {
        // best-effort; the gap simply stays collapsed
      });
  }

  expandGap(gapId: string): void {
    if (this.nav.kind !== "file") return;
    const path = this.nav.path;
    const gen = this.navGen;
    const generationAtCall = this.generation;
    this.transport
      .request({ type: "getSrc", path })
      .then((resp) => {
        if (this.navGen !== gen || this.nav.kind !== "file" || this.nav.path !== path) return;
        if (this.generation !== generationAtCall) return;
        if (resp.type !== "src") return;
        const install = this.core.expand(path, gapId, resp.content);
        this.installPane({ kind: "file", view: install.view, comments: install.comments }, true);
        this.emit();
      })
      .catch((e: unknown) => {
        this.flash = e instanceof Error ? e.message : String(e);
        this.emit();
      });
  }

  stepFile(delta: number): void {
    const order = this.files.map((f) => f.path);
    const at = this.nav.kind === "file" ? order.indexOf(this.nav.path) : -1;
    const next = at < 0 ? 0 : at + delta;
    if (next < 0) {
      this.openOverview();
      return;
    }
    const path = order[next];
    if (path === undefined) {
      this.flash = delta > 0 ? "last file" : "first file";
      this.emit();
      return;
    }
    this.openFile(path, false);
  }

  treeStep(delta: number): void {
    const rows = this.projection?.tree ?? [];
    const len = rows.length + 1; // +1 for the (review) row at index 0
    this.treeCursor = Math.max(0, Math.min(len - 1, this.treeCursor + delta));
    this.emit();
  }

  treeActivate(): void {
    const rows = this.projection?.tree ?? [];
    if (this.treeCursor === 0) {
      this.openOverview();
      return;
    }
    const row = rows[this.treeCursor - 1];
    if (!row) return;
    if (row.isDir) {
      if (this.collapsed.has(row.path)) this.collapsed.delete(row.path);
      else this.collapsed.add(row.path);
      this.reproject();
      this.emit();
      return;
    }
    this.openFile(row.path, false);
  }

  toggleDirPath(path: string): void {
    if (this.collapsed.has(path)) this.collapsed.delete(path);
    else this.collapsed.add(path);
    this.reproject();
    this.emit();
  }

  setFocus(focus: Focus): void {
    this.focus = focus;
    this.emit();
  }

  toggleTree(): void {
    this.showTree = !this.showTree;
    this.emit();
  }

  toggleLineNumbers(): void {
    this.showLineNumbers = !this.showLineNumbers;
    this.emit();
  }

  toggleWrap(): void {
    this.wrap = !this.wrap;
    this.emit();
  }

  setFlash(message: string): void {
    this.flash = message;
    this.emit();
  }

  // ------------------------------------------------------------- cursor

  moveCursor(delta: number): void {
    if (this.display.length === 0) return;
    this.cursor = Math.max(0, Math.min(this.display.length - 1, this.cursor + delta));
    this.activeCell = "auto";
    this.emit();
  }

  clickRow(index: number, cell: ActiveCell | null): void {
    this.focus = "diff";
    this.cursor = Math.max(0, Math.min(Math.max(0, this.display.length - 1), index));
    if (cell) this.activeCell = cell;
    this.emit();
  }

  setCursor(i: number): void {
    if (this.display.length === 0) return;
    this.cursor = Math.max(0, Math.min(this.display.length - 1, i));
    this.emit();
  }

  cursorToTop(): void {
    this.cursor = 0;
    this.activeCell = "auto";
    this.emit();
  }

  cursorToBottom(): void {
    this.cursor = Math.max(0, this.display.length - 1);
    this.activeCell = "auto";
    this.emit();
  }

  jumpTo(pred: (d: DisplayLine) => boolean, forward: boolean): void {
    const { display, cursor } = this;
    if (forward) {
      for (let i = cursor + 1; i < display.length; i++) {
        const line = display[i];
        if (line && pred(line)) {
          this.cursor = i;
          this.activeCell = "auto";
          this.emit();
          return;
        }
      }
    } else {
      for (let i = cursor - 1; i >= 0; i--) {
        const line = display[i];
        if (line && pred(line)) {
          this.cursor = i;
          this.activeCell = "auto";
          this.emit();
          return;
        }
      }
    }
    this.flash = "no more";
    this.emit();
  }

  isHunkLine(d: DisplayLine): boolean {
    return d.kind === "row" && this.pane?.kind === "file" && this.pane.view.rows[d.rowIndex]?.type === "hunkHeader";
  }

  cursorRowIndex(): number | null {
    const d = this.display[this.cursor];
    return d?.kind === "row" ? d.rowIndex : null;
  }

  cursorComment(): CommentRecord | null {
    const d = this.display[this.cursor];
    if (d && (d.kind === "chead" || d.kind === "cline" || d.kind === "cfoot")) return d.record;
    return null;
  }

  cursorLineTarget(): { side: Side; line: number } | null {
    if (this.pane?.kind !== "file") return null;
    const rowIndex = this.cursorRowIndex();
    if (rowIndex === null) return null;
    const anchor = this.core.anchorTarget(this.pane.view.path, rowIndex, this.activeCell);
    return anchor;
  }

  // -------------------------------------------------------------- options

  cycleFilter(): void {
    this.filter = nextFilter(this.filter);
    this.reproject();
    if (this.nav.kind === "overview") this.installPane(overviewPane(this.projection), true);
    this.flash = `filter: ${this.filter}`;
    this.emit();
  }

  setOptions(partial: Partial<ViewOptionsWire>): void {
    this.options = { ...this.options, ...partial };
    this.core.setViewOptions(this.options.mode, this.options.wordDiff, this.options.theme);
    if (this.nav.kind === "file") {
      const fp = this.core.fileProjection(this.nav.path);
      this.installPane({ kind: "file", view: fp.view, comments: fp.comments }, true);
    }
    this.emit();
  }

  toggleMode(): void {
    this.setOptions({ mode: this.options.mode === "unified" ? "split" : "unified" });
  }

  toggleWordDiff(): void {
    this.setOptions({ wordDiff: !this.options.wordDiff });
  }

  setTheme(theme: ThemeWire): void {
    this.setOptions({ theme });
  }

  // -------------------------------------------------------------- search

  startSearch(query: string): void {
    this.search = { query, matches: [], current: 0, active: query.length > 0 };
    this.refreshSearchMatches();
    if (this.search.matches.length > 0) this.gotoMatch(0);
    else if (query) this.flash = "no matches";
    this.emit();
  }

  private refreshSearchMatches(): void {
    if (!this.search.query || this.nav.kind !== "file") {
      this.search.matches = [];
      return;
    }
    try {
      this.search.matches = this.core.search(this.nav.path, this.search.query);
    } catch {
      this.search.matches = [];
    }
    if (this.search.current >= this.search.matches.length) this.search.current = 0;
  }

  searchNext(): void {
    this.stepMatch(1);
  }

  searchPrev(): void {
    this.stepMatch(-1);
  }

  private stepMatch(delta: number): void {
    const n = this.search.matches.length;
    if (n === 0) return;
    this.search.current = (this.search.current + delta + n) % n;
    this.gotoMatch(this.search.current);
  }

  private gotoMatch(i: number): void {
    const m = this.search.matches[i];
    if (!m) return;
    const idx = this.display.findIndex((d) => d.kind === "row" && d.rowIndex === m.row);
    if (idx >= 0) {
      this.cursor = idx;
      this.emit();
    }
  }

  // -------------------------------------------------------------- writes

  private ensureWritable(): void {
    if (!this.transport.connected) throw new WriteBlocked("disconnected", "not connected");
    if (this.diagnostics.readOnly) {
      throw new WriteBlocked("readOnly", this.diagnostics.readOnlyReason ?? "review is read-only");
    }
  }

  async addComment(
    body: string,
    target: { path?: string; side?: Side; line?: number; endLine?: number } = {},
  ): Promise<Comment> {
    this.ensureWritable();
    if (body.trim() === "") throw new WriteBlocked("empty", "comment body is required");
    const req: RequestBody = {
      type: "comment.add",
      body,
      ...(target.path !== undefined ? { path: target.path } : {}),
      ...(target.side !== undefined ? { side: target.side } : {}),
      ...(target.line !== undefined ? { line: target.line } : {}),
      ...(target.endLine !== undefined ? { endLine: target.endLine } : {}),
    };
    const resp = await this.transport.request(req);
    if (resp.type !== "comment") throw new Error(`unexpected response ${resp.type}`);
    return resp.comment;
  }

  async editComment(id: string, body: string): Promise<Comment> {
    this.ensureWritable();
    if (body.trim() === "") throw new WriteBlocked("empty", "comment body is required");
    const resp = await this.transport.request({ type: "comment.edit", commentId: id, body });
    if (resp.type !== "comment") throw new Error(`unexpected response ${resp.type}`);
    return resp.comment;
  }

  async deleteComment(id: string): Promise<void> {
    this.ensureWritable();
    await this.transport.request({ type: "comment.delete", commentId: id });
  }

  async address(id: string, response: string): Promise<Comment> {
    this.ensureWritable();
    const req: RequestBody = {
      type: "comment.address",
      commentId: id,
      ...(response.trim() !== "" ? { response } : {}),
    };
    const resp = await this.transport.request(req);
    if (resp.type !== "comment") throw new Error(`unexpected response ${resp.type}`);
    return resp.comment;
  }

  async resolve(id: string): Promise<Comment> {
    this.ensureWritable();
    const resp = await this.transport.request({ type: "comment.resolve", commentId: id });
    if (resp.type !== "comment") throw new Error(`unexpected response ${resp.type}`);
    return resp.comment;
  }

  async reopen(id: string): Promise<Comment> {
    this.ensureWritable();
    const resp = await this.transport.request({ type: "comment.reopen", commentId: id });
    if (resp.type !== "comment") throw new Error(`unexpected response ${resp.type}`);
    return resp.comment;
  }
}

// Wire contract for the browser frontend: the shapes in fixtures/contracts
// as TypeScript interfaces, plus `unknown`-to-validated decoders. Every
// server message and every core JSON value enters the page through here;
// nothing downstream sees `any`.
//
// Decoding rules mirror crates/core/src/protocol.rs: absent or null means
// omitted, any other wrong type is a DecodeError, line coordinates are
// integral within 1..=4294967295, and `side` is exactly "old" or "new".

// ------------------------------------------------------------ decode core

export type DecodeErrorKind = "missing" | "wrongType" | "outOfRange" | "unknown";

export class DecodeError extends Error {
  constructor(
    public readonly kind: DecodeErrorKind,
    public readonly field: string,
    message: string,
  ) {
    super(message);
    this.name = "DecodeError";
  }
}

function missing(field: string): DecodeError {
  return new DecodeError("missing", field, `missing required field "${field}"`);
}
function wrongType(field: string, expected: string): DecodeError {
  return new DecodeError("wrongType", field, `field "${field}" must be ${expected}`);
}
function outOfRange(field: string): DecodeError {
  return new DecodeError(
    "outOfRange",
    field,
    `field "${field}" is out of range (expected an integer in 1..=4294967295)`,
  );
}
function unknown(field: string, value: string): DecodeError {
  return new DecodeError("unknown", field, `field "${field}" has unknown value "${value}"`);
}

export type JsonObject = Record<string, unknown>;

/** The payload must be a plain object. */
export function obj(v: unknown, field = ""): JsonObject {
  if (typeof v !== "object" || v === null || Array.isArray(v)) throw wrongType(field, "an object");
  return v as JsonObject;
}

/** A present, non-null field, or undefined. */
function present(o: JsonObject, field: string): unknown {
  const v = o[field];
  return v === null || v === undefined ? undefined : v;
}

export function optStr(o: JsonObject, field: string): string | undefined {
  const v = present(o, field);
  if (v === undefined) return undefined;
  if (typeof v !== "string") throw wrongType(field, "a string");
  return v;
}

export function str(o: JsonObject, field: string): string {
  const v = optStr(o, field);
  if (v === undefined) throw missing(field);
  return v;
}

/** A 1-based line coordinate: integral, 1..=u32::MAX, never truncated. */
export function optLine(o: JsonObject, field: string): number | undefined {
  const v = present(o, field);
  if (v === undefined) return undefined;
  if (typeof v !== "number") throw wrongType(field, "an integer");
  if (!Number.isInteger(v) || v < 1 || v > 4294967295) throw outOfRange(field);
  return v;
}

/** Any integer (counts, indices, generations); null is omitted. */
export function optInt(o: JsonObject, field: string): number | undefined {
  const v = present(o, field);
  if (v === undefined) return undefined;
  if (typeof v !== "number" || !Number.isInteger(v)) throw wrongType(field, "an integer");
  return v;
}

export function int(o: JsonObject, field: string): number {
  const v = optInt(o, field);
  if (v === undefined) throw missing(field);
  return v;
}

export function optBool(o: JsonObject, field: string): boolean | undefined {
  const v = present(o, field);
  if (v === undefined) return undefined;
  if (typeof v !== "boolean") throw wrongType(field, "a boolean");
  return v;
}

export function bool(o: JsonObject, field: string): boolean {
  const v = optBool(o, field);
  if (v === undefined) throw missing(field);
  return v;
}

/** A string that must be one of `allowed`; absent means undefined. */
export function optLit<T extends string>(
  o: JsonObject,
  field: string,
  allowed: readonly T[],
): T | undefined {
  const v = optStr(o, field);
  if (v === undefined) return undefined;
  if (!(allowed as readonly string[]).includes(v)) throw unknown(field, v);
  return v as T;
}

export function lit<T extends string>(o: JsonObject, field: string, allowed: readonly T[]): T {
  const v = optLit(o, field, allowed);
  if (v === undefined) throw missing(field);
  return v;
}

export function arr<T>(o: JsonObject, field: string, item: (v: unknown, at: string) => T): T[] {
  const v = present(o, field);
  if (v === undefined) throw missing(field);
  if (!Array.isArray(v)) throw wrongType(field, "an array");
  return v.map((x, i) => item(x, `${field}[${i}]`));
}

export function optArr<T>(
  o: JsonObject,
  field: string,
  item: (v: unknown, at: string) => T,
): T[] | undefined {
  return present(o, field) === undefined ? undefined : arr(o, field, item);
}

// ------------------------------------------------------------ core values

export type Side = "old" | "new";
export type Status = "open" | "addressed" | "resolved" | "reopened";
export type FileStatus = "added" | "modified" | "deleted" | "renamed" | "copied" | "untracked" | "other";
export type ViewMode = "unified" | "split";
export type ThemeWire = "dark" | "light" | "none";
export type CellKind = "context" | "add" | "remove" | "empty";

export interface FileEntry {
  path: string;
  oldPath?: string;
  status: FileStatus;
  adds?: number;
  dels?: number;
}

/** Which comparison inside a stack review a comment or a view refers to. */
export type TargetId =
  | { kind: "branch"; name: string }
  | { kind: "head" }
  | { kind: "stack" }
  | { kind: "worktree" };

/** Stable string key of a target (`branch:<name>` or the kind). */
export function targetKey(id: TargetId): string {
  return id.kind === "branch" ? `branch:${id.name}` : id.kind;
}

export function sameTarget(a: TargetId | null | undefined, b: TargetId | null | undefined): boolean {
  if (!a || !b) return a === b;
  return targetKey(a) === targetKey(b);
}

/** Human label: the branch name, or the kind. */
export function targetLabel(id: TargetId): string {
  return id.kind === "branch" ? id.name : id.kind;
}

/** One reviewable comparison inside a stack; every field is present. */
export interface Target {
  id: TargetId;
  label: string;
  position: number | null;
  tip: string | null;
  commitCount: number;
  subject: string | null;
  aliases: string[];
  comparison: Comparison;
}

/** The reviewed commit of a single-commit review (the banner). */
export interface CommitSummary {
  oid: string;
  subject: string;
}

export interface Comment {
  id: string;
  rev: number;
  status: Status;
  path: string | null;
  side?: Side;
  line: number | null;
  endLine?: number;
  target?: TargetId;
  snippet?: string;
  body: string;
  response?: string;
  author: string;
  createdAt: string;
  updatedAt: string;
}

export interface Range {
  start: number;
  end: number;
}

export interface HlSpan extends Range {
  fg?: string;
  bold?: boolean;
  italic?: boolean;
}

export interface Cell {
  kind: CellKind;
  line?: number;
  text: string;
  wordRanges: Range[];
  hl: HlSpan[];
}

export interface GapInfo {
  id: string;
  count: number;
  oldRange: [number, number];
  newRange: [number, number];
}

export type Row =
  | { type: "hunkHeader"; key: string; hunk: number; text: string }
  | { type: "gap"; key: string; gap: GapInfo }
  | {
      type: "unified";
      key: string;
      hunk: number;
      oldNum: number | null;
      newNum: number | null;
      cell: Cell;
      isExpansion: boolean;
    }
  | { type: "split"; key: string; hunk: number; left: Cell; right: Cell; isExpansion: boolean };

export type FileViewKind =
  | { kind: "text" }
  | { kind: "binary"; desc: string }
  | { kind: "toolarge" };

export interface FileView {
  path: string;
  oldPath?: string;
  status: FileStatus;
  kind: FileViewKind;
  mode: ViewMode;
  adds: number;
  dels: number;
  oldMissingNewline: boolean;
  newMissingNewline: boolean;
  rows: Row[];
}

export interface RowAnchor {
  commentId: string;
  row: number | null;
  outdated: boolean;
  clamped: boolean;
}

export interface AnchoredComment {
  comment: Comment;
  anchor: RowAnchor;
  wasPath: string | null;
}

export interface SearchMatch {
  row: number;
  cell: "unified" | "left" | "right";
  start: number;
  end: number;
}

export type LineSide = "new" | "old";

export type LineTarget =
  | { kind: "exact"; row: number; side: LineSide }
  | { kind: "inGap"; row: number; gapId: string }
  | { kind: "nearest"; row: number | null };

export interface SkippedPath {
  display: string;
  reason: string;
}

export type Endpoint =
  | { kind: "commit"; oid: string }
  | { kind: "emptyTree"; oid: string }
  | { kind: "index" }
  | { kind: "worktree" };

export interface Comparison {
  old: Endpoint;
  new: Endpoint;
}

// ------------------------------------------------------ core value decoders

/** A `{kind, name?}` target object; errors name the nested field. */
export function decodeTargetId(v: unknown, at = "target"): TargetId {
  if (typeof v !== "object" || v === null || Array.isArray(v)) throw wrongType(at, "an object {kind, name?}");
  const o = v as JsonObject;
  const kindField = `${at}.kind`;
  const rawKind = present(o, "kind");
  if (rawKind === undefined) throw missing(kindField);
  if (typeof rawKind !== "string") throw wrongType(kindField, "a string");
  switch (rawKind) {
    case "branch": {
      const nameField = `${at}.name`;
      const name = present(o, "name");
      if (name === undefined) throw missing(nameField);
      if (typeof name !== "string") throw wrongType(nameField, "a string");
      return { kind: "branch", name };
    }
    case "head":
    case "stack":
    case "worktree":
      return { kind: rawKind };
    default:
      throw unknown(kindField, rawKind);
  }
}

/** A present target, or null when absent/null. */
export function optTargetId(o: JsonObject, field: string): TargetId | null {
  const v = present(o, field);
  return v === undefined ? null : decodeTargetId(v, field);
}

export function decodeTarget(v: unknown, at = "target"): Target {
  const o = obj(v, at);
  return {
    id: decodeTargetId(o["id"], `${at}.id`),
    label: str(o, "label"),
    position: optInt(o, "position") ?? null,
    tip: optStr(o, "tip") ?? null,
    commitCount: int(o, "commitCount"),
    subject: optStr(o, "subject") ?? null,
    aliases: arr(o, "aliases", strings),
    comparison: decodeComparison(o["comparison"], `${at}.comparison`),
  };
}

export function decodeCommitSummary(v: unknown, at = "commit"): CommitSummary {
  const o = obj(v, at);
  return { oid: str(o, "oid"), subject: str(o, "subject") };
}

export function decodeFileEntry(v: unknown, at = "entry"): FileEntry {
  const o = obj(v, at);
  const entry: FileEntry = {
    path: str(o, "path"),
    status: lit(o, "status", [
      "added",
      "modified",
      "deleted",
      "renamed",
      "copied",
      "untracked",
      "other",
    ] as const),
  };
  const oldPath = optStr(o, "oldPath");
  if (oldPath !== undefined) entry.oldPath = oldPath;
  const adds = optInt(o, "adds");
  if (adds !== undefined) entry.adds = adds;
  const dels = optInt(o, "dels");
  if (dels !== undefined) entry.dels = dels;
  return entry;
}

export function decodeComment(v: unknown, at = "comment"): Comment {
  const o = obj(v, at);
  const comment: Comment = {
    id: str(o, "id"),
    rev: int(o, "rev"),
    status: lit(o, "status", ["open", "addressed", "resolved", "reopened"] as const),
    path: optStr(o, "path") ?? null,
    line: optLine(o, "line") ?? null,
    body: str(o, "body"),
    author: str(o, "author"),
    createdAt: str(o, "createdAt"),
    updatedAt: str(o, "updatedAt"),
  };
  const side = optLit(o, "side", ["old", "new"] as const);
  if (side !== undefined) comment.side = side;
  const endLine = optLine(o, "endLine");
  if (endLine !== undefined) comment.endLine = endLine;
  const target = optTargetId(o, "target");
  if (target !== null) comment.target = target;
  const snippet = optStr(o, "snippet");
  if (snippet !== undefined) comment.snippet = snippet;
  const response = optStr(o, "response");
  if (response !== undefined) comment.response = response;
  return comment;
}

function decodeRange(v: unknown, at: string): Range {
  const o = obj(v, at);
  return { start: int(o, "start"), end: int(o, "end") };
}

function decodeHlSpan(v: unknown, at: string): HlSpan {
  const o = obj(v, at);
  const span: HlSpan = { start: int(o, "start"), end: int(o, "end") };
  const fg = optStr(o, "fg");
  if (fg !== undefined) span.fg = fg;
  const bold = optBool(o, "bold");
  if (bold !== undefined) span.bold = bold;
  const italic = optBool(o, "italic");
  if (italic !== undefined) span.italic = italic;
  return span;
}

export function decodeCell(v: unknown, at = "cell"): Cell {
  const o = obj(v, at);
  const cell: Cell = {
    kind: lit(o, "kind", ["context", "add", "remove", "empty"] as const),
    text: str(o, "text"),
    wordRanges: optArr(o, "wordRanges", decodeRange) ?? [],
    hl: optArr(o, "hl", decodeHlSpan) ?? [],
  };
  const line = optLine(o, "line");
  if (line !== undefined) cell.line = line;
  return cell;
}

function decodePair(v: unknown, at: string): [number, number] {
  if (!Array.isArray(v) || v.length !== 2 || !v.every((n) => Number.isInteger(n))) {
    throw wrongType(at, "a pair of integers");
  }
  return [v[0] as number, v[1] as number];
}

export function decodeGap(v: unknown, at = "gap"): GapInfo {
  const o = obj(v, at);
  return {
    id: str(o, "id"),
    count: int(o, "count"),
    oldRange: decodePair(o["oldRange"], `${at}.oldRange`),
    newRange: decodePair(o["newRange"], `${at}.newRange`),
  };
}

export function decodeRow(v: unknown, at = "row"): Row {
  const o = obj(v, at);
  const type = lit(o, "type", ["hunkHeader", "gap", "unified", "split"] as const);
  const key = str(o, "key");
  switch (type) {
    case "hunkHeader":
      return { type, key, hunk: int(o, "hunk"), text: str(o, "text") };
    case "gap":
      return { type, key, gap: decodeGap(o["gap"], `${at}.gap`) };
    case "unified":
      return {
        type,
        key,
        hunk: int(o, "hunk"),
        oldNum: optLine(o, "oldNum") ?? null,
        newNum: optLine(o, "newNum") ?? null,
        cell: decodeCell(o["cell"], `${at}.cell`),
        isExpansion: optBool(o, "isExpansion") ?? false,
      };
    case "split":
      return {
        type,
        key,
        hunk: int(o, "hunk"),
        left: decodeCell(o["left"], `${at}.left`),
        right: decodeCell(o["right"], `${at}.right`),
        isExpansion: optBool(o, "isExpansion") ?? false,
      };
  }
}

export function decodeFileView(v: unknown, at = "view"): FileView {
  const o = obj(v, at);
  const kindTag = lit(o, "kind", ["text", "binary", "toolarge"] as const);
  const kind: FileViewKind =
    kindTag === "binary" ? { kind: "binary", desc: str(o, "desc") } : { kind: kindTag };
  const view: FileView = {
    path: str(o, "path"),
    status: lit(o, "status", [
      "added",
      "modified",
      "deleted",
      "renamed",
      "copied",
      "untracked",
      "other",
    ] as const),
    kind,
    mode: lit(o, "mode", ["unified", "split"] as const),
    adds: int(o, "adds"),
    dels: int(o, "dels"),
    oldMissingNewline: bool(o, "oldMissingNewline"),
    newMissingNewline: bool(o, "newMissingNewline"),
    rows: arr(o, "rows", decodeRow),
  };
  const oldPath = optStr(o, "oldPath");
  if (oldPath !== undefined) view.oldPath = oldPath;
  return view;
}

export function decodeRowAnchor(v: unknown, at = "anchor"): RowAnchor {
  const o = obj(v, at);
  return {
    commentId: str(o, "commentId"),
    row: optInt(o, "row") ?? null,
    outdated: bool(o, "outdated"),
    clamped: bool(o, "clamped"),
  };
}

export function decodeAnchoredComment(v: unknown, at = "comments"): AnchoredComment {
  const o = obj(v, at);
  return {
    comment: decodeComment(o["comment"], `${at}.comment`),
    anchor: decodeRowAnchor(o["anchor"], `${at}.anchor`),
    wasPath: optStr(o, "wasPath") ?? null,
  };
}

export function decodeSearchMatch(v: unknown, at = "match"): SearchMatch {
  const o = obj(v, at);
  return {
    row: int(o, "row"),
    cell: lit(o, "cell", ["unified", "left", "right"] as const),
    start: int(o, "start"),
    end: int(o, "end"),
  };
}

export function decodeLineTarget(v: unknown, at = "lineTarget"): LineTarget {
  const o = obj(v, at);
  const kind = lit(o, "kind", ["exact", "inGap", "nearest"] as const);
  switch (kind) {
    case "exact":
      return { kind, row: int(o, "row"), side: lit(o, "side", ["new", "old"] as const) };
    case "inGap":
      return { kind, row: int(o, "row"), gapId: str(o, "gapId") };
    case "nearest":
      return { kind, row: optInt(o, "row") ?? null };
  }
}

export function decodeSkippedPath(v: unknown, at = "skipped"): SkippedPath {
  const o = obj(v, at);
  return { display: str(o, "display"), reason: str(o, "reason") };
}

export function decodeEndpoint(v: unknown, at: string): Endpoint {
  const o = obj(v, at);
  const kind = lit(o, "kind", ["commit", "emptyTree", "index", "worktree"] as const);
  switch (kind) {
    case "commit":
    case "emptyTree":
      return { kind, oid: str(o, "oid") };
    case "index":
    case "worktree":
      return { kind };
  }
}

export function decodeComparison(v: unknown, at = "comparison"): Comparison {
  const o = obj(v, at);
  return { old: decodeEndpoint(o["old"], `${at}.old`), new: decodeEndpoint(o["new"], `${at}.new`) };
}

// --------------------------------------------------------- core JSON values

export type FileFilter = "all" | "annotated" | "unreviewed";
export const FILE_FILTERS: readonly FileFilter[] = ["all", "annotated", "unreviewed"] as const;

export interface FileCounts {
  todo: number;
  total: number;
}

export interface TreeRow {
  depth: number;
  name: string;
  path: string;
  isDir: boolean;
  collapsed: boolean;
  fileIndex?: number;
  commentsTodo: number;
  commentsTotal: number;
}

export interface OverviewCommentOwned {
  comment: Comment;
  unattached: boolean;
  /** The target this comment was made on when that target left the stack. */
  wasOn: string | null;
}

/** The tally of one live target's own comments (the strip). */
export interface TargetCounts {
  id: TargetId;
  counts: FileCounts;
}

export interface ProjectionSnapshot {
  filter: FileFilter;
  files: FileEntry[];
  tree: TreeRow[];
  counts: Record<string, FileCounts>;
  reviewLevelComments: number;
  unattachedComments: number;
  overview: OverviewCommentOwned[];
  selected: TargetId | null;
  targetCounts: TargetCounts[];
  untargetedComments: number;
  wasOnComments: number;
}

export interface ExpansionResult {
  gap: GapInfo;
  at: number;
  rows: Row[];
}

export interface CommandSpec {
  id: string;
  name: string;
  desc: string;
  tui: string[];
  nvim: string[];
  web: string[];
}

export function decodeFileCounts(v: unknown, at = "counts"): FileCounts {
  const o = obj(v, at);
  return { todo: int(o, "todo"), total: int(o, "total") };
}

export function decodeTreeRow(v: unknown, at = "row"): TreeRow {
  const o = obj(v, at);
  const row: TreeRow = {
    depth: int(o, "depth"),
    name: str(o, "name"),
    path: str(o, "path"),
    isDir: bool(o, "isDir"),
    collapsed: bool(o, "collapsed"),
    commentsTodo: optInt(o, "commentsTodo") ?? 0,
    commentsTotal: optInt(o, "commentsTotal") ?? 0,
  };
  const fileIndex = optInt(o, "fileIndex");
  if (fileIndex !== undefined) row.fileIndex = fileIndex;
  return row;
}

export function decodeOverviewComment(v: unknown, at = "overview"): OverviewCommentOwned {
  const o = obj(v, at);
  return {
    comment: decodeComment(o["comment"], `${at}.comment`),
    unattached: bool(o, "unattached"),
    wasOn: optStr(o, "wasOn") ?? null,
  };
}

export function decodeTargetCounts(v: unknown, at = "targetCounts"): TargetCounts {
  const o = obj(v, at);
  return { id: decodeTargetId(o["id"], `${at}.id`), counts: decodeFileCounts(o["counts"], `${at}.counts`) };
}

function decodeCounts(v: unknown, at: string): Record<string, FileCounts> {
  const o = obj(v, at);
  const out: Record<string, FileCounts> = {};
  for (const [key, value] of Object.entries(o)) out[key] = decodeFileCounts(value, `${at}.${key}`);
  return out;
}

export function decodeProjectionSnapshot(v: unknown, at = "projection"): ProjectionSnapshot {
  const o = obj(v, at);
  return {
    filter: lit(o, "filter", FILE_FILTERS),
    files: arr(o, "files", decodeFileEntry),
    tree: arr(o, "tree", decodeTreeRow),
    counts: decodeCounts(o["counts"], `${at}.counts`),
    reviewLevelComments: int(o, "reviewLevelComments"),
    unattachedComments: int(o, "unattachedComments"),
    overview: arr(o, "overview", decodeOverviewComment),
    selected: optTargetId(o, "selected"),
    targetCounts: optArr(o, "targetCounts", decodeTargetCounts) ?? [],
    untargetedComments: optInt(o, "untargetedComments") ?? 0,
    wasOnComments: optInt(o, "wasOnComments") ?? 0,
  };
}

export function decodeExpansionResult(v: unknown, at = "expansion"): ExpansionResult {
  const o = obj(v, at);
  return { gap: decodeGap(o["gap"], `${at}.gap`), at: int(o, "at"), rows: arr(o, "rows", decodeRow) };
}

function decodeStringArray(v: unknown, at: string): string[] {
  if (!Array.isArray(v)) throw wrongType(at, "an array");
  return v.map((x, i) => strings(x, `${at}[${i}]`));
}

export function decodeCommandSpec(v: unknown, at = "command"): CommandSpec {
  const o = obj(v, at);
  return {
    id: str(o, "id"),
    name: str(o, "name"),
    desc: str(o, "desc"),
    tui: decodeStringArray(o["tui"], `${at}.tui`),
    nvim: decodeStringArray(o["nvim"], `${at}.nvim`),
    web: decodeStringArray(o["web"], `${at}.web`),
  };
}

export function decodeCommandTable(v: unknown, at = "commands"): CommandSpec[] {
  if (!Array.isArray(v)) throw wrongType(at, "an array");
  return v.map((x, i) => decodeCommandSpec(x, `${at}[${i}]`));
}

// ------------------------------------------------------------- requests

export interface CommentAddRequest {
  path: string | null;
  side: Side | null;
  line: number | null;
  endLine: number | null;
  body: string;
  author: string | null;
  target: TargetId | null;
}

export interface TargetSelectRequest {
  target: TargetId;
}

export interface CommentEditRequest {
  id: string;
  body: string;
}

export interface CommentDeleteRequest {
  id: string;
}

export interface LifecycleRequest {
  id: string;
  response: string | null;
}

export interface ViewOptionsWire {
  mode: ViewMode;
  wordDiff: boolean;
  theme: ThemeWire;
}

export interface ViewRequest {
  path: string;
  options: ViewOptionsWire;
}

export interface ExpandRequest {
  path: string;
  gapId: string;
  options: ViewOptionsWire;
}

export const DEFAULT_VIEW_OPTIONS: ViewOptionsWire = { mode: "unified", wordDiff: true, theme: "dark" };

export function decodeCommentAdd(v: unknown): CommentAddRequest {
  const o = obj(v);
  const path = optStr(o, "path") ?? null;
  const line = optLine(o, "line") ?? null;
  const endLine = optLine(o, "endLine") ?? null;
  const explicitSide = optLit(o, "side", ["old", "new"] as const);
  const side: Side | null = explicitSide ?? (line !== null ? "new" : null);
  const body = str(o, "body");
  const author = optStr(o, "author") ?? null;
  const target = optTargetId(o, "target");
  return { path, side, line, endLine, body, author, target };
}

export function decodeTargetSelect(v: unknown): TargetSelectRequest {
  const o = obj(v);
  const target = optTargetId(o, "target");
  if (target === null) throw missing("target");
  return { target };
}

export function decodeCommentEdit(v: unknown, idField: string): CommentEditRequest {
  const o = obj(v);
  return { id: str(o, idField), body: str(o, "body") };
}

export function decodeCommentDelete(v: unknown, idField: string): CommentDeleteRequest {
  const o = obj(v);
  return { id: str(o, idField) };
}

export function decodeLifecycle(v: unknown, idField: string): LifecycleRequest {
  const o = obj(v);
  const response = optStr(o, "response");
  return {
    id: str(o, idField),
    response: response === undefined || response.trim() === "" ? null : response,
  };
}

export function decodeViewOptions(v: unknown): ViewOptionsWire {
  const o = obj(v);
  return {
    mode: optLit(o, "mode", ["unified", "split"] as const) ?? DEFAULT_VIEW_OPTIONS.mode,
    wordDiff: optBool(o, "wordDiff") ?? DEFAULT_VIEW_OPTIONS.wordDiff,
    theme: optLit(o, "theme", ["dark", "light", "none"] as const) ?? DEFAULT_VIEW_OPTIONS.theme,
  };
}

export function decodeView(v: unknown): ViewRequest {
  const o = obj(v);
  return { path: str(o, "path"), options: decodeViewOptions(o) };
}

export function decodeExpand(v: unknown): ExpandRequest {
  const o = obj(v);
  return { path: str(o, "path"), gapId: str(o, "gapId"), options: decodeViewOptions(o) };
}

// ------------------------------------------------- client -> server messages

export type ClientRequest =
  | { type: "auth"; token: string }
  | { type: "refresh"; id: number }
  | { type: "getFile"; id: number; path: string }
  | { type: "getSrc"; id: number; path: string }
  | {
      type: "comment.add";
      id: number;
      path?: string;
      side?: Side;
      line?: number;
      endLine?: number;
      body: string;
      author?: string;
      target?: TargetId;
    }
  | { type: "comment.edit"; id: number; commentId: string; body: string }
  | { type: "comment.delete"; id: number; commentId: string }
  | { type: "comment.address"; id: number; commentId: string; response?: string }
  | { type: "comment.resolve"; id: number; commentId: string }
  | { type: "comment.reopen"; id: number; commentId: string }
  | { type: "comment.resolveAddressed"; id: number }
  | { type: "rev.bump"; id: number }
  | { type: "target.select"; id: number; target: TargetId };

export const CLIENT_MESSAGE_TYPES = [
  "auth",
  "refresh",
  "getFile",
  "getSrc",
  "comment.add",
  "comment.edit",
  "comment.delete",
  "comment.address",
  "comment.resolve",
  "comment.reopen",
  "comment.resolveAddressed",
  "rev.bump",
  "target.select",
] as const;

// ------------------------------------------------- server -> client messages

/** How a snapshot's file listing relates to its source: this load's
 * listing, a kept previous one after a failed attempt, or nothing at all.
 * A failed listing is never shown as an empty one. */
export type ListingState = "fresh" | "stale" | "unavailable";
export const LISTING_STATES: readonly ListingState[] = ["fresh", "stale", "unavailable"] as const;

/** Tree placeholder when the listing needs explaining, `null` when the
 * files speak for themselves; mirrors the core's `ListingState::placeholder`
 * (`fixtures/contracts/listing-state.json`). `fileCount` is the whole
 * listing, never a filtered view of it. */
export function listingPlaceholder(state: ListingState, fileCount: number): string | null {
  switch (state) {
    case "fresh":
      return fileCount === 0 ? "no changes" : null;
    case "stale":
      return "showing previous listing";
    case "unavailable":
      return "source unavailable";
  }
}

/** Fields shared by `hello` and `snapshot` (one builder on the server). */
export interface SnapshotFields {
  appVersion: string;
  root: string;
  /** The review file text; the wasm core parses it. */
  review: string;
  files: FileEntry[];
  warnings: string[];
  readOnly: boolean;
  readOnlyReason: string | null;
  sourceError: string | null;
  listing: ListingState;
  skipped: SkippedPath[];
  comparison: Comparison | null;
  /** The stack's targets in order (`[]` outside stack reviews). */
  targets: Target[];
  /** The server process's selected target (`null` outside stacks). */
  selected: TargetId | null;
  /** The reviewed commit of a single-commit review. */
  commit: CommitSummary | null;
  generation: number;
}

export interface HelloMessage extends SnapshotFields {
  type: "hello";
}
export interface SnapshotMessage extends SnapshotFields {
  type: "snapshot";
  id: number;
}
export interface FileMessage {
  type: "file";
  id: number;
  path: string;
  entry: FileEntry;
  /** Raw diff text (absent for an oversized file). */
  raw?: string;
  oldTotalLines?: number;
  tooLarge?: { adds: number; dels: number };
}
export interface SrcMessage {
  type: "src";
  id: number;
  path: string;
  content: string;
}
export interface CommentMessage {
  type: "comment";
  id: number;
  comment: Comment;
  warnings: string[];
}
export interface DeletedMessage {
  type: "deleted";
  id: number;
  commentId: string;
  warnings: string[];
}
export interface RevisionMessage {
  type: "revision";
  id: number;
  revision: number;
  warnings: string[];
}
export interface ResolvedAddressedMessage {
  type: "resolvedAddressed";
  id: number;
  commentIds: string[];
  warnings: string[];
}
export interface ErrorMessage {
  type: "error";
  id: number | null;
  code: string;
  message: string;
}
export interface ReviewChangedMessage {
  type: "reviewChanged";
  review: string;
  warnings: string[];
  readOnly: boolean;
  readOnlyReason: string | null;
  generation: number;
}
export interface DiffChangedMessage {
  type: "diffChanged";
  files: FileEntry[];
  skipped: SkippedPath[];
  sourceError: string | null;
  listing: ListingState;
  comparison: Comparison | null;
  targets: Target[];
  selected: TargetId | null;
  commit: CommitSummary | null;
  generation: number;
}
/** The acknowledgement of a `target.select`; the new listing follows as a
 * `diffChanged` broadcast to every tab, the requesting one included. */
export interface TargetSelectedMessage {
  type: "targetSelected";
  id: number;
  selected: TargetId | null;
  targets: Target[];
  comparison: Comparison | null;
  generation: number;
}

export type ServerMessage =
  | HelloMessage
  | SnapshotMessage
  | FileMessage
  | SrcMessage
  | CommentMessage
  | DeletedMessage
  | RevisionMessage
  | ResolvedAddressedMessage
  | ErrorMessage
  | ReviewChangedMessage
  | DiffChangedMessage
  | TargetSelectedMessage;

/** Responses that answer a request (carry the echoed `id`). */
export type Response = Exclude<ServerMessage, HelloMessage | ReviewChangedMessage | DiffChangedMessage>;

export const SERVER_MESSAGE_TYPES = [
  "hello",
  "snapshot",
  "file",
  "src",
  "comment",
  "deleted",
  "revision",
  "resolvedAddressed",
  "error",
  "reviewChanged",
  "diffChanged",
  "targetSelected",
] as const;

function strings(v: unknown, at: string): string {
  if (typeof v !== "string") throw wrongType(at, "a string");
  return v;
}

function decodeSnapshotFields(o: JsonObject): SnapshotFields {
  const fields: SnapshotFields = {
    appVersion: str(o, "appVersion"),
    root: str(o, "root"),
    review: str(o, "review"),
    files: arr(o, "files", decodeFileEntry),
    warnings: arr(o, "warnings", strings),
    readOnly: bool(o, "readOnly"),
    readOnlyReason: optStr(o, "readOnlyReason") ?? null,
    sourceError: optStr(o, "sourceError") ?? null,
    listing: lit(o, "listing", LISTING_STATES),
    skipped: arr(o, "skipped", decodeSkippedPath),
    comparison: present(o, "comparison") === undefined ? null : decodeComparison(o["comparison"]),
    targets: arr(o, "targets", decodeTarget),
    selected: optTargetId(o, "selected"),
    commit: present(o, "commit") === undefined ? null : decodeCommitSummary(o["commit"]),
    generation: int(o, "generation"),
  };
  return fields;
}

export function decodeServerMessage(v: unknown): ServerMessage {
  const o = obj(v);
  const type = lit(o, "type", SERVER_MESSAGE_TYPES);
  switch (type) {
    case "hello":
      return { type, ...decodeSnapshotFields(o) };
    case "snapshot":
      return { type, id: int(o, "id"), ...decodeSnapshotFields(o) };
    case "file": {
      const msg: FileMessage = {
        type,
        id: int(o, "id"),
        path: str(o, "path"),
        entry: decodeFileEntry(o["entry"]),
      };
      const tooLarge = present(o, "tooLarge");
      if (tooLarge !== undefined) {
        const t = obj(tooLarge, "tooLarge");
        msg.tooLarge = { adds: int(t, "adds"), dels: int(t, "dels") };
      } else {
        msg.raw = str(o, "raw");
        const total = optInt(o, "oldTotalLines");
        if (total !== undefined) msg.oldTotalLines = total;
      }
      return msg;
    }
    case "src":
      return { type, id: int(o, "id"), path: str(o, "path"), content: str(o, "content") };
    case "comment":
      return {
        type,
        id: int(o, "id"),
        comment: decodeComment(o["comment"]),
        warnings: arr(o, "warnings", strings),
      };
    case "deleted":
      return {
        type,
        id: int(o, "id"),
        commentId: str(o, "commentId"),
        warnings: arr(o, "warnings", strings),
      };
    case "revision":
      return {
        type,
        id: int(o, "id"),
        revision: int(o, "revision"),
        warnings: arr(o, "warnings", strings),
      };
    case "resolvedAddressed":
      return {
        type,
        id: int(o, "id"),
        commentIds: arr(o, "commentIds", strings),
        warnings: arr(o, "warnings", strings),
      };
    case "error":
      return {
        type,
        id: optInt(o, "id") ?? null,
        code: optStr(o, "code") ?? "error",
        message: str(o, "message"),
      };
    case "reviewChanged":
      return {
        type,
        review: str(o, "review"),
        warnings: arr(o, "warnings", strings),
        readOnly: bool(o, "readOnly"),
        readOnlyReason: optStr(o, "readOnlyReason") ?? null,
        generation: int(o, "generation"),
      };
    case "diffChanged":
      return {
        type,
        files: arr(o, "files", decodeFileEntry),
        skipped: arr(o, "skipped", decodeSkippedPath),
        sourceError: optStr(o, "sourceError") ?? null,
        listing: lit(o, "listing", LISTING_STATES),
        comparison: present(o, "comparison") === undefined ? null : decodeComparison(o["comparison"]),
        targets: arr(o, "targets", decodeTarget),
        selected: optTargetId(o, "selected"),
        commit: present(o, "commit") === undefined ? null : decodeCommitSummary(o["commit"]),
        generation: int(o, "generation"),
      };
    case "targetSelected":
      return {
        type,
        id: int(o, "id"),
        selected: optTargetId(o, "selected"),
        targets: arr(o, "targets", decodeTarget),
        comparison: present(o, "comparison") === undefined ? null : decodeComparison(o["comparison"]),
        generation: int(o, "generation"),
      };
  }
}

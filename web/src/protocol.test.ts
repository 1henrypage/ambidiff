// The browser decoders against the shared contract fixtures: every request
// case decodes (or fails) exactly as fixtures/contracts specifies, every
// server message fixture decodes to a value that re-serialises to itself,
// and every client envelope belongs to the request union.
import { describe, expect, test } from "bun:test";
import { readFileSync, readdirSync } from "node:fs";
import { join } from "node:path";

import {
  CLIENT_MESSAGE_TYPES,
  DecodeError,
  decodeAnchoredComment,
  decodeComment,
  decodeCommentAdd,
  decodeCommentDelete,
  decodeCommentEdit,
  decodeExpand,
  decodeFileCounts,
  decodeFileView,
  decodeLifecycle,
  decodeRow,
  decodeCommandTable,
  decodeExpansionResult,
  decodeProjectionSnapshot,
  decodeSearchMatch,
  decodeServerMessage,
  decodeTreeRow,
  decodeView,
  decodeViewOptions,
  type ServerMessage,
} from "./protocol";

const DIR = join(import.meta.dirname, "../../fixtures/contracts");
const PROJECTIONS_DIR = join(import.meta.dirname, "../../fixtures/projections");

function projectionFixture(name: string): unknown {
  return JSON.parse(readFileSync(join(PROJECTIONS_DIR, name), "utf8"));
}

function fixture(name: string): unknown {
  return JSON.parse(readFileSync(join(DIR, name), "utf8"));
}

interface RequestCase {
  name: string;
  payload: unknown;
  idField?: string;
  ok?: unknown;
  error?: { kind: string; field: string };
}

function cases(file: string): RequestCase[] {
  const doc = fixture(file) as { cases: RequestCase[] };
  expect(doc.cases.length).toBeGreaterThan(0);
  return doc.cases;
}

function check(file: string, decode: (payload: unknown, idField?: string) => unknown) {
  for (const c of cases(file)) {
    if (c.ok !== undefined) {
      expect(decode(c.payload, c.idField), `${file}: ${c.name}`).toEqual(c.ok);
    } else {
      let caught: unknown;
      try {
        decode(c.payload, c.idField);
      } catch (e) {
        caught = e;
      }
      expect(caught, `${file}: ${c.name}: expected an error`).toBeInstanceOf(DecodeError);
      const err = caught as DecodeError;
      const summary: { kind: string; field: string } = { kind: err.kind, field: err.field };
      expect(summary, `${file}: ${c.name}`).toEqual(c.error ?? { kind: "?", field: "?" });
    }
  }
}

describe("request decoders", () => {
  test("decodes_every_contract_fixture", () => {
    check("request-comment-add.json", (p) => decodeCommentAdd(p));
    check("request-comment-edit.json", (p, id) => decodeCommentEdit(p, id!));
    check("request-comment-delete.json", (p, id) => decodeCommentDelete(p, id!));
    check("request-lifecycle.json", (p, id) => decodeLifecycle(p, id!));
    check("request-view-options.json", (p) => decodeViewOptions(p));
    check("request-view.json", (p) => decodeView(p));
    check("request-expand.json", (p) => decodeExpand(p));
  });

  test("every_web_envelope_is_a_known_client_message", () => {
    const doc = fixture("request-envelopes.json") as { web: { type: string; id?: number }[] };
    for (const msg of doc.web) {
      expect(CLIENT_MESSAGE_TYPES as readonly string[]).toContain(msg.type);
      if (msg.type !== "auth") expect(typeof msg.id).toBe("number");
    }
    // Every request type in the union has an envelope example.
    const seen = new Set(doc.web.map((m) => m.type));
    for (const type of CLIENT_MESSAGE_TYPES) expect(seen.has(type), type).toBe(true);
  });
});

/** Drop `undefined` properties so decoded values compare against raw JSON. */
function plain(v: unknown): unknown {
  return JSON.parse(JSON.stringify(v));
}

/** The core skips empty `wordRanges` / `hl` and false `isExpansion`; the
 *  decoder always materialises them. Fill the fixture the same way. */
function withRowDefaults(row: Record<string, unknown>): Record<string, unknown> {
  const cell = (c: unknown) => ({ wordRanges: [], hl: [], ...(c as Record<string, unknown>) });
  if (row["type"] === "unified") return { isExpansion: false, ...row, cell: cell(row["cell"]) };
  if (row["type"] === "split") {
    return { isExpansion: false, ...row, left: cell(row["left"]), right: cell(row["right"]) };
  }
  return row;
}

describe("server messages", () => {
  const files = readdirSync(DIR).filter((f) => f.startsWith("web-") && f.endsWith(".json"));

  test("every_web_fixture_decodes_and_reserialises_identically", () => {
    expect(files.length).toBeGreaterThan(0);
    for (const file of files) {
      const raw = fixture(file);
      const msg: ServerMessage = decodeServerMessage(raw);
      expect(plain(msg), file).toEqual(raw);
    }
  });

  test("view_fixtures_decode_to_typed_views", () => {
    for (const file of ["stdio-view-text.json", "stdio-view-binary.json", "stdio-view-toolarge.json"]) {
      const doc = fixture(file) as { view: unknown; comments: unknown[] };
      const view = decodeFileView(doc.view);
      // The kind tag is lifted into a discriminated union; the rest is verbatim.
      const { kind, ...rest } = doc.view as Record<string, unknown> & { kind: string };
      const expected: Record<string, unknown> = {
        ...rest,
        kind: { kind },
        rows: (rest["rows"] as Record<string, unknown>[]).map(withRowDefaults),
      };
      if (kind === "binary") {
        expected["kind"] = { kind, desc: rest["desc"] };
        delete expected["desc"];
      }
      expect(plain(view), file).toEqual(expected);
      for (const record of doc.comments) {
        expect(plain(decodeAnchoredComment(record))).toEqual(record);
      }
    }
  });

  test("expansion_rows_decode_with_their_expansion_flag", () => {
    const doc = fixture("stdio-expand.json") as { rows: unknown[] };
    for (const row of doc.rows) {
      const decoded = decodeRow(row);
      expect(decoded.type).toBe("unified");
      if (decoded.type === "unified") expect(decoded.isExpansion).toBe(true);
    }
  });

  test("comment_fixture_round_trips", () => {
    const doc = fixture("stdio-comment-address.json");
    expect(plain(decodeComment(doc))).toEqual(doc);
  });

  test("malformed_messages_fail_with_field_names", () => {
    expect(() => decodeServerMessage({ type: "file", id: 1, path: "a" })).toThrow(DecodeError);
    expect(() => decodeServerMessage({ type: "nope" })).toThrow(/unknown value "nope"/);
    expect(() => decodeServerMessage({ type: "comment", id: 1, comment: { id: "c" }, warnings: [] })).toThrow(
      /"rev"/,
    );
  });

  test("decodes_projection_snapshot", () => {
    const raw = {
      filter: "annotated",
      files: [{ path: "a.ts", status: "modified" }],
      tree: [
        { depth: 0, name: "a.ts", path: "a.ts", isDir: false, collapsed: false, fileIndex: 0, commentsTodo: 1, commentsTotal: 2 },
      ],
      counts: { "a.ts": { todo: 1, total: 2 } },
      reviewLevelComments: 0,
      unattachedComments: 0,
      overview: [],
    };
    expect(decodeProjectionSnapshot(raw)).toEqual(raw as never);
    expect(() => decodeProjectionSnapshot({ ...raw, filter: "nope" })).toThrow(DecodeError);
  });

  test("decodes_expansion_result", () => {
    const raw = {
      gap: { id: "g1", count: 3, oldRange: [1, 4], newRange: [1, 4] },
      at: 2,
      rows: [{ type: "hunkHeader", key: "h0", hunk: 0, text: "@@ -1,4 +1,4 @@" }],
    };
    expect(plain(decodeExpansionResult(raw))).toEqual(raw);
  });

  test("decodes_projection_corpus", () => {
    // filtered-tree: tree rows + per-file counts.
    const filteredTree = projectionFixture("filtered-tree/expected.json") as {
      tree: unknown[];
      countsSrcA: unknown;
      countsReadme: unknown;
      countsSrcB: unknown;
    };
    for (const row of filteredTree.tree) expect(plain(decodeTreeRow(row))).toEqual(row);
    expect(plain(decodeFileCounts(filteredTree.countsSrcA))).toEqual(filteredTree.countsSrcA);
    expect(plain(decodeFileCounts(filteredTree.countsReadme))).toEqual(filteredTree.countsReadme);
    expect(plain(decodeFileCounts(filteredTree.countsSrcB))).toEqual(filteredTree.countsSrcB);

    // placeholder-kinds: full FileView values (binary and too-large). The
    // wire's flat `kind` tag lifts into a discriminated union on decode.
    const placeholders = projectionFixture("placeholder-kinds/expected.json") as { views: unknown[] };
    for (const raw of placeholders.views) {
      const { kind, ...rest } = raw as Record<string, unknown> & { kind: string };
      const expected: Record<string, unknown> = { ...rest, kind: { kind } };
      if (kind === "binary") {
        expected["kind"] = { kind, desc: rest["desc"] };
        delete expected["desc"];
      }
      expect(plain(decodeFileView(raw))).toEqual(expected);
    }

    // unicode-search: SearchMatch values over CJK byte offsets.
    const search = projectionFixture("unicode-search/expected.json") as { matches: unknown[] };
    for (const match of search.matches) expect(plain(decodeSearchMatch(match))).toEqual(match);

    // rename-unattached: the comment half of each overview record (the
    // fixture's own shape wraps it as {commentId, unattached}, not the wire
    // {comment, unattached} shape, so only the decodable comment fixtures
    // elsewhere cover its "comment" side; unattached/wasPath are asserted
    // directly by crates/core/tests/projections.rs).
    const renameUnattached = projectionFixture("rename-unattached/expected.json") as {
      overview: { unattached: boolean }[];
    };
    for (const o of renameUnattached.overview) expect(typeof o.unattached).toBe("boolean");
  });

  test("decodes_command_table", () => {
    const raw = [{ id: "ambidiff.nav.cursorDown", name: "Cursor down", desc: "d", tui: ["j"], nvim: ["j"], web: ["j"] }];
    expect(decodeCommandTable(raw)).toEqual(raw);
    expect(() => decodeCommandTable({})).toThrow(DecodeError);
  });
});

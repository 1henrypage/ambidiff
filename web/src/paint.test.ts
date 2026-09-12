import { describe, expect, test } from "bun:test";

import { HeightIndex, buildDisplay, composeSegments, gutterDigits, lineKey } from "./paint";
import type { AnchoredComment, Cell, Comment, FileView, Row } from "./protocol";

const bytes = (s: string) => new TextEncoder().encode(s).length;

function cell(text: string, extra: Partial<Cell> = {}): Cell {
  return { kind: "context", line: 1, text, wordRanges: [], hl: [], ...extra };
}

function joined(text: string, hits: { start: number; end: number }[] = [], extra: Partial<Cell> = {}) {
  return composeSegments(cell(text, extra), hits).map((s) => s.text).join("");
}

describe("composeSegments", () => {
  test("paint_cell_preserves_unicode", () => {
    for (const text of ["é", "café", "foo 😀 tail", "你好世界", "é combining", "a‍b"]) {
      // Boundaries at every byte offset the core could emit: 0..len.
      const len = bytes(text);
      const hl = [];
      for (let i = 0; i < len; i++) hl.push({ start: i, end: i + 1 });
      expect(joined(text, [], { hl })).toBe(text);
      expect(joined(text)).toBe(text);
    }
  });

  test("boundaries_snap_to_char_boundaries", () => {
    // "é" is 2 bytes; a boundary inside it snaps down so no U+FFFD appears.
    const segments = composeSegments(cell("aéb", { hl: [{ start: 2, end: 3, fg: "#123456" }] }), []);
    expect(segments.map((s) => s.text).join("")).toBe("aéb");
    expect(segments.every((s) => !s.text.includes("�"))).toBe(true);
    // Snapping puts the whole "é" in the run starting at byte 1.
    const runs = segments.map((s) => s.text);
    expect(runs).toEqual(["a", "é", "b"]);
  });

  test("bounds_beyond_byte_length_are_clamped", () => {
    const text = "short";
    const segments = composeSegments(
      cell(text, { wordRanges: [{ start: 2, end: 999 }], hl: [{ start: 100, end: 200 }] }),
      [{ start: 3, end: 5000 }],
    );
    expect(segments.map((s) => s.text).join("")).toBe(text);
    expect(segments.find((s) => s.text === "sh")?.inWord).toBe(false);
    expect(segments.find((s) => s.text === "o")?.inWord).toBe(true);
    expect(segments.find((s) => s.text === "rt")?.inHit).toBe(true);
  });

  test("tabs_and_metacharacters_survive", () => {
    const text = "\ta<b>&amp;\t\"q\" \\";
    expect(joined(text)).toBe(text);
    const segs = composeSegments(cell(text, { hl: [{ start: 1, end: 4, fg: "#ff0000", bold: true }] }), []);
    expect(segs.map((s) => s.text).join("")).toBe(text);
    expect(segs[1]?.fg).toBe("#ff0000");
    expect(segs[1]?.bold).toBe(true);
  });

  test("word_and_hit_flags_follow_byte_ranges", () => {
    const text = "let x = foo(1)";
    const segs = composeSegments(cell(text, { kind: "add", wordRanges: [{ start: 8, end: 11 }] }), [
      { start: 4, end: 5 },
    ]);
    expect(segs.map((s) => s.text)).toEqual(["let ", "x", " = ", "foo", "(1)"]);
    expect(segs.map((s) => s.inHit)).toEqual([false, true, false, false, false]);
    expect(segs.map((s) => s.inWord)).toEqual([false, false, false, true, false]);
  });

  test("empty_text_yields_one_empty_segment", () => {
    expect(composeSegments(cell(""), [])).toEqual([
      { text: "", fg: null, bold: false, italic: false, inWord: false, inHit: false },
    ]);
  });
});

function unified(key: string, oldNum: number | null, newNum: number | null, text: string): Row {
  return {
    type: "unified",
    key,
    hunk: 0,
    oldNum,
    newNum,
    cell: { kind: "context", line: newNum ?? oldNum ?? 1, text, wordRanges: [], hl: [] },
    isExpansion: false,
  };
}

function comment(id: string, line: number | null): Comment {
  return {
    id,
    rev: 1,
    status: "open",
    path: "f",
    line,
    body: "b1\nb2",
    author: "a",
    createdAt: "t",
    updatedAt: "t",
  };
}

function view(rows: Row[]): FileView {
  return {
    path: "f",
    status: "modified",
    kind: { kind: "text" },
    mode: "unified",
    adds: 0,
    dels: 0,
    oldMissingNewline: false,
    newMissingNewline: false,
    rows,
  };
}

describe("display model", () => {
  test("gutter_digits_use_the_widest_number_with_a_minimum_of_three", () => {
    expect(gutterDigits([])).toBe(3);
    expect(gutterDigits([unified("k", 12345, 3, "x")])).toBe(5);
    const split: Row = {
      type: "split",
      key: "s",
      hunk: 0,
      left: cell("l", { line: 7 }),
      right: cell("r", { line: 1000 }),
      isExpansion: false,
    };
    expect(gutterDigits([split])).toBe(4);
  });

  test("build_display_places_rowless_comments_in_file_group", () => {
    const rows = [unified("a", 1, 1, "one"), unified("b", 2, 2, "two")];
    const anchored: AnchoredComment[] = [
      { comment: comment("c-file", null), anchor: { commentId: "c-file", row: null, outdated: false, clamped: false }, wasPath: null },
      { comment: comment("c-gone", 9), anchor: { commentId: "c-gone", row: null, outdated: false, clamped: true }, wasPath: "old" },
      { comment: comment("c-line", 2), anchor: { commentId: "c-line", row: 1, outdated: false, clamped: false }, wasPath: null },
    ];
    const display = buildDisplay({ kind: "file", view: view(rows), comments: anchored });
    const kinds = display.map((d) => (d.kind === "chead" ? `head:${d.record.comment.id}` : d.kind));
    expect(kinds).toEqual([
      "head:c-file",
      "cline",
      "cline",
      "cfoot",
      "head:c-gone",
      "cline",
      "cline",
      "cfoot",
      "row",
      "row",
      "head:c-line",
      "cline",
      "cline",
      "cfoot",
    ]);
    expect(lineKey(display[0]!, view(rows))).toBe("c:c-file");
    expect(lineKey(display[8]!, view(rows))).toBe("r:a");
  });

  test("placeholders_get_a_notice_and_keep_their_comments", () => {
    const big = { ...view([]), kind: { kind: "toolarge" as const }, adds: 12000, dels: 20 };
    const anchored: AnchoredComment[] = [
      { comment: comment("c-1", 5), anchor: { commentId: "c-1", row: null, outdated: false, clamped: true }, wasPath: null },
    ];
    const display = buildDisplay({ kind: "file", view: big, comments: anchored });
    expect(display[0]).toEqual({ kind: "notice", text: "diff too large to render (+12000 -20 lines); skipped" });
    expect(display[1]?.kind).toBe("chead");
    const binary = { ...view([]), kind: { kind: "binary" as const, desc: "(binary file)" } };
    expect(buildDisplay({ kind: "file", view: binary, comments: [] })[0]).toEqual({ kind: "notice", text: "(binary file)" });
  });

  test("overview_groups_unattached_after_a_section_and_shows_snippets", () => {
    const display = buildDisplay({
      kind: "overview",
      comments: [
        { comment: { ...comment("c-r", null), path: null }, anchor: null, wasPath: null, unattached: false },
        { comment: { ...comment("c-u", 3), snippet: "gone()" }, anchor: null, wasPath: null, unattached: true },
      ],
    });
    const kinds = display.map((d) => d.kind);
    expect(kinds).toEqual(["chead", "cline", "cline", "cfoot", "blank", "section", "chead", "cline", "cline", "cline", "cfoot"]);
    expect(display[9]).toEqual(expect.objectContaining({ kind: "cline", text: "snippet: gone()" }));
    expect(buildDisplay({ kind: "overview", comments: [] })[0]?.kind).toBe("notice");
  });

  test("only_the_response_line_carries_the_response_role", () => {
    const display = buildDisplay({
      kind: "overview",
      comments: [
        { comment: { ...comment("c-r", null), path: null, response: "all done" }, anchor: null, wasPath: null, unattached: false },
      ],
    });
    const clines = display.filter((d): d is Extract<typeof d, { kind: "cline" }> => d.kind === "cline");
    expect(clines).toHaveLength(3); // "b1", "b2", the response
    expect(clines.filter((c) => c.role === "response")).toHaveLength(1);
    expect(clines.find((c) => c.role === "response")?.text).toBe("↳ all done");
    expect(clines.filter((c) => c.role !== "response").every((c) => c.role === undefined)).toBe(true);
  });

  test("a_single_line_body_never_splits_regardless_of_length", () => {
    // The layout engine, not paint.ts, owns wrapping (the drift firewall):
    // however long a body line is, buildDisplay must still emit it as
    // exactly one `cline`.
    const longBody = "x".repeat(5000);
    const display = buildDisplay({
      kind: "overview",
      comments: [
        { comment: { ...comment("c-long", null), path: null, body: longBody }, anchor: null, wasPath: null, unattached: false },
      ],
    });
    const clines = display.filter((d) => d.kind === "cline");
    expect(clines).toHaveLength(1);
    expect((clines[0] as { text: string }).text).toBe(longBody);
  });
});

describe("HeightIndex", () => {
  test("height_index_prefix_and_search", () => {
    const index = new HeightIndex(5, 20);
    expect(index.total()).toBe(100);
    expect(index.prefix(0)).toBe(0);
    expect(index.prefix(3)).toBe(60);
    index.set(1, 50); // row 1 wraps to 50px
    expect(index.total()).toBe(130);
    expect(index.prefix(2)).toBe(70);
    expect(index.indexAt(0)).toBe(0);
    expect(index.indexAt(19)).toBe(0);
    expect(index.indexAt(20)).toBe(1);
    expect(index.indexAt(69)).toBe(1);
    expect(index.indexAt(70)).toBe(2);
    expect(index.indexAt(10_000)).toBe(4);
    index.set(1, 20);
    expect(index.total()).toBe(100);
    expect(new HeightIndex(0, 20).indexAt(5)).toBe(0);
    expect(new HeightIndex(0, 20).total()).toBe(0);
  });

  test("height_index_matches_a_naive_prefix_sum", () => {
    const n = 37;
    const index = new HeightIndex(n, 20);
    const heights = new Array<number>(n).fill(20);
    let seed = 7;
    for (let k = 0; k < 200; k++) {
      seed = (seed * 1103515245 + 12345) & 0x7fffffff;
      const i = seed % n;
      const h = 20 + (seed % 5) * 20;
      heights[i] = h;
      index.set(i, h);
    }
    let sum = 0;
    for (let i = 0; i < n; i++) {
      expect(index.prefix(i)).toBe(sum);
      expect(index.indexAt(sum)).toBe(i);
      expect(index.indexAt(sum + (heights[i] ?? 0) - 1)).toBe(i);
      sum += heights[i] ?? 0;
    }
    expect(index.total()).toBe(sum);
  });
});

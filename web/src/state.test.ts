// State-flow tests: navigation ownership (B14), the pane-only-replaced-on-
// success invariant (B19), placeholders and expansion (B10, B16), and
// write guards -- all against fakes, no DOM and no real wasm.
import { describe, expect, test } from "bun:test";

import type { CommentMessage, FileEntry, FileMessage, HelloMessage, SrcMessage } from "./protocol";
import { Store, WriteBlocked } from "./state";
import { FakeCore, emptyView } from "./testing/fakeCore";
import { FakeTransport } from "./testing/fakeTransport";
import { TransportError } from "./transport";

const tick = () => new Promise((r) => setTimeout(r, 0));

function hello(files: FileEntry[]): HelloMessage {
  return {
    type: "hello",
    appVersion: "0",
    root: "/repo",
    review: "review text",
    files,
    warnings: [],
    readOnly: false,
    readOnlyReason: null,
    sourceError: null,
    skipped: [],
    comparison: null,
    generation: 0,
  };
}

function fileMsg(path: string): FileMessage {
  return { type: "file", id: 0, path, entry: { path, status: "modified" }, raw: `raw:${path}` };
}

function harness(files: FileEntry[]) {
  const core = new FakeCore();
  const transport = new FakeTransport();
  transport.nextHello = hello(files);
  const store = new Store(core, transport);
  return { core, transport, store };
}

async function bootedOn(path: string) {
  const files: FileEntry[] = [{ path, status: "modified" }];
  const h = harness(files);
  const connectPromise = h.store.connect("tok");
  await connectPromise;
  h.transport.answer("getFile", path, fileMsg(path));
  await tick();
  return h;
}

describe("navigation ownership (B14)", () => {
  test("late_response_after_overview_is_ignored", async () => {
    const files: FileEntry[] = [
      { path: "a.ts", status: "modified" },
      { path: "b.ts", status: "modified" },
    ];
    const { store, transport } = harness(files);
    await store.connect("tok");
    // boot auto-opened a.ts; a getFile is in flight.
    expect(transport.find("getFile", "a.ts")).toBeDefined();

    store.openOverview();
    expect(store.nav).toEqual({ kind: "overview" });

    transport.answer("getFile", "a.ts", fileMsg("a.ts"));
    await tick();

    expect(store.nav).toEqual({ kind: "overview" });
    expect(store.pane?.kind).toBe("overview");
  });

  test("cross_path_late_response_is_ignored", async () => {
    const files: FileEntry[] = [
      { path: "a.ts", status: "modified" },
      { path: "b.ts", status: "modified" },
    ];
    const { store, transport } = harness(files);
    await store.connect("tok");
    store.openFile("b.ts", false);
    expect(transport.find("getFile", "b.ts")).toBeDefined();

    // The stale a.ts response must not clobber the newer navigation to b.ts.
    transport.answer("getFile", "a.ts", fileMsg("a.ts"));
    await tick();
    expect(store.nav).toEqual({ kind: "file", path: "b.ts" });

    transport.answer("getFile", "b.ts", fileMsg("b.ts"));
    await tick();
    expect(store.pane?.kind).toBe("file");
    expect(store.pane?.kind === "file" && store.pane.view.path).toBe("b.ts");
  });

  test("failed_open_reverts_nav_and_keeps_pane", async () => {
    const { store, transport } = await bootedOn("a.ts").then((h) => h);
    expect(store.nav).toEqual({ kind: "file", path: "a.ts" });
    const paneBefore = store.pane;

    store.openFile("b.ts", false);
    transport.fail("getFile", "b.ts", new TransportError("error", "boom", "store"));
    await tick();

    expect(store.nav).toEqual({ kind: "file", path: "a.ts" });
    expect(store.pane).toBe(paneBefore);
    expect(store.flash).toContain("boom");
  });
});

describe("diff and review reconciliation", () => {
  test("vanished_file_switches_to_overview", async () => {
    const { store } = await bootedOn("a.ts");
    store.onDiffChanged({ files: [{ path: "b.ts", status: "modified" }], skipped: [], sourceError: null });
    expect(store.nav).toEqual({ kind: "overview" });
    expect(store.flash).toContain("no longer in this diff");
  });

  test("renamed_file_follows_old_path", async () => {
    const { store, transport } = await bootedOn("a.ts");
    store.onDiffChanged({
      files: [{ path: "a2.ts", oldPath: "a.ts", status: "renamed" }],
      skipped: [],
      sourceError: null,
    });
    expect(store.nav).toEqual({ kind: "file", path: "a2.ts" });
    transport.answer("getFile", "a2.ts", fileMsg("a2.ts"));
    await tick();
    expect(store.pane?.kind === "file" && store.pane.view.path).toBe("a2.ts");
  });
});

describe("expansion (B10)", () => {
  test("expand_result_paints_without_reload", async () => {
    const { store, core, transport } = await bootedOn("a.ts");
    const expandedView = emptyView("a.ts");
    core.nextExpand = { expansion: { gap: { id: "g1", count: 3, oldRange: [1, 3], newRange: [1, 3] }, at: 0, rows: [] }, view: expandedView, comments: [] };

    store.expandGap("g1");
    const srcReq = transport.find("getSrc", "a.ts");
    expect(srcReq).toBeDefined();
    const before = transport.requests.length;
    transport.answer("getSrc", "a.ts", { type: "src", id: 0, path: "a.ts", content: "full file text" } as SrcMessage);
    await tick();

    expect(store.pane?.kind === "file" && store.pane.view).toBe(expandedView);
    // no follow-up getFile: the caller repaints straight from the expand result.
    expect(transport.requests.length).toBeLessThanOrEqual(before);
    expect(transport.find("getFile")).toBeUndefined();
  });

  test("stale_src_after_generation_change_is_dropped", async () => {
    const { store, core, transport } = await bootedOn("a.ts");
    const wouldInstall = emptyView("would-not-apply");
    core.nextExpand = { expansion: { gap: { id: "g1", count: 1, oldRange: [1, 1], newRange: [1, 1] }, at: 0, rows: [] }, view: wouldInstall, comments: [] };

    store.expandGap("g1");
    // The diff changes underneath the in-flight getSrc (bumps core.generation).
    store.onDiffChanged({ files: [{ path: "a.ts", status: "modified" }], skipped: [], sourceError: null });
    transport.answer("getFile", "a.ts", fileMsg("a.ts"));
    await tick();

    transport.answer("getSrc", "a.ts", { type: "src", id: 0, path: "a.ts", content: "stale" } as SrcMessage);
    await tick();

    expect(store.pane?.kind === "file" && store.pane.view).not.toBe(wouldInstall);
  });

  test("diff_change_restores_pending_expansions", async () => {
    const { store, core, transport } = await bootedOn("a.ts");
    core.gaps.set("a.ts", [{ id: "g1", count: 2, oldRange: [1, 2], newRange: [1, 2] }]);
    const restored = emptyView("a.ts");
    core.nextExpand = { expansion: { gap: { id: "g1", count: 2, oldRange: [1, 2], newRange: [1, 2] }, at: 0, rows: [] }, view: restored, comments: [] };

    store.onDiffChanged({ files: [{ path: "a.ts", status: "modified" }], skipped: [], sourceError: null });
    transport.answer("getFile", "a.ts", fileMsg("a.ts"));
    await tick();
    transport.answer("getSrc", "a.ts", { type: "src", id: 0, path: "a.ts", content: "full" } as SrcMessage);
    await tick();

    expect(store.pane?.kind === "file" && store.pane.view).toBe(restored);
  });
});

describe("placeholders (B16)", () => {
  test("oversized_file_uses_placeholder_and_keeps_comments", async () => {
    const files: FileEntry[] = [{ path: "big.js", status: "modified" }];
    const { store, core, transport } = harness(files);
    core.commentsByPath.set("big.js", [
      {
        comment: {
          id: "c-1",
          rev: 1,
          status: "open",
          path: "big.js",
          line: null,
          body: "huge",
          author: "h",
          createdAt: "t",
          updatedAt: "t",
        },
        anchor: { commentId: "c-1", row: null, outdated: false, clamped: true },
        wasPath: null,
      },
    ]);
    await store.connect("tok");
    transport.answer("getFile", "big.js", {
      type: "file",
      id: 0,
      path: "big.js",
      entry: { path: "big.js", status: "modified" },
      tooLarge: { adds: 12000, dels: 20 },
    });
    await tick();

    expect(store.pane?.kind === "file" && store.pane.view.kind.kind).toBe("toolarge");
    expect(store.pane?.kind === "file" && store.pane.comments).toHaveLength(1);
  });
});

describe("review broadcasts (B21)", () => {
  test("review_error_keeps_last_summary", async () => {
    const { store, core } = await bootedOn("a.ts");
    const before = store.summary;
    core.nextReviewError = "salvage failed";
    store.onReviewChanged({ review: "garbage", warnings: [], readOnly: false, readOnlyReason: null });
    expect(store.summary).toBe(before);
    expect(store.diagnostics.reviewError).toBe("salvage failed");
  });
});

describe("writes", () => {
  test("write_blocked_when_disconnected_or_read_only", async () => {
    const { store } = harness([]);
    await expect(store.addComment("hi")).rejects.toBeInstanceOf(WriteBlocked);

    const { store: store2, transport } = await bootedOn("a.ts");
    transport.state = "connected";
    store2.diagnostics.readOnly = true;
    await expect(store2.addComment("hi")).rejects.toBeInstanceOf(WriteBlocked);
  });

  test("address_with_empty_response_omits_field", async () => {
    const { store, transport } = await bootedOn("a.ts");
    void store.address("c-1", "");
    const req1 = transport.find("comment.address");
    expect(req1).toBeDefined();
    expect("response" in (req1?.msg ?? {})).toBe(false);
    transport.answer("comment.address", undefined, {
      type: "comment",
      id: 0,
      comment: {
        id: "c-1",
        rev: 1,
        status: "addressed",
        path: null,
        line: null,
        body: "b",
        author: "h",
        createdAt: "t",
        updatedAt: "t",
      },
      warnings: [],
    } as CommentMessage);

    void store.address("c-1", "restored the guard");
    const req2 = transport.find("comment.address");
    expect(req2 && (req2.msg as { response?: string }).response).toBe("restored the guard");
  });
});

describe("cursor", () => {
  test("keyboard_move_resets_active_cell", async () => {
    const { store } = await bootedOn("a.ts");
    store.activeCell = "left";
    store.moveCursor(1);
    const after: string = store.activeCell;
    expect(after).toBe("auto");
  });
});

describe("filter (B15)", () => {
  test("annotated_filter_includes_file_after_first_comment", async () => {
    const { store, core } = await bootedOn("a.ts");
    store.cycleFilter(); // all -> annotated
    expect(store.projection?.files.map((f) => f.path)).toEqual([]);

    core.commentsByPath.set("a.ts", [
      {
        comment: {
          id: "c-1",
          rev: 1,
          status: "open",
          path: "a.ts",
          line: null,
          body: "please look",
          author: "h",
          createdAt: "t",
          updatedAt: "t",
        },
        anchor: { commentId: "c-1", row: null, outdated: false, clamped: false },
        wasPath: null,
      },
    ]);
    // The broadcast alone (no getFile, no manual refresh) must reproject.
    store.onReviewChanged({ review: "r", warnings: [], readOnly: false, readOnlyReason: null });

    expect(store.projection?.files.map((f) => f.path)).toEqual(["a.ts"]);
  });
});

describe("refresh (B22)", () => {
  test("refresh_applies_snapshot_and_reloads_file", async () => {
    const { store, transport } = await bootedOn("a.ts");

    const refreshDone = store.refresh();
    const refreshReq = transport.find("refresh");
    expect(refreshReq).toBeDefined();
    transport.answer("refresh", undefined, {
      type: "snapshot",
      id: 0,
      appVersion: "0",
      root: "/repo",
      review: "review text",
      files: [{ path: "a.ts", status: "modified" }],
      warnings: [],
      readOnly: false,
      readOnlyReason: null,
      sourceError: null,
      skipped: [],
      comparison: null,
      generation: 0,
    });
    await refreshDone;

    // applySnapshot's reconcileNav reopens the currently-viewed file.
    expect(transport.find("getFile", "a.ts")).toBeDefined();
    transport.answer("getFile", "a.ts", fileMsg("a.ts"));
    await tick();
    expect(store.pane?.kind).toBe("file");
    expect(store.nav).toEqual({ kind: "file", path: "a.ts" });
  });
});

describe("review broadcasts (B21), exact naming", () => {
  test("empty_review_string_keeps_last_summary", async () => {
    const { store, core } = await bootedOn("a.ts");
    const before = store.summary;
    core.nextReviewError = "empty or unparsable review string";
    store.onReviewChanged({ review: "", warnings: [], readOnly: false, readOnlyReason: null });
    expect(store.summary).toBe(before);
    expect(store.diagnostics.reviewError).toBe("empty or unparsable review string");
  });
});

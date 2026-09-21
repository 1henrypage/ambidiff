// State-flow tests: navigation ownership (B14), the pane-only-replaced-on-
// success invariant (B19), placeholders and expansion (B10, B16), and
// write guards -- all against fakes, no DOM and no real wasm.
import { describe, expect, test } from "bun:test";

import type { CommentMessage, FileEntry, FileMessage, HelloMessage, SrcMessage, Target } from "./protocol";
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
      listing: "fresh",
    skipped: [],
    comparison: null,
    targets: [],
    selected: null,
    commit: null,
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
    store.onDiffChanged({ files: [{ path: "b.ts", status: "modified" }], skipped: [], sourceError: null, listing: "fresh", targets: [], selected: null, commit: null });
    expect(store.nav).toEqual({ kind: "overview" });
    expect(store.flash).toContain("no longer in this diff");
  });

  test("renamed_file_follows_old_path", async () => {
    const { store, transport } = await bootedOn("a.ts");
    store.onDiffChanged({
      files: [{ path: "a2.ts", oldPath: "a.ts", status: "renamed" }],
      skipped: [],
      sourceError: null,
      listing: "fresh",
      targets: [],
      selected: null,
      commit: null,
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
    store.onDiffChanged({ files: [{ path: "a.ts", status: "modified" }], skipped: [], sourceError: null, listing: "fresh", targets: [], selected: null, commit: null });
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

    store.onDiffChanged({ files: [{ path: "a.ts", status: "modified" }], skipped: [], sourceError: null, listing: "fresh", targets: [], selected: null, commit: null });
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

  test("resolveAddressed_sends_the_right_request_and_returns_the_ids", async () => {
    const { store, transport } = await bootedOn("a.ts");
    const promise = store.resolveAddressed();
    const req = transport.find("comment.resolveAddressed");
    expect(req).toBeDefined();
    expect(req?.msg).toEqual({ type: "comment.resolveAddressed" });
    transport.answer("comment.resolveAddressed", undefined, {
      type: "resolvedAddressed",
      id: 0,
      commentIds: ["c-1", "c-2"],
      warnings: [],
    });
    await expect(promise).resolves.toEqual(["c-1", "c-2"]);
  });

  test("resolveAddressed_is_blocked_when_disconnected_or_read_only", async () => {
    const { store } = harness([]);
    await expect(store.resolveAddressed()).rejects.toBeInstanceOf(WriteBlocked);

    const { store: store2 } = await bootedOn("a.ts");
    store2.diagnostics.readOnly = true;
    await expect(store2.resolveAddressed()).rejects.toBeInstanceOf(WriteBlocked);
  });
});

describe("addressed count", () => {
  test("addressedCount_reads_the_review_summary", async () => {
    const { store, core } = await bootedOn("a.ts");
    expect(store.addressedCount()).toBe(0);
    core.summary = { ...core.summary, counts: { ...core.summary.counts, addressed: 3 } };
    store.onReviewChanged({ review: "r", warnings: [], readOnly: false, readOnlyReason: null });
    expect(store.addressedCount()).toBe(3);
  });
});

describe("delete confirmation", () => {
  test("delegates_to_core_for_each_status", () => {
    const { store } = harness([]);
    expect(store.deleteNeedsConfirm("open")).toBe(true);
    expect(store.deleteNeedsConfirm("addressed")).toBe(true);
    expect(store.deleteNeedsConfirm("reopened")).toBe(true);
    expect(store.deleteNeedsConfirm("resolved")).toBe(false);
  });

  test("fails_safe_toward_asking_when_core_throws", () => {
    const { store, core } = harness([]);
    core.deleteNeedsConfirmThrows = true;
    expect(store.deleteNeedsConfirm("resolved")).toBe(true);
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

describe("count prefix", () => {
  test("digits_accumulate_and_take_count_reads_then_clears", async () => {
    const { store } = await bootedOn("a.ts");
    store.pushCountDigit(1);
    store.pushCountDigit(0);
    expect(store.pendingCount).toBe(10);
    expect(store.takeCount()).toBe(10);
    expect(store.pendingCount).toBeNull();
  });

  test("clear_count_resets_to_null", async () => {
    const { store } = await bootedOn("a.ts");
    store.pushCountDigit(5);
    store.clearCount();
    expect(store.pendingCount).toBeNull();
  });
});

describe("goto line", () => {
  test("exact_target_moves_the_cursor_to_that_row", async () => {
    const { store, core } = await bootedOn("a.ts");
    core.gotoResult = { kind: "exact", row: 1, side: "new" };
    store.gotoLine(2);
    expect(store.cursor).toBe(1);
    expect(store.flash).toBeNull();
    expect(core.lastGoto).toEqual({ path: "a.ts", line: 2 });
  });

  test("in_gap_target_moves_onto_the_gap_row_and_flashes", async () => {
    const { store, core } = await bootedOn("a.ts");
    core.gotoResult = { kind: "inGap", row: 0, gapId: "before:0" };
    store.gotoLine(3);
    expect(store.cursor).toBe(0);
    expect(store.flash).toContain("collapsed");
  });

  test("nearest_target_moves_to_the_fallback_row_and_flashes", async () => {
    const { store, core } = await bootedOn("a.ts");
    core.gotoResult = { kind: "nearest", row: 1 };
    store.gotoLine(999);
    expect(store.cursor).toBe(1);
    expect(store.flash).toContain("not in this diff");
  });

  test("nearest_with_no_row_only_flashes", async () => {
    const { store, core } = await bootedOn("a.ts");
    const before = store.cursor;
    core.gotoResult = { kind: "nearest", row: null };
    store.gotoLine(999);
    expect(store.cursor).toBe(before);
    expect(store.flash).toContain("not in this diff");
  });

  test("goto_line_on_the_overview_flashes_instead_of_jumping", async () => {
    const { store } = await bootedOn("a.ts");
    store.openOverview();
    store.gotoLine(1);
    expect(store.flash).toBe("open a file first");
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
      listing: "fresh",
      skipped: [],
      comparison: null,
      targets: [],
      selected: null,
      commit: null,
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

// ------------------------------------------------------------ stack targets

function target(name: string, position: number): Target {
  return {
    id: { kind: "branch", name },
    label: name,
    position,
    tip: `${position}`.repeat(40),
    commitCount: 1,
    subject: `subject ${name}`,
    aliases: [],
    comparison: { old: { kind: "commit", oid: "1".repeat(40) }, new: { kind: "commit", oid: "2".repeat(40) } },
  };
}

function stackTargets(): Target[] {
  return [
    target("auth-1", 1),
    target("auth-2", 2),
    {
      id: { kind: "stack" },
      label: "stack",
      position: null,
      tip: "3".repeat(40),
      commitCount: 2,
      subject: null,
      aliases: [],
      comparison: { old: { kind: "commit", oid: "1".repeat(40) }, new: { kind: "commit", oid: "3".repeat(40) } },
    },
  ];
}

async function bootedStack() {
  const files: FileEntry[] = [{ path: "src/b.ts", status: "modified" }];
  const core = new FakeCore();
  const transport = new FakeTransport();
  transport.nextHello = {
    ...hello(files),
    targets: stackTargets(),
    selected: { kind: "branch", name: "auth-2" },
  };
  const store = new Store(core, transport);
  await store.connect("tok");
  transport.answer("getFile", "src/b.ts", fileMsg("src/b.ts"));
  await tick();
  return { core, transport, store };
}

describe("stack targets", () => {
  test("hello_scopes_the_core_to_the_stack_before_projecting", async () => {
    const { store, core } = await bootedStack();
    expect(store.isStack()).toBe(true);
    expect(store.selectedLabel()).toBe("auth-2");
    expect(store.targetIndex()).toBe(1);
    const scope = core.targetScopes.at(-1);
    expect(scope?.selected).toEqual({ kind: "branch", name: "auth-2" });
    expect(scope?.targets.map((t) => (t.kind === "branch" ? t.name : t.kind))).toEqual(["auth-1", "auth-2", "stack"]);
    expect(store.projection?.selected).toEqual({ kind: "branch", name: "auth-2" });
  });

  test("a_plain_review_is_not_a_stack_and_never_scopes", async () => {
    const { store, core } = await bootedOn("a.ts");
    expect(store.isStack()).toBe(false);
    expect(core.targetScopes.at(-1)).toEqual({ targets: [], selected: null });
    store.stepTarget(1);
    expect(store.flash).toBe("not a stack review");
  });

  test("select_target_acks_then_the_diff_broadcast_reloads_the_files", async () => {
    const { store, transport, core } = await bootedStack();
    store.selectTarget({ kind: "branch", name: "auth-1" });
    const req = transport.find("target.select");
    expect(req).toBeDefined();
    expect(req?.msg).toEqual({ type: "target.select", target: { kind: "branch", name: "auth-1" } });
    // No file is fetched on the request alone.
    expect(transport.find("getFile")).toBeUndefined();

    transport.answer("target.select", undefined, {
      type: "targetSelected",
      id: 0,
      selected: { kind: "branch", name: "auth-1" },
      targets: stackTargets(),
      comparison: null,
      generation: 2,
    });
    await tick();
    expect(store.selectedLabel()).toBe("auth-1");
    expect(store.flash).toBe("target: auth-1");
    expect(core.targetScopes.at(-1)?.selected).toEqual({ kind: "branch", name: "auth-1" });
    expect(transport.find("getFile")).toBeUndefined();

    // The listing arrives as every tab's diffChanged; the old file is gone
    // and the new target's first file opens.
    store.onDiffChanged({
      files: [{ path: "src/a.ts", status: "modified" }],
      skipped: [],
      sourceError: null,
      listing: "fresh",
      targets: stackTargets(),
      selected: { kind: "branch", name: "auth-1" },
      commit: null,
    });
    expect(store.nav).toEqual({ kind: "file", path: "src/a.ts" });
    expect(transport.find("getFile", "src/a.ts")).toBeDefined();
    transport.answer("getFile", "src/a.ts", fileMsg("src/a.ts"));
    await tick();
    expect(store.pane?.kind === "file" && store.pane.view.path).toBe("src/a.ts");
  });

  test("another_tabs_selection_arrives_through_diff_changed_alone", async () => {
    const { store, core } = await bootedStack();
    store.onDiffChanged({
      files: [{ path: "src/a.ts", status: "modified" }],
      skipped: [],
      sourceError: null,
      listing: "fresh",
      targets: stackTargets(),
      selected: { kind: "branch", name: "auth-1" },
      commit: null,
    });
    expect(store.selectedLabel()).toBe("auth-1");
    expect(core.targetScopes.at(-1)?.selected).toEqual({ kind: "branch", name: "auth-1" });
    expect(store.nav).toEqual({ kind: "file", path: "src/a.ts" });
  });

  test("selecting_the_current_target_is_a_no_op", async () => {
    const { store, transport } = await bootedStack();
    store.selectTarget({ kind: "branch", name: "auth-2" });
    expect(transport.find("target.select")).toBeUndefined();
  });

  test("step_target_stops_at_both_ends", async () => {
    const { store, transport } = await bootedStack();
    store.stepTarget(1);
    expect(transport.find("target.select")?.msg).toEqual({ type: "target.select", target: { kind: "stack" } });
    transport.answer("target.select", undefined, {
      type: "targetSelected",
      id: 0,
      selected: { kind: "stack" },
      targets: stackTargets(),
      comparison: null,
      generation: 2,
    });
    await tick();
    store.stepTarget(1);
    expect(store.flash).toBe("top of stack");
    expect(transport.find("target.select")).toBeUndefined();

    store.onTargetSelected({ selected: { kind: "branch", name: "auth-1" }, targets: stackTargets() });
    store.stepTarget(-1);
    expect(store.flash).toBe("bottom of stack");
    expect(transport.find("target.select")).toBeUndefined();
  });

  test("a_rejected_selection_flashes_and_keeps_the_current_target", async () => {
    const { store, transport } = await bootedStack();
    store.selectTarget({ kind: "branch", name: "auth-9" });
    transport.fail("target.select", undefined, new TransportError("error", "target auth-9 is not in the stack", "unknownTarget"));
    await tick();
    expect(store.selectedLabel()).toBe("auth-2");
    expect(store.flash).toContain("auth-9");
  });

  test("add_comment_carries_the_selected_target_in_a_stack", async () => {
    const { store, transport } = await bootedStack();
    void store.addComment("bravo??", { path: "src/b.ts", side: "new", line: 1 });
    const req = transport.find("comment.add");
    expect(req?.msg).toEqual({
      type: "comment.add",
      body: "bravo??",
      path: "src/b.ts",
      side: "new",
      line: 1,
      target: { kind: "branch", name: "auth-2" },
    });

    const plain = await bootedOn("a.ts");
    void plain.store.addComment("note", { path: "a.ts" });
    expect("target" in (plain.transport.find("comment.add")?.msg ?? {})).toBe(false);
  });

  test("target_counts_read_the_projection", async () => {
    const { store, core } = await bootedStack();
    core.targetCounts = [{ id: { kind: "branch", name: "auth-1" }, counts: { todo: 2, total: 3 } }];
    store.onReviewChanged({ review: "r", warnings: [], readOnly: false, readOnlyReason: null });
    expect(store.targetCounts({ kind: "branch", name: "auth-1" })).toEqual({ todo: 2, total: 3 });
    expect(store.targetCounts({ kind: "stack" })).toEqual({ todo: 0, total: 0 });
  });

  test("a_commit_review_exposes_its_banner_summary", async () => {
    const files: FileEntry[] = [{ path: "a.ts", status: "modified" }];
    const { store, transport } = harness(files);
    transport.nextHello = { ...hello(files), commit: { oid: "abcdef1234567890", subject: "add bravo" } };
    await store.connect("tok");
    expect(store.commit?.subject).toBe("add bravo");
    expect(store.isStack()).toBe(false);
  });
});

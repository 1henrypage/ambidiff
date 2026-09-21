// Every command id with a non-empty `web` chord list (the real command
// table, straight from the real wasm) must resolve to a dispatcher
// handler; a chord with nothing behind it is a silent dead key. Skipped
// when `web/pkg` hasn't been built (`scripts/build-web.sh`), since bun
// can't load a wasm module that was never generated.
import { describe, expect, test } from "bun:test";
import { existsSync, readFileSync } from "node:fs";
import { join } from "node:path";

import { createDispatcher } from "./commands";
import type { Editor } from "./editor";
import { decodeCommandTable } from "./protocol";
import type { Store } from "./state";

const PKG_DIR = join(import.meta.dirname, "../pkg");
const WASM_PATH = join(PKG_DIR, "ambidiff_core_bg.wasm");
const wasmBuilt = existsSync(WASM_PATH);

describe("commands", () => {
  (wasmBuilt ? test : test.skip)(
    "every_web_chord_has_a_handler",
    async () => {
      const mod = (await import("../pkg/ambidiff_core.js")) as {
        initSync: (input: { module: Uint8Array }) => unknown;
        ad_commands: () => string;
      };
      mod.initSync({ module: readFileSync(WASM_PATH) });
      const commands = decodeCommandTable(JSON.parse(mod.ad_commands()));
      expect(commands.length).toBeGreaterThan(0);

      const dispatcher = createDispatcher({} as unknown as Store, {} as unknown as Editor, () => {});
      const handled = dispatcher.handledIds();

      const missing = commands.filter((c) => c.web.length > 0 && !handled.has(c.id)).map((c) => c.id);
      expect(missing).toEqual([]);
    },
  );
});

describe("counted dispatch", () => {
  function spyStore() {
    const calls = {
      moveCursor: [] as number[],
      gotoLine: [] as number[],
      cursorToBottom: 0,
      cursorToTop: 0,
    };
    const store = {
      focus: "diff",
      moveCursor: (n: number) => calls.moveCursor.push(n),
      treeStep: (n: number) => calls.moveCursor.push(n),
      gotoLine: (n: number) => calls.gotoLine.push(n),
      cursorToBottom: () => {
        calls.cursorToBottom++;
      },
      cursorToTop: () => {
        calls.cursorToTop++;
      },
    } as unknown as Store;
    return { store, calls };
  }

  test("a_count_multiplies_cursor_down", () => {
    const { store, calls } = spyStore();
    const dispatcher = createDispatcher(store, {} as unknown as Editor, () => {});
    dispatcher.run("ambidiff.nav.cursorDown", 10);
    expect(calls.moveCursor).toEqual([10]);
  });

  test("no_count_defaults_to_one", () => {
    const { store, calls } = spyStore();
    const dispatcher = createDispatcher(store, {} as unknown as Editor, () => {});
    dispatcher.run("ambidiff.nav.cursorUp");
    expect(calls.moveCursor).toEqual([-1]);
  });

  test("bottom_with_a_count_goes_to_that_line_instead_of_the_end", () => {
    const { store, calls } = spyStore();
    const dispatcher = createDispatcher(store, {} as unknown as Editor, () => {});
    dispatcher.run("ambidiff.nav.bottom", 42);
    expect(calls.gotoLine).toEqual([42]);
    expect(calls.cursorToBottom).toBe(0);
  });

  test("bottom_without_a_count_goes_to_the_end", () => {
    const { store, calls } = spyStore();
    const dispatcher = createDispatcher(store, {} as unknown as Editor, () => {});
    dispatcher.run("ambidiff.nav.bottom");
    expect(calls.cursorToBottom).toBe(1);
    expect(calls.gotoLine).toEqual([]);
  });
});

describe("targets", () => {
  function spyTargetStore(stack: boolean) {
    const calls = { stepTarget: [] as number[], selectTarget: [] as string[], setFlash: [] as string[] };
    const targets = stack
      ? [
          { id: { kind: "branch", name: "auth-1" }, label: "auth-1", position: 1, tip: "1".repeat(40), commitCount: 1, subject: "add bravo", aliases: [] },
          { id: { kind: "branch", name: "auth-2" }, label: "auth-2", position: 2, tip: "2".repeat(40), commitCount: 1, subject: "add charlie", aliases: ["alias"] },
        ]
      : [];
    const store = {
      targets,
      isStack: () => stack,
      targetIndex: () => 1,
      targetCounts: () => ({ todo: 0, total: 0 }),
      stepTarget: (n: number) => calls.stepTarget.push(n),
      selectTarget: (id: { kind: string; name?: string }) => calls.selectTarget.push(id.name ?? id.kind),
      setFlash: (msg: string) => calls.setFlash.push(msg),
    } as unknown as Store;
    return { store, calls };
  }

  function spyPickEditor() {
    const picks: { title: string; items: { label: string; detail?: string }[]; initial: number; onPick: (i: number) => void }[] = [];
    const editor = {
      pick: (opts: { title: string; items: { label: string; detail?: string }[]; initial: number; onPick: (i: number) => void }) =>
        picks.push(opts),
    } as unknown as Editor;
    return { editor, picks };
  }

  test("next_and_prev_step_with_the_count", () => {
    const { store, calls } = spyTargetStore(true);
    const dispatcher = createDispatcher(store, {} as unknown as Editor, () => {});
    dispatcher.run("ambidiff.target.next");
    dispatcher.run("ambidiff.target.prev", 2);
    expect(calls.stepTarget).toEqual([1, -2]);
  });

  test("pick_lists_every_target_with_its_subject_and_selects_the_chosen_one", () => {
    const { store, calls } = spyTargetStore(true);
    const { editor, picks } = spyPickEditor();
    const dispatcher = createDispatcher(store, editor, () => {});
    dispatcher.run("ambidiff.target.pick");
    expect(picks.length).toBe(1);
    const pick = picks[0]!;
    expect(pick.initial).toBe(1);
    expect(pick.items.map((i) => i.label)).toEqual([
      "1 auth-1  11111111  1c  ○0/0",
      "2 auth-2  22222222  1c  ○0/0",
    ]);
    expect(pick.items[1]?.detail).toBe("add charlie  (also: alias)");
    pick.onPick(0);
    expect(calls.selectTarget).toEqual(["auth-1"]);
  });

  test("pick_on_a_plain_review_flashes", () => {
    const { store, calls } = spyTargetStore(false);
    const { editor, picks } = spyPickEditor();
    const dispatcher = createDispatcher(store, editor, () => {});
    dispatcher.run("ambidiff.target.pick");
    expect(picks).toEqual([]);
    expect(calls.setFlash).toEqual(["not a stack review"]);
  });
});

describe("delete comment", () => {
  function spyDeleteStore(status: "open" | "resolved") {
    const calls = { deleteComment: [] as string[], setFlash: [] as string[] };
    const store = {
      cursorComment: () => ({ comment: { id: "c-1", status } }),
      deleteNeedsConfirm: (s: string) => s !== "resolved",
      deleteComment: (id: string) => {
        calls.deleteComment.push(id);
        return Promise.resolve();
      },
      setFlash: (msg: string) => calls.setFlash.push(msg),
    } as unknown as Store;
    return { store, calls };
  }

  function spyEditor() {
    const confirms: { title: string; onConfirm: () => void }[] = [];
    const editor = {
      confirm: (title: string, onConfirm: () => void) => confirms.push({ title, onConfirm }),
    } as unknown as Editor;
    return { editor, confirms };
  }

  test("a_resolved_comment_deletes_without_confirming", async () => {
    const { store, calls } = spyDeleteStore("resolved");
    const { editor, confirms } = spyEditor();
    const dispatcher = createDispatcher(store, editor, () => {});
    dispatcher.run("ambidiff.review.deleteComment");
    expect(confirms).toEqual([]);
    expect(calls.deleteComment).toEqual(["c-1"]);
    await Promise.resolve();
    expect(calls.setFlash).toEqual(["comment deleted"]);
  });

  test("an_open_comment_waits_for_confirmation", async () => {
    const { store, calls } = spyDeleteStore("open");
    const { editor, confirms } = spyEditor();
    const dispatcher = createDispatcher(store, editor, () => {});
    dispatcher.run("ambidiff.review.deleteComment");
    expect(confirms.length).toBe(1);
    expect(calls.deleteComment).toEqual([]);
    confirms[0]?.onConfirm();
    expect(calls.deleteComment).toEqual(["c-1"]);
    await Promise.resolve();
    expect(calls.setFlash).toEqual(["comment deleted"]);
  });
});

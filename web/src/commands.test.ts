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

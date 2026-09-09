// The keyboard side of the shared command table (`crates/core/src/commands.rs`,
// `ad_commands()`): turns a `KeyboardEvent` into the chord string the table
// uses, and dispatches a command id onto `Store`/`Editor` calls. One switch
// covers every id the table defines; `handledIds()` lets the boot code (and
// tests) verify every web chord actually resolves to a handler.
import type { Editor } from "./editor";
import type { Store } from "./state";

/** The chord string a web command id binds, e.g. "Enter", "Tab", "j", "]". */
export function chordOf(e: KeyboardEvent): string {
  return e.key;
}

export interface Dispatcher {
  run(id: string): void;
  handledIds(): Set<string>;
}

function reportWrite(store: Store, promise: Promise<unknown>): void {
  promise.catch((e: unknown) => store.setFlash(e instanceof Error ? e.message : String(e)));
}

export function createDispatcher(store: Store, editor: Editor, render: () => void): Dispatcher {
  const handlers = new Map<string, () => void>([
    [
      "ambidiff.nav.cursorDown",
      () => (store.focus === "tree" ? store.treeStep(1) : store.moveCursor(1)),
    ],
    [
      "ambidiff.nav.cursorUp",
      () => (store.focus === "tree" ? store.treeStep(-1) : store.moveCursor(-1)),
    ],
    ["ambidiff.nav.pageDown", () => store.moveCursor(20)],
    ["ambidiff.nav.pageUp", () => store.moveCursor(-20)],
    ["ambidiff.nav.top", () => store.cursorToTop()],
    ["ambidiff.nav.bottom", () => store.cursorToBottom()],
    ["ambidiff.nav.nextHunk", () => store.jumpTo((d) => store.isHunkLine(d), true)],
    ["ambidiff.nav.prevHunk", () => store.jumpTo((d) => store.isHunkLine(d), false)],
    ["ambidiff.nav.nextFile", () => store.stepFile(1)],
    ["ambidiff.nav.prevFile", () => store.stepFile(-1)],
    ["ambidiff.nav.nextComment", () => store.jumpTo((d) => d.kind === "chead", true)],
    ["ambidiff.nav.prevComment", () => store.jumpTo((d) => d.kind === "chead", false)],
    ["ambidiff.nav.focusSwitch", () => store.setFocus(store.focus === "tree" ? "diff" : "tree")],

    ["ambidiff.view.toggleLayout", () => store.toggleMode()],
    ["ambidiff.view.toggleWordDiff", () => store.toggleWordDiff()],
    ["ambidiff.view.toggleTree", () => store.toggleTree()],
    ["ambidiff.view.toggleWrap", () => store.toggleWrap()],
    ["ambidiff.view.toggleLineNumbers", () => store.toggleLineNumbers()],
    ["ambidiff.view.toggleTheme", () => store.setTheme(store.options.theme === "dark" ? "light" : "dark")],
    ["ambidiff.view.cycleFilter", () => store.cycleFilter()],
    [
      "ambidiff.view.expand",
      () => {
        if (store.focus === "tree") {
          store.treeActivate();
          return;
        }
        const rowIndex = store.cursorRowIndex();
        const row = rowIndex !== null && store.pane?.kind === "file" ? store.pane.view.rows[rowIndex] : undefined;
        if (row?.type === "gap") store.expandGap(row.gap.id);
        else store.setFlash("not a collapsed gap");
      },
    ],
    [
      "ambidiff.view.refresh",
      () => {
        if (!store.connected) void store.reconnect();
        else void store.refresh();
      },
    ],

    [
      "ambidiff.review.comment",
      () => {
        const target = store.cursorLineTarget();
        if (!target || store.pane?.kind !== "file") {
          store.setFlash("cursor is not on a diff line (F file, R review)");
          return;
        }
        const path = store.pane.view.path;
        editor.prompt({
          title: `comment on ${path}:${target.line}`,
          initial: "",
          required: true,
          singleLine: false,
          onSave: (body) => store.addComment(body, { path, side: target.side, line: target.line }),
        });
      },
    ],
    [
      "ambidiff.review.commentFile",
      () => {
        if (store.pane?.kind !== "file") {
          store.setFlash("open a file first");
          return;
        }
        const path = store.pane.view.path;
        editor.prompt({
          title: `comment on ${path}`,
          initial: "",
          required: true,
          singleLine: false,
          onSave: (body) => store.addComment(body, { path }),
        });
      },
    ],
    [
      "ambidiff.review.commentReview",
      () => {
        editor.prompt({
          title: "comment on review",
          initial: "",
          required: true,
          singleLine: false,
          onSave: (body) => store.addComment(body, {}),
        });
      },
    ],
    [
      "ambidiff.review.address",
      () => {
        const record = store.cursorComment();
        if (!record) {
          store.setFlash("cursor is not on a comment");
          return;
        }
        editor.prompt({
          title: `response for ${record.comment.id} (optional)`,
          initial: "",
          required: false,
          singleLine: false,
          onSave: (response) => store.address(record.comment.id, response),
        });
      },
    ],
    [
      "ambidiff.review.resolve",
      () => {
        const record = store.cursorComment();
        if (!record) {
          store.setFlash("cursor is not on a comment");
          return;
        }
        reportWrite(store, store.resolve(record.comment.id));
      },
    ],
    [
      "ambidiff.review.reopen",
      () => {
        const record = store.cursorComment();
        if (!record) {
          store.setFlash("cursor is not on a comment");
          return;
        }
        reportWrite(store, store.reopen(record.comment.id));
      },
    ],
    [
      "ambidiff.review.editComment",
      () => {
        const record = store.cursorComment();
        if (!record) {
          store.setFlash("cursor is not on a comment");
          return;
        }
        editor.prompt({
          title: `edit ${record.comment.id}`,
          initial: record.comment.body,
          required: true,
          singleLine: false,
          onSave: (body) => store.editComment(record.comment.id, body),
        });
      },
    ],
    [
      "ambidiff.review.deleteComment",
      () => {
        const record = store.cursorComment();
        if (!record) {
          store.setFlash("cursor is not on a comment");
          return;
        }
        editor.confirm(`delete comment ${record.comment.id}?`, () =>
          reportWrite(store, store.deleteComment(record.comment.id)),
        );
      },
    ],

    ["ambidiff.search.start", () => editor.search((query) => store.startSearch(query))],
    ["ambidiff.search.next", () => store.searchNext()],
    ["ambidiff.search.prev", () => store.searchPrev()],

    ["ambidiff.app.help", () => editor.help(store.commands)],
  ]);

  function run(id: string): void {
    const handler = handlers.get(id);
    if (handler) {
      handler();
      render();
    }
  }

  function handledIds(): Set<string> {
    return new Set(handlers.keys());
  }

  return { run, handledIds };
}

// Boot only: wire the wasm core, the websocket transport, the store, the
// editor overlay, the renderer, and the command dispatcher together, then
// hand control to the keyboard/click bindings. No diff semantics live
// here -- see core.ts (wasm wrapper), state.ts (the read model / flows),
// render.ts (painting), editor.ts (overlay), commands.ts (dispatch).
import init, * as wasmModule from "../pkg/ambidiff_core.js";
import { createCore } from "./core";
import { createDispatcher, chordOf } from "./commands";
import { createEditor } from "./editor";
import type { DiffChangedMessage, ReviewChangedMessage } from "./protocol";
import { createRenderer } from "./render";
import { Store } from "./state";
import { Transport, type SocketLike } from "./transport";

const THEME_KEY = "ambidiff-theme";

function el<T extends HTMLElement>(id: string): T {
  const found = document.getElementById(id);
  if (!found) throw new Error(`missing #${id}`);
  return found as T;
}

function socketFactory(): SocketLike {
  return new WebSocket(`ws://${location.host}/ws`) as unknown as SocketLike;
}

async function main(): Promise<void> {
  const body = document.body;
  const storedTheme = (() => {
    try {
      return localStorage.getItem(THEME_KEY);
    } catch {
      return null;
    }
  })();
  body.dataset["theme"] = storedTheme === "light" ? "light" : "dark";

  await init();
  const core = createCore(wasmModule);

  const dom = {
    app: el<HTMLElement>("app"),
    tree: el<HTMLElement>("tree"),
    banner: el<HTMLElement>("banner"),
    diag: el<HTMLElement>("diag"),
    viewport: el<HTMLElement>("viewport"),
    spacer: el<HTMLElement>("spacer"),
    rows: el<HTMLElement>("rows"),
    statusbar: el<HTMLElement>("statusbar"),
    body,
  };
  const overlay = el<HTMLElement>("overlay");

  let store: Store;
  const transport = new Transport(socketFactory, {
    onBroadcast: (msg) => {
      if (msg.type === "reviewChanged") store.onReviewChanged(msg as ReviewChangedMessage);
      else store.onDiffChanged(msg as DiffChangedMessage);
    },
    onConnection: (state) => store.onConnection(state),
  });
  store = new Store(core, transport);

  const editor = createEditor({ overlay, focusReturn: dom.viewport });

  const renderer = createRenderer(store, dom, window, {
    openOverview: () => store.openOverview(),
    toggleDir: (path) => store.toggleDirPath(path),
    openFile: (path) => store.openFile(path, false),
    expandGap: (gapId) => store.expandGap(gapId),
    onRowClick: (index, cell) => store.clickRow(index, cell),
  });

  const dispatcher = createDispatcher(store, editor, renderer.render);

  document.addEventListener("keydown", (e) => {
    if (editor.isOpen()) {
      if (e.key === "Escape") editor.close();
      return;
    }
    const tag = (e.target as HTMLElement | null)?.tagName;
    if (tag === "TEXTAREA" || tag === "INPUT") return;
    const chord = chordOf(e);
    const command = store.commands.find((c) => c.web.includes(chord));
    if (command && dispatcher.handledIds().has(command.id)) {
      e.preventDefault();
      dispatcher.run(command.id);
    }
  });

  store.subscribe(() => {
    try {
      localStorage.setItem(THEME_KEY, store.options.theme === "light" ? "light" : "dark");
    } catch {
      // storage may be unavailable (private browsing); theme just resets next load
    }
  });

  const token = new URLSearchParams(location.hash.slice(1)).get("t") ?? "";
  await store.connect(token);
}

void main();

// DOM painting: tree, banner, diagnostics strip, status bar, and the
// virtual-scrolling diff viewport. All user-controlled text (comment
// bodies, paths, snippets, diff content) reaches the DOM through text
// nodes / `textContent`, never `innerHTML`, so nothing a repo or a
// reviewer wrote can inject markup.
import type { Cell, Row } from "./protocol";
import { HeightIndex, type CommentRecord, type DisplayLine, type Segment, composeSegments, gutterDigits } from "./paint";
import type { Store } from "./state";

const ROW_H = 20;
/** How far past the visible viewport a measured pass keeps rendering rows. */
const OVERSCAN_PX = 400;
/** A safety cap so a pathological (near-infinite) scroll jump cannot force
 *  the measured pass to render the whole file in one frame. */
const MAX_MEASURED_ROWS = 500;
const STATUS_GLYPH: Record<string, string> = { open: "○", addressed: "◐", resolved: "●", reopened: "↺" };
const FILE_STATUS_LETTER: Record<string, string> = {
  added: "A",
  modified: "M",
  deleted: "D",
  renamed: "R",
  copied: "C",
  untracked: "?",
  other: "T",
};

export interface RenderDom {
  app: HTMLElement;
  tree: HTMLElement;
  banner: HTMLElement;
  diag: HTMLElement;
  viewport: HTMLElement;
  spacer: HTMLElement;
  rows: HTMLElement;
  statusbar: HTMLElement;
  body: HTMLElement;
}

export interface RenderActions {
  openOverview(): void;
  toggleDir(path: string): void;
  openFile(path: string): void;
  expandGap(gapId: string): void;
  onRowClick(displayIndex: number, cell: "left" | "right" | null): void;
}

function clear(el: HTMLElement): void {
  while (el.firstChild) el.removeChild(el.firstChild);
}

function span(className: string | null, text: string, style?: string): HTMLSpanElement {
  const s = document.createElement("span");
  if (className) s.className = className;
  s.textContent = text;
  if (style) s.setAttribute("style", style);
  return s;
}

function pad(n: number | null | undefined, w: number): string {
  return (n === null || n === undefined ? "" : String(n)).padStart(w, " ");
}

export function createRenderer(store: Store, dom: RenderDom, win: Window, actions: RenderActions) {
  function tallies(path: string): { todo: number; total: number } {
    return store.projection?.counts[path] ?? { todo: 0, total: 0 };
  }

  function renderTree(): void {
    clear(dom.tree);
    const overview = document.createElement("div");
    overview.className = "tree-row" + (store.nav.kind === "overview" ? " current" : "");
    if (store.focus === "tree" && store.treeCursor === 0) overview.classList.add("cursor");
    const rl = (store.projection?.reviewLevelComments ?? 0) + (store.projection?.unattachedComments ?? 0);
    overview.appendChild(document.createTextNode("(review)"));
    if (rl > 0) {
      overview.appendChild(document.createTextNode(" "));
      overview.appendChild(span("todo", String(rl)));
    }
    overview.onclick = () => actions.openOverview();
    dom.tree.appendChild(overview);

    const rows = store.projection?.tree ?? [];
    rows.forEach((row, i) => {
      const div = document.createElement("div");
      div.style.paddingLeft = `${10 + row.depth * 14}px`;
      if (store.focus === "tree" && store.treeCursor === i + 1) div.classList.add("cursor");
      if (row.isDir) {
        div.className = "tree-row" + (div.classList.contains("cursor") ? " cursor" : "");
        const dir = span("dir", `${row.collapsed ? "▸" : "▾"} ${row.name}`);
        div.appendChild(dir);
        div.onclick = () => actions.toggleDir(row.path);
      } else {
        const entry = row.fileIndex !== undefined ? store.files[row.fileIndex] : undefined;
        const path = entry?.path ?? row.path;
        const current = store.nav.kind === "file" && store.nav.path === path;
        div.className = "tree-row" + (current ? " current" : "") + (div.classList.contains("cursor") ? " cursor" : "");
        const t = tallies(path);
        div.appendChild(span("fstatus", (entry ? FILE_STATUS_LETTER[entry.status] : undefined) ?? "T"));
        div.appendChild(document.createTextNode(row.name));
        if (t.total > 0) {
          div.appendChild(document.createTextNode(" "));
          div.appendChild(span(t.todo > 0 ? "todo" : "done", `○${t.todo}/${t.total}`));
        }
        if (entry?.adds !== undefined) {
          div.appendChild(document.createTextNode(" "));
          div.appendChild(span("stat-a", `+${entry.adds}`));
          div.appendChild(document.createTextNode(" "));
          div.appendChild(span("stat-d", `-${entry.dels ?? 0}`));
        }
        div.onclick = () => actions.openFile(path);
      }
      dom.tree.appendChild(div);
    });
  }

  function renderBanner(): void {
    clear(dom.banner);
    if (store.nav.kind === "overview") {
      const s = store.summary;
      if (s) {
        dom.banner.appendChild(document.createTextNode("review "));
        dom.banner.appendChild(span(null, s.name));
        dom.banner.appendChild(
          document.createTextNode(
            ` rev ${s.revision} • ○${s.counts.open} ↺${s.counts.reopened} ◐${s.counts.addressed} ●${s.counts.resolved}`,
          ),
        );
      }
      return;
    }
    const path = store.nav.path;
    const entry = store.files.find((f) => f.path === path);
    const b = document.createElement("b");
    b.textContent = path;
    dom.banner.appendChild(b);
    if (entry?.oldPath) {
      dom.banner.appendChild(document.createTextNode(" "));
      dom.banner.appendChild(span("was", `(was ${entry.oldPath})`));
    }
    if (store.pane?.kind === "file") {
      dom.banner.appendChild(document.createTextNode(" "));
      dom.banner.appendChild(span("adds", `+${store.pane.view.adds}`));
      dom.banner.appendChild(document.createTextNode(" "));
      dom.banner.appendChild(span("dels", `-${store.pane.view.dels}`));
    }
    const t = tallies(path);
    if (t.total > 0) dom.banner.appendChild(document.createTextNode(` • ○ ${t.todo}/${t.total} comments`));
  }

  function renderDiag(): void {
    const d = store.diagnostics;
    const messages: string[] = [];
    // `summary !== null` means the page has completed at least one boot,
    // so a "disconnected" state here is a real drop, not the brief
    // pre-connect window.
    const disconnected = store.connectionState === "disconnected" && store.summary !== null;
    if (disconnected) messages.push("disconnected");
    if (d.reviewError) messages.push(`review file problem: ${d.reviewError}`);
    if (d.sourceError) messages.push(`source: ${d.sourceError}`);
    if (d.readOnly) messages.push(`read-only${d.readOnlyReason ? `: ${d.readOnlyReason}` : ""}`);
    for (const s of d.skipped) messages.push(`skipped ${s.display}: ${s.reason}`);
    for (const w of d.warnings) messages.push(w);
    clear(dom.diag);
    if (messages.length === 0) {
      dom.diag.hidden = true;
      return;
    }
    dom.diag.hidden = false;
    for (const m of messages) {
      const line = document.createElement("div");
      line.textContent = m;
      dom.diag.appendChild(line);
    }
    if (disconnected) {
      const btn = document.createElement("button");
      btn.textContent = "reconnect";
      btn.dataset["act"] = "reconnect";
      btn.onclick = () => void store.reconnect();
      dom.diag.appendChild(btn);
    }
  }

  function renderStatus(): void {
    clear(dom.statusbar);
    const s = store.summary;
    const left = s
      ? `${s.name} rev ${s.revision} • ○${s.counts.open} ↺${s.counts.reopened} ◐${s.counts.addressed} ●${s.counts.resolved}`
      : "connecting";
    dom.statusbar.appendChild(span(null, left));
    dom.statusbar.appendChild(span("msg", store.flash ?? ""));
    const toggles = [
      store.options.mode,
      store.options.wordDiff ? "word" : "",
      store.wrap ? "wrap" : "",
      store.filter !== "all" ? `filter:${store.filter}` : "",
    ]
      .filter(Boolean)
      .join(" ");
    const searchText = store.search.active ? `/${store.search.query} (${store.search.matches.length})` : "";
    const right = span(null, searchText);
    right.style.marginLeft = "auto";
    dom.statusbar.appendChild(right);
    dom.statusbar.appendChild(span(null, toggles));
    dom.statusbar.appendChild(span(null, "? help"));
  }

  function paintCell(container: HTMLElement, cell: Cell, rowIndex: number, matchCell: "unified" | "left" | "right"): void {
    const hits = store.search.matches.filter((m) => m.row === rowIndex && m.cell === matchCell);
    const segments: Segment[] = composeSegments(cell, hits);
    for (const seg of segments) {
      const cls = seg.inHit ? "hit" : seg.inWord ? (cell.kind === "add" ? "wa" : "wd") : null;
      const style = seg.fg ? `color:${seg.fg}` : undefined;
      container.appendChild(span(cls, seg.text, style));
    }
  }

  function commentCardLine(
    kind: "chead" | "cline" | "cfoot",
    record: CommentRecord,
    text?: string,
    role?: "response",
  ): HTMLElement {
    const div = document.createElement("div");
    div.className = `dl card ${kind}`;
    if (kind === "chead") {
      div.appendChild(span("border", "  ┌─ "));
      div.appendChild(span(`st-${record.comment.status}`, `${STATUS_GLYPH[record.comment.status] ?? "?"} `));
      const b = document.createElement("b");
      b.textContent = `${record.comment.id} `;
      div.appendChild(b);
      div.appendChild(span("gut", `${record.comment.status} rev ${record.comment.rev} by ${record.comment.author} `));
      if (record.comment.body.includes("??")) div.appendChild(span("qbadge", "[question] "));
      if (record.anchor?.outdated) div.appendChild(span("obadge", "[outdated] "));
      if (record.wasPath) div.appendChild(span("obadge", `(was ${record.wasPath}) `));
    } else if (kind === "cline") {
      div.appendChild(span("border", "  │ "));
      div.appendChild(span(role === "response" ? "celltext hang" : "celltext", text ?? ""));
    } else {
      div.appendChild(span("border", "  └─"));
    }
    return div;
  }

  function paintRow(row: Row, rowIndex: number, digits: number): HTMLElement {
    const div = document.createElement("div");
    div.className = "dl";
    switch (row.type) {
      case "hunkHeader":
        div.classList.add("hunk");
        div.textContent = row.text;
        break;
      case "gap":
        div.classList.add("gap");
        div.textContent =
          " ".repeat(digits * 2 + 2) + `⋯ ${row.gap.count} unchanged lines ⋯  (enter expands)`;
        div.onclick = () => actions.expandGap(row.gap.id);
        break;
      case "unified": {
        if (row.cell.kind === "add") div.classList.add("add");
        if (row.cell.kind === "remove") div.classList.add("remove");
        const sign = row.cell.kind === "add" ? "+" : row.cell.kind === "remove" ? "-" : " ";
        const signClass = row.cell.kind === "add" ? "sign-a" : row.cell.kind === "remove" ? "sign-d" : null;
        div.appendChild(span("gut", `${pad(row.oldNum, digits)} ${pad(row.newNum, digits)} `));
        div.appendChild(span(signClass, `${sign} `));
        {
          const textWrap = document.createElement("span");
          textWrap.className = "celltext";
          paintCell(textWrap, row.cell, rowIndex, "unified");
          div.appendChild(textWrap);
        }
        break;
      }
      case "split": {
        const half = (cell: Cell, matchCell: "left" | "right") => {
          const wrap = document.createElement("span");
          wrap.className = "split-half" + (cell.kind === "add" ? " add" : cell.kind === "remove" ? " remove" : "");
          wrap.dataset["cell"] = matchCell;
          const sign = cell.kind === "add" ? "+" : cell.kind === "remove" ? "-" : " ";
          const signClass = cell.kind === "add" ? "sign-a" : cell.kind === "remove" ? "sign-d" : null;
          wrap.appendChild(span("gut", `${pad(cell.line, digits)} `));
          wrap.appendChild(span(signClass, `${sign} `));
          const textWrap = document.createElement("span");
          textWrap.className = "celltext";
          paintCell(textWrap, cell, rowIndex, matchCell);
          wrap.appendChild(textWrap);
          return wrap;
        };
        div.appendChild(half(row.left, "left"));
        div.appendChild(span("split-sep", "│"));
        div.appendChild(half(row.right, "right"));
        break;
      }
    }
    return div;
  }

  function paintDisplayLine(line: DisplayLine, index: number, digits: number): HTMLElement {
    let div: HTMLElement;
    switch (line.kind) {
      case "notice":
        div = document.createElement("div");
        div.className = "dl notice";
        div.textContent = `  ${line.text}`;
        break;
      case "section":
        div = document.createElement("div");
        div.className = "dl notice";
        div.textContent = `── ${line.text} `;
        break;
      case "blank":
        div = document.createElement("div");
        div.className = "dl";
        break;
      case "chead":
        div = commentCardLine("chead", line.record);
        break;
      case "cline":
        div = commentCardLine("cline", line.record, line.text, line.role);
        break;
      case "cfoot":
        div = commentCardLine("cfoot", line.record);
        break;
      case "row": {
        const row = store.pane?.kind === "file" ? store.pane.view.rows[line.rowIndex] : undefined;
        div = row ? paintRow(row, line.rowIndex, digits) : document.createElement("div");
        break;
      }
    }
    div.dataset["index"] = String(index);
    if (index === store.cursor && store.focus === "diff") div.classList.add("cursor");
    return div;
  }

  // ---------------------------------------------------------- fixed layout
  //
  // The unwrapped mode: every row is exactly ROW_H tall, so the visible
  // window is a plain arithmetic slice and #rows is positioned as one
  // block at `first * ROW_H`.

  function renderDiffFixed(total: number): void {
    dom.spacer.style.height = `${total * ROW_H}px`;
    const first = Math.max(0, Math.floor(dom.viewport.scrollTop / ROW_H) - 10);
    const count = Math.ceil(dom.viewport.clientHeight / ROW_H) + 20;
    dom.rows.style.position = "absolute";
    dom.rows.style.top = `${first * ROW_H}px`;
    clear(dom.rows);
    const digits = store.pane?.kind === "file" ? gutterDigits(store.pane.view.rows) : 3;
    for (let i = first; i < Math.min(total, first + count); i++) {
      const line = store.display[i];
      if (line) {
        const el = paintDisplayLine(line, i, digits);
        dom.rows.appendChild(el);
      }
    }
  }

  // -------------------------------------------------------- measured layout
  //
  // The wrapped mode: rows vary in height (a wrapped long line spans
  // several visual lines), so positions come from a Fenwick prefix-sum
  // (`HeightIndex`) instead of `index * ROW_H`. Each rendered row is
  // measured with `getBoundingClientRect` the instant it is appended (so
  // the very first paint of a window is already consistent -- row i+1's
  // position is computed from row i's just-updated height, never a stale
  // guess) and again, later, by a `ResizeObserver` that catches a row
  // re-wrapping for a reason that did not go through `store` (e.g. a plain
  // window resize is also caught by the generic "resize" listener below,
  // which fully re-renders and so re-measures; the observer is the
  // backstop for width changes that reach the DOM without either of
  // those). Observer-driven reflows are coalesced onto `requestAnimationFrame`
  // and keep the cursor row visually anchored while the layout shifts.

  let measuredHeights: HeightIndex | null = null;
  let measuredForDisplay: DisplayLine[] | null = null;
  let reflowRaf: number | null = null;

  function ensureMeasured(total: number): HeightIndex {
    if (!measuredHeights || measuredForDisplay !== store.display || measuredHeights.length !== total) {
      measuredHeights = new HeightIndex(total, ROW_H);
      measuredForDisplay = store.display;
    }
    return measuredHeights;
  }

  function anchoredToCursor(mutate: () => void): void {
    const before = dom.rows.querySelector(`[data-index="${store.cursor}"]`) as HTMLElement | null;
    const beforeTop = before ? before.getBoundingClientRect().top : null;
    mutate();
    if (beforeTop === null) return;
    const after = dom.rows.querySelector(`[data-index="${store.cursor}"]`) as HTMLElement | null;
    if (!after) return;
    const afterTop = after.getBoundingClientRect().top;
    if (afterTop !== beforeTop) dom.viewport.scrollTop += afterTop - beforeTop;
  }

  const rowObserver = new ResizeObserver((entries) => {
    if (!store.wrap || !measuredHeights) return;
    let changed = false;
    for (const entry of entries) {
      const idxAttr = (entry.target as HTMLElement).dataset["index"];
      if (idxAttr === undefined) continue;
      const idx = Number(idxAttr);
      if (idx < 0 || idx >= measuredHeights.length) continue;
      const h = (entry.target as HTMLElement).getBoundingClientRect().height;
      if (h > 0 && Math.abs(measuredHeights.get(idx) - h) > 0.5) {
        measuredHeights.set(idx, h);
        changed = true;
      }
    }
    if (!changed) return;
    if (reflowRaf !== null) return;
    reflowRaf = win.requestAnimationFrame(() => {
      reflowRaf = null;
      anchoredToCursor(() => renderDiff());
    });
  });

  function renderDiffMeasured(total: number): void {
    const heights = ensureMeasured(total);
    dom.spacer.style.height = `${heights.total()}px`;
    const scrollTop = dom.viewport.scrollTop;
    const bottomLimit = scrollTop + dom.viewport.clientHeight + OVERSCAN_PX;
    const first = Math.max(0, heights.indexAt(scrollTop) - 3);
    rowObserver.disconnect();
    dom.rows.style.position = "static";
    dom.rows.style.top = "0px";
    clear(dom.rows);
    const digits = store.pane?.kind === "file" ? gutterDigits(store.pane.view.rows) : 3;
    let i = first;
    let rendered = 0;
    while (i < total && rendered < MAX_MEASURED_ROWS) {
      const top = heights.prefix(i);
      if (rendered > 0 && top > bottomLimit) break;
      const line = store.display[i];
      if (line) {
        const el = paintDisplayLine(line, i, digits);
        el.style.position = "absolute";
        el.style.left = "0";
        el.style.right = "0";
        el.style.top = `${top}px`;
        dom.rows.appendChild(el);
        const measuredHeight = el.getBoundingClientRect().height;
        if (measuredHeight > 0) heights.set(i, measuredHeight);
        rowObserver.observe(el);
      }
      i += 1;
      rendered += 1;
    }
  }

  function renderDiff(): void {
    const total = store.display.length;
    if (store.wrap) renderDiffMeasured(total);
    else renderDiffFixed(total);
  }

  function ensureCursorVisible(): void {
    if (store.wrap) {
      const heights = measuredHeights ?? ensureMeasured(store.display.length);
      const y = heights.prefix(store.cursor);
      const h = heights.get(store.cursor);
      if (y < dom.viewport.scrollTop) dom.viewport.scrollTop = y;
      else if (y + h > dom.viewport.scrollTop + dom.viewport.clientHeight) {
        dom.viewport.scrollTop = y + h - dom.viewport.clientHeight;
      }
      return;
    }
    const y = store.cursor * ROW_H;
    if (y < dom.viewport.scrollTop) dom.viewport.scrollTop = y;
    else if (y + ROW_H > dom.viewport.scrollTop + dom.viewport.clientHeight) {
      dom.viewport.scrollTop = y + ROW_H - dom.viewport.clientHeight;
    }
  }

  let lastCursor = -1;
  let lastWrap = false;
  function render(): void {
    dom.app.classList.toggle("tree-hidden", !store.showTree);
    dom.app.classList.toggle("no-gutter", !store.showLineNumbers);
    dom.app.classList.toggle("wrap", store.wrap);
    dom.body.dataset["theme"] = store.options.theme === "light" ? "light" : "dark";
    renderTree();
    renderBanner();
    renderDiag();
    renderStatus();
    if (store.cursor !== lastCursor || store.wrap !== lastWrap) {
      lastCursor = store.cursor;
      lastWrap = store.wrap;
      ensureCursorVisible();
    }
    renderDiff();
  }

  dom.viewport.addEventListener("scroll", renderDiff);
  win.addEventListener("resize", renderDiff);
  dom.viewport.addEventListener("click", (e) => {
    const target = (e.target as HTMLElement).closest("[data-index]") as HTMLElement | null;
    if (!target) return;
    const index = Number(target.dataset["index"]);
    const cellTarget = (e.target as HTMLElement).closest("[data-cell]") as HTMLElement | null;
    const cell = (cellTarget?.dataset["cell"] as "left" | "right" | undefined) ?? null;
    actions.onRowClick(index, cell);
  });

  const unsubscribe = store.subscribe(render);
  render();
  return {
    render,
    dispose: () => {
      unsubscribe();
      rowObserver.disconnect();
    },
  };
}

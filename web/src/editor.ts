// The overlay: a single `#overlay` element repurposed for a draft prompt,
// a yes/no confirm, the help table, and the inline search box. All user
// text (titles built from paths/ids are safe literals, but comment bodies
// and the like) goes in through `textContent`/`.value`, never `innerHTML`.
// The overlay's own lifecycle is independent of the store: state changes
// (a broadcast, a reconnect) never close it out from under a draft (B19),
// and its Save always attempts the write even if the connection or
// read-only flag flipped while it was open -- the write itself is what
// reports that back to the user.
import type { CommandSpec } from "./protocol";

export interface PromptOptions {
  title: string;
  initial: string;
  required: boolean;
  singleLine: boolean;
  onSave: (text: string) => Promise<unknown>;
}

export interface EditorDom {
  overlay: HTMLElement;
  focusReturn: HTMLElement;
}

/** One row of a picker list. */
export interface PickItem {
  label: string;
  detail?: string;
}

export interface PickOptions<T extends PickItem> {
  title: string;
  items: T[];
  /** Index of the row the cursor starts on. */
  initial: number;
  onPick: (index: number, item: T) => void;
}

export interface Editor {
  prompt(opts: PromptOptions): void;
  confirm(title: string, onConfirm: () => void): void;
  help(commands: CommandSpec[]): void;
  search(onSubmit: (query: string) => void): void;
  lineInput(onSubmit: (line: number) => void): void;
  pick<T extends PickItem>(opts: PickOptions<T>): void;
  close(): void;
  isOpen(): boolean;
}

function clear(el: HTMLElement): void {
  while (el.firstChild) el.removeChild(el.firstChild);
}

export function createEditor(dom: EditorDom): Editor {
  function close(): void {
    dom.overlay.hidden = true;
    clear(dom.overlay);
    dom.overlay.onclick = null;
    dom.focusReturn.focus();
  }

  function isOpen(): boolean {
    return !dom.overlay.hidden;
  }

  function box(): HTMLElement {
    dom.overlay.hidden = false;
    clear(dom.overlay);
    const b = document.createElement("div");
    b.className = "box";
    dom.overlay.appendChild(b);
    return b;
  }

  function prompt(opts: PromptOptions): void {
    const b = box();
    const title = document.createElement("div");
    title.style.marginBottom = "8px";
    const strong = document.createElement("b");
    strong.textContent = opts.title;
    title.appendChild(strong);
    b.appendChild(title);

    const input: HTMLTextAreaElement | HTMLInputElement = opts.singleLine
      ? document.createElement("input")
      : document.createElement("textarea");
    input.value = opts.initial;
    b.appendChild(input);

    const hint = document.createElement("div");
    hint.className = "hint";
    hint.textContent = "ctrl-enter or Save saves · esc cancels";
    b.appendChild(hint);

    const errorEl = document.createElement("div");
    errorEl.className = "overlay-error";
    errorEl.hidden = true;
    b.appendChild(errorEl);

    const buttons = document.createElement("div");
    buttons.style.marginTop = "8px";
    const saveBtn = document.createElement("button");
    saveBtn.textContent = "Save";
    saveBtn.dataset["act"] = "save";
    const cancelBtn = document.createElement("button");
    cancelBtn.textContent = "Cancel";
    cancelBtn.dataset["act"] = "cancel";
    buttons.append(saveBtn, cancelBtn);
    b.appendChild(buttons);

    input.focus();

    let saving = false;
    const doSave = (): void => {
      if (saving) return;
      const text = opts.required ? input.value.trim() : input.value;
      if (opts.required && text === "") {
        errorEl.hidden = false;
        errorEl.textContent = "required";
        return;
      }
      saving = true;
      saveBtn.setAttribute("disabled", "disabled");
      errorEl.hidden = true;
      opts
        .onSave(text)
        .then(() => close())
        .catch((e: unknown) => {
          saving = false;
          saveBtn.removeAttribute("disabled");
          errorEl.hidden = false;
          errorEl.textContent = e instanceof Error ? e.message : String(e);
          // the draft text stays exactly as the user left it (B19).
        });
    };
    input.onkeydown = (e) => {
      if (e.key === "Enter" && (e.ctrlKey || e.metaKey)) {
        e.preventDefault();
        doSave();
      } else if (e.key === "Escape") {
        e.preventDefault();
        close();
      }
      e.stopPropagation();
    };
    saveBtn.onclick = doSave;
    cancelBtn.onclick = () => close();
  }

  function confirm(title: string, onConfirm: () => void): void {
    const b = box();
    const t = document.createElement("div");
    t.style.marginBottom = "8px";
    t.textContent = title;
    b.appendChild(t);
    const buttons = document.createElement("div");
    const yes = document.createElement("button");
    yes.textContent = "Delete";
    yes.dataset["act"] = "confirm";
    const no = document.createElement("button");
    no.textContent = "Cancel";
    no.dataset["act"] = "cancel";
    buttons.append(yes, no);
    b.appendChild(buttons);
    yes.onclick = () => {
      close();
      onConfirm();
    };
    no.onclick = () => close();
    dom.overlay.onkeydown = (e) => {
      if (e.key === "Escape") close();
    };
  }

  function help(commands: CommandSpec[]): void {
    const b = box();
    const title = document.createElement("div");
    title.style.marginBottom = "8px";
    const strong = document.createElement("b");
    strong.textContent = "ambidiff help";
    title.appendChild(strong);
    b.appendChild(title);
    const table = document.createElement("table");
    for (const c of commands) {
      if (c.web.length === 0) continue;
      const tr = document.createElement("tr");
      const chord = document.createElement("td");
      chord.className = "chord";
      chord.textContent = c.web.join(" / ");
      const name = document.createElement("td");
      name.textContent = c.name;
      const desc = document.createElement("td");
      desc.className = "desc";
      desc.textContent = c.desc;
      tr.append(chord, name, desc);
      table.appendChild(tr);
    }
    b.appendChild(table);
    const countHint = document.createElement("div");
    countHint.className = "hint";
    countHint.textContent = "123j  repeat a motion (10j, 10k, 10G)";
    b.appendChild(countHint);
    const hint = document.createElement("div");
    hint.className = "hint";
    hint.textContent = "esc closes";
    b.appendChild(hint);
    dom.overlay.onclick = (e) => {
      if (e.target === dom.overlay) close();
    };
  }

  /** Shared body for a single-input status-style box (search, goto-line):
   * one text input, a hint line, enter submits, esc cancels. */
  function inputBox(opts: {
    placeholder: string;
    hint: string;
    filter?: (raw: string) => string;
    onSubmit: (text: string) => void;
  }): void {
    const b = box();
    const input = document.createElement("input");
    input.placeholder = opts.placeholder;
    b.appendChild(input);
    const hint = document.createElement("div");
    hint.className = "hint";
    hint.textContent = opts.hint;
    b.appendChild(hint);
    input.focus();
    if (opts.filter) {
      input.oninput = () => {
        input.value = opts.filter?.(input.value) ?? input.value;
      };
    }
    input.onkeydown = (e) => {
      if (e.key === "Enter") {
        e.preventDefault();
        const text = input.value;
        close();
        opts.onSubmit(text);
      } else if (e.key === "Escape") {
        e.preventDefault();
        close();
      }
      e.stopPropagation();
    };
  }

  /** A generic list picker (the target picker): j/k/arrows move, enter
   * picks, esc closes, click picks, the backdrop closes. */
  function pick<T extends PickItem>(opts: PickOptions<T>): void {
    const b = box();
    b.tabIndex = -1;
    const title = document.createElement("div");
    title.style.marginBottom = "8px";
    const strong = document.createElement("b");
    strong.textContent = opts.title;
    title.appendChild(strong);
    b.appendChild(title);
    const list = document.createElement("div");
    list.className = "picker";
    b.appendChild(list);
    let cursor = Math.max(0, Math.min(opts.items.length - 1, opts.initial));
    const rows: HTMLElement[] = opts.items.map((item, i) => {
      const row = document.createElement("div");
      row.className = "picker-row";
      row.dataset["index"] = String(i);
      row.appendChild(document.createTextNode(item.label));
      if (item.detail) {
        row.appendChild(document.createTextNode(" "));
        const detail = document.createElement("span");
        detail.className = "desc";
        detail.textContent = item.detail;
        row.appendChild(detail);
      }
      row.onclick = () => {
        close();
        opts.onPick(i, item);
      };
      list.appendChild(row);
      return row;
    });
    const paint = () => rows.forEach((row, i) => row.classList.toggle("cursor", i === cursor));
    paint();
    const hint = document.createElement("div");
    hint.className = "hint";
    hint.textContent = "enter selects · j/k move · esc closes";
    b.appendChild(hint);
    b.focus();
    b.onkeydown = (e) => {
      if (e.key === "Escape") {
        e.preventDefault();
        close();
      } else if (e.key === "j" || e.key === "ArrowDown") {
        e.preventDefault();
        cursor = Math.min(rows.length - 1, cursor + 1);
        paint();
      } else if (e.key === "k" || e.key === "ArrowUp") {
        e.preventDefault();
        cursor = Math.max(0, cursor - 1);
        paint();
      } else if (e.key === "Enter") {
        e.preventDefault();
        const item = opts.items[cursor];
        close();
        if (item) opts.onPick(cursor, item);
      }
      e.stopPropagation();
    };
    dom.overlay.onclick = (e) => {
      if (e.target === dom.overlay) close();
    };
  }

  function search(onSubmit: (query: string) => void): void {
    inputBox({
      placeholder: "search in diff",
      hint: "enter searches · esc cancels",
      onSubmit,
    });
  }

  function lineInput(onSubmit: (line: number) => void): void {
    inputBox({
      placeholder: "line number",
      hint: "enter jumps · esc cancels",
      filter: (raw) => raw.replace(/\D/g, ""),
      onSubmit: (text) => {
        const line = Number.parseInt(text, 10);
        if (Number.isInteger(line) && line > 0) onSubmit(line);
      },
    });
  }

  return { prompt, confirm, help, search, lineInput, pick, close, isOpen };
}

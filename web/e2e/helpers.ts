// Shared harness for the browser journeys: one scratch git repository and
// one `ambidiff web` process per spec file, readiness taken from the URL
// the binary prints, child stderr captured so a startup failure reports
// its real cause instead of a bare timeout, and every temp dir removed.
import type { Page, WebSocketRoute } from "@playwright/test";
import { spawn, execFileSync, type ChildProcess } from "node:child_process";
import { chmodSync, existsSync, mkdtempSync, mkdirSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { dirname, join } from "node:path";

export const BIN = process.env.AMBIDIFF_BIN ?? join(import.meta.dirname, "../../target/debug/ambidiff");

/** A scratch repository with the review file initialised. */
export class ScratchRepo {
  readonly root: string;

  constructor(prefix = "ambidiff-e2e-") {
    this.root = mkdtempSync(join(tmpdir(), prefix));
    this.git(["init", "-q", "-b", "main"]);
    this.git(["config", "core.autocrlf", "false"]);
    this.git(["config", "user.email", "t@t"]);
    this.git(["config", "user.name", "t"]);
  }

  git(args: string[]): string {
    return execFileSync("git", args, { cwd: this.root }).toString();
  }

  /** Run the ambidiff binary in the repository; returns stdout. */
  cli(args: string[]): string {
    return execFileSync(BIN, args, {
      cwd: this.root,
      env: { ...process.env, AMBIDIFF_AUTHOR: "henry" },
    }).toString();
  }

  write(path: string, content: string): void {
    const full = join(this.root, path);
    mkdirSync(dirname(full), { recursive: true });
    writeFileSync(full, content);
  }

  /** Binary-safe write, for a file whose diff must show git's binary marker. */
  writeBytes(path: string, content: Buffer): void {
    const full = join(this.root, path);
    mkdirSync(dirname(full), { recursive: true });
    writeFileSync(full, content);
  }

  chmodExecutable(path: string): void {
    chmodSync(join(this.root, path), 0o755);
  }

  commitAll(message: string): void {
    this.git(["add", "-A"]);
    this.git(["commit", "-qm", message]);
  }

  remove(): void {
    rmSync(this.root, { recursive: true, force: true });
  }
}

/** Parse the review file the way a real reader would: it's the sync bus. */
export function readReview(root: string): Record<string, unknown> {
  return JSON.parse(readFileSync(join(root, ".ambidiff.json"), "utf8")) as Record<string, unknown>;
}

/**
 * Overwrite the review file directly, the way an external agent editing
 * the same tree would (`.ambidiff.json` is the sync bus, not CLI-only).
 */
export function writeReview(root: string, review: Record<string, unknown>): void {
  writeFileSync(join(root, ".ambidiff.json"), JSON.stringify(review));
}

export interface Server {
  url: string;
  process: ChildProcess;
  /** Everything the child wrote to stderr so far. */
  stderr(): string;
  stop(): void;
}

/**
 * Start `ambidiff web` in `root` and resolve once it prints its URL.
 * Rejects with the collected output when the child exits, errors, or stays
 * silent for `timeoutMs`.
 */
export function startServer(root: string, timeoutMs = 15_000): Promise<Server> {
  if (!existsSync(BIN)) {
    return Promise.reject(new Error(`ambidiff binary not found at ${BIN}; run cargo build first`));
  }
  const child = spawn(BIN, ["web", "--port", "0"], { cwd: root });
  let out = "";
  let err = "";
  child.stderr?.on("data", (chunk) => {
    err += chunk.toString();
  });
  return new Promise<Server>((resolve, reject) => {
    const fail = (why: string) =>
      reject(new Error(`${why}\nstdout:\n${out}\nstderr:\n${err}`));
    const timer = setTimeout(() => {
      child.kill();
      fail(`server never printed its url within ${timeoutMs} ms`);
    }, timeoutMs);
    child.once("error", (e) => {
      clearTimeout(timer);
      fail(`server failed to spawn: ${e.message}`);
    });
    child.once("exit", (code, signal) => {
      clearTimeout(timer);
      fail(`server exited early (code ${code}, signal ${signal})`);
    });
    child.stdout?.on("data", (chunk) => {
      out += chunk.toString();
      const m = out.match(/ambidiff web at (http\S+)/);
      if (m?.[1]) {
        clearTimeout(timer);
        child.removeAllListeners("exit");
        resolve({
          url: m[1],
          process: child,
          stderr: () => err,
          stop: () => {
            child.kill();
          },
        });
      }
    });
  });
}

export interface WsHold {
  /** Release the oldest queued server->page frame matching `predicate`. */
  release(predicate: (msg: Record<string, unknown>) => boolean): void;
  /** Release every currently-queued frame, oldest first. */
  releaseAll(): void;
  /** Queued frames still held, parsed. */
  pending(): Record<string, unknown>[];
}

/**
 * Intercept the page's `/ws` connection: every server->page frame matching
 * `hold` is queued instead of delivered, so a test can simulate a delayed
 * reply (`getFile`) or a dropped broadcast (`reviewChanged`) and release it
 * (or never release it) exactly when the test wants. Every page->server
 * frame is forwarded untouched. Must be called before `page.goto`.
 */
export async function routeWithHold(
  page: Page,
  hold: (msg: Record<string, unknown>) => boolean,
): Promise<WsHold> {
  const queue: (string | Buffer)[] = [];
  let clientRoute: WebSocketRoute | null = null;
  await page.routeWebSocket(/\/ws$/, (ws) => {
    clientRoute = ws;
    const server = ws.connectToServer();
    server.onMessage((message) => {
      let parsed: unknown;
      try {
        parsed = JSON.parse(String(message));
      } catch {
        ws.send(message);
        return;
      }
      if (typeof parsed === "object" && parsed !== null && hold(parsed as Record<string, unknown>)) {
        queue.push(message);
      } else {
        ws.send(message);
      }
    });
  });
  return {
    release(predicate) {
      const idx = queue.findIndex((m) => predicate(JSON.parse(String(m)) as Record<string, unknown>));
      if (idx === -1) throw new Error("no held websocket frame matches");
      const [msg] = queue.splice(idx, 1);
      if (msg === undefined || !clientRoute) throw new Error("held frame vanished");
      clientRoute.send(msg);
    },
    releaseAll() {
      const all = queue.splice(0, queue.length);
      if (!clientRoute) throw new Error("websocket route never connected");
      for (const m of all) clientRoute.send(m);
    },
    pending() {
      return queue.map((m) => JSON.parse(String(m)) as Record<string, unknown>);
    },
  };
}

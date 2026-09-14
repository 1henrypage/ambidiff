// Vim-style count prefixes (`10j`) and the `:<num>` goto-line motion,
// browser-only (the plan's TUI journey is `crates/cli/tests/tui_pty.rs`'s
// `count_prefix_and_goto_line_navigate`). The fixture file rewrites every
// line so the single hunk is one block of 15 removes then 15 adds, with no
// gaps -- a fully predictable row layout.
import { test, expect } from "@playwright/test";

import { ScratchRepo, startServer, type Server } from "./helpers";

let repo: ScratchRepo;
let server: Server;
let url: string;

test.beforeAll(async () => {
  repo = new ScratchRepo();
  const original = Array.from({ length: 15 }, (_, i) => `const l${i + 1} = ${i + 1};\n`).join("");
  const modified = Array.from({ length: 15 }, (_, i) => `const m${i + 1} = ${(i + 1) * 100};\n`).join("");
  repo.write("src/app.ts", original);
  repo.commitAll("base");
  repo.write("src/app.ts", modified);
  repo.cli(["init", "--review", "motions-e2e", "--base", "HEAD"]);
  server = await startServer(repo.root);
  url = server.url;
});

test.afterAll(() => {
  server?.stop();
  repo?.remove();
});

test("a count prefix repeats cursorDown and then clears", async ({ page }) => {
  await page.goto(url);
  await expect(page.locator("#rows")).toContainText("l1", { timeout: 20_000 });

  await page.locator("#viewport").click();
  const before = Number(await page.locator(".dl.cursor").getAttribute("data-index"));

  await page.keyboard.press("1");
  await page.keyboard.press("0");
  await expect(page.locator("#statusbar .count")).toHaveText("10");

  await page.keyboard.press("j");
  await expect(page.locator("#statusbar .count")).toHaveCount(0);
  const after = Number(await page.locator(".dl.cursor").getAttribute("data-index"));
  expect(after - before).toBe(10);
});

test("Escape clears a pending count", async ({ page }) => {
  await page.goto(url);
  await expect(page.locator("#rows")).toContainText("l1", { timeout: 20_000 });

  await page.locator("#viewport").click();
  await page.keyboard.press("5");
  await expect(page.locator("#statusbar .count")).toHaveText("5");
  await page.keyboard.press("Escape");
  await expect(page.locator("#statusbar .count")).toHaveCount(0);
});

test(": jumps straight to a line number", async ({ page }) => {
  await page.goto(url);
  await expect(page.locator("#rows")).toContainText("l1", { timeout: 20_000 });

  await page.locator("#viewport").click();
  await page.keyboard.press(":");
  await page.locator("#overlay input").fill("7");
  await page.keyboard.press("Enter");
  // New-side line 7 is the added "m7" row (the side rule prefers new).
  await expect(page.locator(".dl.cursor")).toContainText("m7");
});

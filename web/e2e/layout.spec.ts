// Layout: the fixed 20px layout and the wrapped MeasuredLayout must never
// let two rows overlap, and a wrapped long line must actually grow taller
// than the fixed row height. Exercises `W` under the default window, a
// narrow window, and split mode.
import { test, expect } from "@playwright/test";

import { ScratchRepo, startServer, type Server } from "./helpers";

let repo: ScratchRepo;
let server: Server;
let url: string;

const LONG_LINE = "x".repeat(400);

test.beforeAll(async () => {
  repo = new ScratchRepo();
  repo.write("src/base.ts", "line one\nline two\nline three\n");
  repo.commitAll("base");
  repo.write("src/base.ts", `line one\n${LONG_LINE}\nline two\nline three\n`);
  repo.cli(["init", "--review", "layout-e2e", "--base", "HEAD"]);
  server = await startServer(repo.root);
  url = server.url;
});

test.afterAll(() => {
  server?.stop();
  repo?.remove();
});

/** No two consecutive `.dl` rows may occupy overlapping vertical space. */
async function assertNoOverlap(page: import("@playwright/test").Page): Promise<{ tallestHeight: number }> {
  const rows = page.locator(".dl[data-index]");
  const count = await rows.count();
  let prevBottom: number | null = null;
  let tallestHeight = 0;
  for (let i = 0; i < count; i++) {
    const box = await rows.nth(i).boundingBox();
    if (!box) continue;
    if (prevBottom !== null) expect(box.y + 0.5).toBeGreaterThanOrEqual(prevBottom);
    prevBottom = box.y + box.height;
    tallestHeight = Math.max(tallestHeight, box.height);
  }
  return { tallestHeight };
}

test("W switches to a measured layout with no overlapping rows, then back", async ({ page }) => {
  await page.goto(url);
  await expect(page.locator("#rows")).toContainText("line one", { timeout: 20_000 });

  const fixedBox = await page.locator(".dl[data-index]").first().boundingBox();
  expect(fixedBox?.height ?? 0).toBeLessThan(21);
  await assertNoOverlap(page);

  await page.locator("#viewport").click();
  await page.keyboard.press("W");
  await expect(page.locator("#app")).toHaveClass(/wrap/);

  const { tallestHeight } = await assertNoOverlap(page);
  expect(tallestHeight).toBeGreaterThan(20.5);

  await page.keyboard.press("W");
  await expect(page.locator("#app")).not.toHaveClass(/wrap/);
  const afterBox = await page.locator(".dl[data-index]").first().boundingBox();
  expect(afterBox?.height ?? 0).toBeLessThan(21);
  await assertNoOverlap(page);
});

test("wrap holds under a narrow window", async ({ page }) => {
  await page.setViewportSize({ width: 420, height: 600 });
  await page.goto(url);
  await expect(page.locator("#rows")).toContainText("line one", { timeout: 20_000 });
  await page.locator("#viewport").click();
  await page.keyboard.press("W");
  await expect(page.locator("#app")).toHaveClass(/wrap/);
  const { tallestHeight } = await assertNoOverlap(page);
  expect(tallestHeight).toBeGreaterThan(20.5);
});

test("wrap holds in split mode", async ({ page }) => {
  await page.goto(url);
  await expect(page.locator("#rows")).toContainText("line one", { timeout: 20_000 });
  await page.locator("#viewport").click();
  await page.keyboard.press("s");
  await expect(page.locator(".split-sep").first()).toBeVisible({ timeout: 15_000 });
  await page.keyboard.press("W");
  await expect(page.locator("#app")).toHaveClass(/wrap/);
  const { tallestHeight } = await assertNoOverlap(page);
  expect(tallestHeight).toBeGreaterThan(20.5);
});

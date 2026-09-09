// Navigation ownership (B14), against a real server through a websocket
// proxy that can delay a `file` reply on demand: a stale response must
// never clobber a newer navigation, a double-click on the same file must
// resolve exactly once, and navigating to the overview before a delayed
// reply arrives must not be undone by that reply.
import { test, expect } from "@playwright/test";

import { ScratchRepo, startServer, routeWithHold, type Server } from "./helpers";

let repo: ScratchRepo;
let server: Server;
let url: string;

test.beforeAll(async () => {
  repo = new ScratchRepo();
  repo.write("src/a.ts", "unique-content-A-old\n");
  repo.write("src/b.ts", "unique-content-B-old\n");
  repo.commitAll("base");
  repo.write("src/a.ts", "unique-content-A-new\n");
  repo.write("src/b.ts", "unique-content-B-new\n");
  repo.cli(["init", "--review", "navigation-e2e", "--base", "HEAD"]);
  server = await startServer(repo.root);
  url = server.url;
});

test.afterAll(() => {
  server?.stop();
  repo?.remove();
});

function isFileFor(path: string) {
  return (msg: Record<string, unknown>) => msg["type"] === "file" && msg["path"] === path;
}

/** Wait until a matching frame is actually queued, then release it. */
async function awaitAndRelease(hold: Awaited<ReturnType<typeof routeWithHold>>, path: string): Promise<void> {
  await expect.poll(() => hold.pending().some(isFileFor(path))).toBe(true);
  hold.release(isFileFor(path));
}

test("a stale cross-path reply is ignored", async ({ page }) => {
  const hold = await routeWithHold(page, (msg) => msg["type"] === "file");
  await page.goto(url);
  // boot auto-opens a.ts; its reply is held.
  await page.locator(".tree-row", { hasText: "b.ts" }).click();

  await awaitAndRelease(hold, "src/b.ts");
  await expect(page.locator("#rows")).toContainText("unique-content-B-new", { timeout: 15_000 });

  // The stale a.ts reply, arriving after, must not clobber b.ts.
  await awaitAndRelease(hold, "src/a.ts");
  await page.waitForTimeout(300);
  await expect(page.locator("#rows")).toContainText("unique-content-B-new");
  await expect(page.locator("#rows")).not.toContainText("unique-content-A");
  await expect(page.locator("#banner")).toContainText("b.ts");
});

test("a same-path double click resolves exactly once", async ({ page }) => {
  const hold = await routeWithHold(page, isFileFor("src/b.ts"));
  await page.goto(url);
  await expect(page.locator("#rows")).toContainText("unique-content-A-new", { timeout: 15_000 });

  const row = page.locator(".tree-row", { hasText: "b.ts" });
  await row.click();
  await row.click();
  await expect.poll(() => hold.pending().length).toBe(2);

  hold.releaseAll();
  await expect(page.locator("#rows")).toContainText("unique-content-B-new", { timeout: 15_000 });
  await expect(page.locator("#rows", { hasText: "unique-content-B-new" })).toHaveCount(1);
  await expect(page.locator("#banner")).toContainText("b.ts");
});

test("navigating to the overview before a delayed reply arrives is not undone by it", async ({ page }) => {
  const hold = await routeWithHold(page, (msg) => msg["type"] === "file");
  await page.goto(url);
  await page.locator(".tree-row", { hasText: "b.ts" }).click();

  await page.locator(".tree-row", { hasText: "(review)" }).click();
  await expect(page.locator("#banner")).toContainText("review");

  await awaitAndRelease(hold, "src/b.ts");
  await awaitAndRelease(hold, "src/a.ts");
  await page.waitForTimeout(300);
  await expect(page.locator("#banner")).toContainText("review");
  await expect(page.locator("#rows")).not.toContainText("unique-content-B");
});

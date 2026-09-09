// Live convergence: a file whose change vanishes from the working tree
// falls the page back to the overview with a notice (never a silent
// crash), and a dropped `reviewChanged` broadcast is repaired by a manual
// refresh (`r`), which reloads the full snapshot regardless.
import { test, expect } from "@playwright/test";

import { ScratchRepo, startServer, routeWithHold, type Server } from "./helpers";

let repo: ScratchRepo;
let server: Server;
let url: string;
const ORIGINAL_A = "unchanged-a\n";

test.beforeEach(async () => {
  repo = new ScratchRepo();
  repo.write("src/a.ts", ORIGINAL_A);
  repo.write("src/b.ts", "unchanged-b\n");
  repo.commitAll("base");
  repo.write("src/a.ts", "changed-a\n");
  repo.write("src/b.ts", "changed-b\n");
  repo.cli(["init", "--review", "sync-e2e", "--base", "HEAD"]);
  server = await startServer(repo.root);
  url = server.url;
});

test.afterEach(() => {
  server?.stop();
  repo?.remove();
});

test("a vanished file's change falls back to the overview with a notice", async ({ page }) => {
  await page.goto(url);
  await expect(page.locator("#rows")).toContainText("changed-a", { timeout: 20_000 });
  await expect(page.locator("#banner")).toContainText("a.ts");

  // The worktree change disappears entirely (content matches the base again).
  repo.write("src/a.ts", ORIGINAL_A);

  await expect(page.locator("#statusbar .msg")).toContainText("no longer in this diff", { timeout: 20_000 });
  await expect(page.locator("#banner")).toContainText("review");
});

test("a dropped reviewChanged broadcast is repaired by a manual refresh", async ({ page }) => {
  const hold = await routeWithHold(page, (msg) => msg["type"] === "reviewChanged");
  await page.goto(url);
  await expect(page.locator("#rows")).toContainText("changed-a", { timeout: 20_000 });

  repo.cli(["comment", "add", "-p", "src/a.ts", "-l", "1", "-m", "missed notification test"]);

  // The broadcast is dropped by the proxy: the page must not see it yet.
  await expect.poll(() => hold.pending().some((m) => m["type"] === "reviewChanged")).toBe(true);
  await page.waitForTimeout(300);
  await expect(page.locator("#rows")).not.toContainText("missed notification test");

  await page.locator("#viewport").click();
  await page.keyboard.press("r");
  await expect(page.locator("#rows")).toContainText("missed notification test", { timeout: 15_000 });
});

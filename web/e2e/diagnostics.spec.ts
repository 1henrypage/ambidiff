// A failed listing is never shown as a successful empty review: a bad base
// ref paints "source unavailable" in the tree, the error in the diagnostics
// strip and a status badge, while a genuinely clean tree says "no changes"
// with nothing to diagnose.
import { test, expect } from "@playwright/test";

import { ScratchRepo, startServer, type Server } from "./helpers";

let broken: ScratchRepo;
let clean: ScratchRepo;
let brokenServer: Server;
let cleanServer: Server;

test.beforeAll(async () => {
  broken = new ScratchRepo();
  broken.write("src/app.ts", "const one = 1;\n");
  broken.commitAll("base");
  broken.write("src/app.ts", "const one = 100;\n");
  broken.cli(["init", "--review", "diagnostics-broken", "--base", "no-such-ref"]);
  brokenServer = await startServer(broken.root);

  clean = new ScratchRepo();
  clean.write("src/app.ts", "const one = 1;\n");
  clean.commitAll("base");
  clean.cli(["init", "--review", "diagnostics-clean", "--base", "HEAD"]);
  cleanServer = await startServer(clean.root);
});

test.afterAll(() => {
  brokenServer?.stop();
  cleanServer?.stop();
  broken?.remove();
  clean?.remove();
});

test("a bad base is an unavailable listing, not an empty review", async ({ page }) => {
  await page.goto(brokenServer.url);
  await expect(page.locator("#tree .tree-row.placeholder")).toHaveText("source unavailable", { timeout: 20_000 });
  await expect(page.locator("#diag")).toBeVisible();
  await expect(page.locator("#diag")).toContainText("no-such-ref");
  await expect(page.locator("#statusbar .badge")).toHaveText("[source error]");
  await expect(page.locator("#tree .tree-row")).toHaveCount(2, { timeout: 20_000 }); // (review) + placeholder
});

test("a clean tree says no changes with nothing to diagnose", async ({ page }) => {
  await page.goto(cleanServer.url);
  await expect(page.locator("#tree .tree-row.placeholder")).toHaveText("no changes", { timeout: 20_000 });
  await expect(page.locator("#diag")).toBeHidden();
  await expect(page.locator("#statusbar .badge")).toHaveCount(0);
});

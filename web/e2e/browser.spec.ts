// Browser E2E: real binary serving the embedded assets, real chromium, the
// wasm core computing in-page. Journey: open -> render -> comment ->
// lifecycle -> live sync on an external CLI edit.
import { test, expect } from "@playwright/test";
import { readFileSync } from "node:fs";
import { join } from "node:path";

import { ScratchRepo, startServer, type Server } from "./helpers";

let repo: ScratchRepo;
let server: Server;
let url: string;

test.beforeAll(async () => {
  repo = new ScratchRepo();
  repo.write(
    "src/auth.ts",
    'export function check(user: User) {\n  if (user == null) return false;\n  return user.ok;\n}\n',
  );
  repo.commitAll("base");
  repo.write(
    "src/auth.ts",
    'export function check(user: User) {\n  if (user != null) return false;\n  return user.ok;\n}\n',
  );
  repo.cli(["init", "--review", "browser-e2e", "--base", "HEAD"]);
  server = await startServer(repo.root);
  url = server.url;
});

test.afterAll(() => {
  server?.stop();
  repo?.remove();
});

test("render, comment, resolve, live sync", async ({ page }) => {
  await page.goto(url);

  // The wasm core parsed and painted the diff.
  await expect(page.locator("#rows")).toContainText("@@ -1,4 +1,4 @@", { timeout: 20_000 });
  await expect(page.locator(".dl.remove")).toContainText("if (user == null) return false;");
  await expect(page.locator(".dl.add")).toContainText("if (user != null) return false;");
  // Syntax highlighting spans came through (inline RGB from the core).
  await expect(page.locator('#rows span[style*="color"]').first()).toBeVisible();
  // Word-diff emphasis on the changed token.
  await expect(page.locator("#rows .wd").first()).toContainText("==");

  // Comment on the removed line: click it, press c, type, save.
  await page.locator(".dl.remove").click();
  await page.keyboard.press("c");
  await page.locator("#overlay textarea").fill("null check flipped?? explain yourself");
  await page.locator('#overlay [data-act="save"]').click();

  // Card renders and the file persisted the comment with old-side anchor.
  await expect(page.locator("#rows")).toContainText("null check flipped?? explain yourself", {
    timeout: 15_000,
  });
  await expect(page.locator("#rows")).toContainText("[question]");
  await expect
    .poll(() => {
      const review = JSON.parse(readFileSync(join(repo.root, ".ambidiff.json"), "utf8"));
      return review.comments[0]?.side + ":" + review.comments[0]?.line;
    })
    .toBe("old:2");
  const review = JSON.parse(readFileSync(join(repo.root, ".ambidiff.json"), "utf8"));
  expect(review.comments[0].snippet).toBe("  if (user == null) return false;");
  const id = review.comments[0].id;

  // External agent addresses via the CLI; the page converges live.
  repo.cli(["comment", "addressed", id, "-m", "restored the null guard"]);
  await expect(page.locator("#rows")).toContainText("restored the null guard", {
    timeout: 20_000,
  });
  await expect(page.locator("#rows .st-addressed")).toBeVisible();

  // Human resolves from the browser: cursor onto the card head, press x.
  await page.locator(".dl.chead, .dl.card").first().click();
  await page.keyboard.press("x");
  await expect
    .poll(() => JSON.parse(readFileSync(join(repo.root, ".ambidiff.json"), "utf8")).comments[0].status, {
      timeout: 15_000,
    })
    .toBe("resolved");
  await expect(page.locator("#rows .st-resolved")).toBeVisible();

  // Layout and theme toggles stay functional after the round trip.
  await page.keyboard.press("s");
  await expect(page.locator(".split-sep").first()).toBeVisible({ timeout: 15_000 });
  await page.keyboard.press("T");
  await expect(page.locator("body")).toHaveAttribute("data-theme", "light");
});
